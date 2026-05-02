/*
 * ty_atomic.h — C11 stdatomic shim for MSVC
 *
 * On every compiler that supports C11 atomics (GCC, Clang, TCC) this header
 * just includes <stdatomic.h> and does nothing else.
 *
 * On MSVC, <stdatomic.h> does not exist.  We implement the exact subset used
 * by scheduler.c using MSVC's <intrin.h> intrinsics:
 *
 *   Types used
 *   ──────────
 *     _Atomic(int)       → volatile LONG
 *     _Atomic(long)      → volatile LONG  (long == 32-bit on MSVC/x64)
 *     _Atomic(size_t)    → volatile LONGLONG  (size_t == 64-bit on x64)
 *     _Atomic(void**)    → volatile void* (pointer-sized)
 *     _Atomic(T*)        → volatile T*    (pointer-sized)
 *     _Atomic(CoroState) → volatile LONG  (enum, fits int)
 *
 *   Operations used (all map to _Interlocked* or compiler barriers)
 *   ──────────────────────────────────────────────────────────────
 *     atomic_init(obj, val)
 *     atomic_load_explicit(obj, order)
 *     atomic_store_explicit(obj, val, order)
 *     atomic_thread_fence(order)
 *     atomic_compare_exchange_strong_explicit(obj,exp,des,succ,fail)
 *     atomic_compare_exchange_weak_explicit(obj,exp,des,succ,fail)
 *     atomic_fetch_add_explicit(obj, val, order)
 *     atomic_fetch_sub_explicit(obj, val, order)
 *
 *   memory_order values used
 *   ─────────────────────────
 *     memory_order_relaxed   — no fence
 *     memory_order_acquire   — _ReadBarrier / LoadAcquire
 *     memory_order_release   — _WriteBarrier / StoreRelease
 *     memory_order_acq_rel   — _ReadWriteBarrier
 *     memory_order_seq_cst   — MemoryBarrier (full fence)
 *     memory_order_consume   — treated as acquire (safe over-approximation)
 */

#ifndef TY_ATOMIC_H
#define TY_ATOMIC_H

/* ── Non-MSVC: just use the real thing ─────────────────────────────────── */
#ifndef _MSC_VER
#  include <stdatomic.h>

#else /* MSVC ────────────────────────────────────────────────────────────── */

#include <stddef.h>   /* size_t */
#include <stdint.h>
#include <intrin.h>

/* ── __forceinline compat ───────────────────────────────────────────────── */
#ifndef __forceinline
#  ifdef __GNUC__
#    define __forceinline __attribute__((always_inline)) inline
#  else
#    define __forceinline inline
#  endif
#endif

/* ── memory_order enum ──────────────────────────────────────────────────── */
typedef enum {
    memory_order_relaxed = 0,
    memory_order_consume = 1,
    memory_order_acquire = 2,
    memory_order_release = 3,
    memory_order_acq_rel = 4,
    memory_order_seq_cst = 5
} memory_order;

/* ── fence ──────────────────────────────────────────────────────────────── */
static __forceinline void atomic_thread_fence(memory_order order) {
    switch (order) {
    case memory_order_relaxed:
        break;
    case memory_order_acquire:
    case memory_order_consume:
        _ReadBarrier();
        break;
    case memory_order_release:
        _WriteBarrier();
        break;
    case memory_order_acq_rel:
        _ReadWriteBarrier();
        break;
    case memory_order_seq_cst:
    default:
        MemoryBarrier();
        break;
    }
}

/* ── _Atomic(T) type wrapper ─────────────────────────────────────────────
 *
 * On MSVC we spell _Atomic(T) as TyAtomic_##mangled.
 * Because C doesn't support templates and _Atomic is a keyword we can't
 * redefine, we instead define _Atomic as a macro that expands to `volatile`
 * for the types we care about.  Volatile is NOT sufficient for correctness
 * on its own, but every actual access goes through the intrinsic wrappers
 * below which emit the right barriers — volatile just prevents the compiler
 * from caching values in registers between those calls.
 *
 * Limitation: this means _Atomic(T) is not a distinct type on MSVC; it's
 * just `volatile T`.  That is fine for our usage because we never rely on
 * the type being distinct (no overloaded generics in C).
 */
#define _Atomic(T)  volatile T

/* ── atomic_init ────────────────────────────────────────────────────────── */
/* Just a plain store; safe at program startup before any threads exist. */
#define atomic_init(obj, val)  (*(obj) = (val))

