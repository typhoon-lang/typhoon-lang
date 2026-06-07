#include "ty_io_backend.h"
#include "io_driver.h"
#include "scheduler.h"
#include <string.h>

/*
 * Phase 4 IO backend dispatcher.
 *
 * Priority:
 * 1. Mock backend (for unit tests)
 * 2. Per-worker TyIoBackend (IOCP/kqueue/io_uring)
 * 3. Global io_driver fallback
 */

/* ── Mock backend for testing ─────────────────────────────────────────────── */

#define MOCK_CAP 1024
static TyIoOp mock_ops[MOCK_CAP];
static int mock_head = 0;
static int mock_count = 0;
static int use_mock = 0;

void ty_io_backend_use_mock(int use) {
    use_mock = use;
    if (!use) {
        mock_head = 0;
        mock_count = 0;
        memset(mock_ops, 0, sizeof(mock_ops));
    }
}

int ty_io_mock_count(void) { return mock_count; }

const TyIoOp* ty_io_mock_get(int idx) {
    if (idx < 0 || idx >= mock_count) return NULL;
    int i = (mock_head + idx) % MOCK_CAP;
    return &mock_ops[i];
}

void ty_io_mock_complete(int idx, int64_t result) {
    if (idx < 0 || idx >= mock_count) return;
    int i = (mock_head + idx) % MOCK_CAP;
    void* coro = mock_ops[i].coro;
    ty_io_wake_coro(coro, result);
}

/* ── Wake callback used when polling per-worker backends ──────────────────── */

static void sched_wake(void* coro, int64_t result) {
    ty_io_wake_coro(coro, result);
}

/* ── submit ───────────────────────────────────────────────────────────────── */

int ty_io_submit(const TyIoOp* op) {
    /* 1. Mock path */
    if (use_mock) {
        if (mock_count >= MOCK_CAP) return -1;
        int i = (mock_head + mock_count) % MOCK_CAP;
        mock_ops[i] = *op;
        mock_count++;
        return 0;
    }

    /* 2. Per-worker backend path */
    Worker* w = ty_sched_current_worker();
    if (w && w->io_backend) {
        TyIoBackend* be = w->io_backend;
        if (be->submit)
            return be->submit(be, op);
    }

    /* 3. Global io_driver fallback */
    void* drv = ty_io_global_driver();
    if (!drv) return -1;
    void* task = ty_current_arena();
    void* coro = ty_current_coro_raw();
    if (op->type == TY_IO_OP_READ) {
        ty_io_read(drv, (SlabArena*)task, coro, op->fd, (uint8_t*)op->buf, op->len);
        return 0;
    } else if (op->type == TY_IO_OP_WRITE) {
        ty_io_write(drv, (SlabArena*)task, coro, op->fd, (const uint8_t*)op->buf, op->len);
        return 0;
    } else if (op->type == TY_IO_OP_ACCEPT) {
        /* Global driver fallback for ACCEPT: we cannot call ty_io_read()
         * here because that would park the coroutine inside this function.
         * The caller (ty_net.c Listener__accept) calls ty_io_park_coro()
         * separately after ty_io_submit() returns, matching the pattern
         * used by per-worker backends where submit() only registers
         * interest and the caller parks.
         *
         * Since the global driver's io_poll_thread doesn't understand
         * ACCEPT, we return -1 and the coroutine's non-blocking accept()
         * loop in ty_net.c will yield and retry on the next scheduler
         * tick.  This path is rarely hit — per-worker backends handle
         * ACCEPT directly. */
        (void)op;
        (void)coro;
        (void)task;
        return -1;
    }
    return -1;
}

/* ── poll ─────────────────────────────────────────────────────────────────── */

int ty_io_poll(void) {
    if (use_mock) return 0;

    /* 1. Per-worker backend path */
    Worker* w = ty_sched_current_worker();
    if (w && w->io_backend) {
        TyIoBackend* be = w->io_backend;
        if (be->poll)
            return be->poll(be, sched_wake);
    }

    /* 2. Global io_driver fallback */
    void* drv = ty_io_global_driver();
    if (!drv) return 0;
    ty_io_driver_poll(drv);
    return 0;
}
