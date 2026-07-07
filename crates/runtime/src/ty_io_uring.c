/*
 * ty_io_uring.c — Linux io_uring backend for Typhoon IO
 *
 * REWRITTEN: single shared io_uring ring for the whole process, not one
 * per scheduler worker thread — same motivation as the kqueue and IOCP
 * rewrites (see those files' headers): a per-worker ring meant an op's
 * completion could only ever be seen by the worker that submitted it,
 * leaving a stolen coroutine waiting on its *original* worker to get
 * back around to polling instead of whichever worker is actually free.
 *
 * Linux is NOT a clean port of the same trick, though, and that's worth
 * being explicit about rather than pretending otherwise: unlike IOCP's
 * completion port or kqueue's kq fd, io_uring's submission queue is a
 * plain shared-memory ring with a tail pointer — it was designed for a
 * single submitting thread, and concurrent SQE writes + tail bumps from
 * multiple threads without synchronization is a real race (lost
 * submissions, corrupted SQE slots). Go and Tokio both sidestep this
 * entirely by using epoll on Linux instead of io_uring, for exactly
 * this reason — epoll's registration model is shared-fd-safe the same
 * way kqueue's is. Since this codebase has already committed to
 * io_uring specifically, this file adds an explicit submit_lock around
 * the SQ ring critical section instead. The alternative,
 * IORING_SETUP_SQPOLL (a kernel-side thread that drains submissions
 * without needing userspace synchronization), is the more "native" fix
 * but isn't implemented anywhere in this codebase today — this is
 * flagged in typhoon_io_redesign.md's own Task 4.2 checklist as
 * unimplemented, not a config flag away. Locking now, SQPOLL later as a
 * pure performance follow-up, keeps this a correctness fix rather than
 * gating it on a bigger feature build-out.
 *
 * The completion side gets a *try*-lock, not a blocking one: if some
 * other worker is already mid-drain, this worker just skips this poll
 * cycle rather than blocking on it — consistent with the rest of the
 * scheduler's "poll never blocks a worker for long" cooperative
 * principle. Whichever worker does hold the lock will see every
 * completion in the ring, including ones meant for coroutines that
 * migrated elsewhere, so nothing is lost by skipping.
 */

#ifdef __linux__

#include <unistd.h>
#include <fcntl.h>
#include <errno.h>
#include <string.h>
#include <stdlib.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <sys/socket.h>
#include <linux/io_uring.h>

#include "ty_io_uring.h"
#include "io_driver.h"
#include "atomic.h"    /* _Atomic — completion-drain try-lock flag */
#include "platform.h"  /* TyMutex — submission-side blocking lock */

#ifndef IORING_OP_READV
#define IORING_OP_READV 1
#define IORING_OP_WRITEV 2
#endif
#ifndef IORING_OP_POLL_ADD
#define IORING_OP_POLL_ADD 6
#endif
#ifndef IORING_ENTER_GETEVENTS
#define IORING_ENTER_GETEVENTS 1u
#endif
#ifndef IORING_OFF_SQ_RING
#define IORING_OFF_SQ_RING 0ULL
#define IORING_OFF_CQ_RING 0x8000000ULL
#define IORING_OFF_SQES 0x10000000ULL
#endif

#ifndef POLLIN
#define POLLIN 0x0001
#endif

#define RING_ENTRIES 256

/* io_uring kernel setup/enter wrappers */
static int uring_setup(uint32_t entries, struct io_uring_params* p) {
    return (int)syscall(SYS_io_uring_setup, entries, p);
}
static int uring_enter(int fd, uint32_t to_submit, uint32_t min_complete, uint32_t flags) {
    return (int)syscall(SYS_io_uring_enter, fd, to_submit, min_complete, flags, NULL, 0);
}

