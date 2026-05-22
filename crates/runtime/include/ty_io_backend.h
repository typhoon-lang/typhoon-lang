#pragma once
#include <stdint.h>
#include <stddef.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct TyIoOp {
    int type;
    int fd;
    void* buf;
    size_t len;
    void* coro;
} TyIoOp;

typedef void (*TySchedWakeFn)(void* coro, int64_t result);

typedef struct TyIoBackend {
    void* impl;
    int (*submit)(struct TyIoBackend* backend, const TyIoOp* op);
    int (*poll)(struct TyIoBackend* backend, TySchedWakeFn wake);
} TyIoBackend;

enum {
    TY_IO_OP_READ  = 1,
    TY_IO_OP_WRITE = 2,
    TY_IO_OP_ACCEPT = 3
};

int ty_io_submit(const TyIoOp* op);
int ty_io_poll(void);

#ifdef __cplusplus
}
#endif

