#include "chibicc.h"

// ShyISA bare-metal backend.
//
// This file intentionally reuses chibicc's complete C frontend and only
// replaces the final code generation stage. It emits Shy assembly text, then
// ShyProject's own asm/linker can produce .sobj/.sfs.
//
// ABI v1:
// - Shy uses ILP32-style pointers: int and pointer are 32-bit, long remains
//   64-bit. Memory is big-endian, so a 64-bit object stores its high word first.
// - Scalar return: 1x=low32, 2x=high32 when the value is 64-bit.
// - Scalars take one 32-bit register slot (two for 64-bit, low word first) when
//   the whole value fits in 4x..bx; otherwise they go on the stack. A scalar is
//   never split between registers and memory, and a scalar that misses the
//   registers does not consume a slot for later scalars.
// - Structs and unions are always passed by value on the stack and returned
//   through a hidden pointer in 4x.
// - fx is the frame pointer. Shy stack grows upward and the return address sits
//   at fx-8, so stack arguments live below that.
// - By default, _start initializes sp, calls main, and writes main's low return
//   word to exit. `#![no_main]` disables this startup and makes user `_start`
//   a bare entry with no prologue.
// - TLS is explicitly unsupported for now.

typedef int VInfo;

typedef struct Rename Rename;
struct Rename {
  Rename *next;
  char *old;
  char *new;
};

static FILE *output_file;
static Obj *current_fn;
static int labelseq;
static bool need_u64_divmod;
static bool need_u64_mul;
static bool need_i64_cmp;

static char *argreg[] = {"4x", "5x", "6x", "7x", "8x", "9x", "ax", "bx"};
static int argreg_len = sizeof(argreg) / sizeof(*argreg);
static Rename *renames;

#define println codegen_shy_println

__attribute__((format(printf, 1, 2)))
static void println(char *fmt, ...) {
  va_list ap;
  va_start(ap, fmt);
  vfprintf(output_file, fmt, ap);
  va_end(ap);
  fprintf(output_file, "\n");
}

static int count(void) {
  return ++labelseq;
}

// ShyISA register names are valid C identifiers, and the assembler reads a
// bare register name as a register. A C symbol spelled like one (`exit`,
// `sp`, ...) is written as `@name` in instruction operands; the symbol table
// keeps the plain name.
static bool is_shy_register_name(char *name) {
  static char *names[] = {
    "0x", "1x", "2x", "3x", "4x", "5x", "6x", "7x", "8x", "9x",
    "ax", "bx", "cx", "dx", "ex", "fx", "pc", "segs", "sp", "tm",
    "status", "trap", "m1", "m2", "m3", "m4", "rs", "exit", "epc",
    "cause", "ksp", "sege",
  };
  for (int i = 0; i < (int)(sizeof(names) / sizeof(*names)); i++)
    if (!strcmp(name, names[i]))
      return true;
  return false;
}

static char *symbol_operand(char *name) {
  if (!is_shy_register_name(name))
    return name;
  return format("@%s", name);
}

static char *sanitize(char *s) {
  char *buf = calloc(1, strlen(s) + 1);
  for (int i = 0; s[i]; i++) {
    char c = s[i];
    buf[i] = isalnum(c) ? c : '_';
  }
  return buf;
}

static void rename_private_symbols(Obj *prog) {
  char *prefix = sanitize(base_file ? base_file : "input");

  for (Obj *var = prog; var; var = var->next) {
    if (!var->name)
      continue;
    if (!var->is_static && strncmp(var->name, ".L", 2))
      continue;

    Rename *r = calloc(1, sizeof(Rename));
    r->old = var->name;
    r->new = format(".L.shy.%s.%s", prefix, sanitize(var->name));
    r->next = renames;
    renames = r;
    var->name = r->new;
  }

  for (Obj *var = prog; var; var = var->next) {
    for (Relocation *rel = var->rel; rel; rel = rel->next) {
      for (Rename *r = renames; r; r = r->next) {
        if (!strcmp(*rel->label, r->old)) {
          *rel->label = r->new;
          break;
        }
      }
    }
  }
}

static void unsupported(Node *node, char *what) {
  error_tok(node->tok, "Shy backend does not support %s yet", what);
}

static bool is_64bit(Type *ty) {
  return ty->size == 8;
}

static bool is_aggregate(Type *ty) {
  return ty->kind == TY_STRUCT || ty->kind == TY_UNION;
}

static VInfo vinfo(Type *ty) {
  if (ty->kind == TY_ARRAY || ty->kind == TY_FUNC)
    return 1;
  return is_64bit(ty) ? 2 : 1;
}

static bool vinfo_is64(VInfo vi) {
  return vi == 2;
}

static bool is_shy_flonum(Type *ty) {
  return ty->kind == TY_FLOAT || ty->kind == TY_DOUBLE;
}

static int stack_size_of(Type *ty) {
  return align_to(MAX(ty->size, 4), 4);
}

static void assign_lvar_offsets(Obj *prog) {
  for (Obj *fn = prog; fn; fn = fn->next) {
    if (!fn->is_function)
      continue;

    int off = 0;
    for (Obj *var = fn->locals; var; var = var->next) {
      if (var->is_sret_alias || var->is_elided)
        continue;
      off = align_to(off, MAX(4, var->align));
      var->offset = off;
      off += stack_size_of(var->ty);
    }
    fn->stack_size = align_to(off, 4);
  }
}

static void push32(char *reg) {
  println("pusha %s", reg);
}

static void pop32(char *reg) {
  println("popa %s", reg);
}

static void push_value(VInfo vi) {
  if (vinfo_is64(vi))
    push32("2x");
  push32("1x");
}

static void pop_value(VInfo vi, char *lo, char *hi) {
  pop32(lo);
  if (vinfo_is64(vi))
    pop32(hi);
}

static void set_bool_from_rs(void) {
  int c = count();
  println("setn 1x 0");
  println("jmpn .L.true.%d", c);
  println("ujmpn .L.end.%d", c);
  println(".L.true.%d:", c);
  println("setn 1x 1");
  println(".L.end.%d:", c);
  println("setn 2x 0");
}

static void gen_expr(Node *node);
static void gen_stmt(Node *node);
static void cmp_zero(VInfo vi);

static bool asm_uses_addr(Node *node, AsmBinding *binding) {
  char *needle = format("{&%s}", binding->name);
  return strstr(node->asm_str, needle);
}

static void gen_var_addr(Obj *var) {
  if (var->is_local) {
    if (var->is_sret_alias) {
      Obj *retptr = current_fn->params;
      if (!retptr || !retptr->ty || retptr->ty->kind != TY_PTR)
        error("missing hidden return buffer");
      println("setn 1x %d", retptr->offset);
      println("adda 1x fx");
      println("geta 1x 1x");
      println("setn 2x 0");
      return;
    }
    println("setn 1x %d", var->offset);
    println("adda 1x fx");
  } else {
    println("setn 1x %s", symbol_operand(var->name));
  }
  println("setn 2x 0");
}

