#include "chibicc.h"

struct ShyFile {
  int fd;
};

static FILE stdout_file = {1};
static FILE stderr_file = {2};
FILE *stdout = &stdout_file;
FILE *stderr = &stderr_file;

static unsigned char *heap = (unsigned char *)0x00200000;

static size_t align16(size_t n) {
  return (n + 15) & ~(size_t)15;
}

int align_to(int n, int align) {
  return (n + align - 1) / align * align;
}

void *malloc(size_t size) {
  if (!size)
    size = 1;
  size = align16(size);
  void *p = heap;
  heap = heap + size;
  return p;
}

void *calloc(size_t nmemb, size_t size) {
  size_t total = nmemb * size;
  unsigned char *p = malloc(total);
  memset(p, 0, total);
  return p;
}

void *realloc(void *ptr, size_t size) {
  void *p = malloc(size);
  if (ptr && size)
    memcpy(p, ptr, size);
  return p;
}

void free(void *ptr) {
  (void *)ptr;
}

char *strdup(const char *s) {
  size_t n = strlen(s) + 1;
  char *p = malloc(n);
  memcpy(p, s, n);
  return p;
}

char *strndup(const char *s, size_t n) {
  size_t len = 0;
  while (len < n && s[len])
    len = len + 1;
  char *p = calloc(1, len + 1);
  memcpy(p, s, len);
  return p;
}

int strncasecmp(const char *s1, const char *s2, size_t n) {
  for (size_t i = 0; i < n; i++) {
    unsigned char a = (unsigned char)tolower(s1[i]);
    unsigned char b = (unsigned char)tolower(s2[i]);
    if (a != b)
      return (int)a - (int)b;
    if (!a)
      return 0;
  }
  return 0;
}

int fputc(int c, FILE *out) {
  char ch = (char)c;
  write(out ? out->fd : 1, &ch, 1);
  return c;
}

int fputs(const char *s, FILE *out) {
  size_t n = strlen(s);
  write(out ? out->fd : 1, s, n);
  return (int)n;
}

size_t fwrite(const void *ptr, size_t size, size_t nmemb, FILE *out) {
  size_t n = size * nmemb;
  ssize_t written = write(out ? out->fd : 1, ptr, n);
  return written < 0 ? 0 : (size_t)written / size;
}

int vfprintf(FILE *out, const char *fmt, va_list ap) {
  char buf[2048];
  int n = vsnprintf(buf, sizeof(buf), fmt, ap);
  if (n < 0)
    return n;
  size_t len = (size_t)n;
  if (len >= sizeof(buf))
    len = sizeof(buf) - 1;
  write(out ? out->fd : 1, buf, len);
  return n;
}

int fprintf(FILE *out, const char *fmt, ...) {
  va_list ap;
  va_start(ap, fmt);
  int n = vfprintf(out, fmt, ap);
  va_end(ap);
  return n;
}

int fclose(FILE *out) {
  (void *)out;
  return 0;
}
