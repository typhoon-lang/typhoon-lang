/*
 * test_phase3_file_chunked_read.c — Task 3.2
 *
 * Covers: "read a file in 4KB chunks" from the Phase 3 checklist.
 *
 * Also resolves a checklist ambiguity: the redesign doc's pseudocode
 * implied File__read might consume-and-return TyFile* like Socket.split()
 * does. Looking at the real ty_io.c, __ty_rt__File__read/write/seek all
 * take `TyFile* self` and mutate it in place via TyResult_i64_i32 (just
 * an int64 value, no TyFile in the result) — ownership is never
 * transferred out of read/write/seek, only open/close move it. So
 * "confirm TyFile is returned in every result path" doesn't apply the
 * way it would for Socket; this test exercises repeated reads against
 * the same `self` pointer across calls to confirm that holds up in
 * practice.
 *
 * UPDATED after seeing the real ty_io.h:
 *   - TyFile is opaque there (`typedef struct TyFile TyFile;`), so this
 *     no longer touches any TyFile fields directly — the "same self
 *     across calls" claim is confirmed purely by successful repeated
 *     reads returning correct data, not by inspecting a closed flag.
 *   - __ty_rt__fs__open / __ty_rt__File__read / __ty_rt__File__write
 *     aren't declared in ty_io.h at all — see the note in
 *     test_phase3_file_lifecycle.c for the two possibilities (a private
 *     header I don't have, vs. LLVM-only `declare`d symbols with no C
 *     header anywhere). Forward-declared here the same way ty_net.c's
 *     own split_host_port has no header.
 *   - TyMode ordinals, TyResult_FilePtr_i32/i64_i32, TyStr are still
 *     copied from ty_io.c/ty_mem.c's inline definitions, not invented —
 *     same caveats as before.
 *
 * UPDATE: path was originally hardcoded to /tmp/..., which doesn't
 * exist on Windows and made the create/write open fail before the
 * chunked-read logic ran at all. Switched to a plain relative filename
 * (created in and cleaned up from the test binary's CWD).
 */

#include <assert.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "ty_io.h"

typedef struct TyStr {
    char* ptr;
    int32_t len;
} TyStr;

typedef enum {
    TY_MODE_READ = 0,
    TY_MODE_WRITE = 1,
    TY_MODE_APPEND = 2,
    TY_MODE_READ_WRITE = 3,
    TY_MODE_CREATE = 4,
} TyMode;

typedef struct { int32_t tag; TyFile* value; int32_t err; } TyResult_FilePtr_i32;
typedef struct { int32_t tag; int64_t value; int32_t err; } TyResult_i64_i32;

extern SlabArena* slab_arena_new(void);
extern void slab_arena_free(SlabArena* arena);

extern void __ty_rt__fs__open(void* task, TyStr* path, TyMode mode,
    TyResult_FilePtr_i32* out);
extern void __ty_rt__File__close(void* task, TyFile* self);
extern void __ty_rt__File__read(void* task, TyFile* self, char* buf, int32_t cap,
    TyResult_i64_i32* out);
extern void __ty_rt__File__write(void* task, TyFile* self, char* buf, int32_t len,
    TyResult_i64_i32* out);

static TyStr make_str(const char* s) {
    TyStr str;
    str.ptr = (char*)s;
    str.len = (int32_t)strlen(s);
    return str;
}

#define CHUNK_SIZE 4096
#define TOTAL_SIZE (CHUNK_SIZE * 3 + 777) /* deliberately not a multiple of 4KB */

int main(void) {
    SlabArena* arena = slab_arena_new();
    assert(arena && "slab_arena_new() failed");

    const char* path_str = "ty_phase3_test_chunked.bin";

    /* Build deterministic, non-repeating content so any byte-shuffling
     * bug in the chunked read shows up as a mismatch rather than
     * accidentally reading correctly-shaped-but-wrong data. */
    char* expected = (char*)malloc(TOTAL_SIZE);
    assert(expected);
    for (int i = 0; i < TOTAL_SIZE; i++) {
        expected[i] = (char)((i * 31 + 7) & 0xFF);
    }

    /* write the whole thing in one call */
    {
        TyStr path = make_str(path_str);
        TyResult_FilePtr_i32 open_out;
        __ty_rt__fs__open((void*)arena, &path, TY_MODE_CREATE, &open_out);
        assert(open_out.tag == 0 && "create/write open should succeed");
        TyFile* f = open_out.value;

        TyResult_i64_i32 write_out;
        __ty_rt__File__write((void*)arena, f, expected, TOTAL_SIZE, &write_out);
        assert(write_out.tag == 0 && "write should succeed");
        assert(write_out.value == TOTAL_SIZE && "write should report full length");

        __ty_rt__File__close((void*)arena, f);
    }

    /* read it back in 4KB chunks against the *same* TyFile* across calls */
    {
        TyStr path = make_str(path_str);
        TyResult_FilePtr_i32 open_out;
        __ty_rt__fs__open((void*)arena, &path, TY_MODE_READ, &open_out);
        assert(open_out.tag == 0 && "read open should succeed");
        TyFile* f = open_out.value;

        char* actual = (char*)malloc(TOTAL_SIZE + CHUNK_SIZE); /* pad for last chunk */
        assert(actual);
        int64_t total_read = 0;
        int chunk_count = 0;

        for (;;) {
            char chunk[CHUNK_SIZE];
            TyResult_i64_i32 read_out;
            __ty_rt__File__read((void*)arena, f, chunk, CHUNK_SIZE, &read_out);
            assert(read_out.tag == 0 && "each chunked read should succeed");
            if (read_out.value == 0) break; /* EOF */

            memcpy(actual + total_read, chunk, (size_t)read_out.value);
            total_read += read_out.value;
            chunk_count++;

            assert(chunk_count < 1000 && "runaway read loop — EOF never signaled");
        }

        assert(total_read == TOTAL_SIZE &&
            "total bytes read across all chunks should match what was written");
        assert(memcmp(actual, expected, TOTAL_SIZE) == 0 &&
            "reassembled content should exactly match what was written");
        /* Can't check f->closed — TyFile is opaque per ty_io.h. "Same
         * self across every read call" is confirmed by the reads all
         * succeeding and returning correct data above, not by a field
         * check. */

        printf("[ok] chunked_read: %d bytes in %d chunks (last chunk %d bytes)\n",
            TOTAL_SIZE, chunk_count, TOTAL_SIZE % CHUNK_SIZE);

        __ty_rt__File__close((void*)arena, f);
        free(actual);
    }

    free(expected);
    remove(path_str);
    slab_arena_free(arena);
    printf("all phase 3.2 chunked-read tests passed\n");
    return 0;
}
