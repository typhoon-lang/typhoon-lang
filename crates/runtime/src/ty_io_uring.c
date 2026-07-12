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
 *
 * ACCEPT — direct IORING_OP_ACCEPT, not IORING_OP_POLL_ADD.
 * Earlier revisions used POLL_ADD with POLLIN, then called accept()
 * synchronously in the completion handler — a common pattern, but
 * the user-visible result was a 60s hang with `cq_head == cq_tail`
 * forever despite nonstop calls to uring_enter(..., GETEVENTS). Root
 * cause: POLL_ADD's registration semantics on a freshly-bindsocket'd
 * listener interact badly with the kernel's poll wait queue when
 * paired with our poll-on-cooperative-tick design — the wake we want
 * is for "fd became readable", but the kernel only registers the
 * waitqueue callback the first time conn-side ack's RTT allows, and
 * if GETEVENTS-driven peels happen microseconds-before the CQE is
 * posted (essentially always, in our test workloads), userspace never
 * sees the transition. Direct IORING_OP_ACCEPT avoids the entire
 * waitqueue dance: the kernel owns the wait, posts a CQE with the
 * accepted fd in res, and we wake the coroutine from `result = c`.
 * Cleaner side: no spurious-readiness EAGAIN re-submit loop.
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
#ifndef IORING_OP_ACCEPT
#define IORING_OP_ACCEPT 13
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
        /* Direct IORING_OP_ACCEPT — kernel owns the wait; we get a CQE
         * with the accepted fd in `res`. Cleaner than POLL_ADD + manual
         * accept() because there's no spurious-readiness race and no
         * re-arm dance for one-shot accept completion.
         *
         * flags = 0 (no SOCK_NONBLOCK, no SOCK_CLOEXEC), so the
         * accepted fd inherits listener flags. We need O_NONBLOCK for
         * the async read/write paths downstream, so set it explicitly
         * afterwards in ty_net.c's Listener__accept — same place
         * kqueue/IOCP backends do their post-accept setup.
         *
         * addr/addrlen must be present (may pass NULL/0 to skip). */
        sqe->opcode = IORING_OP_ACCEPT;
        sqe->fd = (int)op->fd;
        sqe->addr = 0;
        sqe->len = 0;
        TY_DEBUG("[uring] submit ACCEPT fd=%d\n", (int)op->fd);
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
        /* READV and WRITEV are vectored ops — sqe->len is iovec
         * *count*, not byte count. Passing a raw buffer pointer with
         * len=N causes the kernel to read N * sizeof(struct iovec)
         * bytes of metadata from the buffer region, which crosses
         * past the buffer end → EFAULT (-14). Wrap the buffer as a
         * single-element iovec and pass its address instead.
         *
         * Const-cast is safe: we only write iovec fields (not
         * structural corruption), and this op struct is stack-local
         * to the caller (ty_net.c) — alive while coro is parked. */
        ((TyIoOp*)op)->iov.iov_base = op->buf;
        ((TyIoOp*)op)->iov.iov_len  = op->len;
        sqe->addr = (uint64_t)(uintptr_t)&((TyIoOp*)op)->iov;
        sqe->len = 1; /* single iovec */
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

    TY_DEBUG("[uring] poll: entry ring_fd=%d\n", b->ring_fd);

    int expected = 0;
    if (!atomic_compare_exchange_strong_explicit(
            &b->poll_lock_flag, &expected, 1,
            memory_order_acquire, memory_order_relaxed)) {
        TY_DEBUG("[uring] poll: try-lock failed, another worker draining\n");
        return 0; /* someone else is already draining this cycle */
    }

    TY_DEBUG("[uring] poll: got lock, calling uring_enter(GETEVENTS)\n");

    /* Enter with GETEVENTS so IORING_OP_POLL_ADD completions actually
     * get delivered. Without this flag, a plain enter(0,0,0) returns
     * immediately without driving kernel-side poll queues — reaping
     * only whatever CQEs are already sitting in the ring buffer, which
     * is never enough to see new accept/read/write completions that
     * haven't been manually flushed from kernel-side poll work.
     * GETEVENTS tells the kernel to actively drive pending internal
     * operations (IORING_OP_POLL_ADD notably), post their CQEs, and
     * make them visible to userspace via the CQ head/tail pointers.
     *
     * min_complete=0 (non-blocking): still returns immediately; it
     * just also does the internal poll sweep before peeking, which costs
     * microseconds but ensures no completion is stuck waiting for
     * someone to call enter. */
    int peek_rc = uring_enter(b->ring_fd, 0, 0, IORING_ENTER_GETEVENTS);

    TY_DEBUG("[uring] poll: uring_enter(GETEVENTS) returned peek_rc=%d errno=%d\n",
        peek_rc, peek_rc < 0 ? errno : 0);

    uint32_t* cq_head_ptr = (uint32_t*)((uint8_t*)b->cq_ring + b->cq_head_off);
    uint32_t* cq_tail_ptr = (uint32_t*)((uint8_t*)b->cq_ring + b->cq_tail_off);
    struct io_uring_cqe* cqes = (struct io_uring_cqe*)((uint8_t*)b->cq_ring + b->cq_cqes_off);

    __sync_synchronize();
    uint32_t head = *cq_head_ptr;
    uint32_t tail = *cq_tail_ptr;
    __sync_synchronize();

    TY_DEBUG("[uring] poll: CQ head=%u tail=%u (diff=%u)\n", head, tail, tail - head);

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
                /* Direct IORING_OP_ACCEPT completion: kernel has
                 * performed accept() and `res` is the new fd (>=0)
                 * or -errno (<0) on failure. Wake the parked
                 * coroutine with that value, then let ty_net.c's
                 * Listener__accept do the post-accept setup
                 * (O_NONBLOCK, fdset tracking, TySocket allocation). */
                result = (int64_t)res;
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

    TY_DEBUG("[uring] poll: processed %d CQE(s), new head=%u\n", count, head);

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
