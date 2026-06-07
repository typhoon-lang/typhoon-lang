/*
 * ty_io_iocp.c — Windows IOCP backend for Typhoon IO
 *
 * Each scheduler worker thread owns one IOCP handle.
 * submit() posts overlapped ReadFile/WriteFile/WSARecv/WSASend.
 * For ACCEPT: registers FD_ACCEPT via WSAEventSelect, then stores
 * the pending op in the backend. poll() checks the event first;
 * when signalled, calls accept() and wakes the coroutine.
 *
 * The OVERLAPPED struct carries the TyIoOp* pointer in hEvent
 * (unused by Windows when IOCP is in use).
 */

#ifdef _WIN32

#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <winsock2.h>
#include <ws2tcpip.h>
#include <string.h>
#include <stdlib.h>

#include "ty_io_iocp.h"
#include "io_driver.h"

#define MAX_DRAIN 64

/* ── submit ──────────────────────────────────────────────────────────────── */

static int iocp_submit(TyIoBackend* base, const TyIoOp* op) {
  TyIocpBackend* b = (TyIocpBackend*)base;
  if (!b || !b->iocp || !op) return -1;

  /* Allocate OVERLAPPED + copy of op. Lifetime: until completion. */
  typedef struct {
    OVERLAPPED ol;
    TyIoOp op;
  } IocpReq;

  IocpReq* req = (IocpReq*)HeapAlloc(GetProcessHeap(), 0, sizeof(IocpReq));
  if (!req) return -1;
  memset(req, 0, sizeof(*req));
  req->op = *op;
  /* hEvent carries the TyIoOp* — retrieved on completion */
  req->ol.hEvent = (HANDLE)(uintptr_t)req;

  if (op->type == TY_IO_OP_ACCEPT) {
    /* ACCEPT: register FD_ACCEPT via WSAEventSelect so Windows
     * signals the event when a connection is pending. Store the
     * op for poll() to check. No IOCP completion posted — poll()
     * checks the accept state before draining IOCP completions. */
    SOCKET s = (SOCKET)op->fd;

    if (b->accept_event == NULL) {
      b->accept_event = CreateEventW(NULL, TRUE, FALSE, NULL);
    }
    if (b->accept_event) {
      WSAEventSelect(s, b->accept_event, FD_ACCEPT);
      ResetEvent(b->accept_event);
    }
    b->accept_sock = s;
    b->accept_req = req;
    return 0;
  }

  /* Associate fd with this IOCP if not already. */
  HANDLE hFile = (HANDLE)(uintptr_t)op->fd;
  CreateIoCompletionPort(hFile, b->iocp, 0, 0);

  BOOL ok = FALSE;
  DWORD bytes = 0;

  if (op->type == TY_IO_OP_READ) {
    ok = ReadFile(hFile, op->buf, (DWORD)op->len, &bytes, &req->ol);
  } else if (op->type == TY_IO_OP_WRITE) {
    ok = WriteFile(hFile, op->buf, (DWORD)op->len, &bytes, &req->ol);
  } else {
    HeapFree(GetProcessHeap(), 0, req);
    return -1;
  }

  if (ok) {
    PostQueuedCompletionStatus(b->iocp, bytes, 0, &req->ol);
  } else {
    DWORD err = GetLastError();
    if (err != ERROR_IO_PENDING) {
      ty_io_wake_coro(op->coro, -(int64_t)err);
      HeapFree(GetProcessHeap(), 0, req);
      return 0;
    }
  }

  return 0;
}

/* ── poll ─────────────────────────────────────────────────────────────────── */

