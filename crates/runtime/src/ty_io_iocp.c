/*
 * ty_io_iocp.c — Windows IOCP backend for Typhoon IO
 *
 * REWRITTEN: one shared IOCP handle for the whole process instead of
 * one per scheduler worker thread.
 *
 * Why: a handle can only ever be bound to ONE IOCP port for its whole
 * lifetime — Windows does not support re-associating it with a
 * different port later. With the old one-port-per-worker design, a
 * socket accepted while a coroutine was running on worker A got
 * permanently bound to worker A's port. If that coroutine was then
 * stolen to worker B (completely normal, expected work-stealing) and
 * tried to submit its next op — say, a read — from worker B, that
 * submission would call CreateIoCompletionPort() again on the SAME
 * socket, but against worker B's DIFFERENT port. That call fails
 * (silently — its return value was never checked), and the ensuing
 * WSARecv() would come back with WSAGetLastError() == ERROR_INVALID_HANDLE
 * (6). This was reproduced directly: a two-coroutine loopback test
 * whose server coroutine got stolen mid-flight failed its read with
 * exactly this code, immediately after a successful accept() on a
 * different worker than the one that initiated it.
 *
 * With a single shared port, every worker's CreateIoCompletionPort()
 * call targets the SAME port every time — Windows documents
 * re-associating a handle with a port it's ALREADY on as a safe no-op,
 * unlike associating with a different one. And any worker's poll() can
 * now see any other worker's completions, which is the actual point:
 * it closes both the hard failure above AND the softer version of the
 * same problem (a completion sitting unseen until its *originating*
 * worker specifically gets back around to polling).
 *
 * submit() posts overlapped ReadFile/WriteFile/WSARecv/WSASend.
 * For ACCEPT: posts AcceptEx overlapped I/O. When a connection arrives,
 * the completion arrives via GetQueuedCompletionStatus on the shared
 * IOCP port, and poll() (called by whichever worker gets there first)
 * wakes the parked coroutine with the accepted socket.
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
#include <io.h> /* _get_osfhandle — File-fd fix in iocp_submit, carried
                    forward from before this rewrite */

#include "ty_io_iocp.h"
#include "io_driver.h"
#include "atomic.h"   /* _Atomic — singleton create-lock once-guard */
#include "platform.h" /* TyMutex — guards the singleton create/destroy path */

/* TyFile definition (must match ty_io.c) — needed for FILE_FLAG_OVERLAPPED
 * position tracking in iocp_submit and iocp_poll. Kept here to avoid
 * exposing TyFile in the public ty_io.h while still allowing the IOCP
 * backend to access the file position field. */
struct TyFile {
    int fd;
    int closed;
#ifdef _WIN32
    int64_t pos; /* file position for FILE_FLAG_OVERLAPPED handles */
#endif
};
typedef struct TyFile TyFile;

#define MAX_DRAIN 64

/* ── request kinds, and the shared header every request struct starts
 * with ──────────────────────────────────────────────────────────────
 *
 * Accept tracking moved from fixed fields on TyIocpBackend (fine when
 * each backend had exactly one worker submitting through it, so at most
 * one pending accept made sense) to a dynamically-allocated AcceptReq
 * per call — necessary now that multiple workers can each have their
 * own listener with its own pending accept in flight concurrently
 * through the SAME shared backend.
 *
 * Both IocpReq and AcceptReq start with the same `kind` field at
 * offset 0, so iocp_poll() can safely peek just that field through a
 * shared IocpReqHeader* before deciding which full type ol->hEvent's
 * back-pointer actually refers to. */

typedef enum {
    IOCP_REQ_RW = 1,
    IOCP_REQ_ACCEPT = 2,
} IocpReqKind;

typedef struct {
    OVERLAPPED ol;    /* MUST be first — GQCS hands this pointer back */
    IocpReqKind kind; /* fixed offset from &ol, no hEvent abuse */
} IocpReqHeader;

