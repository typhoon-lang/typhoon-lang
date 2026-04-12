/*
 * platform.h — cross-platform shims for scheduler.c
 *
 * Provides a single set of macros/inlines that cover:
 *   - Virtual memory  (mmap / VirtualAlloc)
 *   - Threads         (pthread / CreateThread)
 *   - Thread-local    (__thread / __declspec(thread))
 *   - Mutex           (pthread_mutex / CRITICAL_SECTION)
 *   - Coroutine ctx   (ucontext / Fiber)
 *   - Sleep           (nanosleep / Sleep)
 *   - CPU count       (sysconf / GetSystemInfo)
 *   - Preemption      (SIGPROF+setitimer / CreateWaitableTimer)
 *   - stderr write    (write / WriteFile)
 */

#ifndef TY_PLATFORM_H
#define TY_PLATFORM_H

#include <stddef.h>
#include <stdint.h>
#include <string.h>   /* memset, memcpy */

/* ── platform detection ─────────────────────────────────────────────────── */

#if defined(_WIN32) || defined(_WIN64)
#  define TY_WINDOWS 1
#else
#  define TY_POSIX   1
#endif

/* ── hard trap ──────────────────────────────────────────────────────────── */
#ifdef TY_WINDOWS
#  define TY_TRAP() __debugbreak()
#else
#  define TY_TRAP() __builtin_trap()
#endif


/* ══════════════════════════════════════════════════════════════════════════
 *  POSIX path
 * ══════════════════════════════════════════════════════════════════════════ */
#ifdef TY_POSIX

#include <pthread.h>
#include <signal.h>
#include <sys/mman.h>
#include <sys/time.h>
#include <unistd.h>

/* ── virtual memory ──────────────────────────────────────────────────────── */
static inline void* ty_vm_alloc(size_t size) {
    void* p = mmap(NULL, size, PROT_READ|PROT_WRITE,
                   MAP_PRIVATE|MAP_ANONYMOUS, -1, 0);
    return (p == MAP_FAILED) ? NULL : p;
}
static inline void ty_vm_free(void* p, size_t size) { munmap(p, size); }
static inline int  ty_vm_guard(void* p, size_t size) {
    return mprotect(p, size, PROT_NONE) == 0 ? 1 : 0;
}

/* ── thread-local ────────────────────────────────────────────────────────── */
#define TY_THREAD_LOCAL __thread

/* ── threads ─────────────────────────────────────────────────────────────── */
typedef pthread_t TyThread;
static inline int ty_thread_create(TyThread* t, void*(*fn)(void*), void* arg) {
    return pthread_create(t, NULL, fn, arg) == 0 ? 1 : 0;
}
static inline void ty_thread_join(TyThread t) { pthread_join(t, NULL); }

/* ── mutex ───────────────────────────────────────────────────────────────── */
typedef pthread_mutex_t TyMutex;
#define TY_MUTEX_INIT    PTHREAD_MUTEX_INITIALIZER
static inline void ty_mutex_init(TyMutex* m)    { pthread_mutex_init(m, NULL); }
static inline void ty_mutex_lock(TyMutex* m)    { pthread_mutex_lock(m); }
static inline void ty_mutex_unlock(TyMutex* m)  { pthread_mutex_unlock(m); }

/* ── coroutine context (inline asm, replaces ucontext) ──────
 *
 * Register save layout (TyCtx.regs[]):
 *   [0] rsp   [1] r15   [2] r14   [3] r13
 *   [4] r12   [5] rbx   [6] rbp   [7] rip  (written by call, read by ret)
 *
 * ty_ctx_swap(from, to):
 *   — saves caller's callee-saved regs + rsp into from->regs
 *   — restores to->regs and jumps to to->regs[7] (rip)
 *
 * ty_ctx_init(ctx, stack, stack_size, tramp, hi, lo):
 *   — places a return address pointing at ty__coro_entry on the stack
 *   — stores hi/lo in slots that ty__coro_entry will read as arguments
 *   — sets rsp to point at that return address
 * ────────────────────────────────────────────────────────────────────── */

#if defined(__x86_64__) || defined(_M_X64)

typedef struct {
    uint64_t regs[8];   /* rsp, r15, r14, r13, r12, rbx, rbp, rip */
} TyCtx;

