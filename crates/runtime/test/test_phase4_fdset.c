/*
 * test_phase4_fdset.c — TyFdSet unit tests
 *
 * Tests per-worker fd set operations: init, add, remove, close_all, destroy.
 * Verifies: capacity growth, remove swap-truncation, close_all invalidation,
 * empty set operations, double-remove safety.
 */

#include "scheduler.h"
#include "platform.h"
#include <assert.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

/* ── Test 1: basic init/add/remove lifecycle ────────────────────────────── */

static void test_fdset_basic_add_remove(void) {
  TyFdSet set;
  ty_fdset_init(&set);

  /* Add a few fds */
  ty_fdset_add(&set, (ty_fd_t)10);
  ty_fdset_add(&set, (ty_fd_t)20);
  ty_fdset_add(&set, (ty_fd_t)30);
  assert(set.len == 3);

  /* Remove middle — swap-with-last truncation */
  int found = ty_fdset_remove(&set, (ty_fd_t)20);
  assert(found == 1);
  assert(set.len == 2);

  /* Remove already-removed — should return 0 */
  found = ty_fdset_remove(&set, (ty_fd_t)20);
  assert(found == 0);
  assert(set.len == 2);

  /* Remove non-existent */
  found = ty_fdset_remove(&set, (ty_fd_t)999);
  assert(found == 0);
  assert(set.len == 2);

  ty_fdset_destroy(&set);
  printf("[phase4] fdset basic add/remove — PASS\n");
}

/* ── Test 2: capacity growth beyond TY_FDSET_INIT_CAP ──────────────────── */

static void test_fdset_growth(void) {
  TyFdSet set;
  ty_fdset_init(&set);

  /* TY_FDSET_INIT_CAP is 256 — fill beyond that */
  for (size_t i = 0; i < 300; i++) {
    ty_fdset_add(&set, (ty_fd_t)(1000 + i));
  }
  assert(set.len == 300);
  assert(set.cap > 256); /* must have grown */

  /* Remove some and verify count */
  for (size_t i = 0; i < 50; i++) {
    int found = ty_fdset_remove(&set, (ty_fd_t)(1000 + i));
    assert(found == 1);
  }
  assert(set.len == 250);

  ty_fdset_destroy(&set);
  printf("[phase4] fdset capacity growth — PASS\n");
}

/* ── Test 3: close_all resets len to 0 ─────────────────────────────────── */

static void test_fdset_close_all(void) {
  TyFdSet set;
  ty_fdset_init(&set);

  /* Use fake fds that are safe to "close" — we override ty_fd_close
   * by using invalid fds. close_all will call ty_fd_close on each
   * but invalid fds just return -1 (harmless). */
  ty_fdset_add(&set, TY_FD_INVALID);
  ty_fdset_add(&set, TY_FD_INVALID);
  ty_fdset_add(&set, TY_FD_INVALID);
  assert(set.len == 3);

  ty_fdset_close_all(&set);
  assert(set.len == 0);

  /* close_all on empty set is safe */
  ty_fdset_close_all(&set);
  assert(set.len == 0);

  ty_fdset_destroy(&set);
  printf("[phase4] fdset close_all — PASS\n");
}

/* ── Test 4: remove-last element (swap-with-self edge case) ────────────── */

static void test_fdset_remove_last(void) {
  TyFdSet set;
  ty_fdset_init(&set);

  ty_fdset_add(&set, (ty_fd_t)42);
  assert(set.len == 1);

  int found = ty_fdset_remove(&set, (ty_fd_t)42);
  assert(found == 1);
  assert(set.len == 0);

  /* Adding after remove-last should work */
  ty_fdset_add(&set, (ty_fd_t)99);
  assert(set.len == 1);

  ty_fdset_destroy(&set);
  printf("[phase4] fdset remove-last edge case — PASS\n");
}

/* ── Test 5: destroy without close_all (resource leak safety) ──────────── */

static void test_fdset_destroy_without_close(void) {
  TyFdSet set;
  ty_fdset_init(&set);

  ty_fdset_add(&set, (ty_fd_t)100);
  ty_fdset_add(&set, (ty_fd_t)200);
  /* Don't close_all — destroy should free the buffer and mutex */
  ty_fdset_destroy(&set);
  printf("[phase4] fdset destroy without close_all — PASS\n");
}

/* ── Test 6: concurrent multi-thread add/remove ────────────────────────── */

#define RACE_FDS 100
#define RACE_ITERS 200

typedef struct {
  TyFdSet* set;
  int start_fd;
  int count;
} RaceAddArg;

static void* race_add_thread(void* arg) {
  RaceAddArg* a = (RaceAddArg*)arg;
  for (int i = 0; i < a->count; i++) {
    ty_fdset_add(a->set, (ty_fd_t)(a->start_fd + i));
  }
  return NULL;
}

static void* race_remove_thread(void* arg) {
  RaceAddArg* a = (RaceAddArg*)arg;
  for (int i = 0; i < a->count; i++) {
    ty_fdset_remove(a->set, (ty_fd_t)(a->start_fd + i));
  }
  return NULL;
}

static void test_fdset_concurrent_add_remove(void) {
  for (int iter = 0; iter < RACE_ITERS; iter++) {
    TyFdSet set;
    ty_fdset_init(&set);

    RaceAddArg argA = { &set, 0, RACE_FDS };
    RaceAddArg argB = { &set, RACE_FDS, RACE_FDS };

    TyThread tA, tB;
    ty_thread_create(&tA, race_add_thread, &argA);
    ty_thread_create(&tB, race_add_thread, &argB);
    ty_thread_join(tA);
    ty_thread_join(tB);

    /* After both adds, len should be exactly 2*RACE_FDS */
    assert(set.len == (size_t)(2 * RACE_FDS));

    /* Concurrent remove from two threads */
    RaceAddArg rmA = { &set, 0, RACE_FDS };
    RaceAddArg rmB = { &set, RACE_FDS, RACE_FDS };
    ty_thread_create(&tA, race_remove_thread, &rmA);
    ty_thread_create(&tB, race_remove_thread, &rmB);
    ty_thread_join(tA);
    ty_thread_join(tB);

    assert(set.len == 0);
    ty_fdset_destroy(&set);
  }
  printf("[phase4] fdset concurrent add/remove (%d iters) — PASS\n", RACE_ITERS);
}

/* ── main ─────────────────────────────────────────────────────────────────── */

int main(void) {
  test_fdset_basic_add_remove();
  test_fdset_growth();
  test_fdset_close_all();
  test_fdset_remove_last();
  test_fdset_destroy_without_close();
  test_fdset_concurrent_add_remove();
  printf("[phase4] All TyFdSet tests PASSED\n");
  return 0;
}
