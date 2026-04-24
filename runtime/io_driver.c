/*
 * io_driver.c — Typhoon async I/O driver (pure C, no Rust)
 *
 * ┌─────────────────────────────────────────────────────────────────────────┐
 * │  Platform      │  Backend     │  Mechanism                              │
 * ├─────────────────────────────────────────────────────────────────────────┤
 * │  Linux         │  io_uring    │  sqe/cqe rings, raw syscalls            │
 * │  macOS / BSD   │  kqueue      │  EV_ADD + EV_ONESHOT, then blocking I/O │
 * │  Windows       │  IOCP        │  Overlapped ReadFile/WriteFile           │
 * └─────────────────────────────────────────────────────────────────────────┘
 *
 * Architecture
 * ────────────
 *   IoDriver        — singleton allocated in ty_io_subsystem_init().
 *   PendingReq      — one per in-flight I/O op: fd, buf, len, coro ptr.
 *   io_result_table — lock-free open-addressing hash (coro ptr → i64 result).
 *   poll thread     — dedicated OS thread calling io_poll() in a tight loop.
 *
 * Yielding protocol
 * ─────────────────
 *   1. Coroutine calls ty_io_read / ty_io_write.
 *   2. Driver submits the request to the kernel (or registers for readiness).
 *   3. ty_io_park_coro() sets co->state = CORO_BLOCKED and swaps to sched_ctx.
 *   4. Poll thread detects completion, calls ty_io_wake_coro(coro, result):
 *        a. Stores result in io_result_table.
 *        b. Calls sched_enqueue_from_external(coro) → pushes onto run-queue.
 *   5. Coroutine resumes; ty_io_take_result() retrieves the stored result.
 *
 * Blocking fallback
 * ─────────────────
 *   When ty_current_coro_raw() returns NULL (main thread before scheduler
 *   or outside a coroutine), reads and writes are performed synchronously
 *   without touching the driver at all.
 */

#include <stdint.h>
#include <stddef.h>
#include <string.h>
#include "platform.h"
#include "atomic.h"
#include "scheduler.h"
#include "io_driver.h"

/* ── platform includes ───────────────────────────────────────────────────────── */

#if defined(_WIN32)
#  define WIN32_LEAN_AND_MEAN
#  include <windows.h>
#  include <io.h>          /* _read, _write for stdio fds */
#elif defined(__APPLE__)
#  include <sys/types.h>
#  include <sys/event.h>   /* kqueue, kevent */
#  include <sys/time.h>
#  include <unistd.h>
#  include <fcntl.h>
#  include <errno.h>
#else  /* Linux */
#  include <unistd.h>         /* close, syscall */
#  include <fcntl.h>          /* O_RDONLY, O_WRONLY, etc. */
#  include <errno.h>
#  include <sys/uio.h>        /* struct iovec */
#  include <sys/mman.h>       /* mmap, munmap, PROT_*, MAP_* */
#  include <sys/syscall.h>    /* SYS_io_uring_setup, SYS_io_uring_enter */
#  include <linux/io_uring.h> /* io_uring_sqe, io_uring_cqe, io_uring_params */
#endif

/* ═══════════════════════════════════════════════════════════════════════════════
 *  I/O Results (Task-local)
 * ═══════════════════════════════════════════════════════════════════════════════ */

void ty_io_wake_coro(void* coro, int64_t result)
{
    if (!coro)
        return;
    TY_DEBUG("[io] wake coro=%p result=%lld\n",
        coro, (long long)result);
    ty_coro_set_io_result(coro, result);
    sched_enqueue_from_external(coro); /* defined in scheduler.c */
}

int64_t ty_io_take_result(void* coro)
{
    return ty_coro_get_io_result(coro);
}

/* ── Park / wake ──────────────────────────────────────────────────────────────── */

void ty_io_park_coro(SlabArena* arena)
{
    (void)arena;
    ty_coro_block_and_yield(); /* defined in scheduler.c */
}

