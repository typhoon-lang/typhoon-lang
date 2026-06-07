/* ty_net.h — Typhoon networking (capability-gated, runtime-provided)
 *
 * This is intentionally small: it exposes an opaque Network capability token
 * plus Listener/Socket handles and a couple of core operations.
 */

#pragma once
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct TyNetwork TyNetwork;
typedef struct TyListener TyListener;
typedef struct TySocket TySocket;

/* Result<Listener, Int32> */
typedef struct TyResult_Listener_i32 {
	uint8_t ok; /* 0/1 */
	TyListener* value; /* valid when ok=1 */
	int32_t err; /* valid when ok=0 */
} TyResult_Listener_i32;

/* Result<Socket, Int32> */
typedef struct TyResult_Socket_i32 {
	uint8_t ok; /* 0/1 */
	TySocket* value; /* valid when ok=1 */
	int32_t err; /* valid when ok=0 */
} TyResult_Socket_i32;

void ty_net_init(void);
void ty_net_shutdown(void);
TyNetwork* ty_net_global(void);

/* LLVM-emitted method symbols */
void __ty_rt__Network__listen(void* task, TyNetwork* self, char* addr, TyResult_Listener_i32* out);
void __ty_rt__Listener__accept(void* task, TyListener* self, TyResult_Socket_i32* out);
void __ty_rt__Listener__close(void* task, TyListener* self);
void __ty_rt__Socket__close(void* task, TySocket* self);

/* Phase 4: canonical async read/write via TyIoOp.
 * Read returns (Socket, Buf, Result<Int32, IoError>).
 * Write returns (Socket, Buf, Result<Int32, IoError>). */
typedef struct TyResult_i32_i32 {
	uint8_t ok;
	int32_t value;
	int32_t err;
} TyResult_i32_i32;
TyResult_i32_i32 __ty_rt__Socket__read(void* task, TySocket* self, char* buf, int32_t cap);
TyResult_i32_i32 __ty_rt__Socket__write(void* task, TySocket* self, char* buf, int32_t len);

#ifdef __cplusplus
}
#endif
