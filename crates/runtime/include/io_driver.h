/*
 * io_driver.h — Typhoon async I/O driver (pure C, no Rust)
 *
 * Platform backends selected at compile time:
 *   Linux   : io_uring (kernel ≥ 5.1)
 *   macOS   : kqueue
 *   Windows : IOCP
 *
 * Everything in this header is callable from ty_io.c and scheduler.c.
 * The actual backends live in io_driver.c.
 */

#pragma once
#include "ty_mem.h"
#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/* ── Subsystem lifecycle (called by ty_sched_init / ty_sched_shutdown) ────────
 */

void ty_io_subsystem_init(void);
void ty_io_subsystem_shutdown(void);

/* ── Driver accessor used by ty_io.c
 * ─────────────────────────────────────────── */

void* ty_io_global_driver(void);

/* ── File open / close
 * ───────────────────────────────────────────────────────── */

/*
 * ty_io_open
 *   flags: 0 = O_RDONLY, 1 = O_WRONLY|O_CREAT|O_TRUNC, 2 = O_RDWR|O_CREAT
 *   mode : permission bits (e.g. 0644); ignored on Windows
 *   Returns fd ≥ 0 on success, -1 on error.
 */
int ty_io_open(void* driver, const char* path, int flags, unsigned mode);
void ty_io_close(void* driver, int fd);

/* ── Async read / write (park coroutine until completion)
 * ────────────────────── */

/*
 * When called from inside a coroutine these submit the I/O to the platform
 * backend, park the coroutine (CORO_BLOCKED), and return.  The poll thread
 * calls ty_io_wake_coro() once the kernel signals completion; the coroutine
 * resumes and should call ty_io_take_result() to get the byte count / errno.
 *
 * When called from the main thread (before scheduler starts) they fall back
 * to synchronous blocking I/O and return immediately with the result stored
 * in the result slot (ty_io_take_result returns it right away).
 */
int64_t ty_io_read(void* driver, SlabArena* arena, void* coro, int fd, uint8_t* buf,
    size_t len);
int64_t ty_io_write(void* driver, SlabArena* arena, void* coro, int fd,
    const uint8_t* buf, size_t len);

/* ── Per-coroutine result slot
 * ────────────────────────────────────────────────── */

/*
 * ty_io_take_result
 *   Read-and-clear the I/O result stored for `coro` by the poll thread.
 *   Call this once after the coroutine resumes from a parked state.
 *   Returns bytes transferred (≥ 0) or -errno on error.
 */
int64_t ty_io_take_result(void* coro);

/* I/O support */
int64_t ty_coro_get_io_result(void* coro);
void ty_coro_set_io_result(void* coro, int64_t res);

/* ── Park / wake (also called from ty_io.c)
 * ──────────────────────────────────── */

void ty_io_park_coro(SlabArena* arena); /* block current coro           */
void ty_io_wake_coro(void* coro, int64_t result); /* wake from poll thread  */

#ifdef __cplusplus
}
#endif
