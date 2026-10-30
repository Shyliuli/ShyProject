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

Options: `-I`, `-D name[=value]`, `-U name`, `-o file`, `-O0`..`-O3`
(`-O1` promotes slots and cleans up, `-O2` repeats that to a fixed point, and
`-O3` adds block-local common subexpression elimination),
`--target <triple>`. `#![no_main]`, `#![mem(...)]`, and `#![stack(...)]` are
honoured, and the resource hints reach the linked image.

## Linking

The two backends have different C libraries, and each one gets its own default:

| Backend | Default library | Turn it off |
|---|---|---|
| Shy (`--backend shyc`, `-S`) | libshy plus the soft-float runtime | `-nostdlib`, `-nodefaultlibs` |
| LLVM (`--emit=llvm`) | the system libc, linked by `clang` | `clang -nostdlib` |

`shycc` links libshy by default because it owns the whole Shy link step;
`-llibshy` and `-lfloat` are still accepted and never duplicated. LLVM
emission stops at LLVM IR — the module is hosted, its `main` is called by the
C runtime — so the system libc comes from whatever links the IR.

## C ABI for aggregates

Aggregates larger than 16 bytes follow the target C ABI: they are passed by
reference (System V and Windows add `byval`, so the callee gets its own copy)
and returned through a hidden `sret` pointer. Verified in both directions
against clang: our code calling a clang-built `sum_big(struct Big)` and
`make_big(int)`, and clang calling ours.

On System V, smaller aggregates are split per eightbyte: each piece goes in an
integer or an SSE register, and multi-register returns are assembled with
`insertvalue`. The IR carries the one fact that needs field knowledge, a mask
of which eightbytes are floating point only; `int`+`float` is an integer
eightbyte, `float`+`float` is an SSE one. AArch64, RISC-V, and LoongArch still
pass small aggregates as opaque byte arrays.

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
