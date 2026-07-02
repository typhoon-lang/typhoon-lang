/*
 * ty_io_uring.h — Linux io_uring backend for Typhoon IO
 *
 * One io_uring ring per scheduler worker thread.
 * submit() fills SQE and calls io_uring_submit (or batches for join).
 * poll() peeks CQEs and calls wake().
 */
#pragma once

#include "ty_io_backend.h"

#ifdef __linux__

#ifdef __cplusplus
extern "C" {
#endif

typedef struct TyUringBackend {
    TyIoBackend base;
    int ring_fd;
    /* ring mappings — filled by ty_uring_backend_new */
    void* sq_ring;
    void* cq_ring;
    void* sqes;
    uint32_t sq_mask;
    uint32_t cq_mask;
    uint32_t sq_entries;
    uint32_t cq_entries;
    /* layout offsets */
    uint32_t sq_head_off, sq_tail_off, sq_array_off;
    uint32_t cq_head_off, cq_tail_off, cq_cqes_off;
} TyUringBackend;

/* Create an io_uring backend for the calling worker thread. */
TyUringBackend* ty_uring_backend_new(void);

/* Destroy ring and release resources. */
void ty_uring_backend_destroy(TyUringBackend* b);

#ifdef __cplusplus
}
#endif

#endif /* __linux__ */
