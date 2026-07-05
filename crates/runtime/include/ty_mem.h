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
} Buf;

Buf*  ty_buf_new(SlabArena* arena);
Buf*  ty_buf_new_sized(SlabArena* arena, int64_t cap);
void  ty_buf_push_str(SlabArena* arena, Buf* b, TyStr* s);
void  ty_buf_push_byte(SlabArena* arena, Buf* b, char c);
TyStr* ty_buf_into_str(SlabArena* arena, Buf* b);

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
