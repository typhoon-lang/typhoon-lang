/*
 * ty_net.c — minimal capability-gated networking for Typhoon
 *
 * Phase 4: global socket registries (g_sockets, g_listeners, g_sock_lock)
 * replaced with per-worker TyFdSet tracking. Shutdown delegates to
 * scheduler's ty_fdset_close_all per worker.
 *
 * Notes:
 * - Listener sockets are set O_NONBLOCK so accept() never blocks the
 *   worker thread.  When no connection is pending, the coroutine parks
 *   via TyIoOp ACCEPT and the IO backend wakes it on readability.
 * - `task` is accepted for future slab allocation; currently unused.
 * - Address parsing supports "host:port" (IPv4 / hostname). IPv6 literals
 * are not supported yet.
 */

#include "ty_net.h"
#include "scheduler.h"
#include "platform.h"
#include "io_driver.h"
#include "ty_io_backend.h"
#include <string.h>
#include <stdlib.h>
#include <stdio.h>

#if defined(_WIN32)
# define WIN32_LEAN_AND_MEAN
# include <winsock2.h>
# include <ws2tcpip.h>
# include <windows.h>
# pragma comment(lib, "Ws2_32.lib")
typedef SOCKET ty_sock_t;
static int32_t ty_net_last_error(void) { return (int32_t)WSAGetLastError(); }
static void ty_sock_close(ty_sock_t s) { closesocket(s); }
static void ty_sock_force_shutdown(ty_sock_t s) { shutdown(s, SD_BOTH); }

static const char* ty_net_errstr_win32(int32_t code, char* buf, size_t buf_len) {
    if (buf_len == 0) return "";
    buf[0] = '\0';
    DWORD flags = FORMAT_MESSAGE_FROM_SYSTEM | FORMAT_MESSAGE_IGNORE_INSERTS;
    DWORD n = FormatMessageA(
        flags,
        NULL,
        (DWORD)code,
        MAKELANGID(LANG_NEUTRAL, SUBLANG_DEFAULT),
        buf,
        (DWORD)buf_len,
        NULL);
    if (n == 0) {
        (void)snprintf(buf, buf_len, "WinSock error %ld", (long)code);
    }
    return buf;
}
#else
# include <errno.h>
# include <unistd.h>
# include <sys/types.h>
# include <sys/socket.h>
# include <netdb.h>
# include <arpa/inet.h>
# include <fcntl.h>
typedef int ty_sock_t;
static int32_t ty_net_last_error(void) { return (int32_t)errno; }
static void ty_sock_close(ty_sock_t s) { close(s); }
static void ty_sock_force_shutdown(ty_sock_t s) { shutdown(s, SHUT_RDWR); }

static const char* ty_net_errstr_errno(int32_t code, char* buf, size_t buf_len) {
    (void)buf;
    (void)buf_len;
    return strerror(code);
}
#endif

#if defined(_WIN32)
# define TY_SOCK_INVALID INVALID_SOCKET
#else
# define TY_SOCK_INVALID ((ty_sock_t)(-1))
#endif

/* TyResult_i32_i32 is now defined in ty_net.h */

struct TyNetwork { uint32_t _tag; };
struct TyListener { ty_sock_t sock; };
struct TySocket { ty_sock_t sock; int closed; };

static TyNetwork g_net = { 0x4E45544Eu }; /* 'NETN' */
static int g_initialized = 0;

void ty_net_init(void) {
    if (!g_initialized) {
        g_initialized = 1;
    }
#if defined(_WIN32)
    WSADATA wsa;
    (void)WSAStartup(MAKEWORD(2, 2), &wsa);
#endif
}

void ty_net_shutdown(void) {
    /*
     * Phase 4: per-worker fd cleanup is handled by ty_sched_shutdown(),
     * which calls ty_fdset_close_all on every worker's fd_set.
     * Here we only do the platform-level network teardown.
     */
#if defined(_WIN32)
    (void)WSACleanup();
#endif
    g_initialized = 0;
}


TyNetwork* ty_net_global(void) {
    return &g_net;
}