static void gen_addr(Node *node) {
  switch (node->kind) {
  case ND_VAR:
    if (node->var->is_local) {
      gen_var_addr(node->var);
      return;
    }
    gen_var_addr(node->var);
    return;
  case ND_DEREF:
    gen_expr(node->lhs);
    return;
  case ND_COMMA:
    gen_expr(node->lhs);
    gen_addr(node->rhs);
    return;
  case ND_MEMBER:
    gen_addr(node->lhs);
    if (node->member->offset)
      println("addn 1x %d", node->member->offset);
    return;
  case ND_COND: {
    int c = count();
    gen_expr(node->cond);
    cmp_zero(vinfo(node->cond->ty));
    println("jmpn .L.addr.else.%d", c);
    gen_addr(node->then);
    println("ujmpn .L.addr.end.%d", c);
    println(".L.addr.else.%d:", c);
    gen_addr(node->els);
    println(".L.addr.end.%d:", c);
    return;
  }
  case ND_STMT_EXPR:
    // The trailing expression leaves the aggregate address in 1x.
    gen_expr(node);
    return;
  case ND_FUNCALL:
    if (node->ret_buffer) {
      gen_expr(node);
      gen_var_addr(node->ret_buffer);
      return;
    }
    break;
  case ND_ASSIGN:
    if (node->ty->kind == TY_STRUCT || node->ty->kind == TY_UNION) {
      gen_expr(node);
      gen_addr(node->lhs);
      return;
    }
    break;
  default:
    unsupported(node, "address expression");
  }
}

static void load(Type *ty) {
  if (ty->kind == TY_ARRAY || ty->kind == TY_FUNC)
    return;
  // Aggregate rvalues are represented by their address in 1x.
  if (ty->kind == TY_STRUCT || ty->kind == TY_UNION)
    return;

  switch (ty->size) {
  case 1:
    println("get8a 1x 1x");
    if (!ty->is_unsigned && ty->kind != TY_BOOL) {
      int c = count();
      println("bign 1x 127");
      println("jmpn .L.load.sign8.%d", c);
      println("ujmpn .L.load.end8.%d", c);
      println(".L.load.sign8.%d:", c);
      println("orn 1x 0xffffff00");
      println(".L.load.end8.%d:", c);
    }
    println("setn 2x 0");
    return;
  case 2:
    println("get16a 1x 1x");
    if (!ty->is_unsigned) {
      int c = count();
      println("bign 1x 32767");
      println("jmpn .L.load.sign16.%d", c);
      println("ujmpn .L.load.end16.%d", c);
      println(".L.load.sign16.%d:", c);
      println("orn 1x 0xffff0000");
      println(".L.load.end16.%d:", c);
    }
    println("setn 2x 0");
    return;
  case 4:
    println("geta 1x 1x");
    println("setn 2x 0");
    return;
  case 8:
    println("seta 3x 1x");
    println("geta 2x 3x");
    println("addn 3x 4");
    println("geta 1x 3x");
    return;
  default:
    error("Shy backend cannot load scalar of size %d", ty->size);
  }
}

static void store(Type *ty) {
  pop32("3x");

  switch (ty->size) {
  case 1:
    println("put8a 3x 1x");
    return;
  case 2:
    println("put16a 3x 1x");
    return;
  case 4:
    println("puta 3x 1x");
    return;
  case 8:
    println("puta 3x 2x");
    println("addn 3x 4");
    println("puta 3x 1x");
    return;
  default:
    error("Shy backend cannot store scalar of size %d", ty->size);
  }
}

// Same as `store`, but the destination address is already in `addr` instead of
// being popped from the stack. `size` is the ABI size, so arrays and functions
// are stored as the pointers they decay to.
static void store_to_addr_reg(int size, char *addr) {
  switch (size) {
  case 1:
    println("put8a %s 1x", addr);
    return;
  case 2:
    println("put16a %s 1x", addr);
    return;
  case 4:
    println("puta %s 1x", addr);
    return;
  case 8:
    println("puta %s 2x", addr);
    println("addn %s 4", addr);
    println("puta %s 1x", addr);
    return;
  default:
    error("Shy backend cannot store scalar of size %d", size);
  }
}

// Byte copy: aggregate sources may be packed or subobjects with align 1/2, so
// word-sized accesses would trap on unaligned addresses.
static void copy_bytes(Type *ty) {
  if (ty->size == 0)
    return;

  int c = count();
  println("setn cx %d", ty->size);
  println(".L.copy.bytes.%d:", c);
  println("equn cx 0");
  println("jmpn .L.copy.done.%d", c);
  println("get8a dx 1x");
  println("put8a 3x dx");
  println("addn 1x 1");
  println("addn 3x 1");
  println("subn cx 1");
  println("ujmpn .L.copy.bytes.%d", c);
  println(".L.copy.done.%d:", c);
}

static void copy_struct_return_to_hidden_buffer(Node *expr) {
  Type *ty = current_fn->ty->return_ty;
  Obj *retptr = current_fn->params;
  if (!retptr || !retptr->ty || retptr->ty->kind != TY_PTR)
    error_tok(expr->tok, "missing hidden return buffer");
  if (expr->kind == ND_VAR && expr->var->is_sret_alias)
    return;

  gen_addr(expr);
  println("seta 4x 1x");
  println("setn dx %d", retptr->offset);
  println("adda dx fx");
  println("geta dx dx");
  println("seta 3x dx");
  println("seta 1x 4x");
  copy_bytes(ty);
}

static void copy_struct_assignment(Node *node) {
  gen_addr(node->lhs);
  push32("1x");
  gen_addr(node->rhs);
  pop32("3x");
  println("seta 4x 3x");
  copy_bytes(node->lhs->ty);
  println("seta 1x 4x");
  println("setn 2x 0");
}

static void cmp_zero(VInfo vi) {
  if (vinfo_is64(vi)) {
    println("ora 1x 2x");
    println("equn 1x 0");
  } else {
    println("equn 1x 0");
  }
}

static void call_helper(char *name) {
  if (!strcmp(name, "__shy_u64_div") || !strcmp(name, "__shy_u64_mod") ||
      !strcmp(name, "__shy_i64_div") || !strcmp(name, "__shy_i64_mod"))
    need_u64_divmod = true;
  if (!strcmp(name, "__shy_u64_mul") || !strcmp(name, "__shy_i64_mul"))
    need_u64_mul = true;
  if (!strcmp(name, "__shy_i64_lt") || !strcmp(name, "__shy_i64_le"))
    need_i64_cmp = true;
  println("calln %s", name);
}

static void call_helper_32_32(char *name) {
  println("seta 4x 1x");
  println("seta 5x 3x");
  call_helper(name);
}

static void call_helper_64_64(char *name) {
  println("seta 4x 1x");
  println("seta 5x 2x");
  println("seta 6x 3x");
  println("seta 7x cx");
  call_helper(name);
}

static void call_helper_32(char *name) {
  println("seta 4x 1x");
  call_helper(name);
}

static void call_helper_64(char *name) {
  println("seta 4x 1x");
  println("seta 5x 2x");
  call_helper(name);
}

static void gen_64_add(void) {
  // lhs: 1x/2x, rhs: 3x/cx. Result: 1x/2x.
  println("seta dx 1x");
  println("adda 1x 3x");
  println("smaa 1x dx");
  println("setn dx 0");
  println("jmpn .L.carry.%d", labelseq + 1);
  println("ujmpn .L.carry.end.%d", labelseq + 1);
  println(".L.carry.%d:", labelseq + 1);
  println("setn dx 1");
  println(".L.carry.end.%d:", labelseq + 1);
  labelseq++;
  println("adda 2x cx");
  println("adda 2x dx");
}

static void gen_64_sub(void) {
  // lhs: 1x/2x, rhs: 3x/cx. Result: 1x/2x.
  println("seta dx 1x");
  println("suba 1x 3x");
  println("smaa dx 3x");
  println("setn dx 0");
  println("jmpn .L.borrow.%d", labelseq + 1);
  println("ujmpn .L.borrow.end.%d", labelseq + 1);
  println(".L.borrow.%d:", labelseq + 1);
  println("setn dx 1");
  println(".L.borrow.end.%d:", labelseq + 1);
  labelseq++;
  println("suba 2x cx");
  println("suba 2x dx");
}

