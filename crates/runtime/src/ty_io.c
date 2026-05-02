/*
 * ty_io.c — Typhoon I/O subsystem (C-only, no Rust)
 *
 * Provides:
 *   - C-only I/O driver (blocking, platform-independent)
 *   - Formatted I/O (printf, scanf family)
 *
 * I/O intrinsics emitted by compiler:
 *   print, println, printf, fprint, fprintln, fprintf,
 *   sprint, sprintln, sprintf, scan, scanf, fscan, fscanf,
 *   sscan, sscanf
 *
 * All I/O uses blocking platform.h functions (ty_read_fd, ty_write_fd).
 * Format strings: %s %c %d %i %u %ld %li %lu %lld %lli %llu
 *                 %f %lf %g %e %x %X %o %b %%
 *                 Width/precision: %5d %.3f %-10s %05d
 *
 * No malloc — all heap via SlabArena. Stack buffers for formatting.
 */

#include <stdint.h>
#include <stddef.h>
#include <stdarg.h>
#include <stdlib.h>
#include <string.h>

#include "platform.h"
#include "io_driver.h"
#include "atomic.h"
#include "ty_mem.h"
#include "scheduler.h"

/* ── hard abort
 * ──────────────────────────────────────────────────────────────── */

static void io_abort(void) { TY_TRAP(); }

/* ═══════════════════════════════════════════════════════════════════════════════
 *  C-only I/O subsystem (no Rust FFI)
 * ═══════════════════════════════════════════════════════════════════════════════
 */

/* ── Forward decls for internal platform functions ──────────────────────────
 */

static int64_t io_sys_read(int fd, char* buf, size_t len);
static int64_t io_sys_write(int fd, const char* buf, size_t len);

/* ─────────────────────────────────────────────────────────────────────────────
 * Portable low-level I/O primitives
 *
 * These replace the phantom ty_write_fd / ty_read_fd / ty_snprintf / ty_strtod
 * symbols that used to be assumed in platform.h.  All four are implemented
 * here directly so ty_io.c has zero unresolved externals beyond the runtime
 * and scheduler symbols it already depends on.
 * ─────────────────────────────────────────────────────────────────────────────
 */

#ifdef _WIN32
#  define WIN32_LEAN_AND_MEAN
#  include <windows.h>
#  include <io.h>       /* _write, _read */

static int64_t io_sys_write(int fd, const char* buf, size_t len) {
    if (fd == 1 || fd == 2) {
        /* stdout/stderr — use WriteConsoleA for proper console output */
        HANDLE h = (fd == 1) ? GetStdHandle(STD_OUTPUT_HANDLE) : GetStdHandle(STD_ERROR_HANDLE);
        if (h == INVALID_HANDLE_VALUE) return -1;
        DWORD written = 0;
        BOOL ok = WriteConsoleA(h, buf, (DWORD)len, &written, NULL);
        return ok ? (int64_t)written : -1;
    }
    if (fd == 0) {
        /* stdin — use CRT _write (shouldn't happen) */
        int n = _write(fd, buf, (unsigned int)len);
        return (int64_t)n;
    }
    /* ty_io_open returns a HANDLE cast to int for non-stdio fds */
    HANDLE h = (HANDLE)(uintptr_t)(unsigned int)fd;
    DWORD written = 0;
    BOOL ok = WriteFile(h, buf, (DWORD)len, &written, NULL);
    return ok ? (int64_t)written : -1;
}

static int64_t io_sys_read(int fd, char* buf, size_t len) {
    if (fd == 0) {
        /* stdin — use ReadConsoleA for console input */
        HANDLE h = GetStdHandle(STD_INPUT_HANDLE);
        if (h == INVALID_HANDLE_VALUE) return -1;
        DWORD got = 0;
        BOOL ok = ReadConsoleA(h, buf, (DWORD)len, &got, NULL);
        return ok ? (int64_t)got : -1;
    }
    if (fd == 1 || fd == 2) {
        /* shouldn't read from stdout/stderr, but handle anyway */
        int n = _read(fd, buf, (unsigned int)len);
        return (int64_t)n;
    }
    /* ty_io_open returns a HANDLE cast to int for non-stdio fds */
    HANDLE h = (HANDLE)(uintptr_t)(unsigned int)fd;
    DWORD got = 0;
    BOOL ok = ReadFile(h, buf, (DWORD)len, &got, NULL);
    return ok ? (int64_t)got : -1;
}

#else /* POSIX */
#  include <unistd.h>   /* read, write  */
#  include <errno.h>

static int64_t io_sys_write(int fd, const char* buf, size_t len) {
    ssize_t n;
    do { n = write(fd, buf, len); } while (n < 0 && errno == EINTR);
    return (int64_t)n;
}

static int64_t io_sys_read(int fd, char* buf, size_t len) {
    ssize_t n;
    do {
        n = read(fd, buf, len);
    } while (n < 0 && errno == EINTR);
    return (int64_t)n;
}
#endif /* _WIN32 */

