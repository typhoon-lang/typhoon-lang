#include "ty_io_backend.h"
#include "io_driver.h"
#include "scheduler.h"
#include "atomic.h"
#include <string.h>

/*
 * Phase 4 IO backend dispatcher.
 *
 * Priority:
 * 1. Mock backend (for unit tests)
 * 2. Per-worker TyIoBackend (IOCP/kqueue/io_uring) — used while fewer than
 *    TY_IO_MAX_OUTSTANDING ops are concurrently in flight through it (see
 *    "Per-worker-backend admission control" below).
 * 3. Global io_driver fallback — also used deliberately, not just as a
 *    before-scheduler-init fallback, once branch 2 is at capacity. It has
 *    its own bounded pool and its own graceful degrade to synchronous I/O,
 *    which is exactly the protection branch 2 is missing on its own.
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

/* ── Per-worker-backend admission control ──────────────────────────────────
 *
 * Nothing used to cap how many ops could be outstanding through the
 * per-worker backend (io_uring/kqueue/IOCP) at once. accept_loop_coro
 * spawns a conn_coro per accepted connection unconditionally, Socket__read
 * submits unconditionally on every call, and uring_submit_op() (etc.)
 * submits to the kernel unconditionally regardless of how much is already
 * in flight — no layer in that chain said no. Under test_phase4_linux_
 * 1000_coroutines' ~1000-way concurrent fan-out that let far more ops pile
 * up as outstanding than the io_uring CQ ring had room for, pushing
 * completions into the kernel's overflow tracking and surfacing them later
 * in large delayed bursts (see RING_CQ_ENTRIES in ty_io_uring.c).
 *
 * The old global io_driver (io_driver.c's do_submit_or_sync) already had
 * exactly this kind of protection — a bounded PendingPool that gracefully
 * degrades to synchronous I/O once full — but branch 3 below, the only
 * place that pool is ever consulted, is unreachable as long as branch 2
 * unconditionally intercepts every op whenever a worker has a per-worker
 * backend registered, which is always true once the scheduler is running.
 * The Phase 4 per-worker/shared-reactor rewrite never carried an
 * equivalent forward.
 *
 * Rather than duplicate io_driver.c's platform-specific sync-fallback
 * logic here, cap concurrent per-worker-backend ops and let branch 2
 * simply decline to run once at capacity — falling through to branch 3,
 * which already does the right thing (its own bounded pool, then graceful
 * degrade to sync). This restores the old admission-control discipline by
 * reusing the code that already implements it correctly, instead of
 * inventing a second implementation of the same idea. */

/* Keep this comfortably below whatever CQ/completion-queue depth the
 * active per-worker backend provides (see RING_CQ_ENTRIES in
 * ty_io_uring.c) — the point is to make it structurally impossible for
 * outstanding ops to approach that ceiling, not to size it exactly to
 * match it. */
#define TY_IO_MAX_OUTSTANDING 2048

static _Atomic(int) g_backend_outstanding = 0;

/* Optimistic reserve-and-check: increment first, then back out if that put
 * us over the cap. A small amount of overshoot under concurrent callers is
 * fine here — this is a soft admission-control ceiling meant to keep
 * outstanding ops in the same order of magnitude as the completion queue,
 * not an exact hard limit — and this avoids a lock around the check. */
static int backend_try_reserve(void) {
    int reserved = atomic_fetch_add_explicit(&g_backend_outstanding, 1, memory_order_relaxed) + 1;
    if (reserved > TY_IO_MAX_OUTSTANDING) {
        atomic_fetch_sub_explicit(&g_backend_outstanding, 1, memory_order_relaxed);
        return 0;
    }
    return 1;
}

static void backend_release(void) {
    atomic_fetch_sub_explicit(&g_backend_outstanding, 1, memory_order_relaxed);
}

void ty_io_backend_note_submit_failed(void) {
    /* See worker_resume_coro() in scheduler.c: called when a deferred
     * per-worker-backend submit fails after this op was already reserved
     * below, so the reservation doesn't leak — without this, a submit
     * failure would permanently shrink the effective cap by one slot. */
    backend_release();
}

/* ── Wake callback used when polling per-worker backends ──────────────────── */

