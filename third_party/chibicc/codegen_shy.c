#include "chibicc.h"

// ShyISA bare-metal backend.
//
// This file intentionally reuses chibicc's complete C frontend and only
// replaces the final code generation stage. It emits Shy assembly text, then
// ShyProject's own asm/linker can produce .sobj/.sfs.
//
// ABI v1:
// - Shy uses ILP32-style pointers: int and pointer are 32-bit, long remains
//   64-bit.
// - Scalar return: 1x=low32, 2x=high32 when the value is 64-bit.
// - Scalar args consume one 32-bit slot for <=32-bit values and two slots for
//   64-bit values. A scalar that does not fit entirely in the remaining slots
//   goes on the stack; register slots it could not use stay available to later
//   scalars. Slots are registers 4x..bx.
// - Structs and unions are always copied by value onto the stack and never
//   consume argument registers. Stack blocks are padded to 4 bytes and keep
//   native big-endian object order.
// - Stack arguments live below B (the sp used by the call instruction), first
//   argument highest. The callee's incoming stack base is fx-8.
// - fx is the frame pointer and the only callee-saved register. Shy stack grows
//   upward.
// - By default, _start initializes sp, calls main, and writes main's low return
//   word to exit. `#![no_main]` disables this startup and makes user `_start`
//   a bare entry with no prologue.
// - TLS is explicitly unsupported for now.

typedef struct {
  int size;
  bool is64;
} VInfo;

typedef struct Rename Rename;
struct Rename {
  Rename *next;
  char *old;
  char *new;
};

typedef struct DebugFile DebugFile;
struct DebugFile {
  DebugFile *next;
  char *path;
  int index;
};

typedef struct DebugFunction DebugFunction;
struct DebugFunction {
  DebugFunction *next;
  Obj *fn;
  char *name;
  char *section;
  int offset;
  int file_index;
  int line;
};

typedef struct DebugType DebugType;
struct DebugType {
  DebugType *next;
  Type *ty;
  int id;
  char *name_override;
};

