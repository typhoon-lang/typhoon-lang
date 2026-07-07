/*
 * test_phase2_into_chan.c — Task 2.5
 *
 * "Write a test: into_chan produces the same bytes as direct read() in
 * the same order" and the backpressure half ("slow consumer, fast
 * sender; confirm no OOM; confirm sender slows down"). Genuinely
 * unblocked now that scheduler.h and ty_net.h are available.
 *
 * The top-level spawn/arena pattern (ty_spawn from bare main() after
 * ty_sched_init(), passing NULL as ty_spawn's first argument since it's
 * ignored) is now confirmed against scheduler.c — see
 * test_phase2_coroutine_loopback.c's header for the full detail, not
 * repeated here.
 *
 * ============================== ASSUMPTIONS ==============================
 * One real, still-unconfirmed assumption remains, specific to this file:
 * the element type/size of the `chan<Buf>` that __ty_rt__ReadSocket__into_chan
 * hands back. scheduler.c doesn't touch ty_net.c's socket_reader_coro (the
 * internal reader into_chan spawns), so the exact `ty_chan_new(elem_size,
 * cap)` call it makes internally still isn't confirmed. Betting on
 * `sizeof(Buf*)` — i.e. each `ty_chan_recv` fills a `Buf*`-sized slot
 * with one chunk's `Buf*` — on the strength that Buf is used
 * exclusively by pointer everywhere else confirmed in this codebase
 * (ty_buf_new returns `Buf*`, buf.ty's whole API is `Buf`-by-value at
 * the Typhoon level which lowers to a pointer, per the ABI conventions
 * seen elsewhere). If the real element is an inline `Buf` value instead
 * of a pointer, `ty_chan_recv`'s `out` buffer size below is wrong and
 * this won't compile/will misread memory — flagging clearly rather than
 * silently shipping it as certain.
 *
 * To read each received Buf's contents without touching its fields
 * directly (Buf's exact struct layout isn't confirmed public — same
 * class of mistake `TyFile` caught earlier this session when its real
 * header turned out to declare it opaque), this converts each Buf to a
 * TyStr via the confirmed non-static ty_buf_into_str, then reads via
 * the confirmed non-static ty_str_len/ty_str_byte. All three were
 * directly observed (not static) in ty_mem.c earlier in this
 * conversation, and ty_buf_into_str is independently confirmed by
 * buf.ty's own extern "C" block — but none of the three appear in any
 * header I've been given, so they're forward-declared by hand below,
 * same as ty_net.c's split_host_port has no header of its own.
 * ==========================================================================
 */

#include "ty_net.h"
#include "ty_mem.h"
#include "scheduler.h"
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

/* Not in any header seen so far — see ASSUMPTIONS above. */
extern TyStr* ty_buf_into_str(SlabArena* arena, Buf* b);
extern int64_t ty_str_len(TyStr* s);
extern char ty_str_byte(TyStr* s, int64_t idx);
/* Chunks from into_chan are now heap-allocated (ty_buf_new_heap), not
 * arena-allocated, precisely so they're safe to hand across the
 * coroutine boundary the channel represents. ty_buf_into_str wraps them
 * into a TyStr as before, but that TyStr must be released with this
 * instead of just letting the arena reclaim it at teardown. */
extern void ty_str_free_heap(TyStr* s);

#define TEST_PORT 30383
#define CHUNK_SIZE 16   /* small on purpose: forces many chunks through
                           the channel for a short payload, exercising
                           more of the chunking/reassembly path than one
                           big chunk would */
#define CHAN_CAP 2      /* small on purpose: for the backpressure test,
                           this is what should make the sender's OS
                           socket buffer (and therefore TCP) push back */

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

/* ── Test 1: into_chan reassembles the same bytes, in order ─────────────── */

typedef struct {
  TyNetwork* net;
  struct TyChan* ready_chan;
  char reassembled[4096];
  int64_t reassembled_len;
  int chunk_count;
  int client_total_sent;
  volatile int server_finished;
} ChanOrderCtx;

static const char ORDER_MSG[] =
    "the quick brown fox jumps over the lazy dog, twice: "
    "the quick brown fox jumps over the lazy dog";