static void gen_64_mul(void) {
  int c = count();
  println("setn 4x 0");
  println("setn 5x 0");
  println(".L.u64.mul.loop.%d:", c);
  println("seta 6x 3x");
  println("ora 6x cx");
  println("equn 6x 0");
  println("jmpn .L.u64.mul.done.%d", c);
  println("seta 6x 3x");
  println("andn 6x 1");
  println("equn 6x 0");
  println("jmpn .L.u64.mul.skip_add.%d", c);
  println("seta 6x 4x");
  println("adda 4x 1x");
  println("smaa 4x 6x");
  println("setn 7x 0");
  println("jmpn .L.u64.mul.carry.%d", c);
  println("ujmpn .L.u64.mul.carry_end.%d", c);
  println(".L.u64.mul.carry.%d:", c);
  println("setn 7x 1");
  println(".L.u64.mul.carry_end.%d:", c);
  println("adda 5x 2x");
  println("adda 5x 7x");
  println(".L.u64.mul.skip_add.%d:", c);
  println("seta 6x 1x");
  println("rsn 6x 31");
  println("lsn 1x 1");
  println("lsn 2x 1");
  println("adda 2x 6x");
  println("seta 6x cx");
  println("andn 6x 1");
  println("lsn 6x 31");
  println("rsn cx 1");
  println("rsn 3x 1");
  println("adda 3x 6x");
  println("ujmpn .L.u64.mul.loop.%d", c);
  println(".L.u64.mul.done.%d:", c);
  println("seta 1x 4x");
  println("seta 2x 5x");
}

static void gen_64_shl(void) {
  // lhs: 1x/2x, rhs shift count: 3x/cx. Result: 1x/2x.
  int c = count();

  println("equn cx 0");
  println("jmpn .L.u64.shl.check.%d", c);
  println("setn 1x 0");
  println("setn 2x 0");
  println("ujmpn .L.u64.shl.done.%d", c);

  println(".L.u64.shl.check.%d:", c);
  println("smaequn 3x 63");
  println("jmpn .L.u64.shl.loop.%d", c);
  println("setn 1x 0");
  println("setn 2x 0");
  println("ujmpn .L.u64.shl.done.%d", c);

  println(".L.u64.shl.loop.%d:", c);
  println("equn 3x 0");
  println("jmpn .L.u64.shl.done.%d", c);
  println("seta dx 1x");
  println("rsn dx 31");
  println("lsn 1x 1");
  println("lsn 2x 1");
  println("ora 2x dx");
  println("subn 3x 1");
  println("ujmpn .L.u64.shl.loop.%d", c);
  println(".L.u64.shl.done.%d:", c);
}

static void gen_64_shr(bool is_unsigned) {
  // lhs: 1x/2x, rhs shift count: 3x/cx. Result: 1x/2x.
  int c = count();

  println("equn cx 0");
  println("jmpn .L.u64.shr.check.%d", c);
  if (is_unsigned) {
    println("setn 1x 0");
    println("setn 2x 0");
  } else {
    println("seta dx 2x");
    println("rsn dx 31");
    println("equn dx 0");
    println("jmpn .L.u64.shr.big_pos.%d", c);
    println("setn 1x 0xffffffff");
    println("setn 2x 0xffffffff");
    println("ujmpn .L.u64.shr.done.%d", c);
    println(".L.u64.shr.big_pos.%d:", c);
    println("setn 1x 0");
    println("setn 2x 0");
  }
  println("ujmpn .L.u64.shr.done.%d", c);

  println(".L.u64.shr.check.%d:", c);
  println("smaequn 3x 63");
  println("jmpn .L.u64.shr.loop.%d", c);
  if (is_unsigned) {
    println("setn 1x 0");
    println("setn 2x 0");
  } else {
    println("seta dx 2x");
    println("rsn dx 31");
    println("equn dx 0");
    println("jmpn .L.u64.shr.large_pos.%d", c);
    println("setn 1x 0xffffffff");
    println("setn 2x 0xffffffff");
    println("ujmpn .L.u64.shr.done.%d", c);
    println(".L.u64.shr.large_pos.%d:", c);
    println("setn 1x 0");
    println("setn 2x 0");
  }
  println("ujmpn .L.u64.shr.done.%d", c);

  println(".L.u64.shr.loop.%d:", c);
  println("equn 3x 0");
  println("jmpn .L.u64.shr.done.%d", c);
  println("seta dx 2x");
  println("andn dx 1");
  println("lsn dx 31");
  if (!is_unsigned) {
    println("seta 4x 2x");
    println("andn 4x 0x80000000");
  }
  println("rsn 2x 1");
  if (!is_unsigned) {
    println("equn 4x 0");
    println("jmpn .L.u64.shr.sign_done.%d", c);
    println("setn 4x 0x80000000");
    println("ora 2x 4x");
    println(".L.u64.shr.sign_done.%d:", c);
  }
  println("rsn 1x 1");
  println("ora 1x dx");
  println("subn 3x 1");
  println("ujmpn .L.u64.shr.loop.%d", c);
  println(".L.u64.shr.done.%d:", c);
}