/* ── submit ──────────────────────────────────────────────────────────────
 *
 * submit_lock serializes the whole SQ critical section — tail read,
 * SQE write, array publish, tail bump, and the uring_enter() call
 * itself — across every worker thread. This is the one place this
 * rewrite adds real contention that didn't exist in the per-worker
 * design, by necessity: unlike kqueue/IOCP, there's no kernel-provided
 * safety net here, so userspace has to provide it. The critical section
 * is small (a handful of memory writes plus one syscall), so contention
 * should stay brief even under load, but it's worth knowing this exists
 * if Linux submission throughput ever becomes a bottleneck — that's
 * exactly what IORING_SETUP_SQPOLL would remove, see this file's header
 * comment. ────────────────────────────────────────────────────────────── */

static int uring_submit_op(TyIoBackend* base, const TyIoOp* op) {
    TyUringBackend* b = (TyUringBackend*)base;
    if (!b || b->ring_fd < 0 || !op) return -1;

    ty_mutex_lock(&b->submit_lock);

    uint32_t* sq_tail_ptr = (uint32_t*)((uint8_t*)b->sq_ring + b->sq_tail_off);
    uint32_t* sq_array = (uint32_t*)((uint8_t*)b->sq_ring + b->sq_array_off);
    uint32_t tail = *sq_tail_ptr;
    uint32_t idx = tail & b->sq_mask;

    struct io_uring_sqe* sqe = &((struct io_uring_sqe*)b->sqes)[idx];
    memset(sqe, 0, sizeof(*sqe));

    if (op->type == TY_IO_OP_ACCEPT) {
        /* Use IORING_OP_POLL_ADD with POLLIN to detect when the
         * listener socket has a pending connection. When the
         * completion fires, poll() calls accept() to get the fd.
         *
         * POLL_ADD uses sqe->fd for the target fd and sqe->len
         * for the poll mask; sqe->addr is unused. */
        sqe->opcode = IORING_OP_POLL_ADD;
        sqe->fd = (int)op->fd;
        sqe->len = POLLIN;
    } else {
        if (op->type == TY_IO_OP_READ) {
            sqe->opcode = IORING_OP_READV;
        } else if (op->type == TY_IO_OP_WRITE) {
            sqe->opcode = IORING_OP_WRITEV;
        } else {
            ty_mutex_unlock(&b->submit_lock);
            return -1;
        }
        sqe->fd = (int)op->fd;
        sqe->addr = (uint64_t)(uintptr_t)op->buf;
        sqe->len = (uint32_t)op->len;
    }

    /* NOTE, unchanged from before this rewrite and still worth flagging:
     * this stores a pointer to the CALLER's own TyIoOp (typically
     * stack-local in whoever called ty_io_submit) directly as
     * user_data, with no defensive copy — unlike IOCP (HeapAlloc's an
     * IocpReq) and kqueue (pool_alloc()s from a dedicated pool). Works
     * today because every caller submits then immediately parks,
     * keeping the coroutine's stack (and op) alive until the result is
     * taken — but it's implicit rather than defensively correct. Not
     * something this rewrite changes, since it's an orthogonal
     * lifetime question, not a cross-worker migration one — flagging
     * again here since this pass touched every other line around it. */
    sqe->user_data = (uint64_t)(uintptr_t)op;

    sq_array[idx] = idx;
    __sync_synchronize();
    *sq_tail_ptr = tail + 1;
    __sync_synchronize();

    int enter_rc = uring_enter(b->ring_fd, 1, 0, 0);
    TY_DEBUG("[uring] submit op_type=%d fd=%d opcode=%u user_data=%p enter_rc=%d errno=%d\n",
        op->type, (int)op->fd, (unsigned)sqe->opcode, (void*)(uintptr_t)op->coro,
        enter_rc, enter_rc < 0 ? errno : 0);

    ty_mutex_unlock(&b->submit_lock);
    return 0;
}