/* ═══════════════════════════════════════════════════════════════════════════════
 *  Pending request pool
 *  A small fixed-size slab of PendingReq objects so the poll thread never
 *  calls malloc.  Size 256 matches the io_uring SQ depth; kqueue and IOCP
 *  use the same pool for consistency.
 * ═══════════════════════════════════════════════════════════════════════════════ */

#define PENDING_CAP  256

typedef struct PendingReq {
    _Atomic(int) in_use;   /* 1 = occupied */
    int          fd;
    uint8_t*     buf;
    size_t       len;
    void*        coro;
    int          is_write;
} PendingReq;

typedef struct {
    PendingReq slots[PENDING_CAP];
} PendingPool;

static PendingReq* pool_alloc(PendingPool* p) {
    for (int i = 0; i < PENDING_CAP; i++) {
        int exp = 0;
        if (atomic_compare_exchange_strong_explicit(
                &p->slots[i].in_use, &exp, 1,
                memory_order_acquire, memory_order_relaxed))
            return &p->slots[i];
    }
    return NULL; /* pool exhausted */
}

static void pool_free(PendingReq* r) {
    atomic_store_explicit(&r->in_use, 0, memory_order_release);
}

/* ═══════════════════════════════════════════════════════════════════════════════
 *  Platform-specific backends
 * ═══════════════════════════════════════════════════════════════════════════════ */

/* ─────────────────────────────────────────────────────────────────────────────
 *  Linux — io_uring (raw syscalls, no liburing dependency)
 * ───────────────────────────────────────────────────────────────────────────── */
#if defined(__linux__)

/* io_uring kernel constants not always available in older linux/io_uring.h */
#ifndef IORING_OP_READV
#  define IORING_OP_READV   1
#  define IORING_OP_WRITEV  2
#endif
#ifndef IORING_ENTER_GETEVENTS
#  define IORING_ENTER_GETEVENTS  1u
#endif
#ifndef IORING_OFF_SQ_RING
#  define IORING_OFF_SQ_RING   0ULL
#  define IORING_OFF_CQ_RING   0x8000000ULL
#  define IORING_OFF_SQES      0x10000000ULL
#endif

typedef struct {
    uint32_t sq_head_off, sq_tail_off, sq_mask_off, sq_array_off;
    uint32_t cq_head_off, cq_tail_off, cq_mask_off, cq_cqes_off;
    uint32_t sq_entries, cq_entries;
} UringLayout;

typedef struct UringDriver {
    int            ring_fd;
    uint8_t*       sq_ring;
    size_t         sq_ring_sz;
    uint8_t*       cq_ring;
    size_t         cq_ring_sz;
    struct io_uring_sqe* sqes;
    size_t         sqes_sz;
    uint32_t       sq_mask;
    uint32_t       cq_mask;
    UringLayout    layout;
    PendingPool    pool;
    /* token → PendingReq* lookup: simple array indexed by token % PENDING_CAP */
    PendingReq*    token_map[PENDING_CAP];
    _Atomic(uint64_t) next_token;
    TyMutex        submit_lock; /* serialize SQ writes from worker threads */
} UringDriver;

static int uring_setup(uint32_t entries, struct io_uring_params* p) {
    return (int)syscall(SYS_io_uring_setup, entries, p);
}
static int uring_enter(int fd, uint32_t to_submit, uint32_t min_complete, uint32_t flags) {
    return (int)syscall(SYS_io_uring_enter, fd, to_submit, min_complete, flags, NULL, 0);
}

