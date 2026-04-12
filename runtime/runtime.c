/*
 * runtime.c — Typhoon language runtime
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
#include <string.h>
#include "platform.h"

/* ── configuration ──────────────────────────────────────────────────────────── */

#define ARENA_PAGE_SIZE  (4 * 1024 * 1024)   /* 4 MiB per bump page             */
#define NUM_SIZE_CLASSES  8
#define LARGE_THRESHOLD   1024               /* bytes; above → oversized         */

static const int32_t SIZE_CLASS_BYTES[NUM_SIZE_CLASSES] = {
    8, 16, 32, 64, 128, 256, 512, 1024
};

/* ── hard abort ─────────────────────────────────────────────────────────────── */

static void ty_abort(void) { TY_TRAP(); }

/* ── helpers ────────────────────────────────────────────────────────────────── */

static inline uint8_t* align_up(uint8_t* ptr, size_t align) {
    uintptr_t p = (uintptr_t)ptr;
    uintptr_t a = (uintptr_t)align;
    return (uint8_t*)((p + a - 1) & ~(a - 1));
}

static inline int32_t size_to_class(size_t size) {
    for (int32_t i = 0; i < NUM_SIZE_CLASSES; i++)
        if (size <= (size_t)SIZE_CLASS_BYTES[i]) return i;
    return NUM_SIZE_CLASSES; /* oversized */
}

/* ── virtual memory ─────────────────────────────────────────────────────────── */

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

/* ── bump page ──────────────────────────────────────────────────────────────── */

typedef struct BumpPage {
    struct BumpPage* next;
    uint8_t*         bump_ptr;
    uint8_t*         end;
    /* mmap data follows immediately after this header */
} BumpPage;

static BumpPage* bump_page_new(size_t capacity) {
    size_t    total = sizeof(BumpPage) + capacity;
    BumpPage* pg    = (BumpPage*)vm_reserve(total);
    pg->next        = NULL;
    pg->bump_ptr    = (uint8_t*)(pg + 1);
    pg->end         = pg->bump_ptr + capacity;
    return pg;
}

/* ── oversized block header ─────────────────────────────────────────────────── */

typedef struct OversizedHdr {
    struct OversizedHdr* next;
    size_t               total_size;
} OversizedHdr;

/* ── free-list node ─────────────────────────────────────────────────────────── */

typedef struct FreeNode {
    struct FreeNode* next;
} FreeNode;

/* ── arena ──────────────────────────────────────────────────────────────────── */

typedef struct SlabArena {
    BumpPage*     current_page;
    OversizedHdr* oversized;
    FreeNode*     free_lists[NUM_SIZE_CLASSES];
} SlabArena;

/* ── forward declarations ───────────────────────────────────────────────────── */

static void* arena_alloc(SlabArena* arena, size_t size, size_t align);
static void  arena_free_slot(SlabArena* arena, void* ptr, size_t size);
static void* arena_realloc(SlabArena* arena, void* ptr,
                            size_t old_size, size_t new_size, size_t align);

/* ── arena lifecycle ────────────────────────────────────────────────────────── */

/*
 * slab_arena_new — called once by main(); the returned pointer is the
 * "task" token threaded through every compiled function as the hidden
 * first i8* argument.
 *
 * The SlabArena header is carved from the start of the first bump page
 * so no allocation outside the arena is ever needed.
 */
SlabArena* slab_arena_new(void) {
    size_t    cap = ARENA_PAGE_SIZE - sizeof(BumpPage);
    BumpPage* pg  = bump_page_new(cap);

    SlabArena* arena = (SlabArena*)pg->bump_ptr;
    pg->bump_ptr    += sizeof(SlabArena);

    arena->current_page = pg;
    arena->oversized    = NULL;
    for (int i = 0; i < NUM_SIZE_CLASSES; i++)
        arena->free_lists[i] = NULL;
    return arena;
}

