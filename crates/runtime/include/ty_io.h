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
#include "ty_mem.h"

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

/* ── File System ─────────────────────────────────────────────────────────── */

typedef enum {
    TY_MODE_READ = 0,
    TY_MODE_WRITE = 1,
    TY_MODE_APPEND = 2,
    TY_MODE_READ_WRITE = 3,
    TY_MODE_CREATE = 4,
} TyMode;

typedef struct { int32_t tag; TyFile* ok; int32_t err; } TyResult_File_i32;
typedef struct { int32_t tag; int32_t ok; int32_t err; } TyResult_i32;
typedef struct { int32_t tag; int64_t value; int32_t err; } TyResult_i64_i32;

void __ty_rt__fs__open(void* task, TyStr* path, TyMode mode, TyResult_File_i32* out);
void __ty_rt__File__close(void* task, TyFile* self);
void __ty_rt__File__read(void* task, TyFile* self, char* buf, int32_t cap, TyResult_i32* out);
void __ty_rt__File__write(void* task, TyFile* self, TyStr* content, TyResult_i32* out);

#ifdef __cplusplus
}
#endif

#endif /* TY_IO_H */
