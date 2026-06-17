/*
 * test_phase4_net_fdset.c — Verify per-worker TyFdSet tracking in ty_net
 *
 * Tests that __ty_rt__Network__listen and __ty_rt__Listener__accept register
 * fds in the current worker's TyFdSet, and that __ty_rt__Listener__close
 * and __ty_rt__Socket__close remove them.
 *
 * Note: these tests run without the full scheduler (no worker threads),
 * so ty_sched_current_worker() returns NULL. The ty_net functions handle
 * this gracefully (they check for NULL worker before calling ty_fdset_add).
 * We verify the networking operations themselves succeed and the fd
 * lifecycle (create → close) works without the global registry.
 */

#include "ty_net.h"
#include "scheduler.h"
#include "platform.h"
#include <assert.h>
#include <stdio.h>
#include <string.h>

#if defined(_WIN32)
# include <winsock2.h>
# include <ws2tcpip.h>
#else
# include <unistd.h>
# include <sys/types.h>
# include <sys/socket.h>
# include <arpa/inet.h>
# include <errno.h>
#endif

/* Helper: create listener on ephemeral port, return actual port.
 * Uses raw OS socket to find the assigned port after listen on port 0. */
static int listen_ephemeral(TyNetwork* net, TyResult_Listener_i32* _out) {
  /* Use a known port range for testing — 31000+ to avoid conflicts */
  static int next_port = 31000;
  int port = next_port++;
  TyResult_Listener_i32 out;

  out = __ty_rt__Network__listen(NULL, net, "127.0.0.1:0");
  if (!out.tag) return -1;

  /* To get the ephemeral port, we'd need to access the listener's fd,
   * but TyListener is opaque. Instead, just use port 0 and connect
   * to port 0 which the OS assigns. We use a helper: bind a raw socket
   * on port 0 to find an available port, then close it and use that port.
   * This is a simple workaround. */

  /* Actually, just close the port-0 listener and re-listen on a known port. */
  __ty_rt__Listener__close(NULL, out.value);
  // memset(out, 0, sizeof(out));

  char addr[64];
  snprintf(addr, sizeof(addr), "127.0.0.1:%d", port);
  out = __ty_rt__Network__listen(NULL, net, addr);
  if (!out.tag) {
    /* Port in use — try next */
    port = next_port++;
    snprintf(addr, sizeof(addr), "127.0.0.1:%d", port);
    out = __ty_rt__Network__listen(NULL, net, addr);
  }
  return out.tag ? port : -1;
}

/* ── Test 1: listen creates listener and close frees it ─────────────────── */

static void test_listen_close(void) {
  ty_net_init();

  TyNetwork* net = ty_net_global();
  assert(net != NULL);

  TyResult_Listener_i32 l = __ty_rt__Network__listen(NULL, net, "127.0.0.1:0");
  assert(l.tag == 1);
  assert(l.value != NULL);

  /* Close listener — should not crash, no global registry to walk */
  __ty_rt__Listener__close(NULL, l.value);

  ty_net_shutdown();
  printf("[phase4] net listen/close — PASS\n");
}

/* ── Test 2: accept creates socket and close frees it ──────────────────── */

static void test_accept_close(void) {
  ty_net_init();

  TyNetwork* net = ty_net_global();

  TyResult_Listener_i32 l = {0};
  int port = listen_ephemeral(net, &l);
  assert(port > 0);
  assert(l.tag == 1);

  /* Connect a client */
#if defined(_WIN32)
  SOCKET c = socket(AF_INET, SOCK_STREAM, 0);
  assert(c != INVALID_SOCKET);
#else
  int c = socket(AF_INET, SOCK_STREAM, 0);
  assert(c >= 0);
#endif

  struct sockaddr_in sa;
  memset(&sa, 0, sizeof(sa));
  sa.sin_family = AF_INET;
  sa.sin_port = htons((uint16_t)port);
  sa.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
  int rc = connect(c, (struct sockaddr*)&sa, sizeof(sa));
  assert(rc == 0);

  /* Accept */
  TyResult_Socket_i32 accepted = __ty_rt__Listener__accept(NULL, l.value);
  assert(accepted.tag == 1);
  assert(accepted.value != NULL);

  /* Close socket — should not crash */
  __ty_rt__Socket__close(NULL, accepted.value);

  /* Close listener */
  __ty_rt__Listener__close(NULL, l.value);

  /* Close client socket */
#if defined(_WIN32)
  closesocket(c);
#else
  close(c);
#endif

  ty_net_shutdown();
  printf("[phase4] net accept/close — PASS\n");
}

/* ── Test 3: listen on invalid address returns error ───────────────────── */

static void test_listen_invalid_addr(void) {
  ty_net_init();
  TyNetwork* net = ty_net_global();

  TyResult_Listener_i32 l = __ty_rt__Network__listen(NULL, net, "not_a_valid_addr");
  assert(l.tag == 0);

  TyResult_Listener_i32 l2 = __ty_rt__Network__listen(NULL, net, ":");
  assert(l2.tag == 0);

  ty_net_shutdown();
  printf("[phase4] net listen invalid addr — PASS\n");
}

/* ── Test 4: multiple sequential listen/close cycles ────────────────────── */

static void test_sequential_listen_close(void) {
  ty_net_init();
  TyNetwork* net = ty_net_global();

TyResult_Listener_i32 l;
  for (int i = 0; i < 10; i++) {
    l = __ty_rt__Network__listen(NULL, net, "127.0.0.1:0");
    assert(l.tag == 1);
    __ty_rt__Listener__close(NULL, l.value);
  }

  ty_net_shutdown();
  printf("[phase4] net sequential listen/close (10 cycles) — PASS\n");
}

/* ── Test 5: socket write and close ─────────────────────────────────────── */

static void test_socket_write_close(void) {
  ty_net_init();
  TyNetwork* net = ty_net_global();

  TyResult_Listener_i32 l = {0};
  int port = listen_ephemeral(net, &l);
  assert(port > 0);

#if defined(_WIN32)
  SOCKET c = socket(AF_INET, SOCK_STREAM, 0);
  assert(c != INVALID_SOCKET);
#else
  int c = socket(AF_INET, SOCK_STREAM, 0);
  assert(c >= 0);
#endif

  struct sockaddr_in sa;
  memset(&sa, 0, sizeof(sa));
  sa.sin_family = AF_INET;
  sa.sin_port = htons((uint16_t)port);
  sa.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
  connect(c, (struct sockaddr*)&sa, sizeof(sa));

  TyResult_Socket_i32 accepted = __ty_rt__Listener__accept(NULL, l.value);
  assert(accepted.tag == 1);

  /* Write data through the socket */
  char msg[] = "hello phase4";
  TyResult_i32_i32 wr = __ty_rt__Socket__write(NULL, accepted.value, msg, (int32_t)strlen(msg));
  assert(wr.tag == 1);
  assert(wr.value == (int32_t)strlen(msg));

  /* Close everything */
  __ty_rt__Socket__close(NULL, accepted.value);
  __ty_rt__Listener__close(NULL, l.value);

#if defined(_WIN32)
  closesocket(c);
#else
  close(c);
#endif

  ty_net_shutdown();
  printf("[phase4] net socket write/close — PASS\n");
}

/* ── main ─────────────────────────────────────────────────────────────────── */

int main(void) {
  test_listen_close();
  test_accept_close();
  test_listen_invalid_addr();
  test_sequential_listen_close();
  test_socket_write_close();
  printf("[phase4] All net fdset tests PASSED\n");
  return 0;
}