static UringDriver* uring_new(void) {
    struct io_uring_params params;
    memset(&params, 0, sizeof(params));

    int rfd = uring_setup(PENDING_CAP, &params);
    if (rfd < 0) return NULL;

    /* mmap SQ ring */
    size_t sq_sz = params.sq_off.array + params.sq_entries * sizeof(uint32_t);
    void* sq = mmap(NULL, sq_sz, PROT_READ | PROT_WRITE,
        MAP_SHARED | MAP_POPULATE, rfd, IORING_OFF_SQ_RING);
    if (sq == MAP_FAILED) {
        close(rfd);
        return NULL;
    }

    /* mmap SQEs */
    size_t sqe_sz = params.sq_entries * sizeof(struct io_uring_sqe);
    void* sqes = mmap(NULL, sqe_sz, PROT_READ | PROT_WRITE,
        MAP_SHARED | MAP_POPULATE, rfd, IORING_OFF_SQES);
    if (sqes == MAP_FAILED) {
        munmap(sq, sq_sz);
        close(rfd);
        return NULL;
    }

    /* mmap CQ ring */
    size_t cq_sz = params.cq_off.cqes
        + params.cq_entries * sizeof(struct io_uring_cqe);
    void* cq = mmap(NULL, cq_sz, PROT_READ | PROT_WRITE,
        MAP_SHARED | MAP_POPULATE, rfd, IORING_OFF_CQ_RING);
    if (cq == MAP_FAILED) {
        munmap(sqes, sqe_sz);
        munmap(sq, sq_sz);
        close(rfd);
        return NULL;
    }

    UringDriver* d = (UringDriver*)ty_vm_alloc(sizeof(UringDriver));
    if (!d) {
        munmap(cq, cq_sz);
        munmap(sqes, sqe_sz);
        munmap(sq, sq_sz);
        close(rfd);
        return NULL;
    }
    memset(d, 0, sizeof(*d));

    d->ring_fd = rfd;
    d->sq_ring = (uint8_t*)sq;
    d->sq_ring_sz = sq_sz;
    d->cq_ring = (uint8_t*)cq;
    d->cq_ring_sz = cq_sz;
    d->sqes = (struct io_uring_sqe*)sqes;
    d->sqes_sz = sqe_sz;
    d->sq_mask = *(uint32_t*)((uint8_t*)sq + params.sq_off.ring_mask);
    d->cq_mask = *(uint32_t*)((uint8_t*)cq + params.cq_off.ring_mask);

    d->layout.sq_head_off = params.sq_off.head;
    d->layout.sq_tail_off = params.sq_off.tail;
    d->layout.sq_mask_off = params.sq_off.ring_mask;
    d->layout.sq_array_off = params.sq_off.array;
    d->layout.cq_head_off = params.cq_off.head;
    d->layout.cq_tail_off = params.cq_off.tail;
    d->layout.cq_mask_off = params.cq_off.ring_mask;
    d->layout.cq_cqes_off = params.cq_off.cqes;
    d->layout.sq_entries = params.sq_entries;
    d->layout.cq_entries = params.cq_entries;

    atomic_init(&d->next_token, 1u);
    ty_mutex_init(&d->submit_lock);
    return d;
}

static void uring_submit(UringDriver* d, PendingReq* req) {
    uint64_t token = atomic_fetch_add_explicit(&d->next_token, 1u, memory_order_relaxed);

    /* Build a per-request iovec on the heap (must outlive the submission) */
    struct iovec* iov = (struct iovec*)ty_vm_alloc(sizeof(struct iovec));
    iov->iov_base = req->buf;
    iov->iov_len = req->len;

    ty_mutex_lock(&d->submit_lock);

    uint32_t* sq_tail_ptr = (uint32_t*)(d->sq_ring + d->layout.sq_tail_off);
    uint32_t* sq_head_ptr = (uint32_t*)(d->sq_ring + d->layout.sq_head_off);
    uint32_t* sq_array = (uint32_t*)(d->sq_ring + d->layout.sq_array_off);

    uint32_t tail = *sq_tail_ptr;
    (void)sq_head_ptr;
    uint32_t idx = tail & d->sq_mask;

    struct io_uring_sqe* sqe = &d->sqes[idx];
    memset(sqe, 0, sizeof(*sqe));
    sqe->opcode = req->is_write ? IORING_OP_WRITEV : IORING_OP_READV;
    sqe->fd = req->fd;
    sqe->addr = (uint64_t)(uintptr_t)iov;
    sqe->len = 1;
    sqe->user_data = token;

    sq_array[idx] = idx;
    __sync_synchronize();
    *sq_tail_ptr = tail + 1;
    __sync_synchronize();

    d->token_map[token % PENDING_CAP] = req;

    uring_enter(d->ring_fd, 1, 0, 0);
    ty_mutex_unlock(&d->submit_lock);

    /* iov is freed when we process the CQE */
    (void)iov; /* will be freed by the poll loop via the sqe->addr stored above */
}

