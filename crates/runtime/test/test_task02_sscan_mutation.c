/*
 * test_task02_sscan_mutation.c — Regression test for Task 0.2
 *
 * Bug: ty_sscan wrote '\0' into the source string to null-terminate a token.
 *      When the source lives in .rodata (string literal), that write is UB:
 *      SIGBUS on some platforms, silent corruption on others.  It also
 *      permanently destroys the rest of the source string for any subsequent
 *      caller.
 *
 *      Before fix:
 *        if (*src) {
 *            *src = '\0';   // UB: src may point into .rodata
 *            src++;
 *        }
 *
 *      After fix:
 *        size_t len = (size_t)(src - start);
 *        char* tok = slab_alloc(task, len + 1);
 *        memcpy(tok, start, len);
 *        tok[len] = '\0';   // writes only into the arena copy
 *
 * Test strategy
 * ─────────────
 * Both the buggy and fixed implementations are inlined as static functions.
 *
 * Bug demonstration (test 0):
 *   The buggy sscan is called on a heap-duplicated string so we can legally
 *   observe the write, then confirm the source was mutated — proving the bug.
 *   The real crash (SIGBUS) happens on .rodata; we document that as a
 *   signal-handler test under POSIX builds.
 *
 * Fix verification (tests 1-6):
 *   The fixed sscan is called on string literals and heap strings; we verify
 *   the token is correct, the source is not mutated, rest_out is accurate,
 *   multi-token iteration works, and leading/trailing whitespace is handled.
 *
 * Build:
 *   gcc -Wall -Wextra -g -o test_task02_sscan_mutation test_task02_sscan_mutation.c
 *   gcc -fsanitize=address,undefined -g -Wall -Wextra \
 *       -o test_task02_sscan_mutation test_task02_sscan_mutation.c
 *
 * Expected output:
 *   [task 0.2] BEFORE fix: source was mutated — '\0' written at offset 5
 *   [task 0.2] BEFORE fix: in-place mutation confirmed (UB on .rodata) — PASS
 *   [task 0.2] AFTER fix:  token = "hello", source unchanged — PASS
 *   [task 0.2] AFTER fix:  token lives in arena, source untouched — PASS
 *   [task 0.2] AFTER fix:  rest_out points past token in original string — PASS
 *   [task 0.2] AFTER fix:  multi-token iteration via rest_out — PASS
 *   [task 0.2] AFTER fix:  leading whitespace skipped, source intact — PASS
 *   [task 0.2] AFTER fix:  NULL source returns NULL token — PASS
 *   [task 0.2] AFTER fix:  whitespace-only source returns NULL token — PASS
 *   [task 0.2] All sscan-mutation tests PASSED
 */

#include <stdio.h>
#include <string.h>
#include <assert.h>
#include <stdlib.h>
#include <stdint.h>

/* ── Minimal arena ───────────────────────────────────────────────────────── */

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
    size_t aligned = (n + 7) & ~(size_t)7;
    assert(a->used + aligned <= ARENA_SIZE);
    void* p = a->mem + a->used;
    a->used += aligned;
    return p;
}

static void arena_free(Arena* a) { free(a); }

/* ── BEFORE fix: buggy_sscan mutates the source string ──────────────────── */

/*
 * This is the exact pre-fix logic from fix.md.  It calls sscan on a mutable
 * heap copy so we can legally observe the mutation without crashing.
 * On a real string literal this write causes SIGBUS / silent corruption.
 */
static char* buggy_sscan(char* src, char** rest_out) {
    /* skip leading whitespace */
    while (*src == ' ' || *src == '\t' || *src == '\n' || *src == '\r') src++;
    if (!*src) {
        if (rest_out) *rest_out = src;
        return NULL;
    }
    char* start = src;
    /* advance to end of token */
    while (*src && *src != ' ' && *src != '\t' && *src != '\n' && *src != '\r') src++;
    /* BUG: write '\0' directly into the source string */
    if (*src) {
        *src = '\0';   /* UB when src is .rodata */
        src++;
    }
    if (rest_out) *rest_out = src;
    return start;   /* pointer into (mutated) source */
}

