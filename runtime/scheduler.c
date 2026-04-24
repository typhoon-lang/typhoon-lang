/*
 * scheduler.c — Typhoon M:N coroutine scheduler
 *
 * All platform-specific calls are routed through platform.h.
 * This file is pure portable C11 + the TY_* macros.
 *
 * Architecture
 * ────────────
 *   Workers      One OS thread per logical CPU.  Each owns a WSDeque.
 *   WSDeque      Chase-Lev work-stealing deque (lock-free push/pop/steal).
 *   TyCoro       Stackful coroutine via TyCtx (ucontext on POSIX, Fiber on Win).
 *                Stack: [guard page | 64 KB data].
 *   SlabArena    Each coroutine owns one arena (runtime.c).
 *   Preemption   SIGPROF on POSIX; timer-queue callback on Windows.
 *   Channels     Bounded ring-buffer + cooperative wait queues.
 */

#include "scheduler.h"
#include "platform.h"
#include "atomic.h"
#include "ty_mem.h"
#include <string.h>
#include <stdio.h>
#include <errno.h>
#include <assert.h>

#ifndef _MSC_VER
#include <unistd.h>
#endif

#if defined(__has_feature)
#  if __has_feature(address_sanitizer)
#    define TY_ASAN 1
#  endif
#endif
#if defined(__SANITIZE_ADDRESS__)
#  define TY_ASAN 1
#endif

#ifdef TY_ASAN
#  include <sanitizer/asan_interface.h>
#endif

#if defined(TY_ASAN) && !defined(_MSC_VER)
#  define TY_NO_ASAN __attribute__((no_sanitize("address")))
#else
#  define TY_NO_ASAN
#endif

/* ── configuration ───────────────────────────────────────────────────────── */

#define CORO_STACK_SIZE   (128 * 1024) /* 128 KB coroutine stack data          */
#define GUARD_PAGE_SIZE   4096 /* one page, PROT_NONE / PAGE_NOACCESS */
#define DEQUE_INITIAL_CAP 256 /* must be power of 2                  */
#define MAX_WORKERS       64
#define SIGPROF_HZ        100 /* preemption ticks per second         */
#define STEAL_RETRIES     4 /* steal attempts before sleeping       */
#define EBR_RECLAIM_THRESHOLD 64
#define EBR_EXTERNAL_SLOT MAX_WORKERS
#define EBR_SLOT_COUNT (MAX_WORKERS + 1)

/* ── hard abort ──────────────────────────────────────────────────────────── */

static void sched_abort(const char* msg) {
    ty_stderr_write(msg);
    TY_TRAP();
}

typedef struct DequeRetiredNode {
    DequeArray* array;
    size_t cap;
    uint64_t retire_epoch;
    struct DequeRetiredNode* next;
} DequeRetiredNode;

static int ebr_worker_id(void);
static void ebr_enter_worker(int worker_id);
static void ebr_exit_worker(int worker_id);
static void ebr_retire_array(DequeArray* array, size_t cap);

/* ══════════════════════════════════════════════════════════════════════════
 *  Chase-Lev Work-Stealing Deque
 * ══════════════════════════════════════════════════════════════════════════ */

static DequeArray* deque_array_new(size_t cap) {
    size_t total = sizeof(DequeArray) + cap * sizeof(void*);
    DequeArray* da = (DequeArray*)ty_vm_alloc(total);
    if (!da) sched_abort("deque_array_new: alloc failed");
    atomic_init(&da->cap, cap);
    void** raw = (void**)((char*)da + sizeof(DequeArray));
    atomic_init(&da->buf, raw);
    return da;
}

static void deque_init(WSDeque* dq) {
    atomic_init(&dq->top, 0);
    atomic_init(&dq->bottom, 0);
    atomic_init(&dq->array, deque_array_new(DEQUE_INITIAL_CAP));
}

static void deque_grow(WSDeque* dq, DequeArray* old, long top, long bot) {
    size_t old_cap = atomic_load_explicit(&old->cap, memory_order_relaxed);
    size_t new_cap = old_cap * 2;
    DequeArray* fresh = deque_array_new(new_cap);
    void** ob = atomic_load_explicit(&old->buf, memory_order_relaxed);
    void** nb = atomic_load_explicit(&fresh->buf, memory_order_relaxed);
    for (long i = top; i < bot; i++)
        nb[i & (new_cap - 1)] = ob[i & (old_cap - 1)];
    atomic_store_explicit(&dq->array, fresh, memory_order_release);
    ebr_retire_array(old, old_cap);
}

static void deque_push(WSDeque* dq, void* item) {
    int ebr_id = ebr_worker_id();
    ebr_enter_worker(ebr_id);
    long bot = atomic_load_explicit(&dq->bottom, memory_order_relaxed);
    long top = atomic_load_explicit(&dq->top, memory_order_acquire);
    DequeArray* a = atomic_load_explicit(&dq->array, memory_order_relaxed);
    size_t cap = atomic_load_explicit(&a->cap, memory_order_relaxed);
    if ((size_t)(bot - top) >= cap - 1) {
        deque_grow(dq, a, top, bot);
        a = atomic_load_explicit(&dq->array, memory_order_relaxed);
        cap = atomic_load_explicit(&a->cap, memory_order_relaxed);
    }
    void** buf = atomic_load_explicit(&a->buf, memory_order_relaxed);
    buf[bot & (cap - 1)] = item;
    atomic_thread_fence(memory_order_release);
    /* seq_cst so stealers on other cores see the updated bottom immediately */
    atomic_store_explicit(&dq->bottom, bot + 1, memory_order_seq_cst);
    ebr_exit_worker(ebr_id);
}

static void* deque_pop(WSDeque* dq) {
    int ebr_id = ebr_worker_id();
    ebr_enter_worker(ebr_id);
    long bot = atomic_load_explicit(&dq->bottom, memory_order_relaxed) - 1;
    DequeArray* a = atomic_load_explicit(&dq->array, memory_order_acquire);
    atomic_store_explicit(&dq->bottom, bot, memory_order_relaxed);
    atomic_thread_fence(memory_order_seq_cst);
    long top = atomic_load_explicit(&dq->top, memory_order_relaxed);
    if (top > bot) {
        atomic_store_explicit(&dq->bottom, bot + 1, memory_order_relaxed);
        ebr_exit_worker(ebr_id);
        return NULL;
    }
    /* Use proper atomic loads — plain field access bypasses memory model */
    size_t cap = atomic_load_explicit(&a->cap, memory_order_relaxed);
    void** buf = atomic_load_explicit(&a->buf, memory_order_relaxed);
    void* item = buf[bot & (cap - 1)];
    if (top == bot) {
        long expected = top;
        if (!atomic_compare_exchange_strong_explicit(
                &dq->top, &expected, top + 1,
                memory_order_seq_cst, memory_order_relaxed))
            item = NULL;
        atomic_store_explicit(&dq->bottom, bot + 1, memory_order_relaxed);
    }
    ebr_exit_worker(ebr_id);
    return item;
}

