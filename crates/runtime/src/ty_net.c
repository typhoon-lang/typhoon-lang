/*
 * ty_net.c — minimal capability-gated networking for Typhoon
 *
 * Notes:
 * - Uses OS sockets directly (blocking for now).
 * - `task` is accepted for future slab allocation; currently unused.
 * - Address parsing supports \"host:port\" (IPv4 / hostname). IPv6 literals
 *   are not supported yet.
 */

#include "ty_net.h"
#include "scheduler.h"
#include "platform.h"
#include <string.h>
#include <stdlib.h>
#include <stdio.h>

#if defined(_WIN32)
#  define WIN32_LEAN_AND_MEAN
#  include <winsock2.h>
#  include <ws2tcpip.h>
#  include <windows.h>
#  pragma comment(lib, "Ws2_32.lib")
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
#  include <errno.h>
#  include <unistd.h>
#  include <sys/types.h>
#  include <sys/socket.h>
#  include <netdb.h>
#  include <arpa/inet.h>
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
#  define TY_SOCK_INVALID  INVALID_SOCKET
#else
#  define TY_SOCK_INVALID  ((ty_sock_t)(-1))
#endif

typedef struct TyResult_i32_i32 {
    uint8_t ok;
    int32_t value;
    int32_t err;
} TyResult_i32_i32;

struct TyNetwork { uint32_t _tag; };
struct TyListener { ty_sock_t sock; struct TyListener* next; };
struct TySocket { ty_sock_t sock; int closed; struct TySocket* next; };

static TyNetwork g_net = { 0x4E45544Eu }; /* 'NETN' */
static TyMutex g_sock_lock;
static int g_initialized = 0;
static struct TyListener* g_listeners = NULL;
static struct TySocket* g_sockets = NULL;

void ty_net_init(void) {
    if (!g_initialized) {
        ty_mutex_init(&g_sock_lock);
        g_initialized = 1;
    }
    #if defined(_WIN32)
    WSADATA wsa;
    (void)WSAStartup(MAKEWORD(2, 2), &wsa);
    #endif
}