static void gen_binary(Node *node) {
  VInfo lvi = vinfo(node->lhs->ty);
  VInfo rvi = vinfo(node->rhs->ty);
  VInfo vi = vinfo(node->ty);

  gen_expr(node->rhs);
  push_value(rvi);
  gen_expr(node->lhs);
  pop_value(rvi, "3x", "cx");

  if (is_shy_flonum(node->lhs->ty) || is_shy_flonum(node->rhs->ty)) {
    char *prefix = node->lhs->ty->kind == TY_FLOAT ? "__shy_f32" : "__shy_f64";
    switch (node->kind) {
    case ND_ADD:
      if (node->lhs->ty->kind == TY_FLOAT)
        call_helper_32_32(format("%s_add", prefix));
      else
        call_helper_64_64(format("%s_add", prefix));
      return;
    case ND_SUB:
      if (node->lhs->ty->kind == TY_FLOAT)
        call_helper_32_32(format("%s_sub", prefix));
      else
        call_helper_64_64(format("%s_sub", prefix));
      return;
    case ND_MUL:
      if (node->lhs->ty->kind == TY_FLOAT)
        call_helper_32_32(format("%s_mul", prefix));
      else
        call_helper_64_64(format("%s_mul", prefix));
      return;
    case ND_DIV:
      if (node->lhs->ty->kind == TY_FLOAT)
        call_helper_32_32(format("%s_div", prefix));
      else
        call_helper_64_64(format("%s_div", prefix));
      return;
    case ND_EQ:
      if (node->lhs->ty->kind == TY_FLOAT)
        call_helper_32_32(format("%s_eq", prefix));
      else
        call_helper_64_64(format("%s_eq", prefix));
      return;
    case ND_NE:
      if (node->lhs->ty->kind == TY_FLOAT)
        call_helper_32_32(format("%s_ne", prefix));
      else
        call_helper_64_64(format("%s_ne", prefix));
      return;
    case ND_LT:
      if (node->lhs->ty->kind == TY_FLOAT)
        call_helper_32_32(format("%s_lt", prefix));
      else
        call_helper_64_64(format("%s_lt", prefix));
      return;
    case ND_LE:
      if (node->lhs->ty->kind == TY_FLOAT)
        call_helper_32_32(format("%s_le", prefix));
      else
        call_helper_64_64(format("%s_le", prefix));
      return;
    default:
      unsupported(node, "floating-point operator");
    }
  }

  if (vinfo_is64(lvi) || vinfo_is64(rvi) || vinfo_is64(vi)) {
    if (!vinfo_is64(lvi))
      println("setn 2x 0");
    if (!vinfo_is64(rvi))
      println("setn cx 0");

    switch (node->kind) {
    case ND_ADD:
      gen_64_add();
      return;
    case ND_SUB:
      gen_64_sub();
      return;
    case ND_MUL:
      gen_64_mul();
      return;
    case ND_DIV:
      call_helper_64_64(node->ty->is_unsigned ? "__shy_u64_div" : "__shy_i64_div");
      return;
    case ND_MOD:
      call_helper_64_64(node->ty->is_unsigned ? "__shy_u64_mod" : "__shy_i64_mod");
      return;
    case ND_BITAND:
      println("anda 1x 3x");
      println("anda 2x cx");
      return;
    case ND_BITOR:
      println("ora 1x 3x");
      println("ora 2x cx");
      return;
    case ND_BITXOR:
      println("xora 1x 3x");
      println("xora 2x cx");
      return;
    case ND_SHL:
      gen_64_shl();
      return;
    case ND_SHR:
      gen_64_shr(node->ty->is_unsigned);
      return;
    case ND_EQ: {
      println("xora 1x 3x");
      println("xora 2x cx");
      println("ora 1x 2x");
      println("equn 1x 0");
      set_bool_from_rs();
      return;
    }
    case ND_NE: {
      println("xora 1x 3x");
      println("xora 2x cx");
      println("ora 1x 2x");
      println("equn 1x 0");
      int c = count();
      println("setn 1x 1");
      println("jmpn .L.ne.false.%d", c);
      println("ujmpn .L.ne.end.%d", c);
      println(".L.ne.false.%d:", c);
      println("setn 1x 0");
      println(".L.ne.end.%d:", c);
      println("setn 2x 0");
      return;
    }
    case ND_LT:
    case ND_LE:
      call_helper_64_64(node->kind == ND_LT ? "__shy_i64_lt" : "__shy_i64_le");
      return;
    default:
      unsupported(node, "64-bit operator");
    }
  }

  switch (node->kind) {
  case ND_ADD:
    println("adda 1x 3x");
    return;
  case ND_SUB:
    println("suba 1x 3x");
    return;
  case ND_MUL:
    println("mula 1x 3x");
    return;
  case ND_DIV:
    println("diva 1x 3x");
    return;
  case ND_MOD:
    println("seta dx 1x");
    println("diva 1x 3x");
    println("mula 1x 3x");
    println("suba dx 1x");
    println("seta 1x dx");
    return;
  case ND_BITAND:
    println("anda 1x 3x");
    return;
  case ND_BITOR:
    println("ora 1x 3x");
    return;
  case ND_BITXOR:
    println("xora 1x 3x");
    return;
  case ND_EQ:
    println("equa 1x 3x");
    set_bool_from_rs();
    return;
  case ND_NE: {
    int c = count();
    println("equa 1x 3x");
    println("setn 1x 1");
    println("jmpn .L.ne.false.%d", c);
    println("ujmpn .L.ne.end.%d", c);
    println(".L.ne.false.%d:", c);
    println("setn 1x 0");
    println(".L.ne.end.%d:", c);
    println("setn 2x 0");
    return;
  }
  case ND_LT:
    println("smaa 1x 3x");
    set_bool_from_rs();
    return;
  case ND_LE:
    println("smaequa 1x 3x");
    set_bool_from_rs();
    return;
  case ND_SHL:
    println("lsa 1x 3x");
    return;
  case ND_SHR:
    println("rsa 1x 3x");
    return;
  default:
    unsupported(node, "binary operator");
  }
}

// ABI v1 argument placement: scalars use the next free register slot when the
// whole value fits, everything else goes on the stack. Aggregates never use
// argument registers and never split between registers and memory.
static int arg_type_size(Type *ty) {
  // Arrays and functions are passed as pointers.
  if (ty->kind == TY_ARRAY || ty->kind == TY_FUNC)
    return 4;
  return ty->size;
}

static bool pass_by_memory(Type *ty) {
  return is_aggregate(ty) || arg_type_size(ty) > 8;
}

static int stack_block_size(Type *ty) {
  return align_to(arg_type_size(ty), 4);
}

static int arg_slots(Type *ty) {
  return arg_type_size(ty) > 4 ? 2 : 1;
}

static bool arg_fits_register(Type *ty, int slot) {
  return !pass_by_memory(ty) && slot + arg_slots(ty) <= argreg_len;
}

// Stack bytes the caller must reserve, plus the number of argument registers
// that end up used (including the hidden aggregate-result pointer).
static int classify_call_args(Node *node, int *reg_slots) {
  int slot = node->ret_buffer ? 1 : 0;
  int bytes = 0;
  for (Node *arg = node->args; arg; arg = arg->next) {
    if (!is_aggregate(arg->ty) && arg_type_size(arg->ty) > 8)
      unsupported(arg, "arguments wider than 8 bytes");
    if (arg_fits_register(arg->ty, slot))
      slot += arg_slots(arg->ty);
    else
      bytes += stack_block_size(arg->ty);
  }
  *reg_slots = slot;
  return bytes;
}

// Register slot assigned to `target`, or -1 when it is passed on the stack.
static int arg_register_slot(Node *node, Node *target) {
  int slot = node->ret_buffer ? 1 : 0;
  for (Node *arg = node->args; arg; arg = arg->next) {
    if (arg_fits_register(arg->ty, slot)) {
      if (arg == target)
        return slot;
      slot += arg_slots(arg->ty);
    } else if (arg == target) {
      return -1;
    }
  }
  return -1;
}

// Cumulative padded size of the stack arguments through `target`.
static int stack_bytes_through(Node *node, Node *target) {
  int slot = node->ret_buffer ? 1 : 0;
  int bytes = 0;
  for (Node *arg = node->args; arg; arg = arg->next) {
    if (arg_fits_register(arg->ty, slot))
      slot += arg_slots(arg->ty);
    else
      bytes += stack_block_size(arg->ty);
    if (arg == target)
      break;
  }
  return bytes;
}

// Bytes already pushed by the arguments that are evaluated before `target`,
// which are the ones later in source order because calls evaluate backwards.
static int pushed_bytes_after(Node *node, Node *target) {
  int slot = node->ret_buffer ? 1 : 0;
  int bytes = 0;
  bool after = false;
  for (Node *arg = node->args; arg; arg = arg->next) {
    bool in_reg = arg_fits_register(arg->ty, slot);
    if (in_reg)
      slot += arg_slots(arg->ty);
    if (after && in_reg)
      bytes += 4 * arg_slots(arg->ty);
    if (arg == target)
      after = true;
  }
  return bytes;
}

// Evaluates the arguments right to left. Register arguments are pushed;
// stack arguments are copied straight into the call area, which sits below
// `sp` once the caller has reserved it. Because the later arguments are
// already pushed when a stack argument is written, its block is
// `pushed + own stack offset` bytes below the current `sp`.
static void gen_call_args(Node *node, Node *arg) {
  if (!arg)
    return;
  gen_call_args(node, arg->next);

  if (arg_register_slot(node, arg) >= 0) {
    gen_expr(arg);
    push_value(vinfo(arg->ty));
    return;
  }

  int below = pushed_bytes_after(node, arg) + stack_bytes_through(node, arg);
  // An aggregate expression denotes storage, so take its address; everything
  // else is evaluated for its value.
  if (is_aggregate(arg->ty))
    gen_addr(arg);
  else
    gen_expr(arg);
  println("seta 3x sp");
  println("subn 3x %d", below);
  if (is_aggregate(arg->ty))
    copy_bytes(arg->ty);
  else
    store_to_addr_reg(arg_type_size(arg->ty), "3x");
}