static void* deque_steal(WSDeque* dq) {
    int ebr_id = ebr_worker_id();
    ebr_enter_worker(ebr_id);
    long top = atomic_load_explicit(&dq->top, memory_order_acquire);
    atomic_thread_fence(memory_order_seq_cst);
    long bot = atomic_load_explicit(&dq->bottom, memory_order_acquire);
    if (top >= bot) {
        ebr_exit_worker(ebr_id);
        return NULL;
    }
    DequeArray* a = atomic_load_explicit(&dq->array, memory_order_consume);
    size_t cap = atomic_load_explicit(&a->cap, memory_order_relaxed);
    void** buf = atomic_load_explicit(&a->buf, memory_order_relaxed);
    void* item = buf[top & (cap - 1)];
    long expected = top;
    if (!atomic_compare_exchange_strong_explicit(
            &dq->top, &expected, top + 1,
            memory_order_seq_cst, memory_order_relaxed)) {
        ebr_exit_worker(ebr_id);
        return NULL;
    }
    ebr_exit_worker(ebr_id);
    return item;
}

/* ── global state ────────────────────────────────────────────────────────── */

static Worker workers[MAX_WORKERS];
static int num_workers = 0;
static _Atomic(int) sched_shutdown_flag;
static _Atomic(int) active_coros;
static TyCtx worker_host_ctx[MAX_WORKERS];
static _Atomic(long) dbg_spawned;
static _Atomic(long) dbg_freed;
static _Atomic(long) dbg_blocked;
static _Atomic(long) dbg_woken;
static _Atomic(long) dbg_deque_retired;
static _Atomic(long) dbg_deque_reclaimed;
static _Atomic(long) dbg_ebr_epoch_advance;
static _Atomic(uint64_t) ebr_global_epoch;
static _Atomic(uint64_t) ebr_announced_epoch[EBR_SLOT_COUNT];
static _Atomic(int) ebr_active[EBR_SLOT_COUNT];
static DequeRetiredNode* ebr_retired[EBR_SLOT_COUNT];
static size_t ebr_retired_count[EBR_SLOT_COUNT];

/* Global channel registry. Shutdown closes all channels to wake parked coros.
 * Without this, shutdown drain loop can hang forever when only chan_park'd
 * coroutines remain. */
static TyMutex all_chans_lock;
static struct TyChan* all_chans_head;

#define CORO_STATE_COUNT 4
static const unsigned char k_coro_transition_allowed[CORO_STATE_COUNT][CORO_STATE_COUNT] = {
    /* from RUNNABLE */ { 1, 1, 0, 0 },
    /* from RUNNING  */ { 1, 0, 1, 1 },
    /* from BLOCKED  */ { 1, 0, 1, 0 },
    /* from DONE     */ { 0, 0, 0, 1 }
};
static _Atomic(int) coro_invalid_logged[CORO_STATE_COUNT][CORO_STATE_COUNT];

enum {
    WORKER_PHASE_INIT = 0,
    WORKER_PHASE_START = 1,
    WORKER_PHASE_LOOP = 2,
    WORKER_PHASE_SEEN_SHUTDOWN = 3,
    WORKER_PHASE_EXIT_LOOP = 4,
    WORKER_PHASE_RETURN_HOST = 5
};

static TY_THREAD_LOCAL Worker* tl_worker = NULL;

static Worker* current_worker(void) { return tl_worker; }
static TyCoro* current_coro(void) {
    Worker* w = current_worker();
    return w ? w->current : NULL;
}

static int coro_transition_valid(CoroState from, CoroState to) {
    if ((int)from < 0 || (int)from >= CORO_STATE_COUNT) return 0;
    if ((int)to < 0 || (int)to >= CORO_STATE_COUNT) return 0;
    return k_coro_transition_allowed[from][to] != 0;
}

static void coro_report_invalid_transition(
    TyCoro* co, CoroState from, CoroState to, const char* op, Worker* w) {
    int worker_id = w ? w->id : -1;

    TY_DEBUG(
        "[sched] invalid state transition op=%s worker=%d coro=%p from=%d to=%d\n",
        op, worker_id, (void*)co, (int)from, (int)to);
    assert(!"invalid coroutine state transition");
}

static void coro_state_store(TyCoro* co, CoroState to, const char* op) {
    Worker* w = current_worker();
    CoroState from = atomic_load_explicit(&co->state, memory_order_acquire);
    if (!coro_transition_valid(from, to)) {
        coro_report_invalid_transition(co, from, to, op, w);
        return;
    }
    atomic_store_explicit(&co->state, to, memory_order_release);
    TY_DEBUG("[state] op=%s worker=%d coro=%p %d->%d\n",
        op, w ? w->id : -1, (void*)co, (int)from, (int)to);
}

static int coro_state_cas(TyCoro* co, CoroState from, CoroState to, const char* op) {
    Worker* w = current_worker();
    if (!coro_transition_valid(from, to)) {
        coro_report_invalid_transition(co, from, to, op, w);
        return 0;
    }

    CoroState expected = from;
    if (atomic_compare_exchange_strong_explicit(
            &co->state, &expected, to,
            memory_order_acq_rel, memory_order_relaxed)) {
        TY_DEBUG("[state] op=%s worker=%d coro=%p %d->%d\n",
            op, w ? w->id : -1, (void*)co, (int)from, (int)to);
        return 1;
    }

    if (!coro_transition_valid(expected, to))
        coro_report_invalid_transition(co, expected, to, op, w);
    return 0;
}

static int ebr_worker_id(void) {
    Worker* w = current_worker();
    return w ? w->id : EBR_EXTERNAL_SLOT;
}

static void ebr_enter_worker(int worker_id) {
    if (worker_id < 0 || worker_id >= EBR_SLOT_COUNT) return;
    if (worker_id != EBR_EXTERNAL_SLOT && worker_id >= num_workers) return;
    uint64_t epoch = atomic_load_explicit(&ebr_global_epoch, memory_order_acquire);
    atomic_store_explicit(&ebr_announced_epoch[worker_id], epoch, memory_order_release);
    atomic_store_explicit(&ebr_active[worker_id], 1, memory_order_release);
}

static void ebr_exit_worker(int worker_id) {
    if (worker_id < 0 || worker_id >= EBR_SLOT_COUNT) return;
    if (worker_id != EBR_EXTERNAL_SLOT && worker_id >= num_workers) return;
    atomic_store_explicit(&ebr_active[worker_id], 0, memory_order_release);
}