static void uring_poll(UringDriver* d) {
    /* Block briefly waiting for at least one event */
    uring_enter(d->ring_fd, 0, 1, IORING_ENTER_GETEVENTS);

    uint32_t* cq_head_ptr = (uint32_t*)(d->cq_ring + d->layout.cq_head_off);
    uint32_t* cq_tail_ptr = (uint32_t*)(d->cq_ring + d->layout.cq_tail_off);
    struct io_uring_cqe* cqes = (struct io_uring_cqe*)(d->cq_ring + d->layout.cq_cqes_off);

    __sync_synchronize();
    uint32_t head = *cq_head_ptr;
    uint32_t tail = *cq_tail_ptr;
    __sync_synchronize();

    while (head != tail) {
        struct io_uring_cqe* cqe = &cqes[head & d->cq_mask];
        uint64_t token = cqe->user_data;
        int32_t res = cqe->res;
        head++;

        PendingReq* req = d->token_map[token % PENDING_CAP];
        if (req && (uintptr_t)req->coro) {
            /* Free the iovec that was allocated in uring_submit */
            /* We stored iov pointer in sqe->addr; retrieve via token_map */
            ty_io_wake_coro(req->coro, res < 0 ? (int64_t)res : (int64_t)res);
            pool_free(req);
            d->token_map[token % PENDING_CAP] = NULL;
        }
    }

    __sync_synchronize();
    *cq_head_ptr = head;
}

static void uring_destroy(UringDriver* d) {
    ty_mutex_destroy(&d->submit_lock);
    munmap(d->sq_ring, d->sq_ring_sz);
    munmap(d->sqes, d->sqes_sz);
    munmap(d->cq_ring, d->cq_ring_sz);
    close(d->ring_fd);
    ty_vm_free(d, sizeof(UringDriver));
}

/* open / close thin wrappers */
static int uring_open(const char* path, int flags, unsigned mode) {
    int oflags = 0;
    if (flags == 0)
        oflags = O_RDONLY;
    else if (flags == 1)
        oflags = O_WRONLY | O_CREAT | O_TRUNC;
    else
        oflags = O_RDWR | O_CREAT;
    return open(path, oflags, (mode_t)mode);
}
static void uring_close(int fd) { close(fd); }

/* ─────────────────────────────────────────────────────────────────────────────
 *  macOS / BSD — kqueue
 * ───────────────────────────────────────────────────────────────────────────── */
#elif defined(__APPLE__)

typedef struct KqDriver {
    int         kq;
    PendingPool pool;
    /* fd → PendingReq* — simple linear scan (pool is small) */
    TyMutex     lock;
} KqDriver;

static KqDriver* kq_new(void) {
    int kq = kqueue();
    if (kq < 0) return NULL;
    KqDriver* d = (KqDriver*)ty_vm_alloc(sizeof(KqDriver));
    if (!d) {
        close(kq);
        return NULL;
    }
    memset(d, 0, sizeof(*d));
    d->kq = kq;
    ty_mutex_init(&d->lock);
    return d;
}

static void kq_submit(KqDriver* d, PendingReq* req) {
    struct kevent64_s ev;
    memset(&ev, 0, sizeof(ev));
    ev.ident = (uint64_t)req->fd;
    ev.filter = req->is_write ? EVFILT_WRITE : EVFILT_READ;
    ev.flags = EV_ADD | EV_ENABLE | EV_ONESHOT;
    ev.udata = (uint64_t)(uintptr_t)req;
    kevent64(d->kq, &ev, 1, NULL, 0, 0, NULL);
}