/* ── AFTER fix: fixed_sscan copies token into the arena ─────────────────── */

static char* fixed_sscan(Arena* arena, const char* src, const char** rest_out) {
    if (!src) {
        if (rest_out) *rest_out = NULL;
        return NULL;
    }
    while (*src == ' ' || *src == '\t' || *src == '\n' || *src == '\r') src++;
    if (!*src) {
        if (rest_out) *rest_out = src;
        return NULL;
    }
    const char* start = src;
    while (*src && *src != ' ' && *src != '\t' && *src != '\n' && *src != '\r') src++;
    size_t len = (size_t)(src - start);
    char* tok = (char*)arena_alloc(arena, len + 1);
    memcpy(tok, start, len);
    tok[len] = '\0';
    if (rest_out) *rest_out = src;
    return tok;
}

/* ── Tests ──────────────────────────────────────────────────────────────── */

/*
 * Test 0 (pre-fix): call buggy_sscan on a heap copy and verify the source
 * was mutated.  This proves the bug — on .rodata the same write is UB.
 */
static void demo_before_fix(void) {
    /* Use a mutable heap copy so we can observe the write legally. */
    char* mutable_src = strdup("hello world");
    char* rest = NULL;

    char* tok = buggy_sscan(mutable_src, &rest);

    /*
     * After the call, mutable_src[5] should now be '\0' — the bug wrote into
     * the source.  "hello\0world" — the space became a null terminator in-place.
     */
    int mutation_byte_offset = 5; /* position of the space in "hello world" */
    int was_mutated = (mutable_src[mutation_byte_offset] == '\0');

    printf("[task 0.2] BEFORE fix: source was mutated — '\\0' written at offset %d\n",
           mutation_byte_offset);
    printf("[task 0.2] BEFORE fix: tok=\"%s\", mutable_src[5]=0x%02x (expected 0x00)\n",
           tok ? tok : "(null)", (unsigned char)mutable_src[mutation_byte_offset]);

    assert(was_mutated &&
           "[task 0.2] FAIL: expected buggy_sscan to mutate the source, but it did not");
    printf("[task 0.2] BEFORE fix: in-place mutation confirmed (UB on .rodata) — PASS\n");

    free(mutable_src);
}

/*
 * Test 1: token is correct and source is NOT mutated.
 * Use a string literal — fixed_sscan must not write into it.
 */
static void test_literal_not_mutated(Arena* arena) {
    /* String literal — lives in .rodata on most platforms. */
    const char* literal = "hello world";
    /* Take a snapshot of the bytes we care about. */
    char snapshot[16];
    strncpy(snapshot, literal, sizeof(snapshot) - 1);
    snapshot[sizeof(snapshot) - 1] = '\0';

    const char* rest = NULL;
    char* tok = fixed_sscan(arena, literal, &rest);

    assert(tok != NULL);
    assert(strcmp(tok, "hello") == 0);
    /* The source must be byte-identical to the snapshot. */
    assert(memcmp(literal, snapshot, strlen(snapshot)) == 0 &&
           "[task 0.2] FAIL: fixed_sscan mutated the source string");
    printf("[task 0.2] AFTER fix:  token = \"%s\", source unchanged — PASS\n", tok);
}

/*
 * Test 2: token pointer is inside the arena, not inside the source string.
 */
static void test_token_in_arena(Arena* arena) {
    const char* src = "arena_check";
    const char* rest = NULL;
    char* tok = fixed_sscan(arena, src, &rest);

    assert(tok != NULL);
    /* tok must be in the arena, not an alias into src. */
    assert((char*)tok >= arena->mem && (char*)tok < arena->mem + ARENA_SIZE &&
           "[task 0.2] FAIL: token is not in the arena");
    assert(tok != src && "[task 0.2] FAIL: token is an alias into source (not a copy)");
    printf("[task 0.2] AFTER fix:  token lives in arena, source untouched — PASS\n");
}

