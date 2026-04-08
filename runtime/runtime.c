#include <stdint.h>
#include <stdlib.h>
#include <string.h>

void* ty_alloc(int64_t size, int64_t align) {
  (void)align;
  if (size <= 0) size = 1;
  void* p = malloc((size_t)size);
  if (!p) abort();
  return p;
}

void* ty_realloc(void* ptr, int64_t new_size, int64_t align) {
  (void)align;
  if (new_size <= 0) new_size = 1;
  void* p = realloc(ptr, (size_t)new_size);
  if (!p) abort();
  return p;
}

void ty_free(void* ptr) { free(ptr); }

typedef struct Buf {
  char* data;
  int64_t len;
  int64_t cap;
} Buf;

static void ty_buf_grow(Buf* b, int64_t extra) {
  if (!b) return;
  int64_t need = b->len + extra + 1;
  if (need <= b->cap) return;
  int64_t new_cap = b->cap ? b->cap : 64;
  while (new_cap < need) new_cap *= 2;
  char* n = (char*)ty_realloc(b->data, new_cap, 0);
  if (!n) abort();
  b->data = n;
  b->cap = new_cap;
}

Buf* ty_buf_new(void) {
  Buf* b = (Buf*)ty_alloc((int64_t)sizeof(Buf), 0);
  memset(b, 0, sizeof(Buf));
  b->cap = 64;
  b->data = (char*)ty_alloc(b->cap, 0);
  if (!b->data) abort();
  b->len = 0;
  b->data[0] = '\0';
  return b;
}

void ty_buf_push_str(Buf* b, char* s) {
  if (!b || !s) return;
  size_t n = strlen(s);
  ty_buf_grow(b, (int64_t)n);
  memcpy(b->data + b->len, s, n);
  b->len += (int64_t)n;
  b->data[b->len] = '\0';
}

char* ty_buf_into_str(Buf* b) {
  if (!b) return NULL;
  char* out = b->data;
  ty_free(b);
  return out;
}

typedef struct TyArray {
  void* data;
  int64_t len;
  int64_t cap;
  int64_t elem_size;
  int64_t elem_align;
} TyArray;

TyArray* ty_array_from_fixed(void* data, int64_t len, int64_t elem_size, int64_t elem_align) {
  if (len < 0) abort();
  if (elem_size <= 0) abort();
  TyArray* arr = (TyArray*)ty_alloc((int64_t)sizeof(TyArray), 0);
  arr->len = len;
  arr->cap = len;
  arr->elem_size = elem_size;
  arr->elem_align = elem_align;
  if (len == 0) {
    arr->data = NULL;
    return arr;
  }
  int64_t bytes = len * elem_size;
  void* out = ty_alloc(bytes, elem_align);
  memcpy(out, data, (size_t)bytes);
  arr->data = out;
  return arr;
}

void* ty_array_get_ptr(TyArray* arr, int64_t idx) {
  if (!arr) return NULL;
  if (idx < 0 || idx >= arr->len) return NULL;
  if (!arr->data) return NULL;
  uint8_t* base = (uint8_t*)arr->data;
  return (void*)(base + (idx * arr->elem_size));
}

void ty_array_push(TyArray* arr, void* elem_bytes) {
  if (!arr) abort();
  if (arr->elem_size <= 0) abort();

  if (arr->len == arr->cap) {
    int64_t new_cap = arr->cap ? arr->cap * 2 : 8;
    int64_t new_bytes = new_cap * arr->elem_size;
    if (arr->data) {
      arr->data = ty_realloc(arr->data, new_bytes, arr->elem_align);
    } else {
      arr->data = ty_alloc(new_bytes, arr->elem_align);
    }
    arr->cap = new_cap;
  }

  uint8_t* base = (uint8_t*)arr->data;
  memcpy(base + (arr->len * arr->elem_size), elem_bytes, (size_t)arr->elem_size);
  arr->len += 1;
}
