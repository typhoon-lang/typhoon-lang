/*
 * ty_io.h — Public interface for the Typhoon I/O subsystem.
 *
 * Thin syscall wrappers only. All formatting/buffering is done in the
 * Typhoon stdlib (std::io). C side provides:
 *   - Per-instance Stdout/Stdin handle constructors
 *   - Raw sys_write / sys_read (thin wrappers around OS syscalls)
 *
 * No malloc — heap via SlabArena.
 */

#ifndef TY_IO_H
#define TY_IO_H

#include <stdint.h>
#include <stddef.h>

#ifdef __cplusplus
extern "C" {
#endif

/* ── Opaque types (defined in ty_mem.h / ty_io.c) ──────────────────────── */

typedef struct SlabArena SlabArena;
typedef struct Buf       Buf;
typedef struct TyFile    TyFile;

/* ── Per-instance I/O handles ────────────────────────────────────────────── */

typedef struct TyStdout TyStdout;
typedef struct TyStdin  TyStdin;

TyStdout* ty_stdout_new(SlabArena* arena);
void      ty_stdout_flush(SlabArena* arena, TyStdout* h);
Buf*      ty_stdout_getbuf(SlabArena* arena, TyStdout* h);
TyStdin*  ty_stdin_new(SlabArena* arena);

/* ── Well-known file descriptors ────────────────────────────────────────── */

#define TY_STDIN_FD   0
#define TY_STDOUT_FD  1
#define TY_STDERR_FD  2

/* ── Raw syscalls ────────────────────────────────────────────────────────── */

int64_t ty_sys_write(int fd, const char* buf, size_t len);
int64_t ty_sys_read (int fd, char* buf, size_t len);

#ifdef __cplusplus
}
#endif

#endif /* TY_IO_H */