static uint64_t ebr_min_active_epoch(void) {
    uint64_t min_epoch = atomic_load_explicit(&ebr_global_epoch, memory_order_acquire);
    for (int i = 0; i < num_workers; i++) {
        if (!atomic_load_explicit(&ebr_active[i], memory_order_acquire))
            continue;
        uint64_t announced = atomic_load_explicit(&ebr_announced_epoch[i], memory_order_acquire);
        if (announced < min_epoch) min_epoch = announced;
    }
    if (atomic_load_explicit(&ebr_active[EBR_EXTERNAL_SLOT], memory_order_acquire)) {
        uint64_t announced = atomic_load_explicit(
            &ebr_announced_epoch[EBR_EXTERNAL_SLOT], memory_order_acquire);
        if (announced < min_epoch) min_epoch = announced;
    }
    return min_epoch;
}

static void ebr_advance_epoch(void) {
    atomic_fetch_add_explicit(&ebr_global_epoch, 1, memory_order_acq_rel);
    atomic_fetch_add_explicit(&dbg_ebr_epoch_advance, 1, memory_order_relaxed);
}

static void ebr_try_reclaim_worker(int worker_id) {
    if (worker_id < 0 || worker_id >= EBR_SLOT_COUNT) return;
    if (worker_id != EBR_EXTERNAL_SLOT && worker_id >= num_workers) return;
    uint64_t safe_epoch = ebr_min_active_epoch();

    DequeRetiredNode* node = ebr_retired[worker_id];
    DequeRetiredNode* keep_head = NULL;
    DequeRetiredNode* keep_tail = NULL;
    size_t keep_count = 0;

    while (node) {
        DequeRetiredNode* next = node->next;
        if (node->retire_epoch < safe_epoch) {
            size_t bytes = sizeof(DequeArray) + node->cap * sizeof(void*);
            ty_vm_free(node->array, bytes);
            ty_vm_free(node, sizeof(*node));
            atomic_fetch_add_explicit(&dbg_deque_reclaimed, 1, memory_order_relaxed);
        } else {
            node->next = NULL;
            if (keep_tail) keep_tail->next = node;
            else keep_head = node;
            keep_tail = node;
            keep_count++;
        }
        node = next;
    }

    ebr_retired[worker_id] = keep_head;
    ebr_retired_count[worker_id] = keep_count;
}

static void ebr_retire_array(DequeArray* array, size_t cap) {
    int worker_id = ebr_worker_id();
    if (worker_id < 0 || worker_id >= EBR_SLOT_COUNT) return;

    DequeRetiredNode* node = (DequeRetiredNode*)ty_vm_alloc(sizeof(DequeRetiredNode));
    if (!node) sched_abort("ebr_retire_array: alloc failed");
    node->array = array;
    node->cap = cap;
    node->retire_epoch = atomic_load_explicit(&ebr_global_epoch, memory_order_acquire);
    node->next = ebr_retired[worker_id];
    ebr_retired[worker_id] = node;
    ebr_retired_count[worker_id]++;
    atomic_fetch_add_explicit(&dbg_deque_retired, 1, memory_order_relaxed);

    if (ebr_retired_count[worker_id] >= EBR_RECLAIM_THRESHOLD) {
        ebr_advance_epoch();
        ebr_try_reclaim_worker(worker_id);
    }
}

static void ebr_force_reclaim_all(void) {
    ebr_advance_epoch();
    ebr_advance_epoch();
    for (int i = 0; i < num_workers; i++)
        ebr_try_reclaim_worker(i);
    ebr_try_reclaim_worker(EBR_EXTERNAL_SLOT);
}

/* ── coroutine trampoline ────────────────────────────────────────────────── */

TY_NO_ASAN
static void coro_trampoline(uint32_t hi, uint32_t lo) {
    TY_DEBUG("[tramp] entered hi=%u lo=%u\n", hi, lo);
    uintptr_t ptr = ((uintptr_t)hi << 32) | (uintptr_t)lo;
    TY_DEBUG("[tramp] co=%p\n", (void*)ptr);
    TyCoro* co = (TyCoro*)ptr;
    TY_DEBUG("[tramp] fn=%p arena=%p arg=%p\n", (void*)co->fn, (void*)co->arena, co->arg);
    co->fn(co->arena, co->arg);
    TY_DEBUG("[tramp] fn returned, calling ty_coro_exit\n");
    ty_coro_exit();
}

/* ── coroutine lifecycle ─────────────────────────────────────────────────── */

static TyCoro* coro_new(void (*fn)(void*, void*), void* arg) {
    size_t coro_size = (sizeof(TyCoro) + GUARD_PAGE_SIZE - 1) & ~(size_t)(GUARD_PAGE_SIZE - 1);
    size_t total = coro_size + GUARD_PAGE_SIZE + CORO_STACK_SIZE;
    char* base = (char*)ty_vm_alloc(total);
    if (!base) sched_abort("coro_new: alloc failed");

    TY_DEBUG("[coro_new] total=%zu base=%p mprotect_addr=%p mprotect_size=%zu\n",
        total, (void*)base, (void*)(base + coro_size), (size_t)GUARD_PAGE_SIZE);
    TY_DEBUG("[coro_new] sizeof(TyCoro)=%zu coro_size=%zu page=%d\n",
        sizeof(TyCoro), coro_size, GUARD_PAGE_SIZE);

    /* Layout: [TyCoro (padded to page) | guard page | stack data] */
    TyCoro* co = (TyCoro*)base;


    TY_DEBUG("[coro_new] co=%p stack_base=%p stack_top=%p guard_end=%p\n",
        (void*)co,
        (void*)base,
        (void*)(base + GUARD_PAGE_SIZE + CORO_STACK_SIZE),
        (void*)(base + GUARD_PAGE_SIZE));

    if (!ty_vm_guard(base + coro_size, GUARD_PAGE_SIZE)) {
        TY_DEBUG("[coro_new] mprotect failed: errno=%d (%s) addr=%p size=%zu\n",
            errno, strerror(errno), (void*)(base + coro_size), (size_t)GUARD_PAGE_SIZE);
        // Guard pages unavailable in this environment (sandboxed kernel).
        // Stack overflows won't be caught but execution is otherwise correct.
        TY_DEBUG("[coro_new] warning: guard page unavailable (mprotect EINVAL)\n");
    }

    co->fn  = fn;
    co->arg = arg;
    atomic_init((_Atomic(struct TyCoro*)*)&co->waiters, NULL);
    atomic_init(&co->waiters_lock, 0);
    co->waiter_next  = NULL;
    co->sched_next   = NULL;
    co->arena        = slab_arena_new();
    atomic_init(&co->state, CORO_RUNNABLE);
    atomic_init(&co->ref, 1);

    co->stack_base  = base;   /* free the whole slab in coro_free */
    co->stack_total = total;

    char* stack_bottom = base + coro_size + GUARD_PAGE_SIZE;
    uintptr_t ptr = (uintptr_t)co;
    ty_ctx_init(&co->ctx,
        stack_bottom, CORO_STACK_SIZE,
        coro_trampoline,
        (uint32_t)(ptr >> 32),
        (uint32_t)(ptr & 0xFFFFFFFFu));

    return co;
}

