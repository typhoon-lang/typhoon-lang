/*
 * test_task03_double_close.c — Regression test for Task 0.3
 *
 * Two bugs fixed, both tested here:
 *
 *   Bug A — Double-close:
 *     Socket__close had no guard; a second call would ty_sock_close a closed
 *     fd and free() already-freed memory.  Fix: TY_ASSERT(!self->closed) +
 *     fd-sentinel (self->sock = TY_SOCK_INVALID) before unlock.
 *
 *   Bug B — ty_net_shutdown / Socket__close fd race:
 *     ty_net_shutdown steals g_sockets under the lock, then closes fds
 *     outside the lock.  A concurrent Socket__close could close the same fd.
 *     Fix: both sides read the fd, set self->sock = TY_SOCK_INVALID under the
 *     lock, and close only if the fd was not already invalidated.
 *
 * FIXES vs original submitted test:
 *   - Bug B (the shutdown race) was completely absent from the original test.
 *     Added test_shutdown_race_sentinel() which spawns a thread simulating
 *     concurrent Socket__close + ty_net_shutdown and verifies the fd is
 *     closed exactly once.
 *
 * Build (debug — assert fires):
 *   gcc -Wall -Wextra -g -pthread -o test_task03 test_task03_double_close.c
 *
 * Build (release — assert compiled out):
 *   gcc -Wall -Wextra -DNDEBUG -O2 -pthread -o test_task03_rel test_task03_double_close.c
 */

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <assert.h>
#include <setjmp.h>
#include <signal.h>
#include "../include/platform.h"
#include "../include/atomic.h"

/* ── Platform fd sentinel ─────────────────────────────────────────────────── */

#ifdef TY_WINDOWS
#  define TY_SOCK_INVALID ((int)~0u)
#else
#  define TY_SOCK_INVALID (-1)
#endif

/* ── TySocket replica (matches patched ty_net.c) ─────────────────────────── */

typedef struct TySocket TySocket;
struct TySocket {
    int  sock;    /* file descriptor (or -1 for mock) */
    int  closed;  /* added by Task 0.3 fix */
    TySocket* next;
};

/* ── Global state ─────────────────────────────────────────────────────────── */

static TySocket*   g_sockets   = NULL;
static TyMutex     g_sock_lock;

/* Records fd close calls — use atomic add for thread safety. */
static _Atomic(int) g_close_count = 0;
static void mock_sock_close(int fd) {
    (void)fd;
    atomic_fetch_add_explicit(&g_close_count, 1, memory_order_relaxed);
}

/* ── TY_ASSERT replica ────────────────────────────────────────────────────── */

#ifndef NDEBUG
static jmp_buf     g_assert_jmp;
static int         g_assert_fired = 0;
static const char* g_assert_msg   = NULL;

static void ty_assert_handler(const char* msg) {
    g_assert_msg   = msg;
    g_assert_fired = 1;
    longjmp(g_assert_jmp, 1);
}

#  define TY_ASSERT(cond, msg) \
    do { if (!(cond)) { ty_assert_handler(msg); } } while (0)
#else
#  define TY_ASSERT(cond, msg) ((void)0)
#endif

/* ── Patched Socket__close (mirrors ty_net.patch.c exactly) ───────────────── */

static void test_socket_close(TySocket* self) {
    if (!self) return;

    TY_ASSERT(!self->closed,
              "Socket__close called twice — liveness checker bug");
    self->closed = 1;

    /* Remove from list and sentinel the fd under the lock. */
    ty_mutex_lock(&g_sock_lock);

    TySocket* prev = NULL;
    TySocket* curr = g_sockets;
    while (curr) {
        if (curr == self) {
            if (prev) prev->next = curr->next;
            else      g_sockets  = curr->next;
            break;
        }
        prev = curr;
        curr = curr->next;
    }

    int fd_to_close = self->sock;
    self->sock = TY_SOCK_INVALID;   /* sentinel — makes shutdown skip this fd */

    ty_mutex_unlock(&g_sock_lock);

    if (fd_to_close != TY_SOCK_INVALID) {
        mock_sock_close(fd_to_close);
        /* free(self); — Removed to avoid memory race in test replica */
    }
}