/* snprintf wrapper — MSVC handling */
#ifdef _MSC_VER
    /* For MSVC 2015 (1900) and later, snprintf is standard.
       For older versions, we must use _snprintf. */
#   if _MSC_VER < 1900
#       define io_snprintf _snprintf
#   else
/* Even in newer MSVC, we sometimes need to include the
   legacy definitions if the linker isn't finding the UCRT version. */
#       include <stdio.h>
#       define io_snprintf snprintf
#   endif
#else
/* POSIX / GCC / Clang */
#   include <stdio.h>
#   define io_snprintf snprintf
#endif

/* strtod wrapper — standard in both POSIX and MSVC */
static double io_strtod(const char* s, char** end) {
    return strtod(s, end);
}

/* ════════════════════════════════════════════════════════════════════════════
 *  Low-level async-aware I/O helpers
 * ════════════════════════════════════════════════════════════════════════════
 */

/*
 * io_do_write
 *   Write `len` bytes from `buf` to `fd`.
 *   Uses the async driver when inside a coroutine, otherwise blocking libc.
 *   Retries on EINTR / short writes.
 */
static int64_t io_do_write(int fd, const char* buf, size_t len) {
    void* driver = ty_io_global_driver();
    void* task   = ty_current_arena();    /* SlabArena* — NULL before sched */
    void* coro   = ty_current_coro_raw(); /* TyCoro*    — NULL outside coro  */

    if (driver && coro) {
        /* async path */
        ty_io_write(driver, task, coro, fd, (const uint8_t*)buf, len);
        int64_t n = ty_io_take_result(coro);
        return n;
    }

    /* synchronous fallback (main thread / before scheduler) */
    size_t sent = 0;
    while (sent < len) {
        int64_t n = io_sys_write(fd, buf + sent, len - sent);
        if (n < 0) return n;
        if (n == 0) break;
        sent += (size_t)n;
    }
    return (int64_t)sent;
}

/*
 * io_do_read
 *   Read up to `len` bytes into `buf` from `fd`.
 *   Returns bytes read (≥ 0) or negative errno.
 */
static int64_t io_do_read(int fd, char* buf, size_t len) {
    void* driver = ty_io_global_driver();
    void* task   = ty_current_arena();
    void* coro   = ty_current_coro_raw();

    if (driver && coro) {
        ty_io_read(driver, task, coro, fd, (uint8_t*)buf, len);
        return ty_io_take_result(coro);
    }

    return io_sys_read(fd, buf, len);
}

/* ── fd constants ───────────────────────────────────────────────────────────
 */

#define TY_STDIN_FD   0
#define TY_STDOUT_FD  1
#define TY_STDERR_FD  2

/* ════════════════════════════════════════════════════════════════════════════
 *  Minimal printf formatter
 *  All output is accumulated into a local Buf then flushed in one write.
 * ════════════════════════════════════════════════════════════════════════════
 */

/* ── stack Buf (no arena needed for ephemeral output)
 * ─────────────────────────
 */

#define STACK_BUF_CAP 4096

typedef struct {
    char   data[STACK_BUF_CAP];
    size_t len;
    int    overflow;   /* if 1, data was truncated */
} StackBuf;

static void sbuf_init(StackBuf* b) { b->len = 0; b->overflow = 0; }

static void sbuf_push(StackBuf* b, const char* s, size_t n) {
    if (b->overflow) return;
    if (b->len + n >= STACK_BUF_CAP) {
        n = STACK_BUF_CAP - b->len - 1;
        b->overflow = 1;
    }
    memcpy(b->data + b->len, s, n);
    b->len += n;
    b->data[b->len] = '\0';
}

static void sbuf_push_char(StackBuf* b, char c) {
    sbuf_push(b, &c, 1);
}

static void sbuf_push_str(StackBuf* b, const char* s) {
    if (!s) s = "(null)";
    sbuf_push(b, s, strlen(s));
}

/* ── integer → string
 * ─────────────────────────────────────────────────────────
 */

static size_t fmt_u64(char* out, uint64_t v, int base, int upper) {
    if (v == 0) {
        out[0] = '0';
        out[1] = '\0';
        return 1;
    }
    const char* digits = upper ? "0123456789ABCDEF" : "0123456789abcdef";
    char tmp[66];
    int i = 0;
    while (v) {
        tmp[i++] = digits[v % (uint64_t)base];
        v /= (uint64_t)base;
    }
    for (int j = 0; j < i; j++)
        out[j] = tmp[i - 1 - j];
    out[i] = '\0';
    return (size_t)i;
}

static size_t fmt_i64(char* out, int64_t v, int base) {
    if (v < 0) {
        out[0] = '-';
        return 1 + fmt_u64(out + 1, (uint64_t)(-(v + 1)) + 1, base, 0);
    }
    return fmt_u64(out, (uint64_t)v, base, 0);
}

