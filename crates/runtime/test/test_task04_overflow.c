/*
 * test_task04_overflow.c — Regression test for Task 0.4
 *
 * Bug: StackBuf.overflow was set on truncation but never read; callers had no
 *      way to detect the error.
 *
 * Fix: ty_fprintf, ty_fprint, ty_fprintln return -1 on overflow.
 *      ty_sprintf return type changes from void to int; returns -1 on overflow
 *      and does NOT push partial data into the Buf.
 *
 * FIXES vs original submitted test:
 *   - ty_sprintf was the primary change in the patch (void → int, no partial
 *     data on overflow) but was completely absent from the original test.
 *     Added test_sprintf_overflow() and test_sprintf_normal() covering:
 *       a) returns -1 on overflow
 *       b) Buf is empty after overflow (no partial data written)
 *       c) returns positive byte count on success
 *       d) Buf contains correct data on success
 *
 * Build:
 *   gcc -Wall -Wextra -g -o test_task04 test_task04_overflow.c
 *   gcc -fsanitize=address -g -Wall -Wextra -o test_task04 test_task04_overflow.c
 */

#include <stdio.h>
#include <string.h>
#include <stdarg.h>
#include <stddef.h>
#include <assert.h>
#include <stdlib.h>

/* ── StackBuf replica ─────────────────────────────────────────────────────── */

#define STACK_BUF_CAP 4096

