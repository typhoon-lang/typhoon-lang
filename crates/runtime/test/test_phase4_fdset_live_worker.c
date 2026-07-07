/*
 * test_phase4_fdset_live_worker.c — Task 4.5, closing a specific gap
 * found while reviewing the existing test suite (not a checklist item
 * with pre-existing wording of its own).
 *
 * test_phase4_net_fdset.c's own header comment says it runs "without
 * the full scheduler (no worker threads), so ty_sched_current_worker()
 * returns NULL" — meaning ty_net.c's ty_fdset_add/remove calls are
 * skipped entirely via their NULL-worker guard for every test in that
 * file. It proves socket open/accept/write/close work correctly in
 * isolation, but provides zero evidence that TyFdSet — the actual
 * mechanism Task 4.5 replaces the old g_sockets global registry with —
 * tracks anything when a real worker exists. This test runs the same
 * kind of socket lifecycle, but from inside a spawned coroutine after
 * ty_sched_init(), so ty_sched_current_worker() returns a real Worker*
 * and Worker.fd_set should actually see the fd added and removed.
 *
 * Confirmed from scheduler.h: Worker.fd_set is a plain embedded
 * TyFdSet (not a pointer, not opaque) with a public `len` field, and
 * ty_sched_current_worker() is documented to "return the Worker struct
 * for the calling thread, or NULL if not a worker" — so calling it from
 * inside a coroutine running on a worker thread should return non-NULL.
 *
 * ASSUMPTION carried over from the other two new Phase 2 coroutine test
 * files: ty_spawn callable from bare main() after ty_sched_init(). See
 * test_phase2_coroutine_loopback.c's header for the full caveat.
 */

#include "ty_net.h"
#include "ty_mem.h"
#include "scheduler.h"
#include <assert.h>
#include <stdint.h>
#include <string.h>
#include <stdio.h>

static TyStr make_str(const char* s) {
  TyStr str;
  str.ptr = (char*)s;
  str.len = (int32_t)strlen(s);
  return str;
}

typedef struct {
  TyNetwork* net;
  size_t fdset_len_before_listen;
  size_t fdset_len_after_listen;
  size_t fdset_len_after_close;
  int worker_was_null;
  volatile int finished;
} FdSetCtx;

static void fdset_coro(void* task, void* arg) {
  FdSetCtx* ctx = (FdSetCtx*)arg;

  Worker* w = ty_sched_current_worker();
  if (!w) {
    /* If this assumption is wrong (spawning from main() doesn't give
     * coroutines a real worker context the way I inferred), fail loud
     * and clearly rather than silently "passing" a test that measured
     * nothing — same NULL-worker situation the existing
     * test_phase4_net_fdset.c is already stuck in. */
    ctx->worker_was_null = 1;
    ctx->finished = 1;
    return;
  }

  ctx->fdset_len_before_listen = w->fd_set.len;

  TyStr addr = make_str("127.0.0.1:30385");
  TyResult_Listener_i32 l;
  __ty_rt__Network__listen(task, ctx->net, &addr, &l);
  assert(l.tag == 0 && l.value != NULL);

  ctx->fdset_len_after_listen = w->fd_set.len;

  __ty_rt__Listener__close(task, l.value);

  ctx->fdset_len_after_close = w->fd_set.len;
  ctx->finished = 1;
}

static void test_fdset_tracks_real_socket_with_live_worker(void) {
  ty_net_init();
  ty_sched_init();

  FdSetCtx ctx;
  memset(&ctx, 0, sizeof(ctx));
  ctx.net = ty_net_global();
  assert(ctx.net != NULL);

  SlabArena* spawn_arena = slab_arena_new();
  assert(spawn_arena != NULL);

  ty_spawn(spawn_arena, fdset_coro, &ctx);
  ty_sched_run();

  assert(ctx.finished);

  if (ctx.worker_was_null) {
    fprintf(stderr,
        "[phase4] ty_sched_current_worker() returned NULL even from "
        "inside a spawned coroutine after ty_sched_init() — the "
        "assumption this test is built on (spawning from main() gives "
        "coroutines a real worker context) doesn't hold. This test "
        "cannot say anything about TyFdSet's real-worker behavior as "
        "written; the spawn/worker sequencing needs to be understood "
        "from scheduler.c before this can be fixed rather than guessed "
        "at again.\n");
    assert(0 && "worker was NULL — see message above");
  }

  assert(ctx.fdset_len_after_listen == ctx.fdset_len_before_listen + 1 &&
      "listening should add exactly one fd to the live worker's TyFdSet — "
      "this is the actual thing test_phase4_net_fdset.c couldn't check");
  assert(ctx.fdset_len_after_close == ctx.fdset_len_before_listen &&
      "closing the listener should remove it from the worker's TyFdSet, "
      "back to the starting count");

  ty_sched_shutdown();
  ty_net_shutdown();
  printf("[phase4] TyFdSet tracks a real socket with a live worker present — PASS\n");
}

int main(void) {
  test_fdset_tracks_real_socket_with_live_worker();
  printf("[phase4] fdset live-worker test PASSED\n");
  return 0;
}
