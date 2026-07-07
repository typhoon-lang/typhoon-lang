#include "ty_net.h"
#include "ty_mem.h"
#include <assert.h>
#include <stdint.h>
#include <string.h>

#if defined(_WIN32)
# include <winsock2.h>
# include <ws2tcpip.h>
# pragma comment(lib, "Ws2_32.lib")
typedef SOCKET test_sock_t;
#else
# include <unistd.h>
# include <sys/types.h>
# include <sys/socket.h>
# include <arpa/inet.h>
typedef int test_sock_t;
#endif

/*
 * FIX (test-suite defect found in review): the real __ty_rt__Network__listen,
 * __ty_rt__Listener__accept, and __ty_rt__Socket__write all use the
 * void-return + out-pointer ABI convention (see ty_net.c), matching
 * needs_out_result_abi's aggregate-return lowering — they do NOT return
 * TyResult_* structs by value. The previous version of this test called
 * them with the wrong argument count/convention and would not compile
 * against the real ty_net.h, or would corrupt memory if it somehow linked.
 * Also: Network__listen takes a TyStr* fat pointer, not a raw C string.
 */
// extern void __ty_rt__Socket__write(void* task, TySocket* self, char* buf, int32_t len, TyResult_i32_i32* out);

static void close_test_sock(test_sock_t s) {
#if defined(_WIN32)
  closesocket(s);
#else
  close(s);
#endif
}

/* Build a TyStr fat pointer over a C string literal/buffer. */
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

  TyStr addr = make_str("127.0.0.1:30379");
  TyResult_Listener_i32 l;
  __ty_rt__Network__listen(NULL, net, &addr, &l);
  assert(l.tag == 0 && l.value != NULL);

  test_sock_t c = socket(AF_INET, SOCK_STREAM, 0);
  assert((int)c >= 0);

  struct sockaddr_in sa;
  memset(&sa, 0, sizeof(sa));
  sa.sin_family = AF_INET;
  sa.sin_port = htons(30379);
  sa.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
  assert(connect(c, (struct sockaddr*)&sa, sizeof(sa)) == 0);

  TyResult_Socket_i32 accepted;
  __ty_rt__Listener__accept(NULL, l.value, &accepted);
  assert(accepted.tag == 0 && accepted.value != NULL);

  char msg[] = "phase2";
  TyResult_i32_i32 wr;
  __ty_rt__Socket__write(NULL, accepted.value, msg, (int32_t)strlen(msg), &wr);
  assert(wr.tag == 0);

  __ty_rt__Socket__close(NULL, accepted.value);
  __ty_rt__Listener__close(NULL, l.value);
  close_test_sock(c);
  ty_net_shutdown();
  return 0;
}
