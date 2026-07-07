/*
 * test_phase2_write_read_roundtrip.c — closes a real gap in
 * test_phase2_accept_write_close.c, not a new checklist item on its own.
 *
 * test_phase2_accept_write_close.c calls __ty_rt__Socket__write and only
 * checks wr.tag == 0 — it never confirms the peer actually received the
 * right bytes. This test does: server writes via the confirmed
 * __ty_rt__Socket__write (proven working — this exact call, with this
 * exact signature, already compiles and passes in
 * test_phase2_accept_write_close.c and test_phase4_net_fdset.c), and the
 * client verifies via a plain OS-level recv() on its own raw socket.
 *
 * Deliberately does NOT attempt to call any Typhoon-side read function
 * (__ty_rt__Socket__read or __ty_rt__ReadSocket__read). The redesign doc
 * itself is inconsistent about which of those two names is real — Task
 * 2.3's checklist claims __ty_rt__Socket__read, Task 2.5's claims
 * __ty_rt__ReadSocket__read (a different function, taking a different
 * type) — and net.ty only exposes ReadSocket-based reading via
 * split()/into_chan(), never a direct Socket.read. Without ty_net.h I
 * can't tell which claim is right, or whether both exist, so this test
 * verifies correctness from the client side instead of guessing a
 * runtime-side read signature that might not compile.
 *
 * Still entirely sync-fallback-path (task == NULL everywhere), same as
 * the existing Phase 2 tests — does not exercise the coroutine
 * suspend/resume path Task 2.3's actual checklist item asks about.
 */

#include "ty_net.h"
#include "ty_mem.h"
#include <assert.h>
#include <stdint.h>
#include <string.h>
#include <stdio.h>

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

static void close_test_sock(test_sock_t s) {
#if defined(_WIN32)
  closesocket(s);
#else
  close(s);
#endif
}

static TyStr make_str(const char* s) {
  TyStr str;
  str.ptr = (char*)s;
  str.len = (int32_t)strlen(s);
  return str;
}

/* ── Test 1: single write, verify exact bytes and length on the client ──── */

static void test_write_verified_by_client_recv(void) {
  ty_net_init();
  TyNetwork* net = ty_net_global();
  assert(net != NULL);

  TyStr addr = make_str("127.0.0.1:30380");
  TyResult_Listener_i32 l;
  __ty_rt__Network__listen(NULL, net, &addr, &l);
  assert(l.tag == 0 && l.value != NULL);

  test_sock_t c = socket(AF_INET, SOCK_STREAM, 0);
  assert((int)c >= 0);

  struct sockaddr_in sa;
  memset(&sa, 0, sizeof(sa));
  sa.sin_family = AF_INET;
  sa.sin_port = htons(30380);
  sa.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
  assert(connect(c, (struct sockaddr*)&sa, sizeof(sa)) == 0);

  TyResult_Socket_i32 accepted;
  __ty_rt__Listener__accept(NULL, l.value, &accepted);
  assert(accepted.tag == 0 && accepted.value != NULL);

  const char msg[] = "roundtrip verification payload";
  int32_t msg_len = (int32_t)strlen(msg);
  TyResult_i32_i32 wr;
  __ty_rt__Socket__write(NULL, accepted.value, (char*)msg, msg_len, &wr);
  assert(wr.tag == 0);
  assert(wr.value == msg_len && "write should report the full length written");

  /* Client-side verification: plain OS recv(), no Typhoon runtime
   * involved on this end at all. */
  char buf[256] = {0};
  int received = recv(c, buf, (int)sizeof(buf), 0);
  assert(received == msg_len &&
    "client should receive exactly what the server wrote — this is the "
    "check test_phase2_accept_write_close.c never did");
  assert(memcmp(buf, msg, (size_t)msg_len) == 0 &&
    "received bytes should exactly match what Socket__write sent");

  __ty_rt__Socket__close(NULL, accepted.value);
  __ty_rt__Listener__close(NULL, l.value);
  close_test_sock(c);
  ty_net_shutdown();
  printf("[phase2] write/read roundtrip byte verification — PASS\n");
}

/* ── Test 2: multiple sequential writes arrive in order, undivided ──────── */

static void test_multiple_writes_arrive_in_order(void) {
  ty_net_init();
  TyNetwork* net = ty_net_global();
  assert(net != NULL);

  TyStr addr = make_str("127.0.0.1:30381");
  TyResult_Listener_i32 l;
  __ty_rt__Network__listen(NULL, net, &addr, &l);
  assert(l.tag == 0 && l.value != NULL);

  test_sock_t c = socket(AF_INET, SOCK_STREAM, 0);
  assert((int)c >= 0);

  struct sockaddr_in sa;
  memset(&sa, 0, sizeof(sa));
  sa.sin_family = AF_INET;
  sa.sin_port = htons(30381);
  sa.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
  assert(connect(c, (struct sockaddr*)&sa, sizeof(sa)) == 0);

  TyResult_Socket_i32 accepted;
  __ty_rt__Listener__accept(NULL, l.value, &accepted);
  assert(accepted.tag == 0 && accepted.value != NULL);

  /* Three separate Socket__write calls. TCP is a byte stream, not
   * message-based, so this also implicitly checks that nothing in
   * Socket__write's implementation is accidentally inserting framing,
   * padding, or truncating between calls. */
  const char* parts[3] = { "first-", "second-", "third" };
  for (int i = 0; i < 3; i++) {
    int32_t part_len = (int32_t)strlen(parts[i]);
    TyResult_i32_i32 wr;
    __ty_rt__Socket__write(NULL, accepted.value, (char*)parts[i], part_len, &wr);
    assert(wr.tag == 0);
    assert(wr.value == part_len);
  }

  char buf[256] = {0};
  int total_received = 0;
  /* Loop rather than a single recv(): TCP doesn't guarantee three
   * writes arrive as one recv() — only that bytes arrive in order.
   * Small payloads on loopback usually coalesce, but looping is the
   * correct way to assert this regardless of that OS-dependent detail. */
  const char* expected = "first-second-third";
  int expected_len = (int)strlen(expected);
  while (total_received < expected_len) {
    int n = recv(c, buf + total_received, (int)sizeof(buf) - total_received, 0);
    assert(n > 0 && "connection closed before all expected bytes arrived");
    total_received += n;
  }
  assert(total_received == expected_len);
  assert(memcmp(buf, expected, (size_t)expected_len) == 0 &&
    "three sequential writes should arrive concatenated, in order, undivided");

  __ty_rt__Socket__close(NULL, accepted.value);
  __ty_rt__Listener__close(NULL, l.value);
  close_test_sock(c);
  ty_net_shutdown();
  printf("[phase2] multiple writes arrive in order — PASS\n");
}

/* ── main ─────────────────────────────────────────────────────────────── */

int main(void) {
  test_write_verified_by_client_recv();
  test_multiple_writes_arrive_in_order();
  printf("[phase2] All write/read roundtrip tests PASSED\n");
  return 0;
}