static void coro_free(TyCoro* co) {
    if (!co) return;
    /* Decrement here, not in ty_coro_exit — a coroutine blocked on a channel
     * is still alive (CORO_BLOCKED).  Decrementing at exit caused the shutdown
     * loop to see active_coros==0 while receivers were still parked. */
    int after = atomic_fetch_sub_explicit(&active_coros, 1, memory_order_release) - 1;
    TY_DEBUG("[sched] coro_free coro=%p active=%d\n", (void*)co, after);
    atomic_fetch_add_explicit(&dbg_freed, 1, memory_order_relaxed);
    TY_DEBUG("[sched] coro_free:free_arena coro=%p arena=%p\n", (void*)co, (void*)co->arena);
#if defined(_WIN32)
    /* TODO: Investigate why slab_arena_free can hang on Windows during shutdown
       drain. For now, avoid blocking shutdown; leak is bounded to remaining
       live coroutines at shutdown. */
    if (!atomic_load_explicit(&sched_shutdown_flag, memory_order_acquire)) {
        slab_arena_free(co->arena);
    } else {
        TY_DEBUG("[sched] coro_free:skip_arena_free_windows_shutdown coro=%p\n", (void*)co);
    }
#else
    slab_arena_free(co->arena);
#endif
    TY_DEBUG("[sched] coro_free:free_stack coro=%p base=%p size=%zu\n",
        (void*)co, (void*)co->stack_base, (size_t)co->stack_total);
    ty_vm_free(co->stack_base, co->stack_total);
    TY_DEBUG("[sched] coro_free:free_header coro=%p\n", (void*)co);
    ty_vm_free(co, sizeof(TyCoro));
}

/* ── scheduling helpers ──────────────────────────────────────────────────── */

/* Push a newly-spawned coroutine onto the current worker's deque. */
static void sched_enqueue(TyCoro* co) {
    coro_state_store(co, CORO_RUNNABLE, "spawn_enqueue");
    Worker* w = current_worker();
    if (atomic_load_explicit(&sched_shutdown_flag, memory_order_acquire)) {
        TY_DEBUG("[sched] enqueue(spawn/shutdown) coro=%p onto worker 0\n", (void*)co);
        deque_push(&workers[0].deque, co);
        return;
    }
    TY_DEBUG("[sched] enqueue(spawn) coro=%p onto worker %d\n",
        (void*)co, w ? w->id : 0);
    deque_push(w ? &w->deque : &workers[0].deque, co);
}

/* Wake a BLOCKED coroutine — always onto worker 0 so ty_sched_shutdown's
 * drive loop (which only processes worker 0) is guaranteed to see it,
 * regardless of which worker thread is executing the wakeup. */
static void sched_enqueue_wake(TyCoro* co) {
    coro_state_store(co, CORO_RUNNABLE, "wake_enqueue");
    // /* Only wake truly BLOCKED coroutines; avoid enqueueing RUNNING/DONE. */
    // if (!coro_state_cas(co, CORO_BLOCKED, CORO_RUNNABLE, "wake_enqueue"))
    //     return;
    atomic_fetch_add_explicit(&dbg_woken, 1, memory_order_relaxed);
    TY_DEBUG("[sched] enqueue(wake) coro=%p onto worker 0\n", (void*)co);
    deque_push(&workers[0].deque, co);
}

static TyCoro* sched_next_coro(Worker* w) {
    TyCoro* co = (TyCoro*)deque_pop(&w->deque);
    if (co) return co;
    for (int i = 0; i < STEAL_RETRIES; i++) {
        int v = (int)(ty_rand() % (uint32_t)num_workers);
        if (v == w->id) continue;
        co = (TyCoro*)deque_steal(&workers[v].deque);
        if (co) return co;
    }
    return NULL;
}

static long deque_size_snapshot(WSDeque* dq) {
    long top = atomic_load_explicit(&dq->top, memory_order_acquire);
    long bottom = atomic_load_explicit(&dq->bottom, memory_order_acquire);
    long size = bottom - top;
    return size > 0 ? size : 0;
}

/* ── preemption ──────────────────────────────────────────────────────────── */

static void preempt_tick(void) {
    Worker* w = current_worker();
    if (w) atomic_store_explicit(&w->preempt_flag, 1, memory_order_relaxed);
}

/* ── worker run / loop ───────────────────────────────────────────────────── */

static char* coro_stack_bottom(TyCoro* co) {
    size_t coro_size = (sizeof(TyCoro) + GUARD_PAGE_SIZE - 1) & ~(size_t)(GUARD_PAGE_SIZE - 1);
    return co->stack_base + coro_size + GUARD_PAGE_SIZE;
}

TY_NO_ASAN
static void worker_resume_coro(Worker* w, TyCoro* co) {
    w->current = co;
    atomic_store_explicit(&w->in_coro, 1, memory_order_release);
    coro_state_store(co, CORO_RUNNING, "resume");

#ifdef TY_ASAN
    __sanitizer_start_switch_fiber(NULL, coro_stack_bottom(co), CORO_STACK_SIZE);
#endif

    ty_ctx_swap(&w->sched_ctx, &co->ctx);

#ifdef TY_ASAN
    __sanitizer_finish_switch_fiber(NULL, NULL, NULL);
#endif

    w->current = NULL;
    atomic_store_explicit(&w->in_coro, 0, memory_order_release);
    if (co->fn == NULL) {
        coro_free(co);
        return;
    }

    if (atomic_load_explicit(&co->state, memory_order_acquire) == CORO_DONE) {
        TY_DEBUG("[sched] BUG: coro=%p reached CORO_DONE without fn=NULL sentinel\n", (void*)co);
#ifdef TY_DEBUG_ENABLED
        assert(!"coroutine terminal path missing fn=NULL sentinel");
#endif
    }
}

