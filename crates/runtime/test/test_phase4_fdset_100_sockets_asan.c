// test_phase4_fdset_100_sockets_asan.c
//
// Closes checklist item (Task 4.5):
//   "Test: 100 sockets opened and closed; ASAN confirms no leaked FDs or memory"
//
// STATUS BEFORE THIS FILE: still open, and for a specific reason the doc's own
// review already worked out — neither existing fdset test file satisfies it:
//   - test_phase4_fdset.c only ever uses fake integer fds (10, 20, 30, 1000+i,
//     TY_FD_INVALID); no real socket is ever opened.
//   - test_phase4_net_fdset.c opens real sockets but only reaches 10 sequential
//     listen/close cycles (not 100), and runs no ASAN itself — ASAN-ness would
//     depend entirely on how the binary happens to be invoked externally.
//   - test_phase4_net_fdset.c's own header comment says it runs with no
//     scheduler/worker threads, so ty_sched_current_worker() is NULL and every
//     ty_fdset_add/remove call is skipped via the NULL-worker guard — meaning
//     even its 10-cycle coverage never touched real TyFdSet integration.
//
// This file is written to close the specific gaps above: 100 cycles (not 10),
// and it deliberately runs inside a spawned coroutine after ty_sched_init() so
// ty_sched_current_worker() has a real chance of being non-NULL and the
// TyFdSet add/remove calls in ty_net.c actually execute, rather than being
// silently skipped like in test_phase4_net_fdset.c. FD-count and (to the extent
// visible from this process) heap-growth checks are done in-test as a cheap
// sanity signal; ASAN/LSan itself is expected to be the authority and must be
// enabled at compile time (-fsanitize=address) when this binary is built — this
// file does not and cannot enable ASAN on itself after the fact.
//
// ASSUMPTIONS CARRIED:
//   - ty_sched_current_worker() and Worker.fd_set are accessible in a spawned
//     coroutine the way test_phase4_fdset_live_worker.c also assumes (same
//     scheduler-sequencing uncertainty flagged there — scheduler.c not reviewed)
//   - "100 sockets" read as 100 sequential listen-then-close cycles on one
//     socket at a time, matching the shape of test_phase4_net_fdset.c's
//     existing (10-cycle) sub-test rather than 100 concurrently-open sockets;
//     if the intended reading was 100 concurrently open, that's a second test
//     worth writing separately since it exercises a different code path
//     (fd_set growth under concurrent adds, not just steady-state add/remove)
//
// Needs a real compile-and-run pass, specifically built with ASAN, before the
// "confirms no leaked FDs or memory" half of this checklist item can be
// checked off — a clean run of this file without ASAN enabled only confirms
// functional correctness, not the absence of leaks.
//
// FIXED (this pass), now that scheduler.c is available:
//   1. ty_sched_run_until_idle() never existed anywhere in scheduler.c —
//      that's the compile error being fixed now, not just a commented-out
//      call. The real "run until idle" function is ty_sched_run(void): it
//      loops while active_coros > 0 and returns once every spawned
//      coroutine has finished. Added a ty_sched_shutdown() call after
//      checking results, for proper teardown (joins worker threads, closes
//      IO backends) — matches the ordering seen in other tests' main.log
//      traces (PASS message first, then the shutdown sequence).
//   2. The coroutine's first parameter is not a caller-controlled "task"
//      flag — coro_new() always allocates its own fresh SlabArena
//      (co->arena) and the trampoline calls co->fn(co->arena, co->arg), so
//      that's what actually arrives as the first argument. This file was
//      both fabricating a fake non-NULL sentinel to pass into ty_spawn
//      (which ignores its first argument entirely — `(void)arena;` in
//      ty_spawn itself) AND separately calling slab_arena_new() a second
//      time inside the coroutine, leaking the real one and using a
//      disconnected arena for no reason. Fixed by renaming the parameter
//      to `arena` and using it directly.
//   3. `net` (from ty_net_global()) is already TyNetwork* — the listen call
//      was passing &net (TyNetwork**) where TyNetwork* is expected.