/* ── double → string (very small grisu-like stub)
 * ─────────────────────────────
 */

static size_t fmt_double(char* out, double v, int prec, char spec) {
    /* Use platform snprintf for floating-point — it's not in the hot path. */
    char fmt[16];
    int fi = 0;
    fmt[fi++] = '%';
    fmt[fi++] = '.';
    /* prec */
    if (prec < 0) prec = 6;
    if (prec > 20) prec = 20;
    if (prec >= 10) { fmt[fi++] = (char)('0' + prec / 10); }
    fmt[fi++] = (char)('0' + prec % 10);
    fmt[fi++] = spec; /* 'f', 'e', 'g' */
    fmt[fi++] = '\0';

    /* io_snprintf wraps the standard snprintf / _snprintf — acceptable for
     * float formatting. */
    int n = io_snprintf(out, 64, fmt, v);
    if (n < 0) n = 0;
    out[n] = '\0';
    return (size_t)n;
}

/* ── padding helper
 * ───────────────────────────────────────────────────────────
 */

static void sbuf_pad(StackBuf* b, int width, size_t used, int left, char padch) {
    if (width <= 0 || (int)used >= width) return;
    int pad = width - (int)used;
    if (left) {
        for (int i = 0; i < pad; i++) sbuf_push_char(b, ' ');
    } else {
        for (int i = 0; i < pad; i++) sbuf_push_char(b, padch);
    }
}

/* ════════════════════════════════════════════════════════════════════════════
 *  Core vprintf — writes formatted output into a StackBuf
 * ════════════════════════════════════════════════════════════════════════════
 */

