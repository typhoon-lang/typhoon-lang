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

#include <stdint.h>
#include <stddef.h>

#ifdef __cplusplus
extern "C" {
#endif

/* ── opaque handle types ────────────────────────────────────────────────── */

typedef struct TyCoro  TyCoro;    /* coroutine (green thread)                */
typedef struct TyChan  TyChan;    /* typed channel (send / recv)             */
struct SlabArena;                 /* task-local memory arena                 */

/* ── scheduler lifecycle ─────────────────────────────────────────────────── */

/* Call once from main() before any spawn.
 * Launches (nproc - 1) background worker threads. */
void ty_sched_init(void);

/* Call at end of main() to drain all coroutines and shut down workers. */
void ty_sched_shutdown(void);

/* ── coroutine API ───────────────────────────────────────────────────────── */

/* Spawn a new coroutine running fn(task, arg). Returns immediately.
 * The new coroutine is pushed onto the current worker's deque. */
TyCoro* ty_spawn(struct SlabArena* arena, void (*fn)(void* task, void* arg), void* arg);

/* Yield the current coroutine voluntarily.
 * The scheduler may resume another runnable coroutine. */
void ty_yield(void);

/* Suspend current coroutine until `coro` has finished.
 * Behaves like cooperative await — the caller is re-queued when `coro` exits. */
void ty_await(struct SlabArena* arena, TyCoro* coro);

/* Exit the current coroutine. Called automatically at function return. */
void ty_coro_exit(void);

/* Return the SlabArena owned by the currently running coroutine.
 * Used by emitted IR to get the task pointer without a parameter. */
struct SlabArena* ty_current_arena(void);

/* ── channel API ─────────────────────────────────────────────────────────── */

/* Create a channel with capacity `cap` slots of `elem_size` bytes each.
 * cap == 0 → synchronous (rendezvous) channel. */
TyChan* ty_chan_new(size_t elem_size, size_t cap);

/* Send `elem` (pointer to elem_size bytes) into chan.
 * Blocks (cooperative) if the channel is full. */
void ty_chan_send(struct SlabArena* arena, TyChan* chan, void* elem);

/* Receive into `out` (pointer to elem_size bytes) from chan.
 * Blocks (cooperative) if the channel is empty. */
void ty_chan_recv(struct SlabArena* arena, TyChan* chan, void* out);

/* Close a channel; receivers drain remaining items then get zeroed values. */
void ty_chan_close(TyChan* chan);

#ifdef __cplusplus
}
#endif
#endif /* TY_SCHEDULER_H */