/* ── poll ─────────────────────────────────────────────────────────────────
 *
 * poll_lock_flag is a try-lock (CAS 0→1), not b->submit_lock and not a
 * blocking mutex — multiple workers racing to poll the same shared ring
 * should not queue up waiting on each other. Whichever worker wins just
 * drains everything currently in the CQ (including completions for
 * coroutines that started on a different worker entirely — that's the
 * actual fix this rewrite is for), and every other worker that lost the
 * race simply returns 0 for this cycle rather than blocking, matching
 * the cooperative-scheduling rule that poll() must never stall a
 * worker. Nothing is lost by losing the race: the winner sees the full
 * queue regardless of who submitted each entry. ───────────────────────── */

static int uring_poll_op(TyIoBackend* base, TySchedWakeFn wake) {
    TyUringBackend* b = (TyUringBackend*)base;
    if (!b || b->ring_fd < 0) return 0;

    int expected = 0;
    if (!atomic_compare_exchange_strong_explicit(
            &b->poll_lock_flag, &expected, 1,
            memory_order_acquire, memory_order_relaxed)) {
        return 0; /* someone else is already draining this cycle */
    }

    /* Non-blocking peek: enter with min_complete=0 and no GETEVENTS flag. */
    int peek_rc = uring_enter(b->ring_fd, 0, 0, 0);

    uint32_t* cq_head_ptr = (uint32_t*)((uint8_t*)b->cq_ring + b->cq_head_off);
    uint32_t* cq_tail_ptr = (uint32_t*)((uint8_t*)b->cq_ring + b->cq_tail_off);
    struct io_uring_cqe* cqes = (struct io_uring_cqe*)((uint8_t*)b->cq_ring + b->cq_cqes_off);

    __sync_synchronize();
    uint32_t head = *cq_head_ptr;
    uint32_t tail = *cq_tail_ptr;
    __sync_synchronize();

    if (peek_rc < 0) {
        TY_DEBUG("[uring] poll: uring_enter(peek) failed rc=%d errno=%d\n", peek_rc, errno);
    }
    if (head != tail) {
        TY_DEBUG("[uring] poll: %u completion(s) pending (head=%u tail=%u)\n",
            tail - head, head, tail);
    }

    int count = 0;
    while (head != tail) {
        struct io_uring_cqe* cqe = &cqes[head & b->cq_mask];
        int32_t res = cqe->res;
        uint64_t ud = cqe->user_data;
        head++;
        count++;

        TyIoOp* op = (TyIoOp*)(uintptr_t)ud;
        TY_DEBUG("[uring] cqe res=%d user_data=%p op_type=%s coro=%p\n",
            res, (void*)(uintptr_t)ud,
            op ? (op->type == TY_IO_OP_ACCEPT ? "ACCEPT" :
                  op->type == TY_IO_OP_READ ? "READ" :
                  op->type == TY_IO_OP_WRITE ? "WRITE" : "?") : "(null op)",
            op ? (void*)(uintptr_t)op->coro : NULL);
        if (op && op->coro) {
            int64_t result;

            if (op->type == TY_IO_OP_ACCEPT) {
                /* POLL_ADD completion — the listener may be readable.
                 * Call accept() to get the new client fd.
                 * Spurious readiness is possible: POLL_ADD can fire
                 * even when no connection is pending.  If accept()
                 * returns EAGAIN, re-submit POLL_ADD and do NOT wake
                 * the coroutine — it stays parked until a real
                 * connection arrives.
                 *
                 * This re-submit goes through the SAME submit_lock as
                 * uring_submit_op — it's manipulating the same SQ ring,
                 * so it needs the same protection, not a separate
                 * ad-hoc critical section. */
                if (res >= 0) {
                    int c = accept((int)op->fd, NULL, NULL);
                    TY_DEBUG("[uring] ACCEPT cqe res=%d -> accept() returned c=%d errno=%d\n",
                        res, c, c < 0 ? errno : 0);
                    if (c < 0 && (errno == EAGAIN || errno == EWOULDBLOCK)) {
                        TY_DEBUG("[uring] ACCEPT spurious readiness (EAGAIN), re-submitting POLL_ADD fd=%d\n",
                            (int)op->fd);
                        ty_mutex_lock(&b->submit_lock);
                        struct io_uring_sqe* rsqe;
                        uint32_t* sq_tail_ptr2 = (uint32_t*)((uint8_t*)b->sq_ring + b->sq_tail_off);
                        uint32_t* sq_array2 = (uint32_t*)((uint8_t*)b->sq_ring + b->sq_array_off);
                        uint32_t tail2 = *sq_tail_ptr2;
                        uint32_t idx2 = tail2 & b->sq_mask;
                        rsqe = &((struct io_uring_sqe*)b->sqes)[idx2];
                        memset(rsqe, 0, sizeof(*rsqe));
                        rsqe->opcode = IORING_OP_POLL_ADD;
                        rsqe->fd = (int)op->fd;
                        rsqe->len = POLLIN;
                        rsqe->user_data = (uint64_t)(uintptr_t)op;
                        sq_array2[idx2] = idx2;
                        __sync_synchronize();
                        *sq_tail_ptr2 = tail2 + 1;
                        __sync_synchronize();
                        int resub_rc = uring_enter(b->ring_fd, 1, 0, 0);
                        TY_DEBUG("[uring] ACCEPT re-submit POLL_ADD enter_rc=%d errno=%d\n",
                            resub_rc, resub_rc < 0 ? errno : 0);
                        ty_mutex_unlock(&b->submit_lock);
                        continue; /* skip wake — coro stays parked */
                    }
                    result = (c < 0) ? -(int64_t)errno : (int64_t)c;
                } else {
                    /* POLL_ADD itself failed — propagate the error
                     * so the accept loop in ty_net.c can handle it
                     * (e.g. retry on EINVAL from old kernels). */
                    result = (int64_t)res;
                }
            } else if (res < 0) {
                /* io_uring negative results are -errno */
                result = (int64_t)res;
            } else {
                result = (int64_t)res;
            }

            if (wake)
                wake(op->coro, result);
            else
                ty_io_wake_coro(op->coro, result);
        }
    }

    __sync_synchronize();
    *cq_head_ptr = head;

    atomic_store_explicit(&b->poll_lock_flag, 0, memory_order_release);
    return count;
}