static void ty_vformat(StackBuf* out, const char* fmt, va_list ap) {
    const char* p = fmt;
    while (*p) {
        if (*p != '%') { sbuf_push_char(out, *p++); continue; }
        p++; /* skip '%' */

        /* ── flags ── */
        int flag_left  = 0;
        int flag_zero  = 0;
        int flag_plus  = 0;
        int flag_space = 0;
        int flag_hash  = 0;
        for (;;) {
            if      (*p == '-') { flag_left  = 1; p++; }
            else if (*p == '0') { flag_zero  = 1; p++; }
            else if (*p == '+') { flag_plus  = 1; p++; }
            else if (*p == ' ') { flag_space = 1; p++; }
            else if (*p == '#') { flag_hash  = 1; p++; }
            else break;
        }

        /* ── width ── */
        int width = 0;
        if (*p == '*') { width = va_arg(ap, int); p++; }
        else while (*p >= '0' && *p <= '9') width = width * 10 + (*p++ - '0');
        if (width < 0) { flag_left = 1; width = -width; }

        /* ── precision ── */
        int prec = -1;
        if (*p == '.') {
            p++;
            prec = 0;
            if (*p == '*') { prec = va_arg(ap, int); p++; }
            else while (*p >= '0' && *p <= '9') prec = prec * 10 + (*p++ - '0');
        }

        /* ── length modifier ── */
        int is_long  = 0;
        int is_llong = 0;
        if (*p == 'l') {
            p++;
            if (*p == 'l') { is_llong = 1; p++; }
            else             is_long  = 1;
        } else if (*p == 'h') { p++; }  /* short — treat as int */

        char spec = *p++;
        char tmp[72];

        switch (spec) {
        case '%':
            sbuf_push_char(out, '%');
            break;

        case 'c': {
            char c = (char)va_arg(ap, int);
            if (!flag_left)
                sbuf_pad(out, width, 1, 0, ' ');
            sbuf_push_char(out, c);
            if (flag_left)
                sbuf_pad(out, width, 1, 1, ' ');
            break;
        }

        case 's': {
            const char* s = va_arg(ap, const char*);
            if (!s)
                s = "(null)";
            size_t slen = strlen(s);
            if (prec >= 0 && (size_t)prec < slen)
                slen = (size_t)prec;
            if (!flag_left)
                sbuf_pad(out, width, slen, 0, ' ');
            sbuf_push(out, s, slen);
            if (flag_left)
                sbuf_pad(out, width, slen, 1, ' ');
            break;
        }

        case 'd':
        case 'i': {
            int64_t v = is_llong ? va_arg(ap, long long) : is_long ? va_arg(ap, long)
                                                                   : (int64_t)va_arg(ap, int);
            size_t n = fmt_i64(tmp, v, 10);
            /* handle sign prefix */
            char sign_ch = 0;
            if (v >= 0) {
                if (flag_plus)
                    sign_ch = '+';
                else if (flag_space)
                    sign_ch = ' ';
            }
            size_t total = n + (sign_ch ? 1 : 0);
            char padch = (flag_zero && !flag_left) ? '0' : ' ';
            if (!flag_left) {
                if (flag_zero && sign_ch)
                    sbuf_push_char(out, sign_ch);
                sbuf_pad(out, width, total, 0, padch);
                if (!flag_zero && sign_ch)
                    sbuf_push_char(out, sign_ch);
            } else {
                if (sign_ch)
                    sbuf_push_char(out, sign_ch);
            }
            sbuf_push(out, tmp + (v < 0 ? 1 : 0), n - (v < 0 ? 1 : 0));
            if (v < 0) { /* already in tmp[0] if negative */
            }
            /* redo cleanly */
            out->len -= n - (v < 0 ? 1 : 0);
            if (v < 0) {
                sbuf_push_char(out, '-');
            } else if (sign_ch && !(!flag_left && !flag_zero)) { /* already pushed */
            }
            sbuf_push(out, tmp + (v < 0 ? 1 : 0), n - (v < 0 ? 1 : 0));
            if (flag_left)
                sbuf_pad(out, width, total, 1, ' ');
            break;
        }

        case 'u': {
            uint64_t v = is_llong ? (uint64_t)va_arg(ap, unsigned long long)
                : is_long         ? (uint64_t)va_arg(ap, unsigned long)
                                  : (uint64_t)va_arg(ap, unsigned int);
            size_t n = fmt_u64(tmp, v, 10, 0);
            char padch = (flag_zero && !flag_left) ? '0' : ' ';
            if (!flag_left)
                sbuf_pad(out, width, n, 0, padch);
            sbuf_push(out, tmp, n);
            if (flag_left)
                sbuf_pad(out, width, n, 1, ' ');
            break;
        }

        case 'x':
        case 'X': {
            uint64_t v = is_llong ? (uint64_t)va_arg(ap, unsigned long long)
                : is_long         ? (uint64_t)va_arg(ap, unsigned long)
                                  : (uint64_t)va_arg(ap, unsigned int);
            size_t pfx = (flag_hash && v) ? 2 : 0;
            size_t n = fmt_u64(tmp, v, 16, spec == 'X');
            size_t total = n + pfx;
            char padch = (flag_zero && !flag_left) ? '0' : ' ';
            if (!flag_left) {
                if (flag_zero && pfx) {
                    sbuf_push(out, spec == 'X' ? "0X" : "0x", 2);
                }
                sbuf_pad(out, width, total, 0, padch);
                if (!flag_zero && pfx)
                    sbuf_push(out, spec == 'X' ? "0X" : "0x", 2);
            } else {
                if (pfx)
                    sbuf_push(out, spec == 'X' ? "0X" : "0x", 2);
            }
            sbuf_push(out, tmp, n);
            if (flag_left)
                sbuf_pad(out, width, total, 1, ' ');
            break;
        }

        case 'o': {
            uint64_t v = is_llong ? (uint64_t)va_arg(ap, unsigned long long)
                : is_long         ? (uint64_t)va_arg(ap, unsigned long)
                                  : (uint64_t)va_arg(ap, unsigned int);
            if (flag_hash && v)
                sbuf_push_char(out, '0');
            size_t n = fmt_u64(tmp, v, 8, 0);
            char padch = (flag_zero && !flag_left) ? '0' : ' ';
            if (!flag_left)
                sbuf_pad(out, width, n, 0, padch);
            sbuf_push(out, tmp, n);
            if (flag_left)
                sbuf_pad(out, width, n, 1, ' ');
            break;
        }

        case 'b': {
            /* %b — binary (extension) */
            uint64_t v = is_llong ? (uint64_t)va_arg(ap, unsigned long long)
                : is_long         ? (uint64_t)va_arg(ap, unsigned long)
                                  : (uint64_t)va_arg(ap, unsigned int);
            size_t n = fmt_u64(tmp, v, 2, 0);
            char padch = (flag_zero && !flag_left) ? '0' : ' ';
            if (!flag_left)
                sbuf_pad(out, width, n, 0, padch);
            sbuf_push(out, tmp, n);
            if (flag_left)
                sbuf_pad(out, width, n, 1, ' ');
            break;
        }

        case 'f':
        case 'F':
        case 'e':
        case 'E':
        case 'g':
        case 'G': {
            double v = va_arg(ap, double);
            size_t n = fmt_double(tmp, v, prec, (char)(spec | 0x20)); /* lowercase */
            if (!flag_left)
                sbuf_pad(out, width, n, 0, flag_zero ? '0' : ' ');
            sbuf_push(out, tmp, n);
            if (flag_left)
                sbuf_pad(out, width, n, 1, ' ');
            break;
        }

        default:
            sbuf_push_char(out, '%');
            sbuf_push_char(out, spec);
            break;
        }
    }
}

/* ═══════════════════════════════════════════════════════════════════════════
 *  PRINT family  — write to fd
 * ═══════════════════════════════════════════════════════════════════════════
 */

/* ── ty_print / ty_println
 * ───────────────────────────────────────────────────── */