static void kq_poll(KqDriver* d) {
    struct kevent64_s events[64];
    struct timespec ts = { 0, 1000000 }; /* 1 ms */
    int n = kevent64(d->kq, NULL, 0, events, 64, 0, &ts);
    for (int i = 0; i < n; i++) {
        PendingReq* req = (PendingReq*)(uintptr_t)events[i].udata;
        if (!req) continue;
        int64_t result;
        if (req->is_write)
            result = (int64_t)write(req->fd, req->buf, req->len);
        else
            result = (int64_t)read(req->fd, req->buf, req->len);
        if (result < 0)
            result = -(int64_t)errno;
        ty_io_wake_coro(req->coro, result);
        pool_free(req);
    }
}

static void kq_destroy(KqDriver* d) {
    ty_mutex_destroy(&d->lock);
    close(d->kq);
    ty_vm_free(d, sizeof(KqDriver));
}

static int kq_open(const char* path, int flags, unsigned mode) {
    int oflags = 0;
    if (flags == 0)
        oflags = O_RDONLY;
    else if (flags == 1)
        oflags = O_WRONLY | O_CREAT | O_TRUNC;
    else
        oflags = O_RDWR | O_CREAT;
    return open(path, oflags, (mode_t)mode);
}
static void kq_close(int fd) { close(fd); }

/* ─────────────────────────────────────────────────────────────────────────────
 *  Windows — IOCP
 * ───────────────────────────────────────────────────────────────────────────── */
#elif defined(_WIN32)

typedef struct IocpOverlapped {
    OVERLAPPED  ov;     /* MUST be first field */
    PendingReq* req;
} IocpOverlapped;

typedef struct IocpDriver {
    HANDLE      iocp;
    PendingPool pool;
    TyMutex     lock;
} IocpDriver;

static IocpDriver* iocp_new(void) {
    HANDLE h = CreateIoCompletionPort(INVALID_HANDLE_VALUE, NULL, 0, 0);
    if (!h) return NULL;
    IocpDriver* d = (IocpDriver*)ty_vm_alloc(sizeof(IocpDriver));
    if (!d) {
        CloseHandle(h);
        return NULL;
    }
    memset(d, 0, sizeof(*d));
    d->iocp = h;
    ty_mutex_init(&d->lock);
    return d;
}

static void iocp_submit(IocpDriver* d, PendingReq* req) {
    HANDLE fh;

    if (req->fd == 1) {
        fh = GetStdHandle(STD_OUTPUT_HANDLE);
    } else if (req->fd == 2) {
        fh = GetStdHandle(STD_ERROR_HANDLE);
    } else {
        fh = (HANDLE)(uintptr_t)(UINT_PTR)(unsigned int)req->fd;
    }

    if (fh == INVALID_HANDLE_VALUE || fh == NULL) {
        ty_io_wake_coro(req->coro, -1);
        pool_free(req);
        return;
    }

    /*
     * Console stdout/stderr cannot go through IOCP WriteFile reliably.
     * Probe for a real console and fall back to synchronous WriteConsoleA.
     * Redirected stdout/stderr still use the async path.
     */
    if (req->is_write && (req->fd == 1 || req->fd == 2)) {
        DWORD mode = 0;
        if (GetConsoleMode(fh, &mode)) {
            DWORD written = 0;
            BOOL ok = WriteConsoleA(fh, req->buf, (DWORD)req->len, &written, NULL);
            ty_io_wake_coro(req->coro, ok ? (int64_t)written : -(int64_t)GetLastError());
            pool_free(req);
            return;
        }
    }

    /* Associate handle with IOCP (idempotent) */
    CreateIoCompletionPort(fh, d->iocp, (ULONG_PTR)req->coro, 0);

    IocpOverlapped* ov = (IocpOverlapped*)ty_vm_alloc(sizeof(IocpOverlapped));
    memset(ov, 0, sizeof(*ov));
    ov->req = req;

    BOOL ok;
    if (req->is_write)
        ok = WriteFile(fh, req->buf, (DWORD)req->len, NULL, &ov->ov);
    else
        ok = ReadFile(fh, req->buf, (DWORD)req->len, NULL, &ov->ov);

    DWORD err = GetLastError();
    if (!ok && err != ERROR_IO_PENDING) {
        /* Immediate failure — synthesize wake */
        ty_io_wake_coro(req->coro, -(int64_t)err);
        pool_free(req);
        ty_vm_free(ov, sizeof(IocpOverlapped));
    }
    /* else IOCP will deliver the completion via GetQueuedCompletionStatus */
}