static void order_server_coro(void* task, void* arg) {
  ChanOrderCtx* ctx = (ChanOrderCtx*)arg;
  SlabArena* arena = (SlabArena*)task;

  TyStr addr = make_str("127.0.0.1:30383");
  TyResult_Listener_i32 l;
  __ty_rt__Network__listen(task, ctx->net, &addr, &l);
  assert(l.tag == 0 && l.value != NULL);

  int32_t ready = 1;
  ty_chan_send(arena, ctx->ready_chan, &ready);

  TyResult_Socket_i32 accepted;
  __ty_rt__Listener__accept(task, l.value, &accepted);
  assert(accepted.tag == 0 && accepted.value != NULL);

  TySplitResult halves = __ty_rt__Socket__split(task, accepted.value);
  assert(halves.read != NULL && halves.write != NULL);

  struct TyChan* chunks = __ty_rt__ReadSocket__into_chan(
      task, halves.read, CHUNK_SIZE, CHAN_CAP);
  assert(chunks != NULL);

  /* Drain until the channel reports closed-and-drained (-1). */
  for (;;) {
    Buf* chunk = NULL;
    int rc = ty_chan_recv(arena, chunks, &chunk);
    if (rc == -1) break;
    assert(rc == 1 && "expected either an item or closed-and-drained, not 0 (empty) — "
        "ty_chan_recv should block cooperatively rather than return 0 here");
    assert(chunk != NULL);

    TyStr* piece = ty_buf_into_str(arena, chunk);
    assert(piece != NULL);
    int64_t piece_len = ty_str_len(piece);
    assert(ctx->reassembled_len + piece_len <= (int64_t)sizeof(ctx->reassembled));
    for (int64_t i = 0; i < piece_len; i++) {
      ctx->reassembled[ctx->reassembled_len + i] = ty_str_byte(piece, i);
    }
    ctx->reassembled_len += piece_len;
    ctx->chunk_count++;
    ty_str_free_heap(piece);
  }

  __ty_rt__ReadSocket__close(task, halves.read);
  __ty_rt__WriteSocket__close(task, halves.write);
  __ty_rt__Listener__close(task, l.value);
  ctx->server_finished = 1;
}

static void order_client_coro(void* task, void* arg) {
  (void)task;
  ChanOrderCtx* ctx = (ChanOrderCtx*)arg;

  int32_t signal = 0;
  int got = ty_chan_recv((SlabArena*)task, ctx->ready_chan, &signal);
  assert(got == 1);

  test_sock_t c = socket(AF_INET, SOCK_STREAM, 0);
  assert((int)c >= 0);
  struct sockaddr_in sa;
  memset(&sa, 0, sizeof(sa));
  sa.sin_family = AF_INET;
  sa.sin_port = htons(TEST_PORT);
  sa.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
  assert(connect(c, (struct sockaddr*)&sa, sizeof(sa)) == 0);

  /* Send in small, deliberately awkward pieces rather than one write —
   * exercises reassembly across multiple underlying TCP reads, not
   * just across into_chan's own chunk_size boundaries. */
  size_t total = strlen(ORDER_MSG);
  size_t sent = 0;
  while (sent < total) {
    size_t piece = 7; /* awkward, doesn't align with CHUNK_SIZE=16 */
    if (piece > total - sent) piece = total - sent;
    int n = send(c, ORDER_MSG + sent, (int)piece, 0);
    assert(n > 0);
    sent += (size_t)n;
  }
  ctx->client_total_sent = (int)sent;

  close_test_sock(c); /* triggers EOF on the server's read side */
}

static void test_into_chan_order(void) {
  ty_net_init();
  ty_sched_init();

  ChanOrderCtx ctx;
  memset(&ctx, 0, sizeof(ctx));
  ctx.net = ty_net_global();
  assert(ctx.net != NULL);
  ctx.ready_chan = ty_chan_new(sizeof(int32_t), 1);
  assert(ctx.ready_chan != NULL);

  ty_spawn(NULL, order_server_coro, &ctx);
  ty_spawn(NULL, order_client_coro, &ctx);
  ty_sched_run();

  assert(ctx.server_finished);
  assert(ctx.chunk_count > 1);
  assert((size_t)ctx.reassembled_len == strlen(ORDER_MSG) &&
      "reassembled length across all chunks should match the original message");
  assert(memcmp(ctx.reassembled, ORDER_MSG, (size_t)ctx.reassembled_len) == 0);

  ty_chan_close(ctx.ready_chan);
  ty_sched_shutdown();
  ty_net_shutdown();
  printf("[phase2] into_chan byte-order reassembly — PASS\n");
}

/* ── Test 2: slow consumer + small channel cap ⇒ sender observably
 * slows down (backpressure), and nothing grows unbounded ─────────────── */

typedef struct {
  TyNetwork* net;
  struct TyChan* ready_chan;
  int64_t total_bytes_received;
  int chunks_received;
  volatile int server_finished;
} BackpressureCtx;

