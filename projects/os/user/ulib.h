#ifndef SHYOS_USER_ULIB_H
#define SHYOS_USER_ULIB_H

#define USER_ARG0_ADDR 0x000eef80
#define USER_ARG_ADDR 0x000ef000

char *user_argv0(void);
char *user_arg(void);
int streq(const char *a, const char *b);
void puts(const char *s);
int read_line(char *buf, int max);
void *malloc(int n);
void free(void *p);

#endif
