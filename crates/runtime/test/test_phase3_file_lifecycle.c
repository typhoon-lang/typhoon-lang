/*
 * test_phase3_file_lifecycle.c — Task 3.1
 *
 * Covers the checklist items:
 *   - open a nonexistent file -> error result, no crash
 *   - open an existing file -> success, read expected bytes back
 *   - close -> no ASAN/leak error
 *   - close called twice -> debug guard (TY_ASSERT) fires
 *
 * ============================== ASSUMPTIONS ==============================
 * UPDATED after seeing the real ty_io.h — two things changed from the
 * first draft:
 *
 * 1. ty_io.h declares TyFile as OPAQUE (`typedef struct TyFile TyFile;`,
 *    no field access). My first draft read `f->closed` directly, which
 *    is illegal against this header — a real test file including only
 *    ty_io.h can't see into TyFile at all. I've removed those checks
 *    below and rely only on black-box behavior (the double-close crash)
 *    to confirm close state, matching what the public API actually
 *    allows. If white-box field access is genuinely wanted for testing,
 *    that needs a private/internal header exposing the real struct, not
 *    a hand-copied redefinition in the test file (redefining `struct
 *    TyFile` here to peek at fields would create a second, possibly
 *    mismatched definition in a different translation unit — UB even if
 *    the field layout happens to match ty_io.c's).
 *
 * 2. ty_io.h does NOT declare __ty_rt__fs__open / __ty_rt__File__close /
 *    __ty_rt__File__read / __ty_rt__File__write anywhere. These aren't
 *    part of the public interface this header describes at all — per
 *    its own comment, ty_io.h only exposes Stdout/Stdin handles and raw
 *    sys_write/sys_read. That means either:
 *      (a) there's a separate internal header for these runtime entry
 *          points that I don't have, or
 *      (b) they're only ever called from LLVM-generated code via a
 *          `declare` in the emitted .ll, with no C-visible prototype
 *          anywhere, and a hand-written C test has to forward-declare
 *          them itself (what I'm doing below) — same way ty_net.c's own
 *          split_host_port has no header either.
 *    Worth confirming which one is true before trusting this file
 *    compiles against your real build — if (a), swap these forward
 *    declarations for `#include`ing the real internal header instead.
 *
 * The struct/enum shapes still not covered by any header (TyMode
 * ordinals, TyResult_FilePtr_i32, TyResult_i64_i32, TyStr) are copied
 * from what's inline in ty_io.c / ty_mem.c, not invented — same caveats
 * as before (TyMode ordinals are flagged ASSUMED in ty_io.c itself;
 * result `tag` polarity is inferred from ty_net.c's convention, not
 * independently confirmed for File results).
 *
 * UPDATE: paths below were originally hardcoded to /tmp/..., which
 * doesn't exist on Windows and made every open-for-create call fail
 * before the test logic even ran. Switched to plain relative filenames
 * (created in and cleaned up from the test binary's CWD), which works
 * the same way on POSIX and Windows.
 * ==========================================================================
 */

#include <assert.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <errno.h>

#if !defined(_WIN32)
#include <unistd.h>
#include <sys/wait.h>
#endif

#include "ty_io.h" /* real header: gives us SlabArena, Buf, TyFile (opaque),
                       TyStdout, TyStdin, ty_stdout_*, ty_sys_read/write */

/* Not declared in ty_io.h — see ASSUMPTIONS note 2 above. */
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

/* slab_arena_new/free aren't in ty_io.h either (likely ty_mem.h, not
 * yet shared) — forward-declared here for the same reason. */
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

/* --- sub-test 1: open a file that doesn't exist ------------------------- */
static void test_open_nonexistent(SlabArena* arena) {
    TyStr path = make_str("ty_phase3_test_definitely_does_not_exist_12345.txt");
    TyResult_FilePtr_i32 out;
    __ty_rt__fs__open((void*)arena, &path, TY_MODE_READ, &out);

    assert(out.tag != 0 && "opening a nonexistent file should be an error result");
    assert(out.err == ENOENT &&
        "expected ENOENT for a missing file — if this fails, check whether "
        "ty_mode_to_flags/open() on your platform surfaces a different errno");
    printf("[ok] open_nonexistent: tag=%d err=%d (ENOENT=%d)\n",
        out.tag, out.err, ENOENT);
}

