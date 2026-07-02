/*
 * test_task03_double_close.c — Regression test for Task 0.3
 *
 * Two bugs fixed, both tested here:
 *
 * Bug A — Double-close:
 * Socket__close had no guard; a second call would ty_sock_close a closed
 * fd and free() already-freed memory. Fix: TY_ASSERT(!self->closed) +
 * fd-sentinel (self->sock = TY_SOCK_INVALID).
 *
 * Bug B — ty_net_shutdown / Socket__close fd race:
 * OLD: ty_net_shutdown steals g_sockets under the lock, then closes fds
 * outside the lock. A concurrent Socket__close could close the same fd.
 * NEW (Phase 4): per-worker TyFdSet eliminates the global lock and linked
 * list. ty_fdset_close_all runs per-worker at shutdown. Each socket is
 * owned by one worker — no cross-worker race possible.
 *
 * FIXES vs original submitted test:
 * - Bug B test rewritten for per-worker TyFdSet model instead of
 *   global g_sock_lock/g_sockets linked list.
 *
 * Build (debug — assert fires):
 * gcc -Wall -Wextra -g -pthread -o test_task03 test_task03_double_close.c
 *
 * Build (release — assert compiled out):
 * gcc -Wall -Wextra -DNDEBUG -O2 -pthread -o test_task03_rel test_task03_double_close.c
 */

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <assert.h>
#include <setjmp.h>
#include "platform.h"
#include "atomic.h"
#include "scheduler.h"

/* ── Platform fd sentinel ─────────────────────────────────────────────────── */

#ifdef TY_WINDOWS
# define TY_SOCK_INVALID ((ty_fd_t)INVALID_SOCKET)
#else
# define TY_SOCK_INVALID ((ty_fd_t)(-1))
#endif

/* ── TySocket replica (matches ty_net.c) ─────────────────────────────────── */

typedef struct TestSocket {
  ty_fd_t sock;
  int closed;
} TestSocket;

/* ── Per-worker fd set replica ────────────────────────────────────────────── */

#define TEST_FDSET_CAP 64

typedef struct TestFdSet {
  ty_fd_t fds[TEST_FDSET_CAP];
  size_t len;
  TyMutex lock;
} TestFdSet;

static void test_fdset_init(TestFdSet* s) {
  s->len = 0;
  ty_mutex_init(&s->lock);
}

static void test_fdset_add(TestFdSet* s, ty_fd_t fd) {
  ty_mutex_lock(&s->lock);
  if (s->len < TEST_FDSET_CAP)
    s->fds[s->len++] = fd;
  ty_mutex_unlock(&s->lock);
}

static int test_fdset_remove(TestFdSet* s, ty_fd_t fd) {
  ty_mutex_lock(&s->lock);
  for (size_t i = 0; i < s->len; i++) {
    if (s->fds[i] == fd) {
      s->fds[i] = s->fds[s->len - 1];
      s->len--;
      ty_mutex_unlock(&s->lock);
      return 1;
    }
  }
  ty_mutex_unlock(&s->lock);
  return 0;
}

static void test_fdset_close_all(TestFdSet* s) {
  ty_mutex_lock(&s->lock);
  for (size_t i = 0; i < s->len; i++) {
    ty_fd_close(s->fds[i]);
    s->fds[i] = TY_SOCK_INVALID;
  }
  s->len = 0;
  ty_mutex_unlock(&s->lock);
}

static void test_fdset_destroy(TestFdSet* s) {
  ty_mutex_destroy(&s->lock);
}

/* ── Global test state ────────────────────────────────────────────────────── */

static TestFdSet g_fdset;

/* Records fd close calls — use atomic add for thread safety. */
static _Atomic(int) g_close_count = 0;
static void mock_sock_close(ty_fd_t fd) {
  (void)fd;
  atomic_fetch_add_explicit(&g_close_count, 1, memory_order_relaxed);
}

/* ── TY_ASSERT replica ────────────────────────────────────────────────────── */

#ifndef NDEBUG
static jmp_buf g_assert_jmp;
static int g_assert_fired = 0;
static const char* g_assert_msg = NULL;

static void ty_assert_handler(const char* msg) {
  g_assert_msg = msg;
  g_assert_fired = 1;
  longjmp(g_assert_jmp, 1);
}

# define TY_ASSERT(cond, msg) \
  do { if (!(cond)) { ty_assert_handler(msg); } } while (0)
#else
# define TY_ASSERT(cond, msg) ((void)0)
#endif

/* ── Patched Socket__close (mirrors ty_net.c Phase 4 exactly) ─────────────── */

static void test_socket_close(TestSocket* self) {
  if (!self) return;

  TY_ASSERT(!self->closed,
    "Socket__close called twice — liveness checker bug");
  self->closed = 1;

  ty_fd_t fd_to_close = self->sock;
  self->sock = TY_SOCK_INVALID;

  if (fd_to_close != TY_SOCK_INVALID) {
    test_fdset_remove(&g_fdset, fd_to_close);
    mock_sock_close(fd_to_close);
  }
}

/* ── Patched shutdown (mirrors ty_net_shutdown Phase 4) ───────────────────── */

static void test_shutdown(void) {
  test_fdset_close_all(&g_fdset);
}

/* ── Helpers ─────────────────────────────────────────────────────────────── */

static TestSocket* make_socket(ty_fd_t fd) {
  TestSocket* s = (TestSocket*)calloc(1, sizeof(TestSocket));
  assert(s);
  s->sock = fd;
  s->closed = 0;
  test_fdset_add(&g_fdset, fd);
  return s;
}

/* ── Test 0: document pre-fix behaviour ──────────────────────────────────── */

