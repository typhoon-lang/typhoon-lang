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
#include <stdlib.h>
#include <errno.h>
#include <fcntl.h>
#include <sys/stat.h>

#ifdef _WIN32
#  define WIN32_LEAN_AND_MEAN
#  include <windows.h>
#  include <io.h>
#else
#  include <unistd.h>
#endif

#include "platform.h"
#include "io_driver.h"
#include "ty_io.h"
#include "ty_mem.h"
#include "ty_io_backend.h"
#include "scheduler.h"
#include "atomic.h"

/* TODO(zal): these belong in ty_io.h alongside TyFile/TyMode once that
 * header is updated for Task 3.1 — declared locally here because
 * ty_io.h wasn't in scope for this edit. */

#ifndef TY_STDIN_FD
#define TY_STDIN_FD 0 /* fallback if platform.h doesn't already define this */
#endif

/* ── Per-instance struct definitions ─────────────────────────────────────── */

struct TyStdout {
    Buf* buf;
};

struct TyStdin {
    Buf* pending; /* bytes read from fd 0 but not yet consumed */
    int64_t pos;  /* read cursor into pending->data */
    int eof;      /* fd 0 has returned EOF; no more reads will be attempted */
};

/* ════════════════════════════════════════════════════════════════════════════
 *  File — Task 3.1/3.2
 * ════════════════════════════════════════════════════════════════════════════ */

struct TyFile {
    int fd;
    int closed; /* debug guard, same pattern as Socket__close (Task 0.3) */
};

/*
 * Same cross-thread lifetime concern as Socket/Listener (see D5 exemption
 * in typhoon_io_redesign.md): a File opened by one coroutine can be
 * handed into a spawned `conc` worker that outlives the opening
 * coroutine's own task arena (Typhoon's spawn is fire-and-forget, not
 * structured-concurrency-joined — confirmed in the accept-loop pattern
 * in ty_net.c/the linked IR, which loops back immediately after spawning
 * without joining). Task-arena allocation would free the File out from
 * under a still-running spawned reader, same failure mode as the socket
 * bug. Pool it independently, same technique as ty_net.c's
 * TY_NET_DEFINE_POOL — duplicated locally since that macro isn't in a
 * shared header.
 */
#define TY_IO_POOL_CAP 4096 /* must be a power of 2 */