typedef struct {
    IocpReqHeader hdr;
    TyIoOp op;
} IocpReq;

typedef struct {
    IocpReqHeader hdr;
    SOCKET listen_sock;
    SOCKET accept_sock;
    void* coro;
    char accept_buf[256]; /* AcceptEx output buffer (addrs + optional data) */
} AcceptReq;

/* ── accept helpers ────────────────────────────────────────────────────── */

/*
 * Load the AcceptEx function pointer via WSAIoctl.
 * Called once total now (shared backend, shared cache), same
 * lazy-load-on-first-use pattern as before this rewrite.
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
        DWORD e = WSAGetLastError();
        TY_DEBUG("[iocp] load_acceptex: WSAIoctl failed, wsa_err=%lu\n", (unsigned long)e);
        b->acceptex_fn = NULL;
        return -1;
    }
    b->acceptex_fn = (void*)fn;
    return 0;
}

/*
 * Post an AcceptEx overlapped operation on the listener socket.
 * Creates a new accept socket, associates both it and the listener
 * with the shared IOCP port, and calls AcceptEx. The completion will
 * arrive via IOCP and may be picked up by any worker's poll().
 */
static int post_acceptex(TyIocpBackend* b, SOCKET listen_sock, void* coro) {
    if (load_acceptex(b) < 0) return -1;

    AcceptReq* req = (AcceptReq*)HeapAlloc(GetProcessHeap(), 0, sizeof(AcceptReq));
    if (!req) return -1;
    memset(req, 0, sizeof(*req));
    req->hdr.kind = IOCP_REQ_ACCEPT;

    /* Pre-create the accept socket — must be the same family as listener
     * and created with WSA_FLAG_OVERLAPPED for IOCP compatibility. */
    SOCKET accept_sock = WSASocketW(AF_INET, SOCK_STREAM, IPPROTO_TCP,
                                    NULL, 0, WSA_FLAG_OVERLAPPED);
    if (accept_sock == INVALID_SOCKET) {
        /* Try IPv6 if IPv4 fails (listener might be IPv6) */
        accept_sock = WSASocketW(AF_INET6, SOCK_STREAM, IPPROTO_TCP,
                                 NULL, 0, WSA_FLAG_OVERLAPPED);
    }
    if (accept_sock == INVALID_SOCKET) {
        TY_DEBUG("[iocp] post_acceptex: WSASocketW failed, wsa_err=%lu\n",
            (unsigned long)WSAGetLastError());
        HeapFree(GetProcessHeap(), 0, req);
        return -1;
    }

    /* Associate accept socket with the shared IOCP port BEFORE AcceptEx.
     * Safe to call unconditionally now (was previously guarded by a
     * listener_associated flag on the backend, to avoid re-associating
     * the LISTENER a second time — but that flag only made sense when
     * each backend had one worker calling this once per listener.
     * CreateIoCompletionPort() on a handle already bound to THIS SAME
     * port is documented as a safe no-op, so with one shared port,
     * always calling it is both simpler and correct for however many
     * listeners exist across however many workers. */
    HANDLE h = CreateIoCompletionPort((HANDLE)accept_sock, b->iocp, 0, 0);
    if (!h) {
        TY_DEBUG("[iocp] post_acceptex: CreateIoCompletionPort(accept_sock) failed, err=%lu\n",
            (unsigned long)GetLastError());
        closesocket(accept_sock);
        HeapFree(GetProcessHeap(), 0, req);
        return -1;
    }
    CreateIoCompletionPort((HANDLE)listen_sock, b->iocp, 0, 0);

    /* Post AcceptEx */
    DWORD received = 0;

    LPFN_ACCEPTEX fn = (LPFN_ACCEPTEX)b->acceptex_fn;
    BOOL ok = fn(
        listen_sock,
        accept_sock,
        req->accept_buf,
        0,  /* dwReceiveDataLength=0 → complete as soon as connection arrives */
        sizeof(SOCKADDR_IN) + 16,   /* local addr space */
        sizeof(SOCKADDR_IN) + 16,   /* remote addr space */
        &received,
        &req->hdr.ol);

    if (!ok) {
        DWORD err = WSAGetLastError();
        if (err != ERROR_IO_PENDING) {
            TY_DEBUG("[iocp] post_acceptex: AcceptEx failed, wsa_err=%lu listen_sock=%llu accept_sock=%llu\n",
                (unsigned long)err, (unsigned long long)listen_sock, (unsigned long long)accept_sock);
            closesocket(accept_sock);
            HeapFree(GetProcessHeap(), 0, req);
            return -1;
        }
        /* ERROR_IO_PENDING is normal — the operation is in progress */
    }

    req->listen_sock = listen_sock;
    req->accept_sock = accept_sock;
    req->coro = coro;
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
        /* ACCEPT: post AcceptEx overlapped. Completion arrives via the
         * shared IOCP port and any worker's poll() can pick it up. */
        return post_acceptex(b, (SOCKET)op->fd, op->coro);
    }

    /* is_socket has to be checked BEFORE hFile is computed, not after —
     * op->fd means two different things depending on what's calling in.
     * For sockets (ty_net.c), it's a real SOCKET, which on Windows *is*
     * directly castable to HANDLE — CreateIoCompletionPort/WSARecv/
     * WSASend all accept it that way, no conversion needed. For files
     * (ty_io.c's File__read/write, which build TyIoOp with
     * `op.fd = self->fd`), it's a CRT file descriptor from _open() — a
     * small integer indexed into the C runtime's own fd table, NOT a
     * HANDLE. _get_osfhandle() does the real fd-to-HANDLE lookup in the
     * CRT's table for that case. (Carried forward unchanged from before
     * this rewrite — this part of the bug history is independent of
     * the shared-port change above.) */
    int is_socket = iocp_fd_is_socket(op->fd);
    HANDLE hFile;
    if (is_socket) {
        hFile = (HANDLE)(uintptr_t)op->fd;
    } else {
        hFile = (HANDLE)_get_osfhandle((int)op->fd);
        if (hFile == INVALID_HANDLE_VALUE) {
            ty_io_wake_coro(op->coro, -(int64_t)ERROR_INVALID_HANDLE);
            return 0;
        }
    }

    /* Associate fd/socket with the shared IOCP port if not already —
     * and now that every worker targets the SAME port, this is always
     * either the handle's first (successful) association or a safe
     * no-op re-association with the port it's already on. This is the
     * actual fix for the cross-worker bug described in this file's
     * header comment: before this rewrite, this call could target a
     * DIFFERENT port than whichever one the handle was originally bound
     * to, and failed silently. */
    CreateIoCompletionPort(hFile, b->iocp, 0, 0);

    /* Allocate OVERLAPPED + copy of op. Lifetime: until completion. */
    IocpReq* req = (IocpReq*)HeapAlloc(GetProcessHeap(), 0, sizeof(IocpReq));
    if (!req) return -1;
    memset(req, 0, sizeof(*req));
    req->hdr.kind = IOCP_REQ_RW;
    req->op = *op;

    /* For non-socket files with FILE_FLAG_OVERLAPPED, set file offset from TyFile.pos */
    if (!is_socket && op->file_ptr) {
        TyFile* file = (TyFile*)op->file_ptr;
        req->hdr.ol.Offset = (DWORD)(file->pos & 0xFFFFFFFF);
        req->hdr.ol.OffsetHigh = (DWORD)(file->pos >> 32);
    }

    BOOL ok = FALSE;
    DWORD bytes = 0;

    if (is_socket && op->type == TY_IO_OP_READ) {
        WSABUF wb;
        DWORD flags = 0;
        wb.buf = (CHAR*)op->buf;
        wb.len = (ULONG)op->len;
        {
            struct sockaddr_storage peer;
            int peer_len = (int)sizeof(peer);
            int peer_rc = getpeername((SOCKET)op->fd, (struct sockaddr*)&peer, &peer_len);
            int peer_err = peer_rc == 0 ? 0 : WSAGetLastError();
            TY_DEBUG("[iocp] read submit fd=%d len=%lu getpeername rc=%d wsa_err=%d peer_len=%d\n",
                     (int)op->fd, (unsigned long)wb.len, peer_rc, peer_err, peer_len);
        }
        ok = (WSARecv((SOCKET)op->fd, &wb, 1, &bytes, &flags, &req->hdr.ol, NULL) == 0);
        TY_DEBUG("[iocp] WSARecv ret=%d bytes=%lu flags=%lu last_err=%lu\n",
                 ok, (unsigned long)bytes, (unsigned long)flags,
                 (unsigned long)WSAGetLastError());
    } else if (is_socket && op->type == TY_IO_OP_WRITE) {
        WSABUF wb;
        wb.buf = (CHAR*)op->buf;
        wb.len = (ULONG)op->len;
        ok = (WSASend((SOCKET)op->fd, &wb, 1, &bytes, 0, &req->hdr.ol, NULL) == 0);
    } else if (op->type == TY_IO_OP_READ) {
        ok = ReadFile(hFile, op->buf, (DWORD)op->len, &bytes, &req->hdr.ol);
    } else if (op->type == TY_IO_OP_WRITE) {
        ok = WriteFile(hFile, op->buf, (DWORD)op->len, &bytes, &req->hdr.ol);
    } else {
        HeapFree(GetProcessHeap(), 0, req);
        return -1;
    }

    if (!ok) {
        DWORD err = is_socket ? (DWORD)WSAGetLastError() : GetLastError();
        DWORD pending = is_socket ? WSA_IO_PENDING : ERROR_IO_PENDING;
        if (err != pending) {
            ty_io_wake_coro(op->coro, -(int64_t)err);
            HeapFree(GetProcessHeap(), 0, req);
            return 0;
        }
    } else {
        /* IOCP still queues a completion for overlapped operations that
         * complete immediately (unless FILE_SKIP_COMPLETION_PORT_ON_SUCCESS
         * was explicitly requested, which we never do). Let poll() deliver
         * the real dwNumberOfBytesTransferred value instead of trusting the
         * synchronous bytes out-param, which can be 0 on overlapped sockets
         * even when the queued completion carries data. */
        (void)bytes;
    }

    return 0;
}

