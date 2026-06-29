#include "chibicc.h"

void strarray_push(StringArray *arr, char *s) {
  if (!arr->data) {
    arr->data = calloc(8, sizeof(char *));
    arr->capacity = 8;
  }

  if (arr->capacity == arr->len) {
    arr->data = realloc(arr->data, sizeof(char *) * arr->capacity * 2);
    arr->capacity *= 2;
    for (int i = arr->len; i < arr->capacity; i++)
      arr->data[i] = NULL;
  }

  arr->data[arr->len++] = s;
}

// Takes a printf-style format string and returns a formatted string.
char *format(char *fmt, ...) {
  size_t cap = 256;
  for (;;) {
    char *buf = calloc(1, cap);

    va_list ap;
    va_start(ap, fmt);
    int len = vsnprintf(buf, cap, fmt, ap);
    va_end(ap);

    if (len < 0)
      return buf;
    if ((size_t)len < cap)
      return buf;

    cap = (size_t)len + 1;
  }
}
