/*
 * test_phase4_mock_io.c — Mock IO backend submit/complete round-trip test
 *
 * Uses ty_io_backend_use_mock to verify submit/poll/complete without
 * real kernel IO. Tests:
 * - submit enqueues TyIoOp
 * - mock_count tracks submissions
 * - mock_get retrieves ops by index
 * - mock_complete fires wake and delivers result
 * - poll returns 0 (mock backend has no real completions)
 */

#include "ty_io_backend.h"
#include "scheduler.h"
#include "platform.h"
#include <assert.h>
#include <stdio.h>
#include <string.h>

/* ── Test 1: submit and count ───────────────────────────────────────────── */

static void test_mock_submit_count(void) {
  ty_io_backend_use_mock(1);
  assert(ty_io_mock_count() == 0);

  TyIoOp op1;
  memset(&op1, 0, sizeof(op1));
  op1.type = TY_IO_OP_READ;
  op1.fd = (ty_fd_t)42;
  op1.buf = (void*)0xDEAD;
  op1.len = 100;
  op1.coro = NULL; /* NULL coro — no wake on complete */

  int rc = ty_io_submit(&op1);
  assert(rc == 0);
  assert(ty_io_mock_count() == 1);

  TyIoOp op2;
  memset(&op2, 0, sizeof(op2));
  op2.type = TY_IO_OP_WRITE;
  op2.fd = (ty_fd_t)99;
  op2.buf = (void*)0xBEEF;
  op2.len = 200;
  op2.coro = NULL;

  rc = ty_io_submit(&op2);
  assert(rc == 0);
  assert(ty_io_mock_count() == 2);

  ty_io_backend_use_mock(0);
  printf("[phase4] mock submit and count — PASS\n");
}

/* ── Test 2: mock_get retrieves ops correctly ───────────────────────────── */

static void test_mock_get_ops(void) {
  ty_io_backend_use_mock(1);

  TyIoOp ops[3];
  for (int i = 0; i < 3; i++) {
    memset(&ops[i], 0, sizeof(ops[i]));
    ops[i].type = (i % 2 == 0) ? TY_IO_OP_READ : TY_IO_OP_WRITE;
    ops[i].fd = (ty_fd_t)(100 + i);
    ops[i].len = (size_t)(10 + i);
    ops[i].coro = NULL;
    ty_io_submit(&ops[i]);
  }
  assert(ty_io_mock_count() == 3);

  /* Verify retrieval */
  const TyIoOp* r0 = ty_io_mock_get(0);
  assert(r0 != NULL);
  assert(r0->fd == (ty_fd_t)100);
  assert(r0->type == TY_IO_OP_READ);
  assert(r0->len == 10);

  const TyIoOp* r1 = ty_io_mock_get(1);
  assert(r1 != NULL);
  assert(r1->fd == (ty_fd_t)101);
  assert(r1->type == TY_IO_OP_WRITE);
  assert(r1->len == 11);

  const TyIoOp* r2 = ty_io_mock_get(2);
  assert(r2 != NULL);
  assert(r2->fd == (ty_fd_t)102);

  /* Out-of-bounds returns NULL */
  assert(ty_io_mock_get(3) == NULL);
  assert(ty_io_mock_get(-1) == NULL);

  ty_io_backend_use_mock(0);
  printf("[phase4] mock get ops — PASS\n");
}

/* ── Test 3: use_mock(0) resets state ───────────────────────────────────── */

static void test_mock_reset(void) {
  ty_io_backend_use_mock(1);
  TyIoOp op;
  memset(&op, 0, sizeof(op));
  op.type = TY_IO_OP_READ;
  ty_io_submit(&op);
  assert(ty_io_mock_count() == 1);

  ty_io_backend_use_mock(0);
  assert(ty_io_mock_count() == 0);

  ty_io_backend_use_mock(1);
  assert(ty_io_mock_count() == 0);
  ty_io_backend_use_mock(0);
  printf("[phase4] mock reset — PASS\n");
}

/* ── Test 4: poll returns 0 for mock backend ───────────────────────────── */

static void test_mock_poll_returns_zero(void) {
  ty_io_backend_use_mock(1);

  TyIoOp op;
  memset(&op, 0, sizeof(op));
  op.type = TY_IO_OP_READ;
  ty_io_submit(&op);

  /* Mock backend poll always returns 0 (no real completions) */
  int n = ty_io_poll();
  assert(n == 0);

  ty_io_backend_use_mock(0);
  printf("[phase4] mock poll returns 0 — PASS\n");
}

/* ── Test 5: mock complete calls wake (with NULL coro — no crash) ──────── */

static void test_mock_complete_null_coro(void) {
  ty_io_backend_use_mock(1);

  TyIoOp op;
  memset(&op, 0, sizeof(op));
  op.type = TY_IO_OP_READ;
  op.coro = NULL;
  ty_io_submit(&op);

  /* Completing with NULL coro should not crash */
  ty_io_mock_complete(0, 42);
  /* ty_io_wake_coro(NULL, 42) is a no-op */

  ty_io_backend_use_mock(0);
  printf("[phase4] mock complete with null coro — PASS\n");
}

/* ── Test 6: overflow cap returns -1 ───────────────────────────────────── */

static void test_mock_overflow(void) {
  ty_io_backend_use_mock(1);

  /* MOCK_CAP is 1024 — fill it */
  for (int i = 0; i < 1024; i++) {
    TyIoOp op;
    memset(&op, 0, sizeof(op));
    op.type = TY_IO_OP_READ;
    op.fd = (ty_fd_t)i;
    int rc = ty_io_submit(&op);
    assert(rc == 0);
  }
  assert(ty_io_mock_count() == 1024);

  /* 1025th submit should fail */
  TyIoOp overflow;
  memset(&overflow, 0, sizeof(overflow));
  overflow.type = TY_IO_OP_READ;
  int rc = ty_io_submit(&overflow);
  assert(rc == -1);

  ty_io_backend_use_mock(0);
  printf("[phase4] mock overflow cap — PASS\n");
}

/* ── main ─────────────────────────────────────────────────────────────────── */

int main(void) {
  test_mock_submit_count();
  test_mock_get_ops();
  test_mock_reset();
  test_mock_poll_returns_zero();
  test_mock_complete_null_coro();
  test_mock_overflow();
  printf("[phase4] All mock IO backend tests PASSED\n");
  return 0;
}
