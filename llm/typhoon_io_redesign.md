# Typhoon IO Redesign Plan

**Status:** Draft v1.2  
**Scope:** `ty_io.c`, `ty_net.c`, `io.md`, `spec.md §14–16`  
**Depends on:** spec.md v0.1, scheduler.h, ty_mem.h, platform.h

---

## Table of Contents

1. [Context and motivation](#1-context-and-motivation)
2. [Core design decisions](#2-core-design-decisions)
3. [Phase 0 — Critical safety fixes](#3-phase-0--critical-safety-fixes)
4. [Phase 1 — Spec reconciliation](#4-phase-1--spec-reconciliation)
5. [Phase 2 — Socket architecture rewrite](#5-phase-2--socket-architecture-rewrite)
6. [Phase 3 — File and stdio as linear types](#6-phase-3--file-and-stdio-as-linear-types)
7. [Phase 4 — Platform IO driver integration](#7-phase-4--platform-io-driver-integration)
8. [Phase 5 — Advanced IO primitives](#8-phase-5--advanced-io-primitives)
9. [Design tradeoffs](#9-design-tradeoffs)
10. [Open questions](#10-open-questions)
11. [Appendix: Canonical API surface](#11-appendix-canonical-api-surface)
12. [Review](#12-review)

---

## 1. Context and motivation

### What exists

Three documents and two C files describe the Typhoon IO system:

| Artifact | Role | Status |
|---|---|---|
| `spec.md §14–16` | Canonical language spec | Authoritative but incomplete on IO |
| `io.md` | IO API design doc | Conflicts with spec; to be retired |
| `ty_io.c` | Stdio + format IO runtime | Blocking; three safety bugs |
| `ty_net.c` | Network runtime | Byte-level channel model; one safety bug |
| `scheduler.h` | Coroutine scheduler | Correct; IO must depend on it, not vice versa |

### Root problems

Two failures compound each other.

**Architectural:** `io.md` introduced `PendingOp<T>` and `.await()` — a futures model — which directly conflicts with the spec's `conc`/`chan<T>` concurrency model. Nothing in the runtime implements either correctly. `ty_net.c` bridges the gap with a byte-by-byte channel bridge (`socket_consumer_coro`) that is the worst of both worlds: it has the overhead of a channel per byte and the blocking semantics of a syscall.

**Safety:** Four memory bugs exist in the current C code and are live in production paths. They must be fixed before any new work begins.

### Guiding principle

IO is a specialised source of completion events. The scheduler owns IO — IO does not own the scheduler. A coroutine submits an IO operation, yields to the scheduler, and resumes when the scheduler has polled for completions and found the coroutine's op done. The IO driver is a dumb completion source; the scheduler is the decision-maker. This single inversion eliminates the need for a dedicated IO thread and makes `select` over channels and IO operations naturally unified.

---

## 2. Core design decisions

These decisions are settled before implementation begins. Phases 1–5 implement them; they are not re-litigated inside those phases.

### D1. No `PendingOp`, no `.await()`

The spec has one concurrency model: `conc {}` and `chan<T>`. IO is integrated into that model, not layered on top of it. IO functions look synchronous from user code. The suspension is an implementation detail of the runtime, invisible at the language level.

### D2. Three-tuple return type for consuming IO operations

Any IO function that consumes a linear resource returns it:

```typhoon
fn read(self: File, buf: Buf)   -> (File, Buf, Result<Int32, IoError>)
fn write(self: File, buf: Buf)  -> (File, Buf, Result<Int32, IoError>)
fn read(self: Socket, buf: Buf) -> (Socket, Buf, Result<Int32, IoError>)
```

`self` is returned so the liveness checker can track the resource after the call. `Buf` is returned so the caller reclaims the buffer handed to the driver. `Result` carries the byte count or error. Non-consuming operations (e.g. `listener.accept()`) return `Result<Socket, IoError>` directly.

### D3. IO depends on the scheduler; the scheduler does not depend on IO

The scheduler's idle path (when no coroutines are runnable) calls `backend->poll()` on the IO driver. The IO driver returns a list of completed operation handles. The scheduler maps handles to waiting coroutines and re-enqueues them. The IO driver has no knowledge of coroutines, run queues, or task slabs.

### D4. Backpressure is a channel capacity, not a watermark

Socket reads are exposed as a `chan<Buf>`. The channel's bounded ring buffer is the backpressure mechanism. A `chan<Buf>(8)` with 4 KB chunks caps buffering at 32 KB per socket. When the channel is full, the reader coroutine stops posting `recv()` ops. The OS socket buffer fills. TCP propagates backpressure to the remote peer. No watermark counters, no high/low thresholds, no additional mechanism.

### D5. No `malloc`/`free` in the IO hot path

All IO-path allocation goes through `ty_arena_alloc(task, ...)`. The task slab is the only allocator. After Phase 3 this is enforced by CI: `malloc` calls in `ty_io.c` and `ty_net.c` are a build failure.

---

## 3. Phase 0 — Critical safety fixes

**Serial prerequisite. No other work ships until all four tasks pass CI.**

These are undefined-behavior bugs in live code paths. They are not design questions. Fix them as standalone patches with regression tests written before the fix.

---

### Task 0.1 — Fix dangling pointer in `ty_vsscanf` `%s` handler

**Bug:** The `%s` case allocates `tok[1024]` on the C stack, then writes a pointer to it into the caller's variable. The stack frame is destroyed on return. Any subsequent access is undefined behavior.

```c
// Before — dangling pointer
char tok[1024];
int n = sc_read_token(&sc, tok, sizeof(tok));
if (!suppress) {
    char** dst = va_arg(ap, char**);
    *dst = tok;   // tok is dead after this function returns
    matched++;
}
```

```c
// After — slab-allocated, lives for coroutine lifetime
int n = sc_read_token_len(&sc, &tok_start, &tok_len);
if (!suppress) {
    char** dst = va_arg(ap, char**);
    char* result = (char*)ty_arena_alloc(task, tok_len + 1);
    memcpy(result, tok_start, tok_len);
    result[tok_len] = '\0';
    *dst = result;
    matched++;
}
```

**Requires:** threading `task` through `ty_vsscanf` — remove `(void)task` suppression.

#### Checklist

- [x] Write a test that calls `ty_sscanf` with `%s` and dereferences the result pointer after the call returns — confirm ASAN catches the bug before the fix
- [x] Implement `sc_read_token_len` helper that returns pointer + length into source without copying
- [x] Thread `task` through `ty_vfscanf` and `ty_vsscanf`
- [x] Replace stack `tok[1024]` with slab allocation in `%s` handler
- [x] Confirm ASAN test passes after fix
- [x] Add to CI gate: `ty_vsscanf` ASAN test must pass

---

### Task 0.2 — Fix `ty_sscan` mutating an immutable `Str`

**Bug:** `ty_sscan` writes `'\0'` into the source string to null-terminate a token. `Str` is defined as immutable in spec §3. When the source is a string literal it lives in `.rodata`; this write is undefined behavior (SIGBUS on some platforms, silent corruption on others).

```c
// Before — writes into source
if (*src) {
    *src = '\0';   // UB when src points into .rodata
    src++;
}
```

```c
// After — copies token into slab, never touches source
char* ty_sscan(void* task, const char* src, const char** rest_out) {
    while (*src == ' ' || *src == '\t' || *src == '\n' || *src == '\r') src++;
    if (!*src) { if (rest_out) *rest_out = src; return NULL; }
    const char* start = src;
    while (*src && *src != ' ' && *src != '\t' && *src != '\n' && *src != '\r') src++;
    size_t len = (size_t)(src - start);
    char* tok = (char*)ty_arena_alloc(task, len + 1);
    memcpy(tok, start, len);
    tok[len] = '\0';
    if (rest_out) *rest_out = src;
    return tok;
}
```

#### Checklist

- [ ] Write a test that calls `ty_sscan` on a string literal — confirm ASAN/MSAN catches the bug
- [ ] Change `src` parameter to `const char*`; change `rest_out` to `const char**`
- [ ] Implement slab-copy approach above
- [ ] Update all call sites that relied on in-place tokenization
- [ ] Confirm ASAN test passes after fix
- [ ] Update LLVM IR declaration: `@ty_sscan` first arg becomes `i8*` (ptr to const in C, same IR type)

---

### Task 0.3 — Fix `Socket__close` double-free

**Bug:** `Socket__close` removes `self` from `g_sockets` and then calls `free(self)`. A second call skips the list removal (element not found) but still calls `ty_sock_close(self->sock)` on a closed file descriptor and `free(self)` on already-freed memory. Both are undefined behavior.

```c
// After — add closed flag; assert in debug, no-op in release
struct TySocket {
    ty_sock_t sock;
    int       closed;      // added
    struct TySocket* next;
};

void __ty_rt__Socket__close(void* task, TySocket* self) {
    (void)task;
    if (!self) return;
    TY_ASSERT(!self->closed, "Socket__close called twice — liveness checker bug");
    self->closed = 1;
    // ... list removal unchanged ...
    ty_sock_close(self->sock);
    free(self);
}
```

Note: the real fix is Phase 2 (the liveness checker prevents double-close at the language level). This guard is the defensive runtime check until then.

#### Checklist

- [ ] Add `closed` field to `TySocket`
- [ ] Add `TY_ASSERT(!self->closed, ...)` at top of `Socket__close`
- [ ] Write a test that calls `Socket__close` twice on the same socket — confirm the assert fires in debug
- [ ] Verify the assert is compiled out in release builds (`NDEBUG`)

---

### Task 0.4 — Surface `StackBuf` overflow as a hard error

**Bug:** `StackBuf.overflow` is set when format output exceeds 4,096 bytes but is never read by any caller. Output is silently truncated.

**Immediate fix:** Return a negative value from all `ty_printf`/`ty_fprintf` family functions when `b.overflow` is set. The caller can check the return value; the language-level `?` operator can propagate it as `IoError::OutputTruncated`.

**Permanent fix:** Phase 3 replaces `StackBuf` with a slab-growing `TyBuf`. This task is only the immediate error-surface fix.

```c
// In ty_vfprintf, after sbuf_flush:
if (b.overflow) return -1;   // signal truncation to caller
return (int)written;
```

#### Checklist

- [ ] Add overflow check and negative return to `ty_vfprintf`, `ty_vfprint`, `ty_vfprintln`
- [ ] Write a test that prints a string longer than 4,096 bytes — confirm the return value is negative
- [ ] Document in a comment: "StackBuf is a temporary measure; replaced by slab TyBuf in Phase 3"

---

### Phase 0 definition of done

All four tasks have:
- A test written before the fix that demonstrates the bug
- A fix that makes the test pass
- CI enforces all four tests on every commit

No new code is merged to the IO subsystem until this gate is green.

---

## 4. Phase 1 — Spec reconciliation

**Serial prerequisite. Resolves the design contradiction before any implementation work begins.**

This is documentation and type-system design work. Every implementation decision in Phases 2–5 flows from what is specified here. Ambiguity in this phase becomes bugs in later phases.

---

### Task 1.1 — Retire `io.md` as a canonical document

`io.md` introduces `PendingOp<T>`, `.await()`, `join!`, `batch()`, `Mode.Read`, and `AsyncRead`/`AsyncWrite` traits — none of which exist in `spec.md`. It is an earlier design artifact that was never reconciled against the final language spec.

Action: move `io.md` to `docs/archive/io_design_draft.md` with a header note explaining it predates the final spec. Add a link from `spec.md §14` to the archive for historical context. The spec is now the single source of truth.

#### Checklist

- [ ] Incorporate Section 12 review findings into Phase 1/2 sequencing and dependency notes
- [ ] Move `io.md` to `docs/archive/io_design_draft.md`
- [ ] Add deprecation header to archived file
- [ ] Add `## IO` subsection to `spec.md §14` (Standard Library) cross-referencing §16
- [ ] Search codebase for any `#include` or reference to `io.md` and update them

---

### Task 1.2 — Define `IoError` as a first-class enum in the spec

Raw `int32_t` error codes have no defined meaning at the language level. Every IO function currently returning an error `int` must return `IoError` instead.

Add to `spec.md §9` (Error Handling) and `spec.md §14` (Standard Library):

```typhoon
enum IoError {
    NotFound(Str),          // ENOENT / ERROR_FILE_NOT_FOUND
    PermissionDenied,       // EACCES / ERROR_ACCESS_DENIED
    ConnectionReset,        // ECONNRESET
    ConnectionRefused,      // ECONNREFUSED
    TimedOut,               // ETIMEDOUT
    BrokenPipe,             // EPIPE / ERROR_BROKEN_PIPE
    Eof,                    // recv() returned 0 / orderly close
    Cancelled,              // operation cancelled via CancelToken
    OutputTruncated,        // format output exceeded buffer (temporary)
    Os(Int32),              // escape hatch for unmapped platform codes
}
```

The C runtime maps `errno` / WSAGetLastError codes to `IoError` variants at the FFI boundary. No `IoError` value ever crosses the boundary as a raw integer.

#### Checklist

- [ ] Add `IoError` enum definition to `spec.md §9`
- [ ] Add `IoError` to `spec.md §14` Standard Library tier 1 (available without import)
- [ ] Add `ty_io_error.h` defining the C-level `TyIoError` tagged union matching the enum
- [ ] Implement `ty_errno_to_io_error(int errno_val) -> TyIoError` for POSIX
- [ ] Implement `ty_wsa_to_io_error(int wsa_code) -> TyIoError` for Windows
- [ ] Update all `ty_net.c` and `ty_io.c` return paths to use `TyIoError` instead of raw `int32_t`

---

### Task 1.3 — Define the canonical IO return type in the spec

Add to `spec.md §16`:

```typhoon
// Consuming IO operations return (Self, Buf, Result<Int32, IoError>)
// — Self: resource returned to caller; liveness checker re-enables use
// — Buf: buffer returned to caller; driver no longer holds it
// — Result<Int32, IoError>: byte count on success, error on failure

// Non-consuming operations return Result<T, IoError> directly
// — Listener.accept() does not consume the listener
```

Add canonical usage examples:

```typhoon
// File read loop
fn read_all(file: File) -> Result<Buf, IoError> {
    let mut out = Buf::new()
    let buf = Buf::with_capacity(4096)
    loop {
        let (file, buf, res) = file.read(buf)
        let n = res?
        if n == 0 { break }
        out.push(buf.as_str())
    }
    file.close()
    Ok(out)
}

// Socket echo
fn echo(socket: Socket) -> Result<(), IoError> {
    let buf = Buf::with_capacity(4096)
    loop {
        let (socket, buf, res) = socket.read(buf)
        let n = res?
        if n == 0 { break }
        let (socket, buf, res) = socket.write(buf)
        res?
    }
    socket.close()
    Ok(())
}
```

#### Checklist

- [ ] Add return type specification to `spec.md §16`
- [ ] Add `read_all` and `echo` canonical examples to `spec.md §16`
- [ ] Verify that `?` composes correctly with the tuple: `let n = res?` (unwraps `Result<Int32, IoError>`, not the outer tuple)
- [ ] Add a note clarifying that `?` applies to `res`, not to the 3-tuple itself

---

### Task 1.4 — Fix `Mode` enum syntax throughout

`io.md` uses `Mode.Read`, `Mode.Write` — dot-separated enum access. The spec uses `::` as the path separator throughout. This is inconsistent and trains readers on wrong syntax.

```typhoon
// Wrong (io.md style)
let file = fs.open(path, Mode.Read)?

// Correct (spec style)
let file = fs.open(path, Mode::Read)?
```

Add `Mode` enum to `spec.md §16`:

```typhoon
enum Mode {
    Read,
    Write,
    ReadWrite,
    Append,
    CreateWrite,     // create if not exists, truncate if exists
    CreateAppend,    // create if not exists, append if exists
}
```

#### Checklist

- [ ] Add `Mode` enum definition to `spec.md §16`
- [ ] Replace every `Mode.X` occurrence in all docs with `Mode::X`
- [ ] Grep for `.Read`, `.Write`, `.Append` in all `.md` and `.ty` files; fix all occurrences

---

### Task 1.5 — Specify `join` as a language construct in `spec.md §11`

`io.md` referenced `join!` (macro syntax) and `batch()` (method call). Neither form fits the language. `join` should be a statement-level construct parallel to `select`, since it has the same ownership semantics: all arms consume external bindings simultaneously.

Add to `spec.md §11` (Concurrency):

```typhoon
// join — submit multiple IO operations simultaneously, resume when all complete
// Each arm is an expression that returns a value
// All arms must consume the same set of external live bindings (same rule as select/match)

let (r1, r2) = join {
    file_a.read(buf_a),
    file_b.read(buf_b),
}
// r1: (File, Buf, Result<Int32, IoError>)
// r2: (File, Buf, Result<Int32, IoError>)
```

Runtime mapping:
- Linux: submit both SQEs in a single `io_uring_submit()` call; track a counter initialized to N; resume when counter reaches 0
- macOS: submit both `kevent` registrations together
- Windows: submit both overlapped operations; use a completion counter in the OVERLAPPED user data

`join` is not a general parallel-execution primitive — it is specifically for IO operations that can be batched at the kernel level. For general parallel coroutine work, `conc` + `chan` is the right tool.

Partial-failure semantics for Phase 1/2:
- `join` always returns all arm results; it does not implicitly cancel sibling arms.
- `join?` is explicitly deferred (not part of this phase set).
- Cancellation across joined arms is caller-managed via `CancelToken` in Phase 5.

#### Checklist

- [x] Add `join` syntax and semantics to `spec.md §11`
- [ ] Add liveness rules for `join` arms (mirror `select` rules in spec §18 Open Questions)
- [ ] Add `join` to the keyword list in `spec.md §2`
- [ ] Add at least two `join` usage examples: file + file, socket read + timer
- [x] Lock `join` partial-failure behavior: no implicit cancellation; no `join?` in Phase 1/2
- [x] Add `select` + IO readiness semantics note (register interest first, submit winner op on readiness)

---

### Task 1.6 — Add `File`, `Socket`, `Listener` to `spec.md §14` with full API surface

These types are mentioned in §16 but their complete method signatures are never listed in one place.

```typhoon
// std::fs — available via `use std::fs`

struct File  // linear resource

fn fs::open(path: Str, mode: Mode) -> Result<File, IoError>

impl File {
    fn read(self, buf: Buf)          -> (File, Buf, Result<Int32, IoError>)
    fn write(self, buf: Buf)         -> (File, Buf, Result<Int32, IoError>)
    fn read_vectored(self, bufs: [Buf])  -> (File, [Buf], Result<Int32, IoError>)
    fn write_vectored(self, bufs: [Buf]) -> (File, [Buf], Result<Int32, IoError>)
    fn seek(self, pos: SeekPos)      -> (File, Result<Int64, IoError>)
    fn close(self)                   -> ()   // consumes; no return
}

enum SeekPos {
    Start(Int64),
    End(Int64),
    Current(Int64),
}

// std::net — available via `use std::net`

struct Socket    // linear resource
struct ReadSocket   // linear resource
struct WriteSocket  // linear resource
struct Listener  // linear resource

impl Network {
    fn listen(self, addr: Str) -> Result<Listener, IoError>
    fn connect(self, addr: Str) -> Result<Socket, IoError>
}

impl Listener {
    fn accept(self) -> (Listener, Result<Socket, IoError>)
    fn close(self)  -> ()
}

impl Socket {
    fn read(self, buf: Buf)          -> (Socket, Buf, Result<Int32, IoError>)
    fn write(self, buf: Buf)         -> (Socket, Buf, Result<Int32, IoError>)
    fn read_vectored(self, bufs: [Buf])  -> (Socket, [Buf], Result<Int32, IoError>)
    fn write_vectored(self, bufs: [Buf]) -> (Socket, [Buf], Result<Int32, IoError>)
    fn split(self) -> (ReadSocket, WriteSocket)
    fn close(self)                   -> ()
}

impl ReadSocket {
    fn read(self, buf: Buf)          -> (ReadSocket, Buf, Result<Int32, IoError>)
    fn read_vectored(self, bufs: [Buf]) -> (ReadSocket, [Buf], Result<Int32, IoError>)
    fn into_chan(self, chunk: Int32, cap: Int32) -> chan<Buf>
    fn close(self)                   -> ()
}

impl WriteSocket {
    fn write(self, buf: Buf)         -> (WriteSocket, Buf, Result<Int32, IoError>)
    fn write_vectored(self, bufs: [Buf]) -> (WriteSocket, [Buf], Result<Int32, IoError>)
    fn close(self)                   -> ()
}

// std::io — stdin/stdout/stderr (non-linear singletons)

fn stdin::read_line() -> Result<Buf, IoError>
fn stdout::write(s: Str) -> Result<(), IoError>
fn stderr::write(s: Str) -> Result<(), IoError>
fn println(s: Str)       -> ()    // panics on error — for convenience only
fn eprintln(s: Str)      -> ()
```

Note on `Listener.accept`: the listener is returned in the tuple so the caller can continue accepting connections in a loop. Consuming the listener would make a server loop impossible without rebinding.

#### Checklist

- [ ] Add `File` API block to `spec.md §14`
- [ ] Add `Socket`, `Listener`, `Network` API block to `spec.md §14`
- [ ] Add `ReadSocket` and `WriteSocket` API blocks to `spec.md §14`
- [ ] Add `stdin`/`stdout`/`stderr` API block to `spec.md §14`
- [ ] Add `SeekPos` enum to `spec.md §14`
- [ ] Verify every method's liveness behavior is explicitly noted (which params are consumed, which are returned)
- [ ] Write the complete server example using the new API in `spec.md §16`
- [x] Replace all `(Socket, chan<Buf>)` `into_chan` examples with split-half examples

---

### Phase 1 definition of done

A Typhoon programmer can read `spec.md` from top to bottom and implement a correct TCP echo server using only what the spec says — no guessing, no `io.md`, no reading the C headers. All examples compile (or will compile once the runtime is implemented).

---

## 5. Phase 2 — Socket architecture rewrite

**Parallel with Phase 3. Requires Phase 0 + 1.**

This is the largest single performance impact. The byte-by-byte channel model is deleted and replaced with chunk-based channel backpressure and direct slab reads.

Current hazard before this phase lands: both `socket_consumer_coro` (`recv`) and `Listener__accept` are blocking syscall paths on worker threads. If blocked IO calls exceed `num_workers`, runnable coroutines can be starved.

---

### Task 2.1 — Delete `socket_consumer_coro` and `Socket__consume`

`socket_consumer_coro` is the function that sends one byte per channel operation. It is architecturally wrong. It is deleted, not refactored.

Files to change:
- `ty_net.c`: remove `socket_consumer_coro`, `__ty_rt__Socket__consume`
- Remove the `void** pair` malloc (the last explicit `malloc` in the network path)
- Update any call sites that used `Socket__consume`

This is a breaking change at the `__ty_rt__` boundary. It is intentional.

#### Checklist

- [ ] Identify all call sites of `__ty_rt__Socket__consume` in generated IR / test code
- [x] Delete `socket_consumer_coro` function body
- [x] Delete `__ty_rt__Socket__consume` entry point
- [x] Delete `void** pair` malloc and the `socket_consumer_coro` spawn
- [x] Confirm no remaining references in `ty_net.c` or `ty_net.h`
- [ ] Update IR declaration list (bottom of `ty_io.c`) — remove `Socket__consume`

---

### Task 2.2 — Add transitional IO submit/poll adapter for Phase 2

Phase 2 must not depend on full Phase 4 backend inversion. Add a minimal adapter now that can route through the current driver API and later swap to the final `TyIoBackend` shape in Phase 4.

#### Checklist

- [ ] Define a minimal Phase-2 IO op contract (`submit`, `await`, `complete`) in `ty_net.c`/driver boundary
- [ ] Implement adapter over current `io_driver.c` API without introducing blocking worker syscalls
- [ ] Ensure adapter carries coroutine wake context needed by scheduler
- [ ] Mark this adapter as transitional and Phase-4-replaceable

---

### Task 2.3 — Implement `Socket__read` and `Socket__write` with transparent suspend

The new `__ty_rt__Socket__read` posts a `recv()` operation to the platform IO backend, then suspends the coroutine via the scheduler. The coroutine resumes when the scheduler's poll cycle finds the completion and re-enqueues the coroutine.

```c
TyResult_Socket_Buf_i32
__ty_rt__Socket__read(void* task, TySocket* self, TyBuf* buf, int32_t cap) {
    void* coro = ty_current_coro_raw();
    TyIoBackend* backend = ty_sched_io_backend();

    if (backend && coro) {
        // async path: register op, suspend, resume with result
        TyIoOp op = {
            .type     = TY_IO_RECV,
            .fd       = self->sock,
            .buf      = ty_buf_ptr(buf),
            .len      = cap,
            .coro     = coro,
        };
        backend->submit(backend, &op);
        ty_coro_suspend(coro);               // yields to scheduler
        int32_t n = ty_coro_io_result(coro); // written by scheduler on resume
        // ...build and return result tuple...
    }

    // sync fallback (main thread / before scheduler start)
    int n = recv(self->sock, ty_buf_ptr(buf), cap, 0);
    // ...build and return result tuple...
}
```

The `TyBuf` passed in must be slab-allocated. The driver writes into it directly. No intermediate copy.

#### Checklist

- [ ] Define `TyIoOp` struct in the transitional adapter (move to `ty_io_backend.h` in Phase 4)
- [ ] Implement `ty_coro_suspend(coro)` in `scheduler.h` — records that coro is waiting for IO and yields
- [ ] Implement `ty_coro_io_result(coro)` — retrieves the result stored by the scheduler after wake
- [ ] Implement `__ty_rt__Socket__read` with async path and sync fallback
- [ ] Implement `__ty_rt__Socket__write` with the same pattern
- [ ] Write an integration test: two coroutines doing loopback read/write; confirm neither blocks the OS thread

---

### Task 2.4 — Implement non-blocking `Listener__accept` and `Listener__close`

`Listener__accept` must follow the same async submit/suspend/resume path as socket read/write. `Listener__close` must exist as a first-class runtime path (not deferred to shutdown-only cleanup).

#### Checklist

- [ ] Implement `__ty_rt__Listener__accept` with async path and sync fallback
- [x] Implement `__ty_rt__Listener__close` and remove shutdown-only ownership assumption
- [x] Add regression test: accept loop does not block worker; close before shutdown does not leak

---

### Task 2.5 — Implement split-half socket API and channel reader

This is the primary user-facing API for reading from a socket. It replaces the deleted `Socket__consume`.

```typhoon
// Split first, then spawn reader coroutine from read half.
// Backpressure: channel full → reader coroutine blocks → no new recv() posted
//               → OS socket buffer fills → TCP flow control engages

impl Socket {
    fn split(self) -> (ReadSocket, WriteSocket)
}

impl ReadSocket {
    fn into_chan(self, chunk_size: Int32, cap: Int32) -> chan<Buf>
}
```

The spawned reader coroutine:
```typhoon
// Internal — not user-visible
fn socket_reader(socket: ReadSocket, ch: chan<Buf>, chunk_size: Int32) {
    loop {
        let buf = Buf::with_capacity(chunk_size)
        let (socket, buf, res) = socket.read(buf)
        match res {
            Ok(0)  => break,
            Ok(_)  => ch.send(buf),   // blocks here when channel full — backpressure
            Err(_) => break,
        }
    }
}
```

At the C level, `ReadSocket.into_chan` calls `ty_spawn(task, socket_reader_coro, args)`. The channel's bounded ring buffer is the only backpressure mechanism — no watermarks, no counters.

#### Checklist

- [ ] Implement runtime split: `__ty_rt__Socket__split(task, self) -> (ReadSocket, WriteSocket)`
- [ ] Implement `socket_reader_coro` at C level using `__ty_rt__ReadSocket__read`
- [ ] Implement `__ty_rt__ReadSocket__into_chan(task, self, chunk_size, cap)` that spawns `socket_reader_coro`
- [ ] Confirm channel with `cap=8` and `chunk_size=4096` caps buffering at 32 KB
- [ ] Write a test: slow consumer, fast sender; confirm no OOM; confirm sender slows down (TCP backpressure)
- [ ] Write a test: `into_chan` produces the same bytes as direct `read()` in the same order

---

### Task 2.6 — Fix shutdown/close race and fd invalidation discipline

`Socket__close` and `ty_net_shutdown` must not double-close the same fd under races.

#### Checklist

- [ ] Under socket/listener registry lock, mark closed and move fd to invalid sentinel before unlock
- [ ] Close fd only after ownership transfer is unambiguous
- [ ] Add race stress test: concurrent close + shutdown across many sockets/listeners

---

### Phase 2 definition of done

Benchmark: send 1 MB over loopback, read via `into_chan` with 4 KB chunks. Confirm:
- Exactly 256 `recv()` syscalls (one per chunk, not one per byte)
- Peak memory usage bounded by `cap × chunk_size` plus socket send buffer
- No `malloc` calls in the hot path (ASAN malloc hook confirms)

---

## 6. Phase 3 — File and stdio as linear types

**Parallel with Phase 2. Requires Phase 0 + 1.**

---

### Task 3.1 — Define `TyFile` and implement `File__open`

```c
struct TyFile {
    int fd;
#ifndef NDEBUG
    int closed;   // debug guard, same pattern as Task 0.3
#endif
};
```

No global registry. No mutex. The liveness checker guarantees `close` is called exactly once. The runtime trusts the compiler's guarantee.

```c
void __ty_rt__fs__open(void* task, const char* path, TyMode mode,
                       TyResult_File_IoError* outp) {
    int flags = ty_mode_to_flags(mode);   // Mode::Read → O_RDONLY, etc.
    int fd = open(path, flags, 0644);     // POSIX; CreateFile on Windows
    if (fd < 0) {
        outp->ok  = 0;
        outp->err = ty_errno_to_io_error(errno);
        return;
    }
    TyFile* f = (TyFile*)ty_arena_alloc(task, sizeof(TyFile));
    f->fd     = fd;
    outp->ok  = 1;
    outp->value = f;
}
```

#### Checklist

- [ ] Define `TyFile` struct in `ty_io.h`
- [ ] Implement `ty_mode_to_flags(TyMode) -> int` for POSIX and Windows
- [ ] Implement `__ty_rt__fs__open`
- [ ] Implement `__ty_rt__File__close` (no return; consumes self)
- [ ] Write test: open nonexistent file → `IoError::NotFound`; open existing → success; read → expected bytes; close → no ASAN error; second close → debug assert fires

---

### Task 3.2 — Implement `File__read` and `File__write` with transparent suspend

Mirrors Task 2.2. On POSIX, `File__read` submits `IORING_OP_READ` (io_uring) or registers `EVFILT_READ` (kqueue). On Windows, submits an overlapped `ReadFile`.

The `TyBuf` is slab-allocated. The driver writes into it directly — no intermediate kernel buffer copy at the application layer.

#### Checklist

- [ ] Implement `__ty_rt__File__read` with async path (suspend) and sync fallback (blocking `read()`)
- [ ] Implement `__ty_rt__File__write` with async path and sync fallback
- [ ] Implement `__ty_rt__File__seek` returning `(TyFile, TyResult_i64_IoError)`
- [ ] Confirm `TyFile` is returned in every result path — no path drops ownership
- [ ] Write test: read file in 4 KB chunks; compare against expected content byte-for-byte

---

### Task 3.3 — Replace `StackBuf` with slab-growing `TyBuf`

`StackBuf` is a 4 KB `char data[4096]` on the C stack. It truncates silently (partially fixed in Task 0.4). The replacement:

```c
typedef struct {
    char*  data;
    size_t len;
    size_t cap;
    void*  task;   // arena for realloc
} SlabBuf;

static void slabbuf_init(SlabBuf* b, void* task, size_t initial_cap) {
    b->data = (char*)ty_arena_alloc(task, initial_cap);
    b->len  = 0;
    b->cap  = initial_cap;
    b->task = task;
}

static void slabbuf_push(SlabBuf* b, const char* s, size_t n) {
    if (b->len + n > b->cap) {
        size_t new_cap = b->cap * 2;
        while (new_cap < b->len + n) new_cap *= 2;
        b->data = (char*)ty_arena_realloc(b->task, b->data, b->cap, new_cap);
        b->cap  = new_cap;
    }
    memcpy(b->data + b->len, s, n);
    b->len += n;
}
```

Initial capacity: 256 bytes (covers the vast majority of format strings). Grows by doubling. The only limit is the task slab size (4 MB default). No overflow flag. No truncation. Output is always complete.

#### Checklist

- [x] Define `SlabBuf` struct in `ty_io.c`
- [ ] Implement `slabbuf_init`, `slabbuf_push`, `slabbuf_push_char`, `slabbuf_push_str`
- [ ] Replace all `StackBuf`/`sbuf_*` call sites with `SlabBuf`/`slabbuf_*`
- [ ] Delete `StackBuf` typedef and all `sbuf_*` functions
- [x] Delete `STACK_BUF_CAP` constant
- [x] Write test: print a 10,000-byte string via `ty_printf`; confirm full output received
- [ ] Confirm no `malloc` in `ty_printf` path — all via `ty_arena_alloc`

---

### Task 3.4 — Rewrite `ty_sscan` and `ty_vsscanf` properly

Task 0.2 provides a hotfix for `ty_sscan`. This task delivers the correct implementation using the slab allocator throughout, and fixes `ty_vsscanf`'s `%s` handler with the same approach (complementing the ASAN fix from Task 0.1).

Key principle: `Str` is never written to. Tokens are always copied into slab memory. The source string is never mutated.

#### Checklist

- [ ] Replace `ty_sscan` hotfix from Task 0.2 with final slab-based implementation
- [ ] Replace `ty_vsscanf` `%s` hotfix from Task 0.1 with final implementation using cursor-based token length detection
- [ ] Confirm source string is declared `const char*` through the entire call chain
- [ ] Write fuzz test: random format strings and inputs; confirm no ASAN/MSAN violations

---

### Phase 3 definition of done

- A Typhoon program can open a file, read it in chunks, and close it — with the liveness checker catching any use-after-close at compile time
- `ty_printf` with a 10,000-character output produces complete output
- `ty_sscan` on a string literal passes ASAN and MSAN
- No `malloc` call in `ty_io.c` or `ty_net.c` — CI enforces this

---

## 7. Phase 4 — Platform IO driver integration

**Requires Phase 2 and Phase 3. Makes transparent suspension real.**

Phases 2 and 3 define the API shape and the `ty_coro_suspend()` call sites. Phase 4 makes the driver's completion loop actually wake coroutines.

---

### Task 4.1 — Define the `TyIoBackend` abstraction layer

```c
// ty_io_backend.h

typedef struct TyIoBackend TyIoBackend;

typedef struct TyIoOp {
    int       type;    // TY_IO_RECV, TY_IO_SEND, TY_IO_READ, TY_IO_WRITE, TY_IO_ACCEPT
    int       fd;
    uint8_t*  buf;
    size_t    len;
    void*     coro;    // TyCoro* — scheduler uses this on completion
    int32_t   result;  // written by driver on completion; read by ty_coro_io_result()
} TyIoOp;

typedef void (*TySchedWakeFn)(void* coro, int32_t result);

struct TyIoBackend {
    void (*submit) (TyIoBackend*, TyIoOp*);
    void (*poll)   (TyIoBackend*, TySchedWakeFn wake);
    void (*shutdown)(TyIoBackend*);
    void* impl;    // platform-specific state (io_uring ring, kq fd, IOCP handle)
};

// Called by the scheduler during its idle cycle
void ty_io_poll(TySchedWakeFn wake);

// Called by IO facade functions (Task 2.2, 3.2)
void ty_io_submit(TyIoOp* op);
```

The backend is selected at compile time:

```c
#if defined(__linux__)
#  include "ty_io_uring.h"
#elif defined(__APPLE__)
#  include "ty_io_kqueue.h"
#elif defined(_WIN32)
#  include "ty_io_iocp.h"
#endif
```

No runtime vtable dispatch. The function pointer table is initialized once at startup and treated as constant thereafter.

#### Checklist

- [x] Create `ty_io_backend.h` with `TyIoBackend`, `TyIoOp`, `TySchedWakeFn`
- [x] Define `TY_IO_*` operation type constants
- [x] Implement `ty_io_poll()` and `ty_io_submit()` as thin wrappers over the backend
- [x] Hook `ty_io_poll()` into the scheduler's idle path (when run queue is empty)
- [ ] Write a mock backend for testing that records submitted ops and lets tests manually trigger completions

---

### Task 4.2 — Linux: io_uring backend

The key design: each scheduler worker thread has its own `io_uring` ring. The worker thread is the submitter and the poller. No dedicated IO thread. No cross-thread SQE queues.

```
Worker thread N:
  loop:
    while (coroutine = dequeue_ready()):
      run(coroutine) until yield
    // idle: check for completions
    io_uring_peek_cqe(ring, &cqe)
    if cqe:
      coro = cqe->user_data
      coro->io_result = cqe->res
      enqueue_ready(coro)
      io_uring_cqe_seen(ring, cqe)
```

`IORING_SETUP_SQPOLL` enables kernel-side SQ polling — once the ring is warmed up, `io_uring_submit()` requires no syscall. Submissions happen inline in `ty_io_submit()` without a ring-crossing.

For `join` (Task 5.3): submit N SQEs before calling `io_uring_submit()` once. The kernel processes all N as a batch.

#### Checklist

- [ ] Create `ty_io_uring.c` / `ty_io_uring.h`
- [ ] Initialize one `io_uring` ring per scheduler worker thread at startup
- [ ] Implement `submit`: fills SQE, sets `user_data = op->coro`, calls `io_uring_submit()` (or batches)
- [ ] Implement `poll`: calls `io_uring_peek_cqe` in a loop; for each CQE calls `wake(cqe->user_data, cqe->res)`
- [ ] Hook `poll` into scheduler idle path (Task 4.1)
- [ ] Test: 1,000 concurrent coroutines doing loopback read/write; all complete; no deadlock
- [ ] Test: `strace` confirms `io_uring_enter` is not called per-op when `SQPOLL` is active

---

### Task 4.3 — macOS: kqueue backend

kqueue is readiness-based (not completion-based), so the backend performs the actual `read()`/`write()` syscall after the event fires and stores the result before waking the coroutine.

```
poll():
  kevent(kq, NULL, 0, events, MAX_EVENTS, &timeout_zero)
  for each event:
    op = event.udata
    n  = read(op->fd, op->buf, op->len)   // syscall happens here, not in submit
    op->result = n
    wake(op->coro, n)
```

The abstraction still holds: from the coroutine's perspective, it suspends before the data arrives and resumes with the result in `io_result`. The fact that the driver does the syscall (rather than the kernel, as in io_uring) is invisible.

#### Checklist

- [ ] Create `ty_io_kqueue.c` / `ty_io_kqueue.h`
- [ ] Initialize one `kqueue` fd per scheduler worker thread
- [ ] Implement `submit`: calls `kevent()` to register `EVFILT_READ` / `EVFILT_WRITE` with `udata = op`
- [ ] Implement `poll`: calls `kevent()` with zero timeout; for each event, performs syscall, calls `wake`
- [ ] Test: same 1,000-coroutine loopback test as Task 4.2
- [ ] Test: `accept()` and socket reads both work through the backend

---

### Task 4.4 — Windows: IOCP backend

IOCP is completion-based like io_uring. `GetQueuedCompletionStatusEx` drains multiple completions per call — natural batch drain.

The `OVERLAPPED` struct carries the `TyIoOp*` pointer in `hEvent` (unused by Windows when IOCP is in use).

```c
poll():
  OVERLAPPED_ENTRY entries[MAX_DRAIN];
  ULONG n;
  GetQueuedCompletionStatusEx(iocp, entries, MAX_DRAIN, &n, 0, FALSE);
  for (i = 0; i < n; i++):
    op = (TyIoOp*)entries[i].lpOverlapped->hEvent;
    op->result = (int32_t)entries[i].dwNumberOfBytesTransferred;
    wake(op->coro, op->result);
```

#### Checklist

- [ ] Create `ty_io_iocp.c` / `ty_io_iocp.h`
- [ ] Initialize one IOCP handle per scheduler worker thread
- [ ] Implement `submit`: calls `ReadFile`/`WriteFile`/`WSARecv`/`WSASend` with overlapped struct
- [ ] Implement `poll`: calls `GetQueuedCompletionStatusEx` with zero timeout
- [ ] Test: same loopback test on Windows
- [ ] Test: file read and socket read both route through IOCP

---

### Task 4.5 — Replace `g_sockets` global registry

The global `g_sockets` linked list and `g_sock_lock` mutex exist solely for `ty_net_shutdown()`. They create a single serialization point for all socket lifecycle operations.

With the scheduler-owned model:
- Each scheduler worker thread maintains a small `TyFdSet` of open FDs it manages
- On shutdown, each worker thread closes its own FDs without contending with other threads
- `Socket__close` becomes: `close(self->fd); ty_arena_free(task, self)` — no list, no lock
- `ty_net_shutdown()` signals each worker thread to close its FD set and exit

```c
// Before — global serialization
ty_mutex_lock(&g_sock_lock);
struct TySocket* prev = NULL;
struct TySocket* curr = g_sockets;
while (curr) { ... }
ty_mutex_unlock(&g_sock_lock);
ty_sock_close(self->sock);
free(self);

// After — no lock, no list
void __ty_rt__Socket__close(void* task, TySocket* self) {
    TY_ASSERT(!self->closed, "Socket__close called twice");
    self->closed = 1;
    ty_sock_close(self->sock);
    ty_arena_free(task, self);
}
```

#### Checklist

- [ ] Add `TyFdSet` per-thread tracking to the scheduler
- [ ] Remove `g_sockets`, `g_listeners`, `g_sock_lock` global variables
- [ ] Remove `ty_mutex_init(&g_sock_lock)` from `ty_net_init()`
- [ ] Rewrite `ty_net_shutdown()` to signal workers rather than iterating a global list
- [ ] Confirm no remaining references to `g_sock_lock` anywhere
- [ ] Test: 100 sockets opened and closed; ASAN confirms no leaked FDs or memory

---

### Phase 4 definition of done

- `strace` on Linux shows `io_uring_setup` called once per thread at startup; no `epoll_wait`, no `select`
- `ktrace` on macOS shows `kqueue` called once per thread; completions via `kevent`
- Windows: Event Viewer shows IOCP handle created per thread
- The 1,000-coroutine loopback test passes on all three platforms under ASAN + TSAN
- No global mutexes in the socket or file IO path

---

## 8. Phase 5 — Advanced IO primitives

**Requires Phase 4. These are features, not fixes.**

---

### Task 5.1 — Vectored IO (`read_vectored`, `write_vectored`)

Maps to:
- Linux: `IORING_OP_READV` / `IORING_OP_WRITEV` with `iovec[]`
- macOS: `readv()` / `writev()` after `EVFILT_READ` event
- Windows: `WSARecv` / `WSASend` with `WSABUF[]`

The Typhoon API passes a `[Buf]` array. At the C level this maps to a slab-allocated `iovec[]`:

```c
struct iovec* iov = (struct iovec*)ty_arena_alloc(task, sizeof(struct iovec) * n_bufs);
for (int i = 0; i < n_bufs; i++) {
    iov[i].iov_base = ty_buf_ptr(&bufs[i]);
    iov[i].iov_len  = ty_buf_cap(&bufs[i]);
}
// submit IORING_OP_READV with iov, n_bufs
```

This is the zero-copy scatter-gather path for HTTP zero-copy parsing: the parser can describe disjoint slab regions and the kernel fills them in one syscall.

#### Checklist

- [ ] Define `__ty_rt__File__read_vectored` and `__ty_rt__File__write_vectored`
- [ ] Define `__ty_rt__Socket__read_vectored` and `__ty_rt__Socket__write_vectored`
- [ ] Implement `iovec` allocation from slab in each function
- [ ] Wire to `IORING_OP_READV`, `readv()`, and `WSARecv` respectively
- [ ] Write test: read a file into three separate `Buf` regions; compare content against sequential read

---

### Task 5.2 — Timeout via `CancelToken`

Timeouts are implemented as a paired timer operation. When the IO op completes first, the timer is cancelled. When the timer fires first, the IO op is cancelled and `IoError::TimedOut` is returned.

```typhoon
// User API
let (socket, buf, res) = socket.read(buf).timeout(Duration::ms(5000))?
```

This desugars to a `join` of the read op and a timer op, with the semantics: whichever completes first wins; the other is cancelled.

At the C level:
- Linux: submit `IORING_OP_RECV` + `IORING_OP_TIMEOUT` linked with `IOSQE_IO_LINK`; when the linked timeout fires, io_uring automatically cancels the linked recv
- macOS: register `EVFILT_TIMER` alongside `EVFILT_READ`; whichever event fires first, cancel the other
- Windows: `CreateThreadpoolTimer` alongside the overlapped recv; cancel whichever loses

`CancelToken` is a handle that can cancel all IO ops associated with it. Useful for server shutdown: cancel all outstanding socket reads, which causes every `socket.read()` to return `IoError::Cancelled`, which propagates via `?`.

#### Checklist

- [ ] Define `TyCancelToken` struct and `ty_cancel_token_new(task)` / `ty_cancel_token_cancel(token)`
- [ ] Associate ops with tokens: add `cancel_token` field to `TyIoOp`
- [ ] Implement `.timeout(Duration)` by generating a linked timer op (platform-specific)
- [ ] Implement `CancelToken::cancel()` via `IORING_OP_ASYNC_CANCEL`, `EVFILT_TIMER` removal, or IOCP cancellation
- [ ] Write test: socket read with 100ms timeout on a socket that never sends data → `IoError::TimedOut` within 110ms
- [ ] Write test: cancel token cancels 10 outstanding reads → all return `IoError::Cancelled`

---

### Task 5.3 — `join` expression runtime support

`join` is the spec construct from Task 1.5. This task implements it in the runtime.

The compiler emits a counter and N SQE registrations. The coroutine suspends with `pending_count = N`. Each completion decrements the counter and stores the result. When the counter reaches 0, the coroutine is re-enqueued.

```c
typedef struct {
    int32_t  pending;        // atomic countdown
    int32_t  results[16];    // max 16 arms (expand if needed)
    void*    coro;
} TyJoinState;
```

On Linux: submit all N SQEs in a single `io_uring_submit()` call. This is one kernel crossing for N operations — the core performance benefit.

#### Checklist

- [ ] Define `TyJoinState` struct in `scheduler.h`
- [ ] Implement `ty_join_begin(task, n) -> TyJoinState*`
- [ ] Modify `ty_io_submit` to accept an optional `TyJoinState*`; use atomic decrement on completion
- [ ] Implement batch `io_uring_submit()` that submits all join SQEs in one call
- [ ] Implement the corresponding kqueue and IOCP batch paths
- [ ] Write test: two simultaneous file reads via `join`; confirm single `io_uring_submit` call (via mock backend)
- [ ] Write test: `join` with one fast and one slow op; both results correct when they complete

---

### Phase 5 definition of done

Benchmark gate (spec §17 Phase 6 success criterion):

| Metric | Target |
|---|---|
| Concurrent connections | 10,000 on loopback |
| Median latency | < 1ms |
| p99 latency | < 5ms |
| Throughput | Within 20% of Go `net/http` on same hardware |
| `io_uring_submit` calls | ≤ 1 per batch cycle, not 1 per op |
| `malloc` calls in hot path | 0 |

---

## 9. Design tradeoffs

### T1. IO depends on the scheduler (not the reverse)

**Decision:** The scheduler's idle cycle calls `backend->poll()`. The IO driver does not call back into the scheduler.

**Benefit:** Clean dependency direction. The IO driver is a dumb completion source; no dedicated IO thread is needed. Cache-local polling (the coroutine that submitted the op and the thread that polls for its completion are the same thread).

**Cost:** The scheduler must be modified to call `ty_io_poll()`. If the scheduler is not in an idle state (all coroutines are CPU-bound), IO completions are not polled until a coroutine yields. This is the correct behavior — a fully CPU-bound workload should not be interrupted by IO polling — but it means IO latency is bounded by the preemption interval (10ms by default, §15) in pathological cases.

**Alternative rejected:** Dedicated IO thread that calls `ty_sched_wake()` directly. This inverts the dependency, requires cross-thread wake calls on every completion, and adds cache misses. Rejected.

---

### T2. Backpressure via channel capacity, not watermarks

**Decision:** `read_socket.into_chan(chunk_size, cap)` — the channel's ring buffer is the backpressure mechanism. No watermark counters, no high/low thresholds.

**Benefit:** Reuses an existing primitive. Simple to reason about: "at most `cap × chunk_size` bytes buffered." The channel's existing block-on-full behavior propagates backpressure automatically through the coroutine scheduler, through TCP's receive window, to the remote peer.

**Cost:** Backpressure granularity is `chunk_size` (e.g. 4 KB), not 1 byte. A consumer that is 1 byte behind can still buffer up to `(cap × chunk_size) - 1` extra bytes before backpressure engages. For typical server applications this is not a problem.

**Alternative rejected:** Byte-granularity watermarks (high/low thresholds). Adds a counter and two comparisons on every read and consume. Solves a problem that essentially does not exist in practice — no real application needs backpressure accurate to the byte. Rejected.

---

### T3. `chan<Buf>` chunks over raw byte stream

**Decision:** The primary socket read API is `Socket.split() -> (ReadSocket, WriteSocket)` and `ReadSocket.into_chan(chunk_size, cap) -> chan<Buf>`, not a raw byte iterator.

**Benefit:** Chunk-based processing aligns with how protocols actually work (HTTP frames, TLS records, etc.). A 4 KB `Buf` is cache-line-sized and fits comfortably in L1/L2. The channel is a natural seam for backpressure (T2). The reader and writer coroutines are decoupled — the reader can be faster or slower than the writer without either blocking the other unnecessarily.

**Cost:** For protocols with variable-length framing (e.g., HTTP/1.1 headers that straddle chunk boundaries), the consumer must handle split frames. This is standard for any chunk-based network IO and is not a new burden — `recv()` has always returned partial data.

**Alternative considered:** Expose a raw `socket.read_byte() -> Result<Byte, IoError>` for simplicity. Rejected because it would reproduce the byte-by-byte channel problem at the language level (one channel operation per byte → O(n) scheduler operations per packet).

---

### T4. `join` as a language keyword, not a macro or method

**Decision:** `join { op_a, op_b }` is a statement-level construct added to `spec.md §2` keywords and `spec.md §11` concurrency.

**Benefit:** The compiler can enforce liveness rules across `join` arms (same rule as `match` and `select` — all arms must consume the same external live bindings). A macro or method cannot enforce this. The compiler can also emit optimal code: submit all SQEs before suspending once, rather than suspending N times.

**Cost:** Adds a new keyword, which means new parser work and potential conflicts with identifiers named `join` in existing (non-existent) user code. Since the language is pre-1.0 this cost is minimal.

**Alternative rejected:** `join!()` macro (as in `io.md`). Macros cannot be given liveness semantics. Rejected.

---

### T5. Per-thread io_uring rings over a shared ring

**Decision:** Each scheduler worker thread has its own `io_uring` ring. SQE submission is thread-local; no cross-thread coordination needed.

**Benefit:** Zero contention on the submission queue. `io_uring_submit()` is called from the same thread that submits the SQE — no lock needed, no memory barrier needed for the SQE itself.

**Cost:** More `io_uring` file descriptors (one per core). Each ring has its own kernel-side overhead (~1 MB per ring for the default SQ/CQ sizes). For 16-core machines this is 16 MB — acceptable.

**Cost:** IO ops stay pinned to the thread that submitted them. If a coroutine migrates to a different worker thread (via work stealing), its pending IO op is still registered on the original thread's ring. The completion wakes the original thread's scheduler, which then must either run the coroutine locally or forward it to the stealing thread. This is a real complexity cost. Mitigated by: coroutines that are blocked on IO are not on the run queue and therefore not steal candidates — they only enter the run queue when their IO completes, at which point they run on the thread whose ring fired the completion.

**Alternative rejected:** Single global `io_uring` ring with a lock. Creates a serialization bottleneck at the exact place where we need to be fast. Rejected.

---

### T6. `StackBuf` replaced entirely, not grown

**Decision:** `StackBuf` (4 KB, stack-allocated, truncates) is deleted and replaced with `SlabBuf` (slab-allocated, grows by doubling, never truncates).

**Benefit:** No silent truncation. No stack usage for format buffers. Consistent with D5 (all allocation via slab).

**Cost:** A heap allocation (via slab) for every `printf` call, even tiny ones. For `println("ok")`, this is a 256-byte slab allocation that would previously have been free (stack). On a hot logging path, this could matter.

**Mitigation:** The slab allocator is a bump pointer — "allocation" is incrementing a counter. The cost is effectively zero for the common case. The allocation only escapes to a free-list entry if the format output is large enough to be worth tracking individually (>512 bytes, per §4).

**Alternative considered:** Keep `StackBuf` for outputs under 256 bytes; spill to slab above that. Adds a branch and complexity. The bump-pointer slab cost is so low that the optimization is premature. Rejected.

---

## 10. Open questions

This section now separates decisions already locked for Phase 1/2 from questions still open for later phases.

---

### OQ1. What is the scheduler's idle detection threshold?

The IO driver is polled when the scheduler is "idle" (no coroutines are runnable). But "idle" has a threshold problem: if one coroutine is CPU-bound, the scheduler's run queue has one entry and is not empty, so `ty_io_poll()` is never called. IO completions for other coroutines accumulate in the completion queue, adding latency.

**Options:**
- A. Poll IO on every N scheduler ticks, not just when idle. N = 64 (one poll per 64 coroutine context switches). Low overhead; bounded IO latency.
- B. Preemptive interrupt also drains IO (the existing `SIGPROF` handler calls `ty_io_poll()`). Adds IO polling to the preemption path.
- C. Accept that IO latency in CPU-bound workloads is bounded by the preemption interval (10ms). Document this as a known limitation.

**Current leaning:** Option A. It's simple, adds at most one `io_uring_peek_cqe` syscall per 64 context switches, and caps IO latency at `64 × avg_coro_quantum`.

**Must answer before:** Phase 4, Task 4.1 (hooking `ty_io_poll` into the scheduler).

---

### OQ2. How does `join` interact with partial failures? (Resolved for Phase 1/2)

`join { op_a, op_b }` returns both results as a tuple. If `op_a` fails and `op_b` succeeds, both results are returned. The caller decides what to do with the error.

If `op_a` fails and `op_b` succeeds, both results are returned. Caller policy decides whether to retry, compensate, or cancel follow-up work.

Decision:
- `join` returns all arm results; no implicit sibling cancellation.
- `join?` is deferred and not part of Phase 1/2.
- Cross-arm cancellation is explicit via `CancelToken` when Phase 5 cancellation support lands.

---

### OQ3. What is the socket ownership model when reads and writes are concurrent? (Resolved)

Decision: Option A is adopted.
- `Socket.split() -> (ReadSocket, WriteSocket)` is required before stream-channel conversion.
- `ReadSocket.into_chan(chunk_size, cap) -> chan<Buf>` owns read direction only.
- `WriteSocket` is the only write-capable handle.
- The runtime may share one underlying fd internally, but ownership is directionally encoded in the type system.

---

### OQ4. How do linear IO types interact with `select`? (Resolved for spec model)

`select` waits on multiple channels simultaneously. The spec (§18 Open Questions) notes that `select` ownership semantics are unresolved: all arms must consume the same external live bindings.

For IO operations, `select` would look like:

```typhoon
select {
    socket_a.read(buf_a) |> |(s, b, r)| handle_a(s, b, r),
    socket_b.read(buf_b) |> |(s, b, r)| handle_b(s, b, r),
}
```

Both `socket_a` and `socket_b` are consumed by their respective arms. If `socket_a`'s read fires, `socket_b` was never consumed — its binding is still live in the parent scope. But `socket_b` had a `recv()` SQE submitted to the driver. That SQE is now orphaned.

The liveness checker sees `socket_b` as live (the arm didn't execute), which is correct from the type perspective. But the driver still has an outstanding op on `socket_b`'s fd. If the caller then passes `socket_b` to another `select`, two `recv()` SQEs will be outstanding on the same fd — which may produce duplicate data.

Decision: Option C is adopted.
- IO arms in `select` register readiness interest first.
- Losing arms do not create orphaned submitted ops.
- Submission occurs for the selected-ready arm (hybrid readiness/proactor behavior).
- This is a Phase 2 constraint and informs driver shape before full Phase 4 integration.

---

### OQ5. Is there a per-task slab limit for IO-path allocations?

The slab is 4 MB by default (§15). `into_chan` with `cap=8` and `chunk_size=4096` uses 32 KB of slab for the channel ring + 8 × 4 KB for the `Buf` objects = ~64 KB per socket. With 10,000 concurrent connections, that is 640 MB of slab memory — far more than any per-task slab.

This is not a contradiction: the 10,000 connections are 10,000 separate coroutines, each with its own slab. 10,000 × 4 MB = 40 GB of virtual address space reserved, but actual physical pages are only allocated on access (demand paging). A coroutine that has 64 KB of active IO buffers uses 64 KB of physical memory, not 4 MB.

**The open question is:** is 4 MB a sensible default slab size for IO-heavy coroutines, or should `conc` spawns for IO handlers use a smaller slab (e.g. `conc(slab: 512kb) { handle_connection(socket) }`)? Smaller slabs reduce virtual address space pressure and may improve TLB performance at 10K+ concurrency.

**Must answer before:** Phase 5 benchmark (10K connections). May require adding a recommended slab size to the HTTP server example in §16.

---

## 11. Appendix: Canonical API surface

Complete method signatures after all five phases. This is the target state for `spec.md §14`.

```typhoon
// ── IoError ───────────────────────────────────────────────────────────────

enum IoError {
    NotFound(Str),
    PermissionDenied,
    ConnectionReset,
    ConnectionRefused,
    TimedOut,
    BrokenPipe,
    Eof,
    Cancelled,
    OutputTruncated,
    Os(Int32),
}

// ── Mode ──────────────────────────────────────────────────────────────────

enum Mode {
    Read,
    Write,
    ReadWrite,
    Append,
    CreateWrite,
    CreateAppend,
}

// ── SeekPos ───────────────────────────────────────────────────────────────

enum SeekPos {
    Start(Int64),
    End(Int64),
    Current(Int64),
}

// ── File (linear) ─────────────────────────────────────────────────────────

fn fs::open(path: Str, mode: Mode) -> Result<File, IoError>

impl File {
    fn read(self, buf: Buf)
        -> (File, Buf, Result<Int32, IoError>)

    fn write(self, buf: Buf)
        -> (File, Buf, Result<Int32, IoError>)

    fn read_vectored(self, bufs: [Buf])
        -> (File, [Buf], Result<Int32, IoError>)

    fn write_vectored(self, bufs: [Buf])
        -> (File, [Buf], Result<Int32, IoError>)

    fn seek(self, pos: SeekPos)
        -> (File, Result<Int64, IoError>)

    fn close(self) -> ()
}

// ── Network / Listener / Socket (linear) ──────────────────────────────────

impl Network {
    fn listen(self, addr: Str)   -> Result<Listener, IoError>
    fn connect(self, addr: Str)  -> Result<Socket, IoError>
    fn split(self)               -> (Network, Network)
}

impl Listener {
    fn accept(self) -> (Listener, Result<Socket, IoError>)
    fn close(self)  -> ()
}

impl Socket {
    fn read(self, buf: Buf)
        -> (Socket, Buf, Result<Int32, IoError>)

    fn write(self, buf: Buf)
        -> (Socket, Buf, Result<Int32, IoError>)

    fn read_vectored(self, bufs: [Buf])
        -> (Socket, [Buf], Result<Int32, IoError>)

    fn write_vectored(self, bufs: [Buf])
        -> (Socket, [Buf], Result<Int32, IoError>)

    // Primary backpressured stream API
    // chunk_size: bytes per Buf; cap: channel capacity (max buffered chunks)
    fn split(self) -> (ReadSocket, WriteSocket)

    fn close(self) -> ()
}

// ── Timeout ───────────────────────────────────────────────────────────────

// Available on any IO-returning expression
// Desugars to a join of the op and a timer op
fn .timeout(dur: Duration) -> Result<T, IoError>

// ── CancelToken ───────────────────────────────────────────────────────────

fn CancelToken::new() -> CancelToken
fn CancelToken::cancel(self)   // cancels all ops registered with this token

// ── stdin / stdout / stderr (non-linear singletons) ───────────────────────

fn stdin::read_line()   -> Result<Buf, IoError>
fn stdin::read_bytes(n: Int32) -> Result<Buf, IoError>

fn stdout::write(s: Str)  -> Result<(), IoError>
fn stderr::write(s: Str)  -> Result<(), IoError>

fn println(s: Str)     -> ()   // panics on error; convenience only
fn eprintln(s: Str)    -> ()

// ── join (language construct, not a function) ──────────────────────────────

// Syntax (see spec §11):
// let (r1, r2) = join { expr_a, expr_b }
// Submits all ops in one kernel call; resumes when all complete
// Partial-failure semantics: join returns all arm results (no implicit cancellation)
```

---

## 12. Review

### Scheduler ↔ IO driver dependency
The proposal's D3 principle ("IO depends on the scheduler; the scheduler does not depend on IO") is stated correctly, but the current io_driver.c already inverts it in a subtle way.
The poll thread calls sched_enqueue_from_external(coro) to wake a completed coroutine. That function is defined in scheduler.c. So right now the dependency flows both ways: ty_io.c → scheduler.h (for ty_coro_block_and_yield) and io_driver.c → scheduler.c (for sched_enqueue_from_external). The proposal describes eliminating the dedicated poll thread and moving backend->poll() into the scheduler's idle path — that's the right fix, but it's described as Phase 4, not Phase 0. Until then, the circular dependency is live.
There's also a subtler issue: sched_enqueue_from_external unconditionally pushes onto workers[0]'s deque, bypassing work-stealing entirely. This is mentioned with a commented-out CAS in the code. When the poll thread fires many completions at once (high-concurrency IO bursts), they all pile onto one worker, causing a thundering-herd pattern on wake. The proposal doesn't address this — it should at least call it out as something that gets resolved naturally once the idle-path polling model lands in Phase 4.

### Scheduler ↔ network dependency
ty_net.c currently bypasses the IO driver entirely. socket_consumer_coro calls recv() directly as a blocking syscall, which means it holds an OS thread for the duration of the receive. The scheduler's M:N model is undermined: a coroutine that's nominally "async" is actually stealing a worker thread from everyone else.
The proposal correctly identifies this (socket_consumer_coro is "architecturally wrong") and deletes it in Task 2.1. What's missing is an explicit statement that until Phase 2 ships, ty_net.c is a correctness hazard under any workload where the number of concurrent connections can exceed num_workers. If you have 8 workers and 9 simultaneous blocked recv() calls, the scheduler deadlocks. This should be called out in Phase 0 or at minimum in the Phase 2 preamble as a hard concurrency ceiling.
The proposed replacement (Socket__read posting a recv() op via TyIoOp) is the right shape, but Task 2.2's checklist references ty_sched_io_backend() and TyIoBackend* — types that don't exist yet. There's a dependency on Phase 4 (driver integration) hidden inside Phase 2 (socket rewrite). The phases need to be sequenced more carefully: either Phase 4's TyIoBackend interface gets defined first (even as a stub), or Phase 2 must continue routing through the existing io_driver.c API and the interface swap happens in Phase 4.

### IO driver dependency on malloc
The PendingPool is a fixed array of 256 PendingReq slots allocated in g_driver (static storage). That's good — no malloc in the IO hot path for submissions. But io_driver.c calls ty_vm_alloc for the UringDriver struct itself, for DequeRetiredNode (EBR), and for the platform-specific backend structs. These aren't hot-path allocations but they're not ty_arena_alloc either.
The proposal's D5 ("no malloc/free in the IO hot path") says malloc calls in ty_io.c and ty_net.c will be a CI build failure — but io_driver.c uses ty_vm_alloc, not malloc, so it would pass that check. The CI gate needs to cover both, or the rule needs to be stated more precisely.

### Specific issues in ty_net.c the proposal misses
Listener__accept is blocking. The current implementation calls accept() directly, which blocks the OS thread just like recv() in socket_consumer_coro. This isn't mentioned as a bug to fix — it's implied by Task 2.2's checklist ("implement __ty_rt__Listener__accept with the same pattern") but deserves explicit treatment, because a server loop calling accept() in a tight loop has the same worker-thread starvation problem as multiple concurrent recv() calls.
ty_net_shutdown vs Socket__close race. Task 0.3 correctly fixes the double-free in Socket__close, but the race between ty_net_shutdown stealing the g_sockets list and a concurrent Socket__close isn't fully resolved by the closed flag. If Socket__close is executing between the list removal and free(self) on one thread while ty_net_shutdown is running on another, the mutex protects the list traversal but not the ty_sock_close(self->sock) call after the unlock. The proposed fix adds the closed flag check, but ty_sock_close could still double-close the fd if shutdown races with close after the list walk. This needs the fd to be set to an invalid sentinel (e.g. -1) before releasing the lock.

### Open questions assessment (addressed in v1.2)
OQ3 (split socket halves) is the most important to resolve before Phase 2 ships, not before Task 1.6 as noted. The into_chan implementation in Task 2.3 returns (Socket, chan<Buf>), which means the caller holds a Socket they can write to while the spawned reader coroutine holds a reference to the same underlying fd. That's the split-ownership problem. You can't implement Task 2.3 correctly without having answered OQ3 — Option A (split into ReadSocket/WriteSocket) or Option C (consume entirely, write-only handle) need to be in place first.
OQ4 (select over IO) has a clean answer that the proposal somewhat obscures: if you adopt a readiness model (register interest, submit SQE on readiness) for IO in select, it composes naturally with the existing channel select semantics. The current proactor model (submit immediately, cancel losers) requires explicit cancellation SQEs and is harder. The proposal's Option C is the right one for select compatibility, but it means the driver needs a hybrid model from the start — this should be surfaced as a Phase 2 constraint, not left open until "before any select over IO operations is documented."
Status in this revision: OQ3 and OQ4 are now resolved and reflected in Phase 1, Phase 2, and Appendix API signatures.

Minor items
The task parameter in nearly all ty_net.c functions is (void)task suppressed — it's passed through for future slab allocation but never used. Phase 0 Task 0.3's checklist should add threading task through the net functions as a prerequisite step for D5, since the slab allocator needs it.
The proposal also doesn't address Listener__close. The existing code has no __ty_rt__Listener__close at all — g_listeners entries are only freed in ty_net_shutdown. That means any listener the user closes before shutdown leaks the fd and the struct until process exit. This is a Phase 0-severity issue that's absent from the Phase 0 list.
Overall, the sequencing of Phase 0 → 1 → 2 is sound, and D1–D5 are good settled decisions. The main gap is that several Phase 2 tasks have hidden dependencies on Phase 4 interfaces that need to be surfaced explicitly before implementation begins.

*End of document. Version 1.1. Next review: after Phase 0 and Phase 1 are complete.*
impl ReadSocket {
    fn read(self, buf: Buf)
        -> (ReadSocket, Buf, Result<Int32, IoError>)

    fn read_vectored(self, bufs: [Buf])
        -> (ReadSocket, [Buf], Result<Int32, IoError>)

    fn into_chan(self, chunk_size: Int32, cap: Int32)
        -> chan<Buf>

    fn close(self) -> ()
}

impl WriteSocket {
    fn write(self, buf: Buf)
        -> (WriteSocket, Buf, Result<Int32, IoError>)

    fn write_vectored(self, bufs: [Buf])
        -> (WriteSocket, [Buf], Result<Int32, IoError>)

    fn close(self) -> ()
}