/* ── poll ─────────────────────────────────────────────────────────────────── */

static int iocp_poll(TyIoBackend* base, TySchedWakeFn wake) {
    TyIocpBackend* b = (TyIocpBackend*)base;
    if (!b || !b->iocp) return 0;

    int count = 0;

    /* ── Drain shared-port completions ────────────────────────────────
     * GetQueuedCompletionStatusEx on a port shared by every worker is
     * exactly the pattern this rewrite exists for: whichever worker
     * calls this next drains whatever's ready, including completions
     * for coroutines that started on a completely different worker. No
     * additional locking needed here — IOCP itself is documented safe
     * for concurrent multi-thread GetQueuedCompletionStatus[Ex] calls
     * against the same port; that safety guarantee is the whole reason
     * this platform didn't need io_uring's explicit locks. */

    OVERLAPPED_ENTRY entries[MAX_DRAIN];
    ULONG n = 0;
    BOOL ok = GetQueuedCompletionStatusEx(
        b->iocp, entries, MAX_DRAIN, &n, 0, FALSE);

    if (ok && n > 0) {
        for (ULONG i = 0; i < n; i++) {
            OVERLAPPED* ol = entries[i].lpOverlapped;
            if (!ol) continue;
            DWORD transferred = entries[i].dwNumberOfBytesTransferred;

            IocpReqHeader* hdr = (IocpReqHeader*)ol;

            if (hdr->kind == IOCP_REQ_ACCEPT) {
                AcceptReq* req = (AcceptReq*)hdr;
                SOCKET accepted = req->accept_sock;
                SOCKET listener = req->listen_sock;
                void* coro = req->coro;

                if (transferred == 0 && ol->Internal != 0) {
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
                HeapFree(GetProcessHeap(), 0, req);
                count++;
                continue;
            }

            /* Regular READ/WRITE completion */
            IocpReq* req = (IocpReq*)hdr;
            TyIoOp* op = &req->op;
            int64_t result;
            if (transferred == 0 && ol->Internal != 0) {
                result = -(int64_t)(ol->Internal & 0xFFFF);
            } else {
                result = (int64_t)transferred;
                /* Update file position for FILE_FLAG_OVERLAPPED files */
                if (op->file_ptr) {
                    TyFile* file = (TyFile*)op->file_ptr;
                    file->pos += transferred;
                }
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

/* ── shared singleton port ───────────────────────────────────────────────
 *
 * Every worker calls ty_iocp_backend_new() once during its own startup
 * (unchanged call site). First caller actually creates the IOCP port;
 * every caller after that gets the SAME TyIocpBackend* back and a
 * bumped refcount. ty_iocp_backend_destroy() only actually closes the
 * port once every worker that got a reference has released it.
 * ────────────────────────────────────────────────────────────────────── */

static TyMutex g_create_lock;
/* Same fix as ty_io_kqueue.c's g_lock_state — see that file's comment
 * for the full reasoning. A plain int flag here is a genuine race if
 * ever called concurrently; not live against today's scheduler.c
 * (serial call loop in ty_sched_init()), but shouldn't be depended on. */
static _Atomic(int) g_lock_state = 0; /* 0=uninit, 1=initializing, 2=ready */
static TyIocpBackend* g_backend = NULL;
static int g_refcount = 0;

static void ensure_create_lock_inited(void) {
    int expected = 0;
    if (atomic_compare_exchange_strong_explicit(&g_lock_state, &expected, 1,
            memory_order_acq_rel, memory_order_acquire)) {
        ty_mutex_init(&g_create_lock);
        atomic_store_explicit(&g_lock_state, 2, memory_order_release);
        return;
    }
    while (atomic_load_explicit(&g_lock_state, memory_order_acquire) != 2) {
        /* busy-wait — one mutex_init() call, startup-only contention */
    }
}

TyIocpBackend* ty_iocp_backend_new(void) {
    ensure_create_lock_inited();
    ty_mutex_lock(&g_create_lock);

    if (g_backend) {
        g_refcount++;
        ty_mutex_unlock(&g_create_lock);
        return g_backend;
    }

    TyIocpBackend* b = (TyIocpBackend*)malloc(sizeof(TyIocpBackend));
    if (!b) {
        ty_mutex_unlock(&g_create_lock);
        return NULL;
    }
    memset(b, 0, sizeof(*b));

    b->iocp = CreateIoCompletionPort(INVALID_HANDLE_VALUE, NULL, 0, 1);
    if (b->iocp == NULL) {
        free(b);
        ty_mutex_unlock(&g_create_lock);
        return NULL;
    }
    b->acceptex_fn = NULL;

    b->base.impl = b;
    b->base.submit = iocp_submit;
    b->base.poll = iocp_poll;
    b->base.readiness_based = 0;

    g_backend = b;
    g_refcount = 1;
    ty_mutex_unlock(&g_create_lock);
    return b;
}

void ty_iocp_backend_destroy(TyIocpBackend* b) {
    if (!b) return;
    ensure_create_lock_inited();
    ty_mutex_lock(&g_create_lock);

    if (b != g_backend) {
        ty_mutex_unlock(&g_create_lock);
        return;
    }

    g_refcount--;
    if (g_refcount > 0) {
        ty_mutex_unlock(&g_create_lock);
        return; /* other workers still holding a reference */
    }

    if (b->iocp) {
        CloseHandle(b->iocp);
        b->iocp = NULL;
    }
    free(b);
    g_backend = NULL;
    ty_mutex_unlock(&g_create_lock);
}

#endif /* _WIN32 */
