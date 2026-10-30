/*
 * `stdarg.h` for hosted (LLVM) targets, x86-64 System V.
 *
 * `va_list` is the System V structure: two offsets into the register save area
 * and two pointers. `llvm.va_start` fills it, and `va_arg` advances the offset
 * or the overflow pointer, which is the same expansion clang inlines.
 */
#ifndef __SHYCC_STDARG_H
#define __SHYCC_STDARG_H

typedef struct __shyc_va_tag {
  unsigned int gp;
  unsigned int fp;
  void *overflow;
  void *reg_save;
} __shyc_va_tag;

typedef __shyc_va_tag va_list[1];
typedef va_list __gnuc_va_list;

void __builtin_va_start(void *ap);

#define va_start(ap, last) __builtin_va_start((void *)(ap))
#define va_end(ap) ((void)0)
#define va_copy(dest, src) ((dest)[0] = (src)[0])

#define __shyc_va_bytes(dst, src, n)                                      \
  do {                                                                    \
    unsigned int __shyc_i;                                                \
    for (__shyc_i = 0; __shyc_i < (unsigned int)(n); __shyc_i++)           \
      ((unsigned char *)(dst))[__shyc_i] =                                \
          ((const unsigned char *)(src))[__shyc_i];                       \
  } while (0)

#define va_arg(ap, ty)                                                    \
  ({                                                                      \
    ty __shyc_v;                                                          \
    unsigned char *__shyc_src;                                            \
    unsigned int __shyc_sz = (unsigned int)sizeof(ty);                    \
    int __shyc_cls = __builtin_reg_class(ty);                             \
    if (__shyc_cls == 0 && (ap)[0].gp + 8u <= 48u) {                      \
      __shyc_src = (unsigned char *)(ap)[0].reg_save + (ap)[0].gp;         \
      (ap)[0].gp += 8;                                                    \
    } else if (__shyc_cls == 1 && (ap)[0].fp + 16u <= 176u) {              \
      __shyc_src = (unsigned char *)(ap)[0].reg_save + (ap)[0].fp;         \
      (ap)[0].fp += 16;                                                   \
    } else {                                                              \
      __shyc_src = (unsigned char *)(ap)[0].overflow;                      \
      (ap)[0].overflow += (__shyc_sz + 7u) & ~7u;                          \
    }                                                                     \
    __shyc_va_bytes(&__shyc_v, __shyc_src, __shyc_sz);                    \
    __shyc_v;                                                             \
  })

#endif
