#ifndef __STDARG_H
#define __STDARG_H

#ifdef __shy__

// Shy varargs descriptor.  The array-of-one form keeps the descriptor
// addressable when it is passed to vprintf/vscanf-style functions.
struct __shy_va_list {
  unsigned int reg_slot;
  char *reg_save;
  char *stack;
};

typedef struct __shy_va_list va_list[1];

// __va_area__ layout written by codegen:
//   +0  unsigned int: number of named argument slots already in registers
//   +4  char *:       register save area (8 slots, 32 bits each)
//   +8  char *:       descending stack cursor for named stack arguments
//   +12 register save area
//
// va_start only copies the descriptor.  It does not modify __va_area__.
#define va_start(ap, last)                                                  \
  do {                                                                      \
    (ap)[0].reg_slot = *(unsigned int *)((char *)__va_area__ + 0);           \
    (ap)[0].reg_save = *(char **)((char *)__va_area__ + 4);                  \
    (ap)[0].stack = *(char **)((char *)__va_area__ + 8);                     \
  } while (0)

#define va_end(ap) ((void)0)

#define va_copy(dest, src) ((dest)[0] = (src)[0])

// Byte copy without memcpy.  Used for arbitrary struct/union sizes and for
// the small register-save scratch buffer.
#define __shy_va_copy_bytes(dst, src, n)                                    \
  do {                                                                      \
    unsigned int __shy_va_i;                                                \
    for (__shy_va_i = 0; __shy_va_i < (unsigned int)(n); __shy_va_i++)      \
      ((unsigned char *)(dst))[__shy_va_i] =                                \
          ((const unsigned char *)(src))[__shy_va_i];                       \
  } while (0)

// Scalar <= 8 bytes: use the next whole register slot(s) if they fit.
// 64-bit scalars consume two slots (low word, then high word) and never
// split; if only one slot remains they spill, leaving that slot for a later
// 32-bit scalar.
//
// struct/union: always read native bytes from the descending stack cursor.
// No register slots are consumed.
//
// Register layout within a 32-bit slot is big-endian.  Narrow scalars put
// their low bits at the end of the slot.  Native 64-bit memory is high word
// first, so register low/high must be swapped while copying.
//
// The value is materialized in a local of type ty, so aggregate and scalar
// results use the same expression shape and arbitrary struct/union sizes are
// supported.
#define va_arg(ap, ty)                                                      \
  ({                                                                        \
    ty __shy_va_v;                                                          \
    unsigned char *__shy_va_dst = (unsigned char *)&__shy_va_v;             \
    unsigned int __shy_va_sz = (unsigned int)sizeof(ty);                    \
    unsigned int __shy_va_cls = (unsigned int)__builtin_reg_class(ty);      \
    unsigned int __shy_va_slots = __shy_va_sz > 4 ? 2u : 1u;                \
    if (__shy_va_cls != 2 && __shy_va_sz <= 8 &&                            \
        (ap)[0].reg_slot + __shy_va_slots <= 8) {                           \
      unsigned char *__shy_va_src =                                         \
          (unsigned char *)(ap)[0].reg_save + (ap)[0].reg_slot * 4;         \
      if (__shy_va_sz > 4) {                                                \
        __shy_va_copy_bytes(__shy_va_dst + 0, __shy_va_src + 4, 4);         \
        __shy_va_copy_bytes(__shy_va_dst + 4, __shy_va_src + 0, 4);         \
      } else {                                                              \
        __shy_va_copy_bytes(__shy_va_dst,                                   \
                            __shy_va_src + (4 - __shy_va_sz),               \
                            __shy_va_sz);                                   \
      }                                                                     \
      (ap)[0].reg_slot = (ap)[0].reg_slot + __shy_va_slots;                 \
    } else {                                                                \
      (ap)[0].stack = (ap)[0].stack - ((__shy_va_sz + 3u) & ~3u);           \
      __shy_va_copy_bytes(__shy_va_dst, (ap)[0].stack, __shy_va_sz);        \
    }                                                                       \
    __shy_va_v;                                                             \
  })

#else

typedef struct {
  unsigned int gp_offset;
  unsigned int fp_offset;
  void *overflow_arg_area;
  void *reg_save_area;
} __va_elem;

typedef __va_elem va_list[1];

#define va_start(ap, last) \
  do { *(ap) = *(__va_elem *)__va_area__; } while (0)

#define va_end(ap)

static void *__va_arg_mem(__va_elem *ap, int sz, int align) {
  void *p = ap->overflow_arg_area;
  if (align > 8)
    p = (p + 15) / 16 * 16;
  ap->overflow_arg_area = ((unsigned long)p + sz + 7) / 8 * 8;
  return p;
}

static void *__va_arg_gp(__va_elem *ap, int sz, int align) {
  if (ap->gp_offset >= 48)
    return __va_arg_mem(ap, sz, align);

  void *r = ap->reg_save_area + ap->gp_offset;
  ap->gp_offset += 8;
  return r;
}

static void *__va_arg_fp(__va_elem *ap, int sz, int align) {
  if (ap->fp_offset >= 112)
    return __va_arg_mem(ap, sz, align);

  void *r = ap->reg_save_area + ap->fp_offset;
  ap->fp_offset += 8;
  return r;
}

#define va_arg(ap, ty)                                                  \
  ({                                                                    \
    int klass = __builtin_reg_class(ty);                                \
    *(ty *)(klass == 0 ? __va_arg_gp(ap, sizeof(ty), _Alignof(ty)) :    \
            klass == 1 ? __va_arg_fp(ap, sizeof(ty), _Alignof(ty)) :    \
            __va_arg_mem(ap, sizeof(ty), _Alignof(ty)));                \
  })

#define va_copy(dest, src) ((dest)[0] = (src)[0])

#endif

#define __GNUC_VA_LIST 1
typedef va_list __gnuc_va_list;

#endif
