#include "ty_io.h"
#include "ty_mem.h"
#include <assert.h>
#include <string.h>

/*
 * FIX (test-suite defect found in review): ty_buf_push_str takes a
 * TyStr* fat pointer (struct { char* ptr; int32_t len; }), not a raw
 * char*. ty_buf_into_str returns TyStr*, not char* — TyStr is not a
 * null-terminated C string layout, so indexing it directly (s[0], etc.)
 * reads raw struct bytes rather than string data. This version wraps
 * the buffer in a proper TyStr and reads it back via s->ptr/s->len
 * (equivalently ty_str_len/ty_str_byte).
 *
 * NOTE: this test still does not exercise ty_printf itself — that
 * function was not present in any ty_io.c reviewed. It only verifies
 * the underlying Buf/TyStr growth path (ty_buf_push_str /
 * ty_buf_into_str) that ty_printf would presumably build on. A true
 * ty_printf test still needs to be written once that function's
 * actual location/signature is confirmed.
 */

int main(void) {
    SlabArena* arena = slab_arena_new();
    assert(arena != NULL);

    Buf* out = ty_buf_new(arena);
    assert(out != NULL);

    char big[12001];
    for (int i = 0; i < 12000; i++) big[i] = 'a';
    big[12000] = '\0';

    TyStr big_str;
    big_str.ptr = big;
    big_str.len = 12000;

    ty_buf_push_str(arena, out, &big_str);
    assert((int)out->len == 12000);

    TyStr* s = ty_buf_into_str(arena, out);
    assert(s != NULL);
    assert(ty_str_len(s) == 12000);
    assert(ty_str_byte(s, 0) == 'a');
    assert(ty_str_byte(s, 11999) == 'a');
    /* Equivalent direct-field checks, since s->ptr is not guaranteed
     * to be a null-terminated C string beyond s->len bytes: */
    assert(s->len == 12000);
    assert(s->ptr[0] == 'a' && s->ptr[11999] == 'a');

    slab_arena_free(arena);
    return 0;
}