/* ── shared singleton ring ───────────────────────────────────────────────
 *
 * Every worker calls ty_uring_backend_new() once during its own
 * startup (unchanged call site). First caller actually creates and
 * mmaps the ring; every caller after that gets the SAME
 * TyUringBackend* back and a bumped refcount.
 * ty_uring_backend_destroy() only actually tears the ring down once
 * every worker that got a reference has released it.
 * ────────────────────────────────────────────────────────────────────── */

static TyMutex g_create_lock;
/* Same fix as ty_io_kqueue.c's g_lock_state — see that file's comment
 * for the full reasoning. A plain int flag here is a genuine race if
 * ever called concurrently; not live against today's scheduler.c
 * (serial call loop in ty_sched_init()), but shouldn't be depended on. */
static _Atomic(int) g_lock_state = 0; /* 0=uninit, 1=initializing, 2=ready */
static TyUringBackend* g_backend = NULL;
static int g_refcount = 0;

static void ensure_create_lock_inited(void) {
    int expected = 0;
    if (atomic_compare_exchange_strong_explicit(&g_lock_state, &expected, 1,
            memory_order_acq_rel, memory_order_acquire)) {
        ty_mutex_init(&g_create_lock);
        atomic_store_explicit(&g_lock_state, 2, memory_order_release);
        return;
    }
    while (atomic_load_explicit(&g_lock_state, memory_order_acquire) != 2) {
        /* busy-wait — one mutex_init() call, startup-only contention */
    }
}

