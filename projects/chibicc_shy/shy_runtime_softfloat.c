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
  if ((a & 0x7fffffffU) == 0 && (b & 0x7fffffffU) == 0)
    return 1;
  return a == b;
}

i32 __shy_f32_ne(u32 a, u32 b) {
  return !__shy_f32_eq(a, b);
}

static u32 f32_order_key(u32 x) {
  return (x & SIGN) ? ~x : (x | SIGN);
}

i32 __shy_f32_lt(u32 a, u32 b) {
  if (__shy_f32_eq(a, b))
    return 0;
  return f32_order_key(a) < f32_order_key(b);
}

i32 __shy_f32_le(u32 a, u32 b) {
  return __shy_f32_eq(a, b) || __shy_f32_lt(a, b);
}

// Minimal double bridge. It handles finite normal values well enough for
// casts and printf("%f") while still avoiding a full IEEE754 runtime.
#define F64_SIGN 0x8000000000000000UL
#define F64_FRAC 0x000fffffffffffffUL
#define F64_HIDDEN 0x0010000000000000UL

u64 __shy_f32_to_f64(u32 x) {
  u64 sign = (u64)(x & SIGN) << 32;
  u32 exp = (x >> 23) & 255;
  u32 frac = x & FRAC;

  if (exp == 0) {
    return sign;
  } else if (exp == 255) {
    return sign | 0x7ff0000000000000UL | ((u64)frac << 29);
  }

  u64 exp64 = (u64)((i32)exp - 127 + 1023);
  return sign | (exp64 << 52) | ((u64)frac << 29);
}

u32 __shy_f64_to_f32(u64 x) {
  u32 sign = (u32)(x >> 32) & SIGN;
  int exp = (int)((x >> 52) & 0x7ff);
  u64 frac = x & F64_FRAC;

  if (exp == 0)
    return sign;
  if (exp == 0x7ff)
    return sign | 0x7f800000U | (u32)(frac >> 29);

  exp = exp - 1023 + 127;
  if (exp <= 0)
    return sign;
  if (exp >= 255)
    return sign | 0x7f800000U;

  return sign | ((u32)exp << 23) | (u32)(frac >> 29);
}

static u64 abs_f64_to_u64(u64 x) {
  int exp = (int)((x >> 52) & 0x7ff) - 1023;
  u64 mant = (x & F64_FRAC) | F64_HIDDEN;

  if ((x & ~F64_SIGN) == 0 || exp < 0)
    return 0;
  if (exp >= 63)
    return 0x7fffffffffffffffUL;
  if (exp >= 52)
    return mant << (exp - 52);
  return mant >> (52 - exp);
}

i64 __shy_f64_to_i64(u64 x) {
  u64 n = abs_f64_to_u64(x);
  i64 v = (i64)n;
  return (x & F64_SIGN) ? -v : v;
}

u64 __shy_f64_to_u64(u64 x) {
  if (x & F64_SIGN)
    return 0;
  return abs_f64_to_u64(x);
}

static u64 u64_to_f64_bits(u64 n, int sign) {
  if (n == 0)
    return sign ? F64_SIGN : 0;

  int bit = 63;
  while (((n >> bit) & 1) == 0)
    bit = bit - 1;

  u64 frac;
  if (bit > 52)
    frac = n >> (bit - 52);
  else
    frac = n << (52 - bit);

  return (sign ? F64_SIGN : 0) | ((u64)(bit + 1023) << 52) | (frac & F64_FRAC);
}

u64 __shy_i64_to_f64(i64 x) {
  u64 ux = (u64)x;
  int sign = (int)(ux >> 63);
  u64 n = sign ? (~ux + 1) : ux;
  return u64_to_f64_bits(n, sign);
}

u64 __shy_u64_to_f64(u64 x) { return u64_to_f64_bits(x, 0); }
u64 __shy_f64_add(u64 a, u64 b) {
  return __shy_f32_to_f64(__shy_f32_add(__shy_f64_to_f32(a), __shy_f64_to_f32(b)));
}

u64 __shy_f64_sub(u64 a, u64 b) {
  return __shy_f32_to_f64(__shy_f32_sub(__shy_f64_to_f32(a), __shy_f64_to_f32(b)));
}

u64 __shy_f64_mul(u64 a, u64 b) {
  return __shy_f32_to_f64(__shy_f32_mul(__shy_f64_to_f32(a), __shy_f64_to_f32(b)));
}

u64 __shy_f64_div(u64 a, u64 b) {
  return __shy_f32_to_f64(__shy_f32_div(__shy_f64_to_f32(a), __shy_f64_to_f32(b)));
}

i32 __shy_f64_eq(u64 a, u64 b) {
  return __shy_f32_eq(__shy_f64_to_f32(a), __shy_f64_to_f32(b));
}

i32 __shy_f64_ne(u64 a, u64 b) {
  return !__shy_f64_eq(a, b);
}

i32 __shy_f64_lt(u64 a, u64 b) {
  return __shy_f32_lt(__shy_f64_to_f32(a), __shy_f64_to_f32(b));
}

i32 __shy_f64_le(u64 a, u64 b) {
  return __shy_f32_le(__shy_f64_to_f32(a), __shy_f64_to_f32(b));
}
