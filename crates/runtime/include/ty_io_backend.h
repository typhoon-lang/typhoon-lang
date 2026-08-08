#pragma once
#include "platform.h"
#include <stdint.h>
#include <stddef.h>
#include <stdio.h>
#ifdef __linux__
#include <sys/uio.h>   /* struct iovec — needed inside TyIoOp for
                        * IORING_OP_READV/WRITEV iovec submission */
#endif

#ifdef __cplusplus
extern "C" {
#endif

typedef struct TyIoOp {
  int type;
  ty_fd_t fd;         /* Phase 4: ty_fd_t for Windows SOCKET compat */
  void* buf;
  size_t len;
  void* coro;
  int32_t cancel_token; /* reserved for Phase 5 */
  void* file_ptr;     /* For Windows FILE_FLAG_OVERLAPPED files: pointer to TyFile for position tracking */
#ifdef __linux__
  struct iovec iov;   /* IORING_OP_READV/WRITEV need an iovec, not raw
                       * buf+len. sqe->len for vectored ops is the iovec
                       * *count*, not byte count — passing buf directly
                       * makes the kernel treat buf as an iovec array
                       * and causes EFAULT.  Storing the iovec here
                       * (in stack-local op, alive while coro parked)
                       * keeps the submission-side code simple. */
#endif
} TyIoOp;

typedef void (*TySchedWakeFn)(void* coro, int64_t result);

typedef struct TyIoBackend {
  void* impl;
  int (*submit)(struct TyIoBackend* backend, const TyIoOp* op);
  int (*poll)(struct TyIoBackend* backend, TySchedWakeFn wake);
  /* Readiness-based backends (kqueue, epoll) must submit immediately.
   * Completion-based backends (io_uring, IOCP) defer submit until after park.
   * 0 = completion-based (deferred), 1 = readiness-based (immediate) */
  int readiness_based;
} TyIoBackend;

enum {
  TY_IO_OP_READ = 1,
  TY_IO_OP_WRITE = 2,
  TY_IO_OP_ACCEPT = 3,
  TY_IO_OP_CONNECT = 4
};

int ty_io_submit(const TyIoOp* op);
int ty_io_poll(void);

/* Testing/mock helpers */
void ty_io_backend_use_mock(int use);
int ty_io_mock_count(void);
const TyIoOp* ty_io_mock_get(int idx);
void ty_io_mock_complete(int idx, int64_t result);

#ifdef __cplusplus
}
#endif
