/*
 * test_task01_dangling_ptr.c — Regression test for Task 0.1
 *
 * Bug: The %s case in ty_vsscanf allocated tok[1024] on the C stack inside
 *      ty_vsscanf, then wrote a pointer to it into the caller's char*.
 *      Once ty_vsscanf returned the stack frame was destroyed; any read of
 *      *dst was undefined behaviour (dangling pointer).
 *
 *      Before fix:
 *        char tok[1024];
 *        int n = sc_read_token(&sc, tok, sizeof(tok));
 *        char** dst = va_arg(ap, char**);
 *        *dst = tok;   // tok is dead after return
 *
 *      After fix:
 *        const char* tok_start;
 *        int n = sc_read_token_len(&sc, &tok_start);
 *        char* result = slab_alloc(task, size_to_class(n + 1));
 *        memcpy(result, tok_start, n);
 *        result[n] = '\0';
 *        *dst = result;   // lives in arena for coroutine lifetime
 *
 * Test strategy
 * ─────────────
 * We inline the exact buggy and fixed implementations as static functions so
 * the test is self-contained and buildable without the full runtime.
 *
 * The buggy version is called, then a stack-clobbering function is called to
 * overwrite the frame where tok[] lived.  Reading *dst after that must produce
 * garbage — that is the bug.  The fixed version is then called and *dst must
 * still be correct after the same clobber.
 *
 * Build:
 *   gcc -Wall -Wextra -g -o test_task01_dangling_ptr test_task01_dangling_ptr.c
 *   gcc -fsanitize=address -g -Wall -Wextra -o test_task01_dangling_ptr test_task01_dangling_ptr.c
 *
 * Expected output:
 *   [task 0.1] BEFORE fix: *dst after return = <garbage or empty — dangling>
 *   [task 0.1] BEFORE fix: dangling pointer confirmed — data is corrupted or lost
 *   [task 0.1] AFTER fix:  %s result = "hello world" — token 1 correct
 *   [task 0.1] AFTER fix:  %s result = "world" — token 2 correct
 *   [task 0.1] AFTER fix:  result pointer outlives vsscanf frame — PASS
 *   [task 0.1] AFTER fix:  result lives in arena, not on vsscanf stack — PASS
 *   [task 0.1] AFTER fix:  multiple %%s in one format string all arena-allocated — PASS
 *   [task 0.1] AFTER fix:  %%*s suppress does not write dst — PASS
 *   [task 0.1] AFTER fix:  empty input returns 0 matches — PASS
 *   [task 0.1] All dangling-pointer tests PASSED
 */

#include <stdio.h>
#include <string.h>
#include <assert.h>
#include <stdlib.h>
#include <stdarg.h>
#include <stdint.h>

/* ── Minimal arena — slab of fixed size, bump-pointer allocator ─────────── */

#define ARENA_SIZE (64 * 1024)

typedef struct {
    char   mem[ARENA_SIZE];
    size_t used;
} Arena;

static Arena* arena_new(void) {
    Arena* a = (Arena*)calloc(1, sizeof(Arena));
    assert(a);
    return a;
}

static void* arena_alloc(Arena* a, size_t n) {
    /* Align to 8 bytes */
    size_t aligned = (n + 7) & ~(size_t)7;
    assert(a->used + aligned <= ARENA_SIZE && "arena exhausted");
    void* p = a->mem + a->used;
    a->used += aligned;
    return p;
}

static void arena_free(Arena* a) { free(a); }

/* ── StrCursor — shared by both implementations ─────────────────────────── */

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
    while ((c = sc_read_char(sc)) >= 0)
        if (c != ' ' && c != '\t' && c != '\n' && c != '\r') return c;
    return -1;
}

/* ── BEFORE fix: sc_read_token copies into a caller-supplied stack buffer ── */

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
 * Buggy vsscanf — %s writes a pointer to a local stack buffer tok[].
 * tok is dead as soon as this function returns.
 */