/* Each worker's scheduler loop runs on its own scheduler stack. */
static void worker_sched_loop(uint32_t hi, uint32_t lo) {
    uintptr_t ptr = ((uintptr_t)hi << 32) | (uintptr_t)lo;
    Worker* w = (Worker*)ptr;
    int seen_shutdown = 0;
    atomic_store_explicit(&w->last_phase, WORKER_PHASE_LOOP, memory_order_release);
    TY_DEBUG("[sched] worker:loop_start id=%d\n", w->id);

    for (;;) {
        atomic_store_explicit(&w->preempt_flag, 0, memory_order_relaxed);
        atomic_store_explicit(
            &w->local_deque_size_snapshot,
            deque_size_snapshot(&w->deque),
            memory_order_release);

        TyCoro* co = (TyCoro*)deque_pop(&w->deque);
        if (co) {
            worker_resume_coro(w, co);
            continue;
        }

        if (atomic_load_explicit(&sched_shutdown_flag, memory_order_acquire)) {
            if (!seen_shutdown) {
                seen_shutdown = 1;
                atomic_store_explicit(&w->last_phase, WORKER_PHASE_SEEN_SHUTDOWN, memory_order_release);
                TY_DEBUG("[sched] worker:seen_shutdown id=%d local=%ld in_coro=%d\n",
                    w->id,
                    atomic_load_explicit(&w->local_deque_size_snapshot, memory_order_relaxed),
                    atomic_load_explicit(&w->in_coro, memory_order_relaxed));
            }
            break;
        }

        co = sched_next_coro(w);
        if (!co) {
            ty_sleep_ns(1000000);
            continue;
        }
        worker_resume_coro(w, co);
    }

    atomic_store_explicit(&w->last_phase, WORKER_PHASE_EXIT_LOOP, memory_order_release);
    TY_DEBUG("[sched] worker:exit_loop id=%d\n", w->id);
    atomic_store_explicit(&w->running, 0, memory_order_release);
    ty_ctx_swap(&w->sched_ctx, &worker_host_ctx[w->id]);
}

static void* worker_thread(void* arg) {
    Worker* w = (Worker*)arg;
    tl_worker = w;
    atomic_store_explicit(&w->last_phase, WORKER_PHASE_START, memory_order_release);
    TY_DEBUG("[sched] worker:start id=%d\n", w->id);

    size_t sched_stack_size = 64 * 1024;
    char* sched_stack = (char*)ty_vm_alloc(sched_stack_size);
    if (!sched_stack) sched_abort("worker_thread: sched stack alloc failed");
    uintptr_t wptr = (uintptr_t)w;
    ty_ctx_init(&w->sched_ctx, sched_stack, sched_stack_size,
        worker_sched_loop,
        (uint32_t)(wptr >> 32), (uint32_t)(wptr & 0xFFFFFFFFu));

#ifdef TY_WINDOWS
    ty_ctx_init_sched(&worker_host_ctx[w->id]);
#endif

    ty_ctx_swap(&worker_host_ctx[w->id], &w->sched_ctx);
    atomic_store_explicit(&w->last_phase, WORKER_PHASE_RETURN_HOST, memory_order_release);
    TY_DEBUG("[sched] worker:return_host id=%d\n", w->id);
    ty_vm_free(sched_stack, sched_stack_size);
    return NULL;
}

static void shutdown_sched_loop(uint32_t hi, uint32_t lo) {
    uintptr_t ptr = ((uintptr_t)hi << 32) | (uintptr_t)lo;
    Worker* w = (Worker*)ptr;
    TY_DEBUG("[sched] shutdown:drain_loop_start worker=%d active=%d\n", w->id,
        atomic_load_explicit(&active_coros, memory_order_relaxed));

    while (atomic_load_explicit(&active_coros, memory_order_acquire) > 0) {
        atomic_store_explicit(
            &w->local_deque_size_snapshot,
            deque_size_snapshot(&w->deque),
            memory_order_release);
        TyCoro* co = (TyCoro*)deque_pop(&w->deque);
        if (!co) {
            for (int i = 1; i < num_workers; i++) {
                co = (TyCoro*)deque_steal(&workers[i].deque);
                if (co) break;
            }
        }
        if (!co) {
            static uint64_t empty_spins = 0;
            empty_spins++;
            if ((empty_spins & ((1u << 14) - 1)) == 0) {
                TY_DEBUG("[sched] shutdown:drain_empty spin=%llu active=%d w0_deque=%ld\n",
                    (unsigned long long)empty_spins,
                    atomic_load_explicit(&active_coros, memory_order_relaxed),
                    (long)atomic_load_explicit(&w->local_deque_size_snapshot, memory_order_relaxed));
            }
            ty_sleep_ns(10000);
            continue;
        }

        TY_DEBUG("[sched] shutdown loop: active_coros=%d\n",
            atomic_load_explicit(&active_coros, memory_order_relaxed));
        TY_DEBUG("[sched] shutdown:drain_pick coro=%p state=%d fn=%p\n",
            (void*)co,
            (int)atomic_load_explicit(&co->state, memory_order_relaxed),
            (void*)co->fn);
        worker_resume_coro(w, co);
        TY_DEBUG("[sched] shutdown:after_resume active=%d\n",
            atomic_load_explicit(&active_coros, memory_order_relaxed));
    }

    TY_DEBUG("[sched] shutdown:drain_loop_done worker=%d\n", w->id);
    ebr_force_reclaim_all();
    ty_ctx_swap(&w->sched_ctx, &worker_host_ctx[w->id]);
}

static void run_sched_loop(uint32_t hi, uint32_t lo) {
    uintptr_t ptr = ((uintptr_t)hi << 32) | (uintptr_t)lo;
    Worker* w = (Worker*)ptr;

    while (atomic_load_explicit(&active_coros, memory_order_acquire) > 0) {
        atomic_store_explicit(
            &w->local_deque_size_snapshot,
            deque_size_snapshot(&w->deque),
            memory_order_release);

        TyCoro* co = (TyCoro*)deque_pop(&w->deque);
        if (!co) {
            for (int i = 1; i < num_workers; i++) {
                co = (TyCoro*)deque_steal(&workers[i].deque);
                if (co) break;
            }
        }
        if (!co) {
            ty_sleep_ns(10000);
            continue;
        }

        worker_resume_coro(w, co);
    }

    ty_ctx_swap(&w->sched_ctx, &worker_host_ctx[w->id]);
}

void ty_sched_run(void) {
    Worker* self = &workers[0];

    size_t sched_stack_size = 64 * 1024;
    char* sched_stack = (char*)ty_vm_alloc(sched_stack_size);
    if (!sched_stack) sched_abort("ty_sched_run: sched stack alloc failed");
    uintptr_t wptr = (uintptr_t)self;
    ty_ctx_init(&self->sched_ctx, sched_stack, sched_stack_size,
        run_sched_loop,
        (uint32_t)(wptr >> 32), (uint32_t)(wptr & 0xFFFFFFFFu));
    ty_ctx_swap(&worker_host_ctx[self->id], &self->sched_ctx);
    ty_vm_free(sched_stack, sched_stack_size);
}

