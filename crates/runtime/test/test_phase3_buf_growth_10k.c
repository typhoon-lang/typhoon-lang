/*
 * test_phase3_buf_growth_10k.c — Task 3.3 (adapted)
 *
 * The checklist item is "print a 10,000-byte string via ty_printf and
 * confirm full output received." That function doesn't exist — I
 * checked io.ty, and formatting moved to the Typhoon stdlib level
 * entirely: Stdout.printf/print/println write straight into a Buf via
 * ty_buf_push_str/ty_buf_push_byte. There's no StackBuf, no sbuf_*, no
 * ty_printf anywhere in C to migrate or test directly.
 *
 * Stdout.printf is a Typhoon method (io.ty), not a C-callable symbol,
 * so a plain C test can't invoke it — that needs an actual .ty program
 * compiled and run (or a Typhoon-level test harness, if one exists).
 * What THIS test does instead is exercise the underlying C mechanism
 * that printf/println/print all sit on top of: pushing a large amount
 * of data through Buf (growth/realloc path in ty_mem.c's ty_buf_grow)
 * and out through ty_buf_into_str, confirming no truncation or data
 * loss. This is the same scope the doc's own "FIXED" note describes
 * for test_phase3_large_sprintf.c — it confirms the buffer-growth path,
 * not ty_printf's formatting logic (which lives in io.ty and would need
 * a Typhoon-level test — see the printf %d/%s spec-parsing loop itself
 * as a separate, not-yet-tested piece).
 *
 * Struct layouts below are copied from ty_mem.c's real definitions
 * (TyStr confirmed at ty_mem.c:401-415: `char* ptr; int32_t len;`).
 * Buf's internal fields are NOT reproduced since every access here goes
 * through the public API (ty_buf_new/push_byte/push_str/into_str) —
 * only Buf's forward declaration is needed.
 */

#include <assert.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "ty_io.h" /* real header: gives us the actual SlabArena/Buf opaque
                       typedefs, so this doesn't fight test_phase3_file_*.c's
                       copies if they're ever compiled together */

typedef struct TyStr {
    char* ptr;
    int32_t len;
} TyStr;

extern SlabArena* slab_arena_new(void);
extern void slab_arena_free(SlabArena* arena);

extern Buf* ty_buf_new(SlabArena* arena);
extern void ty_buf_push_str(SlabArena* arena, Buf* b, TyStr* s);
extern void ty_buf_push_byte(SlabArena* arena, Buf* b, char c);
extern TyStr* ty_buf_into_str(SlabArena* arena, Buf* b);
extern int64_t ty_str_len(TyStr* s);
extern char ty_str_byte(TyStr* s, int64_t idx);

#define TARGET_SIZE 10000
#define PUSH_CHUNK 137 /* deliberately awkward size so growth doesn't
                          line up neatly with power-of-2 cap doublings */

int main(void) {
    SlabArena* arena = slab_arena_new();
    assert(arena && "slab_arena_new() failed");

    Buf* b = ty_buf_new(arena);
    assert(b && "ty_buf_new() failed");

    /* Build a deterministic 10,000+ byte string in awkward-sized
     * chunks, so any off-by-one in ty_buf_grow's doubling logic shows
     * up as either truncation or a boundary corruption. */
    char chunk[PUSH_CHUNK + 1];
    int64_t pushed = 0;
    int chunk_idx = 0;
    while (pushed < TARGET_SIZE) {
        int this_len = PUSH_CHUNK;
        if (pushed + this_len > TARGET_SIZE) this_len = (int)(TARGET_SIZE - pushed);
        for (int i = 0; i < this_len; i++) {
            chunk[i] = (char)('A' + ((chunk_idx + i) % 26));
        }
        chunk[this_len] = '\0';

        TyStr piece;
        piece.ptr = chunk;
        piece.len = this_len;
        ty_buf_push_str(arena, b, &piece);

        pushed += this_len;
        chunk_idx++;
    }
    /* one extra byte via push_byte, exercising that path too */
    ty_buf_push_byte(arena, b, '!');
    pushed += 1;

    TyStr* result = ty_buf_into_str(arena, b);
    assert(result && "ty_buf_into_str returned NULL");
    assert(ty_str_len(result) == pushed &&
        "resulting Str length should match everything pushed — "
        "mismatch here means truncation or an off-by-one in growth");

    /* Spot-check content at the start, at a few growth-boundary-ish
     * offsets, and at the end (rather than a full byte-by-byte compare,
     * since the exact chunk boundaries are reconstructible from the
     * same pattern). */
    assert(ty_str_byte(result, 0) == 'A');
    assert(ty_str_byte(result, ty_str_len(result) - 1) == '!');

    int64_t mismatches = 0;
    int64_t check_chunk_idx = 0;
    int64_t offset = 0;
    while (offset < TARGET_SIZE) {
        int this_len = PUSH_CHUNK;
        if (offset + this_len > TARGET_SIZE) this_len = (int)(TARGET_SIZE - offset);
        for (int i = 0; i < this_len; i++) {
            char expected = (char)('A' + ((check_chunk_idx + i) % 26));
            if (ty_str_byte(result, offset + i) != expected) mismatches++;
        }
        offset += this_len;
        check_chunk_idx++;
    }
    assert(mismatches == 0 &&
        "reassembled 10,000-byte content should exactly match what was pushed");

    printf("[ok] buf_growth_10k: pushed %lld bytes across %d chunks, "
        "read back correctly (0 mismatches)\n",
        (long long)pushed, chunk_idx);

    slab_arena_free(arena);
    printf("phase 3.3 buf-growth test passed (does NOT cover printf's %%d/%%s "
        "spec-parsing loop in io.ty — that's untested and Typhoon-level)\n");
    return 0;
}