/* --- sub-test 2: open/write/close, then reopen/read/close --------------- */
static void test_write_then_read(SlabArena* arena) {
    const char* path_str = "ty_phase3_test_roundtrip.txt";
    const char* content = "hello from phase 3\n";
    int32_t content_len = (int32_t)strlen(content);

    /* write */
    {
        TyStr path = make_str(path_str);
        TyResult_FilePtr_i32 open_out;
        __ty_rt__fs__open((void*)arena, &path, TY_MODE_CREATE, &open_out);
        assert(open_out.tag == 0 && "opening for create/write should succeed");
        TyFile* f = open_out.value;

        TyResult_i64_i32 write_out;
        __ty_rt__File__write((void*)arena, f, (char*)content, content_len, &write_out);
        assert(write_out.tag == 0 && "write should succeed");
        assert(write_out.value == content_len && "write should report full length written");

        __ty_rt__File__close((void*)arena, f);
        /* Can't check a "closed" flag here — TyFile is opaque per
         * ty_io.h. Black-box confirmation of close state comes from
         * the double-close sub-test below instead. */
    }

    /* read back */
    {
        TyStr path = make_str(path_str);
        TyResult_FilePtr_i32 open_out;
        __ty_rt__fs__open((void*)arena, &path, TY_MODE_READ, &open_out);
        assert(open_out.tag == 0 && "reopening for read should succeed");
        TyFile* f = open_out.value;

        char buf[256] = {0};
        TyResult_i64_i32 read_out;
        __ty_rt__File__read((void*)arena, f, buf, (int32_t)sizeof(buf), &read_out);
        assert(read_out.tag == 0 && "read should succeed");
        assert(read_out.value == content_len &&
            "read should return exactly the bytes written");
        assert(memcmp(buf, content, (size_t)content_len) == 0 &&
            "read bytes should match what was written");

        __ty_rt__File__close((void*)arena, f);
    }

    printf("[ok] write_then_read: round-tripped %d bytes\n", content_len);
    remove(path_str);
}

/* --- sub-test 3: closing twice should hit the debug guard ---------------- */
static void test_double_close_asserts(SlabArena* arena) {
#if defined(_WIN32)
    (void)arena;
    printf("[skip] double_close_asserts: no fork() on Windows in this draft — "
        "needs a Windows-native crash-detection equivalent "
        "(e.g. a subprocess + structured exception check).\n");
#else
    const char* path_str = "ty_phase3_test_doubleclose.txt";
    TyStr path = make_str(path_str);
    TyResult_FilePtr_i32 open_out;
    __ty_rt__fs__open((void*)arena, &path, TY_MODE_CREATE, &open_out);
    assert(open_out.tag == 0);
    TyFile* f = open_out.value;

    pid_t pid = fork();
    if (pid == 0) {
        /* child: close twice, second one should trip TY_ASSERT and abort */
        __ty_rt__File__close((void*)arena, f);
        __ty_rt__File__close((void*)arena, f);
        _exit(0); /* should never get here */
    }

    int status = 0;
    waitpid(pid, &status, 0);
    int crashed = WIFSIGNALED(status) || (WIFEXITED(status) && WEXITSTATUS(status) != 0);
    assert(crashed &&
        "second File__close should trip TY_ASSERT and abort the process — "
        "if this fails, TY_ASSERT may be compiled out (NDEBUG) in this build");

    printf("[ok] double_close_asserts: child terminated abnormally as expected "
        "(status=0x%x)\n", status);
    remove(path_str);
#endif
}

int main(void) {
    SlabArena* arena = slab_arena_new();
    assert(arena && "slab_arena_new() failed");

    test_open_nonexistent(arena);
    test_write_then_read(arena);
    test_double_close_asserts(arena);

    slab_arena_free(arena);
    printf("all phase 3.1 file-lifecycle tests passed\n");
    return 0;
}