static int split_host_port(const char* addr, char** host_out, char** port_out) {
    if (!addr) return 0;
    const char* last_colon = strrchr(addr, ':');
    if (!last_colon) return 0;
    size_t host_len = (size_t)(last_colon - addr);
    const char* port = last_colon + 1;
    if (*port == '\0') return 0;

    char* host = (char*)malloc(host_len + 1);
    if (!host) return 0;
    memcpy(host, addr, host_len);
    host[host_len] = '\0';

    *host_out = host;
    *port_out = (char*)port;
    return 1;
}

/* ── Set socket non-blocking ────────────────────────────────────────────── */

static void ty_sock_set_nonblock(ty_sock_t s) {
#if defined(_WIN32)
    u_long mode = 1;
    (void)ioctlsocket(s, FIONBIO, &mode);
#else
    int flags = fcntl(s, F_GETFL, 0);
    if (flags >= 0) {
        (void)fcntl(s, F_SETFL, flags | O_NONBLOCK);
    }
#endif
}

void __ty_rt__Network__listen(void* task, TyNetwork* self, char* addr, TyResult_Listener_i32* out) {
    (void)task;
    (void)self;

    TyResult_Listener_i32 result;
    result.tag = 1;
    result.value = NULL;
    result.err = -1;

    TY_DEBUG("[net] listen enter addr_ptr=%p out_ptr=%p\n", (void*)addr, &result);

    char* host = NULL;
    char* port = NULL;
    if (!split_host_port(addr, &host, &port)) {
        result.err = -2;
        TY_DEBUG("[net] listen invalid addr=\"%s\" (expected host:port)\n",
            addr ? addr : "(null)");
        *out = result;
        return;
    }

    struct addrinfo hints;
    memset(&hints, 0, sizeof(hints));
    /* Use AF_INET for wildcard addresses (0.0.0.0 / empty host) to ensure
     * IPv4 loopback connections work. AF_UNSPEC returns IPv6 first, and
     * even with IPV6_V6ONLY=0 some CI environments don't properly support
     * IPv4-mapped IPv6 connections on IPv6 listeners. For explicit IPv6
     * addresses (e.g. "[::]") we still use AF_UNSPEC to allow IPv6. */
    if (host[0] == '\0' || strcmp(host, "0.0.0.0") == 0) {
        hints.ai_family = AF_INET;
    } else {
        hints.ai_family = AF_UNSPEC;
    }
    hints.ai_socktype = SOCK_STREAM;
    hints.ai_protocol = IPPROTO_TCP;
    hints.ai_flags = AI_PASSIVE;

    struct addrinfo* res = NULL;
    int gai = getaddrinfo((host[0] == '\0') ? NULL : host, port, &hints, &res);
    if (gai != 0 || !res) {
        free(host);
        result.err = (int32_t)gai;
#if defined(_WIN32)
        TY_DEBUG("[net] listen getaddrinfo failed addr=\"%s\" gai=%d (%s)\n",
            addr ? addr : "(null)", gai, gai_strerrorA(gai));
#else
        TY_DEBUG("[net] listen getaddrinfo failed addr=\"%s\" gai=%d (%s)\n",
            addr ? addr : "(null)", gai, gai_strerror(gai));
#endif
        *out = result;
        return;
    }

    ty_sock_t s = (ty_sock_t)(-1);
    int32_t last_err = 0;
    struct addrinfo* it = res;
    for (; it; it = it->ai_next) {
#if defined(_WIN32)
        s = (ty_sock_t)WSASocketW(it->ai_family, it->ai_socktype, it->ai_protocol, NULL, 0, WSA_FLAG_OVERLAPPED);
#else
        s = (ty_sock_t)socket(it->ai_family, it->ai_socktype, it->ai_protocol);
#endif
#if defined(_WIN32)
        if (s == INVALID_SOCKET) { last_err = ty_net_last_error(); continue; }
#else
        if (s < 0) { last_err = ty_net_last_error(); continue; }
#endif

        int yes = 1;
        (void)setsockopt(s, SOL_SOCKET, SO_REUSEADDR, (const char*)&yes, (socklen_t)sizeof(yes));

        /* For IPv6 sockets: explicitly enable dual-stack so IPv4 loopback
         * connections (127.0.0.1) reach the listener via mapped addresses.
         * macOS defaults to V6ONLY=0 but some CI environments override it;
         * Windows also defaults to V6ONLY=0 but we set it explicitly. */
        if (it->ai_family == AF_INET6) {
            int v6only = 0;
#if defined(_WIN32)
            setsockopt(s, IPPROTO_IPV6, IPV6_V6ONLY,
                       (const char*)&v6only, (socklen_t)sizeof(v6only));
#else
            setsockopt(s, IPPROTO_IPV6, IPV6_V6ONLY,
                       &v6only, (socklen_t)sizeof(v6only));
#endif
        }

        if (bind(s, it->ai_addr, (socklen_t)it->ai_addrlen) != 0) {
            last_err = ty_net_last_error();
            TY_DEBUG("[net] Listen failed: bind: %s\n", strerror(last_err));
            ty_sock_close(s);
            s = (ty_sock_t)(-1);
            continue;
        }
        if (listen(s, 128) != 0) {
            last_err = ty_net_last_error();
            TY_DEBUG("[net] Listen failed: listen: %s\n", strerror(last_err));
            ty_sock_close(s);
            s = (ty_sock_t)(-1);
            continue;
        }

        /* Set the listener socket non-blocking so accept() never stalls
         * the worker thread.  The coroutine parks via TyIoOp ACCEPT and
         * the IO backend wakes it when a connection is pending. */
        /* On Windows: do NOT set FIONBIO on the listener. AcceptEx
         * uses overlapped I/O for async notification — FIONBIO
         * conflicts with IOCP completion and prevents AcceptEx
         * completions from arriving. The IOCP backend handles async
         * accept entirely via AcceptEx; non-blocking is unnecessary. */
#if !defined(_WIN32)
        ty_sock_set_nonblock(s);
#endif

        break;
    }

    freeaddrinfo(res);
    free(host);

#if defined(_WIN32)
    if (s == INVALID_SOCKET) {
        result.err = last_err ? last_err : ty_net_last_error();
        char msg[256];
        TY_DEBUG("[net] listen failed addr=\"%s\" wsa=%ld (%s)\n",
            addr ? addr : "(null)", (long)result.err,
            ty_net_errstr_win32(result.err, msg, sizeof(msg)));
        *out = result;
        return;
    }
#else
    if (s < 0) {
        result.err = last_err ? last_err : ty_net_last_error();
        TY_DEBUG("[net] listen failed addr=\"%s\" errno=%d (%s)\n",
            addr ? addr : "(null)", (int)result.err, ty_net_errstr_errno(result.err, NULL, 0));
        *out = result;
        return;
    }
#endif

    TyListener* listener = (TyListener*)malloc(sizeof(TyListener));
    if (!listener) {
        ty_sock_close(s);
        result.err = -3;
        TY_DEBUG("[net] listen OOM allocating listener for addr=\"%s\"\n",
            addr ? addr : "(null)");
        *out = result;
        return;
    }
    listener->sock = s;

    /* Phase 4: register fd in per-worker TyFdSet */
    Worker* w = ty_sched_current_worker();
    if (w) {
        ty_fdset_add(&w->fd_set, (ty_fd_t)s);
    }

    result.tag = 0;
    result.value = listener;
    result.err = 0;
    *out = result;
}

