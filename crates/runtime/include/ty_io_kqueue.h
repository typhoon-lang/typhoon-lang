/*
 * ty_io_kqueue.h — macOS kqueue backend for Typhoon IO
 *
 * One kqueue fd per scheduler worker thread.
 * submit() registers EVFILT_READ/EVFILT_WRITE with EV_ONESHOT.
 * poll() calls kevent64 with zero timeout, performs the syscall, wakes coro.
 */
#pragma once

#include "ty_io_backend.h"

#ifdef __APPLE__

#ifdef __cplusplus
extern "C" {
#endif

typedef struct TyKqBackend {
    TyIoBackend base;
    int kq;
} TyKqBackend;

/* Create a kqueue backend for the calling worker thread. */
TyKqBackend* ty_kq_backend_new(void);

/* Destroy and close kqueue fd. */
void ty_kq_backend_destroy(TyKqBackend* b);

#ifdef __cplusplus
}
#endif

#endif /* __APPLE__ */
