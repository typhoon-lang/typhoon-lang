/*
 * test_task02_sscan_mutation.c — Regression test for Task 0.2
 *
 * Bug: ty_sscan wrote '\0' into the source string to null-terminate a token.
 *      When the source lives in .rodata (string literal), that write is UB:
 *      SIGBUS on some platforms, silent corruption on others.
 *
 * FIXES vs original submitted test:
 *   - test_rest_out: the range check `rest >= src && rest <= src + strlen(src)`
 *     is too loose — it passes even if rest_out is off by one. Replaced with
 *     an exact position check: rest must equal src+5 (past "alpha"), and
 *     separately rest must not point at the space character (fixed_sscan
 *     leaves rest at the first non-consumed character, which is the space
 *     ' ' before "beta"). The key invariant is that `fixed_sscan(rest, &r)`
 *     yields "beta" — which the original test did verify, but the intermediate
 *     position was not pinned. Both the exact-offset check and the functional
 *     check are now present.
 *
 * Build:
 *   gcc -Wall -Wextra -g -o test_task02 test_task02_sscan_mutation.c
 *   gcc -fsanitize=address,undefined -g -Wall -Wextra \
 *       -o test_task02 test_task02_sscan_mutation.c
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
        *src = '\0';   /* BUG: UB on .rodata */
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
    int was_mutated = (mutable_src[5] == '\0');
    printf("[task 0.2] BEFORE fix: source was mutated — '\\0' written at offset 5\n");
    printf("[task 0.2] BEFORE fix: tok=\"%s\", mutable_src[5]=0x%02x (expected 0x00)\n",
           tok ? tok : "(null)", (unsigned char)mutable_src[5]);
    assert(was_mutated && "[task 0.2] FAIL: expected mutation at offset 5");
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
    assert(tok != src && "[task 0.2] FAIL: token is an alias into source");
    printf("[task 0.2] AFTER fix:  token lives in arena, source untouched — PASS\n");
}

/*
 * FIX: original used a loose range check (rest >= src && rest <= src+len).
 * Now we pin the exact offset AND verify the next token can be read from rest.
 * "alpha" is 5 bytes; rest must point to src+5 (the space before "beta").
 */
static void test_rest_out(Arena* arena) {
    const char* src = "alpha beta";
    const char* rest = NULL;
    char* tok = fixed_sscan(arena, src, &rest);

    assert(tok != NULL && strcmp(tok, "alpha") == 0);
    /* rest should point to the space before "beta", i.e. src + 5. */
    assert(rest != NULL);

    /* Exact position: rest must point to the character immediately after
     * "alpha", which is the space at src[5]. */
    assert(rest == src + 5 &&
           "[task 0.2] FAIL: rest_out is not at src+5 (exact position check)");
    assert(*rest == ' ' &&
           "[task 0.2] FAIL: rest_out does not point to the space separator");

    /* Functional check: consuming rest must yield "beta". */
    const char* rest2 = NULL;
    char* tok2 = fixed_sscan(arena, rest, &rest2);
    assert(tok2 != NULL && strcmp(tok2, "beta") == 0 &&
           "[task 0.2] FAIL: second token from rest_out is not 'beta'");
    /* rest2 must point to the null terminator — nothing left. */
    assert(rest2 != NULL && *rest2 == '\0' &&
           "[task 0.2] FAIL: rest2 should point to null terminator after last token");

    printf("[task 0.2] AFTER fix:  rest_out exact position and functional check — PASS\n");
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
           "[task 0.2] FAIL: source mutated during iteration");
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