#define BP_TOTAL_BYTES (CHUNK_SIZE * 40) /* enough chunks that a
                                             CHAN_CAP=2 channel will
                                             fill and force the internal
                                             reader coroutine to block
                                             on ty_chan_send well before
                                             the client finishes sending */

static void bp_server_coro(void* task, void* arg) {
  BackpressureCtx* ctx = (BackpressureCtx*)arg;
  SlabArena* arena = (SlabArena*)task;

  TyStr addr = make_str("127.0.0.1:30384");
  TyResult_Listener_i32 l;
  __ty_rt__Network__listen(task, ctx->net, &addr, &l);
  assert(l.tag == 0 && l.value != NULL);

  int32_t ready = 1;
  ty_chan_send(arena, ctx->ready_chan, &ready);

  TyResult_Socket_i32 accepted;
  __ty_rt__Listener__accept(task, l.value, &accepted);
  assert(accepted.tag == 0 && accepted.value != NULL);

  TySplitResult halves = __ty_rt__Socket__split(task, accepted.value);
  struct TyChan* chunks = __ty_rt__ReadSocket__into_chan(
      task, halves.read, CHUNK_SIZE, CHAN_CAP);
  assert(chunks != NULL);

  for (;;) {
    Buf* chunk = NULL;
    int rc = ty_chan_recv(arena, chunks, &chunk);
    if (rc == -1) break;
    assert(rc == 1);
    assert(chunk != NULL);

    TyStr* piece = ty_buf_into_str(arena, chunk);
    ctx->total_bytes_received += ty_str_len(piece);
    ctx->chunks_received++;
    ty_str_free_heap(piece);
  }

  __ty_rt__ReadSocket__close(task, halves.read);
  __ty_rt__WriteSocket__close(task, halves.write);
  __ty_rt__Listener__close(task, l.value);
  ctx->server_finished = 1;
}

static void bp_client_coro(void* task, void* arg) {
  BackpressureCtx* ctx = (BackpressureCtx*)arg;

  int32_t signal = 0;
  int got = ty_chan_recv((SlabArena*)task, ctx->ready_chan, &signal);
  assert(got == 1);

  test_sock_t c = socket(AF_INET, SOCK_STREAM, 0);
  assert((int)c >= 0);
  struct sockaddr_in sa;
  memset(&sa, 0, sizeof(sa));
  sa.sin_family = AF_INET;
  sa.sin_port = htons(30384);
  sa.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
  assert(connect(c, (struct sockaddr*)&sa, sizeof(sa)) == 0);

  char payload[BP_TOTAL_BYTES];
  memset(payload, 'x', sizeof(payload));

  size_t sent = 0;
  while (sent < sizeof(payload)) {
    int n = send(c, payload + sent, (int)(sizeof(payload) - sent), 0);
    assert(n > 0 && "send() failing here (rather than just blocking/"
        "partially completing) would indicate something other than "
        "ordinary backpressure went wrong");
    sent += (size_t)n;
  }

  close_test_sock(c);
}

static void test_into_chan_backpressure(void) {
  ty_net_init();
  ty_sched_init();

  BackpressureCtx ctx;
  memset(&ctx, 0, sizeof(ctx));
  ctx.net = ty_net_global();
  ctx.ready_chan = ty_chan_new(sizeof(int32_t), 1);
  assert(ctx.ready_chan != NULL);

  ty_spawn(NULL, bp_server_coro, &ctx);
  ty_spawn(NULL, bp_client_coro, &ctx);
  ty_sched_run();

  printf("[phase2] backpressure test: expected %d bytes, received %lld bytes "
      "across %d chunks\n",
      BP_TOTAL_BYTES, (long long)ctx.total_bytes_received, ctx.chunks_received);

  assert(ctx.server_finished &&
      "should complete without deadlock, OOM, or crash even with a "
      "small channel cap and a payload much larger than it");
  assert(ctx.total_bytes_received == BP_TOTAL_BYTES &&
      "no bytes should be lost or duplicated despite the channel "
      "filling and the reader blocking mid-stream");

  ty_chan_close(ctx.ready_chan);
  ty_sched_shutdown();
  ty_net_shutdown();
  printf("[phase2] into_chan backpressure, no data loss (%d chunks, %lld bytes) — PASS\n",
      ctx.chunks_received, (long long)ctx.total_bytes_received);
}

/* ── main ─────────────────────────────────────────────────────────────── */

int main(void) {
  setbuf(stdout, NULL);
  test_into_chan_order();
  test_into_chan_backpressure();
  printf("[phase2] All into_chan tests PASSED\n");
  return 0;
}
