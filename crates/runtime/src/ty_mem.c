/*
 * ty_mem.c — Typhoon memory system
 *
 * Memory layout
 * ─────────────
 *   SlabArena  (TyTask)
 *   ├── bump pages  : linked list of mmap'd pages, bump_ptr walks forward
 *   ├── free lists  : NUM_SIZE_CLASSES singly-linked lists of recycled slots
 *   └── oversized   : linked list of large mmap pages, released at arena_free
 *
 * Size classes (index → max object bytes)
 *   0 →   8      4 →  128
 *   1 →  16      5 →  256
 *   2 →  32      6 →  512
 *   3 →  64      7 → 1024
 *   ≥8 → oversized: dedicated mmap page tracked for bulk-release
 *
 * Buf / TyArray use arena_alloc / arena_realloc / arena_free_slot exclusively.
 * No malloc / realloc / free anywhere in this file.
 */

#include <stdint.h>
#include <stddef.h>
#include <stdlib.h>
#include <string.h>
#include <stdio.h>
#include "platform.h"
#include "ty_mem.h"

#ifndef _MSC_VER
#include <unistd.h>
#endif

/* ── configuration ────────────────────────────────────────────────────────────
 */

#define ARENA_PAGE_SIZE  (4 * 1024 * 1024) /* 4 MiB per bump page */
#define NUM_SIZE_CLASSES 8
#define LARGE_THRESHOLD  1024              /* bytes; above → oversized         */

static const int32_t SIZE_CLASS_BYTES[NUM_SIZE_CLASSES] = {
    8, 16, 32, 64, 128, 256, 512, 1024
};

/* ── generation tagging (header cookie) ───────────────────────────────────────
 *
 * Each allocation gets a SlotCookie immediately before the user pointer.
 * The cookie stores the arena's generation at allocation time.
 * On task death, arena->generation increments. Any stale pointer from
 * outside the dead arena will have a mismatched cookie and abort on access.
 */
typedef struct SlotCookie {
    uint64_t generation;
} SlotCookie;

#define COOKIE_SIZE  sizeof(SlotCookie)
#define COOKIE_ALIGN 8

static inline SlotCookie* cookie_of(void* user_ptr) {
    return (SlotCookie*)((uint8_t*)user_ptr - COOKIE_SIZE);
}

static inline void* user_of(SlotCookie* cookie) {
    return (void*)((uint8_t*)cookie + COOKIE_SIZE);
}

/* ── hard abort ───────────────────────────────────────────────────────────────
 */

static void ty_abort(void) { TY_TRAP(); }

/* ── helpers ──────────────────────────────────────────────────────────────────
 */

static inline uint8_t* align_up(uint8_t* ptr, size_t align) {
    uintptr_t p = (uintptr_t)ptr;
    uintptr_t a = (uintptr_t)align;
    return (uint8_t*)((p + a - 1) & ~(a - 1));
}

int32_t size_to_class(size_t size) {
    for (int32_t i = 0; i < NUM_SIZE_CLASSES; i++)
        if (size <= (size_t)SIZE_CLASS_BYTES[i]) return i;
    return NUM_SIZE_CLASSES; /* oversized */
}

/* ── virtual memory ───────────────────────────────────────────────────────────
 */