typedef struct DebugLine DebugLine;
struct DebugLine {
  DebugLine *next;
  int file_index;
  int line;
  char *section;
  int offset;
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
static char *last_source_filename;
static int last_source_line;
static DebugFile *debug_files;
static DebugFunction *debug_functions;
static DebugType *debug_types;
static DebugLine *debug_lines;
static DebugLine *debug_lines_tail;
static char *debug_current_section;
static int debug_current_offset;

static void json_string(FILE *out, char *s);

static bool startswith(char *s, char *prefix) {
  return !strncmp(s, prefix, strlen(prefix));
}

static char *skip_space(char *s) {
  while (*s && isspace(*s))
    s++;
  return s;
}

static bool is_label_line(char *s) {
  int len = strlen(s);
  return len > 0 && s[len - 1] == ':';
}

static char *token_text(Token *tok) {
  if (!tok || !tok->loc || tok->len <= 0)
    return NULL;
  return strndup(tok->loc, tok->len);
}

static int debug_file_index(char *path) {
  if (!path)
    path = "-";

  for (DebugFile *file = debug_files; file; file = file->next)
    if (!strcmp(file->path, path))
      return file->index;

  DebugFile *file = calloc(1, sizeof(DebugFile));
  file->path = strdup(path);
  int index = 1;
  for (DebugFile *p = debug_files; p; p = p->next)
    index++;
  file->index = index;
  file->next = debug_files;
  debug_files = file;
  return index;
}

static void debug_add_line(char *filename, int line) {
  if (!opt_shy_emit_debug_info || !debug_current_section)
    return;
  if (!startswith(debug_current_section, "text."))
    return;

  DebugLine *entry = calloc(1, sizeof(DebugLine));
  entry->file_index = debug_file_index(filename);
  entry->line = line;
  entry->section = strdup(debug_current_section);
  entry->offset = debug_current_offset;

  if (debug_lines_tail) {
    debug_lines_tail->next = entry;
    debug_lines_tail = entry;
  } else {
    debug_lines = debug_lines_tail = entry;
  }
}

static void debug_add_function(Obj *fn, char *section) {
  if (!opt_shy_emit_debug_info || !fn || !fn->tok)
    return;

  char *filename = fn->tok->filename ? fn->tok->filename :
                   fn->tok->file && fn->tok->file->display_name ? fn->tok->file->display_name :
                   fn->tok->file ? fn->tok->file->name : "-";

  DebugFunction *entry = calloc(1, sizeof(DebugFunction));
  entry->fn = fn;
  entry->name = strdup(fn->name);
  entry->section = strdup(section);
  entry->offset = 0;
  entry->file_index = debug_file_index(filename);
  entry->line = fn->tok->line_no;
  entry->next = debug_functions;
  debug_functions = entry;
}

static int debug_type_id(Type *ty) {
  if (!ty)
    return 0;

  for (DebugType *dt = debug_types; dt; dt = dt->next)
    if (dt->ty == ty)
      return dt->id;

  DebugType *dt = calloc(1, sizeof(DebugType));
  dt->ty = ty;
  int id = 1;
  for (DebugType *p = debug_types; p; p = p->next)
    id++;
  dt->id = id;
  dt->next = debug_types;
  debug_types = dt;

  switch (ty->kind) {
  case TY_PTR:
  case TY_ARRAY:
  case TY_VLA:
    debug_type_id(ty->base);
    break;
  case TY_FUNC:
    debug_type_id(ty->return_ty);
    for (Type *param = ty->params; param; param = param->next)
      debug_type_id(param);
    break;
  case TY_STRUCT:
  case TY_UNION:
    for (Member *mem = ty->members; mem; mem = mem->next)
      debug_type_id(mem->ty);
    break;
  default:
    break;
  }

  return dt->id;
}

static DebugType *debug_find_type(Type *ty) {
  for (DebugType *dt = debug_types; dt; dt = dt->next)
    if (dt->ty == ty)
      return dt;
  return NULL;
}

static void debug_note_var_type_name(Obj *var) {
  if (!var || !var->ty || !var->name || !var->name[0])
    return;
  if (var->ty->kind != TY_STRUCT && var->ty->kind != TY_UNION)
    return;

  int id = debug_type_id(var->ty);
  (void)id;
  DebugType *dt = debug_find_type(var->ty);
  if (!dt || dt->name_override || var->ty->tag)
    return;
  dt->name_override =
      format("%s %s", var->ty->kind == TY_STRUCT ? "struct" : "union",
             var->name);
}

static bool debug_is_param(Obj *fn, Obj *var) {
  for (Obj *param = fn->params; param; param = param->next)
    if (param == var)
      return true;
  return false;
}

static bool debug_should_emit_var(Obj *var) {
  return var && var->name && var->name[0] && var->ty && !var->is_sret_alias &&
         !var->is_elided;
}

static bool debug_is_internal_var(Obj *fn, Obj *var) {
  return var == fn->va_area || var == fn->alloca_bottom ||
         !strcmp(var->name, "__va_area__") ||
         !strcmp(var->name, "__alloca_size__");
}

static void debug_collect_function_types(Obj *fn) {
  for (Obj *var = fn->params; var; var = var->next) {
    if (!debug_should_emit_var(var))
      continue;
    debug_note_var_type_name(var);
    debug_type_id(var->ty);
  }
  for (Obj *var = fn->locals; var; var = var->next) {
    if (debug_is_param(fn, var) || !debug_should_emit_var(var) ||
        debug_is_internal_var(fn, var))
      continue;
    debug_note_var_type_name(var);
    debug_type_id(var->ty);
  }
}

static void debug_collect_types(void) {
  for (DebugFunction *fn = debug_functions; fn; fn = fn->next)
    if (fn->fn)
      debug_collect_function_types(fn->fn);
}

static char *debug_type_name(Type *ty) {
  if (!ty)
    return NULL;

  switch (ty->kind) {
  case TY_VOID:
    return "void";
  case TY_BOOL:
    return "_Bool";
  case TY_CHAR:
    return ty->is_unsigned ? "unsigned char" : "char";
  case TY_SHORT:
    return ty->is_unsigned ? "unsigned short" : "short";
  case TY_INT:
    return ty->is_unsigned ? "unsigned int" : "int";
  case TY_LONG:
    return ty->is_unsigned ? "unsigned long" : "long";
  case TY_FLOAT:
    return "float";
  case TY_DOUBLE:
    return "double";
  case TY_LDOUBLE:
    return "long double";
  case TY_ENUM:
    return "enum";
  case TY_STRUCT:
    if (ty->tag)
      return format("struct %s", token_text(ty->tag));
    return "struct";
  case TY_UNION:
    if (ty->tag)
      return format("union %s", token_text(ty->tag));
    return "union";
  case TY_PTR:
    return "pointer";
  case TY_ARRAY:
    return "array";
  case TY_VLA:
    return "vla";
  case TY_FUNC:
    return "function";
  }
  return "type";
}

static char *debug_type_kind(Type *ty) {
  switch (ty->kind) {
  case TY_PTR:
    return "pointer";
  case TY_ARRAY:
  case TY_VLA:
    return "array";
  case TY_STRUCT:
    return "struct";
  case TY_UNION:
    return "union";
  case TY_ENUM:
    return "enum";
  case TY_FUNC:
    return "function";
  default:
    return "base";
  }
}

static char *debug_base_encoding(Type *ty) {
  switch (ty->kind) {
  case TY_VOID:
    return "void";
  case TY_BOOL:
    return "bool";
  case TY_FLOAT:
  case TY_DOUBLE:
  case TY_LDOUBLE:
    return "float";
  case TY_CHAR:
  case TY_SHORT:
  case TY_INT:
  case TY_LONG:
  case TY_ENUM:
    return ty->is_unsigned ? "unsigned" : "signed";
  default:
    return NULL;
  }
}

static void debug_write_var(FILE *out, Obj *var) {
  char *filename = var->tok && var->tok->filename ? var->tok->filename :
                   var->tok && var->tok->file && var->tok->file->display_name ? var->tok->file->display_name :
                   var->tok && var->tok->file ? var->tok->file->name : NULL;
  fprintf(out, "{\"name\": ");
  json_string(out, var->name);
  fprintf(out, ", \"type\": %d, \"offset\": %d, \"file\": %d, \"line\": %d}",
          debug_type_id(var->ty), var->offset,
          filename ? debug_file_index(filename) : 0,
          var->tok ? var->tok->line_no : 0);
}

static void debug_write_vars(FILE *out, Obj *fn, bool params) {
  bool first = true;
  if (params) {
    for (Obj *var = fn->params; var; var = var->next) {
      if (!debug_should_emit_var(var))
        continue;
      if (!first)
        fprintf(out, ", ");
      first = false;
      debug_write_var(out, var);
    }
    return;
  }

  for (Obj *var = fn->locals; var; var = var->next) {
    if (debug_is_param(fn, var) || !debug_should_emit_var(var) ||
        debug_is_internal_var(fn, var))
      continue;
    if (!first)
      fprintf(out, ", ");
    first = false;
    debug_write_var(out, var);
  }
}

static void debug_write_type(FILE *out, DebugType *dt) {
  Type *ty = dt->ty;
  fprintf(out, "    {\"id\": %d, \"kind\": ", dt->id);
  json_string(out, debug_type_kind(ty));
  fprintf(out, ", \"name\": ");
  json_string(out, dt->name_override ? dt->name_override : debug_type_name(ty));
  fprintf(out, ", \"size\": %d", ty->size);

  char *encoding = debug_base_encoding(ty);
  if (encoding) {
    fprintf(out, ", \"encoding\": ");
    json_string(out, encoding);
  }

  if (ty->base)
    fprintf(out, ", \"base\": %d", debug_type_id(ty->base));
  if (ty->kind == TY_ARRAY)
    fprintf(out, ", \"array_len\": %d", ty->array_len);
  if (ty->kind == TY_VLA)
    fprintf(out, ", \"array_len\": -1");

  if (ty->kind == TY_STRUCT || ty->kind == TY_UNION) {
    fprintf(out, ", \"members\": [");
    bool first = true;
    for (Member *mem = ty->members; mem; mem = mem->next) {
      if (!mem->name)
        continue;
      if (!first)
        fprintf(out, ", ");
      first = false;
      fprintf(out, "{\"name\": ");
      json_string(out, token_text(mem->name));
      fprintf(out, ", \"type\": %d, \"offset\": %d", debug_type_id(mem->ty),
              mem->offset);
      if (mem->is_bitfield)
        fprintf(out, ", \"bit_offset\": %d, \"bit_size\": %d",
                mem->bit_offset, mem->bit_width);
      fprintf(out, "}");
    }
    fprintf(out, "]");
  }

  fprintf(out, "}");
}

static void debug_track_emitted_line(char *line) {
  if (!opt_shy_emit_debug_info)
    return;

  char *s = skip_space(line);
  if (!*s)
    return;

  if (startswith(s, ".section ")) {
    free(debug_current_section);
    debug_current_section = strdup(skip_space(s + strlen(".section ")));
    debug_current_offset = 0;
    return;
  }

  if (!debug_current_section || !startswith(debug_current_section, "text."))
    return;
  if (startswith(s, "//") || startswith(s, "#![") || *s == '.' || is_label_line(s))
    return;

  debug_current_offset += 12;
}

static void json_string(FILE *out, char *s) {
  fputc('"', out);
  for (; s && *s; s++) {
    switch (*s) {
    case '\\':
      fputs("\\\\", out);
      break;
    case '"':
      fputs("\\\"", out);
      break;
    case '\n':
      fputs("\\n", out);
      break;
    case '\r':
      fputs("\\r", out);
      break;
    case '\t':
      fputs("\\t", out);
      break;
    default:
      fputc(*s, out);
    }
  }
  fputc('"', out);
}

static void write_shy_debug_info(void) {
  if (!opt_shy_emit_debug_info)
    return;

  debug_collect_types();

  FILE *out = fopen(opt_shy_debug_output_path, "w");
  if (!out)
    error("cannot open debug info file: %s: %s", opt_shy_debug_output_path,
          strerror(errno));

  fprintf(out, "{\n  \"version\": 1,\n  \"address_size\": 4,\n  \"endian\": \"big\",\n");

  fprintf(out, "  \"files\": [\n");
  bool first = true;
  for (DebugFile *file = debug_files; file; file = file->next) {
    if (!first)
      fprintf(out, ",\n");
    first = false;
    fprintf(out, "    {\"index\": %d, \"path\": ", file->index);
    json_string(out, file->path);
    fprintf(out, "}");
  }
  fprintf(out, "\n  ],\n");

  fprintf(out, "  \"types\": [\n");
  first = true;
  for (DebugType *type = debug_types; type; type = type->next) {
    if (!first)
      fprintf(out, ",\n");
    first = false;
    debug_write_type(out, type);
  }
  fprintf(out, "\n  ],\n");

  fprintf(out, "  \"functions\": [\n");
  first = true;
  for (DebugFunction *fn = debug_functions; fn; fn = fn->next) {
    if (!first)
      fprintf(out, ",\n");
    first = false;
    fprintf(out, "    {\"name\": ");
    json_string(out, fn->name);
    fprintf(out, ", \"symbol\": ");
    json_string(out, fn->name);
    fprintf(out, ", \"section\": ");
    json_string(out, fn->section);
    fprintf(out, ", \"offset\": %d, \"file\": %d, \"line\": %d",
            fn->offset, fn->file_index, fn->line);
    fprintf(out, ", \"frame_base\": \"r15\", \"params\": [");
    if (fn->fn)
      debug_write_vars(out, fn->fn, true);
    fprintf(out, "], \"locals\": [");
    if (fn->fn)
      debug_write_vars(out, fn->fn, false);
    fprintf(out, "]}");
  }
  fprintf(out, "\n  ],\n");

  fprintf(out, "  \"line_entries\": [\n");
  first = true;
  for (DebugLine *line = debug_lines; line; line = line->next) {
    if (!first)
      fprintf(out, ",\n");
    first = false;
    fprintf(out, "    {\"file\": %d, \"line\": %d, \"section\": ",
            line->file_index, line->line);
    json_string(out, line->section);
    fprintf(out, ", \"offset\": %d}", line->offset);
  }
  fprintf(out, "\n  ]\n}\n");
  fclose(out);
}

__attribute__((format(printf, 1, 2)))
static void println(char *fmt, ...) {
  char *line;
  size_t len;
  FILE *buf = open_memstream(&line, &len);

  va_list ap;
  va_start(ap, fmt);
  vfprintf(buf, fmt, ap);
  va_end(ap);
  fclose(buf);

  fputs(line, output_file);
  fprintf(output_file, "\n");
  debug_track_emitted_line(line);
  free(line);
}

static void emit_source_line(Token *tok) {
  if (!opt_shy_emit_source_lines || !tok || !tok->file || !tok->loc)
    return;

  char *filename = tok->filename ? tok->filename :
                   tok->file->display_name ? tok->file->display_name :
                   tok->file->name;
  int line_no = tok->line_no;
  if (filename == last_source_filename && line_no == last_source_line)
    return;
  last_source_filename = filename;
  last_source_line = line_no;
  debug_add_line(filename, line_no);

  uintptr_t file_start = (uintptr_t)tok->file->contents;
  uintptr_t file_end = file_start + strlen(tok->file->contents);
  uintptr_t loc = (uintptr_t)tok->loc;
  if (loc < file_start || file_end < loc) {
    println("//source %s:%d", filename ? filename : "-", line_no);
    return;
  }

  char *line = tok->loc;
  while (tok->file->contents < line && line[-1] != '\n')
    line--;

  char *end = tok->loc;
  while (*end && *end != '\n')
    end++;
  if (line < end && end[-1] == '\r')
    end--;

  println("//source %s:%d %.*s", filename ? filename : "-", line_no,
          (int)(end - line), line);
}

static int count(void) {
  return ++labelseq;
}

static char *sanitize(char *s) {
  char *buf = calloc(1, strlen(s) + 1);
  for (int i = 0; s[i]; i++) {
    char c = s[i];
    buf[i] = isalnum(c) ? c : '_';
  }
  return buf;
}

// ShyISA register names are also valid C identifiers, and the assembler reads
// a bare register name as a register. A C symbol spelled like one (`exit`,
// `sp`, ...) must therefore be written as `@name` in an instruction operand.
// The symbol table keeps the plain name, so linking is unaffected.
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

static bool node_refs_var(Node *node, Obj *var);

static bool is_64bit(Type *ty) {
  return ty->size == 8;
}

static VInfo vinfo(Type *ty) {
  if (ty->kind == TY_ARRAY || ty->kind == TY_FUNC)
    return (VInfo){4, false};
  return (VInfo){ty->size, is_64bit(ty)};
}

static bool is_shy_flonum(Type *ty) {
  return ty->kind == TY_FLOAT || ty->kind == TY_DOUBLE;
}

static bool returns_by_sret(Type *ty) {
  return ty->kind == TY_STRUCT || ty->kind == TY_UNION;
}

static int stack_size_of(Type *ty) {
  return align_to(MAX(ty->size, 4), 4);
}

// ABI v1 argument classification, shared by call sites and callee prologues.
typedef struct {
  int slot;       // first register slot, -1 when passed on the stack
  int slots;      // number of 4-byte register slots
  int stack_off;  // bytes below B to the low address of this stack block
  int stack_size; // padded stack block size
} ShyArgClass;

static bool is_aggregate_type(Type *ty) {
  return ty->kind == TY_STRUCT || ty->kind == TY_UNION;
}

static int arg_type_size(Type *ty) {
  if (ty->kind == TY_ARRAY || ty->kind == TY_FUNC)
    return 4;
  return ty->size;
}

static bool pass_by_memory(Type *ty) {
  return is_aggregate_type(ty) || arg_type_size(ty) > 8;
}

static ShyArgClass classify_arg(Type *ty, int *slot_cursor, int *stack_cursor) {
  ShyArgClass c = {-1, 0, 0, 0};
  int size = arg_type_size(ty);

  if (pass_by_memory(ty)) {
    c.stack_size = align_to(MAX(size, 0), 4);
    c.stack_off = *stack_cursor + c.stack_size;
    *stack_cursor = c.stack_off;
    return c;
  }

  int need = size > 4 ? 2 : 1;
  if (*slot_cursor + need <= argreg_len) {
    c.slot = *slot_cursor;
    c.slots = need;
    *slot_cursor += need;
    return c;
  }

  c.stack_size = align_to(MAX(size, 4), 4);
  c.stack_off = *stack_cursor + c.stack_size;
  *stack_cursor = c.stack_off;
  return c;
}

static void assign_lvar_offsets(Obj *prog) {
  for (Obj *fn = prog; fn; fn = fn->next) {
    if (!fn->is_function)
      continue;

    int off = 0;
    for (Obj *var = fn->locals; var; var = var->next) {
      if (var->is_sret_alias || var->is_elided)
        continue;
      if (var == fn->alloca_bottom && !node_refs_var(fn->body, var))
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
  if (vi.is64)
    push32("2x");
  push32("1x");
}

static void pop_value(VInfo vi, char *lo, char *hi) {
  pop32(lo);
  if (vi.is64)
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

static void gen_signed_cmp32(bool le) {
  int c = count();
  println("seta 4x 1x");
  println("rsn 4x 31");
  println("seta 5x 3x");
  println("rsn 5x 31");
  println("equa 4x 5x");
  println("jmpn .L.scmp.same.%d", c);
  println("seta 1x 4x");
  println("ujmpn .L.scmp.end.%d", c);
  println(".L.scmp.same.%d:", c);
  println("%s 1x 3x", le ? "smaequa" : "smaa");
  set_bool_from_rs();
  println(".L.scmp.end.%d:", c);
  println("setn 2x 0");
}

static void gen_signed_cmp32_imm(uint32_t imm, bool le) {
  int c = count();
  println("seta 4x 1x");
  println("rsn 4x 31");
  println("setn 5x %u", imm >> 31);
  println("equa 4x 5x");
  println("jmpn .L.scmp.same.%d", c);
  println("seta 1x 4x");
  println("ujmpn .L.scmp.end.%d", c);
  println(".L.scmp.same.%d:", c);
  println("%s 1x %u", le ? "smaequn" : "sman", imm);
  set_bool_from_rs();
  println(".L.scmp.end.%d:", c);
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
    if (var->offset) {
      println("setn 1x %d", var->offset);
      println("adda 1x fx");
    } else {
      println("seta 1x fx");
    }
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
  case ND_STMT_EXPR:
    // A statement expression used in an aggregate context leaves the last
    // expression's aggregate address in 1x.
    gen_expr(node);
    return;
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

static void store_to_addr_reg(Type *ty, char *addr) {
  switch (ty->size) {
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
    error("Shy backend cannot store scalar of size %d", ty->size);
  }
}

static bool try_gen_simple_assign(Node *node) {
  if (node->lhs->kind != ND_VAR ||
      node->lhs->ty->kind == TY_STRUCT || node->lhs->ty->kind == TY_UNION)
    return false;

  Obj *var = node->lhs->var;
  if (var->is_sret_alias)
    return false;

  gen_expr(node->rhs);
  if (var->is_local) {
    if (var->offset) {
      println("setn 3x %d", var->offset);
      println("adda 3x fx");
    } else {
      println("seta 3x fx");
    }
  } else {
    println("setn 3x %s", symbol_operand(var->name));
  }
  store_to_addr_reg(node->lhs->ty, "3x");
  return true;
}

// Byte copy: aggregate sources may be packed or subobjects with align 1/2,
// so word-sized accesses would trap on unaligned addresses.
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
  if (vi.is64) {
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

static bool get_imm32(Node *node, uint32_t *val) {
  if (node->kind == ND_NUM && !is_shy_flonum(node->ty) && !vinfo(node->ty).is64) {
    *val = (uint32_t)node->val;
    return true;
  }

  if (node->kind != ND_CAST || is_shy_flonum(node->ty) || vinfo(node->ty).is64)
    return false;

  uint32_t inner;
  if (!get_imm32(node->lhs, &inner))
    return false;

  switch (node->ty->kind) {
  case TY_BOOL:
    *val = inner != 0;
    return true;
  case TY_CHAR:
    *val = node->ty->is_unsigned ? (uint8_t)inner : (uint32_t)(int8_t)inner;
    return true;
  case TY_SHORT:
    *val = node->ty->is_unsigned ? (uint16_t)inner : (uint32_t)(int16_t)inner;
    return true;
  default:
    *val = inner;
    return true;
  }
}

static bool try_gen_binary_imm(Node *node) {
  VInfo lvi = vinfo(node->lhs->ty);
  VInfo rvi = vinfo(node->rhs->ty);
  VInfo vi = vinfo(node->ty);
  uint32_t imm;
  if (is_shy_flonum(node->lhs->ty) || is_shy_flonum(node->rhs->ty) ||
      lvi.is64 || rvi.is64 || vi.is64 || !get_imm32(node->rhs, &imm))
    return false;

  gen_expr(node->lhs);

  switch (node->kind) {
  case ND_ADD:
    println("addn 1x %u", imm);
    return true;
  case ND_SUB:
    println("subn 1x %u", imm);
    return true;
  case ND_MUL:
    println("muln 1x %u", imm);
    return true;
  case ND_DIV:
    println("divn 1x %u", imm);
    return true;
  case ND_MOD:
    println("seta dx 1x");
    println("divn 1x %u", imm);
    println("muln 1x %u", imm);
    println("suba dx 1x");
    println("seta 1x dx");
    return true;
  case ND_BITAND:
    println("andn 1x %u", imm);
    return true;
  case ND_BITOR:
    println("orn 1x %u", imm);
    return true;
  case ND_BITXOR:
    println("xorn 1x %u", imm);
    return true;
  case ND_EQ:
    println("equn 1x %u", imm);
    set_bool_from_rs();
    return true;
  case ND_NE: {
    int c = count();
    println("equn 1x %u", imm);
    println("setn 1x 1");
    println("jmpn .L.ne.false.%d", c);
    println("ujmpn .L.ne.end.%d", c);
    println(".L.ne.false.%d:", c);
    println("setn 1x 0");
    println(".L.ne.end.%d:", c);
    println("setn 2x 0");
    return true;
  }
  case ND_LT:
    if (!node->lhs->ty->is_unsigned) {
      gen_signed_cmp32_imm(imm, false);
      return true;
    }
    println("sman 1x %u", imm);
    set_bool_from_rs();
    return true;
  case ND_LE:
    if (!node->lhs->ty->is_unsigned) {
      gen_signed_cmp32_imm(imm, true);
      return true;
    }
    println("smaequn 1x %u", imm);
    set_bool_from_rs();
    return true;
  case ND_SHL:
    println("lsn 1x %u", imm);
    return true;
  case ND_SHR:
    println("rsn 1x %u", imm);
    return true;
  default:
    return false;
  }
}

static void gen_binary(Node *node) {
  if (try_gen_binary_imm(node))
    return;

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

  if (lvi.is64 || rvi.is64 || vi.is64) {
    if (!lvi.is64)
      println("setn 2x 0");
    if (!rvi.is64)
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
    if (!node->lhs->ty->is_unsigned) {
      gen_signed_cmp32(false);
      return;
    }
    println("smaa 1x 3x");
    set_bool_from_rs();
    return;
  case ND_LE:
    if (!node->lhs->ty->is_unsigned) {
      gen_signed_cmp32(true);
      return;
    }
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
    if (vinfo(node->ty).is64) {
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
    if (try_gen_simple_assign(node))
      return;
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
    } else if (vinfo(node->ty).is64 && !vinfo(node->lhs->ty).is64) {
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
    } else if (!vinfo(node->ty).is64) {
      switch (node->ty->kind) {
      case TY_BOOL:
        println("equn 1x 0");
        {
          int c = count();
          println("setn 1x 1");
          println("jmpn .L.cast.bool.false.%d", c);
          println("ujmpn .L.cast.bool.end.%d", c);
          println(".L.cast.bool.false.%d:", c);
          println("setn 1x 0");
          println(".L.cast.bool.end.%d:", c);
          println("setn 2x 0");
        }
        break;
      case TY_CHAR:
        println("andn 1x 0xff");
        if (!node->ty->is_unsigned) {
          int c = count();
          println("bign 1x 127");
          println("jmpn .L.cast.sign8.%d", c);
          println("ujmpn .L.cast.end8.%d", c);
          println(".L.cast.sign8.%d:", c);
          println("orn 1x 0xffffff00");
          println(".L.cast.end8.%d:", c);
        }
        println("setn 2x 0");
        break;
      case TY_SHORT:
        println("andn 1x 0xffff");
        if (!node->ty->is_unsigned) {
          int c = count();
          println("bign 1x 32767");
          println("jmpn .L.cast.sign16.%d", c);
          println("ujmpn .L.cast.end16.%d", c);
          println(".L.cast.sign16.%d:", c);
          println("orn 1x 0xffff0000");
          println(".L.cast.end16.%d:", c);
        }
        println("setn 2x 0");
        break;
      default:
        println("setn 2x 0");
      }
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
    if (vinfo(node->ty).is64)
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
    if (returns_by_sret(node->ty) && !node->ret_buffer)
      unsupported(node, "missing hidden result buffer");
    bool sret = node->ret_buffer && returns_by_sret(node->ty);
    bool indirect =
        !(node->lhs->kind == ND_VAR && node->lhs->var->is_function);

    int nargs = 0;
    for (Node *arg = node->args; arg; arg = arg->next) {
      if (!is_aggregate_type(arg->ty) && arg_type_size(arg->ty) > 8)
        unsupported(arg, "arguments wider than 8 bytes");
      nargs++;
    }

    int total = nargs + (sret ? 1 : 0);
    Node **argv = calloc(total ? total : 1, sizeof(Node *));
    ShyArgClass *argv_cls = calloc(total ? total : 1, sizeof(ShyArgClass));
    int *suffix = calloc(total + 1, sizeof(int));
    int slot = 0, stack = 0, idx = 0;

    // The hidden result pointer is always the first argument register.
    if (sret) {
      argv_cls[idx].slot = slot++;
      argv_cls[idx].slots = 1;
      idx++;
    }

    for (Node *arg = node->args; arg; arg = arg->next) {
      argv[idx] = arg;
      argv_cls[idx] = classify_arg(arg->ty, &slot, &stack);
      idx++;
    }

    int total_stack = stack;
    int extra = indirect ? 4 : 0;
    for (int i = total - 1; i >= 0; i--)
      suffix[i] = suffix[i + 1] + argv_cls[i].slots * 4;

    if (total_stack)
      println("addn sp %d", total_stack);

    // Resolve an indirect callee before arguments are staged; keep it on the
    // stack so nested calls in the arguments cannot clobber it.
    if (indirect) {
      gen_expr(node->lhs);
      push32("1x");
    }

    for (int i = total - 1; i >= 0; i--) {
      if (i == 0 && sret) {
        gen_var_addr(node->ret_buffer);
        push32("1x");
        continue;
      }

      ShyArgClass c = argv_cls[i];
      if (c.slot >= 0) {
        gen_expr(argv[i]);
        push_value(vinfo(argv[i]->ty));
        continue;
      }

      // Stack block address is B - S_i. B = sp - extra - pushed register
      // temporaries, so the destination does not depend on the frame base.
      if (pass_by_memory(argv[i]->ty)) {
        gen_expr(argv[i]);
        println("seta 4x 1x");
        println("seta 3x sp");
        if (extra + suffix[i + 1])
          println("subn 3x %d", extra + suffix[i + 1]);
        println("subn 3x %d", c.stack_off);
        println("seta 1x 4x");
        copy_bytes(argv[i]->ty);
        continue;
      }

      gen_expr(argv[i]);
      println("seta 3x sp");
      if (extra + suffix[i + 1])
        println("subn 3x %d", extra + suffix[i + 1]);
      println("subn 3x %d", c.stack_off);

      switch (arg_type_size(argv[i]->ty)) {
      case 1:
        println("put8a 3x 1x");
        break;
      case 2:
        println("put16a 3x 1x");
        break;
      case 4:
        println("puta 3x 1x");
        break;
      case 8:
        // Stack objects use native big-endian order: high word first.
        println("puta 3x 2x");
        println("addn 3x 4");
        println("puta 3x 1x");
        break;
      default:
        unsupported(argv[i], "stack argument size");
      }
    }

    for (int i = 0; i < total; i++) {
      ShyArgClass c = argv_cls[i];
      for (int s = 0; s < c.slots; s++)
        pop32(argreg[c.slot + s]);
    }

    if (indirect) {
      pop32("ex");
      println("calla ex");
    } else {
      println("calln %s", symbol_operand(node->lhs->var->name));
    }

    if (total_stack)
      println("subn sp %d", total_stack);

    if (sret)
      gen_var_addr(node->ret_buffer);

    free(argv);
    free(argv_cls);
    free(suffix);
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
    for (Node *n = node->body; n; n = n->next)
      gen_stmt(n);
    return;
  default:
    unsupported(node, "expression");
  }
}

static void gen_stmt(Node *node) {
  emit_source_line(node->tok);

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
    if (vinfo(node->cond->ty).is64)
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
      Type *ty = node->lhs->ty;
      if (returns_by_sret(ty))
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

static bool node_refs_var(Node *node, Obj *var) {
  for (; node; node = node->next) {
    if (node->kind == ND_VAR && node->var == var)
      return true;

    if (node_refs_var(node->lhs, var) || node_refs_var(node->rhs, var) ||
        node_refs_var(node->cond, var) || node_refs_var(node->then, var) ||
        node_refs_var(node->els, var) || node_refs_var(node->init, var) ||
        node_refs_var(node->inc, var) || node_refs_var(node->body, var) ||
        node_refs_var(node->args, var) || node_refs_var(node->cas_addr, var) ||
        node_refs_var(node->cas_old, var) || node_refs_var(node->cas_new, var) ||
        node_refs_var(node->atomic_expr, var))
      return true;
  }
  return false;
}

static void emit_text(Obj *prog) {
  emit_start(prog);

  for (Obj *fn = prog; fn; fn = fn->next) {
    if (!fn->is_function || !fn->is_definition || !fn->is_live)
      continue;
    bool bare_start = opt_shy_no_main && !strcmp(fn->name, "_start");
    char *section_name = format("text.%s", fn->name);
    println(".section %s", section_name);
    debug_add_function(fn, section_name);
    emit_source_line(fn->tok);
    println(".symbol %s", fn->name);
    current_fn = fn;

    if (!bare_start) {
      println("pusha fx");
      println("seta fx sp");
      if (fn->stack_size)
        println("addn sp %d", fn->stack_size);
    }

    if (bare_start) {
      for (Obj *var = fn->params; var; var = var->next)
        error_tok(var->tok, "Shy bare _start cannot have parameters");
      if (fn->va_area)
        error_tok(fn->tok, "Shy bare _start cannot be variadic");
    } else {
      int named_slot = 0, named_stack = 0;
      for (Obj *var = fn->params; var; var = var->next)
        classify_arg(var->ty, &named_slot, &named_stack);

      // The register save area must be captured before parameters are copied
      // into the frame, because the parameter copies may reuse scratch regs.
      if (fn->va_area) {
        int base = fn->va_area->offset;

        for (int i = 0; i < argreg_len; i++) {
          println("setn 3x %d", base + 12 + i * 4);
          println("adda 3x fx");
          println("puta 3x %s", argreg[i]);
        }

        // va_list descriptor: next register slot, saved register area,
        // descending stack cursor at the first variadic stack block.
        println("setn 3x %d", base);
        println("adda 3x fx");
        println("putn 3x %d", named_slot);

        println("setn 3x %d", base + 4);
        println("adda 3x fx");
        println("setn 1x %d", base + 12);
        println("adda 1x fx");
        println("puta 3x 1x");

        println("setn 3x %d", base + 8);
        println("adda 3x fx");
        println("seta 1x fx");
        println("subn 1x 8");
        if (named_stack)
          println("subn 1x %d", named_stack);
        println("puta 3x 1x");
      }

      int pslot = 0, pstack = 0;
      for (Obj *var = fn->params; var; var = var->next) {
        ShyArgClass c = classify_arg(var->ty, &pslot, &pstack);
        int size = arg_type_size(var->ty);

        if (pass_by_memory(var->ty)) {
          println("setn 3x %d", var->offset);
          println("adda 3x fx");
          println("seta 1x fx");
          println("subn 1x 8");
          println("subn 1x %d", c.stack_off);
          copy_bytes(var->ty);
          continue;
        }

        if (c.slot >= 0) {
          println("setn 3x %d", var->offset);
          println("adda 3x fx");
          switch (size) {
          case 1:
            println("put8a 3x %s", argreg[c.slot]);
            break;
          case 2:
            println("put16a 3x %s", argreg[c.slot]);
            break;
          case 4:
            println("puta 3x %s", argreg[c.slot]);
            break;
          case 8:
            // Register slots are low-word-first; memory is high-word-first.
            println("puta 3x %s", argreg[c.slot + 1]);
            println("addn 3x 4");
            println("puta 3x %s", argreg[c.slot]);
            break;
          default:
            error_tok(var->tok, "unsupported parameter size");
          }
          continue;
        }

        // Scalar stack parameter: the block already uses native object order.
        println("setn 3x %d", var->offset);
        println("adda 3x fx");
        println("seta 1x fx");
        println("subn 1x 8");
        println("subn 1x %d", c.stack_off);
        switch (size) {
        case 1:
          println("get8a 1x 1x");
          println("put8a 3x 1x");
          break;
        case 2:
          println("get16a 1x 1x");
          println("put16a 3x 1x");
          break;
        case 4:
          println("geta 1x 1x");
          println("puta 3x 1x");
          break;
        case 8:
          println("geta 2x 1x");
          println("addn 1x 4");
          println("geta 1x 1x");
          println("puta 3x 2x");
          println("addn 3x 4");
          println("puta 3x 1x");
          break;
        default:
          error_tok(var->tok, "unsupported parameter size");
        }
      }
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
  last_source_filename = NULL;
  last_source_line = 0;
  debug_files = NULL;
  debug_functions = NULL;
  debug_types = NULL;
  debug_lines = NULL;
  debug_lines_tail = NULL;
  debug_current_section = NULL;
  debug_current_offset = 0;
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
  write_shy_debug_info();
}
