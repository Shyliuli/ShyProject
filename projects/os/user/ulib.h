#ifndef SHYOS_USER_ULIB_H
#define SHYOS_USER_ULIB_H

// 参数区地址是内核与用户程序共享的约定，直接复用内核的内存布局定义。
#include "../kernel/memlayout.h"

char *user_argv0(void);
char *user_arg(void);
int streq(const char *a, const char *b);
void puts(const char *s);
int read_line(char *buf, int max);
void *malloc(int n);
void free(void *p);

#endif
