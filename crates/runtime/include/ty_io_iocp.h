/*
 * ty_io_iocp.h — Windows IOCP backend for Typhoon IO
 *
 * One IOCP handle per scheduler worker thread.
 * submit() posts overlapped ReadFile/WriteFile/WSARecv/WSASend.
 * For ACCEPT: uses WSAEventSelect + FD_ACCEPT event for non-blocking
 * readiness detection. poll() drains completions and calls wake().
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
  HANDLE accept_event;   /* WSAEventSelect event for FD_ACCEPT */
  SOCKET accept_sock;    /* current listener being watched */
  void* accept_req;      /* pending IocpReq for accept, or NULL */
} TyIocpBackend;

/* Create an IOCP backend for the calling worker thread. */
TyIocpBackend* ty_iocp_backend_new(void);

/* Destroy and release IOCP handle. */
void ty_iocp_backend_destroy(TyIocpBackend* b);

#ifdef __cplusplus
}
#endif

#endif /* _WIN32 */
