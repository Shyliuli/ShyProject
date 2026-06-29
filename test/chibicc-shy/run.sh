#!/usr/bin/env bash
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/../.." && pwd)
OUT="$ROOT/target/chibicc-shy-tests"
mkdir -p "$OUT"

run_case() {
  local src="$1"
  local expected="$2"
  shift 2

  local name
  name=$(basename "$src")
  name=${name%.*}
  local sfs="$OUT/$name.sfs"

  printf 'compile %-28s' "$src"
  (cd "$ROOT" && cargo run -q -p shycc -- "test/chibicc-shy/cases/$src" "$@" -o "$sfs")

  set +e
  local output
  output=$(cd "$ROOT" && cargo run -q -p emu -- "$sfs" 2>&1)
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
run_case shyc_small_sret_raii.shyc 0
