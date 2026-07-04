#![no_main]

#include "usys.h"
#include "ulib.h"

char *user_argv0(void) {
    return (char *)USER_ARG0_ADDR;
}

char *user_arg(void) {
    return (char *)USER_ARG_ADDR;
}

int streq(const char *a, const char *b) {
    int i = 0;

    while (a[i] && b[i]) {
        if (a[i] != b[i]) {
            return 0;
        }
        i = i + 1;
    }
    return a[i] == b[i];
}

void puts(const char *s) {
    int i = 0;

    while (s[i]) {
        i = i + 1;
    }
    if (i > 0) {
        write(1, s, i);
    }
}

int read_line(char *buf, int max) {
    int n = 0;

    if (max <= 0) {
        return 0;
    }

    while (n < max - 1) {
        char c;
        int r = read(0, &c, 1);
        if (r <= 0) {
            break;
        }

        if (c == '\n') {
            break;
        }
        buf[n] = c;
        n = n + 1;
    }
    buf[n] = 0;
    return n;
}

void *malloc(int n) {
    int aligned;
    void *p;

    if (n <= 0) {
        return (void *)0;
    }

    aligned = (n + 3) & ~3;
    if (aligned < n) {
        return (void *)0;
    }

    p = sbrk(aligned);
    if ((int)p == -1) {
        return (void *)0;
    }
    return p;
}

void free(void *p) {
    (void)p;
}
