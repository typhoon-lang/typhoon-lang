/*
 * ty_io_kqueue.c — macOS kqueue backend for Typhoon IO
 *
 * REWRITTEN: single shared kqueue fd for the whole process, not one per
 * scheduler worker thread. This is the direct macOS translation of the
 * shared-reactor pattern Go's netpoller and Tokio's mio reactor both
 * use — kevent() is explicitly documented as safe for concurrent calls
 * from multiple threads against the same kq fd, so this is the natural
 * fit, not a workaround.
 *
 * Why this changed from one-kq-per-worker: with a private kq per
 * worker, an op registered by whichever worker happened to call
 * submit() could only ever be seen by that SAME worker's own poll()
 * loop. A coroutine that submitted a read and then got stolen to a
 * different worker (completely normal, expected work-stealing) left
 * its readiness event sitting unseen in its *original* worker's kq
 * until that worker got back around to polling — a latency/fairness
 * gap, not a hard failure the way the equivalent bug was on Windows
 * (see ty_io_iocp.c's rewrite), but a real gap all the same, present
 * on every op that outlives a steal.
 *
 * submit() registers EVFILT_READ/EVFILT_WRITE with EV_ONESHOT + udata=op
 * against the one shared kq. poll() calls kevent64 with zero timeout,
 * then performs the actual read()/write()/accept() syscall (kqueue is
 * readiness-based, not completion-based), and wakes the coro. Whichever
 * worker happens to call poll() next picks up whatever's ready,
 * regardless of which worker originally submitted it — the kernel's
 * own readiness delivery does the work-stealing-equivalent distribution
 * for us, so nothing else in this file needed to change to get that
 * property.
 */

#ifdef __APPLE__

#include <sys/types.h>
#include <sys/event.h>
#include <sys/time.h>
#include <unistd.h>
#include <errno.h>
#include <string.h>
#include <stdlib.h>
#include <sys/socket.h>

#include "ty_io_kqueue.h"
#include "io_driver.h"
#include "atomic.h"
#include "platform.h" /* TyMutex — guards the singleton create/destroy path */

#define MAX_EVENTS 64

/* Pending request pool — already process-global before this rewrite,
 * not per-worker, so no change needed here at all. Every in-flight op
 * (read/write/accept alike) gets a slot from this single pool
 * regardless of which worker submitted it or which worker's poll()
 * eventually drains its completion. */
#define POOL_CAP 1024

typedef struct {
    _Atomic(int) in_use;
    TyIoOp op;
} KqPending;

static KqPending g_pool[POOL_CAP];

static KqPending* pool_alloc(void) {
    for (int i = 0; i < POOL_CAP; i++) {
        int exp = 0;
        if (atomic_compare_exchange_strong_explicit(
                &g_pool[i].in_use, &exp, 1,
                memory_order_acquire, memory_order_relaxed))
            return &g_pool[i];
    }
    return NULL;
}

static void pool_free(KqPending* r) {
    atomic_store_explicit(&r->in_use, 0, memory_order_release);
}

/* ── submit ──────────────────────────────────────────────────────────────── */

static int kq_submit(TyIoBackend* base, const TyIoOp* op) {
    TyKqBackend* b = (TyKqBackend*)base;
    if (!b || b->kq < 0 || !op) return -1;

    KqPending* req = pool_alloc();
    if (!req) return -1;
    req->op = *op;

    struct kevent64_s ev;
    memset(&ev, 0, sizeof(ev));
    /* Phase 4: op->fd is ty_fd_t; cast to uint64 for kevent ident */
    ev.ident = (uint64_t)op->fd;
    if (op->type == TY_IO_OP_WRITE)
        ev.filter = EVFILT_WRITE;
    else
        /* EVFILT_READ for both READ and ACCEPT — listener becomes
         * readable when a connection is pending. */
        ev.filter = EVFILT_READ;
    ev.flags = EV_ADD | EV_ENABLE | EV_ONESHOT;
    ev.udata = (uint64_t)(uintptr_t)req;

    /* b->kq is now the one shared fd every worker submits against —
     * kevent64() is documented safe for concurrent multi-thread use on
     * the same kq, no locking needed here unlike io_uring's SQ ring. */
    int n = kevent64(b->kq, &ev, 1, NULL, 0, 0, NULL);
    if (n < 0) {
        pool_free(req);
        return -1;
    }
    return 0;
}

/* ── poll ─────────────────────────────────────────────────────────────────── */

