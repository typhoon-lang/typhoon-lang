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
#include <string.h>

/* ── forward declarations ────────────────────────────────────────────────── */

struct SlabArena* slab_arena_new(void);
void              slab_arena_free(struct SlabArena*);

/* ── configuration ───────────────────────────────────────────────────────── */

#define CORO_STACK_SIZE   (64 * 1024)   /* 64 KB coroutine stack data          */
#define GUARD_PAGE_SIZE   4096          /* one page, PROT_NONE / PAGE_NOACCESS */
#define DEQUE_INITIAL_CAP 256           /* must be power of 2                  */
#define MAX_WORKERS       64
#define SIGPROF_HZ        100           /* preemption ticks per second         */
#define STEAL_RETRIES     4             /* steal attempts before sleeping       */

/* ── hard abort ──────────────────────────────────────────────────────────── */

static void sched_abort(const char* msg) {
    ty_stderr_write(msg);
    TY_TRAP();
}

/* ══════════════════════════════════════════════════════════════════════════
 *  Chase-Lev Work-Stealing Deque
 * ══════════════════════════════════════════════════════════════════════════ */

typedef struct {
    _Atomic(size_t)  cap;
    _Atomic(void**)  buf;
} DequeArray;

typedef struct WSDeque {
    _Atomic(long)         top;
    _Atomic(long)         bottom;
    _Atomic(DequeArray*)  array;
} WSDeque;

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
    atomic_init(&dq->top,    0);
    atomic_init(&dq->bottom, 0);
    atomic_init(&dq->array,  deque_array_new(DEQUE_INITIAL_CAP));
}

static void deque_grow(WSDeque* dq, DequeArray* old, long top, long bot) {
    size_t old_cap = atomic_load_explicit(&old->cap, memory_order_relaxed);
    size_t new_cap = old_cap * 2;
    DequeArray* fresh = deque_array_new(new_cap);
    void** ob = atomic_load_explicit(&old->buf, memory_order_relaxed);
    void** nb = atomic_load_explicit(&fresh->buf, memory_order_relaxed);
    for (long i = top; i < bot; i++)
        nb[i & (new_cap-1)] = ob[i & (old_cap-1)];
    atomic_store_explicit(&dq->array, fresh, memory_order_release);
    /* old array leaked — safe reclamation needs hazard pointers */
}

static void deque_push(WSDeque* dq, void* item) {
    long bot = atomic_load_explicit(&dq->bottom, memory_order_relaxed);
    long top = atomic_load_explicit(&dq->top,    memory_order_acquire);
    DequeArray* a = atomic_load_explicit(&dq->array, memory_order_relaxed);
    size_t cap    = atomic_load_explicit(&a->cap,    memory_order_relaxed);
    if ((size_t)(bot - top) >= cap - 1) {
        deque_grow(dq, a, top, bot);
        a   = atomic_load_explicit(&dq->array, memory_order_relaxed);
        cap = atomic_load_explicit(&a->cap,    memory_order_relaxed);
    }
    void** buf = atomic_load_explicit(&a->buf, memory_order_relaxed);
    buf[bot & (cap-1)] = item;
    atomic_thread_fence(memory_order_release);
    atomic_store_explicit(&dq->bottom, bot + 1, memory_order_relaxed);
}

static void* deque_pop(WSDeque* dq) {
    long bot = atomic_load_explicit(&dq->bottom, memory_order_relaxed) - 1;
    DequeArray* a = atomic_load_explicit(&dq->array, memory_order_relaxed);
    atomic_store_explicit(&dq->bottom, bot, memory_order_relaxed);
    atomic_thread_fence(memory_order_seq_cst);
    long top = atomic_load_explicit(&dq->top, memory_order_relaxed);
    if (top > bot) {
        atomic_store_explicit(&dq->bottom, bot + 1, memory_order_relaxed);
        return NULL;
    }
    size_t cap = atomic_load_explicit(&a->cap, memory_order_relaxed);
    void** buf = atomic_load_explicit(&a->buf, memory_order_relaxed);
    void*  item = buf[bot & (cap-1)];
    if (top == bot) {
        long expected = top;
        if (!atomic_compare_exchange_strong_explicit(
                &dq->top, &expected, top + 1,
                memory_order_seq_cst, memory_order_relaxed))
            item = NULL;
        atomic_store_explicit(&dq->bottom, bot + 1, memory_order_relaxed);
    }
    return item;
}