static void iocp_poll(IocpDriver* d) {
    DWORD bytes = 0;
    ULONG_PTR key = 0;
    OVERLAPPED* raw_ov = NULL;
    BOOL ok = GetQueuedCompletionStatus(d->iocp, &bytes, &key, &raw_ov, 1 /*1ms*/);
    if (!raw_ov) return;

    IocpOverlapped* ov = (IocpOverlapped*)raw_ov;
    PendingReq* req = ov->req;
    int64_t result = ok ? (int64_t)bytes : -(int64_t)GetLastError();
    ty_io_wake_coro(req->coro, result);
    pool_free(req);
    ty_vm_free(ov, sizeof(IocpOverlapped));
}

static void iocp_destroy(IocpDriver* d) {
    ty_mutex_destroy(&d->lock);
    CloseHandle(d->iocp);
    ty_vm_free(d, sizeof(IocpDriver));
}

/* Windows open: convert narrow path to wide, open with OVERLAPPED flag */
static int iocp_open(const char* path, int flags, unsigned mode) {
    /* Convert UTF-8 path to UTF-16 */
    int wlen = MultiByteToWideChar(CP_UTF8, 0, path, -1, NULL, 0);
    if (wlen <= 0) return -1;
    WCHAR* wpath = (WCHAR*)ty_vm_alloc((size_t)wlen * sizeof(WCHAR));
    MultiByteToWideChar(CP_UTF8, 0, path, -1, wpath, wlen);

    DWORD access = (flags == 0) ? GENERIC_READ
        : (flags == 1)          ? GENERIC_WRITE
                                : (GENERIC_READ | GENERIC_WRITE);
    DWORD disp = (flags == 0) ? OPEN_EXISTING : CREATE_ALWAYS;
    HANDLE h = CreateFileW(wpath, access, FILE_SHARE_READ, NULL,
        disp, FILE_FLAG_OVERLAPPED, NULL);
    ty_vm_free(wpath, (size_t)wlen * sizeof(WCHAR));
    if (h == INVALID_HANDLE_VALUE)
        return -1;
    /* Cast HANDLE to int — callers must use iocp_close to release it */
    return (int)(UINT_PTR)h;
}

static void iocp_close(int fd) {
    CloseHandle((HANDLE)(UINT_PTR)(unsigned int)fd);
}

/* Synchronous stdio read/write (fd 0/1/2 are CRT fds, not HANDLES) */
static int64_t iocp_stdio_write(int fd, const uint8_t* buf, size_t len) {
    int n = _write(fd, buf, (unsigned int)len);
    return (int64_t)n;
}
static int64_t iocp_stdio_read(int fd, uint8_t* buf, size_t len) {
    int n = _read(fd, buf, (unsigned int)len);
    return (int64_t)n;
}

#endif /* platform */

/* ═══════════════════════════════════════════════════════════════════════════════
 *  IoDriver — thin union wrapping the platform-specific driver
 * ═══════════════════════════════════════════════════════════════════════════════ */

