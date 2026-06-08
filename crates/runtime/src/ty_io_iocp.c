/*
 * ty_io_iocp.c — Windows IOCP backend for Typhoon IO
 *
 * Each scheduler worker thread owns one IOCP handle.
 * submit() posts overlapped ReadFile/WriteFile/WSARecv/WSASend.
 * For ACCEPT: posts AcceptEx overlapped I/O. When a connection arrives,
 * the completion arrives via GetQueuedCompletionStatus on the IOCP port,
 * and poll() wakes the parked coroutine with the accepted socket.
 *
 * AcceptEx is the proper IOCP-compatible accept mechanism. The plain
 * accept() function does not integrate with IOCP — on some Windows
 * versions/configurations, a non-blocking accept() in a poll loop
 * may never see connections that are in the kernel backlog, because
 * the IOCP port has no way to signal readability on the listener.
 * AcceptEx solves this by posting an overlapped operation that the
 * kernel completes when a connection arrives.
 *
 * After AcceptEx completes, setsockopt(SO_UPDATE_ACCEPT_CONTEXT) is
 * called on the accepted socket so that getsockname/getpeername work
 * and the socket inherits the listener's properties.
 */

#ifdef _WIN32

#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <winsock2.h>
#include <ws2tcpip.h>
#include <mswsock.h>
#include <string.h>
#include <stdlib.h>

#include "ty_io_iocp.h"
#include "io_driver.h"

#define MAX_DRAIN 64

/* ── IocpReq for READ/WRITE overlapped ops ─────────────────────────────── */

typedef struct {
    OVERLAPPED ol;
    TyIoOp op;
} IocpReq;

/* ── accept helpers ────────────────────────────────────────────────────── */

/*
 * Load the AcceptEx function pointer via WSAIoctl.
 * Called once per backend (lazy, on first ACCEPT submit).
 */
static int load_acceptex(TyIocpBackend* b) {
    if (b->acceptex_fn) return 0; /* already loaded */

    GUID guid = WSAID_ACCEPTEX;
    LPFN_ACCEPTEX fn = NULL;
    DWORD bytes = 0;
    /* Use a dummy socket for the WSAIoctl — any socket will do. */
    SOCKET tmp = socket(AF_INET, SOCK_STREAM, IPPROTO_TCP);
    int rc = WSAIoctl(tmp, SIO_GET_EXTENSION_FUNCTION_POINTER,
                      &guid, sizeof(guid),
                      &fn, sizeof(fn),
                      &bytes, NULL, NULL);
    closesocket(tmp);
    if (rc == SOCKET_ERROR) {
        b->acceptex_fn = NULL;
        return -1;
    }
    b->acceptex_fn = (void*)fn;
    return 0;
}

/*
 * Post an AcceptEx overlapped operation on the listener socket.
 * Creates a new accept socket, associates it with the IOCP port,
 * and calls AcceptEx. The completion will arrive via IOCP.
 */
static int post_acceptex(TyIocpBackend* b, SOCKET listen_sock, void* coro) {
    if (load_acceptex(b) < 0) return -1;

    /* Pre-create the accept socket — must be the same family as listener
     * and created with WSA_FLAG_OVERLAPPED for IOCP compatibility. */
    SOCKET accept_sock = WSASocketW(AF_INET, SOCK_STREAM, IPPROTO_TCP,
                                    NULL, 0, WSA_FLAG_OVERLAPPED);
    if (accept_sock == INVALID_SOCKET) {
        /* Try IPv6 if IPv4 fails (listener might be IPv6) */
        accept_sock = WSASocketW(AF_INET6, SOCK_STREAM, IPPROTO_TCP,
                                 NULL, 0, WSA_FLAG_OVERLAPPED);
    }
    if (accept_sock == INVALID_SOCKET) return -1;

    /* Associate accept socket with this IOCP port BEFORE AcceptEx */
    HANDLE h = CreateIoCompletionPort((HANDLE)accept_sock, b->iocp,
                                      0, 0);
    if (!h) {
        closesocket(accept_sock);
        return -1;
    }

    /* Also associate listener socket with this IOCP port — but only
     * once.  Re-associating a socket with a different IOCP port
     * silently moves it, which breaks whichever worker previously
     * owned it.  Since the accept coroutine runs on one worker, we
     * only need to associate on the first post. */
    if (!b->listener_associated) {
        CreateIoCompletionPort((HANDLE)listen_sock, b->iocp, 0, 0);
        b->listener_associated = 1;
    }

    /* Post AcceptEx */
    DWORD received = 0;
    memset(&b->accept_ol, 0, sizeof(b->accept_ol));

    LPFN_ACCEPTEX fn = (LPFN_ACCEPTEX)b->acceptex_fn;
    BOOL ok = fn(
        listen_sock,
        accept_sock,
        b->accept_buf,
        0,  /* dwReceiveDataLength=0 → complete as soon as connection arrives */
        sizeof(SOCKADDR_IN) + 16,   /* local addr space */
        sizeof(SOCKADDR_IN) + 16,   /* remote addr space */
        &received,
        &b->accept_ol);

    if (!ok) {
        DWORD err = WSAGetLastError();
        if (err != ERROR_IO_PENDING) {
            closesocket(accept_sock);
            return -1;
        }
        /* ERROR_IO_PENDING is normal — the operation is in progress */
    }

    b->listen_sock = listen_sock;
    b->accept_sock = accept_sock;
    b->accept_coro = coro;
    b->accept_pending = 1;
    return 0;
}

/* ── fd is socket check ──────────────────────────────────────────────── */