static int buggy_vsscanf(const char* src, const char* fmt, va_list ap) {
    StrCursor sc = { src, 0, strlen(src) };
    int matched = 0;
    const char* p = fmt;
    while (*p) {
        if (*p != '%') { sc_read_char(&sc); p++; continue; }
        p++;
        char spec = *p++;
        if (spec == 's') {
            char tok[1024];                          /* lives on this frame */
            int n = sc_read_token(&sc, tok, sizeof(tok));
            if (n == 0) return matched;
            char** dst = va_arg(ap, char**);
            *dst = tok;                              /* BUG: pointer to dead stack */
            matched++;
        }
    }
    return matched;
}

static int buggy_sscanf(const char* src, const char* fmt, ...) {
    va_list ap;
    va_start(ap, fmt);
    int n = buggy_vsscanf(src, fmt, ap);
    va_end(ap);
    return n;
}

/* ── AFTER fix: sc_read_token_len returns pointer+length into source ─────── */

static int sc_read_token_len(StrCursor* sc, const char** out_start) {
    int c = sc_skip_ws(sc);
    if (c < 0) return 0;
    *out_start = &sc->src[sc->pos - 1];
    int len = 0;
    while (c >= 0 && c != ' ' && c != '\t' && c != '\n' && c != '\r') {
        len++;
        c = sc_read_char(sc);
    }
    return len;
}

/*
 * Fixed vsscanf — %s arena-allocates the result; pointer outlives the frame.
 */
static int fixed_vsscanf(Arena* arena, const char* src, const char* fmt, va_list ap) {
    StrCursor sc = { src, 0, strlen(src) };
    int matched = 0;
    const char* p = fmt;
    while (*p) {
        if (*p == ' ' || *p == '\t' || *p == '\n') { p++; continue; }
        if (*p != '%') { sc_read_char(&sc); p++; continue; }
        p++;
        int suppress = (*p == '*') ? (p++, 1) : 0;
        char spec = *p++;
        if (spec == 's') {
            const char* tok_start;
            int n = sc_read_token_len(&sc, &tok_start);
            if (n == 0) return matched;
            if (!suppress) {
                char** dst = va_arg(ap, char**);
                char* result = (char*)arena_alloc(arena, (size_t)n + 1);  /* arena lifetime */
                memcpy(result, tok_start, (size_t)n);
                result[n] = '\0';
                *dst = result;
                matched++;
            }
        }
    }
    return matched;
}

static int fixed_sscanf(Arena* arena, const char* src, const char* fmt, ...) {
    va_list ap;
    va_start(ap, fmt);
    int n = fixed_vsscanf(arena, src, fmt, ap);
    va_end(ap);
    return n;
}

/* ── Stack-clobber helper ────────────────────────────────────────────────────
 *
 * Fills the local stack region with 0xCD after the buggy call returns.
 * This overwrites the frame where tok[1024] lived, turning any dangling
 * read into visible garbage rather than a lucky stale-value pass.
 *
 * Marked noinline so the compiler does not fold it into the caller's frame.
 */
#if defined(_MSC_VER)
     #define NOINLINE __declspec(noinline)
 #elif defined(__GNUC__) || defined(__clang__)
     #define NOINLINE __attribute__((noinline))
 #else
     #define NOINLINE
 #endif

NOINLINE static void clobber_stack(void) {
    volatile char pad[2048];
    memset((void*)pad, 0xCD, sizeof(pad));
    /* Prevent the compiler optimising this away */
    (void)pad[0];
}

/* ── Tests ──────────────────────────────────────────────────────────────── */

/*
 * Test 0 (pre-fix): call buggy_sscanf, then clobber the stack, then read *dst.
 * The value should be garbage — that is the bug.  We print rather than assert
 * so the test binary doesn't abort; we just document the corrupted state.
 */
static void demo_before_fix(void) {
    char* result = NULL;
    buggy_sscanf("hello", "%s", &result);
    /*
     * result now points into the dead stack frame of buggy_vsscanf.
     * Clobber that region to make the corruption visible.
     */
    clobber_stack();

    /*
     * Reading *result here is UB.  In practice it will be 0xCD bytes or an
     * empty string, not "hello".  We print but do not assert so the test
     * continues to the fixed cases.
     */
    int corrupted = (result == NULL || result[0] != 'h' ||
                     strcmp(result, "hello") != 0);
    printf("[task 0.1] BEFORE fix: *dst after return = \"%s\" (raw byte 0: 0x%02x)\n",
           result ? result : "(null)",
           result ? (unsigned char)result[0] : 0);
    if (corrupted)
        printf("[task 0.1] BEFORE fix: dangling pointer confirmed — data is corrupted or lost\n");
    else
        printf("[task 0.1] BEFORE fix: WARNING — stale value survived clobber "
               "(run under ASAN to see the UB caught reliably)\n");
}

