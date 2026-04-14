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

/* ── Slab allocation (called from emitted LLVM IR) ──────────────────────────── */

void* slab_alloc(SlabArena* arena, int32_t size_class);
void  slab_free(SlabArena* arena, void* ptr, int32_t size_class);

/* ── Buf (string builder) ───────────────────────────────────────────────────── */

typedef struct Buf {
    char*   data;
    int64_t len;
    int64_t cap;
} Buf;

Buf*  ty_buf_new(SlabArena* arena);
void  ty_buf_push_str(SlabArena* arena, Buf* b, char* s);
char* ty_buf_into_str(SlabArena* arena, Buf* b);

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