static void* deque_steal(WSDeque* dq) {
    long top = atomic_load_explicit(&dq->top, memory_order_acquire);
    atomic_thread_fence(memory_order_seq_cst);
    long bot = atomic_load_explicit(&dq->bottom, memory_order_acquire);
    if (top >= bot) return NULL;
    DequeArray* a   = atomic_load_explicit(&dq->array, memory_order_consume);
    size_t      cap = atomic_load_explicit(&a->cap,    memory_order_relaxed);
    void**      buf = atomic_load_explicit(&a->buf,    memory_order_relaxed);
    void*       item = buf[top & (cap-1)];
    long expected = top;
    if (!atomic_compare_exchange_strong_explicit(
            &dq->top, &expected, top + 1,
            memory_order_seq_cst, memory_order_relaxed))
        return NULL;
    return item;
}

/* ══════════════════════════════════════════════════════════════════════════
 *  Coroutine
 * ══════════════════════════════════════════════════════════════════════════ */

typedef enum {
    CORO_RUNNABLE = 0,
    CORO_RUNNING,
    CORO_BLOCKED,
    CORO_DONE
} CoroState;

struct TyCoro {
    TyCtx                    ctx;
    void                   (*fn)(void*, void*);
    void                    *arg;
    char                    *stack_base;
    size_t                   stack_total;
    struct SlabArena        *arena;
    _Atomic(CoroState)       state;
    _Atomic(int)             ref;
    _Atomic(struct TyCoro*)  waiters;
    _Atomic(int)             waiters_lock;
    struct TyCoro           *waiter_next;
    struct TyCoro           *sched_next;
};

/* ══════════════════════════════════════════════════════════════════════════
 *  Worker
 * ══════════════════════════════════════════════════════════════════════════ */

typedef struct Worker {
    TyThread      thread;
    int           id;
    WSDeque       deque;
    TyCoro       *current;
    TyCtx         sched_ctx;      /* scheduler's saved context                */
    _Atomic(int)  preempt_flag;
    _Atomic(int)  running;
} Worker;

/* ── global state ────────────────────────────────────────────────────────── */

static Worker           workers[MAX_WORKERS];
static int              num_workers = 0;
static _Atomic(int)     sched_shutdown_flag;
static _Atomic(int)     active_coros;

static TY_THREAD_LOCAL Worker* tl_worker = NULL;

static Worker* current_worker(void)  { return tl_worker; }
static TyCoro* current_coro(void)    { Worker* w = current_worker(); return w ? w->current : NULL; }

/* ── coroutine trampoline ────────────────────────────────────────────────── */

static void coro_trampoline(uint32_t hi, uint32_t lo) {
    uintptr_t ptr = ((uintptr_t)hi << 32) | (uintptr_t)lo;
    TyCoro* co = (TyCoro*)ptr;
    co->fn(co->arena, co->arg);
    ty_coro_exit();
}

/* ── coroutine lifecycle ─────────────────────────────────────────────────── */

static TyCoro* coro_new(void (*fn)(void*, void*), void* arg) {
    TyCoro* co = (TyCoro*)ty_vm_alloc(sizeof(TyCoro));
    if (!co) sched_abort("coro_new: alloc TyCoro failed");

    co->fn  = fn;
    co->arg = arg;
    atomic_init((_Atomic(struct TyCoro*)*)&co->waiters, NULL);
    atomic_init(&co->waiters_lock, 0);
    co->waiter_next = NULL;
    co->sched_next  = NULL;
    co->arena       = slab_arena_new();
    atomic_init(&co->state, CORO_RUNNABLE);
    atomic_init(&co->ref,   1);

    size_t total = GUARD_PAGE_SIZE + CORO_STACK_SIZE;
    char* base = (char*)ty_vm_alloc(total);
    if (!base) sched_abort("coro_new: alloc stack failed");
    if (!ty_vm_guard(base, GUARD_PAGE_SIZE))
        sched_abort("coro_new: guard page failed");

    co->stack_base  = base;
    co->stack_total = total;

    uintptr_t ptr = (uintptr_t)co;
    ty_ctx_init(&co->ctx,
                base + GUARD_PAGE_SIZE, CORO_STACK_SIZE,
                coro_trampoline,
                (uint32_t)(ptr >> 32),
                (uint32_t)(ptr & 0xFFFFFFFFu));
    return co;
}

