/*
 * ty_io.h — Public interface for the Typhoon I/O subsystem.
 *
 * Consumers must link against ty_io.c and its transitive deps:
 *   platform.h / io_driver.h / atomic.h / ty_mem.h / scheduler.h
 *
 * All functions that take a `SlabArena*` or `void* task` parameter operate on
 * the calling coroutine's arena.  Pass NULL only in unit tests that do not
 * need arena-backed allocation (ty_sscan falls back to malloc in that case).
 *
 * Formatted output uses a slab-backed growable buffer. Functions that return
 * int use negative values for write/format failures (e.g. arena OOM).
 */

#ifndef TY_IO_H
#define TY_IO_H

#include <stdint.h>
#include <stddef.h>
#include <stdarg.h>

#ifdef __cplusplus
extern "C" {
#endif

/* ── Opaque types (defined in ty_mem.h / ty_io.c) ──────────────────────── */

typedef struct SlabArena SlabArena;
typedef struct Buf       Buf;

/* ── Well-known file descriptors ────────────────────────────────────────── */

#define TY_STDIN_FD   0
#define TY_STDOUT_FD  1
#define TY_STDERR_FD  2

/* ══════════════════════════════════════════════════════════════════════════
 *  PRINT family — write to stdout / arbitrary fd / Buf
 * ══════════════════════════════════════════════════════════════════════════ */

/*
 * ty_print / ty_println — write a raw string to stdout.
 * Return void: plain strings perform direct writes (no formatting buffer).
 */
void ty_print  (SlabArena* arena, char* s);
void ty_println(SlabArena* arena, char* s);

/*
 * ty_printf — fixed-arity formatted print to stdout.
 * Returns number of bytes written, or -1 on formatting/write failure.
 * Accepts exactly 4 uint64_t args; unused slots should be 0.
 */
int ty_printf(SlabArena* arena, char* fmt,
              uint64_t a1, uint64_t a2, uint64_t a3, uint64_t a4);

/*
 * ty_fprint / ty_fprintln — write a raw string to an arbitrary fd.
 * Returns number of bytes written, or -1 on write failure.
 */
int ty_fprint  (SlabArena* arena, int fd, char* s);
int ty_fprintln(SlabArena* arena, int fd, char* s);

/*
 * ty_fprintf — formatted print to an arbitrary fd.
 * Returns number of bytes written, or -1 on formatting/write failure.
 */
int ty_fprintf(SlabArena* arena, int fd, char* fmt, ...);

/*
 * ty_sprint / ty_sprintln / ty_sprintf — append to a Buf in the arena.
 * The Buf grows via ty_buf_push_str; no overflow flag applies here.
 */
void ty_sprint  (SlabArena* arena, Buf* out, char* s);
void ty_sprintln(SlabArena* arena, Buf* out, char* s);
int  ty_sprintf (SlabArena* arena, Buf* out, char* fmt, ...);

/* ══════════════════════════════════════════════════════════════════════════
 *  SCAN family — read tokens / formatted data
 *
 *  scan/fscan    — one whitespace-delimited token, returned as arena-owned str
 *  scanf/fscanf  — formatted read from stdin / fd  (fd-based)
 *  sscan/sscanf  — formatted read from an in-memory Str
 *
 *  All sscan/sscanf functions take a `void* task` (= SlabArena*).
 *  %s results are arena-allocated and live until slab_arena_free.
 * ══════════════════════════════════════════════════════════════════════════ */

/* Read one token from stdin/fd; returns arena-owned char*, or NULL on EOF. */
char* ty_scan (SlabArena* arena);
char* ty_fscan(SlabArena* arena, int fd);

/*
 * Read formatted input from stdin / fd.
 * Returns number of items matched; task is currently unused (reserved for
 * future %s arena allocation in the fd path).
 */
int ty_scanf (void* task, char* fmt, ...);
int ty_fscanf(void* task, int fd, char* fmt, ...);

/*
 * ty_sscan — read one whitespace-delimited token from an immutable Str.
 *
 * `src`      — pointer into source string (not mutated).
 * `rest_out` — if non-NULL, set to the position in `src` after the token.
 *
 * Returns an arena-allocated (task != NULL) or malloc'd (task == NULL) copy
 * of the token, or NULL if `src` contains no non-whitespace characters.
 *
 * The returned pointer is valid for the lifetime of the coroutine's arena.
 */
char* ty_sscan(void* task, const char* src, const char** rest_out);

/*
 * ty_sscanf — formatted scan from an immutable Str.
 *
 * Supported specifiers: %s %c %d %i %u %ld %li %lu %lld %lli %llu
 *                       %f %lf %g %e %x %X %o
 * %s results are arena-allocated; all others fill caller-supplied pointers.
 * Returns number of items successfully matched.
 */
int ty_sscanf(void* task, char* src, char* fmt, ...);

#ifdef __cplusplus
}
#endif

#endif /* TY_IO_H */
