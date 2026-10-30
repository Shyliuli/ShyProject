# shyc

`shyc` is an independent ShyC compiler: its own preprocessor, parser, type
checker, typed IR, optimiser, and two code generators. It is not yet the
default toolchain; `shycc` still drives chibicc.

## Pipeline

```text
source -> lexer -> preprocessor -> parser -> typed AST
       -> typed CFG IR (SSA values, explicit slots/load/store)
       -> -O1 passes (mem2reg, constant folding, CFG cleanup, DCE)
       -> Shy assembly  (ShyISA, project ABI)
       or LLVM IR      (x86-64, RISC-V 64, LoongArch 64, AArch64)
```

## Usage

Emit Shy assembly, then assemble and link with the existing tools:

```sh
cargo run -q -p shyc -- -S -O1 file.shyc -o file.shy
target/bin/shyasm file.shy -o file.sobj
target/bin/shyld file.sobj -o file.sfs
target/bin/shyemu file.sfs
```

Emit LLVM IR and compile it for a host target:

```sh
cargo run -q -p shyc -- --emit=llvm --target=x86_64-pc-linux-gnu \
  file.shyc -o file.ll
clang -x ir -O2 file.ll -o program
```

Inspect intermediate stages:

```sh
cargo run -q -p shyc -- -E file.shyc
cargo run -q -p shyc -- --dump-tokens file.shyc
cargo run -q -p shyc -- --dump-ast file.shyc
cargo run -q -p shyc -- --dump-ir -O1 file.shyc
```

Options: `-I`, `-D name[=value]`, `-U name`, `-o file`, `-O0`..`-O3`,
`--target <triple>`. `#![no_main]`, `#![mem(...)]`, and `#![stack(...)]` are
honoured, and the resource hints reach the linked image.

## Status

Working: integers, pointers, arrays, structs and unions, enums, control flow,
direct and indirect calls, recursion, global initialisers with symbol
relocations, ShyC's `impl`/`self`/`drop`, and floating point through the
soft-float runtime.

Floating point on the Shy target needs the runtime object linked in; build it
once with the existing toolchain:

```sh
target/bin/shycc -S third_party/chibicc/shy_runtime_softfloat.c -o float.shy
target/bin/shyasm float.shy -o float.sobj
```

Variadic functions follow the same ABI: fixed parameters take their slots, the
callee saves the argument registers into `__va_area__`, and `stdarg.shyh`
walks the register area and the descending stack cursor.

Inline assembly is emitted verbatim: `asm(...)` bodies go straight into the
output, and `asm!(a, b) { ... }` binds values to the argument registers,
splices `{a}`/`{&a}`, and writes the registers back afterwards. The LLVM
backend rejects it, because the text is ShyISA assembly.

Diagnosed rather than miscompiled: atomics, bit-fields, variable-length
arrays, and thread-local storage. The LLVM backend additionally treats aggregate parameters and returns
as this compiler's own byte-blob convention until target C ABI lowering is
enabled for them.

## Layout

| Path | Contents |
|---|---|
| `src/source.rs` | files, byte spans, diagnostics |
| `src/lexer.rs` | tokens |
| `src/preprocess/` | directives, macros, `#if` |
| `src/ast/`, `src/parser/` | syntax |
| `src/sema/` | target types and record layout |
| `src/lower/` | AST to IR, ShyC extensions |
| `src/ir/` | typed CFG IR |
| `src/passes/` | mem2reg and scalar optimisations |
| `src/shy/` | ShyISA code generation |
| `src/llvm/` | LLVM IR generation |
| `src/abi*.rs` | shared ABI planning (`abi-plan` crate) |
