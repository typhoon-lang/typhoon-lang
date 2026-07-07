/* ty_net.h — Typhoon networking (capability-gated, runtime-provided)
 *
 * This is intentionally small: it exposes an opaque Network capability token
 * plus Listener/Socket handles and a couple of core operations.
 */

#pragma once
#include <stdint.h>
#include "ty_mem.h"

#ifdef __cplusplus
extern "C" {
#endif

typedef struct TyNetwork TyNetwork;
typedef struct TyListener TyListener;
typedef struct TySocket TySocket;
typedef struct TyReadSocket TyReadSocket;
typedef struct TyWriteSocket TyWriteSocket;

// Field layout mirrors what the compiler derives from `enum Result<T, E> { Ok(T), Err(E) }`
// in result.ty: tag is field 0, then one payload slot per variant in
// declaration order (Ok's payload first, Err's payload second).
// tag = 0 means Ok (declared first), tag = 1 means Err (declared second).
// This must stay in lockstep with result.ty's variant order — if that
// order ever changes, these tag values must change too.
//
// ABI: every Result<T, E> struct below is >8 bytes, which on Windows x64
// means it can never be returned in registers (Windows has no SysV-style
// register-pair packing for aggregates — anything over 8 bytes always
// crosses via a hidden out-pointer). To stay correct on every target this
// project builds for, all functions returning one of these structs take an
// explicit trailing out-pointer and return void, rather than returning the
// struct by value. codegen.rs's `needs_out_result_abi` computes each
// struct's size from the Typhoon-level Result<T, E> signature and applies
// this same convention automatically on the caller side — the two must
// stay in agreement.
//
// `err` below is int32_t, matching the small error codes actually stored
// in it (-1/-2/-3 sentinels, errno, WSA codes, getaddrinfo codes) and
// matching what the LLVM-side struct layout treats that field as (an i32,
// not a pointer). It was previously declared `void*`, which happened to
// still produce a correctly-sized struct (padding absorbed the extra
// width) but relied on undefined-behavior-adjacent implicit int-to-pointer
// conversions at every assignment site — harmless on MSVC/clang-Windows in
// practice, but a hard compile error on Linux/macOS clang.

/* Result<Listener, Int32> */
typedef struct TyResult_Listener_i32 {
	int32_t tag; /* 0 = Ok, 1 = Err */
	TyListener* value; /* valid when tag=0 */
	int32_t err; /* valid when tag=1 — small error code, never a pointer */
} TyResult_Listener_i32;

/* Result<Socket, Int32> */
typedef struct TyResult_Socket_i32 {
	int32_t tag; /* 0 = Ok, 1 = Err */
	TySocket* value; /* valid when tag=0 */
	int32_t err; /* valid when tag=1 — small error code, never a pointer */
} TyResult_Socket_i32;

void ty_net_init(void);
void ty_net_shutdown(void);
TyNetwork* ty_net_global(void);

typedef struct {
    TyReadSocket*  read;
    TyWriteSocket* write;
} TySplitResult;

/* LLVM-emitted method symbols */
void __ty_rt__Network__listen(void* task, TyNetwork* self, TyStr* addr, TyResult_Listener_i32* out);
void __ty_rt__Network__dial(void* task, TyNetwork* self, TyStr* addr, TyResult_Socket_i32* out);
void __ty_rt__Listener__accept(void* task, TyListener* self, TyResult_Socket_i32* out);
void __ty_rt__Listener__close(void* task, TyListener* self);
void __ty_rt__Socket__close(void* task, TySocket* self);
TySplitResult __ty_rt__Socket__split(void* task, TySocket* self);

/* Phase 4: canonical async read/write via TyIoOp.
 * Read returns (Socket, Buf, Result<Int32, IoError>).
 * Write returns (Socket, Buf, Result<Int32, IoError>). */
typedef struct TyResult_i32_i32 {
	int32_t tag; /* 0 = Ok, 1 = Err */
	int32_t value; /* valid when tag=0 */
	int32_t err; /* valid when tag=1 — small error code, never a pointer */
} TyResult_i32_i32;
void __ty_rt__Socket__read(void* task, TySocket* self, char* buf, int32_t cap, TyResult_i32_i32* out);
void __ty_rt__Socket__write(void* task, TySocket* self, char* buf, int32_t len, TyResult_i32_i32* out);
/* chan<T> lowers to a bare i8*\/struct TyChan* (same slot as Ref in
 * codegen.rs), not an out-pointer-requiring aggregate like Result<T,E> —
 * returned by value, 4 params. This was previously a 5-param/void/
 * out-pointer signature that didn't match what net.ty's own declaration
 * (`-> ref chan<Buf>`) causes the compiler to actually expect, which is
 * what caused the "conflicting types" build error. */
struct TyChan* __ty_rt__ReadSocket__into_chan(void* task, TyReadSocket* self, int64_t chunk_size, int64_t cap);
void __ty_rt__ReadSocket__close(void* task, TyReadSocket* self);
void __ty_rt__WriteSocket__close(void* task, TyWriteSocket* self);
void __ty_rt__WriteSocket__write(void* task, TyWriteSocket* self, TyStr* buf, int32_t len, TyResult_i32_i32* out);

/* Blocking / non-blocking byte receive over a channel populated by
 * Socket__consume's background reader coroutine. Same TyResult_i32_i32
 * shape and out-pointer convention as read/write above — these were
 * previously missing from this header entirely (declared only implicitly
 * via their .c definitions), which is a real gap: the .ty-level extern
 * block for these must declare them with a matching out-pointer signature,
 * or codegen's auto out_result detection and this header will disagree. */
struct TyChan;
void __ty_rt__Socket__recv(void* task, TySocket* self, struct TyChan* chan, TyResult_i32_i32* out);
void __ty_rt__Socket__try_recv(void* task, TySocket* self, struct TyChan* chan, TyResult_i32_i32* out);

#ifdef __cplusplus
}
#endif