static int kq_poll(TyIoBackend* base, TySchedWakeFn wake) {
    TyKqBackend* b = (TyKqBackend*)base;
    if (!b || b->kq < 0) return 0;

    struct kevent64_s events[MAX_EVENTS];
    struct timespec ts = { 0, 0 }; /* zero timeout — non-blocking */
    /* Whichever worker calls this next drains whatever's ready on the
     * shared kq, regardless of which worker originally submitted it —
     * this is the actual fix. kevent64() itself handles safely handing
     * out ready events to concurrent callers; no additional
     * synchronization needed on this side either. */
    int n = kevent64(b->kq, NULL, 0, events, MAX_EVENTS, 0, &ts);
    if (n <= 0) return 0;

    for (int i = 0; i < n; i++) {
        KqPending* req = (KqPending*)(uintptr_t)events[i].udata;
        if (!req) continue;

        TyIoOp* op = &req->op;
        int64_t result;

        /* kqueue is readiness-based: perform the actual syscall now. */
        if (op->type == TY_IO_OP_WRITE) {
            ssize_t w = write((int)op->fd, op->buf, op->len);
            result = (w < 0) ? -(int64_t)errno : (int64_t)w;
        } else if (op->type == TY_IO_OP_ACCEPT) {
            /* Listener may be readable — call accept() to get the new fd.
             * Spurious EVFILT_READ is possible (e.g. listen backlog
             * signal).  If accept() returns EAGAIN, re-register the
             * kevent and do NOT wake the coroutine. */
            int c = accept((int)op->fd, NULL, NULL);
            if (c < 0 && (errno == EAGAIN || errno == EWOULDBLOCK)) {
                /* Spurious wake — re-arm EVFILT_READ and skip wake. */
                struct kevent64_s re_ev;
                memset(&re_ev, 0, sizeof(re_ev));
                re_ev.ident = (uint64_t)op->fd;
                re_ev.filter = EVFILT_READ;
                re_ev.flags = EV_ADD | EV_ENABLE | EV_ONESHOT;
                re_ev.udata = (uint64_t)(uintptr_t)req;
                kevent64(b->kq, &re_ev, 1, NULL, 0, 0, NULL);
                continue; /* skip wake + pool_free — req stays alive */
            }
            result = (c < 0) ? -(int64_t)errno : (int64_t)c;
        } else {
            /* TY_IO_OP_READ */
            ssize_t r = read((int)op->fd, op->buf, op->len);
            result = (r < 0) ? -(int64_t)errno : (int64_t)r;
        }

        if (wake && op->coro)
            wake(op->coro, result);
        else
            ty_io_wake_coro(op->coro, result);

        pool_free(req);
    }

    return n;
}

/* ── shared singleton kq fd ──────────────────────────────────────────────
 *
 * Every worker calls ty_kq_backend_new() once during its own startup
 * (unchanged call site — the scheduler doesn't need to know this became
 * a singleton). First caller actually creates the kqueue fd; every
 * caller after that gets the SAME TyKqBackend* back and a bumped
 * refcount. ty_kq_backend_destroy() only actually closes the fd once
 * every worker that got a reference has released it.
 *
 * Double-checked locking under g_lock: cheap fast path once created
 * (a single atomic load), only ever contends during the brief startup
 * window while workers are spinning up.
 * ────────────────────────────────────────────────────────────────────── */

static TyMutex g_lock;
static TyKqBackend* g_backend = NULL;
static int g_refcount = 0;

/* Guards first-ever init of g_lock itself. A plain int flag here would
 * be a genuine data race if two threads ever called
 * ty_kq_backend_new() for the first time concurrently — both could see
 * "not yet initialized" and both call ty_mutex_init() on the same
 * TyMutex simultaneously, which is undefined behavior. The current
 * scheduler.c happens to call this from a single serial loop on the
 * main thread during ty_sched_init(), not concurrently from each
 * worker's own thread, so this race isn't live against today's actual
 * caller — but a singleton's whole point is to be correct regardless
 * of caller pattern, not to quietly depend on that. CAS-based
 * once-guard instead: exactly one thread wins the race to actually
 * call ty_mutex_init(); everyone else spins on the state flag until
 * it's ready, rather than racing the init call itself. Same atomic
 * style already used by this file's own KqPending pool above. */
static _Atomic(int) g_lock_state = 0; /* 0=uninit, 1=initializing, 2=ready */

static void ensure_lock_inited(void) {
    int expected = 0;
    if (atomic_compare_exchange_strong_explicit(&g_lock_state, &expected, 1,
            memory_order_acq_rel, memory_order_acquire)) {
        ty_mutex_init(&g_lock);
        atomic_store_explicit(&g_lock_state, 2, memory_order_release);
        return;
    }
    while (atomic_load_explicit(&g_lock_state, memory_order_acquire) != 2) {
        /* busy-wait — this window is one mutex_init() call, contended
         * only during the brief process-startup race, if ever. */
    }
}

TyKqBackend* ty_kq_backend_new(void) {
    ensure_lock_inited();
    ty_mutex_lock(&g_lock);

    if (g_backend) {
        g_refcount++;
        ty_mutex_unlock(&g_lock);
        return g_backend;
    }

    TyKqBackend* b = (TyKqBackend*)malloc(sizeof(TyKqBackend));
    if (!b) {
        ty_mutex_unlock(&g_lock);
        return NULL;
    }
    memset(b, 0, sizeof(*b));

    b->kq = kqueue();
    if (b->kq < 0) {
        free(b);
        ty_mutex_unlock(&g_lock);
        return NULL;
    }

    b->base.impl = b;
    b->base.submit = kq_submit;
    b->base.poll = kq_poll;
    b->base.readiness_based = 1;

    g_backend = b;
    g_refcount = 1;
    ty_mutex_unlock(&g_lock);
    return b;
}

void ty_kq_backend_destroy(TyKqBackend* b) {
    if (!b) return;
    ensure_lock_inited();
    ty_mutex_lock(&g_lock);

    if (b != g_backend) {
        /* Not the singleton — shouldn't happen given every caller gets
         * the same pointer back from ty_kq_backend_new(), but fail
         * safe rather than double-free something unexpected. */
        ty_mutex_unlock(&g_lock);
        return;
    }

    g_refcount--;
    if (g_refcount > 0) {
        ty_mutex_unlock(&g_lock);
        return; /* other workers still holding a reference */
    }

    if (b->kq >= 0) {
        close(b->kq);
        b->kq = -1;
    }
    free(b);
    g_backend = NULL;
    ty_mutex_unlock(&g_lock);
}

#endif /* __APPLE__ */