static void coro_free(TyCoro* co) {
    if (!co) return;
    slab_arena_free(co->arena);
    ty_vm_free(co->stack_base, co->stack_total);
    ty_vm_free(co, sizeof(TyCoro));
}

/* ── scheduling helpers ──────────────────────────────────────────────────── */

static void sched_enqueue(TyCoro* co) {
    atomic_store_explicit(&co->state, CORO_RUNNABLE, memory_order_release);
    Worker* w = current_worker();
    deque_push(w ? &w->deque : &workers[0].deque, co);
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

/* ── preemption ──────────────────────────────────────────────────────────── */

static void preempt_tick(void) {
    Worker* w = current_worker();
    if (w) atomic_store_explicit(&w->preempt_flag, 1, memory_order_relaxed);
}

/* ── worker run / loop ───────────────────────────────────────────────────── */

static void worker_run_coro(Worker* w, TyCoro* co) {
    w->current = co;
    atomic_store_explicit(&co->state, CORO_RUNNING, memory_order_release);
    ty_ctx_swap(&w->sched_ctx, &co->ctx);
    w->current = NULL;
    if (co->fn == NULL) coro_free(co);
}

static void* worker_thread(void* arg) {
    Worker* w = (Worker*)arg;
    tl_worker = w;

#ifdef TY_WINDOWS
    /* On Windows each thread must become a fiber before calling SwitchToFiber. */
    ty_ctx_init_sched(&w->sched_ctx);
#endif

    while (!atomic_load_explicit(&sched_shutdown_flag, memory_order_acquire)) {
        atomic_store_explicit(&w->preempt_flag, 0, memory_order_relaxed);
        TyCoro* co = sched_next_coro(w);
        if (!co) {
            ty_sleep_ns(100000); /* 100 µs */
            continue;
        }
        worker_run_coro(w, co);
    }
    atomic_store_explicit(&w->running, 0, memory_order_release);
    return NULL;
}

/* ══════════════════════════════════════════════════════════════════════════
 *  Public API
 * ══════════════════════════════════════════════════════════════════════════ */

void ty_sched_init(void) {
    atomic_init(&sched_shutdown_flag, 0);
    atomic_init(&active_coros, 0);

    num_workers = ty_cpu_count();
    if (num_workers > MAX_WORKERS) num_workers = MAX_WORKERS;

    ty_preempt_install(preempt_tick, SIGPROF_HZ);

    workers[0].id = 0;
    deque_init(&workers[0].deque);
    atomic_init(&workers[0].preempt_flag, 0);
    atomic_init(&workers[0].running, 1);
    workers[0].current = NULL;

    for (int i = 1; i < num_workers; i++) {
        workers[i].id = i;
        deque_init(&workers[i].deque);
        atomic_init(&workers[i].preempt_flag, 0);
        atomic_init(&workers[i].running, 1);
        workers[i].current = NULL;
        if (!ty_thread_create(&workers[i].thread, worker_thread, &workers[i]))
            sched_abort("ty_sched_init: thread_create failed");
    }

    tl_worker = &workers[0];

#ifdef TY_WINDOWS
    ty_ctx_init_sched(&workers[0].sched_ctx);
#endif
}

void ty_sched_shutdown(void) {
    Worker* w = &workers[0];

    while (atomic_load_explicit(&active_coros, memory_order_acquire) > 0) {
        for (int i = 1; i < num_workers; i++) {
            TyCoro* stolen = (TyCoro*)deque_steal(&workers[i].deque);
            if (stolen) deque_push(&w->deque, stolen);
        }
        TyCoro* co = (TyCoro*)deque_pop(&w->deque);
        if (co) {
            worker_run_coro(w, co);
        } else {
            ty_sleep_ns(10000); /* 10 µs */
        }
    }
    for (;;) {
        TyCoro* co = (TyCoro*)deque_pop(&w->deque);
        if (!co) break;
        worker_run_coro(w, co);
    }

    atomic_store_explicit(&sched_shutdown_flag, 1, memory_order_release);
    for (int i = 1; i < num_workers; i++)
        ty_thread_join(workers[i].thread);

    ty_preempt_stop();
}

TyCoro* ty_spawn(struct SlabArena* arena, void (*fn)(void*, void*), void* arg) {
    (void)arena;
    TyCoro* co = coro_new(fn, arg);
    atomic_fetch_add_explicit(&active_coros, 1, memory_order_relaxed);
    sched_enqueue(co);
    return co;
}

void ty_yield(void) {
    Worker* w = current_worker();
    if (!w || !w->current) return;
    TyCoro* co = w->current;
    CoroState expected = CORO_RUNNING;
    if (atomic_compare_exchange_strong_explicit(
            &co->state, &expected, CORO_RUNNABLE,
            memory_order_acq_rel, memory_order_relaxed))
        deque_push(&w->deque, co);
    ty_ctx_swap(&co->ctx, &w->sched_ctx);
}

static void coro_lock(TyCoro* co) {
    int zero;
    do { zero = 0; }
    while (!atomic_compare_exchange_weak_explicit(
        &co->waiters_lock, &zero, 1,
        memory_order_acquire, memory_order_relaxed));
}
static void coro_unlock(TyCoro* co) {
    atomic_store_explicit(&co->waiters_lock, 0, memory_order_release);
}

void ty_await(struct SlabArena* arena, TyCoro* target) {
    (void)arena;
    if (!target) return;
    Worker* w  = current_worker();
    TyCoro* me = w ? w->current : NULL;
    if (!me) return;

    coro_lock(target);
    if (atomic_load_explicit(&target->state, memory_order_acquire) == CORO_DONE) {
        coro_unlock(target);
        return;
    }
    me->waiter_next = atomic_load_explicit(&target->waiters, memory_order_relaxed);
    atomic_store_explicit(&target->waiters, me, memory_order_relaxed);
    atomic_store_explicit(&me->state, CORO_BLOCKED, memory_order_release);
    coro_unlock(target);

    ty_ctx_swap(&me->ctx, &w->sched_ctx);
}

void ty_coro_exit(void) {
    Worker* w  = current_worker();
    TyCoro* co = w ? w->current : NULL;
    if (!co) return;

    atomic_store_explicit(&co->state, CORO_DONE, memory_order_release);
    atomic_fetch_sub_explicit(&active_coros, 1, memory_order_release);

    coro_lock(co);
    TyCoro* waiter = atomic_load_explicit(&co->waiters, memory_order_relaxed);
    atomic_store_explicit(&co->waiters, NULL, memory_order_relaxed);
    coro_unlock(co);
    while (waiter) {
        TyCoro* next = waiter->waiter_next;
        sched_enqueue(waiter);
        waiter = next;
    }

    co->fn = NULL; /* sentinel for coro_free in worker_run_coro */
    ty_ctx_swap(&co->ctx, &w->sched_ctx);
    TY_TRAP(); /* unreachable */
}

struct SlabArena* ty_current_arena(void) {
    TyCoro* co = current_coro();
    if (co) return co->arena;
    static struct SlabArena* main_arena = NULL;
    if (!main_arena) main_arena = slab_arena_new();
    return main_arena;
}

/* ══════════════════════════════════════════════════════════════════════════
 *  Channel
 * ══════════════════════════════════════════════════════════════════════════ */

typedef struct WaitNode {
    TyCoro*          coro;
    void*            elem;
    struct WaitNode* next;
} WaitNode;

struct TyChan {
    TyMutex   lock;
    size_t    elem_size;
    size_t    cap;
    size_t    len;
    size_t    head;
    size_t    tail;
    char*     buf;
    WaitNode* send_q;
    WaitNode* recv_q;
    int       closed;
};

TyChan* ty_chan_new(size_t elem_size, size_t cap) {
    TyChan* ch = (TyChan*)ty_vm_alloc(sizeof(TyChan));
    if (!ch) sched_abort("ty_chan_new: alloc failed");
    ty_mutex_init(&ch->lock);
    ch->elem_size = elem_size;
    ch->cap       = cap;
    ch->len = ch->head = ch->tail = 0;
    ch->send_q = ch->recv_q = NULL;
    ch->closed = 0;
    ch->buf = NULL;
    if (cap > 0) {
        ch->buf = (char*)ty_vm_alloc(cap * elem_size);
        if (!ch->buf) sched_abort("ty_chan_new: alloc buf failed");
    }
    return ch;
}

static void chan_park(TyChan* ch, WaitNode** queue, void* elem, TyMutex* lock) {
    Worker* w  = current_worker();
    TyCoro* me = w ? w->current : NULL;
    if (!me) sched_abort("chan_park: not in a coroutine");
    WaitNode node = { .coro = me, .elem = elem, .next = *queue };
    *queue = &node;
    atomic_store_explicit(&me->state, CORO_BLOCKED, memory_order_release);
    ty_mutex_unlock(lock);
    ty_ctx_swap(&me->ctx, &w->sched_ctx);
    (void)ch;
}

void ty_chan_send(struct SlabArena* arena, TyChan* ch, void* elem) {
    (void)arena;
    ty_mutex_lock(&ch->lock);
    if (ch->closed) { ty_mutex_unlock(&ch->lock); sched_abort("send on closed chan"); }

    if (ch->recv_q) {
        WaitNode* r = ch->recv_q; ch->recv_q = r->next;
        memcpy(r->elem, elem, ch->elem_size);
        ty_mutex_unlock(&ch->lock);
        sched_enqueue(r->coro);
        return;
    }
    if (ch->cap > 0 && ch->len < ch->cap) {
        memcpy(ch->buf + ch->tail * ch->elem_size, elem, ch->elem_size);
        ch->tail = (ch->tail + 1) % ch->cap;
        ch->len++;
        ty_mutex_unlock(&ch->lock);
        return;
    }
    chan_park(ch, &ch->send_q, elem, &ch->lock);
}

void ty_chan_recv(struct SlabArena* arena, TyChan* ch, void* out) {
    (void)arena;
    ty_mutex_lock(&ch->lock);
    if (ch->len > 0) {
        memcpy(out, ch->buf + ch->head * ch->elem_size, ch->elem_size);
        ch->head = (ch->head + 1) % ch->cap;
        ch->len--;
        if (ch->send_q) {
            WaitNode* s = ch->send_q; ch->send_q = s->next;
            memcpy(ch->buf + ch->tail * ch->elem_size, s->elem, ch->elem_size);
            ch->tail = (ch->tail + 1) % ch->cap;
            ch->len++;
            ty_mutex_unlock(&ch->lock);
            sched_enqueue(s->coro);
        } else { ty_mutex_unlock(&ch->lock); }
        return;
    }
    if (ch->send_q) {
        WaitNode* s = ch->send_q; ch->send_q = s->next;
        memcpy(out, s->elem, ch->elem_size);
        ty_mutex_unlock(&ch->lock);
        sched_enqueue(s->coro);
        return;
    }
    if (ch->closed) { memset(out, 0, ch->elem_size); ty_mutex_unlock(&ch->lock); return; }
    chan_park(ch, &ch->recv_q, out, &ch->lock);
}

void ty_chan_close(TyChan* ch) {
    ty_mutex_lock(&ch->lock);
    ch->closed = 1;
    WaitNode* r = ch->recv_q; ch->recv_q = NULL;
    ty_mutex_unlock(&ch->lock);
    while (r) {
        WaitNode* next = r->next;
        memset(r->elem, 0, ch->elem_size);
        sched_enqueue(r->coro);
        r = next;
    }
}
