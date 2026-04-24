#ifndef TY_SCHEDULER_H
#define TY_SCHEDULER_H

/*
 * scheduler.h — Typhoon M:N scheduler public API
 *
 * Exposed to the language runtime and to emitted LLVM IR via declares.
 *
 * Threading model
 * ───────────────
 *   - One OS thread per logical CPU core (worker threads).
 *   - Each worker runs a work-stealing loop over its own deque and
 *     randomly steals from peers when idle.
 *   - Each coroutine owns one SlabArena (task-local); cross-thread
 *     allocation never occurs.
 *   - Cooperative yield at I/O / channel blocks.
 *   - Preemptive yield via SIGPROF delivered to all workers.
 */

#include "platform.h"
#include "atomic.h"
#include "ty_mem.h"
#include <stdio.h>
#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/* ── opaque handle types ────────────────────────────────────────────────── */

/* ══════════════════════════════════════════════════════════════════════════
 *  Coroutine
 * ══════════════════════════════════════════════════════════════════════════ */

typedef enum {
    CORO_RUNNABLE = 0,
    CORO_RUNNING,
    CORO_BLOCKED,
    CORO_DONE
} CoroState;

typedef struct TyCoro {
    TyCtx ctx;
    void (*fn)(void*, void*);
    void* arg;
    char* stack_base;
    size_t stack_total;
    SlabArena* arena;
    _Atomic(CoroState) state;
    _Atomic(int) ref;
    _Atomic(struct TyCoro*) waiters;
    _Atomic(int) waiters_lock;
    struct TyCoro* waiter_next;
    struct TyCoro* sched_next;
    _Atomic(int64_t) io_result;
} TyCoro;

/* ══════════════════════════════════════════════════════════════════════════
 *  Chase-Lev Work-Stealing Deque
 * ══════════════════════════════════════════════════════════════════════════ */

typedef struct {
    _Atomic(size_t) cap;
    _Atomic(void**) buf;
} DequeArray;

typedef struct WSDeque {
    _Atomic(long) top;
    _Atomic(long) bottom;
    _Atomic(DequeArray*) array;
} WSDeque;

/* ══════════════════════════════════════════════════════════════════════════
 *  Worker
 * ══════════════════════════════════════════════════════════════════════════ */

typedef struct Worker {
    TyThread thread;
    int id;
    WSDeque deque;
    TyCoro* current;
    TyCtx sched_ctx; /* scheduler's saved context                */
    _Atomic(int) preempt_flag;
    _Atomic(int) running;
    _Atomic(int) last_phase; /* debug-only observability */
    _Atomic(int) in_coro; /* debug-only observability */
    _Atomic(long) local_deque_size_snapshot; /* debug-only observability */
} Worker;

/* ══════════════════════════════════════════════════════════════════════════
 *  Channel
 * ══════════════════════════════════════════════════════════════════════════ */

typedef struct WaitNode {
    TyCoro* coro;
    void* elem;
    struct WaitNode* next;
} WaitNode;

struct TyChan {
    TyMutex lock;
    size_t elem_size;
    size_t cap;
    size_t len;
    size_t head;
    size_t tail;
    char* buf;
    WaitNode* send_q;
    WaitNode* recv_q;
    int closed;
    /* Global registry so scheduler shutdown can close all channels and wake
     * parked coroutines (avoid shutdown drain hang on chan recv/send). */
    struct TyChan* all_next;
};

/* ── scheduler lifecycle ─────────────────────────────────────────────────── */

/* Call once from main() before any spawn.
 * Launches (nproc - 1) background worker threads. */
void ty_sched_init(void);

/* Drive the scheduler on worker 0 until all coroutines finish.
 * Does not initiate shutdown; intended for normal "run main to completion"
 * execution. Call ty_sched_shutdown() afterwards to stop workers. */
void ty_sched_run(void);

/* Call at end of main() to drain all coroutines and shut down workers. */
void ty_sched_shutdown(void);

/* ── coroutine API ───────────────────────────────────────────────────────── */

/* Spawn a new coroutine running fn(task, arg). Returns immediately.
 * The new coroutine is pushed onto the current worker's deque. */
TyCoro* ty_spawn(SlabArena* arena, void (*fn)(void* task, void* arg), void* arg);

/* Yield the current coroutine voluntarily.
 * The scheduler may resume another runnable coroutine. */
void ty_yield(void);

/* Suspend current coroutine until `coro` has finished.
 * Behaves like cooperative await — the caller is re-queued when `coro` exits. */
void ty_await(SlabArena* arena, TyCoro* coro);

/* Exit the current coroutine. Called automatically at function return. */
void ty_coro_exit(void);

// Block current coroutine; swap back to scheduler.
void ty_coro_block_and_yield(void);

// Thread-safe enqueue from the I/O poll thread.
void sched_enqueue_from_external(void* co);

// Expose current coro pointer as void* (ABI boundary).
void* ty_current_coro_raw(void);

/* Return the SlabArena owned by the currently running coroutine.
 * Used by emitted IR to get the task pointer without a parameter. */
SlabArena* ty_current_arena(void);

/* ── channel API ─────────────────────────────────────────────────────────── */

/* Create a channel with capacity `cap` slots of `elem_size` bytes each.
 * cap == 0 → synchronous (rendezvous) channel. */
struct TyChan* ty_chan_new(size_t elem_size, size_t cap);

/* Send `elem` (pointer to elem_size bytes) into chan.
 * Blocks (cooperative) if the channel is full. */
void ty_chan_send(SlabArena* arena, struct TyChan* chan, void* elem);

/* Receive into `out` (pointer to elem_size bytes) from chan.
 * Blocks (cooperative) if the channel is empty. */
void ty_chan_recv(SlabArena* arena, struct TyChan* chan, void* out);

/* Try receive into `out`.
 * Returns 1 if received, 0 if currently empty, -1 if closed and drained.
 * The compiler may choose to poll/yield on 0 when lowering Option<T>. */
int ty_chan_try_recv(SlabArena* arena, struct TyChan* chan, void* out);

/* Close a channel; receivers drain remaining items then get zeroed values. */
void ty_chan_close(struct TyChan* chan);

#ifdef __cplusplus
}
#endif
#endif /* TY_SCHEDULER_H */