static void gen_expr(Node *node) {
  switch (node->kind) {
  case ND_NULL_EXPR:
    return;
  case ND_NUM:
    if (node->ty->kind == TY_FLOAT) {
      union { float f32; uint32_t u32; } u = { node->fval };
      println("setn 1x %u", u.u32);
      println("setn 2x 0");
    } else if (node->ty->kind == TY_DOUBLE) {
      union { double f64; uint64_t u64; } u = { node->fval };
      println("setn 1x %u", (uint32_t)u.u64);
      println("setn 2x %u", (uint32_t)(u.u64 >> 32));
    } else {
      println("setn 1x %u", (uint32_t)node->val);
      println("setn 2x %u", (uint32_t)((uint64_t)node->val >> 32));
    }
    return;
  case ND_NEG:
    gen_expr(node->lhs);
    if (node->ty->kind == TY_FLOAT) {
      println("xorn 1x 0x80000000");
      return;
    }
    if (node->ty->kind == TY_DOUBLE) {
      println("xorn 2x 0x80000000");
      return;
    }
    if (vinfo_is64(vinfo(node->ty))) {
      println("nota 1x");
      println("nota 2x");
      println("addn 1x 1");
      println("equn 1x 0");
      println("jmpn .L.neg.carry.%d", labelseq + 1);
      println("ujmpn .L.neg.end.%d", labelseq + 1);
      println(".L.neg.carry.%d:", labelseq + 1);
      println("addn 2x 1");
      println(".L.neg.end.%d:", labelseq + 1);
      labelseq++;
    } else {
      println("setn 3x 0");
      println("suba 3x 1x");
      println("seta 1x 3x");
      println("setn 2x 0");
    }
    return;
  case ND_VAR:
    gen_addr(node);
    load(node->ty);
    return;
  case ND_MEMBER:
    gen_addr(node);
    if (node->ty->kind == TY_ARRAY)
      return;
    load(node->ty);
    return;
  case ND_DEREF:
    gen_expr(node->lhs);
    load(node->ty);
    return;
  case ND_ADDR:
    gen_addr(node->lhs);
    return;
  case ND_ASSIGN:
    if (node->lhs->ty->kind == TY_STRUCT || node->lhs->ty->kind == TY_UNION) {
      copy_struct_assignment(node);
      return;
    }
    gen_addr(node->lhs);
    push32("1x");
    gen_expr(node->rhs);
    store(node->lhs->ty);
    return;
  case ND_COMMA:
    gen_expr(node->lhs);
    gen_expr(node->rhs);
    return;
  case ND_CAST:
    gen_expr(node->lhs);
    if (is_shy_flonum(node->lhs->ty) || is_shy_flonum(node->ty)) {
      if (node->lhs->ty->kind == TY_FLOAT && node->ty->kind == TY_DOUBLE)
        call_helper_32("__shy_f32_to_f64");
      else if (node->lhs->ty->kind == TY_DOUBLE && node->ty->kind == TY_FLOAT)
        call_helper_64("__shy_f64_to_f32");
      else if (node->lhs->ty->kind == TY_FLOAT && is_integer(node->ty))
        call_helper_32(node->ty->is_unsigned ? "__shy_f32_to_u64" : "__shy_f32_to_i64");
      else if (node->lhs->ty->kind == TY_DOUBLE && is_integer(node->ty))
        call_helper_64(node->ty->is_unsigned ? "__shy_f64_to_u64" : "__shy_f64_to_i64");
      else if (is_integer(node->lhs->ty) && node->ty->kind == TY_FLOAT)
        call_helper_64(node->lhs->ty->is_unsigned ? "__shy_u64_to_f32" : "__shy_i64_to_f32");
      else if (is_integer(node->lhs->ty) && node->ty->kind == TY_DOUBLE)
        call_helper_64(node->lhs->ty->is_unsigned ? "__shy_u64_to_f64" : "__shy_i64_to_f64");
    } else if (vinfo_is64(vinfo(node->ty)) && !vinfo_is64(vinfo(node->lhs->ty))) {
      if (node->lhs->ty->is_unsigned) {
        println("setn 2x 0");
      } else {
        int c = count();
        println("seta 2x 1x");
        println("rsn 2x 31");
        println("equn 2x 0");
        println("jmpn .L.cast.pos.%d", c);
        println("setn 2x 0xffffffff");
        println("ujmpn .L.cast.end.%d", c);
        println(".L.cast.pos.%d:", c);
        println("setn 2x 0");
        println(".L.cast.end.%d:", c);
      }
    } else if (!vinfo_is64(vinfo(node->ty))) {
      println("setn 2x 0");
    }
    return;
  case ND_MEMZERO:
    for (int off = 0; off < node->var->ty->size; off += 4) {
      println("setn 3x %d", node->var->offset + off);
      println("adda 3x fx");
      println("putn 3x 0");
    }
    println("setn 1x 0");
    println("setn 2x 0");
    return;
  case ND_NOT:
    gen_expr(node->lhs);
    cmp_zero(vinfo(node->lhs->ty));
    set_bool_from_rs();
    return;
  case ND_BITNOT:
    gen_expr(node->lhs);
    println("nota 1x");
    if (vinfo_is64(vinfo(node->ty)))
      println("nota 2x");
    return;
  case ND_LOGAND: {
    int c = count();
    gen_expr(node->lhs);
    cmp_zero(vinfo(node->lhs->ty));
    println("jmpn .L.false.%d", c);
    gen_expr(node->rhs);
    cmp_zero(vinfo(node->rhs->ty));
    println("jmpn .L.false.%d", c);
    println("setn 1x 1");
    println("ujmpn .L.end.%d", c);
    println(".L.false.%d:", c);
    println("setn 1x 0");
    println(".L.end.%d:", c);
    println("setn 2x 0");
    return;
  }
  case ND_LOGOR: {
    int c = count();
    gen_expr(node->lhs);
    cmp_zero(vinfo(node->lhs->ty));
    println("jmpn .L.rhs.%d", c);
    println("setn 1x 1");
    println("ujmpn .L.end.%d", c);
    println(".L.rhs.%d:", c);
    gen_expr(node->rhs);
    cmp_zero(vinfo(node->rhs->ty));
    println("jmpn .L.false.%d", c);
    println("setn 1x 1");
    println("ujmpn .L.end.%d", c);
    println(".L.false.%d:", c);
    println("setn 1x 0");
    println(".L.end.%d:", c);
    println("setn 2x 0");
    return;
  }
  case ND_COND: {
    int c = count();
    gen_expr(node->cond);
    cmp_zero(vinfo(node->cond->ty));
    println("jmpn .L.else.%d", c);
    gen_expr(node->then);
    println("ujmpn .L.end.%d", c);
    println(".L.else.%d:", c);
    gen_expr(node->els);
    println(".L.end.%d:", c);
    return;
  }
  case ND_FUNCALL: {
    if (node->lhs->kind != ND_VAR)
      unsupported(node, "indirect function call");

    int reg_slots;
    int stack_bytes = classify_call_args(node, &reg_slots);

    // The stack argument area sits above the caller's locals and below the
    // call's sp, so reserve it before evaluating anything. Nested calls in
    // the arguments allocate above this area and cannot overwrite it.
    if (stack_bytes)
      println("addn sp %d", stack_bytes);

    gen_call_args(node, node->args);

    // The hidden aggregate-result pointer occupies 4x, so push it last.
    if (node->ret_buffer) {
      gen_var_addr(node->ret_buffer);
      push_value(vinfo(pointer_to(node->ty)));
    }

    for (int i = 0; i < reg_slots; i++)
      pop32(argreg[i]);

    println("calln %s", symbol_operand(node->lhs->var->name));

    if (stack_bytes)
      println("subn sp %d", stack_bytes);

    // Like every other aggregate expression, an aggregate call denotes its
    // result storage. The hidden buffer is a stable local of this function, so
    // its address can be rebuilt after the call.
    if (node->ret_buffer)
      gen_var_addr(node->ret_buffer);
    return;
  }
  case ND_EXCH:
    if (node->ty->size != 4)
      unsupported(node, "atomic exchange wider than 32 bits");
    gen_expr(node->lhs);
    push32("1x");
    gen_expr(node->rhs);
    pop32("3x");
    println("atoma 3x 1x");
    println("setn 2x 0");
    return;
  case ND_CAS:
    unsupported(node, "atomic compare-and-swap without a ShyISA CAS primitive");
  case ND_ADD:
  case ND_SUB:
  case ND_MUL:
  case ND_DIV:
  case ND_MOD:
  case ND_BITAND:
  case ND_BITOR:
  case ND_BITXOR:
  case ND_EQ:
  case ND_NE:
  case ND_LT:
  case ND_LE:
  case ND_SHL:
  case ND_SHR:
    gen_binary(node);
    return;
  case ND_STMT_EXPR:
    // Run the statements in order. The last one is an expression statement,
    // so its value (or aggregate address) is what the block yields.
    for (Node *n = node->body; n; n = n->next)
      gen_stmt(n);
    return;
  default:
    unsupported(node, "expression");
  }
}