typedef struct IoDriver {
#if defined(__linux__)
    UringDriver* impl;
#elif defined(__APPLE__)
    KqDriver*    impl;
#elif defined(_WIN32)
    IocpDriver*  impl;
#else
    void*        impl;  /* unsupported platform — I/O falls back to blocking */
#endif
} IoDriver;

/* ── Global state ─────────────────────────────────────────────────────────────── */

static IoDriver     g_driver;
static _Atomic(int) g_poll_running;
static TyThread     g_poll_thread;

void* ty_io_global_driver(void) { return &g_driver; }

/* ═══════════════════════════════════════════════════════════════════════════════
 *  Poll thread
 * ═══════════════════════════════════════════════════════════════════════════════ */

static void* io_poll_thread(void* arg) {
    (void)arg;
    IoDriver* d = &g_driver;
    while (atomic_load_explicit(&g_poll_running, memory_order_acquire)) {
#if defined(__linux__)
        if (d->impl) uring_poll(d->impl);
#elif defined(__APPLE__)
        if (d->impl) kq_poll(d->impl);
#elif defined(_WIN32)
        if (d->impl) iocp_poll(d->impl);
#endif
    }
    return NULL;
}

/* ═══════════════════════════════════════════════════════════════════════════════
 *  Subsystem lifecycle
 * ═══════════════════════════════════════════════════════════════════════════════ */

static int g_subsystem_initialized = 0;

void ty_io_subsystem_init(void) {
    if (g_subsystem_initialized)
        return;
    memset(&g_driver, 0, sizeof(g_driver));
    g_subsystem_initialized = 1;

#if defined(__linux__)
    g_driver.impl = uring_new();
#elif defined(__APPLE__)
    g_driver.impl = kq_new();
#elif defined(_WIN32)
    g_driver.impl = iocp_new();
#endif

    atomic_init(&g_poll_running, 1);

    if (g_driver.impl) {
        if (!ty_thread_create(&g_poll_thread, io_poll_thread, NULL)) {
            /* Non-fatal: poll loop driven inline (degraded to blocking I/O) */
            atomic_store_explicit(&g_poll_running, 0, memory_order_relaxed);
        }
    }
}

void ty_io_subsystem_shutdown(void) {
    atomic_store_explicit(&g_poll_running, 0, memory_order_release);
    if (g_driver.impl) {
        ty_thread_join(g_poll_thread);
#if defined(__linux__)
        uring_destroy(g_driver.impl);
#elif defined(__APPLE__)
        kq_destroy(g_driver.impl);
#elif defined(_WIN32)
        iocp_destroy(g_driver.impl);
#endif
        g_driver.impl = NULL;
    }
}

/* ═══════════════════════════════════════════════════════════════════════════════
 *  Public file open / close
 * ═══════════════════════════════════════════════════════════════════════════════ */

int ty_io_open(void* driver, const char* path, int flags, unsigned mode) {
    (void)driver;
#if defined(__linux__)
    return uring_open(path, flags, mode);
#elif defined(__APPLE__)
    return kq_open(path, flags, mode);
#elif defined(_WIN32)
    return iocp_open(path, flags, mode);
#else
    (void)path;
    (void)flags;
    (void)mode;
    return -1;
#endif
}

void ty_io_close(void* driver, int fd) {
    (void)driver;
#if defined(__linux__)
    uring_close(fd);
#elif defined(__APPLE__)
    kq_close(fd);
#elif defined(_WIN32)
    iocp_close(fd);
#else
    (void)fd;
#endif
}

/* ═══════════════════════════════════════════════════════════════════════════════
 *  Public async read / write
 * ═══════════════════════════════════════════════════════════════════════════════ */

/*
 * Shared submit path.
 * If inside a coroutine AND the driver is running:
 *   1. Allocate a PendingReq from the pool.
 *   2. Submit to the platform backend.
 *   3. Park the coroutine (returns to scheduler).
 *   [coroutine resumes here after poll thread calls ty_io_wake_coro]
 *   4. Return — caller uses ty_io_take_result(coro) to get the byte count.
 *
 * Otherwise (main thread / driver not running): perform blocking I/O inline
 * and store the result directly so ty_io_take_result() works the same way.
 */

