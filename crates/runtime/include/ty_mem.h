/*
 * ty_mem.h — Typhoon memory public API
 *
 * Exports:
 *   - SlabArena memory management (per-coroutine allocation)
 *   - Buf (string builder)
 *   - TyArray (dynamic array)
 */

#pragma once
#include <stdint.h>
#include <stddef.h>

#ifdef __cplusplus
extern "C" {
#endif

/* ── Opaque arena type (defined in runtime.c) ───────────────────────────────── */

typedef struct SlabArena SlabArena;

/* ── Arena lifecycle ────────────────────────────────────────────────────────── */

SlabArena* slab_arena_new(void);
void       slab_arena_free(SlabArena* arena);

/* Helper: map allocation size to runtime size class (exported for scheduler) */
int32_t    size_to_class(size_t size);

/* ── Slab allocation (called from emitted LLVM IR) ──────────────────────────── */

void* slab_alloc(SlabArena* arena, int32_t size_class);
void  slab_free(SlabArena* arena, void* ptr, int32_t size_class);
void* slab_alloc_sized(SlabArena* arena, int64_t size);
/* Verify that ptr was allocated from arena and belongs to the current generation.
 * Returns 1 if valid, 0 if stale/NULL/wrong arena. Safe to call on any pointer. */
int slab_verify_generation(SlabArena* arena, void* ptr);

/*
 * TyStr — must match codegen.rs's "%struct.Str = type { i8*, i32 }"
 * field-for-field: field 0 = ptr, field 1 = len. Str is a fat pointer now,
 * not a null-terminated C string — no assumption here that ptr[len] is
 * '\0' (it usually is, since ty_buf_* keeps a trailing '\0' for any
 * remaining C interop, but nothing in this file relies on that anymore).
 */
typedef struct {
    char* ptr;
    int32_t len;
} TyStr;

/* ── Buf (string builder) ───────────────────────────────────────────────────── */

typedef struct Buf {
    char*   data;
    int64_t len;
    int64_t cap;
    int32_t heap_owned; /* 0 = normal arena-allocated Buf (the default —
                            every existing call site). 1 = allocated via
                            ty_buf_new_heap: plain malloc-backed, NOT tied
                            to any coroutine's SlabArena. Needed for Bufs
                            that cross a coroutine boundary through a
                            channel (e.g. ReadSocket.into_chan's chan<Buf>)
                            — SlabArena has no locking (by design, for
                            single-owner speed), so a Buf produced by one
                            coroutine and consumed by another on a
                            different OS worker thread cannot safely be
                            allocated from (or recycled back into) either
                            side's arena. ty_buf_into_str checks this flag
                            and dispatches to the malloc/free path instead
                            of arena_alloc/arena_free_slot when set. */
} Buf;

Buf*  ty_buf_new(SlabArena* arena);
Buf*  ty_buf_new_sized(SlabArena* arena, int64_t cap);
/* Thread-safe alternative to ty_buf_new_sized: no SlabArena involved,
 * safe to allocate on one coroutine/thread and free on another. Use for
 * any Buf that will cross a channel to a different coroutine. Pair with
 * ty_str_free_heap once the resulting TyStr (from ty_buf_into_str) is no
 * longer needed. */
Buf*  ty_buf_new_heap(int64_t cap);
void  ty_buf_push_str(SlabArena* arena, Buf* b, TyStr* s);
void  ty_buf_push_byte(SlabArena* arena, Buf* b, char c);
TyStr* ty_buf_into_str(SlabArena* arena, Buf* b);
/* Frees a TyStr produced by ty_buf_into_str() from a heap_owned Buf
 * (ty_buf_new_heap). Do NOT call this on a TyStr from an arena-owned
 * Buf — that memory belongs to the arena's bump pages, not malloc, and
 * freeing it here will corrupt the heap.
 * Chunks from into_chan are now heap-allocated (ty_buf_new_heap), not
 * arena-allocated, precisely so they're safe to hand across the
 * coroutine boundary the channel represents. ty_buf_into_str wraps them
 * into a TyStr as before, but that TyStr must be released with this
 * instead of just letting the arena reclaim it at teardown. */
void  ty_str_free_heap(TyStr* s);

int64_t ty_str_len(TyStr* s);
char    ty_str_byte(TyStr* s, int64_t idx);

/* ── TyArray (dynamic array) ────────────────────────────────────────────────── */

typedef struct TyArray {
    void*   data;
    int64_t len;
    int64_t cap;
    int64_t elem_size;
    int64_t elem_align;
} TyArray;

TyArray* ty_array_from_fixed(SlabArena* arena, void* data,
                              int64_t len, int64_t elem_size, int64_t elem_align);
void*    ty_array_get_ptr(TyArray* arr, int64_t idx);
void     ty_array_push(SlabArena* arena, TyArray* arr, void* elem_bytes);

#ifdef __cplusplus
}
#endif
