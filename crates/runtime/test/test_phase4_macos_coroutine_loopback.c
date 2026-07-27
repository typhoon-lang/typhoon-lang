// test_phase4_macos_coroutine_loopback.c
//
// Closes two checklist items under the macOS/kqueue Task 4.3 section:
//   "Test: same 1,000-coroutine loopback test as Task 4.2"
//   "Test: accept() and socket reads both work through the backend"
//
// STATUS BEFORE THIS FILE: both fully open — nothing macOS-specific had been
// exercised at all; the only coroutine-based IO tests (test_phase2_coroutine_loopback.c,
// test_phase2_into_chan.c) are Windows-only, written specifically to chase the
// IOCP cross-worker re-association bug and never intended to run against kqueue.
//
// This file is structured in two parts on purpose rather than one, because the
// two checklist items are different claims:
//   test_kqueue_accept_and_read()   — functional: does accept()+read() actually
//                                      route through the kqueue backend at all,
//                                      for a small number of connections.
//   test_kqueue_1000_coroutines()   — scale: does it hold at 1,000 concurrent
//                                      coroutines without deadlock.
// Passing the small functional test but failing the 1,000-scale one (or vice
// versa) is useful signal, so keeping them as separate sub-tests rather than
// folding scale into the first pass.
//
// FIXED (this pass), now that scheduler.c is available (same fixes as the
// Linux io_uring version of this test):
//   - ty_sched_run_until_idle() never existed — replaced with the real
//     ty_sched_run(void), which loops while active_coros > 0 and returns
//     once every coroutine (including ones spawned by other coroutines) has
//     finished. ty_sched_shutdown() is called exactly once, in main() after
//     BOTH sub-tests, not inside each sub-test — it joins worker threads
//     and tears down IO backends, so calling it after sub-test 1 would
//     break sub-test 2's use of the same scheduler.
//   - ty_spawn's first argument is ignored entirely by the real
//     implementation (`(void)arena;`) — task_placeholder did nothing;
//     passing NULL there now for clarity.
//   - A coroutine's first parameter is always co->arena, allocated fresh by
//     coro_new() — not a caller-controlled flag. Renamed every coroutine's
//     first parameter from `task` to `arena` to reflect that.
//   - accept_loop_coro/scale_accept_loop_coro were calling
//     slab_alloc_sized(NULL, ...) for spawn_args instead of using the real
//     arena they already had as their own first parameter — passing NULL
//     as the arena there was a separate bug, now fixed to use `arena`.
//
// ASSUMPTIONS STILL CARRIED (ty_net.c/ty_kq_backend.c not reviewed, only
// scheduler.c):
//   - no Typhoon-level connect(), so clients are raw OS sockets driven from a
//     helper pthread, not coroutines — this tests the *server*-side coroutine
//     path through kqueue, not both ends
//   - Network/Socket/Listener __ty_rt__ calls take (arena, self, ..., out) —
//     confirmed shape from the other net test files, not from ty_net.c itself
//
// Needs a real compile-and-run pass on actual Apple Silicon hardware (the doc's
// Phase 4 kqueue section separately flags an Apple Silicon page-size bug found
// this session — this test was not run against that fix specifically and could
// resurface it if the fix is incomplete).

#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <netinet/in.h>
#include <unistd.h>

#include "scheduler.h"
#include "ty_net.h"
#include "ty_mem.h"

#define MSG "kq-ping"
#define MSG_LEN 7

static TyStr make_str(SlabArena* a, const char* s) {
    size_t len = strlen(s);
    char* buf = slab_alloc_sized(a, len);
    memcpy(buf, s, len);
    return (TyStr){ .ptr = buf, .len = (int32_t)len };
}

// ---------- Sub-test 1: functional accept()+read() through kqueue ----------

typedef struct { int done; } ConnResult;
static ConnResult g_small_results[8];

static void small_conn_coro(void* arena, void* arg) {
    void** a = (void**)arg;
    TySocket* sock = (TySocket*)a[0];
    int idx = (int)(intptr_t)a[1];

    TyResult_i32_i32 read_res;
    char rbuf[MSG_LEN];
    __ty_rt__Socket__read(arena, sock, rbuf, MSG_LEN, &read_res);
    if (read_res.tag == 0 && read_res.value == MSG_LEN &&
        memcmp(rbuf, MSG, MSG_LEN) == 0) {
        g_small_results[idx].done = 1;
    }
}

static void small_accept_loop_coro(void* arena, void* arg) {
    TyListener* listener = (TyListener*)arg;
    for (int i = 0; i < 8; i++) {
        TyResult_Socket_i32 accept_res;
        __ty_rt__Listener__accept(arena, listener, &accept_res);
        if (accept_res.tag != 0) {
            fprintf(stderr, "small accept %d failed\n", i);
            continue;
        }
        void** spawn_args = slab_alloc_sized(arena, sizeof(void*) * 2);
        spawn_args[0] = accept_res.value;
        spawn_args[1] = (void*)(intptr_t)i;
        ty_spawn(NULL, small_conn_coro, spawn_args); // first arg ignored by ty_spawn
    }
}