void ty_print(SlabArena* arena, char* s) {
    if (!s) s = "";
    (void)arena;
    io_do_write(TY_STDOUT_FD, s, strlen(s));
}

void ty_println(SlabArena* arena, char* s) {
    ty_print(arena, s);
    io_do_write(TY_STDOUT_FD, "\n", 1);
}

/* ── ty_printf
 * ───────────────────────────────────────────────────────────────── */

static void ty_format_fixed(StackBuf* out, const char* fmt, uint64_t* args, int n_args) {
    const char* p = fmt;
    int arg_idx = 0;
    while (*p) {
        if (*p != '%') {
            sbuf_push_char(out, *p++);
            continue;
        }
        p++; /* skip '%' */
        if (*p == '%') {
            sbuf_push_char(out, '%');
            p++;
            continue;
        }

        /* Skip flags/width for now or handle simply */
        while (*p && strchr("-+ #0123456789.", *p))
            p++;

        int is_long = 0;
        if (*p == 'l') {
            is_long = 1;
            p++;
            if (*p == 'l') p++;
        }

        char spec = *p++;
        char tmp[72];
        uint64_t val = (arg_idx < n_args) ? args[arg_idx++] : 0;

        if (spec == 'd' || spec == 'i') {
            fmt_i64(tmp, (int64_t)val, 10);
            sbuf_push(out, tmp, strlen(tmp));
        } else if (spec == 'u') {
            fmt_u64(tmp, val, 10, 0);
            sbuf_push(out, tmp, strlen(tmp));
        } else if (spec == 'x' || spec == 'X') {
            fmt_u64(tmp, val, 16, spec == 'X');
            sbuf_push(out, tmp, strlen(tmp));
        } else if (spec == 's') {
            const char* s = (const char*)val;
            if (!s) s = "(null)";
            sbuf_push(out, s, strlen(s));
        } else if (spec == 'c') {
            sbuf_push_char(out, (char)val);
        } else if (spec == 'p') {
            sbuf_push(out, "0x", 2);
            fmt_u64(tmp, val, 16, 0);
            sbuf_push(out, tmp, strlen(tmp));
        }
    }
}

void ty_printf(SlabArena* arena, char* fmt, uint64_t a1, uint64_t a2, uint64_t a3, uint64_t a4) {
    (void)arena;
    StackBuf buf;
    sbuf_init(&buf);
    uint64_t args[4] = { a1, a2, a3, a4 };
    ty_format_fixed(&buf, fmt, args, 4);
    io_do_write(TY_STDOUT_FD, buf.data, buf.len);
}

/* ── ty_fprint / ty_fprintln / ty_fprintf
 * ────────────────────────────────────── */

void ty_fprint(SlabArena* arena, int fd, char* s) {
    (void)arena;
    if (!s) s = "";
    io_do_write(fd, s, strlen(s));
}

void ty_fprintln(SlabArena* arena, int fd, char* s) {
    ty_fprint(arena, fd, s);
    io_do_write(fd, "\n", 1);
}

void ty_fprintf(SlabArena* arena, int fd, char* fmt, ...) {
    (void)arena;
    StackBuf buf;
    sbuf_init(&buf);
    va_list ap;
    va_start(ap, fmt);
    ty_vformat(&buf, fmt, ap);
    va_end(ap);
    io_do_write(fd, buf.data, buf.len);
}

/* ── ty_sprint / ty_sprintln / ty_sprintf — write into a Buf
 * ──────────────────
 */

/*
 * The `out` parameter is a Buf* allocated in the caller's arena (task).
 * We push the formatted text into it using ty_buf_push_str.
 */

void ty_sprint(SlabArena* arena, Buf* out, char* s) {
    if (!out || !s) return;
    ty_buf_push_str(arena, out, s);
}

void ty_sprintln(SlabArena* arena, Buf* out, char* s) {
    ty_sprint(arena, out, s);
    ty_buf_push_str(arena, out, "\n");
}

void ty_sprintf(SlabArena* arena, Buf* out, char* fmt, ...) {
    if (!out) return;
    StackBuf tmp;
    sbuf_init(&tmp);
    va_list ap;
    va_start(ap, fmt);
    ty_vformat(&tmp, fmt, ap);
    va_end(ap);
    ty_buf_push_str(arena, out, tmp.data);
}

/* ═══════════════════════════════════════════════════════════════════════════
 *  SCAN family  — read tokens / formatted data
 * ═══════════════════════════════════════════════════════════════════════════
 */

/*
 * Token reading strategy
 * ──────────────────────
 *   scan / fscan read one whitespace-delimited token into a caller-supplied
 *   Buf.  Bytes are read one at a time to avoid over-reading the stream.
 *   (This is async-aware: each single-byte read parks then resumes.)
 *
 *   scanf / fscanf parse a format string and write results into the va_list
 *   pointers.  Supported format specifiers:
 *     %s %c %d %i %u %ld %li %lu %lld %lli %llu %f %lf %g %e %x %X %o
 */

