# ShyProject chibicc Fork Notes

This directory is a local fork of chibicc. Keep upstream-compatible C behavior
separate from ShyProject target work whenever possible.

## Patch Areas

- `main.c`: driver flags, accepted file extensions, and target-mode defaults.
- `tokenize.c`: ShyC source metadata such as `#![no_main]`, `#![mem(...)]`,
  `#![stack(...)]`, plus ShyC-only punctuation such as `::`.
- `parse.c`: ShyC language extensions that change the AST or semantic model:
  top-level predeclaration, struct/union tag type names, `impl`, method calls,
  `self`, and local RAII/drop tracking.
- `codegen.c`: target dispatch into the Shy backend.
- `codegen_shy.c`: ShyISA ABI lowering, assembly emission, helper symbols,
  startup generation, and target limitations.
- `shy_runtime_softfloat.c`: optional approximate floating-point helper runtime.

## Parser Extension Boundaries

The local parser additions are intentionally grouped by role:

- Method registry: maps `(struct-or-union type, method name)` to the lowered
  function symbol.
- Impl context: tracks the current `impl Type { ... }` target while parsing or
  predeclaring methods.
- RAII/drop state: tracks active local values whose type has a `drop(self *s)`
  method and inserts cleanup at scope exits and control-flow exits.
- Call argument preparation: applies the shared argument count checks, variadic
  float promotion, non-aggregate casts, and RAII move handling for ordinary
  calls, static method calls, and instance method calls.
- Top-level predeclaration: scans typedefs, tags, functions, and methods before
  parsing bodies so later declarations are callable earlier in the file.

When changing function-call semantics, update the shared call argument path
first. Avoid adding a one-off copy inside method call parsing unless the method
receiver itself needs special handling.

When changing `impl` parsing, keep the predeclaration pass and the real parse
pass in sync. Both must enter the same impl context and lower method symbols in
the same way.

## Verification

Build the fork:

```sh
make -C third_party/chibicc chibicc
```

Run the minimal ShyC integration sample:

```sh
cargo run -q -p shycc -- test/testc/main.c test/testc/put.c -o test/testc/testc.sfs --sym test/testc/testc.sym
cargo run -q -p emu -- test/testc/testc.sfs
```

Compile the ShyC extension sample:

```sh
cargo run -q -p shycc -- test/shyc/struct_impl.shyc -o test/shyc/struct_impl.sfs --sym test/shyc/struct_impl.sym
cargo run -q -p emu -- test/shyc/struct_impl.sfs
```

For driver-only changes, also run:

```sh
cargo test -q -p shycc
```