void __ty_rt__Listener__accept(void* task, TyListener* self, TyResult_Socket_i32* out) {
    TyResult_Socket_i32 result;
    result.tag = 1;
    result.value = NULL;
    result.err = -1;

    if (!self) {
        result.err = -2;
        *out = result;
        return;
    }

    /* ── Async accept path (inside coroutine with IO backend) ────────────
     *
     * The listener socket is O_NONBLOCK (set at listen() time), so
     * accept() returns immediately.  If no connection is pending we
     * submit a TY_IO_OP_ACCEPT to the per-worker backend, park the
     * coroutine, and resume when the backend signals readability.
     * The backend's poll() callback calls accept() and stores the
     * accepted fd as the io_result.
     */
    void* drv = ty_io_global_driver();
    void* coro = ty_current_coro_raw();

    if (drv && coro) {
        for (;;) {
#if defined(_WIN32)
            /* On Windows, skip the initial accept() call entirely.
             * The listener socket is NOT non-blocking (FIONBIO would
             * break AcceptEx/IOCP), so accept() would block the worker
             * thread. Instead, go directly to ty_io_submit() which
             * posts AcceptEx via the per-worker IOCP backend. */
            (void)0;
#else
            ty_sock_t c = (ty_sock_t)accept(self->sock, NULL, NULL);
            if (c >= 0) {
                /* Accepted socket inherits O_NONBLOCK from listener on
                 * Linux; on other platforms we set it explicitly so
                 * async read/write paths work correctly. */
#ifdef __APPLE__
                ty_sock_set_nonblock(c);
#endif
                TySocket* sock = (TySocket*)malloc(sizeof(TySocket));
                if (!sock) {
                    ty_sock_close(c);
                    result.err = -3;
                    *out = result;
                    return;
                }
                sock->sock = c;
                sock->closed = 0;

                Worker* w = ty_sched_current_worker();
                if (w) {
                    ty_fdset_add(&w->fd_set, (ty_fd_t)c);
                }

                result.tag = 0;
                result.value = sock;
                result.err = 0;
                *out = result;
                return;
            }

            /* accept() would block — submit async and park */
            if (!(errno == EAGAIN || errno == EWOULDBLOCK)) {
                result.err = ty_net_last_error();
                *out = result;
                return;
            }
#endif /* _WIN32 / POSIX */

            TY_DEBUG("[net] accept: would block, parking coro=%p\n", coro);
            TyIoOp op;
            memset(&op, 0, sizeof(op));
            op.type = TY_IO_OP_ACCEPT;
            op.fd = (ty_fd_t)self->sock;
            op.buf = NULL;
            op.len = 0;
            op.coro = coro;
            int submit_rc = ty_io_submit(&op);
            if (submit_rc < 0) {
                /* No backend handles ACCEPT — yield and retry.
                 * This happens when only the global io_driver is active
                 * (no per-worker backend).  The coroutine yields so the
                 * scheduler can poll IO and retry accept() later. */
                ty_yield();
                continue;
            }
            ty_io_park_coro((SlabArena*)task);

            /* Resumed — io_result holds the accepted fd (>=0) or error (<0).
             * The backend poll() performed the actual accept() syscall and
             * stored the resulting fd as the io_result.  On error the result
             * is the negative errno/WSA error. */
            int64_t io_result = ty_io_take_result(coro);
            TY_DEBUG("[net] accept: resumed coro=%p result=%lld\n", coro, (long long)io_result);
            if (io_result < 0) {
                int32_t wake_err = (int32_t)(-io_result);
#if defined(_WIN32)
                if (wake_err == WSAEWOULDBLOCK) {
                    continue;
                }
#else
                if (wake_err == EAGAIN || wake_err == EWOULDBLOCK) {
                    continue;
                }
                /* EINVAL can come from io_uring POLL_ADD on older kernels
                 * or from accept() on a socket that lost its listen state.
                 * Fall back to yield-and-retry instead of aborting. */
                if (wake_err == EINVAL) {
                    ty_yield();
                    continue;
                }
#endif
                result.err = wake_err;
                *out = result;
                return;
            }

            ty_sock_t accepted = (ty_sock_t)io_result;
            /* Set accepted socket non-blocking for async read/write. */
            ty_sock_set_nonblock(accepted);
            TySocket* sock = (TySocket*)malloc(sizeof(TySocket));
            if (!sock) {
                ty_sock_close(accepted);
                result.err = -3;
                *out = result;
                return;
            }
            sock->sock = accepted;
            sock->closed = 0;

            Worker* w = ty_sched_current_worker();
            if (w) {
                ty_fdset_add(&w->fd_set, (ty_fd_t)accepted);
            }

            result.tag = 0;
            result.value = sock;
            result.err = 0;
            *out = result;
            return;
        }
    }

    /* ── Sync fallback (outside coroutine) ─────────────────────────────── */
    ty_sock_t c = (ty_sock_t)(-1);
    c = (ty_sock_t)accept(self->sock, NULL, NULL);
#if defined(_WIN32)
    if (c == INVALID_SOCKET) {
        result.err = ty_net_last_error();
        *out = result;
        return;
    }
#else
    if (c < 0) {
        result.err = ty_net_last_error();
        *out = result;
        return;
    }
#endif

    /* Accepted socket needs non-blocking for async IO. */
    ty_sock_set_nonblock(c);

    TySocket* sock = (TySocket*)malloc(sizeof(TySocket));
    if (!sock) {
        ty_sock_close(c);
        result.err = -3;
        *out = result;
        return;
    }
    sock->sock = c;
    sock->closed = 0;

    /* Phase 4: register fd in per-worker TyFdSet */
    Worker* w = ty_sched_current_worker();
    if (w) {
        ty_fdset_add(&w->fd_set, (ty_fd_t)c);
    }

    result.tag = 0;
    result.value = sock;
    result.err = 0;
    *out = result;
    return;
}

