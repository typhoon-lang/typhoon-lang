/*
 * ty_io_uring.c — Linux io_uring backend for Typhoon IO
 *
 * One io_uring ring per scheduler worker thread (raw syscalls, no liburing).
 * submit() fills an SQE, sets user_data = op, and calls io_uring_enter.
 * poll() peeks CQEs in a loop and calls wake(coro, result) for each.
 *
 * ACCEPT: uses readiness-based approach.  submit() registers the
 * listener fd for EVFILT_READ (via io_uring poll), and poll() calls
 * accept() when the completion fires.  This avoids the complexity of
 * IORING_OP_ACCEPT which requires pre-allocated socket fds.
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

/* ── submit ──────────────────────────────────────────────────────────────── */

static int uring_submit_op(TyIoBackend* base, const TyIoOp* op) {
    TyUringBackend* b = (TyUringBackend*)base;
    if (!b || b->ring_fd < 0 || !op) return -1;

    uint32_t* sq_tail_ptr = (uint32_t*)((uint8_t*)b->sq_ring + b->sq_tail_off);
    uint32_t* sq_array = (uint32_t*)((uint8_t*)b->sq_ring + b->sq_array_off);
    uint32_t tail = *sq_tail_ptr;
    uint32_t idx = tail & b->sq_mask;

    struct io_uring_sqe* sqe = &((struct io_uring_sqe*)b->sqes)[idx];
    memset(sqe, 0, sizeof(*sqe));

    if (op->type == TY_IO_OP_READ) {
        sqe->opcode = IORING_OP_READV;
    } else if (op->type == TY_IO_OP_WRITE) {
        sqe->opcode = IORING_OP_WRITEV;
    } else if (op->type == TY_IO_OP_ACCEPT) {
        /* Use IORING_OP_POLL_ADD with POLLIN to detect when the
         * listener socket has a pending connection.  When the
         * completion fires, poll() calls accept() to get the fd. */
        sqe->opcode = IORING_OP_POLL_ADD;
        sqe->addr = (uint64_t)(uintptr_t)op->fd;
        sqe->len = POLLIN;
    } else {
        return -1;
    }

    /* sqe->fd is __s32; ty_fd_t is int on Linux, so cast is safe.
     * For POLL_ADD, fd goes in the addr field above, not here. */
    if (op->type != TY_IO_OP_ACCEPT) {
        sqe->fd = (int)op->fd;
        sqe->addr = (uint64_t)(uintptr_t)op->buf;
        sqe->len = (uint32_t)op->len;
    }

    sqe->user_data = (uint64_t)(uintptr_t)op;

    sq_array[idx] = idx;
    __sync_synchronize();
    *sq_tail_ptr = tail + 1;
    __sync_synchronize();


    uring_enter(b->ring_fd, 1, 0, 0);
    return 0;
}

/* ── poll ─────────────────────────────────────────────────────────────────── */

static int uring_poll_op(TyIoBackend* base, TySchedWakeFn wake) {
    TyUringBackend* b = (TyUringBackend*)base;
    if (!b || b->ring_fd < 0) return 0;

    /* Non-blocking peek: enter with min_complete=0 and no GETEVENTS flag. */
    uring_enter(b->ring_fd, 0, 0, 0);

    uint32_t* cq_head_ptr = (uint32_t*)((uint8_t*)b->cq_ring + b->cq_head_off);
    uint32_t* cq_tail_ptr = (uint32_t*)((uint8_t*)b->cq_ring + b->cq_tail_off);
    struct io_uring_cqe* cqes = (struct io_uring_cqe*)((uint8_t*)b->cq_ring + b->cq_cqes_off);

    __sync_synchronize();
    uint32_t head = *cq_head_ptr;
    uint32_t tail = *cq_tail_ptr;
    __sync_synchronize();

    int count = 0;
    while (head != tail) {
        struct io_uring_cqe* cqe = &cqes[head & b->cq_mask];
        int32_t res = cqe->res;
        uint64_t ud = cqe->user_data;
        head++;
        count++;

        TyIoOp* op = (TyIoOp*)(uintptr_t)ud;
        if (op && op->coro) {
            int64_t result;

            if (op->type == TY_IO_OP_ACCEPT) {
                /* POLL_ADD completion — the listener is readable.
                 * Call accept() to get the new client fd. */
                if (res >= 0) {
                    int c = accept((int)op->fd, NULL, NULL);
                    result = (c < 0) ? -(int64_t)errno : (int64_t)c;
                } else {
                    /* POLL_ADD itself failed */
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

    return count;
}

/* ── lifecycle ────────────────────────────────────────────────────────────── */

TyUringBackend* ty_uring_backend_new(void) {
    TyUringBackend* b = (TyUringBackend*)malloc(sizeof(TyUringBackend));
    if (!b) return NULL;
    memset(b, 0, sizeof(*b));

    struct io_uring_params params;
    memset(&params, 0, sizeof(params));

    int rfd = uring_setup(RING_ENTRIES, &params);
    if (rfd < 0) {
        free(b);
        return NULL;
    }
    b->ring_fd = rfd;

    /* mmap SQ ring */
    size_t sq_sz = params.sq_off.array + params.sq_entries * sizeof(uint32_t);
    void* sq = mmap(NULL, sq_sz, PROT_READ | PROT_WRITE,
        MAP_SHARED | MAP_POPULATE, rfd, IORING_OFF_SQ_RING);
    if (sq == MAP_FAILED) { close(rfd); free(b); return NULL; }
    b->sq_ring = sq;

    /* mmap SQEs */
    size_t sqe_sz = params.sq_entries * sizeof(struct io_uring_sqe);
    void* sqes = mmap(NULL, sqe_sz, PROT_READ | PROT_WRITE,
        MAP_SHARED | MAP_POPULATE, rfd, IORING_OFF_SQES);
    if (sqes == MAP_FAILED) { munmap(sq, sq_sz); close(rfd); free(b); return NULL; }
    b->sqes = sqes;

    /* mmap CQ ring */
    size_t cq_sz = params.cq_off.cqes
        + params.cq_entries * sizeof(struct io_uring_cqe);
    void* cq = mmap(NULL, cq_sz, PROT_READ | PROT_WRITE,
        MAP_SHARED | MAP_POPULATE, rfd, IORING_OFF_CQ_RING);
    if (cq == MAP_FAILED) {
        munmap(sqes, sqe_sz); munmap(sq, sq_sz); close(rfd); free(b);
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

    b->base.impl = b;
    b->base.submit = uring_submit_op;
    b->base.poll = uring_poll_op;
    return b;
}

void ty_uring_backend_destroy(TyUringBackend* b) {
    if (!b) return;
    if (b->ring_fd >= 0) {
        close(b->ring_fd);
        b->ring_fd = -1;
    }
    if (b->sq_ring) munmap(b->sq_ring, 0); /* sizes not tracked — simplified */
    if (b->cq_ring) munmap(b->cq_ring, 0);
    if (b->sqes) munmap(b->sqes, 0);
    free(b);
}

#endif /* __linux__ */