static void close_all_channels_for_shutdown(void) {
    ty_mutex_lock(&all_chans_lock);
    for (struct TyChan* ch = all_chans_head; ch; ch = ch->all_next)
        ty_chan_close(ch);
    ty_mutex_unlock(&all_chans_lock);
}

void ty_sched_shutdown(void) {
    Worker* self = &workers[0];
    TY_DEBUG("[sched] shutdown:start active=%d\n",
        atomic_load_explicit(&active_coros, memory_order_relaxed));
    atomic_store_explicit(&sched_shutdown_flag, 1, memory_order_release);
    close_all_channels_for_shutdown();
    TY_DEBUG("[sched] shutdown:enter_drain active=%d\n",
        atomic_load_explicit(&active_coros, memory_order_relaxed));

    size_t sched_stack_size = 64 * 1024;
    char* sched_stack = (char*)ty_vm_alloc(sched_stack_size);
    if (!sched_stack) sched_abort("ty_sched_shutdown: sched stack alloc failed");
    uintptr_t wptr = (uintptr_t)self;
    ty_ctx_init(&self->sched_ctx, sched_stack, sched_stack_size,
        shutdown_sched_loop,
        (uint32_t)(wptr >> 32), (uint32_t)(wptr & 0xFFFFFFFFu));
    ty_ctx_swap(&worker_host_ctx[self->id], &self->sched_ctx);

    TY_DEBUG("[sched] shutdown:join_workers\n");
    for (int i = 1; i < num_workers; i++)
        ty_thread_join(workers[i].thread);

    ty_vm_free(sched_stack, sched_stack_size);
    ebr_force_reclaim_all();
    ty_preempt_stop();
    TY_DEBUG("[sched] shutdown:done active=%d spawned=%ld freed=%ld blocked=%ld woken=%ld deque_retired=%ld deque_reclaimed=%ld ebr_epoch_advance=%ld\n",
        atomic_load_explicit(&active_coros, memory_order_relaxed),
        atomic_load_explicit(&dbg_spawned, memory_order_relaxed),
        atomic_load_explicit(&dbg_freed, memory_order_relaxed),
        atomic_load_explicit(&dbg_blocked, memory_order_relaxed),
        atomic_load_explicit(&dbg_woken, memory_order_relaxed),
        atomic_load_explicit(&dbg_deque_retired, memory_order_relaxed),
        atomic_load_explicit(&dbg_deque_reclaimed, memory_order_relaxed),
        atomic_load_explicit(&dbg_ebr_epoch_advance, memory_order_relaxed));
}

/* ══════════════════════════════════════════════════════════════════════════
 *  Public API
 * ══════════════════════════════════════════════════════════════════════════ */

void ty_sched_init(void) {
    atomic_init(&sched_shutdown_flag, 0);
    atomic_init(&active_coros, 0);
    atomic_init(&dbg_spawned, 0);
    atomic_init(&dbg_freed, 0);
    atomic_init(&dbg_blocked, 0);
    atomic_init(&dbg_woken, 0);
    atomic_init(&dbg_deque_retired, 0);
    atomic_init(&dbg_deque_reclaimed, 0);
    atomic_init(&dbg_ebr_epoch_advance, 0);
    atomic_init(&ebr_global_epoch, 1);
    ty_mutex_init(&all_chans_lock);
    all_chans_head = NULL;

    num_workers = ty_cpu_count();
    if (num_workers > MAX_WORKERS) num_workers = MAX_WORKERS;
    if (num_workers < 1) num_workers = 1;

    for (int i = 0; i < EBR_SLOT_COUNT; i++) {
        atomic_init(&ebr_announced_epoch[i], 0);
        atomic_init(&ebr_active[i], 0);
        ebr_retired[i] = NULL;
        ebr_retired_count[i] = 0;
    }

    ty_preempt_install(preempt_tick, SIGPROF_HZ);

    // main thread
    workers[0].id = 0;
    deque_init(&workers[0].deque);
    atomic_init(&workers[0].preempt_flag, 0);
    atomic_init(&workers[0].running, 1);
    atomic_init(&workers[0].last_phase, WORKER_PHASE_INIT);
    atomic_init(&workers[0].in_coro, 0);
    atomic_init(&workers[0].local_deque_size_snapshot, 0);
    workers[0].current = NULL;
    TY_DEBUG("[sched] worker:start id=0\n");

    for (int i = 1; i < num_workers; i++) {
        workers[i].id = i;
        deque_init(&workers[i].deque);
        atomic_init(&workers[i].preempt_flag, 0);
        atomic_init(&workers[i].running, 1);
        atomic_init(&workers[i].last_phase, WORKER_PHASE_INIT);
        atomic_init(&workers[i].in_coro, 0);
        atomic_init(&workers[i].local_deque_size_snapshot, 0);
        workers[i].current = NULL;
        if (!ty_thread_create(&workers[i].thread, worker_thread, &workers[i]))
            sched_abort("ty_sched_init: thread_create failed");
    }

    tl_worker = &workers[0];
#ifdef TY_WINDOWS
    ty_ctx_init_sched(&worker_host_ctx[0]);
#endif
}

TyCoro* ty_spawn(SlabArena* arena, void (*fn)(void*, void*), void* arg) {
    (void)arena;
    TyCoro* co = coro_new(fn, arg);
    atomic_fetch_add_explicit(&dbg_spawned, 1, memory_order_relaxed);
    atomic_fetch_add_explicit(&active_coros, 1, memory_order_relaxed);
    TY_DEBUG("[sched] spawn coro=%p active=%d\n", (void*)co,
        atomic_load_explicit(&active_coros, memory_order_relaxed));
    sched_enqueue(co);
    return co;
}

void ty_yield(void) {
    Worker* w = current_worker();
    if (!w || !w->current) {
        /* If called from the main thread, help out and be cooperative.
         * We sleep a bit to not hog the CPU so real workers can run. */
        ty_sleep_ns(1000000); /* 1 ms */
        return;
    }
    TyCoro* co = w->current;
    if (coro_state_cas(co, CORO_RUNNING, CORO_RUNNABLE, "yield"))
        deque_push(&w->deque, co);
    ty_ctx_swap(&co->ctx, &w->sched_ctx);
}

static void coro_lock(TyCoro* co) {
    int zero;
    do {
        zero = 0;
    } while (!atomic_compare_exchange_weak_explicit(
        &co->waiters_lock, &zero, 1,
        memory_order_acquire, memory_order_relaxed));
}
static void coro_unlock(TyCoro* co) {
    atomic_store_explicit(&co->waiters_lock, 0, memory_order_release);
}