#define TY_IO_DEFINE_POOL(NAME, TYPE)                                        \
    static TYPE NAME##_slots[TY_IO_POOL_CAP];                                \
    static _Atomic(int) NAME##_in_use[TY_IO_POOL_CAP];                       \
    static _Atomic(int) NAME##_hint;                                         \
    static TYPE* NAME##_alloc(void) {                                        \
        int start = atomic_fetch_add_explicit(&NAME##_hint, 1,               \
            memory_order_relaxed) & (TY_IO_POOL_CAP - 1);                    \
        for (int i = 0; i < TY_IO_POOL_CAP; i++) {                           \
            int idx = (start + i) & (TY_IO_POOL_CAP - 1);                    \
            int exp = 0;                                                     \
            if (atomic_compare_exchange_strong_explicit(                     \
                    &NAME##_in_use[idx], &exp, 1,                            \
                    memory_order_acquire, memory_order_relaxed))             \
                return &NAME##_slots[idx];                                   \
        }                                                                    \
        return (TYPE*)malloc(sizeof(TYPE)); /* pool exhausted — fallback */  \
    }                                                                        \
    static void NAME##_free(TYPE* p) {                                      \
        if (!p) return;                                                      \
        uintptr_t base = (uintptr_t)NAME##_slots;                            \
        uintptr_t addr = (uintptr_t)p;                                       \
        if (addr < base || addr >= base + sizeof(NAME##_slots)) {            \
            free(p); return; /* came from the malloc fallback path */        \
        }                                                                    \
        size_t idx = (addr - base) / sizeof(TYPE);                           \
        atomic_store_explicit(&NAME##_in_use[idx], 0, memory_order_release); \
    }

TY_IO_DEFINE_POOL(g_file_pool, TyFile)

/*
 * Result shapes local to ty_io.c — mirrors ty_net.c's TyResult_i32_i32
 * shape (tag/value/err) but scoped here since ty_io.c doesn't include
 * ty_net.h. `err` is a raw errno/GetLastError code, matching the
 * codebase's actual current convention: Task 1.2's typed IoError enum
 * doesn't exist yet anywhere in the runtime (see typhoon_io_redesign.md
 * Phase 1, still open) — using it here would be referencing a type that
 * isn't real yet, so this follows what ty_net.c actually does today.
 */
typedef struct { int32_t tag; TyFile* value; int32_t err; } TyResult_FilePtr_i32;
typedef struct { int32_t tag; int64_t value; int32_t err; } TyResult_i64_i32;

/*
 * TyMode ordinals: ASSUMED — I don't have visibility into codegen.rs's
 * actual enum lowering for Mode::Read/Write/Append/ReadWrite/Create, so
 * these values are a guess at the ordinal order, not a confirmed ABI.
 * Verify against the compiler before relying on this in a real build.
 */
typedef enum {
    TY_MODE_READ = 0,
    TY_MODE_WRITE = 1,
    TY_MODE_APPEND = 2,
    TY_MODE_READ_WRITE = 3,
    TY_MODE_CREATE = 4,
} TyMode;

#ifdef _WIN32
static int ty_mode_to_flags(TyMode mode) {
    switch (mode) {
        case TY_MODE_READ:       return _O_RDONLY | _O_BINARY;
        case TY_MODE_WRITE:      return _O_WRONLY | _O_CREAT | _O_TRUNC | _O_BINARY;
        case TY_MODE_APPEND:     return _O_WRONLY | _O_CREAT | _O_APPEND | _O_BINARY;
        case TY_MODE_READ_WRITE: return _O_RDWR | _O_BINARY;
        case TY_MODE_CREATE:     return _O_RDWR | _O_CREAT | _O_TRUNC | _O_BINARY;
        default:                 return _O_RDONLY | _O_BINARY;
    }
}
#else
static int ty_mode_to_flags(TyMode mode) {
    switch (mode) {
        case TY_MODE_READ:       return O_RDONLY;
        case TY_MODE_WRITE:      return O_WRONLY | O_CREAT | O_TRUNC;
        case TY_MODE_APPEND:     return O_WRONLY | O_CREAT | O_APPEND;
        case TY_MODE_READ_WRITE: return O_RDWR;
        case TY_MODE_CREATE:     return O_RDWR | O_CREAT | O_TRUNC;
        default:                 return O_RDONLY;
    }
}
#endif

void __ty_rt__fs__open(void* task, TyStr* path, TyMode mode,
    TyResult_FilePtr_i32* out) {
    (void)task;
    TyResult_FilePtr_i32 result;
    result.tag = 1;
    result.value = NULL;
    result.err = -1;

    if (!path || !path->ptr) { *out = result; return; }

    int flags = ty_mode_to_flags(mode);

    /* open()/_open() require a null-terminated string. Str is a fat
     * pointer now, not guaranteed null-terminated in general — but every
     * current Str-producing path (string literals in codegen.rs's
     * emit_string, ty_buf_into_str) keeps a trailing '\0' at ptr[len] as
     * a byproduct of how Buf works. Relies on that invariant rather than
     * defensively copying. If Str slicing is ever added and can produce
     * a view that doesn't end at a '\0', this breaks silently. */
#ifdef _WIN32
    int fd = _open(path->ptr, flags, _S_IREAD | _S_IWRITE);
#else
    int fd = open(path->ptr, flags, 0644);
#endif
    if (fd < 0) {
        result.err = errno;
        *out = result;
        return;
    }

    TyFile* f = g_file_pool_alloc();
    if (!f) {
#ifdef _WIN32
        _close(fd);
#else
        close(fd);
#endif
        result.err = -1;
        *out = result;
        return;
    }
    f->fd = fd;
    f->closed = 0;

    result.tag = 0;
    result.value = f;
    result.err = 0;
    *out = result;
}

void __ty_rt__File__close(void* task, TyFile* self) {
    (void)task;
    if (!self) return;
    TY_ASSERT(!self->closed, "File__close called twice — liveness checker bug");
    self->closed = 1;

    if (self->fd >= 0) {
#ifdef _WIN32
        _close(self->fd);
#else
        close(self->fd);
#endif
        self->fd = -1;
    }
    g_file_pool_free(self);
}

/*
 * File__read / File__write — same shape as ty_net.c's Socket__read/write:
 * submit via the Phase 4 TyIoOp path when inside a coroutine (parks and
 * resumes on completion), fall back to a blocking syscall otherwise.
 * Doc's Task 3.2 pseudocode predates the unified Phase 4 IO backend and
 * described a File-specific submit path — this follows what Socket
 * actually does today instead, for consistency with the rest of the
 * runtime rather than the older per-subsystem design.
 */
void __ty_rt__File__read(void* task, TyFile* self, char* buf, int32_t cap,
    TyResult_i64_i32* out) {
    TyResult_i64_i32 result;
    result.tag = 1;
    result.value = 0;
    result.err = -1;

    if (!self || !buf) { *out = result; return; }

    void* coro = ty_current_coro_raw();
    if (coro) {
        /* Submit via TyIoOp — the canonical Phase 4 path.
         *
         * ty_io_submit parks the coroutine internally (both per-worker
         * backend and global driver paths). */
        TyIoOp op;
        memset(&op, 0, sizeof(op));
        op.type = TY_IO_OP_READ;
        op.fd = self->fd;
        op.buf = buf;
        op.len = (size_t)cap;
        op.coro = coro;
        ty_io_submit(&op);
        int64_t r = ty_io_take_result(coro);
        if (r < 0) {
            result.err = (int32_t)(-r);
            *out = result;
            return;
        }
        result.tag = 0;
        result.value = r;
        result.err = 0;
        *out = result;
        return;
    }

    /* sync fallback — outside coroutine context */
    int64_t n = ty_sys_read(self->fd, buf, (size_t)cap);
    if (n < 0) {
        result.err = errno;
        *out = result;
        return;
    }
    result.tag = 0;
    result.value = n;
    result.err = 0;
    *out = result;
}

void __ty_rt__File__write(void* task, TyFile* self, char* buf, int32_t len,
    TyResult_i64_i32* out) {
    (void)task;
    TyResult_i64_i32 result;
    result.tag = 1;
    result.value = 0;
    result.err = -1;

    if (!self || !buf) { *out = result; return; }

    void* coro = ty_current_coro_raw();
    if (coro) {
        /* ty_io_submit parks the coroutine internally. */
        TyIoOp op;
        memset(&op, 0, sizeof(op));
        op.type = TY_IO_OP_WRITE;
        op.fd = self->fd;
        op.buf = buf;
        op.len = (size_t)len;
        op.coro = coro;
        ty_io_submit(&op);
        int64_t r = ty_io_take_result(coro);
        if (r < 0) {
            result.err = (int32_t)(-r);
            *out = result;
            return;
        }
        result.tag = 0;
        result.value = r;
        result.err = 0;
        *out = result;
        return;
    }

    /* sync fallback — outside coroutine context */
    int64_t n = ty_sys_write(self->fd, buf, (size_t)len);
    if (n < 0) {
        result.err = errno;
        *out = result;
        return;
    }
    result.tag = 0;
    result.value = n;
    result.err = 0;
    *out = result;
}

/*
 * File__seek — lseek/SetFilePointer are always synchronous (no io_uring/
 * kqueue/IOCP path needed; there's no kernel completion to wait on).
 */
void __ty_rt__File__seek(void* task, TyFile* self, int64_t offset,
    int32_t whence, TyResult_i64_i32* out) {
    (void)task;
    TyResult_i64_i32 result;
    result.tag = 1;
    result.value = 0;
    result.err = -1;

    if (!self) { *out = result; return; }

#ifdef _WIN32
    LARGE_INTEGER li;
    li.QuadPart = offset;
    LARGE_INTEGER newpos;
    HANDLE h = (HANDLE)_get_osfhandle(self->fd);
    BOOL ok = SetFilePointerEx(h, li, &newpos, (DWORD)whence);
    if (!ok) {
        result.err = (int32_t)GetLastError();
        *out = result;
        return;
    }
    result.tag = 0;
    result.value = newpos.QuadPart;
    result.err = 0;
    *out = result;
#else
    off_t pos = lseek(self->fd, (off_t)offset, whence);
    if (pos < 0) {
        result.err = errno;
        *out = result;
        return;
    }
    result.tag = 0;
    result.value = (int64_t)pos;
    result.err = 0;
    *out = result;
#endif
}

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
    h->pending = ty_buf_new(arena);
    h->pos = 0;
    h->eof = 0;
    return h;
}

