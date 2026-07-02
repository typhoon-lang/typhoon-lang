/*
 * ty_io.c — Typhoon I/O subsystem (C-only, no Rust)
 *
 * Thin syscall wrappers only. All formatting/buffering done in Typhoon stdlib.
 *
 * Per-instance handles:
 *   TyStdout — holds Buf* for accumulated output
 *   TyStdin  — placeholder (reserved for future buffered read state)
 *
 * Syscall wrappers:
 *   ty_sys_write(fd, buf, len)  — raw write to fd
 *   ty_sys_read (fd, buf, len)  — raw read from fd
 *
 * No malloc — heap via SlabArena (slab_alloc / slab_free).
 */

#include <stdint.h>
#include <stddef.h>
#include <string.h>

#include "platform.h"
#include "ty_io.h"
#include "ty_mem.h"

/* ── Per-instance struct definitions ─────────────────────────────────────── */

struct TyStdout {
    Buf* buf;
};

struct TyStdin {
    char _reserved; /* placeholder for future buffered-read state */
};

/* ════════════════════════════════════════════════════════════════════════════
 *  Per-instance I/O handles
 * ════════════════════════════════════════════════════════════════════════════ */

TyStdout* ty_stdout_new(SlabArena* arena) {
    int32_t cls_h = size_to_class(sizeof(TyStdout));
    TyStdout* h = (TyStdout*)slab_alloc(arena, cls_h);
    h->buf   = ty_buf_new(arena);
    return h;
}

void ty_stdout_resetbuf(TyStdout* h) {
    if (!h || !h->buf) return;
    h->buf->len = 0;
    h->buf->data[0] = '\0';
}

Buf* ty_stdout_getbuf(SlabArena* arena, TyStdout* h) {
    (void)arena;
    return h ? h->buf : NULL;
}

void ty_stdout_flush(SlabArena* arena, TyStdout* h) {
    (void)arena;
    if (!h || !h->buf) return;
    if (h->buf->len > 0) {
        ty_sys_write(TY_STDOUT_FD, h->buf->data, (size_t)h->buf->len);
    }
    h->buf->len = 0;
    h->buf->data[0] = '\0';
}

TyStdin* ty_stdin_new(SlabArena* arena) {
    int32_t cls_h = size_to_class(sizeof(TyStdin));
    TyStdin* h = (TyStdin*)slab_alloc(arena, cls_h);
    return h;
}

/* ════════════════════════════════════════════════════════════════════════════
 *  Raw syscalls
 * ════════════════════════════════════════════════════════════════════════════ */

#ifdef _WIN32
#  define WIN32_LEAN_AND_MEAN
#  include <windows.h>
#  include <io.h>

int64_t ty_sys_write(int fd, const char* buf, size_t len) {
    if (len == 0) return 0;
    if (fd == 1 || fd == 2) {
        HANDLE h = (fd == 1) ? GetStdHandle(STD_OUTPUT_HANDLE) : GetStdHandle(STD_ERROR_HANDLE);
        if (h == INVALID_HANDLE_VALUE) return -1;
        DWORD mode = 0;
        if (!GetConsoleMode(h, &mode)) {
            int n = _write(fd, buf, (unsigned int)len);
            return (int64_t)n;
        }
        DWORD written = 0;
        BOOL ok = WriteConsoleA(h, buf, (DWORD)len, &written, NULL);
        return ok ? (int64_t)written : -1;
    }
    if (fd == 0) {
        int n = _write(fd, buf, (unsigned int)len);
        return (int64_t)n;
    }
    HANDLE h = (HANDLE)(uintptr_t)(unsigned int)fd;
    DWORD written = 0;
    BOOL ok = WriteFile(h, buf, (DWORD)len, &written, NULL);
    return ok ? (int64_t)written : -1;
}

int64_t ty_sys_read(int fd, char* buf, size_t len) {
    if (len == 0) return 0;
    if (fd == 0) {
        HANDLE h = GetStdHandle(STD_INPUT_HANDLE);
        if (h == INVALID_HANDLE_VALUE) return -1;
        DWORD mode = 0;
        if (!GetConsoleMode(h, &mode)) {
            int n = _read(fd, buf, (unsigned int)len);
            return (int64_t)n;
        }
        DWORD got = 0;
        BOOL ok = ReadConsoleA(h, buf, (DWORD)len, &got, NULL);
        return ok ? (int64_t)got : -1;
    }
    if (fd == 1 || fd == 2) {
        int n = _read(fd, buf, (unsigned int)len);
        return (int64_t)n;
    }
    HANDLE h = (HANDLE)(uintptr_t)(unsigned int)fd;
    DWORD got = 0;
    BOOL ok = ReadFile(h, buf, (DWORD)len, &got, NULL);
    return ok ? (int64_t)got : -1;
}

#else /* POSIX */
#  include <unistd.h>
#  include <errno.h>

int64_t ty_sys_write(int fd, const char* buf, size_t len) {
    if (len == 0) return 0;
    ssize_t n;
    do { n = write(fd, buf, len); } while (n < 0 && errno == EINTR);
    return (int64_t)n;
}

int64_t ty_sys_read(int fd, char* buf, size_t len) {
    if (len == 0) return 0;
    ssize_t n;
    do { n = read(fd, buf, len); } while (n < 0 && errno == EINTR);
    return (int64_t)n;
}
#endif