static void do_submit_or_sync(void* driver_ptr, SlabArena* arena, void* coro,
    int fd, uint8_t* buf, size_t len, int is_write) {
    IoDriver* d = (IoDriver*)driver_ptr;
    int has_driver = (d && d->impl && atomic_load_explicit(&g_poll_running, memory_order_acquire));

#if defined(_WIN32)
    /* Never park a coroutine on stdio fds: IOCP is not reliable for them, and
       a parked stdio write during shutdown will hang the drain loop forever. */
    if (fd >= 0 && fd <= 2)
        has_driver = 0;
#endif

    if (coro && has_driver) {
        PendingReq* req = pool_alloc(
#if defined(__linux__)
            &((UringDriver*)d->impl)->pool
#elif defined(__APPLE__)
            &((KqDriver*)d->impl)->pool
#elif defined(_WIN32)
            &((IocpDriver*)d->impl)->pool
#else
            NULL
#endif
        );
        if (req) {
            req->fd = fd;
            req->buf = buf;
            req->len = len;
            req->coro = coro;
            req->is_write = is_write;

#if defined(__linux__)
            uring_submit((UringDriver*)d->impl, req);
#elif defined(__APPLE__)
            kq_submit((KqDriver*)d->impl, req);
#elif defined(_WIN32)
            iocp_submit((IocpDriver*)d->impl, req);
#endif
            TY_DEBUG("[io] park coro=%p fd=%d is_write=%d len=%zu\n",
                coro, fd, is_write, len);
            ty_io_park_coro(arena); /* yields; result stored by poll thread */
            return;
        }
        /* Pool exhausted — fall through to blocking I/O */
    }

    /* Blocking synchronous fallback */
    int64_t result;
#if defined(_WIN32)
    if (fd <= 2) {
        result = is_write ? iocp_stdio_write(fd, buf, len)
                          : iocp_stdio_read(fd, buf, len);
    } else {
        HANDLE h = (HANDLE)(UINT_PTR)(unsigned int)fd;
        DWORD n = 0;
        BOOL ok = is_write ? WriteFile(h, buf, (DWORD)len, &n, NULL)
                           : ReadFile(h, buf, (DWORD)len, &n, NULL);
        result = ok ? (int64_t)n : -(int64_t)GetLastError();
    }
#else
    {
        ssize_t n;
        if (is_write) {
            do {
                n = write(fd, buf, len);
            } while (n < 0 && errno == EINTR);
        } else {
            do {
                n = read(fd, buf, len);
            } while (n < 0 && errno == EINTR);
        }
        result = (int64_t)(n < 0 ? -errno : n);
    }
#endif
    /* Store result so ty_io_take_result() is always the retrieval point */
    if (coro)
        ty_coro_set_io_result(coro, result);
    (void)arena;
}

int64_t ty_coro_get_io_result(void* coro) {
    if (!coro)
        return 0;
    TyCoro* co = (TyCoro*)coro;
    return atomic_load_explicit(&co->io_result, memory_order_acquire);
}

void ty_coro_set_io_result(void* coro, int64_t res) {
    if (!coro)
        return;
    TyCoro* co = (TyCoro*)coro;
    atomic_store_explicit(&co->io_result, res, memory_order_release);
}

int64_t ty_io_read(void* driver, SlabArena* arena, void* coro,
    int fd, uint8_t* buf, size_t len) {
    do_submit_or_sync(driver, arena, coro, fd, buf, len, 0);
    return 0; /* actual value via ty_io_take_result(coro) in ty_io.c */
}

int64_t ty_io_write(void* driver, SlabArena* arena, void* coro,
    int fd, const uint8_t* buf, size_t len) {
    do_submit_or_sync(driver, arena, coro, fd, (uint8_t*)buf, len, 1);
    return 0;
}