/*
 * slab_arena_free — release the entire arena in two passes:
 *   1. all dedicated oversized pages
 *   2. all bump pages (the first page holds the SlabArena header)
 * Individual slab_free calls before this are optional.
 */
void slab_arena_free(SlabArena* arena) {
    if (!arena) return;

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
        size_t    sz   = (size_t)(pg->end - (uint8_t*)pg);
        vm_release(pg, sz);
        pg = prev;
    }
}

/* ── internal allocator ─────────────────────────────────────────────────────── */

static void* arena_bump_raw(SlabArena* arena, size_t size, size_t align) {
    BumpPage* pg      = arena->current_page;
    uint8_t*  aligned = align_up(pg->bump_ptr, align);

    if (aligned + size <= pg->end) {
        pg->bump_ptr = aligned + size;
        return aligned;
    }

    /* page full — chain a new one */
    size_t new_cap = ARENA_PAGE_SIZE - sizeof(BumpPage);
    if (size + align > new_cap) new_cap = size + align;

    BumpPage* np        = bump_page_new(new_cap);
    np->next            = arena->current_page;
    arena->current_page = np;

    aligned       = align_up(np->bump_ptr, align);
    np->bump_ptr  = aligned + size;
    return aligned;
}

static void* arena_alloc(SlabArena* arena, size_t size, size_t align) {
    if (!arena) ty_abort();
    if (size  == 0) size  = 1;
    if (align == 0) align = 1;

    int32_t cls = size_to_class(size);

    if (cls < NUM_SIZE_CLASSES) {
        FreeNode* head = arena->free_lists[cls];
        if (head) {
            arena->free_lists[cls] = head->next;
            return (void*)head;
        }
        size_t slot = (size_t)SIZE_CLASS_BYTES[cls];
        size_t al   = align < 8 ? 8 : align;
        return arena_bump_raw(arena, slot, al);
    }

    /* oversized: dedicated mmap page */
    size_t total  = sizeof(OversizedHdr) + size;
    size_t pgsz   = (total + 4095) & ~(size_t)4095;
    OversizedHdr* hdr = (OversizedHdr*)vm_reserve(pgsz);
    hdr->total_size   = pgsz;
    hdr->next         = arena->oversized;
    arena->oversized  = hdr;
    return (void*)(hdr + 1);
}

static void arena_free_slot(SlabArena* arena, void* ptr, size_t size) {
    if (!arena || !ptr) return;
    int32_t cls = size_to_class(size);
    if (cls >= NUM_SIZE_CLASSES) return; /* oversized — released at arena_free */
    FreeNode* node         = (FreeNode*)ptr;
    node->next             = arena->free_lists[cls];
    arena->free_lists[cls] = node;
}

static void* arena_realloc(SlabArena* arena, void* ptr,
                            size_t old_size, size_t new_size, size_t align) {
    if (!ptr)      return arena_alloc(arena, new_size, align);
    if (!new_size) { arena_free_slot(arena, ptr, old_size); return NULL; }

    void*  fresh = arena_alloc(arena, new_size, align);
    size_t copy  = old_size < new_size ? old_size : new_size;
    memcpy(fresh, ptr, copy);
    arena_free_slot(arena, ptr, old_size);
    return fresh;
}

/* ── public slab API (called from emitted LLVM IR) ──────────────────────────── */

void* slab_alloc(SlabArena* arena, int32_t size_class) {
    if (!arena) ty_abort();
    if (size_class < 0 || size_class >= NUM_SIZE_CLASSES)
        return arena_alloc(arena, (size_t)LARGE_THRESHOLD * 2, 8);

    FreeNode* head = arena->free_lists[size_class];
    if (head) {
        arena->free_lists[size_class] = head->next;
        return (void*)head;
    }
    return arena_bump_raw(arena, (size_t)SIZE_CLASS_BYTES[size_class], 8);
}