#define TY_STDIN_CHUNK 4096

/*
 * ty_stdin_read_line_buf — buffered line read from fd 0 (Task 3.4).
 *
 * Handles the two cases raw read() doesn't: a line split across
 * multiple reads, and multiple lines arriving in one read (leftover
 * bytes are kept in h->pending for the next call, not dropped).
 *
 * Internal only — see ty_stdin_read_line below for the Str-returning
 * function actually exposed to Typhoon.
 *
 * ASSUMES TY_STDIN_FD exists in platform.h analogous to the already-used
 * TY_STDOUT_FD — I don't have platform.h to confirm the exact name.
 */
static Buf* ty_stdin_read_line_buf(SlabArena* arena, TyStdin* h) {
    if (!h) return ty_buf_new(arena);

    for (;;) {
        /* Scan what's already buffered for a newline. */
        for (int64_t i = h->pos; i < h->pending->len; i++) {
            if (h->pending->data[i] == '\n') {
                int64_t line_len = i - h->pos;
                Buf* line = ty_buf_new_sized(arena, line_len);
                memcpy(line->data, h->pending->data + h->pos, (size_t)line_len);
                line->len = line_len;
                line->data[line_len] = '\0';
                h->pos = i + 1;
                return line;
            }
        }

        if (h->eof) {
            /* No newline left, and no more data will ever arrive.
             * Return whatever's buffered as the final line (possibly
             * empty, if the stream ended exactly at a newline). */
            int64_t remaining = h->pending->len - h->pos;
            if (remaining <= 0) return ty_buf_new(arena);
            Buf* line = ty_buf_new_sized(arena, remaining);
            memcpy(line->data, h->pending->data + h->pos, (size_t)remaining);
            line->len = remaining;
            line->data[remaining] = '\0';
            h->pos = h->pending->len;
            return line;
        }

        /* Need more data: compact unread bytes to the front of a fresh
         * buffer, then read another chunk from fd 0. */
        int64_t unread = h->pending->len - h->pos;
        Buf* fresh = ty_buf_new_sized(arena, unread + TY_STDIN_CHUNK);
        if (unread > 0)
            memcpy(fresh->data, h->pending->data + h->pos, (size_t)unread);

        int64_t got = ty_sys_read(TY_STDIN_FD, fresh->data + unread, TY_STDIN_CHUNK);
        if (got <= 0) {
            h->eof = 1;
            fresh->len = unread;
        } else {
            fresh->len = unread + got;
        }
        fresh->data[fresh->len] = '\0';

        h->pending = fresh;
        h->pos = 0;
    }
}