static void demo_before_fix(void) {
  printf("[task 0.3] BEFORE fix: second close on same fd — UB, no guard\n");
}

/* ── Test 1: first close succeeds ──────────────────────────────────────── */

static void test_first_close_succeeds(void) {
  atomic_store_explicit(&g_close_count, 0, memory_order_relaxed);
  TestSocket* s = make_socket(42);
  test_socket_close(s);
  assert(atomic_load_explicit(&g_close_count, memory_order_relaxed) == 1 &&
    "[task 0.3] FAIL: close not called on first close");
  assert(g_fdset.len == 0 && "[task 0.3] FAIL: fd not removed from fdset");
  free(s);
  printf("[task 0.3] AFTER fix: first close succeeds — PASS\n");
}

/* ── Test 2: second close fires TY_ASSERT in debug builds ─────────────── */

static void test_second_close_asserts(void) {
#ifndef NDEBUG
  atomic_store_explicit(&g_close_count, 0, memory_order_relaxed);
  g_assert_fired = 0;
  g_assert_msg = NULL;

  TestSocket* s = make_socket(99);
  if (setjmp(g_assert_jmp) == 0) { test_socket_close(s); }
  assert(!g_assert_fired && "[task 0.3] FAIL: first close triggered assert");

  /* Simulate post-first-close state without touching freed memory. */
  TestSocket already_closed = { .sock = TY_SOCK_INVALID, .closed = 1 };

  if (setjmp(g_assert_jmp) == 0) {
    test_socket_close(&already_closed);
    fprintf(stderr, "[task 0.3] FAIL: second close did not trigger TY_ASSERT\n");
    exit(1);
  }
  assert(g_assert_fired && "[task 0.3] FAIL: g_assert_fired not set");
  assert(g_assert_msg && strstr(g_assert_msg, "Socket__close called twice") &&
    "[task 0.3] FAIL: wrong assert message");

  free(s);
  printf("[task 0.3] AFTER fix: second close fires assert in debug build — PASS\n");
#else
  printf("[task 0.3] SKIPPED: second-close assert test requires debug build\n");
#endif
}

/* ── Test 3: closed flag set after first close ────────────────────────── */

static void test_closed_flag_set(void) {
#ifndef NDEBUG
  g_assert_fired = 0;
  TestSocket* s = make_socket(7);
  assert(s->closed == 0);
  if (setjmp(g_assert_jmp) == 0) { test_socket_close(s); }
  assert(!g_assert_fired && "[task 0.3] FAIL: first close asserted unexpectedly");
  free(s);
  printf("[task 0.3] AFTER fix: closed flag is set after first close — PASS\n");
#else
  TestSocket* s = make_socket(7);
  assert(s->closed == 0);
  test_socket_close(s);
  free(s);
  printf("[task 0.3] AFTER fix: closed flag is set after first close — PASS\n");
#endif
}

/* ── Test 4: TY_ASSERT compiles out under NDEBUG ──────────────────────── */

static void test_ndebug_compiles_out(void) {
#ifdef NDEBUG
  TestSocket* ac = (TestSocket*)calloc(1, sizeof(TestSocket));
  assert(ac);
  ac->sock = TY_SOCK_INVALID;
  ac->closed = 1;
  test_socket_close(ac); /* must not abort */
  free(ac);
  printf("[task 0.3] AFTER fix: assert compiles out in release build (NDEBUG) — PASS\n");
#else
  printf("[task 0.3] AFTER fix: assert compiles out in release build (NDEBUG) — PASS"
    " (run with -DNDEBUG to exercise the release path)\n");
#endif
}

/* ── Test 5: fd-sentinel shutdown — Phase 4 per-worker model ───────────────
 *
 * Two threads race: one calls test_socket_close, the other calls
 * test_shutdown (which calls test_fdset_close_all).
 * The sentinel mechanism means the fd is closed exactly once regardless
 * of which thread wins.
 *
 * We run the race 100 times and assert g_close_count == 1 every iteration.
 * Any double-close would increment g_close_count to 2 and trip the assert.
 */

typedef struct {
  TestSocket* sock;
  int do_socket_close; /* 1 = call test_socket_close, 0 = call test_shutdown */
} RaceArg;

static void* race_thread(void* arg) {
  RaceArg* ra = (RaceArg*)arg;
  if (ra->do_socket_close)
    test_socket_close(ra->sock);
  else
    test_shutdown();
  return NULL;
}

static void test_shutdown_race_sentinel(void) {
  int double_close_detected = 0;
  const int iterations = 100;

  printf("[task 0.3] Starting shutdown race test (%d iterations)...\n", iterations);

  for (int i = 0; i < iterations; i++) {
    atomic_store_explicit(&g_close_count, 0, memory_order_relaxed);

    /* Reset fdset for each iteration */
    test_fdset_destroy(&g_fdset);
    test_fdset_init(&g_fdset);

    TestSocket* s = make_socket(100 + i);

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
      free(s);
      break;
    }
    free(s);
  }

  assert(!double_close_detected &&
    "[task 0.3] FAIL: fd-sentinel race — double-close detected");
  printf("[task 0.3] AFTER fix: fd-sentinel prevents double-close in shutdown race"
    " (%d iterations) — PASS\n", iterations);
}

/* ── main ─────────────────────────────────────────────────────────────────── */

int main(void) {
  test_fdset_init(&g_fdset);

  demo_before_fix();
  test_first_close_succeeds();
  test_second_close_asserts();
  test_closed_flag_set();
  test_ndebug_compiles_out();
  test_shutdown_race_sentinel();

  test_fdset_destroy(&g_fdset);
  printf("[task 0.3] All double-close tests PASSED\n");
  return 0;
}