/* ── skip whitespace
 * ─────────────────────────────────────────────────────────── */

static int read_char(int fd) {
    char c = 0;
    int64_t n = io_do_read(fd, &c, 1);
    if (n <= 0) return -1;
    return (unsigned char)c;
}

static int skip_ws(int fd) {
    int c;
    while ((c = read_char(fd)) >= 0) {
        if (c != ' ' && c != '\t' && c != '\n' && c != '\r') return c;
    }
    return -1;
}

/* ── read one whitespace-delimited token
 * ──────────────────────────────────────── */

static int read_token(int fd, char* buf, size_t cap) {
    int c = skip_ws(fd);
    if (c < 0) return 0;
    size_t i = 0;
    while (c >= 0 && c != ' ' && c != '\t' && c != '\n' && c != '\r') {
        if (i + 1 < cap) buf[i++] = (char)c;
        c = read_char(fd);
    }
    buf[i] = '\0';
    return (int)i;
}

/* ── ty_scan / ty_fscan
 * ──────────────────────────────────────────────────────── */

/*
 * Returns a new Str (char*) in the calling coroutine's arena.
 * The string persists until slab_arena_free.
 */
char* ty_scan(SlabArena* arena) {
    char tmp[1024];
    int n = read_token(TY_STDIN_FD, tmp, sizeof(tmp));
    if (n == 0) return NULL;
    Buf* b = ty_buf_new(arena);
    ty_buf_push_str(arena, b, tmp);
    return ty_buf_into_str(arena, b);
}

char* ty_fscan(SlabArena* arena, int fd) {
    char tmp[1024];
    int n = read_token(fd, tmp, sizeof(tmp));
    if (n == 0) return NULL;
    Buf* b = ty_buf_new(arena);
    ty_buf_push_str(arena, b, tmp);
    return ty_buf_into_str(arena, b);
}

/* ── Core vscanf
 * ─────────────────────────────────────────────────────────────── */

/*
 * ty_vfscanf — reads from fd according to fmt, filling va_list pointers.
 * Returns number of items successfully matched.
 */
static int ty_vfscanf(int fd, const char* fmt, va_list ap) {
    int matched = 0;
    const char* p = fmt;

    while (*p) {
        /* literal whitespace in format → skip whitespace in input */
        if (*p == ' ' || *p == '\t' || *p == '\n') {
            /* just advance; whitespace skipped per-specifier */
            p++;
            continue;
        }

        if (*p != '%') {
            /* literal character match */
            int c = read_char(fd);
            if (c != (unsigned char)*p) return matched;
            p++;
            continue;
        }

        p++; /* skip '%' */
        if (*p == '%') { /* literal %% */
            int c = read_char(fd);
            if (c != '%') return matched;
            p++;
            continue;
        }

        /* suppress assignment flag */
        int suppress = 0;
        if (*p == '*') { suppress = 1; p++; }

        /* width */
        int width = 0;
        while (*p >= '0' && *p <= '9') width = width * 10 + (*p++ - '0');

        /* length */
        int is_long = 0, is_llong = 0;
        if (*p == 'l') { p++; if (*p == 'l') { is_llong = 1; p++; } else is_long = 1; }
        else if (*p == 'h') p++;

        char spec = *p++;

        switch (spec) {
        case 's': {
            char tok[1024]; int n = read_token(fd, tok, sizeof(tok));
            if (n == 0) return matched;
            if (!suppress) { char** dst = va_arg(ap, char**); *dst = tok; matched++; }
            break;
        }

        case 'c': {
            int c = read_char(fd);
            if (c < 0) return matched;
            if (!suppress) { *va_arg(ap, char*) = (char)c; matched++; }
            break;
        }

        case 'd':
        case 'i': {
            char tok[32];
            int n = read_token(fd, tok, sizeof(tok));
            if (n == 0)
                return matched;
            int64_t v = 0;
            int neg = 0;
            const char* q = tok;
            if (*q == '-') {
                neg = 1;
                q++;
            } else if (*q == '+')
                q++;
            while (*q >= '0' && *q <= '9')
                v = v * 10 + (*q++ - '0');
            if (neg)
                v = -v;
            if (!suppress) {
                if (is_llong)
                    *va_arg(ap, long long*) = (long long)v;
                else if (is_long)
                    *va_arg(ap, long*) = (long)v;
                else
                    *va_arg(ap, int*) = (int)v;
                matched++;
            }
            break;
        }

        case 'u': {
            char tok[32];
            int n = read_token(fd, tok, sizeof(tok));
            if (n == 0)
                return matched;
            uint64_t v = 0;
            const char* q = tok;
            while (*q >= '0' && *q <= '9')
                v = v * 10 + (uint64_t)(*q++ - '0');
            if (!suppress) {
                if (is_llong)
                    *va_arg(ap, unsigned long long*) = (unsigned long long)v;
                else if (is_long)
                    *va_arg(ap, unsigned long*) = (unsigned long)v;
                else
                    *va_arg(ap, unsigned int*) = (unsigned int)v;
                matched++;
            }
            break;
        }

        case 'x':
        case 'X': {
            char tok[32];
            int n = read_token(fd, tok, sizeof(tok));
            if (n == 0)
                return matched;
            uint64_t v = 0;
            const char* q = tok;
            if (q[0] == '0' && (q[1] == 'x' || q[1] == 'X'))
                q += 2;
            while ((*q >= '0' && *q <= '9') || (*q >= 'a' && *q <= 'f') || (*q >= 'A' && *q <= 'F')) {
                uint64_t d = (*q >= 'a') ? (uint64_t)(*q - 'a' + 10)
                    : (*q >= 'A')        ? (uint64_t)(*q - 'A' + 10)
                                         : (uint64_t)(*q - '0');
                v = v * 16 + d;
                q++;
            }
            if (!suppress) {
                *va_arg(ap, unsigned int*) = (unsigned int)v;
                matched++;
            }
            break;
        }

        case 'o': {
            char tok[32];
            int n = read_token(fd, tok, sizeof(tok));
            if (n == 0)
                return matched;
            uint64_t v = 0;
            const char* q = tok;
            while (*q >= '0' && *q <= '7')
                v = v * 8 + (uint64_t)(*q++ - '0');
            if (!suppress) {
                *va_arg(ap, unsigned int*) = (unsigned int)v;
                matched++;
            }
            break;
        }

        case 'f':
        case 'F':
        case 'e':
        case 'E':
        case 'g':
        case 'G': {
            char tok[64];
            int n = read_token(fd, tok, sizeof(tok));
            if (n == 0)
                return matched;
            /* Use platform strtod */
            double v = io_strtod(tok, NULL);
            if (!suppress) {
                if (is_long)
                    *va_arg(ap, double*) = v;
                else
                    *va_arg(ap, float*) = (float)v;
                matched++;
            }
            break;
        }

        default:
            return matched;
        }
    }
    return matched;
}

