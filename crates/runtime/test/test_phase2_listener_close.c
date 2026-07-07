#include "ty_net.h"
#include "ty_mem.h"
#include <assert.h>
#include <stddef.h>
#include <string.h>

/* FIX: __ty_rt__Network__listen is void-return + out-pointer, and takes
 * a TyStr* rather than a raw C string. See test_phase2_accept_write_close.c
 * for the full rationale. */
static TyStr make_str(const char* s) {
  TyStr str;
  str.ptr = (char*)s;
  str.len = (int32_t)strlen(s);
  return str;
}

int main(void) {
    ty_net_init();

    TyNetwork* net = ty_net_global();
    assert(net != NULL);

    TyStr addr = make_str("127.0.0.1:0");
    TyResult_Listener_i32 l;
    __ty_rt__Network__listen(NULL, net, &addr, &l);
    assert(l.tag == 0);
    assert(l.value != NULL);

    __ty_rt__Listener__close(NULL, l.value);
    ty_net_shutdown();
    return 0;
}