TyUringBackend* ty_uring_backend_new(void) {
    ensure_create_lock_inited();
    ty_mutex_lock(&g_create_lock);

    if (g_backend) {
        g_refcount++;
        ty_mutex_unlock(&g_create_lock);
        return g_backend;
    }

    TyUringBackend* b = (TyUringBackend*)malloc(sizeof(TyUringBackend));
    if (!b) {
        ty_mutex_unlock(&g_create_lock);
        return NULL;
    }
    memset(b, 0, sizeof(*b));

    struct io_uring_params params;
    memset(&params, 0, sizeof(params));

    int rfd = uring_setup(RING_ENTRIES, &params);
    if (rfd < 0) {
        free(b);
        ty_mutex_unlock(&g_create_lock);
        return NULL;
    }
    b->ring_fd = rfd;

    /* mmap SQ ring */
    size_t sq_sz = params.sq_off.array + params.sq_entries * sizeof(uint32_t);
    void* sq = mmap(NULL, sq_sz, PROT_READ | PROT_WRITE,
        MAP_SHARED | MAP_POPULATE, rfd, IORING_OFF_SQ_RING);
    if (sq == MAP_FAILED) { close(rfd); free(b); ty_mutex_unlock(&g_create_lock); return NULL; }
    b->sq_ring = sq;

    /* mmap SQEs */
    size_t sqe_sz = params.sq_entries * sizeof(struct io_uring_sqe);
    void* sqes = mmap(NULL, sqe_sz, PROT_READ | PROT_WRITE,
        MAP_SHARED | MAP_POPULATE, rfd, IORING_OFF_SQES);
    if (sqes == MAP_FAILED) {
        munmap(sq, sq_sz); close(rfd); free(b);
        ty_mutex_unlock(&g_create_lock);
        return NULL;
    }
    b->sqes = sqes;

    /* mmap CQ ring */
    size_t cq_sz = params.cq_off.cqes
        + params.cq_entries * sizeof(struct io_uring_cqe);
    void* cq = mmap(NULL, cq_sz, PROT_READ | PROT_WRITE,
        MAP_SHARED | MAP_POPULATE, rfd, IORING_OFF_CQ_RING);
    if (cq == MAP_FAILED) {
        munmap(sqes, sqe_sz); munmap(sq, sq_sz); close(rfd); free(b);
        ty_mutex_unlock(&g_create_lock);
        return NULL;
    }
    b->cq_ring = cq;

    b->sq_mask = *(uint32_t*)((uint8_t*)sq + params.sq_off.ring_mask);
    b->cq_mask = *(uint32_t*)((uint8_t*)cq + params.cq_off.ring_mask);
    b->sq_entries = params.sq_entries;
    b->cq_entries = params.cq_entries;

    b->sq_head_off = params.sq_off.head;
    b->sq_tail_off = params.sq_off.tail;
    b->sq_array_off = params.sq_off.array;
    b->cq_head_off = params.cq_off.head;
    b->cq_tail_off = params.cq_off.tail;
    b->cq_cqes_off = params.cq_off.cqes;

    ty_mutex_init(&b->submit_lock);
    atomic_init(&b->poll_lock_flag, 0);

    b->base.impl = b;
    b->base.submit = uring_submit_op;
    b->base.poll = uring_poll_op;

    g_backend = b;
    g_refcount = 1;
    ty_mutex_unlock(&g_create_lock);
    return b;
}

void ty_uring_backend_destroy(TyUringBackend* b) {
    if (!b) return;
    ensure_create_lock_inited();
    ty_mutex_lock(&g_create_lock);

    if (b != g_backend) {
        ty_mutex_unlock(&g_create_lock);
        return;
    }

    g_refcount--;
    if (g_refcount > 0) {
        ty_mutex_unlock(&g_create_lock);
        return; /* other workers still holding a reference */
    }

    if (b->ring_fd >= 0) {
        close(b->ring_fd);
        b->ring_fd = -1;
    }
    if (b->sq_ring) munmap(b->sq_ring, 0); /* sizes not tracked — simplified, unchanged from before this rewrite */
    if (b->cq_ring) munmap(b->cq_ring, 0);
    if (b->sqes) munmap(b->sqes, 0);
    ty_mutex_destroy(&b->submit_lock);
    free(b);
    g_backend = NULL;
    ty_mutex_unlock(&g_create_lock);
}

#endif /* __linux__ */
