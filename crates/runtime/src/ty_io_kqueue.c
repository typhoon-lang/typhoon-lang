/*
 * ty_io_kqueue.c — macOS kqueue backend for Typhoon IO
 *
 * One kqueue fd per scheduler worker thread.
 * submit() registers EVFILT_READ/EVFILT_WRITE with EV_ONESHOT + udata=op.
 * poll() calls kevent64 with zero timeout, then performs the actual
 * read()/write()/accept() syscall (kqueue is readiness-based), and wakes the coro.
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

#define MAX_EVENTS 64

/* Pending request pool — same pattern as io_driver.c */
#define POOL_CAP 256

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

/* ── lifecycle ────────────────────────────────────────────────────────────── */

TyKqBackend* ty_kq_backend_new(void) {
    TyKqBackend* b = (TyKqBackend*)malloc(sizeof(TyKqBackend));
    if (!b) return NULL;
    memset(b, 0, sizeof(*b));

    b->kq = kqueue();
    if (b->kq < 0) {
        free(b);
        return NULL;
    }

    b->base.impl = b;
    b->base.submit = kq_submit;
    b->base.poll = kq_poll;
    return b;
}

void ty_kq_backend_destroy(TyKqBackend* b) {
    if (!b) return;
    if (b->kq >= 0) {
        close(b->kq);
        b->kq = -1;
    }
    free(b);
}

#endif /* __APPLE__ */