void ty_await(SlabArena* arena, TyCoro* target) {
    (void)arena;
    if (!target) return;
    Worker* w = current_worker();
    TyCoro* me = w ? w->current : NULL;
    if (!me) return;

    coro_lock(target);
    if (atomic_load_explicit(&target->state, memory_order_acquire) == CORO_DONE) {
        coro_unlock(target);
        return;
    }
    me->waiter_next = atomic_load_explicit(&target->waiters, memory_order_relaxed);
    atomic_store_explicit(&target->waiters, me, memory_order_relaxed);
    coro_state_store(me, CORO_BLOCKED, "await_block");
    atomic_fetch_add_explicit(&dbg_blocked, 1, memory_order_relaxed);
    coro_unlock(target);

    ty_ctx_swap(&me->ctx, &w->sched_ctx);
}

void ty_coro_exit(void) {
    Worker* w = current_worker();
    TyCoro* co = w ? w->current : NULL;
    if (!co) return;

    /* NOTE: active_coros is decremented in coro_free(), not here.
     * Decrementing here caused premature shutdown when blocked coroutines
     * (CORO_BLOCKED on a channel recv) were still alive. */
    coro_state_store(co, CORO_DONE, "coro_exit");
    TY_DEBUG("[sched] coro_exit coro=%p active=%d (decrements at coro_free)\n",
        (void*)co, atomic_load_explicit(&active_coros, memory_order_relaxed));

    coro_lock(co);
    TyCoro* waiter = atomic_load_explicit(&co->waiters, memory_order_relaxed);
    atomic_store_explicit(&co->waiters, NULL, memory_order_relaxed);
    coro_unlock(co);
    while (waiter) {
        TyCoro* next = waiter->waiter_next;
        sched_enqueue_wake(waiter);
        waiter = next;
    }

    co->fn = NULL; /* sentinel for coro_free in worker_resume_coro */
    ty_ctx_swap(&co->ctx, &w->sched_ctx);
    TY_TRAP(); /* unreachable */
}

SlabArena* ty_current_arena(void) {
    TyCoro* co = current_coro();
    if (co) return co->arena;
    static SlabArena* main_arena = NULL;
    if (!main_arena) main_arena = slab_arena_new();
    return main_arena;
}

/* ══════════════════════════════════════════════════════════════════════════
 *  Channel
 * ══════════════════════════════════════════════════════════════════════════ */

struct TyChan* ty_chan_new(size_t elem_size, size_t cap) {
    struct TyChan* ch = (struct TyChan*)ty_vm_alloc(sizeof(struct TyChan));
    if (!ch) sched_abort("ty_chan_new: alloc failed");
    ty_mutex_init(&ch->lock);
    ch->elem_size = elem_size;
    ch->cap = cap;
    ch->len = ch->head = ch->tail = 0;
    ch->send_q = ch->recv_q = NULL;
    ch->closed = 0;
    ch->all_next = NULL;
    ch->buf = NULL;
    if (cap > 0) {
        ch->buf = (char*)ty_vm_alloc(cap * elem_size);
        if (!ch->buf)
            sched_abort("ty_chan_new: alloc buf failed");
    }

    ty_mutex_lock(&all_chans_lock);
    ch->all_next = all_chans_head;
    all_chans_head = ch;
    ty_mutex_unlock(&all_chans_lock);
    return ch;
}

static void chan_park(struct TyChan* ch, WaitNode** queue, void* elem, TyMutex* lock) {
    Worker* w = current_worker();
    TyCoro* me = w ? w->current : NULL;
    if (!me) {
        char buf[64];
        int worker_id = w ? w->id : -1;
        snprintf(buf, sizeof(buf), "chan_park: not in a coroutine (worker %d)", worker_id);
        sched_abort(buf);
    }

    // /* During shutdown, never park: shutdown drain loop must finish. */
    // if (atomic_load_explicit(&sched_shutdown_flag, memory_order_acquire)) {
    //     ty_mutex_unlock(lock);
    //     ty_coro_exit();
    // }

    WaitNode* node = (WaitNode*)ty_vm_alloc(sizeof(WaitNode));
    if (!node) sched_abort("chan_park: alloc WaitNode failed");
    node->coro = me;
    node->elem = elem;
    node->next = *queue;
    *queue = node;

    coro_state_store(me, CORO_BLOCKED, "chan_park");
    atomic_fetch_add_explicit(&dbg_blocked, 1, memory_order_relaxed);
    TY_DEBUG("[chan] park coro=%p on %s active=%d\n", (void*)me,
        queue == &ch->recv_q ? "recv_q" : "send_q",
        atomic_load_explicit(&active_coros, memory_order_relaxed));
    ty_mutex_unlock(lock);

    /* During shutdown, do not park. This ensures the drain loop can finish
     * even if coroutines attempt to re-park after being woken by chan_close. */
    if (atomic_load_explicit(&sched_shutdown_flag, memory_order_acquire)) {
        ty_coro_exit();
    }

    ty_ctx_swap(&me->ctx, &w->sched_ctx);
    /* Coroutine resumes here after wakeup — node has been consumed, free it */
    ty_vm_free(node, sizeof(WaitNode));
}

void ty_chan_send(struct SlabArena* arena, struct TyChan* ch, void* elem) {
    (void)arena;
    ty_mutex_lock(&ch->lock);
    Worker* w = current_worker();
    TyCoro* me = w ? w->current : NULL;
    if (ch->closed) {
        ty_mutex_unlock(&ch->lock);
        /* If we are shutting down, just drop the send and return.
         * This prevents aborts/hangs during the drain loop. */
        if (atomic_load_explicit(&sched_shutdown_flag, memory_order_acquire))
            return;
        sched_abort("send on closed chan");
    }

    if (ch->recv_q) {
        WaitNode* r = ch->recv_q;
        ch->recv_q = r->next;
        memcpy(r->elem, elem, ch->elem_size);
        ty_mutex_unlock(&ch->lock);
        sched_enqueue_wake(r->coro);
        return;
    }
    if (ch->cap > 0 && ch->len < ch->cap) {
        memcpy(ch->buf + ch->tail * ch->elem_size, elem, ch->elem_size);
        ch->tail = (ch->tail + 1) % ch->cap;
        ch->len++;
        ty_mutex_unlock(&ch->lock);
        return;
    }
    if (!me) {
        ty_mutex_unlock(&ch->lock);
        for (;;) {
            ty_mutex_lock(&ch->lock);
            if (ch->closed) {
                ty_mutex_unlock(&ch->lock);
                sched_abort("send on closed chan");
            }
            if (ch->recv_q) {
                WaitNode* r = ch->recv_q;
                ch->recv_q = r->next;
                memcpy(r->elem, elem, ch->elem_size);
                ty_mutex_unlock(&ch->lock);
                sched_enqueue_wake(r->coro);
                return;
            }
            if (ch->cap > 0 && ch->len < ch->cap) {
                memcpy(ch->buf + ch->tail * ch->elem_size, elem, ch->elem_size);
                ch->tail = (ch->tail + 1) % ch->cap;
                ch->len++;
                ty_mutex_unlock(&ch->lock);
                return;
            }
            ty_mutex_unlock(&ch->lock);
            ty_sleep_ns(1000);
        }
    }
    chan_park(ch, &ch->send_q, elem, &ch->lock);

    // /* If woken by close, do not silently treat send as successful. */
    // ty_mutex_lock(&ch->lock);
    // int closed = ch->closed;
    // ty_mutex_unlock(&ch->lock);
    // if (closed) {
    //     if (atomic_load_explicit(&sched_shutdown_flag, memory_order_acquire))
    //         ty_coro_exit();
    //     sched_abort("send on closed chan");
    // }
}