/* ── Socket__consume ─────────────────────────────────────────────────────────
 *
 * Spawns a background coroutine that reads chunks from self->sock and sends
 * each byte into `ch`, then closes `ch` on EOF or error.
 *
 * Phase 4: reads 4096-byte chunks via ty_io_read (async when driver present),
 * falls back to blocking recv() otherwise. Backpressure via channel send.
 */

typedef struct {
    TySocket* socket;
    struct TyChan* chan;
} TyConsumeCtx;

static void ty_socket_reader_coro(void* task, void* arg) {
    TyConsumeCtx* ctx = (TyConsumeCtx*)arg;
    TySocket* self = ctx->socket;
    struct TyChan* ch = ctx->chan;
    free(ctx); /* closure is no longer needed once unpacked */

    const size_t CHUNK = 4096;
    for (;;) {
        /* Phase 4: read self->sock directly — closed flag provides safety,
         * no global lock needed. Each socket is owned by one coroutine. */
        ty_sock_t fd = self ? self->sock : TY_SOCK_INVALID;

        if (fd == TY_SOCK_INVALID)
            break; /* socket was closed externally */

        /* Allocate slab buffer from task arena. */
        SlabArena* arena = (SlabArena*)task;
        int32_t cls = size_to_class(CHUNK);
        char* buf = (char*)slab_alloc(arena, cls);
        if (!buf) break; /* OOM on slab (rare) */

        int64_t got = 0;
        void* drv = ty_io_global_driver();
        void* coro = ty_current_coro_raw();
        if (drv && coro) {
            /* async path: submit via driver and park until completion */
            ty_io_read(drv, arena, coro, fd, (uint8_t*)buf, CHUNK);
            got = ty_io_take_result(coro);
        } else {
#if defined(_WIN32)
            int n = recv((SOCKET)fd, buf, (int)CHUNK, 0);
            got = (int64_t)n;
#else
            ssize_t n;
            do { n = recv(fd, buf, CHUNK, 0); } while (n < 0 && errno == EINTR);
            got = (int64_t)n;
#endif
        }

        if (got <= 0) break; /* EOF or error */

        /* Send each byte into channel; backpressure applies at ty_chan_send */
        for (int64_t i = 0; i < got; i++) {
            int8_t b = (int8_t)buf[i];
            ty_chan_send(arena, ch, &b);
        }
        /* slab buffer reclaimed when arena freed; no free needed */
    }
    ty_chan_close(ch);
}