static void* small_client_thread(void* arg) {
    int port = (int)(intptr_t)arg;
    for (int i = 0; i < 8; i++) {
        int fd = socket(AF_INET, SOCK_STREAM, 0);
        struct sockaddr_in sa = { 0 };
        sa.sin_family = AF_INET;
        sa.sin_port = htons((uint16_t)port);
        sa.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
        if (connect(fd, (struct sockaddr*)&sa, sizeof(sa)) == 0) {
            write(fd, MSG, MSG_LEN);
        } else {
            perror("small connect");
        }
        close(fd);
    }
    return NULL;
}

static int test_kqueue_accept_and_read(void) {
    memset(g_small_results, 0, sizeof(g_small_results));
    SlabArena* arena = slab_arena_new(); // no coroutine running yet at this scope
    TyNetwork net = { 0 };
    int port = 34568;

    TyResult_Listener_i32 listen_res;
    char addr[32];
    snprintf(addr, sizeof(addr), "127.0.0.1:%d", port);
    TyStr addr_str = make_str(arena, addr);
    __ty_rt__Network__listen(arena, &net, &addr_str, &listen_res);
    if (listen_res.tag != 0) {
        fprintf(stderr, "FAIL(small): listen failed\n");
        return 0;
    }

    ty_spawn(NULL, small_accept_loop_coro, listen_res.value); // first arg ignored by ty_spawn

    pthread_t tid;
    pthread_create(&tid, NULL, small_client_thread, (void*)(intptr_t)port);
    ty_sched_run(); // blocks until active_coros == 0
    pthread_join(tid, NULL);

    int ok = 1;
    for (int i = 0; i < 8; i++) {
        if (!g_small_results[i].done) {
            fprintf(stderr, "FAIL(small): connection %d never completed a "
                            "kqueue-backed accept+read\n", i);
            ok = 0;
        }
    }
    if (ok) printf("PASS: accept() and socket reads both route through kqueue\n");
    return ok;
}

// ---------- Sub-test 2: 1,000-coroutine scale ----------

#define N_CORO 1000
typedef struct { int done; } ScaleResult;
static ScaleResult g_scale_results[N_CORO];

static void scale_conn_coro(void* arena, void* arg) {
    void** a = (void**)arg;
    TySocket* sock = (TySocket*)a[0];
    int idx = (int)(intptr_t)a[1];

    TyResult_i32_i32 read_res;
    char rbuf[MSG_LEN];
    __ty_rt__Socket__read(arena, sock, rbuf, MSG_LEN, &read_res);
    if (read_res.tag == 0 && read_res.value == MSG_LEN &&
        memcmp(rbuf, MSG, MSG_LEN) == 0) {
        g_scale_results[idx].done = 1;
    }
}

static void scale_accept_loop_coro(void* arena, void* arg) {
    TyListener* listener = (TyListener*)arg;
    for (int i = 0; i < N_CORO; i++) {
        TyResult_Socket_i32 accept_res;
        __ty_rt__Listener__accept(arena, listener, &accept_res);
        if (accept_res.tag != 0) continue;
        void** spawn_args = slab_alloc_sized(arena, sizeof(void*) * 2);
        spawn_args[0] = accept_res.value;
        spawn_args[1] = (void*)(intptr_t)i;
        ty_spawn(NULL, scale_conn_coro, spawn_args); // first arg ignored by ty_spawn
    }
}

static void* scale_client_thread(void* arg) {
    int port = (int)(intptr_t)arg;
    for (int i = 0; i < N_CORO; i++) {
        int fd = socket(AF_INET, SOCK_STREAM, 0);
        struct sockaddr_in sa = { 0 };
        sa.sin_family = AF_INET;
        sa.sin_port = htons((uint16_t)port);
        sa.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
        if (connect(fd, (struct sockaddr*)&sa, sizeof(sa)) == 0) {
            write(fd, MSG, MSG_LEN);
        }
        close(fd);
    }
    return NULL;
}

static int test_kqueue_1000_coroutines(void) {
    memset(g_scale_results, 0, sizeof(g_scale_results));
    SlabArena* arena = slab_arena_new(); // no coroutine running yet at this scope
    TyNetwork net = { 0 };
    int port = 34569;

    TyResult_Listener_i32 listen_res;
    char addr[32];
    snprintf(addr, sizeof(addr), "127.0.0.1:%d", port);
    TyStr addr_str = make_str(arena, addr);
    __ty_rt__Network__listen(arena, &net, &addr_str, &listen_res);
    if (listen_res.tag != 0) {
        fprintf(stderr, "FAIL(scale): listen failed\n");
        return 0;
    }

    ty_spawn(NULL, scale_accept_loop_coro, listen_res.value); // first arg ignored by ty_spawn

    pthread_t tid;
    pthread_create(&tid, NULL, scale_client_thread, (void*)(intptr_t)port);
    ty_sched_run(); // blocks until active_coros == 0
    pthread_join(tid, NULL);

    int completed = 0;
    for (int i = 0; i < N_CORO; i++) if (g_scale_results[i].done) completed++;

    printf("%d / %d coroutine connections completed\n", completed, N_CORO);
    if (completed != N_CORO) {
        fprintf(stderr, "FAIL(scale): expected %d, got %d\n", N_CORO, completed);
        return 0;
    }
    printf("PASS: 1000 coroutine loopback connections completed on kqueue, no deadlock\n");
    return 1;
}

int main(void) {
    ty_sched_init();
    int ok = 1;
    ok &= test_kqueue_accept_and_read();
    ok &= test_kqueue_1000_coroutines();
    ty_sched_shutdown(); // once, after both sub-tests share the same scheduler
    return ok ? 0 : 1;
}