void ty_net_shutdown(void) {
    /* Steal both lists under the lock so Socket__close cannot race with us. */
    ty_mutex_lock(&g_sock_lock);
    struct TyListener* listeners = g_listeners;
    g_listeners = NULL;
    struct TySocket*  sockets   = g_sockets;
    g_sockets = NULL;
    ty_mutex_unlock(&g_sock_lock);

    /*
     * Shut down and free every listener.
     *
     * A concurrent __ty_rt__Listener__close could race here.  Apply the same
     * sentinel pattern: read l->sock, set sentinel, then close outside any lock.
     */
    struct TyListener* l = listeners;
    while (l) {
        struct TyListener* next = l->next;

        ty_mutex_lock(&g_sock_lock);
        ty_sock_t lfd = l->sock;
        l->sock = TY_SOCK_INVALID;
        ty_mutex_unlock(&g_sock_lock);

        if (lfd != TY_SOCK_INVALID) {
            ty_sock_force_shutdown(lfd);
            ty_sock_close(lfd);
        }
        free(l);
        l = next;
    }

    /*
     * Shut down and free every socket.
     *
     * A concurrent Socket__close may have already closed the fd and set
     * self->sock = TY_SOCK_INVALID before we reach this node (case B in
     * the comment in __ty_rt__Socket__close above).  Check before closing.
     */
    struct TySocket* s = sockets;
    while (s) {
        struct TySocket* next = s->next;

        ty_mutex_lock(&g_sock_lock);
        ty_sock_t sfd = s->sock;
        s->sock = TY_SOCK_INVALID;
        ty_mutex_unlock(&g_sock_lock);

        if (sfd != TY_SOCK_INVALID) {
            ty_sock_force_shutdown(sfd);
            ty_sock_close(sfd);
        }
        free(s);
        s = next;
    }

#if defined(_WIN32)
    (void)WSACleanup();
#endif
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

void __ty_rt__Network__listen(void* task, TyNetwork* self, char* addr, TyResult_Listener_i32* outp) {
    (void)task;
    (void)self;

    TyResult_Listener_i32 out;
    out.ok = 0;
    out.value = NULL;
    out.err = -1;

    if (!outp) {
        TY_DEBUG("[net] listen BUG: out=NULL\n");
        return;
    }
    TY_DEBUG("[net] listen enter addr_ptr=%p out_ptr=%p\n", (void*)addr, (void*)outp);

    char* host = NULL;
    char* port = NULL;
    if (!split_host_port(addr, &host, &port)) {
        out.err = -2;
        TY_DEBUG("[net] listen invalid addr=\"%s\" (expected host:port)\n",
            addr ? addr : "(null)");
        *outp = out;
        return;
    }

    struct addrinfo hints;
    memset(&hints, 0, sizeof(hints));
    hints.ai_family = AF_UNSPEC;
    hints.ai_socktype = SOCK_STREAM;
    hints.ai_protocol = IPPROTO_TCP;
    hints.ai_flags = AI_PASSIVE;

    struct addrinfo* res = NULL;
    int gai = getaddrinfo((host[0] == '\0') ? NULL : host, port, &hints, &res);
    if (gai != 0 || !res) {
        free(host);
        out.err = (int32_t)gai;
#if defined(_WIN32)
        TY_DEBUG("[net] listen getaddrinfo failed addr=\"%s\" gai=%d (%s)\n",
            addr ? addr : "(null)", gai, gai_strerrorA(gai));
#else
        TY_DEBUG("[net] listen getaddrinfo failed addr=\"%s\" gai=%d (%s)\n",
            addr ? addr : "(null)", gai, gai_strerror(gai));
#endif
        *outp = out;
        return;
    }

    ty_sock_t s = (ty_sock_t)(-1);
    int32_t last_err = 0;
    struct addrinfo* it = res;
    for (; it; it = it->ai_next) {
        s = (ty_sock_t)socket(it->ai_family, it->ai_socktype, it->ai_protocol);
        #if defined(_WIN32)
        if (s == INVALID_SOCKET) { last_err = ty_net_last_error(); continue; }
        #else
        if (s < 0) { last_err = ty_net_last_error(); continue; }
        #endif

        int yes = 1;
        (void)setsockopt(s, SOL_SOCKET, SO_REUSEADDR, (const char*)&yes, (socklen_t)sizeof(yes));

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
        break;
    }

    freeaddrinfo(res);
    free(host);

    #if defined(_WIN32)
    if (s == INVALID_SOCKET) {
        out.err = last_err ? last_err : ty_net_last_error();
        char msg[256];
        TY_DEBUG("[net] listen failed addr=\"%s\" wsa=%ld (%s)\n",
            addr ? addr : "(null)", (long)out.err,
            ty_net_errstr_win32(out.err, msg, sizeof(msg)));
        *outp = out;
        return;
    }
    #else
    if (s < 0) {
        out.err = last_err ? last_err : ty_net_last_error();
        TY_DEBUG("[net] listen failed addr=\"%s\" errno=%d (%s)\n",
            addr ? addr : "(null)", (int)out.err, ty_net_errstr_errno(out.err, NULL, 0));
        *outp = out;
        return;
    }
    #endif

    TyListener* listener = (TyListener*)malloc(sizeof(TyListener));
    if (!listener) {
        ty_sock_close(s);
        out.err = -3;
        TY_DEBUG("[net] listen OOM allocating listener for addr=\"%s\"\n",
            addr ? addr : "(null)");
        *outp = out;
        return;
    }
    listener->sock = s;
    ty_mutex_lock(&g_sock_lock);
    listener->next = g_listeners;
    g_listeners = listener;
    ty_mutex_unlock(&g_sock_lock);

    out.ok = 1;
    out.value = listener;
    out.err = 0;
    *outp = out;
    return;
}

void __ty_rt__Listener__accept(void* task, TyListener* self, TyResult_Socket_i32* outp) {
    (void)task;
    TyResult_Socket_i32 out;
    out.ok = 0;
    out.value = NULL;
    out.err = -1;

    if (!self) {
        out.err = -2;
        *outp = out;
        return;
    }

    ty_sock_t c = (ty_sock_t)(-1);
    c = (ty_sock_t)accept(self->sock, NULL, NULL);
    #if defined(_WIN32)
    if (c == INVALID_SOCKET) {
        out.err = ty_net_last_error();
        *outp = out;
        return;
    }
    #else
    if (c < 0) {
        out.err = ty_net_last_error();
        *outp = out;
        return;
    }
    #endif

    TySocket* sock = (TySocket*)malloc(sizeof(TySocket));
    if (!sock) {
        ty_sock_close(c);
        out.err = -3;
        *outp = out;
        return;
    }
    sock->sock = c;
    sock->closed = 0;
    ty_mutex_lock(&g_sock_lock);
    sock->next = g_sockets;
    g_sockets = sock;
    ty_mutex_unlock(&g_sock_lock);

    out.ok = 1;
    out.value = sock;
    out.err = 0;
    *outp = out;
    return;
}

/* ── Socket__consume ─────────────────────────────────────────────────────────
 *
 * Spawns a background coroutine that reads bytes from self->sock and sends
 * each one into `ch`, then closes `ch` on EOF or error.
 *
 * The spawned coroutine runs on the scheduler and owns the read loop; the
 * caller returns immediately and drives from ch.recv() / ch.try_recv().
 *
 * Closure layout (heap-allocated, freed by the reader coro on exit):
 *   struct TyConsumeCtx { TySocket* socket; struct TyChan* chan; }
 *
 * The reader uses blocking recv(2) on the OS socket.  This is consistent with
 * the rest of the runtime ("blocking for now" per the file-level comment).
 * A single scheduler worker thread will be occupied per active connection until
 * async I/O is wired up in a later phase.
 */

typedef struct {
    TySocket*       socket;
    struct TyChan*  chan;
} TyConsumeCtx;

static void ty_socket_reader_coro(void* task, void* arg) {
    TyConsumeCtx*  ctx  = (TyConsumeCtx*)arg;
    TySocket*      self = ctx->socket;
    struct TyChan* ch   = ctx->chan;
    free(ctx);          /* closure is no longer needed once unpacked */

    char byte_buf;
    for (;;) {
        /* Check whether Socket__close has already invalidated the fd. */
        ty_mutex_lock(&g_sock_lock);
        ty_sock_t fd = self ? self->sock : TY_SOCK_INVALID;
        ty_mutex_unlock(&g_sock_lock);

        if (fd == TY_SOCK_INVALID)
            break;      /* socket was closed externally */

#if defined(_WIN32)
        int n = recv((SOCKET)fd, &byte_buf, 1, 0);
        if (n == SOCKET_ERROR || n == 0) break;
#else
        ssize_t n;
        do { n = recv(fd, &byte_buf, 1, 0); } while (n < 0 && errno == EINTR);
        if (n <= 0) break;  /* 0 = EOF, <0 = error */
#endif
        ty_chan_send((SlabArena*)task, ch, &byte_buf);
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
    ctx->chan   = (struct TyChan*)ch;

    /* ty_spawn(arena, fn_ptr, arg_ptr) → new coroutine.
     * Passing the caller's task as the arena shares the same slab pool,
     * matching how connection handler coros are spawned elsewhere. */
    ty_spawn((SlabArena*)task, ty_socket_reader_coro, (void*)ctx);
}

/*
 * Socket__recv — blocking receive.
 *
 * Blocks the calling coroutine until a byte arrives on the channel or the
 * channel is closed (remote EOF / error).  Returns:
 *   ok=1, value=byte   — byte received, connection still open
 *   ok=0, err=0        — channel closed (EOF), caller should stop reading
 *   ok=0, err=-1       — self or chan is NULL (programming error)
 *
 * Maps to:  let Some(i) = ch.recv() else { break; }
 */
TyResult_i32_i32 __ty_rt__Socket__recv(void* task, TySocket* self, struct TyChan* chan) {
    TyResult_i32_i32 out;
    out.ok = 0;
    out.value = 0;
    out.err = -1;

    if (!self || !chan) return out;

    int8_t byte = 0;
    /* ty_chan_recv now returns 1 (data) or -1 (closed/EOF). */
    int status = ty_chan_recv(task, chan, &byte);
    if (status == -1) {
        /* Channel closed — signal EOF cleanly, not as an error. */
        out.err = 0;
        return out;
    }

    out.ok = 1;
    out.value = (int32_t)(uint8_t)byte;
    out.err = 0;
    return out;
}

/*
 * Socket__try_recv — non-blocking receive.
 *
 * Returns immediately whether or not a byte is available:
 *   ok=1, value=byte   — byte received
 *   ok=0, err=1        — would block (no data yet, connection still open)
 *   ok=0, err=0        — channel closed (EOF)
 *   ok=0, err=-1       — self or chan is NULL
 *
 * Maps to:  let Some(i) = ch.try_recv() else { break; }
 * Callers MUST distinguish err=1 (retry later) from err=0 (stop reading).
 */
TyResult_i32_i32 __ty_rt__Socket__try_recv(void* task, TySocket* self, struct TyChan* chan) {
    TyResult_i32_i32 out;
    out.ok = 0;
    out.value = 0;
    out.err = -1;

    if (!self || !chan) return out;

    int8_t byte = 0;
    /* ty_chan_try_recv returns:
     *   1   — data received          (TY_CHAN_OK)
     *   0   — empty, still open      (TY_CHAN_EMPTY)  → err=1 (would block)
     *  -1   — channel closed (EOF)   (TY_CHAN_CLOSED) → err=0 */
    int status = ty_chan_try_recv(task, chan, &byte);
    if (status == 1) {
        out.ok = 1;
        out.value = (int32_t)(uint8_t)byte;
        out.err = 0;
    } else if (status == 0) {
        out.err = 1; /* would block — not an error, not EOF */
    } else {
        out.err = 0; /* -1 == closed — EOF */
    }
    return out;
}

TyResult_i32_i32 __ty_rt__Socket__write(void* task, TySocket* self, char* buf, int32_t len) {
    (void)task;
    TyResult_i32_i32 out;
    out.ok = 0;
    out.value = 0;
    out.err = -1;

    if (!self || !buf) return out;

    int r = send(self->sock, buf, len, 0);
    if (r < 0) {
        out.err = ty_net_last_error();
        return out;
    }

    out.ok = 1;
    out.value = r;
    out.err = 0;
    return out;
}

void __ty_rt__Listener__close(void* task, TyListener* self) {
    (void)task;
    if (!self) return;

    ty_mutex_lock(&g_sock_lock);
    struct TyListener* prev = NULL;
    struct TyListener* curr = g_listeners;
    while (curr) {
        if (curr == self) {
            if (prev) prev->next = curr->next;
            else      g_listeners = curr->next;
            break;
        }
        prev = curr;
        curr = curr->next;
    }
    ty_sock_t fd_to_close = self->sock;
    self->sock = TY_SOCK_INVALID;
    ty_mutex_unlock(&g_sock_lock);

    if (fd_to_close != TY_SOCK_INVALID) {
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
    /* Mark closed immediately.  This is visible to any concurrent caller
     * before we acquire the lock, giving an early signal in debug builds. */
    self->closed = 1;

    /* ── Remove from global list and invalidate the fd atomically ──── */
    ty_mutex_lock(&g_sock_lock);

    struct TySocket* prev = NULL;
    struct TySocket* curr = g_sockets;
    while (curr) {
        if (curr == self) {
            if (prev) prev->next = curr->next;
            else       g_sockets = curr->next;
            break;
        }
        prev = curr;
        curr = curr->next;
    }

    /* Sentinel the fd before releasing the lock.
     *
     * ty_net_shutdown steals the list under the lock and then walks it
     * outside the lock.  By the time shutdown reaches this node (if it
     * already stole the list before we locked above, `curr` will be NULL
     * and we won't find self in g_sockets — but self->sock must still be
     * invalidated so shutdown's walk sees TY_SOCK_INVALID and skips it).
     *
     * Two cases:
     *   A. We removed self from g_sockets (curr != NULL above):
     *      shutdown will never see this node.  Sentinel is defensive only.
     *   B. Shutdown already stole the list (curr == NULL above):
     *      shutdown holds a pointer to self in its local `sockets` variable.
     *      We must set self->sock = TY_SOCK_INVALID before shutdown's walk
     *      reaches this node.  The mutex ensures we finish before shutdown
     *      proceeds past ty_mutex_unlock in its own list-steal section.
     *      (Shutdown holds g_sock_lock while stealing; we held it here too,
     *      so one of us went first.  If we go first: we close the fd, set
     *      sentinel, unlock — shutdown then sees sentinel and skips.
     *      If shutdown goes first: it steals the list, we get the lock, find
     *      curr==NULL, set sentinel on the now-shutdown-owned node, unlock —
     *      shutdown's walk will check for TY_SOCK_INVALID and skip.)
     */
    ty_sock_t fd_to_close = self->sock;
    self->sock = TY_SOCK_INVALID;

    ty_mutex_unlock(&g_sock_lock);

    /* ── Close outside the lock ── */
    if (fd_to_close != TY_SOCK_INVALID) {
        ty_sock_close(fd_to_close);
    }
    free(self);
}