void ty_chan_recv(struct SlabArena* arena, struct TyChan* ch, void* out) {
    (void)arena;
    ty_mutex_lock(&ch->lock);
    Worker* w = current_worker();
    TyCoro* me = w ? w->current : NULL;
    if (ch->len > 0) {
        memcpy(out, ch->buf + ch->head * ch->elem_size, ch->elem_size);
        ch->head = (ch->head + 1) % ch->cap;
        ch->len--;
        if (ch->send_q) {
            WaitNode* s = ch->send_q;
            ch->send_q = s->next;
            memcpy(ch->buf + ch->tail * ch->elem_size, s->elem, ch->elem_size);
            ch->tail = (ch->tail + 1) % ch->cap;
            ch->len++;
            ty_mutex_unlock(&ch->lock);
            sched_enqueue_wake(s->coro);
        } else {
            ty_mutex_unlock(&ch->lock);
        }
        return;
    }
    if (!me) {
        ty_mutex_unlock(&ch->lock);
        for (;;) {
            ty_mutex_lock(&ch->lock);
            if (ch->len > 0) {
                memcpy(out, ch->buf + ch->head * ch->elem_size, ch->elem_size);
                ch->head = (ch->head + 1) % ch->cap;
                ch->len--;
                if (ch->send_q) {
                    WaitNode* s = ch->send_q;
                    ch->send_q = s->next;
                    memcpy(ch->buf + ch->tail * ch->elem_size, s->elem, ch->elem_size);
                    ch->tail = (ch->tail + 1) % ch->cap;
                    ch->len++;
                    ty_mutex_unlock(&ch->lock);
                    sched_enqueue_wake(s->coro);
                } else {
                    ty_mutex_unlock(&ch->lock);
                }
                return;
            }
            if (ch->send_q) {
                WaitNode* s = ch->send_q;
                ch->send_q = s->next;
                memcpy(out, s->elem, ch->elem_size);
                ty_mutex_unlock(&ch->lock);
                sched_enqueue_wake(s->coro);
                return;
            }
            if (ch->closed) {
                memset(out, 0, ch->elem_size);
                ty_mutex_unlock(&ch->lock);
                return;
            }
            ty_mutex_unlock(&ch->lock);
            ty_sleep_ns(1000);
        }
    }
    if (ch->send_q) {
        WaitNode* s = ch->send_q;
        ch->send_q = s->next;
        memcpy(out, s->elem, ch->elem_size);
        ty_mutex_unlock(&ch->lock);
        sched_enqueue_wake(s->coro);
        return;
    }
    if (ch->closed) {
        memset(out, 0, ch->elem_size);
        ty_mutex_unlock(&ch->lock);
        return;
    }
    chan_park(ch, &ch->recv_q, out, &ch->lock);
}

int ty_chan_try_recv(struct SlabArena* arena, struct TyChan* ch, void* out) {
    (void)arena;
    ty_mutex_lock(&ch->lock);

    if (ch->len > 0) {
        memcpy(out, ch->buf + ch->head * ch->elem_size, ch->elem_size);
        ch->head = (ch->head + 1) % ch->cap;
        ch->len--;
        if (ch->send_q) {
            WaitNode* s = ch->send_q;
            ch->send_q = s->next;
            memcpy(ch->buf + ch->tail * ch->elem_size, s->elem, ch->elem_size);
            ch->tail = (ch->tail + 1) % ch->cap;
            ch->len++;
            ty_mutex_unlock(&ch->lock);
            sched_enqueue_wake(s->coro);
        } else {
            ty_mutex_unlock(&ch->lock);
        }
        return 1;
    }

    if (ch->send_q) {
        WaitNode* s = ch->send_q;
        ch->send_q = s->next;
        memcpy(out, s->elem, ch->elem_size);
        ty_mutex_unlock(&ch->lock);
        sched_enqueue_wake(s->coro);
        return 1;
    }

    if (ch->closed) {
        memset(out, 0, ch->elem_size);
        ty_mutex_unlock(&ch->lock);
        return -1;
    }

    ty_mutex_unlock(&ch->lock);
    return 0;
}

void ty_chan_close(struct TyChan* ch) {
    ty_mutex_lock(&ch->lock);
    ch->closed = 1;
    WaitNode* r = ch->recv_q;
    ch->recv_q = NULL;
    WaitNode* s = ch->send_q;
    ch->send_q = NULL;
    ty_mutex_unlock(&ch->lock);
    while (r) {
        WaitNode* next = r->next;
        memset(r->elem, 0, ch->elem_size);
        sched_enqueue_wake(r->coro);
        r = next;
    }
    while (s) {
        WaitNode* next = s->next;
        sched_enqueue_wake(s->coro);
        s = next;
    }
}

void ty_coro_block_and_yield(void) {
    Worker* w = current_worker();
    TyCoro* me = w ? w->current : NULL;
    if (!me) return;
    if (atomic_load_explicit(&sched_shutdown_flag, memory_order_acquire)) {
        TY_DEBUG("[sched] shutdown:block_and_yield worker=%d coro=%p state=%d active=%d\n",
            w->id,
            (void*)me,
            (int)atomic_load_explicit(&me->state, memory_order_relaxed),
            atomic_load_explicit(&active_coros, memory_order_relaxed));
    }
    coro_state_store(me, CORO_BLOCKED, "io_block_and_yield");
    atomic_fetch_add_explicit(&dbg_blocked, 1, memory_order_relaxed);
    ty_ctx_swap(&me->ctx, &w->sched_ctx);
}

void sched_enqueue_from_external(void* co) {
    TyCoro* coro = (TyCoro*)co;
    // if (!coro_state_cas(coro, CORO_BLOCKED, CORO_RUNNABLE, "external_enqueue"))
    //     return;
    deque_push(&workers[0].deque, coro);
}

void* ty_current_coro_raw(void) {
    return (void*)current_coro();
}

Worker* ty_current_worker_ptr(void) {
    return current_worker();
}
