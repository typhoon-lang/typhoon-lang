/*
 * ty_io_iocp.h — Windows IOCP backend for Typhoon IO
 *
 * One IOCP handle per scheduler worker thread.
 * submit() posts overlapped ReadFile/WriteFile/WSARecv/WSASend.
 * For ACCEPT: posts AcceptEx overlapped; completion arrives via IOCP.
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
    HANDLE iocp;
    void* acceptex_fn;          /* LPFN_ACCEPTEX — loaded via WSAIoctl */
    SOCKET listen_sock;         /* listener socket for pending AcceptEx */
    SOCKET accept_sock;         /* pre-created socket for AcceptEx */
    OVERLAPPED accept_ol;       /* OVERLAPPED for pending AcceptEx */
    char accept_buf[256];       /* AcceptEx output buffer (addrs + optional data) */
    void* accept_coro;          /* coroutine to wake on completion */
    int accept_pending;         /* 1 = AcceptEx posted and awaiting completion */
    int listener_associated;    /* 1 = listen_sock already on this IOCP port */
} TyIocpBackend;

/* Create an IOCP backend for the calling worker thread. */
TyIocpBackend* ty_iocp_backend_new(void);

/* Destroy and release IOCP handle. */
void ty_iocp_backend_destroy(TyIocpBackend* b);

#ifdef __cplusplus
}
#endif

#endif /* _WIN32 */