/* ── ty_scanf / ty_fscanf
 * ────────────────────────────────────────────────────── */

int ty_scanf(void* task, char* fmt, ...) {
    (void)task;
    va_list ap;
    va_start(ap, fmt);
    int n = ty_vfscanf(TY_STDIN_FD, fmt, ap);
    va_end(ap);
    return n;
}

int ty_fscanf(void* task, int fd, char* fmt, ...) {
    (void)task;
    va_list ap;
    va_start(ap, fmt);
    int n = ty_vfscanf(fd, fmt, ap);
    va_end(ap);
    return n;
}

/* ── ty_sscan / ty_sscanf — read from a Str (char*)
 * ───────────────────────────
 */

/*
 * sscan / sscanf are purely in-memory; no async I/O needed.
 * We keep a cursor into the source string.
 */

typedef struct {
    const char* src;
    size_t      pos;
    size_t      len;
} StrCursor;

static int sc_read_char(StrCursor* sc) {
    if (sc->pos >= sc->len) return -1;
    return (unsigned char)sc->src[sc->pos++];
}

static int sc_skip_ws(StrCursor* sc) {
    int c;
    while ((c = sc_read_char(sc)) >= 0) {
        if (c != ' ' && c != '\t' && c != '\n' && c != '\r') return c;
    }
    return -1;
}

static int sc_read_token(StrCursor* sc, char* buf, size_t cap) {
    int c = sc_skip_ws(sc);
    if (c < 0) return 0;
    size_t i = 0;
    while (c >= 0 && c != ' ' && c != '\t' && c != '\n' && c != '\r') {
        if (i + 1 < cap) buf[i++] = (char)c;
        c = sc_read_char(sc);
    }
    buf[i] = '\0';
    return (int)i;
}

/*
 * ty_sscan — reads one whitespace-delimited token from `src`.
 * Returns a pointer into the source string (not arena-allocated) or NULL.
 *
 * Caller receives a (char*, char*) pair: (result_ptr, remaining_src_ptr).
 * For simplicity we return the token as a null-terminated slice by mutating
 * the source string in-place (caller must own the string).
 */
char* ty_sscan(void* task, char* src, char** rest_out) {
    (void)task;
    if (!src) {
        if (rest_out) *rest_out = NULL;
        return NULL;
    }
    /* skip leading whitespace */
    while (*src == ' ' || *src == '\t' || *src == '\n' || *src == '\r')
        src++;
    if (!*src) {
        if (rest_out) *rest_out = src;
        return NULL;
    }
    char* start = src;
    while (*src && *src != ' ' && *src != '\t' && *src != '\n' && *src != '\r')
        src++;
    if (*src) {
        *src = '\0';
        src++;
    }
    if (rest_out) *rest_out = src;
    return start;
}