static void gen_stmt(Node *node) {
  switch (node->kind) {
  case ND_IF: {
    int c = count();
    gen_expr(node->cond);
    cmp_zero(vinfo(node->cond->ty));
    println("jmpn .L.else.%d", c);
    gen_stmt(node->then);
    println("ujmpn .L.end.%d", c);
    println(".L.else.%d:", c);
    if (node->els)
      gen_stmt(node->els);
    println(".L.end.%d:", c);
    return;
  }
  case ND_FOR: {
    int c = count();
    if (node->init)
      gen_stmt(node->init);
    println(".L.begin.%d:", c);
    if (node->cond) {
      gen_expr(node->cond);
      cmp_zero(vinfo(node->cond->ty));
      println("jmpn %s", node->brk_label);
    }
    gen_stmt(node->then);
    println("%s:", node->cont_label);
    if (node->inc)
      gen_expr(node->inc);
    println("ujmpn .L.begin.%d", c);
    println("%s:", node->brk_label);
    return;
  }
  case ND_DO: {
    int c = count();
    println(".L.begin.%d:", c);
    gen_stmt(node->then);
    println("%s:", node->cont_label);
    gen_expr(node->cond);
    cmp_zero(vinfo(node->cond->ty));
    println("jmpn %s", node->brk_label);
    println("ujmpn .L.begin.%d", c);
    println("%s:", node->brk_label);
    return;
  }
  case ND_SWITCH:
    gen_expr(node->cond);
    if (vinfo_is64(vinfo(node->cond->ty)))
      println("seta 1x 1x");

    for (Node *n = node->case_next; n; n = n->case_next) {
      if (n->begin == n->end) {
        println("equn 1x %u", (uint32_t)n->begin);
        println("jmpn %s", n->label);
      } else {
        println("seta 3x 1x");
        println("subn 3x %u", (uint32_t)n->begin);
        println("smaequn 3x %u", (uint32_t)(n->end - n->begin));
        println("jmpn %s", n->label);
      }
    }

    if (node->default_case)
      println("ujmpn %s", node->default_case->label);
    println("ujmpn %s", node->brk_label);
    gen_stmt(node->then);
    println("%s:", node->brk_label);
    return;
  case ND_CASE:
    println("%s:", node->label);
    gen_stmt(node->lhs);
    return;
  case ND_BLOCK:
    for (Node *n = node->body; n; n = n->next)
      gen_stmt(n);
    return;
  case ND_GOTO:
    println("ujmpn %s", node->unique_label);
    return;
  case ND_LABEL:
    println("%s:", node->unique_label);
    gen_stmt(node->lhs);
    return;
  case ND_RETURN:
    if (node->lhs) {
      // ABI v1: every struct/union result goes through the hidden pointer.
      if (is_aggregate(node->lhs->ty))
        copy_struct_return_to_hidden_buffer(node->lhs);
      else
        gen_expr(node->lhs);
    }
    println("ujmpn .L.return.%s", current_fn->name);
    return;
  case ND_EXPR_STMT:
    gen_expr(node->lhs);
    return;
  case ND_ASM:
    if (!node->asm_bindings) {
      println("%s", node->asm_str);
      return;
    }

    for (AsmBinding *binding = node->asm_bindings; binding; binding = binding->next) {
      if (binding->var->ty->size > 4)
        error_tok(node->tok, "asm! binding supports only <=32-bit scalar variables for now");
      if (asm_uses_addr(node, binding)) {
        gen_var_addr(binding->var);
      } else {
        gen_var_addr(binding->var);
        load(binding->var->ty);
      }
      println("seta %s 1x", binding->reg);
    }

    for (char *p = node->asm_str; *p;) {
      if (*p != '{') {
        fputc(*p++, output_file);
        continue;
      }

      char *q = strchr(p, '}');
      if (!q) {
        fputc(*p++, output_file);
        continue;
      }

      bool wants_addr = p[1] == '&';
      char *name = wants_addr ? strndup(p + 2, q - p - 2) : strndup(p + 1, q - p - 1);
      char *reg = NULL;
      for (AsmBinding *binding = node->asm_bindings; binding; binding = binding->next) {
        if (!strcmp(binding->name, name)) {
          reg = binding->reg;
          break;
        }
      }

      if (reg)
        fputs(reg, output_file);
      else
        fprintf(output_file, wants_addr ? "{&%s}" : "{%s}", name);
      p = q + 1;
    }
    fputc('\n', output_file);

    for (AsmBinding *binding = node->asm_bindings; binding; binding = binding->next) {
      if (asm_uses_addr(node, binding))
        continue;
      gen_var_addr(binding->var);
      push32("1x");
      println("seta 1x %s", binding->reg);
      store(binding->var->ty);
    }
    return;
  default:
    unsupported(node, "statement");
  }
}

static bool has_main(Obj *prog) {
  for (Obj *fn = prog; fn; fn = fn->next)
    if (fn->is_function && fn->is_definition && !strcmp(fn->name, "main"))
      return true;
  return false;
}

static void copy_init_bytes(Type *ty, char *src, uint8_t *dst, int off) {
  if (!src)
    return;

  switch (ty->kind) {
  case TY_BOOL:
  case TY_CHAR:
    dst[off] = src[off];
    return;
  case TY_SHORT:
    for (int i = 0; i < 2; i++)
      dst[off + i] = src[off + 1 - i];
    return;
  case TY_INT:
  case TY_ENUM:
  case TY_FLOAT:
  case TY_PTR:
    for (int i = 0; i < 4; i++)
      dst[off + i] = src[off + 3 - i];
    return;
  case TY_LONG:
  case TY_DOUBLE:
    for (int i = 0; i < 8; i++)
      dst[off + i] = src[off + 7 - i];
    return;
  case TY_ARRAY:
    for (int i = 0; i < ty->array_len; i++)
      copy_init_bytes(ty->base, src, dst, off + i * ty->base->size);
    return;
  case TY_STRUCT:
    for (Member *mem = ty->members; mem; mem = mem->next)
      copy_init_bytes(mem->ty, src, dst, off + mem->offset);
    return;
  default:
    for (int i = 0; i < ty->size; i++)
      dst[off + i] = src[off + i];
  }
}

static void emit_byte_array(char *name, int off, uint8_t *bytes, int len) {
  fprintf(output_file, "%s(%d) [", name, off);
  for (int i = 0; i < len; i++) {
    if (i)
      fprintf(output_file, ",");
    fprintf(output_file, "%u", bytes[off + i]);
  }
  fprintf(output_file, "]\n");
}