static void* vm_reserve(size_t size) {
#ifdef _WIN32
    void* p = VirtualAlloc(NULL, size, MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE);
    if (!p) ty_abort();
    return p;
#else
    void* p = mmap(NULL, size, PROT_READ | PROT_WRITE,
        MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (p == MAP_FAILED) ty_abort();
    return p;
#endif
}

static void vm_release(void* ptr, size_t size) {
#ifdef _WIN32
    (void)size;
    VirtualFree(ptr, 0, MEM_RELEASE);
#else
    munmap(ptr, size);
#endif
}

/* ── bump page ────────────────────────────────────────────────────────────────
 */

typedef struct BumpPage {
    struct BumpPage* next;
    uint8_t*         bump_ptr;
    uint8_t*         end;
    /* mmap data follows immediately after this header */
} BumpPage;

static BumpPage* bump_page_new(size_t capacity) {
    size_t total = sizeof(BumpPage) + capacity;
    BumpPage* pg = (BumpPage*)vm_reserve(total);
    pg->next     = NULL;
    pg->bump_ptr = (uint8_t*)(pg + 1);
    pg->end      = pg->bump_ptr + capacity;
    return pg;
}

/* ── oversized block header ───────────────────────────────────────────────────
 */

typedef struct OversizedHdr {
    struct OversizedHdr* next;
    size_t               total_size;
} OversizedHdr;

/* ── free-list node ───────────────────────────────────────────────────────────
 */

typedef struct FreeNode {
    struct FreeNode* next;
} FreeNode;

/* ── arena ────────────────────────────────────────────────────────────────────
 */

typedef struct SlabArena {
    BumpPage*     current_page;
    OversizedHdr* oversized;
    FreeNode*     free_lists[NUM_SIZE_CLASSES];
    uint64_t      generation;
} SlabArena;

/* ── forward declarations ─────────────────────────────────────────────────────
 */

static void* arena_alloc(SlabArena* arena, size_t size, size_t align);
static void arena_free_slot(SlabArena* arena, void* ptr, size_t size);
static void* arena_realloc(SlabArena* arena, void* ptr,
                           size_t old_size, size_t new_size, size_t align);

/* ── arena lifecycle ──────────────────────────────────────────────────────────
 */

/*
 * slab_arena_new — called once by main(); the returned pointer is the
 * "task" token threaded through every compiled function as the hidden
 * first i8* argument.
 *
 * The SlabArena header is carved from the start of the first bump page
 * so no allocation outside the arena is ever needed.
 */
SlabArena* slab_arena_new(void) {
    size_t cap = ARENA_PAGE_SIZE - sizeof(BumpPage);
    BumpPage* pg = bump_page_new(cap);
    uint8_t* ptr = align_up(pg->bump_ptr, 16);
    SlabArena* arena = (SlabArena*)ptr;
    pg->bump_ptr = ptr + sizeof(SlabArena);

    arena->current_page = pg;
    arena->oversized = NULL;
    arena->generation = 1; /* start at 1 so 0 = uninitialized / freed */
    for (int i = 0; i < NUM_SIZE_CLASSES; i++)
        arena->free_lists[i] = NULL;
    return arena;
}

/*
 * slab_arena_free — release the entire arena in two passes:
 *   1. all dedicated oversized pages
 *   2. all bump pages (the first page holds the SlabArena header)
 * Individual slab_free calls before this are optional.
 *
 * Increments generation so any stale external pointers fail cookie check.
 */
void slab_arena_free(SlabArena* arena) {
    if (!arena) return;

    /* Bump generation FIRST — any live external pointer with old cookie
     * will now fail validation on next access. */
    arena->generation++;

    OversizedHdr* oh = arena->oversized;
    while (oh) {
        OversizedHdr* next = oh->next;
        size_t sz = oh->total_size;
        vm_release(oh, sz);
        oh = next;
    }

    BumpPage* pg = arena->current_page;
    while (pg) {
        BumpPage* prev = pg->next;
        size_t sz = (size_t)(pg->end - (uint8_t*)pg);
        vm_release(pg, sz);
        pg = prev;
    }
}

/* ── internal allocator ───────────────────────────────────────────────────────
 */

static void* arena_bump_raw(SlabArena* arena, size_t size, size_t align) {
    BumpPage* pg = arena->current_page;
    /* Account for cookie: align the cookie, then user pointer follows */
    size_t cookie_off = align_up(pg->bump_ptr, COOKIE_ALIGN) - pg->bump_ptr;
    uint8_t* cookie_ptr = pg->bump_ptr + cookie_off;
    uint8_t* user_ptr = cookie_ptr + COOKIE_SIZE;
    user_ptr = align_up(user_ptr, align);

    if (user_ptr + size <= pg->end) {
        SlotCookie* cookie = (SlotCookie*)cookie_ptr;
        cookie->generation = arena->generation;
        pg->bump_ptr = user_ptr + size;
        return user_ptr;
    }

    /* page full — chain a new one */
    size_t new_cap = ARENA_PAGE_SIZE - sizeof(BumpPage);
    if (size + align + COOKIE_SIZE + COOKIE_ALIGN > new_cap)
        new_cap = size + align + COOKIE_SIZE + COOKIE_ALIGN;

    BumpPage* np = bump_page_new(new_cap);
    np->next = arena->current_page;
    arena->current_page = np;

    cookie_off = align_up(np->bump_ptr, COOKIE_ALIGN) - np->bump_ptr;
    cookie_ptr = np->bump_ptr + cookie_off;
    user_ptr = cookie_ptr + COOKIE_SIZE;
    user_ptr = align_up(user_ptr, align);

    SlotCookie* cookie = (SlotCookie*)cookie_ptr;
    cookie->generation = arena->generation;
    np->bump_ptr = user_ptr + size;
    return user_ptr;
}

static void* arena_alloc(SlabArena* arena, size_t size, size_t align) {
    if (!arena) ty_abort();
    if (size == 0) size = 1;
    if (align == 0) align = 1;

    void* ptr = NULL;
    int32_t cls = size_to_class(size);

    if (cls < NUM_SIZE_CLASSES) {
        FreeNode* head = arena->free_lists[cls];
        if (head) {
            arena->free_lists[cls] = head->next;
            ptr = (void*)head;
            /* Free-list slot already has a valid cookie from original allocation.
             * Just verify it matches current generation (should always match for
             * same-arena allocations). */
        } else {
            size_t slot = (size_t)SIZE_CLASS_BYTES[cls];
            size_t al = align < 8 ? 8 : align;
            ptr = arena_bump_raw(arena, slot, al);
        }
        if (ptr)
            memset(ptr, 0, (cls < NUM_SIZE_CLASSES) ? (size_t)SIZE_CLASS_BYTES[cls] : size);
    } else {
        /* oversized: dedicated mmap page with cookie */
        size_t total = sizeof(OversizedHdr) + COOKIE_SIZE + size;
        size_t pgsz = (total + 4095) & ~(size_t)4095;
        OversizedHdr* hdr = (OversizedHdr*)vm_reserve(pgsz);
        hdr->total_size = pgsz;
        hdr->next = arena->oversized;
        arena->oversized = hdr;
        /* Place cookie after OversizedHdr, before user data */
        SlotCookie* cookie = (SlotCookie*)((uint8_t*)(hdr + 1));
        cookie->generation = arena->generation;
        ptr = user_of(cookie);
    }

    return ptr;
}

static void arena_free_slot(SlabArena* arena, void* ptr, size_t size) {
    if (!arena || !ptr) return;
    int32_t cls = size_to_class(size);
    if (cls >= NUM_SIZE_CLASSES) return; /* oversized — released at arena_free */
    /* Verify cookie before recycling */
    SlotCookie* cookie = cookie_of(ptr);
    if (cookie->generation != arena->generation) {
        ty_abort(); /* stale pointer freed into wrong generation */
    }
    FreeNode* node = (FreeNode*)ptr;
    node->next = arena->free_lists[cls];
    arena->free_lists[cls] = node;
}

static void* arena_realloc(SlabArena* arena, void* ptr, size_t old_size,
    size_t new_size, size_t align) {
    if (!ptr) return arena_alloc(arena, new_size, align);
    if (!new_size) {
        arena_free_slot(arena, ptr, old_size);
        return NULL;
    }

    void* fresh = arena_alloc(arena, new_size, align);
    size_t copy = old_size < new_size ? old_size : new_size;
    memcpy(fresh, ptr, copy);
    arena_free_slot(arena, ptr, old_size);
    return fresh;
}

/* ── public slab API (called from emitted LLVM IR) ────────────────────────────
 */

void* slab_alloc(SlabArena* arena, int32_t size_class) {
    if (!arena) ty_abort();
    if (size_class < 0 || size_class >= NUM_SIZE_CLASSES) return arena_alloc(arena, (size_t)LARGE_THRESHOLD * 2, 8);

    FreeNode* head = arena->free_lists[size_class];
    if (head) {
        arena->free_lists[size_class] = head->next;
        memset(head, 0, (size_t)SIZE_CLASS_BYTES[size_class]);
        return (void*)head;
    }
    void* p = arena_bump_raw(arena, (size_t)SIZE_CLASS_BYTES[size_class], 16);
    if (!p) {
        // Try to grow the arena or allocate a new chunk here
        TY_DEBUG("DEBUG: Arena bump failed. Current page: %p\n", (void*)arena->current_page);
    }
    if (p)
        memset(p, 0, (size_t)SIZE_CLASS_BYTES[size_class]);
    if (p == NULL) {
        TY_DEBUG("FATAL: slab_alloc returned NULL\n");
    }
    return p;
}

void slab_free(SlabArena* arena, void* ptr, int32_t size_class) {
    if (!arena || !ptr) return;
    if (size_class < 0 || size_class >= NUM_SIZE_CLASSES) return;
    /* Verify cookie before recycling */
    SlotCookie* cookie = cookie_of(ptr);
    if (cookie->generation != arena->generation) {
        ty_abort(); /* stale pointer freed into wrong generation */
    }
    FreeNode* node = (FreeNode*)ptr;
    node->next = arena->free_lists[size_class];
    arena->free_lists[size_class] = node;
}

/* ── generation tag validation (for debug / compiler-inserted checks) ──────────
 *
 * Returns 1 if ptr's cookie matches arena's current generation, 0 otherwise.
 * Safe to call with any pointer (NULL returns 0).
 */
int slab_verify_generation(SlabArena* arena, void* ptr) {
    if (!arena || !ptr) return 0;
    SlotCookie* cookie = cookie_of(ptr);
    return cookie->generation == arena->generation;
}

/*
 * slab_alloc_sized — like slab_alloc, but takes a real byte size instead
 * of a size_class index.
 *
 * slab_alloc(arena, size_class) is lossy above 1024 bytes: size_to_class()
 * collapses every size >1024 into a single sentinel class (8), and
 * slab_alloc has no way to recover the real size from that — it silently
 * allocates a fixed 2048 bytes regardless of what was actually needed.
 * Any caller requesting >2048 bytes via slab_alloc(arena,
 * size_to_class(n)) under-allocates and the caller's subsequent write
 * overflows the slot. This bit ty_net.c's 4096-byte socket read buffer
 * (2048 allocated, 4096-byte recv() told to fill it) and is a latent bug
 * anywhere else that pattern is used for a dynamic or >2048-byte size.
 *
 * Use this instead whenever the requested size isn't a small
 * compile-time-known constant guaranteed to fit a size class.
 */
void* slab_alloc_sized(SlabArena* arena, int64_t size) {
    if (!arena) ty_abort();
    if (size <= 0) size = 1;
    return arena_alloc(arena, (size_t)size, 8);
}

/* ── Buf
 * ─────────────────────────────────────────────────────────────────────── */

static void ty_buf_grow(SlabArena* arena, Buf* b, int64_t extra) {
    if (!b) return;
    int64_t need = b->len + extra + 1;
    if (need <= b->cap) return;

    int64_t new_cap = b->cap ? b->cap : 64;
    while (new_cap < need) new_cap *= 2;

    b->data = (char*)arena_realloc(arena, b->data, (size_t)b->cap, (size_t)new_cap, 1);
    b->cap = new_cap;
}

Buf* ty_buf_new(SlabArena* arena) {
    Buf* b = (Buf*)arena_alloc(arena, sizeof(Buf), 8);
    b->len = 0;
    b->cap = 64;
    b->data = (char*)arena_alloc(arena, 64, 1);
    b->data[0] = '\0';
    b->heap_owned = 0;
    return b;
}

/*
 * ty_buf_new_sized — like ty_buf_new, but pre-allocates `cap` bytes
 * instead of the fixed 64-byte default and growing from there.
 *
 * Added for socket read-into-chan: reading a chunk_size chunk via
 * ty_buf_new + repeated pushes means doubling through 64→128→...→4096+
 * (several reallocs + copies) for a single read. This lets the caller
 * size the buffer once and read the syscall result directly into
 * b->data — one allocation, one copy (kernel into buffer), nothing else.
 * Caller is responsible for setting b->len (and the trailing '\0', at
 * b->data[b->len]) after filling b->data directly.
 *
 * NOTE: arena-allocated, single-owner only — see ty_buf_new_heap below
 * for the version safe to allocate on one coroutine and consume on
 * another (e.g. across a channel).
 */
Buf* ty_buf_new_sized(SlabArena* arena, int64_t cap) {
    if (cap < 0) cap = 0;
    Buf* b = (Buf*)arena_alloc(arena, sizeof(Buf), 8);
    b->len = 0;
    b->cap = cap;
    b->data = (char*)arena_alloc(arena, (size_t)cap + 1, 1);
    b->data[0] = '\0';
    b->heap_owned = 0;
    return b;
}

/*
 * ty_buf_new_heap — same shape as ty_buf_new_sized, but backed by plain
 * malloc instead of any SlabArena. SlabArena's bump/free-list allocator
 * has no locking (by design — it's meant to be single-coroutine-owned,
 * which is what makes it cheap), so it isn't safe for a Buf that's
 * produced by one coroutine and consumed by a different one, since the
 * M:N scheduler can genuinely run those two coroutines on different OS
 * worker threads concurrently. malloc/free are thread-safe on every
 * platform this runtime targets, at the cost of losing the arena's bump
 * allocation speed — an acceptable trade specifically for data crossing
 * a coroutine boundary (e.g. ReadSocket.into_chan's chan<Buf>), which
 * doesn't benefit from arena locality anyway since the two ends don't
 * share an arena.
 */
Buf* ty_buf_new_heap(int64_t cap) {
    if (cap < 0) cap = 0;
    Buf* b = (Buf*)malloc(sizeof(Buf));
    if (!b) return NULL;
    b->data = (char*)malloc((size_t)cap + 1);
    if (!b->data) { free(b); return NULL; }
    b->len = 0;
    b->cap = cap;
    b->heap_owned = 1;
    b->data[0] = '\0';
    return b;
}

void ty_buf_push_str(SlabArena* arena, Buf* b, TyStr* s) {
    if (!b || !s) return;
    ty_buf_grow(arena, b, (int64_t)s->len);
    memcpy(b->data + b->len, s->ptr, (size_t)s->len);
    b->len += (int64_t)s->len;
    b->data[b->len] = '\0';
}

void ty_buf_push_byte(SlabArena* arena, Buf* b, char c) {
    if (!b) return;
    ty_buf_grow(arena, b, 1);
    b->data[b->len] = c;
    b->len += 1;
    b->data[b->len] = '\0';
}

/*
 * ty_buf_into_str — wraps the Buf's data in a TyStr fat pointer instead
 * of handing back a bare char*. Data pointer transfers as before (no
 * copy); the Buf header slot is recycled. Class 1 (16 bytes) for the
 * TyStr wrapper itself, matching codegen's own literal-Str allocation
 * (see emit_string in codegen.rs) so both sides agree on layout AND
 * allocation size class.
 */
TyStr* ty_buf_into_str(SlabArena* arena, Buf* b) {
    if (!b) return NULL;

    if (b->heap_owned) {
        /* Cross-coroutine chunk (see ty_buf_new_heap) — not part of any
         * SlabArena. Recycling it through arena_free_slot(arena, ...)
         * here would push this malloc'd pointer onto the CALLING
         * coroutine's own arena free-list, and a later arena_alloc of
         * the same size class would hand it back out as if it were
         * arena memory — corrupting that arena. Wrap and release with
         * plain malloc/free instead; `arena` is unused on this path. */
        TyStr* s = (TyStr*)malloc(sizeof(TyStr));
        if (!s) return NULL;
        s->ptr = b->data; /* ownership of the data buffer transfers to s */
        s->len = (int32_t)b->len;
        free(b); /* just the header — data lives on via s->ptr */
        return s;
    }

    TyStr* s = (TyStr*)slab_alloc(arena, 1);
    s->ptr = b->data;
    s->len = (int32_t)b->len;
    arena_free_slot(arena, b, sizeof(Buf));
    return s;
}

/* See ty_mem.h — only ever call this on a TyStr that came from
 * ty_buf_into_str() on a heap_owned Buf. */
void ty_str_free_heap(TyStr* s) {
    if (!s) return;
    free(s->ptr);
    free(s);
}

int64_t ty_str_len(TyStr* s) {
    if (!s) return 0;
    return (int64_t)s->len;
}

char ty_str_byte(TyStr* s, int64_t idx) {
    if (!s || idx < 0 || idx >= (int64_t)s->len) return 0;
    return s->ptr[(size_t)idx];
}

/* ── String helpers
 * ─────────────────────────────────────────────────────────────────── */

TyArray* ty_array_from_fixed(SlabArena* arena, void* data, int64_t len,
    int64_t elem_size, int64_t elem_align) {
    if (len < 0) ty_abort();
    if (elem_size <= 0) ty_abort();

    TyArray* arr = (TyArray*)arena_alloc(arena, sizeof(TyArray), 8);
    arr->len = len;
    arr->cap = len;
    arr->elem_size = elem_size;
    arr->elem_align = elem_align;

    if (len == 0) {
        arr->data = NULL;
        return arr;
    }

    size_t bytes = (size_t)(len * elem_size);
    arr->data = arena_alloc(arena, bytes, (size_t)elem_align);
    memcpy(arr->data, data, bytes);
    return arr;
}

void* ty_array_get_ptr(TyArray* arr, int64_t idx) {
    if (!arr) return NULL;
    if (idx < 0 || idx >= arr->len) return NULL;
    if (!arr->data) return NULL;
    return (void*)((uint8_t*)arr->data + (size_t)(idx * arr->elem_size));
}

void ty_array_push(SlabArena* arena, TyArray* arr, void* elem_bytes) {
    if (!arr || arr->elem_size <= 0) ty_abort();

    if (arr->len == arr->cap) {
        int64_t new_cap = arr->cap ? arr->cap * 2 : 8;
        size_t old_bytes = (size_t)(arr->cap * arr->elem_size);
        size_t new_bytes = (size_t)(new_cap * arr->elem_size);

        arr->data = arena_realloc(arena, arr->data, old_bytes, new_bytes,
            (size_t)arr->elem_align);
        arr->cap = new_cap;
    }

    memcpy((uint8_t*)arr->data + (size_t)(arr->len * arr->elem_size), elem_bytes,
        (size_t)arr->elem_size);
    arr->len++;
}
