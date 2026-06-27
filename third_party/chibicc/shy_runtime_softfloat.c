// Approximate Shy bare-metal soft-float runtime.
//
// Build separately and link with C objects that use float helpers:
//
//   chibicc --target=shy -S -o shy_runtime_softfloat.shy shy_runtime_softfloat.c
//   asm shy_runtime_softfloat.shy -o shy_runtime_softfloat.sobj
//   linker app.sobj shy_runtime_softfloat.sobj -o app.sfs
//
// This is intentionally small and pragmatic. It handles zero, finite normal
// f32 values and truncating int conversions. It does not try to be strict
// IEEE754: no NaN/Inf/subnormal/rounding-mode support yet.

typedef unsigned int u32;
typedef int i32;
typedef unsigned long u64;
typedef long i64;

#define SIGN 0x80000000U
#define FRAC 0x007fffffU
#define HIDDEN 0x00800000U

static u32 pack_f32(int sign, int exp, u32 mant) {
  if (mant == 0)
    return 0;

  while (mant >= 0x01000000U) {
    mant = mant >> 1;
    exp = exp + 1;
  }

  while (mant < HIDDEN && exp > 0) {
    mant = mant << 1;
    exp = exp - 1;
  }

  if (exp <= 0)
    return 0;
  if (exp >= 255)
    return (sign ? SIGN : 0) | 0x7f800000U;

  return (sign ? SIGN : 0) | ((u32)exp << 23) | (mant & FRAC);
}

static u32 abs_f32_to_i32(u32 x) {
  int exp = ((x >> 23) & 255) - 127;
  u32 mant = (x & FRAC) | HIDDEN;

  if ((x & 0x7fffffffU) == 0 || exp < 0)
    return 0;
  if (exp > 30)
    return 0x7fffffffU;
  if (exp >= 23)
    return mant << (exp - 23);
  return mant >> (23 - exp);
}

static u32 i32_to_f32_bits(i32 v) {
  if (v == 0)
    return 0;

  int sign = v < 0;
  u32 n = sign ? (u32)-v : (u32)v;
  int exp = 127 + 23;

  while (n < HIDDEN) {
    n = n << 1;
    exp = exp - 1;
  }

  return pack_f32(sign, exp, n);
}

u32 __shy_i64_to_f32(i64 v) {
  return i32_to_f32_bits((i32)v);
}

u32 __shy_u64_to_f32(u64 v) {
  return i32_to_f32_bits((i32)(u32)v);
}

i64 __shy_f32_to_i64(u32 x) {
  u32 n = abs_f32_to_i32(x);
  i32 v = (i32)n;
  return (x & SIGN) ? -v : v;
}

u64 __shy_f32_to_u64(u32 x) {
  return abs_f32_to_i32(x);
}

u32 __shy_f32_add(u32 a, u32 b) {
  if ((a & 0x7fffffffU) == 0)
    return b;
  if ((b & 0x7fffffffU) == 0)
    return a;

  int sa = (a >> 31) & 1;
  int sb = (b >> 31) & 1;
  int ea = (a >> 23) & 255;
  int eb = (b >> 23) & 255;
  u32 ma = (a & FRAC) | HIDDEN;
  u32 mb = (b & FRAC) | HIDDEN;

  if (ea > eb) {
    int d = ea - eb;
    mb = d >= 31 ? 0 : mb >> d;
    eb = ea;
  } else if (eb > ea) {
    int d = eb - ea;
    ma = d >= 31 ? 0 : ma >> d;
    ea = eb;
  }

  if (sa == sb)
    return pack_f32(sa, ea, ma + mb);

  if (ma >= mb)
    return pack_f32(sa, ea, ma - mb);
  return pack_f32(sb, ea, mb - ma);
}

u32 __shy_f32_sub(u32 a, u32 b) {
  return __shy_f32_add(a, b ^ SIGN);
}

u32 __shy_f32_mul(u32 a, u32 b) {
  if ((a & 0x7fffffffU) == 0 || (b & 0x7fffffffU) == 0)
    return 0;

  int sign = ((a ^ b) >> 31) & 1;
  int exp = ((a >> 23) & 255) + ((b >> 23) & 255) - 127;
  u32 ma = ((a & FRAC) | HIDDEN) >> 8;
  u32 mb = ((b & FRAC) | HIDDEN) >> 8;
  u32 prod = ma * mb;

  return pack_f32(sign, exp - 7, prod);
}

u32 __shy_f32_div(u32 a, u32 b) {
  if ((a & 0x7fffffffU) == 0)
    return 0;
  if ((b & 0x7fffffffU) == 0)
    return ((a ^ b) & SIGN) | 0x7f800000U;

  int sign = ((a ^ b) >> 31) & 1;
  int exp = ((a >> 23) & 255) - ((b >> 23) & 255) + 127;
  u32 ma = (a & FRAC) | HIDDEN;
  u32 mb = ((b & FRAC) | HIDDEN) >> 8;
  u32 q = mb ? ma / mb : 0x00ffffffU;

  return pack_f32(sign, exp + 8, q);
}

i32 __shy_f32_eq(u32 a, u32 b) {
  return a == b;
}

i32 __shy_f32_ne(u32 a, u32 b) {
  return a != b;
}

i32 __shy_f32_lt(u32 a, u32 b) {
  i32 ia = (i32)__shy_f32_to_i64(a);
  i32 ib = (i32)__shy_f32_to_i64(b);
  return ia < ib;
}

i32 __shy_f32_le(u32 a, u32 b) {
  i32 ia = (i32)__shy_f32_to_i64(a);
  i32 ib = (i32)__shy_f32_to_i64(b);
  return ia <= ib;
}

// Minimal double bridge. These preserve linkability for double-heavy code.
// They intentionally degrade through int/float-like behavior until a real
// f64 implementation is added.
u64 __shy_f32_to_f64(u32 x) { return __shy_f32_to_i64(x); }
u32 __shy_f64_to_f32(u64 x) { return __shy_i64_to_f32((i64)x); }
i64 __shy_f64_to_i64(u64 x) { return (i64)x; }
u64 __shy_f64_to_u64(u64 x) { return x; }
u64 __shy_i64_to_f64(i64 x) { return (u64)x; }
u64 __shy_u64_to_f64(u64 x) { return x; }
u64 __shy_f64_add(u64 a, u64 b) { return a + b; }
u64 __shy_f64_sub(u64 a, u64 b) { return a - b; }
u64 __shy_f64_mul(u64 a, u64 b) { return a; }
u64 __shy_f64_div(u64 a, u64 b) { return a; }
i32 __shy_f64_eq(u64 a, u64 b) { return a == b; }
i32 __shy_f64_ne(u64 a, u64 b) { return a != b; }
i32 __shy_f64_lt(u64 a, u64 b) { return 0; }
i32 __shy_f64_le(u64 a, u64 b) { return a == b; }
