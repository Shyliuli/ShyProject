#![no_main]

#include "usys.h"
#include "ulib.h"

struct String {
    const char *data;
};

struct Buffer {
    char *data;
    int max;
    int len;
};

impl String {
    int len(self *s) {
        int n = 0;

        while (s.data[n]) {
            n = n + 1;
        }
        return n;
    }

    int eq(self *s, const char *other) {
        int i = 0;

        while (s.data[i] && other[i]) {
            if (s.data[i] != other[i]) {
                return 0;
            }
            i = i + 1;
        }
        return s.data[i] == other[i];
    }

    int starts_with(self *s, const char *prefix) {
        int i = 0;

        while (prefix[i]) {
            if (s.data[i] != prefix[i]) {
                return 0;
            }
            i = i + 1;
        }
        return 1;
    }

    int find(self *s, const char *needle) {
        int i = 0;
        struct String pat;
        int n;

        pat.data = needle;
        n = pat.len();
        if (n == 0) {
            return 0;
        }
        while (s.data[i]) {
            int j = 0;

            while (needle[j] && s.data[i + j] == needle[j]) {
                j = j + 1;
            }
            if (j == n) {
                return i;
            }
            i = i + 1;
        }
        return -1;
    }
}

impl Buffer {
    void init(self *b, char *data, int max) {
        b.data = data;
        b.max = max;
        b.len = 0;
    }

    int has_room(self *b) {
        return b.len < b.max - 1;
    }
}

char *user_argv0(void) {
    return (char *)USER_ARG0_ADDR;
}

char *user_arg(void) {
    return (char *)USER_ARG_ADDR;
}

int streq(const char *a, const char *b) {
    struct String s;

    s.data = a;
    return s.eq(b);
}

void puts(const char *s) {
    struct String text;
    int n;

    text.data = s;
    n = text.len();
    if (n > 0) {
        write(1, s, n);
    }
}

int read_line(char *buf, int max) {
    struct Buffer line;

    if (max <= 0) {
        return 0;
    }

    line.init(buf, max);
    while (line.has_room()) {
        char c;
        int r = read(0, &c, 1);
        if (r <= 0) {
            break;
        }

        if (c == '\n') {
            break;
        }
        line.data[line.len] = c;
        line.len = line.len + 1;
    }
    line.data[line.len] = 0;
    return line.len;
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