/* ── internal fence helpers ──────────────────────────────────────────────── */
static __forceinline void ty__acq_fence(memory_order o) {
    if (o >= memory_order_acquire) _ReadBarrier();
}
static __forceinline void ty__rel_fence(memory_order o) {
    if (o >= memory_order_release) _WriteBarrier();
}
static __forceinline void ty__seq_fence(memory_order o) {
    if (o == memory_order_seq_cst) MemoryBarrier();
}

/* ── atomic_load_explicit ───────────────────────────────────────────────── */
/*
 * For 32-bit int/long/enum: plain volatile read + acquire barrier if needed.
 * For 64-bit (size_t, pointers on x64): same — volatile 64-bit reads are
 * atomic on x64 as long as they are naturally aligned (which they always are
 * for stack/global variables).
 */
#define atomic_load_explicit(obj, order)                    \
    ( ty__acq_fence(order), *(obj) )

/* ── atomic_store_explicit ──────────────────────────────────────────────── */
#define atomic_store_explicit(obj, val, order)              \
    do { ty__rel_fence(order); *(obj) = (val);              \
         ty__seq_fence(order); } while(0)

/* ── atomic_fetch_add_explicit (int / LONG) ─────────────────────────────── */
static __forceinline long
ty__fetch_add32(volatile long* obj, long val, memory_order order) {
    (void)order;
    return _InterlockedExchangeAdd(obj, val);
}
static __forceinline long long
ty__fetch_add64(volatile long long* obj, long long val, memory_order order) {
    (void)order;
    return _InterlockedExchangeAdd64(obj, val);
}

/* Dispatch on size: int/long → 32-bit path; size_t/pointer → 64-bit path. */
#define atomic_fetch_add_explicit(obj, val, order)                          \
    ( (void)sizeof(char[ sizeof(*(obj)) == 4 || sizeof(*(obj)) == 8 ]),   \
      sizeof(*(obj)) == 8                                                   \
      ? (long long)ty__fetch_add64(                                        \
            (volatile long long*)(obj), (long long)(val), (order))         \
      : (long)ty__fetch_add32(                                             \
            (volatile long*)(obj), (long)(val), (order)) )

/* ── atomic_fetch_sub_explicit ──────────────────────────────────────────── */
#define atomic_fetch_sub_explicit(obj, val, order) \
    atomic_fetch_add_explicit(obj, -(val), order)

/* ── CAS helpers ────────────────────────────────────────────────────────── */
/*
 * _InterlockedCompareExchange(dst, exchange, comparand):
 *   if (*dst == comparand) { *dst = exchange; return comparand; }
 *   else return *dst;
 * Returns the OLD value regardless of success.
 * We want: returns 1 on success (C11 semantics), writes old into *expected.
 */
static __forceinline int
ty__cas32(volatile long* obj, long* expected, long desired,
          memory_order succ, memory_order fail) {
    (void)succ; (void)fail;
    long old = _InterlockedCompareExchange(obj, desired, *expected);
    if (old == *expected) return 1;
    *expected = old; return 0;
}

static __forceinline int
ty__cas64(volatile long long* obj, long long* expected, long long desired,
          memory_order succ, memory_order fail) {
    (void)succ; (void)fail;
    long long old = _InterlockedCompareExchange64(obj, desired, *expected);
    if (old == *expected) return 1;
    *expected = old; return 0;
}

static __forceinline int
ty__cas_ptr(void* volatile* obj, void** expected, void* desired,
            memory_order succ, memory_order fail) {
    (void)succ; (void)fail;
    void* old = _InterlockedCompareExchangePointer(obj, desired, *expected);
    if (old == *expected) return 1;
    *expected = old; return 0;
}

/*
 * Unified CAS macro.  Dispatches to the right width based on sizeof(*obj).
 * Both strong and weak are identical on x86/x64 (no spurious failures on TSO).
 */
#define TY__CAS(obj, expected, desired, succ, fail)                         \
    ( sizeof(*(obj)) == 8                                                   \
      ? ty__cas64((volatile long long*)(obj),                              \
                  (long long*)(expected), (long long)(desired), succ, fail) \
      : sizeof(*(obj)) == sizeof(void*)                                     \
        ? ty__cas_ptr((void* volatile*)(void*)(obj),                       \
                      (void**)(expected), (void*)(desired), succ, fail)    \
        : ty__cas32((volatile long*)(obj),                                 \
                    (long*)(expected), (long)(desired), succ, fail) )

#define atomic_compare_exchange_strong_explicit(obj,exp,des,succ,fail) \
    TY__CAS(obj, exp, des, succ, fail)

#define atomic_compare_exchange_weak_explicit(obj,exp,des,succ,fail)   \
    TY__CAS(obj, exp, des, succ, fail)

#endif /* _MSC_VER */
#endif /* TY_ATOMIC_H */