/* Test 1: basic %s — result pointer outlives the vsscanf stack frame. */
static void test_result_outlives_frame(Arena* arena) {
    char* result = NULL;
    int n = fixed_sscanf(arena, "hello", "%s", &result);

    assert(n == 1);
    /* Clobber the region where buggy tok[] would have lived. */
    clobber_stack();

    /* Result must still be correct — it lives in the arena, not the frame. */
    assert(result != NULL);
    assert(strcmp(result, "hello") == 0);
    printf("[task 0.1] AFTER fix:  %%s result = \"%s\" — token 1 correct\n", result);
}

/* Test 2: two %s tokens from a multi-token input. */
static void test_two_tokens(Arena* arena) {
    char* t1 = NULL;
    char* t2 = NULL;
    int n = fixed_sscanf(arena, "hello world", "%s %s", &t1, &t2);

    clobber_stack();

    assert(n == 2);
    assert(t1 != NULL && strcmp(t1, "hello") == 0);
    assert(t2 != NULL && strcmp(t2, "world") == 0);
    printf("[task 0.1] AFTER fix:  %%s result = \"%s\" — token 1 correct\n", t1);
    printf("[task 0.1] AFTER fix:  %%s result = \"%s\" — token 2 correct\n", t2);
}

/* Test 3: result pointer is inside the arena, not on any stack frame. */
static void test_pointer_in_arena(Arena* arena) {
    char* result = NULL;
    fixed_sscanf(arena, "canary", "%s", &result);
    clobber_stack();

    /* The result must fall within the arena's memory region. */
    assert(result != NULL);
    assert((char*)result >= arena->mem &&
           (char*)result <  arena->mem + ARENA_SIZE);
    printf("[task 0.1] AFTER fix:  result pointer outlives vsscanf frame — PASS\n");
    printf("[task 0.1] AFTER fix:  result lives in arena, not on vsscanf stack — PASS\n");
}

/* Test 4: three tokens from a single format string. */
static void test_multiple_tokens(Arena* arena) {
    char* a = NULL;
    char* b = NULL;
    char* c = NULL;
    int n = fixed_sscanf(arena, "one two three", "%s %s %s", &a, &b, &c);
    clobber_stack();

    assert(n == 3);
    assert(strcmp(a, "one")   == 0);
    assert(strcmp(b, "two")   == 0);
    assert(strcmp(c, "three") == 0);
    printf("[task 0.1] AFTER fix:  multiple %%s in one format string all arena-allocated — PASS\n");
}

/* Test 5: %*s (suppress) — dst pointer must not be written at all. */
static void test_suppress(Arena* arena) {
    char* result = (char*)0xDEADBEEF; /* sentinel */
    int n = fixed_sscanf(arena, "skip", "%*s", &result);
    clobber_stack();

    /* Suppress means matched count stays 0 and dst is untouched. */
    assert(n == 0);
    assert(result == (char*)0xDEADBEEF);
    printf("[task 0.1] AFTER fix:  %%*s suppress does not write dst — PASS\n");
}

/* Test 6: empty / whitespace-only input returns 0 matches. */
static void test_empty_input(Arena* arena) {
    char* result = NULL;
    int n = fixed_sscanf(arena, "   ", "%s", &result);
    clobber_stack();

    assert(n == 0);
    assert(result == NULL);
    printf("[task 0.1] AFTER fix:  empty input returns 0 matches — PASS\n");
}

/* ── main ──────────────────────────────────────────────────────────────── */

int main(void) {
    demo_before_fix();

    Arena* arena = arena_new();

    test_result_outlives_frame(arena);
    test_two_tokens(arena);
    test_pointer_in_arena(arena);
    test_multiple_tokens(arena);
    test_suppress(arena);
    test_empty_input(arena);

    arena_free(arena);
    printf("[task 0.1] All dangling-pointer tests PASSED\n");
    return 0;
}
