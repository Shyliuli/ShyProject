#!/usr/bin/env bash
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/../.." && pwd)
SHYCC="$ROOT/build/bin/shycc"
SHYEMU="$ROOT/build/bin/shyemu"
OUT="$ROOT/target/shyc-tests"
mkdir -p "$OUT"

[ -x "$SHYCC" ] && [ -x "$SHYEMU" ] || { echo "missing $SHYCC or $SHYEMU: run \`make testpath\`" >&2; exit 1; }

run_case() {
  local src="$1"
  local expected="$2"
  shift 2

  local name
  name=$(basename "$src")
  name=${name%.*}
  local sfs="$OUT/$name.sfs"

  printf 'compile %-28s' "$src"
  (cd "$ROOT" && "$SHYCC" "test/shyc/cases/$src" "$@" -o "$sfs")

  set +e
  local output
  output=$("$SHYEMU" "$sfs" 2>&1)
  local status=$?
  set -e

  if [ "$status" -ne "$expected" ]; then
    printf 'FAIL exit=%d expected=%d\n' "$status" "$expected"
    if [ -n "$output" ]; then
      printf '%s\n' "$output"
    fi
    return 1
  fi

  printf 'ok exit=%d\n' "$status"
}

run_case c_arith_control.c 0
run_case c_pointers_arrays.c 0
run_case c_structs_globals.c 0
run_case c_calls_varargs.c 0
run_case c_64bit_casts.c 0
run_case c_float_ops.c 0 -lfloat
run_case shyc_impl_methods.shyc 0
run_case shyc_asm_and_defer.shyc 0
run_case shyc_small_sret_defer.shyc 0
run_case shyc_default_static.shyc 0 test/shyc/cases/shyc_default_static_lib.shyc
run_case shyc_pub.shyc 0 test/shyc/cases/shyc_pub_lib.shyc
run_case shyc_comment_attr.shyc 0 test/shyc/cases/shyc_pub_lib.shyc