static void emit_data(Obj *prog) {
  for (Obj *var = prog; var; var = var->next) {
    if (var->is_function || !var->is_definition)
      continue;
    if (var->is_tls)
      error_tok(var->tok, "Shy backend does not support TLS");

    println(".section data.%s", var->name);
    println(".symbol %s", var->name);

    uint8_t *bytes = calloc(1, var->ty->size);
    copy_init_bytes(var->ty, var->init_data, bytes, 0);
    for (int off = 0; off < var->ty->size; off += 32)
      emit_byte_array(var->name, off, bytes, MIN(32, var->ty->size - off));

    for (Relocation *rel = var->rel; rel; rel = rel->next) {
      if (rel->offset < 0 || rel->offset + 4 > var->ty->size)
        error_tok(var->tok, "Shy backend cannot emit out-of-range relocation");
      if (rel->addend < 0 || rel->addend > UINT32_MAX)
        error_tok(var->tok, "Shy backend supports only non-negative 32-bit relocation addends");

      if (rel->addend)
        println("%s(%d) %s(%ld)", var->name, rel->offset, *rel->label, rel->addend);
      else
        println("%s(%d) %s", var->name, rel->offset, *rel->label);
    }
  }
}

static void emit_start(Obj *prog) {
  if (opt_shy_no_main)
    return;

  if (!has_main(prog))
    return;

  println(".section text._start");
  println(".symbol _start");
  println("setn sp 0x00fff000");
  println("calln main");
  println("seta exit 1x");
}

static void emit_text(Obj *prog) {
  emit_start(prog);

  for (Obj *fn = prog; fn; fn = fn->next) {
    if (!fn->is_function || !fn->is_definition || !fn->is_live)
      continue;
    bool bare_start = opt_shy_no_main && !strcmp(fn->name, "_start");
    println(".section text.%s", fn->name);
    println(".symbol %s", fn->name);
    current_fn = fn;

    if (!bare_start) {
      println("pusha fx");
      println("seta fx sp");
      if (fn->stack_size)
        println("addn sp %d", fn->stack_size);
    }

    // Variadic state, ABI v1. __va_area__ holds the next free register slot at
    // +0, a pointer to the saved slots at +4, a descending stack cursor at +8,
    // and the eight argument registers at +12. The registers are saved before
    // the parameter copies so nothing can clobber them first.
    if (fn->va_area && !bare_start) {
      for (int i = 0; i < argreg_len; i++) {
        println("setn 3x %d", fn->va_area->offset + 12 + i * 4);
        println("adda 3x fx");
        println("puta 3x %s", argreg[i]);
      }
    }

    int slot = 0;
    int stack_used = 0;
    for (Obj *var = fn->params; var; var = var->next) {
      if (bare_start)
        error_tok(var->tok, "Shy bare _start cannot have parameters");

      if (!pass_by_memory(var->ty) && slot + arg_slots(var->ty) <= argreg_len) {
        println("setn 3x %d", var->offset);
        println("adda 3x fx");
        switch (arg_type_size(var->ty)) {
        case 1:
          println("put8a 3x %s", argreg[slot]);
          break;
        case 2:
          println("put16a 3x %s", argreg[slot]);
          break;
        case 4:
          println("puta 3x %s", argreg[slot]);
          break;
        case 8:
          // Register slots are low-word-first; memory is high-word-first.
          println("puta 3x %s", argreg[slot + 1]);
          println("addn 3x 4");
          println("puta 3x %s", argreg[slot]);
          break;
        default:
          error_tok(var->tok, "unsupported parameter size");
        }
        slot += arg_slots(var->ty);
        continue;
      }

      // Stack parameters are copied out of their block below the return
      // address at fx-8, in the same order the caller laid them down.
      stack_used += stack_block_size(var->ty);
      println("seta 1x fx");
      println("subn 1x %d", 8 + stack_used);
      println("setn 3x %d", var->offset);
      println("adda 3x fx");
      copy_bytes(var->ty);
    }

    if (fn->va_area && !bare_start) {
      // +0: first register slot a variadic argument may use.
      println("setn 3x %d", fn->va_area->offset);
      println("adda 3x fx");
      println("setn 4x %d", slot);
      println("puta 3x 4x");

      // +4: address of the saved register area.
      println("setn 3x %d", fn->va_area->offset + 4);
      println("adda 3x fx");
      println("setn 4x %d", fn->va_area->offset + 12);
      println("adda 4x fx");
      println("puta 3x 4x");

      // +8: cursor just below the named stack arguments.
      println("setn 3x %d", fn->va_area->offset + 8);
      println("adda 3x fx");
      println("seta 4x fx");
      println("subn 4x %d", 8 + stack_used);
      println("puta 3x 4x");
    }

    gen_stmt(fn->body);

    if (!strcmp(fn->name, "main")) {
      println("setn 1x 0");
      println("setn 2x 0");
    }

    println(".L.return.%s:", fn->name);
    if (bare_start) {
      println("ujmpn .L.return.%s", fn->name);
      continue;
    }
    println("seta sp fx");
    println("popa fx");
    println("ret");
  }
}