/* Implemented in ty_ctx.S — one translation unit only. */
extern void ty_ctx_swap(TyCtx* from, TyCtx* to);
extern void ty__coro_entry(void);

static inline void ty_ctx_init(TyCtx* ctx, void* stack_bottom, size_t stack_size,
                 void (*tramp)(uint32_t, uint32_t),
                 uint32_t hi, uint32_t lo) {
    // Stacks grow down; start at the top
    uint64_t* stack_top = (uint64_t*)((char*)stack_bottom + stack_size);

    // Push arguments in the order ty__coro_entry will pop them
    *(--stack_top) = (uint64_t)lo;     // [rsp+16]
    *(--stack_top) = (uint64_t)hi;     // [rsp+8]
    *(--stack_top) = (uint64_t)tramp;  // [rsp+0]

    memset(ctx, 0, sizeof(*ctx));
    ctx->regs[0] = (uint64_t)stack_top;        // Saved RSP
    ctx->regs[7] = (uint64_t)ty__coro_entry;   // Saved RIP
}

#elif defined(__aarch64__)

typedef struct {
    uint64_t regs[14];
    /* x19,x20,x21,x22,x23,x24,x25,x26,x27,x28,x29,x30(lr),sp,pad */
} TyCtx;

/* Implemented in ty_ctx.S — one translation unit only. */
extern void ty_ctx_swap(TyCtx* from, TyCtx* to);
extern void ty__coro_entry(void);

static inline void ty_ctx_init(TyCtx* ctx, void* stack_bottom, size_t stack_size,
                 void (*tramp)(uint32_t, uint32_t),
                 uint32_t hi, uint32_t lo) {
    memset(ctx, 0, sizeof(*ctx));

    // x19, x20, x21 are used by ty__coro_entry
    ctx->regs[0]  = (uint64_t)tramp;           // x19
    ctx->regs[1]  = (uint64_t)hi;              // x20
    ctx->regs[2]  = (uint64_t)lo;              // x21
    ctx->regs[11] = (uint64_t)ty__coro_entry;  // x30 (Link Register)
    ctx->regs[12] = (uint64_t)stack_bottom + stack_size; // sp
}

#endif

/* ── sleep ───────────────────────────────────────────────────────────────── */
static inline void ty_sleep_ns(long ns) {
    struct timespec ts = { .tv_sec = 0, .tv_nsec = ns };
    nanosleep(&ts, NULL);
}

/* ── CPU count ───────────────────────────────────────────────────────────── */
static inline int ty_cpu_count(void) {
    int n = (int)sysconf(_SC_NPROCESSORS_ONLN);
    return n < 1 ? 1 : n;
}

/* ── preemption (SIGPROF) ────────────────────────────────────────────────── */
typedef void(*TyPreemptHandler)(void);
static TyPreemptHandler ty__preempt_cb = NULL;
static void ty__sigprof(int sig) { (void)sig; if (ty__preempt_cb) ty__preempt_cb(); }

static inline void ty_preempt_install(TyPreemptHandler cb, int hz) {
    ty__preempt_cb = cb;
    struct sigaction sa;
    memset(&sa, 0, sizeof(sa));
    sa.sa_handler = ty__sigprof;
    sigemptyset(&sa.sa_mask);
    sa.sa_flags = SA_RESTART;
    sigaction(SIGPROF, &sa, NULL);
    struct itimerval itv;
    itv.it_interval.tv_sec  = 0;
    itv.it_interval.tv_usec = 1000000 / hz;
    itv.it_value = itv.it_interval;
    setitimer(ITIMER_PROF, &itv, NULL);
}
static inline void ty_preempt_stop(void) {
    struct itimerval zero = {0};
    setitimer(ITIMER_PROF, &zero, NULL);
}

/* ── stderr write (signal-safe) ──────────────────────────────────────────── */
static inline void ty_stderr_write(const char* msg) {
    size_t n = strlen(msg);
#pragma GCC diagnostic push
#pragma GCC diagnostic ignored "-Wunused-result"
    write(STDERR_FILENO, msg, n);
    write(STDERR_FILENO, "\n", 1);
#pragma GCC diagnostic pop
}

#endif /* TY_POSIX */

/* ══════════════════════════════════════════════════════════════════════════
 *  Windows path
 * ══════════════════════════════════════════════════════════════════════════ */