static int iocp_poll(TyIoBackend* base, TySchedWakeFn wake) {
  TyIocpBackend* b = (TyIocpBackend*)base;
  if (!b || !b->iocp) return 0;

  int count = 0;

  /* ── Check pending ACCEPT first (non-blocking) ──────────────────────
   * WSAEventSelect fires accept_event when a connection arrives.
   * WaitForSingleObject(timeout=0) is non-blocking. */
  if (b->accept_req && b->accept_event) {
    DWORD wait = WaitForSingleObject(b->accept_event, 0);
    if (wait == WAIT_OBJECT_0) {
      /* Event signalled — a connection is pending. */
      WSANETWORKEVENTS ne;
      WSAEnumNetworkEvents(b->accept_sock, b->accept_event, &ne);

      typedef struct {
        OVERLAPPED ol;
        TyIoOp op;
      } IocpReq;
      IocpReq* req = (IocpReq*)b->accept_req;
      TyIoOp* op = &req->op;

      SOCKET c = accept(b->accept_sock, NULL, NULL);
      int64_t result;
      if (c != INVALID_SOCKET) {
        result = (int64_t)c;
      } else {
        int err = WSAGetLastError();
        if (err == WSAEWOULDBLOCK) {
          /* Spurious — event fired but accept still would block.
           * Reset and wait for next signal. */
          ResetEvent(b->accept_event);
          /* Don't free req — it stays pending for next poll. */
        } else {
          result = -(int64_t)err;
          if (wake && op->coro)
            wake(op->coro, result);
          else
            ty_io_wake_coro(op->coro, result);
          HeapFree(GetProcessHeap(), 0, req);
          b->accept_req = NULL;
          b->accept_sock = INVALID_SOCKET;
          count++;
        }
      }

      if (c != INVALID_SOCKET) {
        if (wake && op->coro)
          wake(op->coro, result);
        else
          ty_io_wake_coro(op->coro, result);
        HeapFree(GetProcessHeap(), 0, req);
        b->accept_req = NULL;
        b->accept_sock = INVALID_SOCKET;
        count++;
      }
    }
    /* If event not signalled: accept stays pending, checked next poll.
     * Worker thread is NOT blocked — it moves on to IOCP drain. */
  }

  /* ── Drain IOCP completions ───────────────────────────────────────── */

  OVERLAPPED_ENTRY entries[MAX_DRAIN];
  ULONG n = 0;
  BOOL ok = GetQueuedCompletionStatusEx(
    b->iocp, entries, MAX_DRAIN, &n, 0, FALSE);

  if (ok && n > 0) {
    for (ULONG i = 0; i < n; i++) {
      OVERLAPPED* ol = entries[i].lpOverlapped;
      DWORD transferred = entries[i].dwNumberOfBytesTransferred;

      typedef struct {
        OVERLAPPED ol;
        TyIoOp op;
      } IocpReq;
      IocpReq* req = (IocpReq*)(uintptr_t)ol->hEvent;
      if (!req) continue;

      TyIoOp* op = &req->op;
      int64_t result;
      if (transferred == 0 && ol->Internal != 0) {
        result = -(int64_t)(ol->Internal & 0xFFFF);
      } else {
        result = (int64_t)transferred;
      }

      if (wake && op->coro) {
        wake(op->coro, result);
      } else {
        ty_io_wake_coro(op->coro, result);
      }

      HeapFree(GetProcessHeap(), 0, req);
      count++;
    }
  }

  return count;
}

/* ── lifecycle ────────────────────────────────────────────────────────────── */

TyIocpBackend* ty_iocp_backend_new(void) {
  TyIocpBackend* b = (TyIocpBackend*)malloc(sizeof(TyIocpBackend));
  if (!b) return NULL;
  memset(b, 0, sizeof(*b));

  b->iocp = CreateIoCompletionPort(INVALID_HANDLE_VALUE, NULL, 0, 1);
  if (b->iocp == NULL) {
    free(b);
    return NULL;
  }
  b->accept_event = NULL;
  b->accept_req = NULL;
  b->accept_sock = INVALID_SOCKET;

  b->base.impl = b;
  b->base.submit = iocp_submit;
  b->base.poll = iocp_poll;
  return b;
}

void ty_iocp_backend_destroy(TyIocpBackend* b) {
  if (!b) return;
  if (b->accept_req) {
    HeapFree(GetProcessHeap(), 0, b->accept_req);
    b->accept_req = NULL;
  }
  if (b->accept_event) {
    CloseHandle(b->accept_event);
    b->accept_event = NULL;
  }
  if (b->iocp) {
    CloseHandle(b->iocp);
    b->iocp = NULL;
  }
  free(b);
}

#endif /* _WIN32 */
