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
#include "ty_mem.h"
#include "atomic.h"
#include <string.h>
#include <stdlib.h>
#include <stdio.h>
#include <stddef.h>
#include <stdint.h>

/*
 * TyStr itself is NOT redeclared here anymore — ty_net.h includes
 * ty_mem.h and uses TyStr* directly in its own declarations (e.g.
 * Network__listen, WriteSocket__write), so the real one must already be
 * available through that include chain. A second, separate `typedef
 * struct { ... } TyStr;` here was a real bug: two anonymous-struct
 * typedefs to the same name are never compatible in C even with
 * identical member layout, which is exactly what caused the "conflicting
 * types for '__ty_rt__ReadSocket__into_chan'" build error — the
 * redefinition corrupted type identity for everything downstream in this
 * file, not just code that directly touches TyStr. */

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
struct TySocket {
    ty_sock_t sock;
    int closed;
    /* Number of live halves sharing this socket's fd. 1 for a plain,
     * never-split Socket (Socket__close always fully closes — same as
     * before). set to 2 by split(); the fd is only actually closed once
     * both ReadSocket__close and WriteSocket__close have run. */
    _Atomic(int) half_count;
};
struct TyReadSocket  { TySocket* sock; int closed; };
struct TyWriteSocket { TySocket* sock; int closed; };

/* ── Fixed-slot lock-free pool for cross-coroutine resources ────────────────
 *
 * Socket/Listener/consume-closure structs must have independent lifetime,
 * not task-arena lifetime (see D5 exemption in typhoon_io_redesign.md) —
 * they're routinely handed from an accepting/owning coroutine to a
 * different spawned coroutine.  malloc/free is correct but not fast here:
 * with the M:N work-stealing scheduler, allocation happens on the
 * accepting worker thread and free happens on whatever thread later runs
 * Socket__close — very likely a different OS thread. That's glibc
 * malloc's slow path (cross-thread free forces arena migration/locking).
 *
 * A fixed-slot pool sidesteps this: alloc/free are a single CAS on a
 * per-slot flag, O(1) regardless of which thread performs them, no lock.
 * Same technique as ty_io_kqueue.c's KqPending pool, with an added
 * rotating search hint so allocation stays ~O(1) instead of O(n) under
 * high occupancy. Falls back to malloc/free if the pool is exhausted —
 * degrades gracefully rather than failing the accept.
 */

#define TY_NET_POOL_CAP 8192 /* must be a power of 2 */

