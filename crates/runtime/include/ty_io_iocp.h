/*
 * ty_io_iocp.h — Windows IOCP backend for Typhoon IO
 *
 * REWRITTEN: one shared IOCP handle for the whole process, not one per
 * scheduler worker thread — see ty_io_iocp.c's header comment for the
 * full rationale (in short: a handle can only ever be bound to ONE IOCP
 * port for its lifetime, so a socket accepted while bound to worker A's
 * port would silently fail every subsequent operation once the
 * coroutine holding it got stolen to worker B, which tried to
 * re-associate it with a *different* port).
 *
 * submit() posts overlapped ReadFile/WriteFile/WSARecv/WSASend against
 * the one shared port. For ACCEPT: posts AcceptEx overlapped; the
 * completion arrives via IOCP and can be picked up by ANY worker's
 * poll() call, not just the one that submitted it.
 *
 * accept_sock/accept_ol/accept_coro/accept_pending/listener_associated
 * moved out of this struct entirely (see ty_io_iocp.c's AcceptReq) —
 * they used to be singleton fields here because each backend only ever
 * had one worker submitting through it, so at most one pending accept
 * made sense to track inline. With a single shared backend, multiple
 * workers can each have their own listener with its own pending accept
 * in flight *concurrently*, so accept state now has to be allocated
 * per-call instead of living as fixed fields on the backend itself —
 * the same reason regular reads/writes already used a heap-allocated
 * IocpReq instead of fields on the backend.
 */

#pragma once

#include "ty_io_backend.h"

#ifdef _WIN32

#ifndef WIN32_LEAN_AND_MEAN
#define WIN32_LEAN_AND_MEAN
#endif
#include <windows.h>
#include <winsock2.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct TyIocpBackend {
    TyIoBackend base;
    HANDLE iocp;        /* the one shared port every worker submits/polls through */
    void* acceptex_fn;  /* LPFN_ACCEPTEX — loaded via WSAIoctl once, shared */
} TyIocpBackend;

/* Get (creating on first call) the shared IOCP backend. Every worker
 * calls this once during its own startup, same call site as before this
 * rewrite — the only thing that changed is every caller now gets the
 * SAME pointer back instead of a fresh one, with an internal refcount
 * tracking how many workers are holding a reference. */
TyIocpBackend* ty_iocp_backend_new(void);

/* Release this worker's reference. Only actually closes the IOCP
 * handle once every worker that called ty_iocp_backend_new() has
 * called this too. */
void ty_iocp_backend_destroy(TyIocpBackend* b);

#ifdef __cplusplus
}
#endif

#endif /* _WIN32 */
