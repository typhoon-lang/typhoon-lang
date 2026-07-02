#include "ty_net.h"
#include <assert.h>
#include <stddef.h>

int main(void) {
    ty_net_init();

    TyNetwork* net = ty_net_global();
    assert(net != NULL);

    TyResult_Listener_i32 l = __ty_rt__Network__listen(NULL, net, "127.0.0.1:0");
    assert(l.tag == 1);
    assert(l.value != NULL);

    __ty_rt__Listener__close(NULL, l.value);
    ty_net_shutdown();
    return 0;
}