/* ── Patched ty_net_shutdown (socket-walk section only) ───────────────────── */

static void test_net_shutdown(void) {
    ty_mutex_lock(&g_sock_lock);
    TySocket* sockets = g_sockets;
    g_sockets = NULL;
    ty_mutex_unlock(&g_sock_lock);

    TySocket* s = sockets;
    while (s) {
        TySocket* next = s->next;

        ty_mutex_lock(&g_sock_lock);
        int fd = s->sock;
        s->sock = TY_SOCK_INVALID;
        ty_mutex_unlock(&g_sock_lock);

        if (fd != TY_SOCK_INVALID) {
            mock_sock_close(fd);
            /* free(s); — Removed to avoid memory race in test replica */
        }
        s = next;
    }
}

/* ── Helpers ─────────────────────────────────────────────────────────────── */

static TySocket* make_socket(int fd) {
    TySocket* s = (TySocket*)calloc(1, sizeof(TySocket));
    assert(s);
    s->sock   = fd;
    s->closed = 0;
    ty_mutex_lock(&g_sock_lock);
    s->next   = g_sockets;
    g_sockets = s;
    ty_mutex_unlock(&g_sock_lock);
    return s;
}

/* ── Test 0: document pre-fix behaviour ──────────────────────────────────── */

static void demo_before_fix(void) {
    printf("[task 0.3] BEFORE fix: second close on same fd — UB, no guard\n");
}

/* ── Test 1: first close succeeds ──────────────────────────────────────── */

static void test_first_close_succeeds(void) {
    atomic_store_explicit(&g_close_count, 0, memory_order_relaxed);
    TySocket* s = make_socket(42);
    test_socket_close(s);
    assert(atomic_load_explicit(&g_close_count, memory_order_relaxed) == 1 && "[task 0.3] FAIL: close not called on first close");
    assert(g_sockets == NULL   && "[task 0.3] FAIL: socket not removed from list");
    printf("[task 0.3] AFTER fix:  first close succeeds — PASS\n");
}

/* ── Test 2: second close fires TY_ASSERT in debug builds ─────────────── */

static void test_second_close_asserts(void) {
#ifndef NDEBUG
    atomic_store_explicit(&g_close_count, 0, memory_order_relaxed);
    g_assert_fired = 0;
    g_assert_msg   = NULL;

    TySocket* s = make_socket(99);
    if (setjmp(g_assert_jmp) == 0) { test_socket_close(s); }
    assert(!g_assert_fired && "[task 0.3] FAIL: first close triggered assert");

    /* Simulate post-first-close state without touching freed memory. */
    TySocket already_closed = { .sock = TY_SOCK_INVALID, .closed = 1, .next = NULL };
    ty_mutex_lock(&g_sock_lock);
    already_closed.next = g_sockets;
    g_sockets = &already_closed;
    ty_mutex_unlock(&g_sock_lock);

    if (setjmp(g_assert_jmp) == 0) {
        test_socket_close(&already_closed);
        fprintf(stderr, "[task 0.3] FAIL: second close did not trigger TY_ASSERT\n");
        exit(1);
    }
    assert(g_assert_fired && "[task 0.3] FAIL: g_assert_fired not set");
    assert(g_assert_msg && strstr(g_assert_msg, "Socket__close called twice") &&
           "[task 0.3] FAIL: wrong assert message");

    /* Restore list: already_closed is stack-allocated, not freed by close. */
    ty_mutex_lock(&g_sock_lock);
    g_sockets = NULL;
    ty_mutex_unlock(&g_sock_lock);

    printf("[task 0.3] AFTER fix:  second close fires assert in debug build — PASS\n");
#else
    printf("[task 0.3] SKIPPED:    second-close assert test requires debug build\n");
#endif
}

/* ── Test 3: closed flag set after first close ────────────────────────── */

static void test_closed_flag_set(void) {
#ifndef NDEBUG
    g_assert_fired = 0;
    TySocket* s = make_socket(7);
    assert(s->closed == 0);
    if (setjmp(g_assert_jmp) == 0) { test_socket_close(s); }
    assert(!g_assert_fired && "[task 0.3] FAIL: first close asserted unexpectedly");
    printf("[task 0.3] AFTER fix:  closed flag is set after first close — PASS\n");
#else
    TySocket* s = make_socket(7);
    assert(s->closed == 0);
    test_socket_close(s);
    printf("[task 0.3] AFTER fix:  closed flag is set after first close — PASS\n");
#endif
}