static int ty_vsscanf(const char* src, const char* fmt, va_list ap) {
    StrCursor sc;
    sc.src = src;
    sc.pos = 0;
    sc.len = strlen(src);
    int matched = 0;
    const char* p = fmt;

    while (*p) {
        if (*p == ' ' || *p == '\t' || *p == '\n') {
            p++;
            continue;
        }
        if (*p != '%') {
            int c = sc_read_char(&sc);
            if (c != (unsigned char)*p) return matched;
            p++;
            continue;
        }
        p++;
        if (*p == '%') {
            sc_read_char(&sc);
            p++;
            continue;
        }

        int suppress = 0;
        if (*p == '*') {
            suppress = 1;
            p++;
        }
        int width = 0;
        while (*p >= '0' && *p <= '9')
            width = width * 10 + (*p++ - '0');
        int is_long = 0, is_llong = 0;
        if (*p == 'l') {
            p++;
            if (*p == 'l') {
                is_llong = 1;
                p++;
            } else
                is_long = 1;
        } else if (*p == 'h')
            p++;
        char spec = *p++;

        switch (spec) {
        case 's': {
            char tok[1024];
            int n = sc_read_token(&sc, tok, sizeof(tok));
            if (n == 0) return matched;
            if (!suppress) {
                char** dst = va_arg(ap, char**);
                *dst = tok;
                matched++;
            }
            break;
        }
        case 'c': {
            int c = sc_read_char(&sc);
            if (c < 0) return matched;
            if (!suppress) {
                *va_arg(ap, char*) = (char)c;
                matched++;
            }
            break;
        }
        case 'd':
        case 'i': {
            char tok[32];
            sc_read_token(&sc, tok, sizeof(tok));
            int64_t v = 0;
            int neg = 0;
            const char* q = tok;
            if (*q == '-') {
                neg = 1;
                q++;
            } else if (*q == '+')
                q++;
            while (*q >= '0' && *q <= '9')
                v = v * 10 + (*q++ - '0');
            if (neg)
                v = -v;
            if (!suppress) {
                if (is_llong)
                    *va_arg(ap, long long*) = (long long)v;
                else if (is_long)
                    *va_arg(ap, long*) = (long)v;
                else
                    *va_arg(ap, int*) = (int)v;
                matched++;
            }
            break;
        }
        case 'u': {
            char tok[32];
            sc_read_token(&sc, tok, sizeof(tok));
            uint64_t v = 0;
            const char* q = tok;
            while (*q >= '0' && *q <= '9')
                v = v * 10 + (uint64_t)(*q++ - '0');
            if (!suppress) {
                *va_arg(ap, unsigned int*) = (unsigned int)v;
                matched++;
            }
            break;
        }
        case 'f':
        case 'g':
        case 'e': {
            char tok[64];
            sc_read_token(&sc, tok, sizeof(tok));
            double v = io_strtod(tok, NULL);
            if (!suppress) {
                if (is_long || is_llong)
                    *va_arg(ap, double*) = v;
                else
                    *va_arg(ap, float*) = (float)v;
                matched++;
            }
            break;
        }
        default:
            return matched;
        }
    }
    return matched;
}

int ty_sscanf(void* task, char* src, char* fmt, ...) {
    (void)task;
    va_list ap;
    va_start(ap, fmt);
    int n = ty_vsscanf(src, fmt, ap);
    va_end(ap);
    return n;
}

/* ═══════════════════════════════════════════════════════════════════════════
 *  LLVM IR declarations emitted by codegen.rs
 *  (kept here as a reference; actual emission is in collect_types)
 * ═══════════════════════════════════════════════════════════════════════════

declare void    @ty_print    (i8* %task, i8* %s)
declare void    @ty_println  (i8* %task, i8* %s)
declare void    @ty_printf   (i8* %task, i8* %fmt, ...)
declare void    @ty_fprint   (i8* %task, i32 %fd, i8* %s)
declare void    @ty_fprintln (i8* %task, i32 %fd, i8* %s)
declare void    @ty_fprintf  (i8* %task, i32 %fd, i8* %fmt, ...)
declare void    @ty_sprint   (i8* %task, %struct.Buf* %out, i8* %s)
declare void    @ty_sprintln (i8* %task, %struct.Buf* %out, i8* %s)
declare void    @ty_sprintf  (i8* %task, %struct.Buf* %out, i8* %fmt, ...)
declare i8*     @ty_scan     (i8* %task)
declare i32     @ty_scanf    (i8* %task, i8* %fmt, ...)
declare i8*     @ty_fscan    (i8* %task, i32 %fd)
declare i32     @ty_fscanf   (i8* %task, i32 %fd, i8* %fmt, ...)
declare i8*     @ty_sscan    (i8* %task, i8* %src, i8** %rest_out)
declare i32     @ty_sscanf   (i8* %task, i8* %src, i8* %fmt, ...)

 * ═══════════════════════════════════════════════════════════════════════════
*/