void __ty_rt__Socket__consume(void* task, TySocket* self, void* ch) {
    if (!self || !ch) {
        if (ch) ty_chan_close((struct TyChan*)ch);
        return;
    }

    TyConsumeCtx* ctx = (TyConsumeCtx*)malloc(sizeof(TyConsumeCtx));
    if (!ctx) {
        /* OOM: close the channel immediately so the receiver sees EOF. */
        ty_chan_close((struct TyChan*)ch);
        return;
    }
    ctx->socket = self;
    ctx->chan = (struct TyChan*)ch;

    /* ty_spawn(arena, fn_ptr, arg_ptr) → new coroutine.
     * Passing the caller's task as the arena shares the same slab pool,
     * matching how connection handler coros are spawned elsewhere. */
    ty_spawn((SlabArena*)task, ty_socket_reader_coro, (void*)ctx);
}

/*
 * Socket__recv — blocking receive.
 *
 * Blocks the calling coroutine until a byte arrives on the channel or the
 * channel is closed (remote EOF / error). Returns:
 * tag=0, value=byte — byte received, connection still open
 * tag=1, err=0 — channel closed (EOF), caller should stop reading
 * tag=1, err=-1 — self or chan is NULL (programming error)
 *
 * Maps to: let Some(i) = ch.recv() else { break; }
 */
