#ifndef SHY_TYPES_H
#define SHY_TYPES_H

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

typedef int8_t i8;
typedef int16_t i16;
typedef int32_t i32;
typedef int64_t i64;

typedef uint8_t u8;
typedef uint16_t u16;
typedef uint32_t u32;
typedef uint64_t u64;

typedef intptr_t isize;
typedef uintptr_t usize;

typedef float f32;
typedef double f64;

#if defined(__cplusplus)
#define SHY_STATIC_ASSERT(cond, msg) static_assert(cond, msg)
#elif defined(__STDC_VERSION__) && __STDC_VERSION__ >= 201112L
#define SHY_STATIC_ASSERT(cond, msg) _Static_assert(cond, msg)
#else
#define SHY_STATIC_ASSERT(cond, msg)
#endif

SHY_STATIC_ASSERT(sizeof(i8) == 1u, "i8 must be 1 byte");
SHY_STATIC_ASSERT(sizeof(i16) == 2u, "i16 must be 2 bytes");
SHY_STATIC_ASSERT(sizeof(i32) == 4u, "i32 must be 4 bytes");
SHY_STATIC_ASSERT(sizeof(i64) == 8u, "i64 must be 8 bytes");

SHY_STATIC_ASSERT(sizeof(u8) == 1u, "u8 must be 1 byte");
SHY_STATIC_ASSERT(sizeof(u16) == 2u, "u16 must be 2 bytes");
SHY_STATIC_ASSERT(sizeof(u32) == 4u, "u32 must be 4 bytes");
SHY_STATIC_ASSERT(sizeof(u64) == 8u, "u64 must be 8 bytes");

SHY_STATIC_ASSERT(sizeof(isize) == sizeof(void *), "isize must be pointer-sized");
SHY_STATIC_ASSERT(sizeof(usize) == sizeof(void *), "usize must be pointer-sized");

SHY_STATIC_ASSERT(sizeof(f32) == 4u, "f32 must be 4 bytes");
SHY_STATIC_ASSERT(sizeof(f64) == 8u, "f64 must be 8 bytes");

#undef SHY_STATIC_ASSERT

#endif