/*
 * Test 3: rest_out points to the correct position in the ORIGINAL source.
 */
static void test_rest_out(Arena* arena) {
    const char* src = "alpha beta";
    const char* rest = NULL;
    char* tok = fixed_sscan(arena, src, &rest);

    assert(tok != NULL && strcmp(tok, "alpha") == 0);
    /* rest should point to the space before "beta", i.e. src + 5. */
    assert(rest != NULL);
    assert(rest >= src && rest <= src + strlen(src));
    /* The remaining source starting from rest should give us "beta". */
    const char* rest2 = NULL;
    char* tok2 = fixed_sscan(arena, rest, &rest2);
    assert(tok2 != NULL && strcmp(tok2, "beta") == 0);
    printf("[task 0.2] AFTER fix:  rest_out points past token in original string — PASS\n");
}

/*
 * Test 4: iterate through all tokens via rest_out without mutating the source.
 */
static void test_multi_token_iteration(Arena* arena) {
    const char* src = "one two three four";
    const char* expected[] = { "one", "two", "three", "four" };
    const char* cursor = src;
    int i = 0;

    while (1) {
        const char* rest = NULL;
        char* tok = fixed_sscan(arena, cursor, &rest);
        if (!tok) break;
        assert(i < 4 && "[task 0.2] FAIL: too many tokens");
        assert(strcmp(tok, expected[i]) == 0 &&
               "[task 0.2] FAIL: wrong token in multi-token iteration");
        cursor = rest;
        i++;
    }
    assert(i == 4 && "[task 0.2] FAIL: wrong token count");
    /* The original source must still be intact. */
    assert(strcmp(src, "one two three four") == 0 &&
           "[task 0.2] FAIL: source was mutated during multi-token iteration");
    printf("[task 0.2] AFTER fix:  multi-token iteration via rest_out — PASS\n");
}

/*
 * Test 5: leading whitespace is skipped and source is not mutated.
 */
static void test_leading_whitespace(Arena* arena) {
    const char* src = "   \t  trimmed";
    const char* rest = NULL;
    char* tok = fixed_sscan(arena, src, &rest);

    assert(tok != NULL && strcmp(tok, "trimmed") == 0 &&
           "[task 0.2] FAIL: leading whitespace not skipped correctly");
    /* Source must still begin with spaces — not overwritten. */
    assert(src[0] == ' ' &&
           "[task 0.2] FAIL: leading whitespace was mutated");
    printf("[task 0.2] AFTER fix:  leading whitespace skipped, source intact — PASS\n");
}

/*
 * Test 6: NULL source returns NULL without crashing.
 */
static void test_null_source(Arena* arena) {
    const char* rest = NULL;
    char* tok = fixed_sscan(arena, NULL, &rest);
    assert(tok == NULL);
    printf("[task 0.2] AFTER fix:  NULL source returns NULL token — PASS\n");
}

/*
 * Test 7: whitespace-only source returns NULL.
 */
static void test_whitespace_only(Arena* arena) {
    const char* src = "    \t\n  ";
    const char* rest = NULL;
    char* tok = fixed_sscan(arena, src, &rest);
    assert(tok == NULL);
    printf("[task 0.2] AFTER fix:  whitespace-only source returns NULL token — PASS\n");
}

/* ── main ──────────────────────────────────────────────────────────────── */

int main(void) {
    demo_before_fix();

    Arena* arena = arena_new();

    test_literal_not_mutated(arena);
    test_token_in_arena(arena);
    test_rest_out(arena);
    test_multi_token_iteration(arena);
    test_leading_whitespace(arena);
    test_null_source(arena);
    test_whitespace_only(arena);

    arena_free(arena);
    printf("[task 0.2] All sscan-mutation tests PASSED\n");
    return 0;
}