void __ty_rt__Socket__recv(void* task, TySocket* self, struct TyChan* chan, TyResult_i32_i32* out) {
    TyResult_i32_i32 result;
    result.tag = 1;
    result.value = 0;
    result.err = -1;

    if (!self || !chan) { *out = result; return; }

    int8_t byte = 0;
    /* ty_chan_recv now returns 1 (data) or -1 (closed/EOF). */
    int status = ty_chan_recv(task, chan, &byte);
    if (status == -1) {
        /* Channel closed — signal EOF cleanly, not as an error. */
        result.err = 0;
        *out = result;
        return;
    }

    result.tag = 0;
    result.value = (int32_t)(uint8_t)byte;
    result.err = 0;
    *out = result;
}

/*
 * Socket__try_recv — non-blocking receive.
 *
 * Returns immediately whether or not a byte is available:
 * tag=0, value=byte — byte received
 * tag=1, err=1 — would block (no data yet, connection still open)
 * tag=1, err=0 — channel closed (EOF)
 * tag=1, err=-1 — self or chan is NULL
 *
 * Maps to: let Some(i) = ch.try_recv() else { break; }
 * Callers MUST distinguish err=1 (retry later) from err=0 (stop reading).
 */
void __ty_rt__Socket__try_recv(void* task, TySocket* self, struct TyChan* chan, TyResult_i32_i32* out) {
    TyResult_i32_i32 result;
    result.tag = 1;
    result.value = 0;
    result.err = -1;

    if (!self || !chan) { *out = result; return; }

    int8_t byte = 0;
    /* ty_chan_try_recv returns:
     * 1 — data received (TY_CHAN_OK)
     * 0 — empty, still open (TY_CHAN_EMPTY) → err=1 (would block)
     * -1 — channel closed (EOF) (TY_CHAN_CLOSED) → err=0 */
    int status = ty_chan_try_recv(task, chan, &byte);
    if (status == 1) {
        result.tag = 0;
        result.value = (int32_t)(uint8_t)byte;
        result.err = 0;
    } else if (status == 0) {
        result.err = 1; /* would block — not an error, not EOF */
    } else {
        result.err = 0; /* -1 == closed — EOF */
    }
    *out = result;
}

void __ty_rt__Socket__write(void* task, TySocket* self, char* buf, int32_t len, TyResult_i32_i32* out) {
    (void)task;
    TyResult_i32_i32 result;
    result.tag = 1;
    result.value = 0;
    result.err = -1;

    if (!self || !buf) { *out = result; return; }

    /* Phase 4: use async driver when inside a coroutine. */
    SlabArena* arena = (SlabArena*)task;
    void* drv = ty_io_global_driver();
    void* coro = ty_current_coro_raw();
    if (drv && coro) {
        ty_io_write(drv, arena, coro, self->sock, (const uint8_t*)buf, (size_t)len);
        int64_t r = ty_io_take_result(coro);
        if (r < 0) {
            result.err = (int32_t)(-r);
            *out = result;
            return;
        }
        result.tag = 0;
        result.value = (int32_t)r;
        result.err = 0;
        *out = result;
        return;
    }

    /* sync fallback */
    int r = send(self->sock, buf, len, 0);
    if (r < 0) {
        result.err = ty_net_last_error();
        *out = result;
        return;
    }

    result.tag = 0;
    result.value = r;
    result.err = 0;
    *out = result;
}