#ifdef TY_WINDOWS

#ifndef WIN32_LEAN_AND_MEAN
#  define WIN32_LEAN_AND_MEAN
#endif
#include <windows.h>

/* ── virtual memory ──────────────────────────────────────────────────────── */
static inline void* ty_vm_alloc(size_t size) {
    return VirtualAlloc(NULL, size, MEM_COMMIT|MEM_RESERVE, PAGE_READWRITE);
}
static inline void ty_vm_free(void* p, size_t size) {
    (void)size; VirtualFree(p, 0, MEM_RELEASE);
}
static inline int ty_vm_guard(void* p, size_t size) {
    DWORD old;
    return VirtualProtect(p, size, PAGE_NOACCESS, &old) ? 1 : 0;
}

/* ── thread-local ────────────────────────────────────────────────────────── */
#define TY_THREAD_LOCAL __declspec(thread)

/* ── threads ─────────────────────────────────────────────────────────────── */
typedef HANDLE TyThread;
typedef struct { void*(*fn)(void*); void* arg; } TyThreadArgs;
static DWORD WINAPI ty__thread_tramp(LPVOID p) {
    TyThreadArgs* a = (TyThreadArgs*)p;
    a->fn(a->arg);
    /* TyThreadArgs is heap-allocated by ty_thread_create */
    HeapFree(GetProcessHeap(), 0, a);
    return 0;
}
static inline int ty_thread_create(TyThread* t, void*(*fn)(void*), void* arg) {
    TyThreadArgs* a = (TyThreadArgs*)HeapAlloc(GetProcessHeap(), 0, sizeof(*a));
    if (!a) return 0;
    a->fn = fn; a->arg = arg;
    *t = CreateThread(NULL, 0, ty__thread_tramp, a, 0, NULL);
    return *t != NULL ? 1 : 0;
}
static inline void ty_thread_join(TyThread t) {
    WaitForSingleObject(t, INFINITE);
    CloseHandle(t);
}

/* ── mutex ───────────────────────────────────────────────────────────────── */
typedef CRITICAL_SECTION TyMutex;
/* No static initialiser equivalent on Windows — always call ty_mutex_init. */
#define TY_MUTEX_INIT {0}
static inline void ty_mutex_init(TyMutex* m)    { InitializeCriticalSection(m); }
static inline void ty_mutex_lock(TyMutex* m)    { EnterCriticalSection(m); }
static inline void ty_mutex_unlock(TyMutex* m)  { LeaveCriticalSection(m); }

/* ── coroutine context (Windows Fibers) ──────────────────────────────────── */
/*
 * Fibers are Windows' built-in stackful coroutine primitive.
 * SwitchToFiber saves the current fiber's registers and resumes another.
 * The "scheduler context" for each worker is itself a Fiber
 * (converted from the thread with ConvertThreadToFiber).
 *
 * TyCtx wraps the LPVOID fiber handle plus the startup closure so the
 * trampoline can reconstruct the TyCoro pointer.
 */
typedef struct {
    LPVOID   fiber;
    /* startup data — only used before first resume */
    void   (*tramp_fn)(uint32_t, uint32_t);
    uint32_t hi, lo;
    int      started;
} TyCtx;

static VOID CALLBACK ty__fiber_entry(LPVOID param) {
    TyCtx* ctx = (TyCtx*)param;
    ctx->tramp_fn(ctx->hi, ctx->lo);
    /* should not reach here — trampoline calls ty_coro_exit */
    __debugbreak();
}

static inline void ty_ctx_init(TyCtx* ctx, void* stack, size_t stack_size,
                                void(*tramp)(uint32_t, uint32_t),
                                uint32_t hi, uint32_t lo) {
    (void)stack; /* Fiber allocates its own stack */
    ctx->tramp_fn = tramp;
    ctx->hi       = hi;
    ctx->lo       = lo;
    ctx->started  = 0;
    ctx->fiber    = CreateFiberEx(stack_size, stack_size, 0,
                                  ty__fiber_entry, ctx);
}

static inline void ty_ctx_swap(TyCtx* from, TyCtx* to) {
    /* 'from' is the scheduler ctx; it was set up with ConvertThreadToFiber
     * so its fiber handle is in from->fiber. We just switch to 'to'. */
    (void)from; /* SwitchToFiber saves current automatically */
    SwitchToFiber(to->fiber);
}

