#ifndef SHYOS_USER_ULIB_H
#define SHYOS_USER_ULIB_H

// The C library is libshy, linked over ShyOS system calls. This header adds
// what only ShyOS has.
#include <stdio.h>
#include <string.h>
#include <fcntl.h>

// 参数区地址是内核与用户程序共享的约定，直接复用内核的内存布局定义。
#include "../kernel/memlayout.h"

char *user_argv0(void);
char *user_arg(void);
int streq(const char *a, const char *b);
int read_line(char *buf, int max);

#endif
