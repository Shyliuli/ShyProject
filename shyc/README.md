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
dropped). The guard comes from the output path, and the header carries the main
file's `#include` lines and `#define` macros before the declarations, which are
wrapped in `#ifdef __cplusplus` / `extern "C"`. Declarations that came from an
included header are not repeated, so the generated header describes exactly one
source header and leaves its dependencies to the carried includes. `static`
declarations and `impl` blocks are not exported; methods need a concrete symbol
before they can be declared. A `trait` is exported as a forward `typedef` plus
the `struct` of function pointers that its value type uses. `--emit=c` prints the
preprocessed unit as plain C and drops the `#![...]` attribute lines. A `let`
declaration has no C spelling, so a unit that uses one is rejected with a
diagnostic instead of being printed as invalid C.

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
relocations, ShyC's `impl`/`self`/`drop`, compile-time traits and trait values,
and floating point through the soft-float runtime.

Generics are compile-time templates. `fn identity<T>(T value) -> T` and
`struct Pair<T> { ... }` are instantiated on use; an explicit `identity<int>(1)`
and an inferred `identity(1)` instantiate through the same cache, so a repeated
call and the type-only analysis before lowering reuse one instance. Inference
uses the value arguments only: a type parameter binds through `T`, `T *`,
`T [N]`, and generic records or enums such as `Pair<T>`. A `const` parameter, a
type parameter that appears only inside a function type, and an argument list
that binds one parameter inconsistently or not at all are not inferred; those
calls report that explicit generic arguments are needed. A local binding or
ordinary function of the same name shadows the template.

A generic function may constrain its type parameters with a static trait bound:
`fn render<T: Drawable>(T *value) -> ...`. The bound is checked when the
template is instantiated, after an explicit argument or inference binds `T`, and
the call is accepted only when the unit contains `impl Drawable for Type`.
Several bounds may be written with `+`. The check is compile-time only: the call
still becomes the ordinary monomorphized function, with no dispatch or ABI
change. A bound on a `const` parameter, a generic record, a generic enum, or a
generic trait, a bound that writes trait arguments such as `T: From<int>`, and
a generic trait method each get an explicit diagnostic instead.

A generic record takes methods through a template `impl`:
`impl<T> Box<T> { T get(self *b) { return b->value; } }`, where the short
`impl Box<T>` form writes the same parameters. A method lookup on one concrete
instance binds the impl parameters to that instance's own arguments and lowers
every method it uses to an ordinary function with a translation-unit internal
symbol, so `Box<int> b; b.get();` stays a direct call. The target must list the
impl's parameters in order, and a method cannot declare generic parameters of
its own. A generic trait implementation, a trait bound on an impl parameter, and
a second generic impl of the same record each get an explicit diagnostic.

`trait Drawable { void draw(self *value); }` declares method signatures, and
`impl Drawable for Circle { ... }` checks that the implementation defines exactly
those methods with matching signatures. The methods are lowered exactly like an
ordinary `impl`: they become plain `____Circle__draw` functions and a call on a
concrete type stays statically dispatched.

A trait name can also be used as an object or field type. It denotes a
transparent record with one function-pointer field per method, named after the
method, and no hidden data pointer, vtable header, or allocation:

```c
trait Drawable { void draw(self *value); }
struct Widget { Drawable drawable; void *data; };

struct Circle { int r; };
impl Drawable for Circle {
    void draw(self *value) { value->r = value->r + 1; }
}

int main() {
    Circle c = { 3 };
    Widget w = { Drawable::from<Circle>(), &c };
    w.drawable.draw(w.data);      // indirect call through the table
    return c.r;                   // 4
}
```

`Trait::from<Type>()` fills every field with the `impl Trait for Type` method
symbol. The method signature is erased only at `self`: `self *` becomes
`void *`, which has the same C ABI as the implementation's `Circle *`; every
other parameter and the return type stay as written. This is why a trait method
that takes or returns `self` by value still works for static dispatch but is
rejected when the trait is used as a value type. A second implementation of the
same trait for one type, a duplicate method name across two traits on one type,
and generic trait methods also get an explicit diagnostic. Construction at file
scope must follow the `impl` in source order;
function bodies are lowered after the whole unit is collected, so their
construction does not depend on declaration order. There is no other dynamic
dispatch mechanism.

`--emit=c-header` prints a forward `typedef` for every trait and the matching
`struct` of function pointers, with `self` erased to `void`, so a C caller sees
the same layout. Implementation methods still have no exported prototypes: a C
translation unit that builds a table by hand must declare the
`____Type__method` symbols itself.

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

`Option<T>` and `Result<T, E>` are ordinary payload enums that use the variant
names `None`/`Some` and `Ok`/`Err`, so postfix `?` is structural: it copies the
success payload out and returns the enclosing function's failure variant.
`?` accepts the same error type, or a differing one that has an
`impl From<Source> for Target`; there is no implicit error-code guess. A
payload-free `Ok` produces no value, so `?` on it is only usable as a
statement.

An instantiated `Result<void, E>` whose `E` is a payload-free enum with no
zero enumerator takes the zero-value niche instead of the tagged aggregate: the
object is `E`'s integer, `0` means `Ok`, and any other value is the error.
`Ok` must be the first enumerator so its tag is `0`. The niche is an integer
(`.tag` reads that value, and arithmetic and integer conversion treat it as
`E`'s underlying type), has `E`'s size and alignment, and crosses both backends
as an integer. An instance whose `E` contains `0`, and a hand-written
`enum { Ok, Err(E) }`, keep the ordinary tagged representation. `switch` on a
niche Result is diagnosed because its cases would compare the `Err` tag with an
error code; switch on `result.tag` and the error enumerators instead. Global
initialisers keep the payload-enum restriction, and `?` needs the same error
enum because `impl` targets must be records, not enums.

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