/*
 * Special: worker 0's sched_ctx.fiber must be initialised with
 * ConvertThreadToFiber before any SwitchToFiber is called.
 * Call ty_ctx_init_sched() on the scheduler context of each worker thread.
 */
static inline void ty_ctx_init_sched(TyCtx* ctx) {
    ctx->fiber   = ConvertThreadToFiber(NULL);
    ctx->started = 1;
}

/* ── sleep ───────────────────────────────────────────────────────────────── */
static inline void ty_sleep_ns(long ns) {
    /* Windows Sleep is millisecond resolution; round up */
    DWORD ms = (DWORD)((ns + 999999) / 1000000);
    if (ms == 0) ms = 1;
    Sleep(ms);
}

/* ── CPU count ───────────────────────────────────────────────────────────── */
static inline int ty_cpu_count(void) {
    SYSTEM_INFO si;
    GetSystemInfo(&si);
    return (int)si.dwNumberOfProcessors;
}

/* ── preemption (high-resolution timer + APC / thread-pool) ──────────────── */
/*
 * Windows doesn't have SIGPROF. We approximate preemptive yielding with a
 * single periodic timer that posts an APC to all worker threads.
 * APCs are delivered at alertable wait points; workers call
 * SleepEx(0, TRUE) in their idle loop to drain them.
 *
 * For a tighter preemption guarantee, replace with a thread-pool timer
 * that calls SuspendThread / GetThreadContext / SetThreadContext / ResumeThread
 * on each worker — but that's heavyweight and usually unnecessary.
 */
typedef void(*TyPreemptHandler)(void);
static TyPreemptHandler ty__preempt_cb   = NULL;
static HANDLE           ty__preempt_timer = NULL;

static VOID CALLBACK ty__timer_apc(ULONG_PTR param) {
    (void)param;
    if (ty__preempt_cb) ty__preempt_cb();
}

static VOID CALLBACK ty__timer_cb(PVOID param, BOOLEAN fired) {
    (void)param; (void)fired;
    /* Queue APC to every worker thread — approximated by the callback itself
     * running on a thread-pool thread; real workers check preempt_flag. */
    if (ty__preempt_cb) ty__preempt_cb();
}

static inline void ty_preempt_install(TyPreemptHandler cb, int hz) {
    ty__preempt_cb = cb;
    DWORD period_ms = (DWORD)(1000 / hz);
    CreateTimerQueueTimer(&ty__preempt_timer, NULL,
                          ty__timer_cb, NULL,
                          period_ms, period_ms,
                          WT_EXECUTEDEFAULT);
}
static inline void ty_preempt_stop(void) {
    if (ty__preempt_timer) {
        DeleteTimerQueueTimer(NULL, ty__preempt_timer, INVALID_HANDLE_VALUE);
        ty__preempt_timer = NULL;
    }
}

/* ── stderr write ────────────────────────────────────────────────────────── */
static inline void ty_stderr_write(const char* msg) {
    HANDLE h = GetStdHandle(STD_ERROR_HANDLE);
    DWORD  n = (DWORD)strlen(msg);
    WriteFile(h, msg, n, &n, NULL);
    WriteFile(h, "\n", 1, &n, NULL);
}

#endif /* TY_WINDOWS */

/* ── fast PRNG (no CRT dependency) ─────────────────────────────────────── */
/* xorshift64 — one state word per thread, seeded from thread id / address  */
static TY_THREAD_LOCAL uint64_t ty__rand_state = 0;

static inline void ty_rand_seed(uint64_t seed) {
    ty__rand_state = seed ? seed : 0x853c49e6748fea9bULL;
}

static inline uint32_t ty_rand(void) {
    /* xorshift64* — passes BigCrush, period 2^64-1 */
    uint64_t x = ty__rand_state;
    if (!x) x = (uint64_t)(uintptr_t)&x ^ 0xdeadbeefcafeULL;
    x ^= x >> 12;
    x ^= x << 25;
    x ^= x >> 27;
    ty__rand_state = x;
    return (uint32_t)((x * 0x2545f4914f6cdd1dULL) >> 32);
}

#endif /* TY_PLATFORM_H */