static void emit_u64_divmod_runtime(void) {
  if (!need_u64_divmod)
    return;

  println(".section text.__shy_u64_div");
  println(".symbol __shy_u64_div");
  println("calln __shy_u64_divmod");
  println("ret");

  println(".section text.__shy_u64_mod");
  println(".symbol __shy_u64_mod");
  println("calln __shy_u64_divmod");
  println("seta 1x 6x");
  println("seta 2x 7x");
  println("ret");

  println(".section text.__shy_i64_div");
  println(".symbol __shy_i64_div");
  println("seta 8x 2x");
  println("rsn 8x 31");
  println("seta 9x cx");
  println("rsn 9x 31");
  println("xora 8x 9x");
  println("pusha 8x");
  println("seta 9x 2x");
  println("rsn 9x 31");
  println("equn 9x 0");
  println("jmpn .L.i64.div.lhs_abs");
  println("nota 1x");
  println("nota 2x");
  println("addn 1x 1");
  println("equn 1x 0");
  println("jmpn .L.i64.div.lhs_carry");
  println("ujmpn .L.i64.div.lhs_abs");
  println(".L.i64.div.lhs_carry:");
  println("addn 2x 1");
  println(".L.i64.div.lhs_abs:");
  println("seta 9x cx");
  println("rsn 9x 31");
  println("equn 9x 0");
  println("jmpn .L.i64.div.rhs_abs");
  println("nota 3x");
  println("nota cx");
  println("addn 3x 1");
  println("equn 3x 0");
  println("jmpn .L.i64.div.rhs_carry");
  println("ujmpn .L.i64.div.rhs_abs");
  println(".L.i64.div.rhs_carry:");
  println("addn cx 1");
  println(".L.i64.div.rhs_abs:");
  println("calln __shy_u64_div");
  println("popa 8x");
  println("equn 8x 0");
  println("jmpn .L.i64.div.done");
  println("nota 1x");
  println("nota 2x");
  println("addn 1x 1");
  println("equn 1x 0");
  println("jmpn .L.i64.div.res_carry");
  println("ujmpn .L.i64.div.done");
  println(".L.i64.div.res_carry:");
  println("addn 2x 1");
  println(".L.i64.div.done:");
  println("ret");

  println(".section text.__shy_i64_mod");
  println(".symbol __shy_i64_mod");
  println("seta 8x 2x");
  println("rsn 8x 31");
  println("pusha 8x");
  println("equn 8x 0");
  println("jmpn .L.i64.mod.lhs_abs");
  println("nota 1x");
  println("nota 2x");
  println("addn 1x 1");
  println("equn 1x 0");
  println("jmpn .L.i64.mod.lhs_carry");
  println("ujmpn .L.i64.mod.lhs_abs");
  println(".L.i64.mod.lhs_carry:");
  println("addn 2x 1");
  println(".L.i64.mod.lhs_abs:");
  println("seta 9x cx");
  println("rsn 9x 31");
  println("equn 9x 0");
  println("jmpn .L.i64.mod.rhs_abs");
  println("nota 3x");
  println("nota cx");
  println("addn 3x 1");
  println("equn 3x 0");
  println("jmpn .L.i64.mod.rhs_carry");
  println("ujmpn .L.i64.mod.rhs_abs");
  println(".L.i64.mod.rhs_carry:");
  println("addn cx 1");
  println(".L.i64.mod.rhs_abs:");
  println("calln __shy_u64_mod");
  println("popa 8x");
  println("equn 8x 0");
  println("jmpn .L.i64.mod.done");
  println("nota 1x");
  println("nota 2x");
  println("addn 1x 1");
  println("equn 1x 0");
  println("jmpn .L.i64.mod.res_carry");
  println("ujmpn .L.i64.mod.done");
  println(".L.i64.mod.res_carry:");
  println("addn 2x 1");
  println(".L.i64.mod.done:");
  println("ret");

  println(".section text.__shy_u64_divmod");
  println(".symbol __shy_u64_divmod");
  println("seta bx 1x");
  println("ora bx 2x");
  println("seta dx 3x");
  println("ora dx cx");
  println("equn dx 0");
  println("jmpn .L.u64.divzero");
  println("setn 4x 0");
  println("setn 5x 0");
  println("setn 6x 0");
  println("setn 7x 0");
  println("setn 8x 64");
  println(".L.u64.loop:");
  println("seta 9x 4x");
  println("rsn 9x 31");
  println("lsn 4x 1");
  println("lsn 5x 1");
  println("adda 5x 9x");
  println("seta 9x 6x");
  println("rsn 9x 31");
  println("lsn 6x 1");
  println("lsn 7x 1");
  println("adda 7x 9x");
  println("seta 9x 2x");
  println("rsn 9x 31");
  println("adda 6x 9x");
  println("seta 9x 1x");
  println("rsn 9x 31");
  println("lsn 1x 1");
  println("lsn 2x 1");
  println("adda 2x 9x");
  println("biga 7x cx");
  println("jmpn .L.u64.sub");
  println("smaa 7x cx");
  println("jmpn .L.u64.skip");
  println("bigequa 6x 3x");
  println("jmpn .L.u64.sub");
  println("ujmpn .L.u64.skip");
  println(".L.u64.sub:");
  println("seta 9x 6x");
  println("suba 6x 3x");
  println("smaa 9x 3x");
  println("setn ax 0");
  println("jmpn .L.u64.borrow");
  println("ujmpn .L.u64.borrow_end");
  println(".L.u64.borrow:");
  println("setn ax 1");
  println(".L.u64.borrow_end:");
  println("suba 7x cx");
  println("suba 7x ax");
  println("addn 4x 1");
  println(".L.u64.skip:");
  println("subn 8x 1");
  println("bign 8x 0");
  println("jmpn .L.u64.loop");
  println("seta 1x 4x");
  println("seta 2x 5x");
  println("ret");
  println(".L.u64.divzero:");
  println("setn 1x 0xffffffff");
  println("setn 2x 0xffffffff");
  println("setn 6x 0xffffffff");
  println("setn 7x 0xffffffff");
  println("ret");
}

static void emit_u64_mul_runtime(void) {
  if (!need_u64_mul)
    return;

  println(".section text.__shy_i64_mul");
  println(".symbol __shy_i64_mul");
  println("calln __shy_u64_mul");
  println("ret");

  println(".section text.__shy_u64_mul");
  println(".symbol __shy_u64_mul");
  println("setn 4x 0");
  println("setn 5x 0");
  println(".L.u64.mul.loop:");
  println("seta 6x 3x");
  println("ora 6x cx");
  println("equn 6x 0");
  println("jmpn .L.u64.mul.done");
  println("seta 6x 3x");
  println("andn 6x 1");
  println("equn 6x 0");
  println("jmpn .L.u64.mul.skip_add");
  println("seta 6x 4x");
  println("adda 4x 1x");
  println("smaa 4x 6x");
  println("setn 7x 0");
  println("jmpn .L.u64.mul.carry");
  println("ujmpn .L.u64.mul.carry_end");
  println(".L.u64.mul.carry:");
  println("setn 7x 1");
  println(".L.u64.mul.carry_end:");
  println("adda 5x 2x");
  println("adda 5x 7x");
  println(".L.u64.mul.skip_add:");
  println("seta 6x 1x");
  println("rsn 6x 31");
  println("lsn 1x 1");
  println("lsn 2x 1");
  println("adda 2x 6x");
  println("seta 6x cx");
  println("andn 6x 1");
  println("lsn 6x 31");
  println("rsn cx 1");
  println("rsn 3x 1");
  println("adda 3x 6x");
  println("ujmpn .L.u64.mul.loop");
  println(".L.u64.mul.done:");
  println("seta 1x 4x");
  println("seta 2x 5x");
  println("ret");
}

static void emit_i64_cmp_runtime(void) {
  if (!need_i64_cmp)
    return;

  println(".section text.__shy_i64_lt");
  println(".symbol __shy_i64_lt");
  println("seta 8x 2x");
  println("rsn 8x 31");
  println("seta 9x cx");
  println("rsn 9x 31");
  println("equa 8x 9x");
  println("jmpn .L.i64.lt.same_sign");
  println("seta 1x 8x");
  println("setn 2x 0");
  println("ret");
  println(".L.i64.lt.same_sign:");
  println("smaa 2x cx");
  println("jmpn .L.i64.lt.true");
  println("biga 2x cx");
  println("jmpn .L.i64.lt.false");
  println("smaa 1x 3x");
  println("jmpn .L.i64.lt.true");
  println(".L.i64.lt.false:");
  println("setn 1x 0");
  println("setn 2x 0");
  println("ret");
  println(".L.i64.lt.true:");
  println("setn 1x 1");
  println("setn 2x 0");
  println("ret");

  println(".section text.__shy_i64_le");
  println(".symbol __shy_i64_le");
  println("seta 4x 1x");
  println("seta 5x 2x");
  println("seta 6x 3x");
  println("seta 7x cx");
  println("xora 1x 3x");
  println("xora 2x cx");
  println("ora 1x 2x");
  println("equn 1x 0");
  println("jmpn .L.i64.le.true");
  println("seta 1x 4x");
  println("seta 2x 5x");
  println("seta 3x 6x");
  println("seta cx 7x");
  println("calln __shy_i64_lt");
  println("ret");
  println(".L.i64.le.true:");
  println("setn 1x 1");
  println("setn 2x 0");
  println("ret");
}

static void emit_runtime(void) {
  emit_u64_mul_runtime();
  emit_u64_divmod_runtime();
  emit_i64_cmp_runtime();
}

void codegen_shy(Obj *prog, FILE *out) {
  output_file = out;
  rename_private_symbols(prog);
  assign_lvar_offsets(prog);

  if (opt_shy_mem_hint)
    println("#![mem(%s)]", opt_shy_mem_hint);
  if (opt_shy_stack_hint)
    println("#![stack(%s)]", opt_shy_stack_hint);
  println("___DEFINE___");
  fputc('\n', output_file);
  println("___DATA___");
  fputc('\n', output_file);
  emit_data(prog);
  fputc('\n', output_file);
  println("___CODE___");
  fputc('\n', output_file);
  emit_text(prog);
  if (opt_shy_link_runtime)
    emit_runtime();
}