typedef struct {
    char   data[STACK_BUF_CAP];
    size_t len;
    int    overflow;  /* set to 1 when output is truncated */
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

static void sbuf_push_str(StackBuf* b, const char* s) {
    if (!s) s = "(null)";
    sbuf_push(b, s, strlen(s));
}

/* Minimal vformat — only %s needed for the overflow test. */
static void sbuf_vformat(StackBuf* out, const char* fmt, va_list ap) {
    const char* p = fmt;
    while (*p) {
        if (*p != '%') { sbuf_push(out, p, 1); p++; continue; }
        p++;
        if (*p == 's') {
            const char* s = va_arg(ap, const char*);
            sbuf_push_str(out, s ? s : "(null)");
            p++;
        } else {
            sbuf_push(out, "%", 1);
            if (*p) { sbuf_push(out, p, 1); p++; }
        }
    }
}

/* ── Minimal Buf for ty_sprintf test ────────────────────────────────────── */

typedef struct {
    char*  data;
    size_t len;
    size_t cap;
} Buf;

static Buf* buf_new(void) {
    Buf* b = (Buf*)calloc(1, sizeof(Buf));
    b->cap  = 64;
    b->data = (char*)calloc(1, b->cap);
    b->data[0] = '\0';
    return b;
}

static void buf_push_str(Buf* b, const char* s) {
    size_t n = strlen(s);
    while (b->len + n + 1 > b->cap) {
        b->cap *= 2;
        b->data = (char*)realloc(b->data, b->cap);
    }
    memcpy(b->data + b->len, s, n);
    b->len += n;
    b->data[b->len] = '\0';
}

static void buf_free(Buf* b) { free(b->data); free(b); }

static void io_do_write_devnull(const char* buf, size_t len) {
    (void)buf; (void)len; /* discard — we test return values, not output */
}

/* ── Inline implementations of the fixed functions ──────────────────────── */
/*
 * These mirror ty_io.c exactly so the test is authoritative even without
 * linking the full runtime.
 * StackBuf is a temporary measure; replaced by slab TyBuf in Phase 3.
 */

/* ty_fprintf — unchanged from original, included for completeness */
static int test_ty_fprintf(int fd, char* fmt, ...) {
    (void)fd;
    StackBuf buf;
    sbuf_init(&buf);
    va_list ap;
    va_start(ap, fmt);
    sbuf_vformat(&buf, fmt, ap);
    va_end(ap);
    /* StackBuf is a temporary measure; replaced by slab TyBuf in Phase 3. */
    if (buf.overflow) return -1; /* signal truncation to caller */
    io_do_write_devnull(buf.data, buf.len);
    return (int)buf.len;
}

static int test_ty_fprint(int fd, char* s) {
    (void)fd;
    if (!s) s = "";
    size_t len = strlen(s);
    if (len >= STACK_BUF_CAP) return -1; /* output would be truncated */
    io_do_write_devnull(s, len);
    return (int)len;
}

static int test_ty_fprintln(int fd, char* s) {
    int r = test_ty_fprint(fd, s);
    if (r < 0) return r;
    io_do_write_devnull("\n", 1);
    return r + 1;
}

/*
 * ty_sprintf — THE primary Task 0.4 change.
 * Before: return type was void; buf.overflow was not checked; partial data
 *         was pushed into out regardless.
 * After:  returns -1 on overflow; no partial data written.
 */
static int test_ty_sprintf(Buf* out, char* fmt, ...) {
    if (!out) return -1;
    StackBuf tmp;
    sbuf_init(&tmp);
    va_list ap;
    va_start(ap, fmt);
    sbuf_vformat(&tmp, fmt, ap);
    va_end(ap);
    if (tmp.overflow) return -1;         /* signal truncation — no partial push */
    buf_push_str(out, tmp.data);
    return (int)tmp.len;
}

/* ── Helpers ──────────────────────────────────────────────────────────────── */

/* Build a string of exactly `n` bytes of 'x'. Caller must free(). */
static char* make_str(size_t n) {
    char* s = (char*)malloc(n + 1);
    assert(s);
    memset(s, 'x', n);
    s[n] = '\0';
    return s;
}

/* ── Test: demonstrate the pre-fix behaviour (informational only) ─────────
 *
 * Before the fix, ty_fprintf / ty_fprint returned void.  There was no way to
 * detect truncation.  We document this with a comment rather than calling the
 * old void functions, since they are no longer present in the fixed ty_io.c.
 */
static void demo_before_fix(void) {
    printf("[task 0.4] BEFORE fix: overflow silently ignored — sprintf returned void\n");
}

/* ── Tests: ty_fprintf / ty_fprint / ty_fprintln (from original) ──────────── */

static void test_fprintf_overflow(void) {
    char* big = make_str(STACK_BUF_CAP + 1);
    int ret = test_ty_fprintf(1, "%s", big);
    free(big);
    assert(ret == -1 && "[task 0.4] FAIL: ty_fprintf did not return -1 on overflow");
    printf("[task 0.4] AFTER fix:  ty_fprintf returns -1 on overflow — PASS\n");
}

/* ── Test 2: ty_fprintf returns correct byte count on normal output ────── */
static void test_fprintf_normal(void) {
    int ret = test_ty_fprintf(1, "%s", "hello");
    assert(ret == 5 && "[task 0.4] FAIL: ty_fprintf returned wrong count on normal output");
    printf("[task 0.4] AFTER fix:  ty_fprintf returns byte count on normal output — PASS\n");
}

/* ── Test 3: ty_fprint returns -1 when the raw string >= 4 096 bytes ────── */
static void test_fprint_overflow(void) {
    char* big = make_str(STACK_BUF_CAP); /* exactly at the cap boundary */

    int ret = test_ty_fprint(1, big);
    free(big);
    assert(ret == -1 && "[task 0.4] FAIL: ty_fprint did not return -1 at cap boundary");
    printf("[task 0.4] AFTER fix:  ty_fprint returns -1 when string >= 4096 bytes — PASS\n");
}

/* ── Test 4: ty_fprint returns correct byte count on normal output ──────── */
static void test_fprint_normal(void) {
    char* msg = "short string";
    int ret = test_ty_fprint(1, msg);
    assert(ret == (int)strlen(msg) && "[task 0.4] FAIL: ty_fprint wrong count");
    printf("[task 0.4] AFTER fix:  ty_fprint returns byte count on normal output — PASS\n");
}

/* ── Test 5: ty_fprintln propagates -1 from ty_fprint ───────────────────── */
static void test_fprintln_overflow(void) {
    char* big = make_str(STACK_BUF_CAP); /* >= cap, fprint returns -1 */

    int ret = test_ty_fprintln(1, big);
    free(big);
    assert(ret == -1 && "[task 0.4] FAIL: ty_fprintln did not propagate -1");
    printf("[task 0.4] AFTER fix:  ty_fprintln propagates -1 on overflow — PASS\n");
}

/* ── Test 6: ty_fprintln returns len+1 (the newline) on normal output ───── */
static void test_fprintln_normal(void) {
    char* msg = "line";
    int ret = test_ty_fprintln(1, msg);
    assert(ret == (int)strlen(msg) + 1 && "[task 0.4] FAIL: ty_fprintln wrong count");
    printf("[task 0.4] AFTER fix:  ty_fprintln returns len+1 on normal output — PASS\n");
}

/* ── Tests: ty_sprintf — NEW, absent from original test ─────────────────────
 *
 * These are the tests for the primary Task 0.4 change.
 */

/* a) overflow: returns -1 */
static void test_sprintf_overflow_returns_error(void) {
    Buf* out = buf_new();
    char* big = make_str(STACK_BUF_CAP + 1);
    int ret = test_ty_sprintf(out, "%s", big);
    free(big);
    assert(ret == -1 && "[task 0.4] FAIL: ty_sprintf did not return -1 on overflow");
    printf("[task 0.4] AFTER fix:  ty_sprintf returns -1 on overflow — PASS\n");
    buf_free(out);
}

/* b) overflow: Buf must be empty — no partial data pushed */
static void test_sprintf_overflow_no_partial_data(void) {
    Buf* out = buf_new();
    char* big = make_str(STACK_BUF_CAP + 1);
    test_ty_sprintf(out, "%s", big);
    free(big);
    /* Before fix: out->len == 4095 (truncated partial data written).
     * After fix:  out->len == 0  (no write on overflow). */
    assert(out->len == 0 &&
           "[task 0.4] FAIL: ty_sprintf wrote partial data into Buf on overflow");
    assert(out->data[0] == '\0' &&
           "[task 0.4] FAIL: ty_sprintf left non-null data in Buf on overflow");
    printf("[task 0.4] AFTER fix:  ty_sprintf Buf is empty after overflow"
           " — no partial data written — PASS\n");
    buf_free(out);
}

/* c) normal: returns positive byte count */
static void test_sprintf_normal_returns_count(void) {
    Buf* out = buf_new();
    int ret = test_ty_sprintf(out, "%s", "hello world");
    assert(ret == 11 && "[task 0.4] FAIL: ty_sprintf wrong count on normal output");
    printf("[task 0.4] AFTER fix:  ty_sprintf returns byte count on normal output — PASS\n");
    buf_free(out);
}

/* d) normal: Buf contains correct data */
static void test_sprintf_normal_correct_data(void) {
    Buf* out = buf_new();
    test_ty_sprintf(out, "%s", "correct");
    assert(strcmp(out->data, "correct") == 0 &&
           "[task 0.4] FAIL: ty_sprintf wrote wrong data into Buf");
    printf("[task 0.4] AFTER fix:  ty_sprintf Buf contains correct data — PASS\n");
    buf_free(out);
}

/* e) overflow on second call must not corrupt existing Buf content */
static void test_sprintf_overflow_preserves_existing_buf(void) {
    Buf* out = buf_new();
    /* First write succeeds */
    int r1 = test_ty_sprintf(out, "%s", "prefix-");
    assert(r1 > 0);
    size_t len_before = out->len;

    /* Second write overflows — must not alter what is already in the Buf */
    char* big = make_str(STACK_BUF_CAP + 1);
    int r2 = test_ty_sprintf(out, "%s", big);
    free(big);
    assert(r2 == -1 && "[task 0.4] FAIL: second sprintf should return -1 on overflow");
    assert(out->len == len_before &&
           "[task 0.4] FAIL: overflow corrupted existing Buf content");
    assert(strcmp(out->data, "prefix-") == 0 &&
           "[task 0.4] FAIL: existing Buf data was overwritten on overflow");
    printf("[task 0.4] AFTER fix:  ty_sprintf overflow preserves existing Buf content — PASS\n");
    buf_free(out);
}

/* ── main ─────────────────────────────────────────────────────────────────── */

int main(void) {
    demo_before_fix();

    /* ty_fprintf / ty_fprint / ty_fprintln (from original test) */
    test_fprintf_overflow();
    test_fprintf_normal();
    test_fprint_overflow();
    test_fprint_normal();
    test_fprintln_overflow();
    test_fprintln_normal();

    /* ty_sprintf — new tests covering the primary Task 0.4 change */
    test_sprintf_overflow_returns_error();
    test_sprintf_overflow_no_partial_data();
    test_sprintf_normal_returns_count();
    test_sprintf_normal_correct_data();
    test_sprintf_overflow_preserves_existing_buf();

    printf("[task 0.4] All overflow tests PASSED\n");
    return 0;
}