/*
 * ty_stdin_read_line — Str-returning wrapper exposed to Typhoon.
 *
 * KNOWN LIMITATION: EOF and a genuine empty line both return a
 * zero-length Str ("") — there's no confirmed nullable/Option<Str>
 * mechanism visible in what I have to distinguish them cleanly. A
 * caller that needs to tell "stream ended" apart from "blank line"
 * can't do so through this function alone yet. Flagging rather than
 * guessing at Option<Str> construction syntax I have no evidence for.
 */
TyStr* ty_stdin_read_line(SlabArena* arena, TyStdin* h) {
    Buf* line = ty_stdin_read_line_buf(arena, h);
    return ty_buf_into_str(arena, line);
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
    /* fd here is a CRT file descriptor from _open() (File I/O is the
     * only non-console caller of this function on Windows — ty_net.c's
     * sockets go through their own recv/send/IOCP path entirely, never
     * through ty_sys_write/read). A CRT fd is a small integer indexing
     * the C runtime's own fd table; it is NOT a Win32 HANDLE and can't
     * be produced by just casting the integer. That cast is what was
     * here before: `(HANDLE)(uintptr_t)(unsigned int)fd`, which handed
     * WriteFile something like literal handle value 3 — an invalid
     * handle — so every real-file write failed with ERROR_INVALID_HANDLE
     * and _sys_write returned -1. _get_osfhandle() does the real
     * fd-to-HANDLE lookup in the CRT's table. */
    HANDLE h = (HANDLE)_get_osfhandle(fd);
    if (h == INVALID_HANDLE_VALUE) return -1;
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
    /* Same fix as ty_sys_write above — fd is a CRT fd from _open(),
     * needs _get_osfhandle() before it's a valid HANDLE for ReadFile. */
    HANDLE h = (HANDLE)_get_osfhandle(fd);
    if (h == INVALID_HANDLE_VALUE) return -1;
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