void slab_free(SlabArena* arena, void* ptr, int32_t size_class) {
    if (!arena || !ptr) return;
    if (size_class < 0 || size_class >= NUM_SIZE_CLASSES) return;
    FreeNode* node         = (FreeNode*)ptr;
    node->next             = arena->free_lists[size_class];
    arena->free_lists[size_class] = node;
}

/* ── Buf ─────────────────────────────────────────────────────────────────────── */

typedef struct Buf {
    char*   data;
    int64_t len;
    int64_t cap;
} Buf;

static void ty_buf_grow(SlabArena* arena, Buf* b, int64_t extra) {
    if (!b) return;
    int64_t need = b->len + extra + 1;
    if (need <= b->cap) return;

    int64_t new_cap = b->cap ? b->cap : 64;
    while (new_cap < need) new_cap *= 2;

    b->data = (char*)arena_realloc(arena,
                                   b->data,
                                   (size_t)b->cap,
                                   (size_t)new_cap,
                                   1);
    b->cap = new_cap;
}

Buf* ty_buf_new(SlabArena* arena) {
    Buf* b  = (Buf*)arena_alloc(arena, sizeof(Buf), 8);
    b->len  = 0;
    b->cap  = 64;
    b->data = (char*)arena_alloc(arena, 64, 1);
    b->data[0] = '\0';
    return b;
}

void ty_buf_push_str(SlabArena* arena, Buf* b, char* s) {
    if (!b || !s) return;
    size_t n = strlen(s);
    ty_buf_grow(arena, b, (int64_t)n);
    memcpy(b->data + b->len, s, n);
    b->len += (int64_t)n;
    b->data[b->len] = '\0';
}

/*
 * ty_buf_into_str — transfers data pointer to the caller.
 * The Buf header slot is recycled; the data lives until slab_arena_free.
 */
char* ty_buf_into_str(SlabArena* arena, Buf* b) {
    if (!b) return NULL;
    char* out = b->data;
    arena_free_slot(arena, b, sizeof(Buf));
    return out;
}

/* ── TyArray ─────────────────────────────────────────────────────────────────── */

typedef struct TyArray {
    void*   data;
    int64_t len;
    int64_t cap;
    int64_t elem_size;
    int64_t elem_align;
} TyArray;

TyArray* ty_array_from_fixed(SlabArena* arena, void* data,
                              int64_t len, int64_t elem_size, int64_t elem_align) {
    if (len < 0)        ty_abort();
    if (elem_size <= 0) ty_abort();

    TyArray* arr    = (TyArray*)arena_alloc(arena, sizeof(TyArray), 8);
    arr->len        = len;
    arr->cap        = len;
    arr->elem_size  = elem_size;
    arr->elem_align = elem_align;

    if (len == 0) { arr->data = NULL; return arr; }

    size_t bytes = (size_t)(len * elem_size);
    arr->data    = arena_alloc(arena, bytes, (size_t)elem_align);
    memcpy(arr->data, data, bytes);
    return arr;
}

void* ty_array_get_ptr(TyArray* arr, int64_t idx) {
    if (!arr)                       return NULL;
    if (idx < 0 || idx >= arr->len) return NULL;
    if (!arr->data)                 return NULL;
    return (void*)((uint8_t*)arr->data + (size_t)(idx * arr->elem_size));
}

void ty_array_push(SlabArena* arena, TyArray* arr, void* elem_bytes) {
    if (!arr || arr->elem_size <= 0) ty_abort();

    if (arr->len == arr->cap) {
        int64_t new_cap   = arr->cap ? arr->cap * 2 : 8;
        size_t  old_bytes = (size_t)(arr->cap * arr->elem_size);
        size_t  new_bytes = (size_t)(new_cap  * arr->elem_size);

        arr->data = arena_realloc(arena,
                                  arr->data,
                                  old_bytes,
                                  new_bytes,
                                  (size_t)arr->elem_align);
        arr->cap = new_cap;
    }

    memcpy((uint8_t*)arr->data + (size_t)(arr->len * arr->elem_size),
           elem_bytes, (size_t)arr->elem_size);
    arr->len++;
}