/*
 * Socket__read — Phase 4 canonical async read via TyIoOp.
 *
 * Reads up to `cap` bytes into slab-allocated `buf`. When inside a coroutine
 * with the IO driver present, submits via TyIoOp and parks; otherwise blocking.
 * Returns (Socket, buf, Result<Int32, IoError>) — Socket returned so liveness
 * checker can track the resource after the call.
 */
void __ty_rt__Socket__read(void* task, TySocket* self, char* buf, int32_t cap, TyResult_i32_i32* out) {
    TyResult_i32_i32 result;
    result.tag = 1;
    result.value = 0;
    result.err = -1;

    if (!self || !buf) { *out = result; return; }

    SlabArena* arena = (SlabArena*)task;
    void* coro = ty_current_coro_raw();

    if (coro) {
        /* Submit via TyIoOp — the canonical Phase 4 path. */
        TyIoOp op;
        memset(&op, 0, sizeof(op));
        op.type = TY_IO_OP_READ;
        op.fd = self->sock;
        op.buf = buf;
        op.len = (size_t)cap;
        op.coro = coro;
        ty_io_submit(&op);
        /* Coroutine is parked by ty_io_submit; resumes when completion fires. */
        int64_t r = ty_io_take_result(coro);
        if (r < 0) {
            result.err = (int32_t)(-r);
            *out = result;
            return;
        }
        result.tag = 0;
        result.value = (int32_t)r;
        result.err = 0;
        *out = result;
        return;
    }

    /* sync fallback — outside coroutine context */
#if defined(_WIN32)
    int n = recv((SOCKET)self->sock, buf, (int)cap, 0);
#else
    ssize_t n;
    do { n = recv(self->sock, buf, (size_t)cap, 0); } while (n < 0 && errno == EINTR);
#endif
    if (n < 0) {
        result.err = ty_net_last_error();
        *out = result;
        return;
    }
    result.tag = 0;
    result.value = (int32_t)n;
    result.err = 0;
    *out = result;
}

void __ty_rt__Listener__close(void* task, TyListener* self) {
    (void)task;
    if (!self) return;

    /* Phase 4: remove from per-worker fd set and close. */
    ty_sock_t fd_to_close = self->sock;
    self->sock = TY_SOCK_INVALID;

    if (fd_to_close != TY_SOCK_INVALID) {
        Worker* w = ty_sched_current_worker();
        if (w) {
            ty_fdset_remove(&w->fd_set, (ty_fd_t)fd_to_close);
        }
        ty_sock_force_shutdown(fd_to_close);
        ty_sock_close(fd_to_close);
    }
    free(self);
}

void __ty_rt__Socket__close(void* task, TySocket* self) {
    (void)task;
    if (!self) return;

    /* ── Debug guard: catch double-close from liveness checker bugs ── */
    TY_ASSERT(!self->closed, "Socket__close called twice — liveness checker bug");
    /* Mark closed immediately. Visible to concurrent readers before we
     * invalidate the fd, giving an early signal in debug builds. */
    self->closed = 1;

    /* Phase 4: remove from per-worker fd set, invalidate, close.
     *
     * Per-worker TyFdSet means no global lock contention.
     * ty_net_shutdown() delegates to ty_fdset_close_all which is
     * called per-worker by the scheduler, so no sentinel race exists.
     * The closed flag prevents the reader coroutine from using a
     * stale fd after we close it here. */
    ty_sock_t fd_to_close = self->sock;
    self->sock = TY_SOCK_INVALID;

    if (fd_to_close != TY_SOCK_INVALID) {
        Worker* w = ty_sched_current_worker();
        if (w) {
            ty_fdset_remove(&w->fd_set, (ty_fd_t)fd_to_close);
        }
        ty_sock_close(fd_to_close);
    }
    free(self);
}
