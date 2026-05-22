#include "ty_io.h"
#include "ty_mem.h"
#include <assert.h>
#include <string.h>

int main(void) {
    SlabArena* arena = slab_arena_new();
    assert(arena != NULL);

    Buf* out = ty_buf_new(arena);
    assert(out != NULL);

    char big[12001];
    for (int i = 0; i < 12000; i++) big[i] = 'a';
    big[12000] = '\0';

    int n = ty_sprintf(arena, out, "%s", big);
    assert(n == 12000);

    char* s = ty_buf_into_str(arena, out);
    assert(s != NULL);
    assert(strlen(s) == 12000);
    assert(s[0] == 'a' && s[11999] == 'a');

    slab_arena_free(arena);
    return 0;
}