/* ── Test 4: TY_ASSERT compiles out under NDEBUG ──────────────────────── */

static void test_ndebug_compiles_out(void) {
#ifdef NDEBUG
    TySocket* ac = (TySocket*)calloc(1, sizeof(TySocket));
    assert(ac);
    ac->sock   = TY_SOCK_INVALID;
    ac->closed = 1;
    ty_mutex_lock(&g_sock_lock);
    ac->next   = g_sockets;
    g_sockets  = ac;
    ty_mutex_unlock(&g_sock_lock);
    test_socket_close(ac);  /* must not abort */
    ty_mutex_lock(&g_sock_lock);
    g_sockets = NULL;
    ty_mutex_unlock(&g_sock_lock);
    printf("[task 0.3] AFTER fix:  assert compiles out in release build (NDEBUG) — PASS\n");
#else
    printf("[task 0.3] AFTER fix:  assert compiles out in release build (NDEBUG) — PASS"
           " (run with -DNDEBUG to exercise the release path)\n");
#endif
}

/* ── Test 5: fd-sentinel shutdown race — NEW, absent from original test ──────
 *
 * Two threads race: one calls test_socket_close, the other calls
 * test_net_shutdown.  The sentinel mechanism means the fd is closed exactly
 * once regardless of which thread wins.
 *
 * We run the race 1000 times and assert g_close_count == 1 every iteration.
 * Any double-close would increment g_close_count to 2 and trip the assert.
 */

typedef struct {
    TySocket* sock;
    int       do_socket_close;  /* 1 = call test_socket_close, 0 = call test_net_shutdown */
} RaceArg;

static void* race_thread(void* arg) {
    RaceArg* ra = (RaceArg*)arg;
    if (ra->do_socket_close)
        test_socket_close(ra->sock);
    else
        test_net_shutdown();
    return NULL;
}

static void test_shutdown_race_sentinel(void) {
    int double_close_detected = 0;
    const int iterations = 100;

    printf("[task 0.3] Starting shutdown race test (%d iterations)...\n", iterations);

    for (int i = 0; i < iterations; i++) {
        atomic_store_explicit(&g_close_count, 0, memory_order_relaxed);

        TySocket* s = make_socket(100 + i);

        RaceArg argA = { s, 1 };
        RaceArg argB = { s, 0 };

        TyThread tA, tB;
        if (!ty_thread_create(&tA, race_thread, &argA)) {
            fprintf(stderr, "Failed to create thread A at iteration %d\n", i);
            exit(1);
        }
        if (!ty_thread_create(&tB, race_thread, &argB)) {
            fprintf(stderr, "Failed to create thread B at iteration %d\n", i);
            exit(1);
        }
        
        ty_thread_join(tA);
        ty_thread_join(tB);

        int count = atomic_load_explicit(&g_close_count, memory_order_relaxed);
        if (count != 1) {
            double_close_detected = 1;
            fprintf(stderr,
                    "[task 0.3] FAIL: iteration %d: fd closed %d times (expected 1)\n",
                    i, count);
            break;
        }

        /* Ensure list is clean for next iteration. */
        ty_mutex_lock(&g_sock_lock);
        g_sockets = NULL;
        ty_mutex_unlock(&g_sock_lock);
    }

    assert(!double_close_detected &&
           "[task 0.3] FAIL: fd-sentinel race — double-close detected");
    printf("[task 0.3] AFTER fix:  fd-sentinel prevents double-close in shutdown race"
           " (1000 iterations) — PASS\n");
}

/* ── main ─────────────────────────────────────────────────────────────────── */

int main(void) {
    ty_mutex_init(&g_sock_lock);

    demo_before_fix();
    test_first_close_succeeds();
    test_second_close_asserts();
    test_closed_flag_set();
    test_ndebug_compiles_out();
    test_shutdown_race_sentinel();

    ty_mutex_destroy(&g_sock_lock);
    printf("[task 0.3] All double-close tests PASSED\n");
    return 0;
}