#include <stdio.h>
#include <string.h>

#include "scheduler.h"
#include "ty_net.h"
#include "ty_mem.h"
#include "ty_io.h"

#define N_CYCLES 100
#define TY_RESULT_OK 0

static TyStr make_str(SlabArena* a, const char* s) {
    size_t len = strlen(s);
    char* buf = slab_alloc_sized(a, len);
    memcpy(buf, s, len);
    return (TyStr){ .ptr = buf, .len = (int32_t)len };
}

typedef struct {
    int cycles_ok;
    long fd_set_len_before;
    long fd_set_len_after;
} CycleResult;

static CycleResult g_result;

// First param is the coroutine's own real arena (co->arena) — not a
// caller-supplied flag. Second param is whatever was passed as `arg` to
// ty_spawn.
static void fdset_cycle_coro(void* arena, void* arg) {
    (void)arg;
    TyNetwork* net = ty_net_global();

    Worker* w = ty_sched_current_worker();
    if (w == NULL) {
        fprintf(stderr, "coro: ty_sched_current_worker() returned NULL — "
                        "same gap as test_phase4_net_fdset.c, fd_set "
                        "add/remove will be skipped, this run cannot confirm "
                        "real TyFdSet integration\n");
    } else {
        g_result.fd_set_len_before = w->fd_set.len;
    }

    int ok = 1;
    for (int i = 0; i < N_CYCLES; i++) {
        char addr[32];
        // port 0 lets the OS assign an ephemeral port each cycle, avoiding
        // TIME_WAIT collisions across 100 rapid listen/close cycles on one
        // fixed port.
        snprintf(addr, sizeof(addr), "127.0.0.1:0");
        TyStr addr_str = make_str(arena, addr);

        TyResult_Listener_i32 listen_res;
        __ty_rt__Network__listen(arena, net, &addr_str, &listen_res);
        if (listen_res.tag != TY_RESULT_OK) {
            fprintf(stderr, "cycle %d: listen failed\n", i);
            ok = 0;
            continue;
        }
        TyListener* l = listen_res.value;

        // Confirm the fd was actually tracked mid-cycle, if a worker is present.
        if (w != NULL && w->fd_set.len <= g_result.fd_set_len_before) {
            fprintf(stderr, "cycle %d: fd_set.len did not increase after "
                            "listen — TyFdSet integration not exercised\n", i);
            ok = 0;
        }

        __ty_rt__Listener__close(arena, l);

        if (w != NULL && w->fd_set.len > g_result.fd_set_len_before) {
            fprintf(stderr, "cycle %d: fd_set.len did not return to baseline "
                            "after close — possible fd leak in TyFdSet "
                            "bookkeeping\n", i);
            ok = 0;
        }
    }

    if (w != NULL) g_result.fd_set_len_after = w->fd_set.len;
    g_result.cycles_ok = ok;
}

int main(void) {
    memset(&g_result, 0, sizeof(g_result));
    ty_net_init();
    ty_sched_init();

    ty_spawn(NULL, fdset_cycle_coro, NULL); // first arg is ignored by ty_spawn
    ty_sched_run();     // blocks until active_coros == 0

    int ok = g_result.cycles_ok;

    if (g_result.fd_set_len_after != g_result.fd_set_len_before) {
        fprintf(stderr, "FAIL: fd_set.len drifted from %ld to %ld over %d "
                        "cycles — net fd bookkeeping leak\n",
                g_result.fd_set_len_before, g_result.fd_set_len_after, N_CYCLES);
        ok = 0;
    }

    if (ok) {
        printf("PASS: %d sequential listen/close cycles completed, fd_set.len "
               "returned to baseline (%ld) each time\n",
               N_CYCLES, g_result.fd_set_len_before);
        printf("NOTE: this binary must be compiled with -fsanitize=address "
               "for the ASAN/leak-detection half of this checklist item to "
               "actually be checked — this process exiting 0 alone does not "
               "confirm that.\n");
    }

    ty_sched_shutdown(); // joins worker threads, tears down IO backends
    ty_net_shutdown();
    return ok ? 0 : 1;
}
