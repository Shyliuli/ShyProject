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

Export a C interface for other languages and pre-existing C code:

```sh
target/bin/shyc --emit=c-header api.shyc -o api.h   # prototypes, types, extern objects
target/bin/shyc --emit=c       api.shyc -o api.c    # the TU as plain C
```

`--emit=c-header` walks the parsed translation unit and prints an include-guarded
header: file-scope typedefs, record and enum definitions, prototypes for every
non-`static` function, and `extern` declarations for objects (initializers are
dropped). `static` declarations, `impl` blocks, and `trait` declarations are not
exported; methods need a concrete symbol before they can be declared. `--emit=c`
prints the preprocessed unit as plain C and drops the `#![...]` attribute lines.
A `let` declaration has no C spelling, so a unit that uses one is rejected with
a diagnostic instead of being printed as invalid C.

Options: `-I`, `-D name[=value]`, `-U name`, `-o file`, `-O0`..`-O3`
(`-O1` promotes slots and cleans up, `-O2` repeats that to a fixed point, and
`-O3` adds block-local common subexpression elimination),
`--target <triple>`. `#![no_main]`, `#![mem(...)]`, `#![stack(...)]`,
`#![disable_nonexhaustive_switch]`, and `#![disable_rustic_mut]` are honoured,
and the resource hints reach the linked image. The switch attribute is
file-level: there is no local attribute form yet.

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

## C ABI

Every rule about where an argument or a return value lives belongs to
`abi-plan`: System V eightbytes, AArch64 homogeneous float aggregates, RISC-V
and LoongArch field flattening, memory passing with `byval`, and hidden `sret`
pointers. Lowering asks for the plan while the field layout is still known and
stores it in the IR, so the backends never re-derive a convention from
`(size, align)`.

The LLVM emitter turns each plan part into one LLVM value: integer parts by
width, floating parts as `float`/`double`, and multi-part returns assembled
with `insertvalue` because LLVM only accepts constant aggregate literals.
Checked against clang for x86-64, aarch64, riscv64, and loongarch64.

## Status

Working: integers, pointers, arrays, structs and unions, enums, control flow,
direct and indirect calls, recursion, global initialisers with symbol
relocations, ShyC's `impl`/`self`/`drop`, compile-time traits, and floating
point through the soft-float runtime.

Traits are compile-time only. `trait Drawable { void draw(self *value); }`
declares method signatures, and `impl Drawable for Circle { ... }` checks that
the implementation defines exactly those methods with matching signatures. The
methods are then lowered exactly like an ordinary `impl`: they become plain
`____Circle__draw` functions and every call stays statically dispatched, with
no vtable and no hidden allocation. A trait name cannot be used as a value type
yet; that use, a second implementation of the same trait for one type, a
duplicate method name across two traits on one type, generic trait methods, and
`<T: Drawable>` bounds all get an explicit diagnostic. There is no dynamic
dispatch.

`let name = expression;` infers its type from the initializer as an rvalue:
arrays and functions decay, and top-level qualifiers do not carry over. The
object is an ordinary local slot, so the ABI and pointer representation are
unchanged. `let mut name = ...` allows assignment to the local; a plain `let`
rejects a direct write with a diagnostic, including a member write through the
local. A write through a pointer (`*p = v`, `p->field = v`, `p[i] = v`) is
ordinary C aliasing and is not restricted, and `&name` keeps its normal pointer
type. `const` keeps its C meaning and is checked separately.
`#![disable_rustic_mut]` restores ordinary C writable defaults for `let`
declarations in that translation unit. Existing C declarations, including
`const` locals, are unchanged.

Payload enums (`enum(u8) Msg { Ping, Text(Str), Num(int) };`) are transparent
aggregates: the tag at offset 0 and one union of payloads at the aligned
payload offset. The tag uses the enum's underlying type, which is `int` when no
`enum(type)` is written. Construct with `Msg::Text(value)` or `Msg::Ping` and
read `.tag` and `.payload.<Variant>`. A payload enum is not an integer: it does
not take part in arithmetic or integer conversion. `switch` is the exception:
it loads the tag at offset 0 and compares it under C integer promotion.

`default:` and its `_:` spelling are the same switch arm. A payload enum is
closed, so a switch needs a `case Msg::Variant:` for every variant unless it
has a `default`/`_` arm; missing variants are diagnosed by name. Plain C enums
stay open: a switch without `default`/`_` is diagnosed, and
`#![disable_nonexhaustive_switch]` in the main input file disables that check.
Duplicate case values are always rejected. Global payload enum initialisers
are not supported. `--emit=c-header` prints the equivalent `struct` with a
`tag` member and a union `payload` member. The payload union keeps abi-plan's
union classification, so a float-only payload does not necessarily use the
same registers as an equivalent C struct.

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