static void sched_wake(void* coro, int64_t result) {
    backend_release();
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
    if (w && w->io_backend && backend_try_reserve()) {
        TyIoBackend* be = w->io_backend;
        if (be->submit) {
            /* OLD: Do NOT call be->submit() here, for ANY op type including
             * ACCEPT. Handing the op to the kernel from inside this
             * coroutine's own stack, before it has actually parked
             * (ty_ctx_swap'd away), leaves a window where a completion
             * arrives fast enough (io_uring against already-buffered
             * loopback data, or an already-pending inbound connection,
             * can complete in microseconds) for another worker to wake
             * *and resume* this coroutine while this thread is still
             * physically executing on its stack — two OS threads
             * running the same coroutine concurrently.
             *
             * This used to be handled with a narrower, incomplete fix
             * that only marked BLOCKED before submit() and special-
             * cased ACCEPT out of it entirely (on the theory that
             * Listener__accept's separate submit()-then-park() call
             * sequence didn't need it) — both of those still had the
             * exact same race, just a smaller window for READ/WRITE and
             * the full original window for ACCEPT. Confirmed by two
             * separate crashes: the phase2 backpressure test's lost
             * final chunk (READ) and test_phase4_linux_1000_coroutines'
             * SIGSEGV under real multi-worker stealing load (ACCEPT).
             *
             * The only fix that actually closes this for every op type
             * is deferring the real submit until *after* this
             * coroutine's own ty_ctx_swap has captured its context —
             * see ty_io_park_coro_deferred() / worker_resume_coro() in
             * scheduler.c. Mark BLOCKED, hand the op to the scheduler,
             * and only resume here once a real result has been
             * delivered via ty_io_wake_coro(). Callers (including
             * Listener__accept) must NOT call ty_io_park_coro()
             * themselves anymore — this does the full submit+park for
             * them, for every op type. */
            ty_coro_set_blocked();
            if (be->readiness_based) {
                /* Readiness-based (kqueue, epoll): submit NOW, before parking.
                 * The kernel needs the interest registered so poll() can
                 * detect readiness. This is the classic pattern:
                 * submit() -> park() -> wake -> poll() -> callback. */
                int rc = be->submit(be, op);
                if (rc < 0) {
                    /* Submit failed before parking — no completion will
                     * ever arrive for this reservation, so release it
                     * here rather than leaking it (mirrors
                     * ty_io_backend_note_submit_failed() below for the
                     * deferred/completion-based case). */
                    backend_release();
                    return -1;
                }
                ty_coro_block_and_yield();
                return 0;
            } else {
                /* Completion-based (io_uring, IOCP): defer submit until after
                 * this coroutine's context is captured by ty_ctx_swap.
                 * See worker_resume_coro() for the actual submit. The
                 * reservation taken above is released either by
                 * sched_wake() on normal completion, or by
                 * ty_io_backend_note_submit_failed() if worker_resume_coro()
                 * finds the deferred submit itself failed. */
                ty_io_park_coro_deferred((SlabArena*)ty_current_arena(), be, (TyIoOp*)op);
                return 0;
            }
        }
        /* be->submit was NULL — a misconfigured backend, not an
         * over-capacity condition. Release the reservation before falling
         * through to the global driver so it isn't leaked. */
        backend_release();
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
         * tick. That retry-and-back-off is a real (if cruder than
         * READ/WRITE's synchronous fallback) form of throttling in its
         * own right — it naturally slows accept_loop_coro down instead
         * of accepting connections as fast as the kernel allows, which
         * is exactly the upstream firehose that overwhelms the read
         * side under load. No longer "rarely hit": this fires whenever
         * branch 2 is at TY_IO_MAX_OUTSTANDING capacity, which under
         * test_phase4_linux_1000_coroutines' fan-out is expected to be
         * fairly often, not an edge case. */
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

    TY_DEBUG("[io] ty_io_poll called\n");

    /* 1. Per-worker backend path */
    Worker* w = ty_sched_current_worker();
    if (w && w->io_backend) {
        TyIoBackend* be = w->io_backend;
        if (be->poll) {
            TY_DEBUG("[io] calling backend poll\n");
            return be->poll(be, sched_wake);
        }
    }

    /* 2. Global io_driver fallback */
    void* drv = ty_io_global_driver();
    if (!drv) return 0;
    TY_DEBUG("[io] calling global driver poll\n");
    ty_io_driver_poll(drv);
    return 0;
}
