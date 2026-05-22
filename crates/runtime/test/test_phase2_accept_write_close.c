#include "ty_net.h"
#include <assert.h>
#include <stdint.h>
#include <string.h>

#if defined(_WIN32)
#  include <winsock2.h>
#  include <ws2tcpip.h>
#  pragma comment(lib, "Ws2_32.lib")
typedef SOCKET test_sock_t;
#else
#  include <unistd.h>
#  include <sys/types.h>
#  include <sys/socket.h>
#  include <arpa/inet.h>
typedef int test_sock_t;
#endif

typedef struct TyResult_i32_i32 {
    uint8_t ok;
    int32_t value;
    int32_t err;
} TyResult_i32_i32;

extern TyResult_i32_i32 __ty_rt__Socket__write(void* task, TySocket* self, char* buf, int32_t len);

static void close_test_sock(test_sock_t s) {
#if defined(_WIN32)
    closesocket(s);
#else
    close(s);
#endif
}

int main(void) {
    ty_net_init();
    TyNetwork* net = ty_net_global();
    assert(net != NULL);

    TyResult_Listener_i32 l = {0};
    __ty_rt__Network__listen(NULL, net, "127.0.0.1:30379", &l);
    assert(l.ok == 1 && l.value != NULL);

    test_sock_t c = socket(AF_INET, SOCK_STREAM, 0);
    assert((int)c >= 0);

    struct sockaddr_in sa;
    memset(&sa, 0, sizeof(sa));
    sa.sin_family = AF_INET;
    sa.sin_port = htons(30379);
    sa.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
    assert(connect(c, (struct sockaddr*)&sa, sizeof(sa)) == 0);

    TyResult_Socket_i32 accepted = {0};
    __ty_rt__Listener__accept(NULL, l.value, &accepted);
    assert(accepted.ok == 1 && accepted.value != NULL);

    char msg[] = "phase2";
    TyResult_i32_i32 wr = __ty_rt__Socket__write(NULL, accepted.value, msg, (int32_t)strlen(msg));
    assert(wr.ok == 1);

    __ty_rt__Socket__close(NULL, accepted.value);
    __ty_rt__Listener__close(NULL, l.value);
    close_test_sock(c);
    ty_net_shutdown();
    return 0;
}