#define TY_NET_DEFINE_POOL(NAME, TYPE)                                       \
    static TYPE NAME##_slots[TY_NET_POOL_CAP];                               \
    static _Atomic(int) NAME##_in_use[TY_NET_POOL_CAP];                      \
    static _Atomic(int) NAME##_hint;                                         \
    static TYPE* NAME##_alloc(void) {                                        \
        int start = atomic_fetch_add_explicit(&NAME##_hint, 1,               \
            memory_order_relaxed) & (TY_NET_POOL_CAP - 1);                   \
        for (int i = 0; i < TY_NET_POOL_CAP; i++) {                          \
            int idx = (start + i) & (TY_NET_POOL_CAP - 1);                   \
            int exp = 0;                                                     \
            if (atomic_compare_exchange_strong_explicit(                     \
                    &NAME##_in_use[idx], &exp, 1,                            \
                    memory_order_acquire, memory_order_relaxed))             \
                return &NAME##_slots[idx];                                   \
        }                                                                    \
        return (TYPE*)malloc(sizeof(TYPE)); /* pool exhausted — fallback */  \
    }                                                                        \
    static void NAME##_free(TYPE* p) {                                      \
        if (!p) return;                                                      \
        uintptr_t base = (uintptr_t)NAME##_slots;                            \
        uintptr_t addr = (uintptr_t)p;                                       \
        if (addr < base || addr >= base + sizeof(NAME##_slots)) {            \
            free(p); return; /* came from the malloc fallback path */        \
        }                                                                    \
        size_t idx = (addr - base) / sizeof(TYPE);                           \
        atomic_store_explicit(&NAME##_in_use[idx], 0, memory_order_release); \
    }

TY_NET_DEFINE_POOL(g_socket_pool, TySocket)
TY_NET_DEFINE_POOL(g_listener_pool, TyListener)
TY_NET_DEFINE_POOL(g_read_pool, TyReadSocket)
TY_NET_DEFINE_POOL(g_write_pool, TyWriteSocket)

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

static int split_host_port(TyStr* addr, char** host_out, char** port_out) {
    if (!addr || !addr->ptr || addr->len <= 0) return 0;

    /* Find the last ':' within [0, addr->len) explicitly — NOT strrchr,
     * which scans for a null terminator Str no longer guarantees in
     * general (it's a fat pointer with an explicit length now, not a
     * null-terminated C string). This was the actual bug behind
     * "listen invalid addr=<garbage>": this function still took a raw
     * char* and was handed the address of a %struct.Str instead of the
     * string's bytes. */
    int32_t colon = -1;
    for (int32_t i = addr->len - 1; i >= 0; i--) {
        if (addr->ptr[i] == ':') { colon = i; break; }
    }
    if (colon < 0) return 0;

    int32_t host_len = colon;
    int32_t port_len = addr->len - colon - 1;
    if (port_len <= 0) return 0;

    char* host = (char*)malloc((size_t)host_len + 1);
    if (!host) return 0;
    memcpy(host, addr->ptr, (size_t)host_len);
    host[host_len] = '\0';

    /* port_out points directly into addr->ptr, not a separate copy —
     * matches the original (pre-fat-pointer) behavior and its existing
     * free() pairing (port is never freed by any caller). Relies on the
     * same trailing-'\0' invariant used in ty_io.c's fs::open: every
     * current Str-producing path (codegen.rs's emit_string for literals,
     * ty_buf_into_str) keeps one byte past len as '\0', so the substring
     * starting after the colon is still validly null-terminated. Breaks
     * silently if Str slicing is ever added and can produce a view that
     * doesn't end at the original buffer's terminator. */
    *host_out = host;
    *port_out = addr->ptr + colon + 1;
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

void __ty_rt__Network__listen(void* task, TyNetwork* self, TyStr* addr, TyResult_Listener_i32* out) {
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
        /* %.*s with an explicit length, not %s — addr->ptr has no
         * null-termination guarantee to stop at in general (see
         * split_host_port's comment). Printing it as if it were a plain
         * C string is exactly what produced garbled/binary output
         * before this function took TyStr* instead of raw char*. */
        TY_DEBUG("[net] listen invalid addr=\"%.*s\" (expected host:port)\n",
            addr ? addr->len : 0, addr ? addr->ptr : "(null)");
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

    TyListener* listener = g_listener_pool_alloc();
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
                TySocket* sock = g_socket_pool_alloc();
                if (!sock) {
                    ty_sock_close(c);
                    result.err = -3;
                    *out = result;
                    return;
                }
                sock->sock = c;
                sock->closed = 0;
                sock->half_count = 1;

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
            TySocket* sock = g_socket_pool_alloc();
            if (!sock) {
                ty_sock_close(accepted);
                result.err = -3;
                *out = result;
                return;
            }
            sock->sock = accepted;
            sock->closed = 0;
            sock->half_count = 1;

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

    TySocket* sock = g_socket_pool_alloc();
    if (!sock) {
        ty_sock_close(c);
        result.err = -3;
        *out = result;
        return;
    }
    sock->sock = c;
    sock->closed = 0;
    sock->half_count = 1;

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

/* ── Socket.split() / ReadSocket.into_chan() ─────────────────────────────────
 *
 * Task 2.1/2.5, OQ3 resolved design:
 *   Socket.split() -> (ReadSocket, WriteSocket)
 *   ReadSocket.into_chan(chunk_size, cap) -> chan<Buf>   — owns read direction
 *   WriteSocket is the only write-capable handle
 *   fd is shared internally; ownership is directionally encoded in the
 *   type system (TySocket.half_count tracks when both halves have closed).
 *
 * Replaces Socket__consume, which sent one channel op per BYTE (chan<int8>
 * under the hood) — the opposite of D4's backpressure model, and the
 * function whose 4096-byte slab_alloc(size_to_class(...)) call was the
 * live heap overflow fixed earlier. into_chan sends one channel op per
 * CHUNK (chan<Buf>), matching D4, and reads directly into a correctly
 * sized Buf via slab_alloc_sized/ty_buf_new_sized rather than a
 * class-rounded raw buffer.
 */

typedef struct {
    TyReadSocket* rs;
    struct TyChan* chan;
    int64_t chunk_size;
} TyIntoChanCtx;

/* Same cross-thread alloc/free pattern as Socket/Listener: ctx is built on
 * the calling coroutine's worker, unpacked and freed on the newly spawned
 * reader coroutine's worker — likely a different thread. Pool it too. */
TY_NET_DEFINE_POOL(g_intochan_ctx_pool, TyIntoChanCtx)

static void ty_read_into_chan_coro(void* task, void* arg) {
    TyIntoChanCtx* ctx = (TyIntoChanCtx*)arg;
    TyReadSocket* rs = ctx->rs;
    struct TyChan* ch = ctx->chan;
    int64_t chunk_size = ctx->chunk_size;
    g_intochan_ctx_pool_free(ctx);

    SlabArena* arena = (SlabArena*)task;

    for (;;) {
        TySocket* sock = rs ? rs->sock : NULL;
        ty_sock_t fd = sock ? sock->sock : TY_SOCK_INVALID;

        if (fd == TY_SOCK_INVALID)
            break; /* socket was closed externally */

        /* Read straight into a Buf sized exactly to chunk_size — one
         * allocation, one copy (kernel into buffer). No intermediate
         * raw buffer, no size-class rounding. */
        Buf* chunk = ty_buf_new_sized(arena, chunk_size);
        if (!chunk) break; /* OOM on slab (rare) */

        int64_t got = 0;
        void* drv = ty_io_global_driver();
        void* coro = ty_current_coro_raw();
        if (drv && coro) {
            /* async path: submit via driver and park until completion */
            ty_io_read(drv, arena, coro, fd, (uint8_t*)chunk->data, (size_t)chunk_size);
            got = ty_io_take_result(coro);
        } else {
#if defined(_WIN32)
            int n = recv((SOCKET)fd, chunk->data, (int)chunk_size, 0);
            got = (int64_t)n;
#else
            ssize_t n;
            do { n = recv(fd, chunk->data, (size_t)chunk_size, 0); } while (n < 0 && errno == EINTR);
            got = (int64_t)n;
#endif
        }

        if (got <= 0) break; /* EOF or error */

        chunk->len = got;
        chunk->data[got] = '\0';

        /* One channel op per CHUNK, not one per byte — D4's backpressure
         * model: chan<Buf> capacity bounds chunks in flight. */
        ty_chan_send(arena, ch, &chunk);
    }
    ty_chan_close(ch);
}

/*
 * Socket__split — consumes a whole Socket, returns two directional
 * handles sharing the underlying fd. Cannot fail (no Result out param) —
 * both halves are just pool-allocated wrapper structs.
 *
 * ABI: returned BY VALUE, not via out-pointer. My first attempt guessed
 * the out-pointer convention by analogy with Result<T,E>, but the actual
 * compiler output (confirmed from the linked IR) declares this as
 * `%struct.SocketHalves @__ty_rt__Socket__split(i8*, %struct.Socket*)` —
 * a plain two-pointer struct is small enough (16 bytes) that codegen
 * returns it directly rather than through sret/out-pointer the way it
 * does for Result<T,E>'s bigger tag+value+err layout.
 *
 * TySplitResult itself is NOT redeclared here — ty_net.h already defines
 * it (same anonymous-struct-redefinition bug as TyStr above; ty_net.c
 * includes ty_net.h, so a second local typedef of the same name is a
 * conflicting, not redundant, declaration).
 */
TySplitResult __ty_rt__Socket__split(void* task, TySocket* self) {
    (void)task;
    atomic_store_explicit(&self->half_count, 2, memory_order_relaxed);

    TyReadSocket* r = g_read_pool_alloc();
    TyWriteSocket* w = g_write_pool_alloc();
    r->sock = self;
    r->closed = 0;
    w->sock = self;
    w->closed = 0;

    TySplitResult result;
    result.read = r;
    result.write = w;
    return result;
}

/*
 * ReadSocket__into_chan — spawns the reader coroutine above and returns
 * the channel immediately; the caller doesn't wait for it to finish.
 *
 * ABI: returns struct TyChan* BY VALUE, not via out-pointer. Same class
 * of mistake as Socket__split originally was: chan<T> lowers to a bare
 * i8* (see codegen.rs: "Chan" => "i8*", same slot as Ref), not an
 * aggregate like Result<T,E> that needs sret/out-pointer treatment.
 * net.ty's own declaration (`-> ref chan<Buf>`) is what the compiler
 * actually builds its expected C signature from, and it expects a
 * direct 4-param/return-value shape, not the 5-param/void/out-pointer
 * shape I originally wrote here.
 */
struct TyChan* __ty_rt__ReadSocket__into_chan(void* task, TyReadSocket* self,
    int64_t chunk_size, int64_t cap) {
    if (chunk_size <= 0) chunk_size = 4096;

    struct TyChan* ch = ty_chan_new(sizeof(Buf*), (size_t)cap);

    if (!self) {
        ty_chan_close(ch);
        return ch;
    }

    TyIntoChanCtx* ctx = g_intochan_ctx_pool_alloc();
    if (!ctx) {
        /* OOM: close the channel immediately so the receiver sees EOF. */
        ty_chan_close(ch);
        return ch;
    }
    ctx->rs = self;
    ctx->chan = ch;
    ctx->chunk_size = chunk_size;

    ty_spawn((SlabArena*)task, ty_read_into_chan_coro, (void*)ctx);
    return ch;
}

/* ReadSocket__close / WriteSocket__close — release this half. The
 * underlying fd is only actually closed once both halves have closed
 * (half_count reaches 0); until then the other half keeps using it. */
void __ty_rt__ReadSocket__close(void* task, TyReadSocket* self) {
    (void)task;
    if (!self) return;
    TY_ASSERT(!self->closed, "ReadSocket__close called twice — liveness checker bug");
    self->closed = 1;

    TySocket* sock = self->sock;
    self->sock = NULL;
    if (sock && atomic_fetch_sub_explicit(&sock->half_count, 1, memory_order_acq_rel) == 1) {
        __ty_rt__Socket__close(task, sock);
    }
    g_read_pool_free(self);
}

void __ty_rt__WriteSocket__close(void* task, TyWriteSocket* self) {
    (void)task;
    if (!self) return;
    TY_ASSERT(!self->closed, "WriteSocket__close called twice — liveness checker bug");
    self->closed = 1;

    TySocket* sock = self->sock;
    self->sock = NULL;
    if (sock && atomic_fetch_sub_explicit(&sock->half_count, 1, memory_order_acq_rel) == 1) {
        __ty_rt__Socket__close(task, sock);
    }
    g_write_pool_free(self);
}

/* WriteSocket__write — thin delegate onto Socket__write; WriteSocket is
 * just a directional wrapper around the same underlying TySocket.
 *
 * buf is Str at the Typhoon level (net.ty: fn write(self, buf: Str, len:
 * Int32)), which is now a fat pointer { ptr, len }, not raw char* — see
 * codegen.rs's "%struct.Str" and ty_mem.c's matching TyStr. The `len`
 * parameter net.ty still passes is redundant now that buf carries its
 * own length; ignored here in favor of buf->len, which can't be wrong
 * the way a separately-passed len could (caller mismatch). Worth
 * dropping `len` from net.ty's signature entirely as a follow-up. */
void __ty_rt__WriteSocket__write(void* task, TyWriteSocket* self, TyStr* buf,
    int32_t len, TyResult_i32_i32* out) {
    (void)len;
    if (!self || !buf) {
        TyResult_i32_i32 result;
        result.tag = 1;
        result.value = 0;
        result.err = -1;
        *out = result;
        return;
    }
    __ty_rt__Socket__write(task, self->sock, buf->ptr, buf->len, out);
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
    g_listener_pool_free(self);
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
    g_socket_pool_free(self);
}