static int iocp_fd_is_socket(ty_fd_t fd) {
    SOCKET s = (SOCKET)fd;
    int type = 0;
    int len = (int)sizeof(type);
    return getsockopt(s, SOL_SOCKET, SO_TYPE, (char*)&type, &len) == 0;
}

/* ── submit ──────────────────────────────────────────────────────────────── */

static int iocp_submit(TyIoBackend* base, const TyIoOp* op) {
    TyIocpBackend* b = (TyIocpBackend*)base;
    if (!b || !b->iocp || !op) return -1;

    if (op->type == TY_IO_OP_ACCEPT) {
        /* ACCEPT: post AcceptEx overlapped. Completion arrives via IOCP
         * and poll() will wake the coroutine with the accepted socket. */
        return post_acceptex(b, (SOCKET)op->fd, op->coro);
    }

    /* Allocate OVERLAPPED + copy of op. Lifetime: until completion. */
    IocpReq* req = (IocpReq*)HeapAlloc(GetProcessHeap(), 0, sizeof(IocpReq));
    if (!req) return -1;
    memset(req, 0, sizeof(*req));
    req->op = *op;
    /* hEvent carries the TyIoOp* — retrieved on completion */
    req->ol.hEvent = (HANDLE)(uintptr_t)req;

    /* Associate fd/socket with this IOCP if not already. */
    HANDLE hFile = (HANDLE)(uintptr_t)op->fd;
    CreateIoCompletionPort(hFile, b->iocp, 0, 0);

    BOOL ok = FALSE;
    DWORD bytes = 0;
    int is_socket = iocp_fd_is_socket(op->fd);

    if (is_socket && op->type == TY_IO_OP_READ) {
        WSABUF wb;
        DWORD flags = 0;
        wb.buf = (CHAR*)op->buf;
        wb.len = (ULONG)op->len;
        ok = (WSARecv((SOCKET)op->fd, &wb, 1, &bytes, &flags, &req->ol, NULL) == 0);
    } else if (is_socket && op->type == TY_IO_OP_WRITE) {
        WSABUF wb;
        wb.buf = (CHAR*)op->buf;
        wb.len = (ULONG)op->len;
        ok = (WSASend((SOCKET)op->fd, &wb, 1, &bytes, 0, &req->ol, NULL) == 0);
    } else if (op->type == TY_IO_OP_READ) {
        ok = ReadFile(hFile, op->buf, (DWORD)op->len, &bytes, &req->ol);
    } else if (op->type == TY_IO_OP_WRITE) {
        ok = WriteFile(hFile, op->buf, (DWORD)op->len, &bytes, &req->ol);
    } else {
        HeapFree(GetProcessHeap(), 0, req);
        return -1;
    }

    (void)bytes;
    if (!ok) {
        DWORD err = is_socket ? (DWORD)WSAGetLastError() : GetLastError();
        DWORD pending = is_socket ? WSA_IO_PENDING : ERROR_IO_PENDING;
        if (err != pending) {
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

    /* ── Drain IOCP completions ───────────────────────────────────────── */

    OVERLAPPED_ENTRY entries[MAX_DRAIN];
    ULONG n = 0;
    BOOL ok = GetQueuedCompletionStatusEx(
        b->iocp, entries, MAX_DRAIN, &n, 0, FALSE);

    if (ok && n > 0) {
        for (ULONG i = 0; i < n; i++) {
            OVERLAPPED* ol = entries[i].lpOverlapped;
            DWORD transferred = entries[i].dwNumberOfBytesTransferred;

            /* Check if this is our pending AcceptEx completion */
            if (b->accept_pending && ol == &b->accept_ol) {
                b->accept_pending = 0;
                SOCKET accepted = b->accept_sock;
                SOCKET listener = b->listen_sock;
                void* coro = b->accept_coro;

                if (entries[i].dwNumberOfBytesTransferred == 0 &&
                    ol->Internal != 0) {
                    /* AcceptEx failed — error in ol->Internal (NTSTATUS) */
                    int64_t result = -(int64_t)(ol->Internal & 0xFFFF);
                    closesocket(accepted);
                    if (wake && coro)
                        wake(coro, result);
                    else
                        ty_io_wake_coro(coro, result);
                } else {
                    /* AcceptEx succeeded — update the accept context
                     * so getsockname/getpeername work and the socket
                     * inherits listener properties. */
                    setsockopt(accepted, SOL_SOCKET, SO_UPDATE_ACCEPT_CONTEXT,
                               (char*)&listener, sizeof(listener));

                    int64_t result = (int64_t)accepted;
                    if (wake && coro)
                        wake(coro, result);
                    else
                        ty_io_wake_coro(coro, result);
                }
                count++;
                continue;
            }

            /* Regular READ/WRITE completion */
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
    b->acceptex_fn = NULL;
    b->listen_sock = INVALID_SOCKET;
    b->accept_sock = INVALID_SOCKET;
    b->accept_pending = 0;
    b->listener_associated = 0;
    b->accept_coro = NULL;

    b->base.impl = b;
    b->base.submit = iocp_submit;
    b->base.poll = iocp_poll;
    return b;
}

void ty_iocp_backend_destroy(TyIocpBackend* b) {
    if (!b) return;
    if (b->accept_pending && b->accept_sock != INVALID_SOCKET) {
        closesocket(b->accept_sock);
        b->accept_sock = INVALID_SOCKET;
        b->accept_pending = 0;
    }
    if (b->iocp) {
        CloseHandle(b->iocp);
        b->iocp = NULL;
    }
    free(b);
}

#endif /* _WIN32 */
