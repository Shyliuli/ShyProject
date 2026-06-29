#include "chibicc.h"

StringArray include_paths;
bool opt_fcommon = true;
bool opt_fpic;
bool opt_target_shy = true;
bool opt_shy_link_runtime;
bool opt_shy_no_main;
char *opt_shy_mem_hint;
char *opt_shy_stack_hint;
char *base_file = "-";

int main(void) {
  Token *tok = tokenize_file("-");
  if (!tok)
    error("failed to read stdin");

  convert_pp_tokens(tok);
  Obj *prog = parse(tok);
  codegen_shy(prog, stdout);
  return 0;
}
