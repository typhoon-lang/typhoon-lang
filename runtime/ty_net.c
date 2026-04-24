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

typedef struct TyResult_i32_i32 {
    uint8_t ok;
    int32_t value;
    int32_t err;
} TyResult_i32_i32;

struct TyNetwork { uint32_t _tag; };
struct TyListener { ty_sock_t sock; struct TyListener* next; };
struct TySocket { ty_sock_t sock; struct TySocket* next; };

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
    struct TySocket* sockets = g_sockets;
    g_sockets = NULL;
    ty_mutex_unlock(&g_sock_lock);

    /* Shut down and free every listener. */
    struct TyListener* l = listeners;
    while (l) {
        struct TyListener* next = l->next;
        ty_sock_force_shutdown(l->sock);
        ty_sock_close(l->sock);
        free(l);
        l = next;
    }

    /* Shut down and free every socket. */
    struct TySocket* s = sockets;
    while (s) {
        struct TySocket* next = s->next;
        ty_sock_force_shutdown(s->sock);
        ty_sock_close(s->sock);
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

void __ty_method__Network__listen(void* task, TyNetwork* self, char* addr, TyResult_Listener_i32* outp) {
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
            ty_sock_close(s);
            s = (ty_sock_t)(-1);
            continue;
        }
        if (listen(s, 128) != 0) {
            last_err = ty_net_last_error();
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
#if defined(_WIN32)
        char msg[256];
        TY_DEBUG("[net] listen failed addr=\"%s\" wsa=%ld (%s)\n",
            addr ? addr : "(null)", (long)out.err,
            ty_net_errstr_win32(out.err, msg, sizeof(msg)));
#endif
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

void __ty_method__Listener__accept(void* task, TyListener* self, TyResult_Socket_i32* outp) {
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

TyResult_i32_i32 __ty_method__Socket__read(void* task, TySocket* self, char* buf, int32_t len) {
    (void)task;
    TyResult_i32_i32 out;
    out.ok = 0;
    out.value = 0;
    out.err = -1;

    if (!self || !buf) return out;

    int r = recv(self->sock, buf, len, 0);
    if (r < 0) {
        out.err = ty_net_last_error();
        return out;
    }

    out.ok = 1;
    out.value = r;
    out.err = 0;
    return out;
}

static void socket_consumer_coro(void* task, void* arg) {
    /* arg: [TySocket*, TyChan*] */
    void** pair = (void**)arg;
    TySocket* sock = (TySocket*)pair[0];
    struct TyChan* chan = (struct TyChan*)pair[1];
    char buf[1024];

    while (1) {
        int r = recv(sock->sock, buf, 1024, 0);
        if (r <= 0) break;
        /* The channel element size is 1 byte (Char/i8).  Send each byte
         * individually so the write matches the slot size.  Passing a
         * char* pointer value into a 1-byte slot would write 8 bytes into
         * a 1-byte region and corrupt adjacent channel memory. */
        for (int i = 0; i < r; i++) {
            ty_chan_send(task, chan, &buf[i]);
        }
    }
    ty_chan_close(chan);
    free(pair);
}

void __ty_method__Socket__consume(void* task, TySocket* self, struct TyChan* chan) {
    void** pair = (void**)malloc(sizeof(void*) * 2);
    if (!pair) {
        TY_DEBUG("[net] Socket__consume OOM allocating coroutine arg\n");
        return;
    }
    pair[0] = self;
    pair[1] = chan;
    ty_spawn(task, socket_consumer_coro, pair);  /* fix #1: pass task, not NULL */
}

TyResult_i32_i32 __ty_method__Socket__write(void* task, TySocket* self, char* buf, int32_t len) {
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

void __ty_method__Socket__close(void* task, TySocket* self) {
    (void)task;
    if (!self) return;

    ty_mutex_lock(&g_sock_lock);
    struct TySocket* prev = NULL;
    struct TySocket* curr = g_sockets;
    while (curr) {
        if (curr == self) {
            if (prev) prev->next = curr->next;
            else g_sockets = curr->next;
            break;
        }
        prev = curr;
        curr = curr->next;
    }
    ty_mutex_unlock(&g_sock_lock);

    ty_sock_close(self->sock);
    free(self);
}
