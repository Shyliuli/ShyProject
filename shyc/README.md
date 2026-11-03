# shyc

`shyc` is an independent ShyC compiler: its own preprocessor, parser, type
checker, typed IR, optimiser, and two code generators. It is the default
backend of `shycc`; `--backend=chibicc` keeps the old bootstrap path.

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

Export concrete generic instances under explicit C names:

```text
// api.shyinst
Vec<int> as vec_int,
Array<int, 16> as int_array_16,
identity<int> as identity_int;
```

```sh
target/bin/shyc --emit=llvm --target=x86_64-pc-linux-gnu lib.shyc \
  --instances api.shyinst -o lib.ll
target/bin/shyc --emit=c-header --target=x86_64-pc-linux-gnu lib.shyc \
  --instances api.shyinst -o api.h
target/bin/shyc --emit=c-wrapper --target=x86_64-pc-linux-gnu lib.shyc \
  --instances api.shyinst -o api_wrapper.c

clang -x ir -c lib.ll -o lib.o
clang -c api_wrapper.c -o api_wrapper.o
clang client.c api_wrapper.o lib.o -o client
```

`--emit=c` accepts the same table: a generic use then prints the alias the
table gives it (`Vec<int>` prints as `vec_int` instead of `Vec_int`), so a
translated unit and the generated header name one instance the same way. The
table itself is not printed by that output: an instance is a symbol of its own,
and the header or wrapper provides it.

The instance table is compile-time input, not C source. `--instances` names the
concrete generic records, enums, and functions that may cross the C boundary:
everything else stays internal to its translation unit. Each entry is
`Name<arguments> as alias`; entries are separated by `,` or `;`, whitespace and
`//` / `/* ... */` comments are free, and arguments use the ordinary ShyC type
and constant-expression grammar, so `Vec<Vec<int>>`, `Pair<u8 *>`, and
`Array<char, 2 * 8>` parse as usual. The alias must be a unique C identifier.
Two aliases for one concrete instance (`Vec<int> as vec_int, Vec<int> as vint`)
instantiate it once and share the implementation. A trait-qualified entry such
as `Drawable for Vec<int>` is rejected with an explicit diagnostic, as are the
cases the table cannot name: a variadic generic function, and an inherent impl
whose methods declare type parameters of their own (one instantiation of such a
method has no `<alias>_<method>` spelling).

With an instance table, `--emit=c-header` adds to the ordinary header:

- a `typedef` of the concrete C layout under each record or enum alias
  (`typedef struct shyc_R... vec_int;`);
- one prototype per inherent method named `<alias>_<method>`
  (`vec_int_push`, `vec_int_get`);
- one prototype per generic free function named `<alias>` (`identity_int`).

The concrete instantiation is exported once under a stable `shyc_inst_...`
symbol, and `--emit=c-wrapper` prints a self-contained C file with one thin
wrapper per alias that calls it. Pass the same `--target` and the same
preprocessing options (`-I`, `-D`, `-U`) to the output invocations as to the
implementation build, because the pointer width and the preprocessed source
decide the concrete layout and symbol names; without `--target` the Shy target
layout is used.
`--emit=c-header` without `--instances` keeps the ordinary AST export
described below.

`--emit=c-header` walks the parsed translation unit and prints an include-guarded
header: file-scope typedefs, record and enum definitions, prototypes for every
non-`static` function, and `extern` declarations for objects (initializers are
dropped). The guard comes from the output path, and the header carries the main
file's `#include` lines and `#define` macros before the declarations, which are
wrapped in `#ifdef __cplusplus` / `extern "C"`. Declarations that came from an
included header are not repeated, so the generated header describes exactly one
source header and leaves its dependencies to the carried includes. `static`
declarations and `impl` blocks are not exported by that ordinary header; a
method needs a concrete instantiated symbol before it can be declared, which is
what the instance table above provides. A `trait` is exported as a forward `typedef` plus
the `struct` of function pointers that its value type uses.

`--emit=c` prints the same unit as plain C. An inferred local becomes
`__auto_type` (`const __auto_type` without `mut`), an `impl` block becomes one
mangled function per method with its receiver as the first parameter, and a
method call becomes a call to that function: `p.add(1)` is
`Point_add(&p, 1)`. ShyC's dot dereferences a pointer, so the C output says
`->` where the source wrote `.`. A `trait` value is the `struct` of function
pointers that `Trait::from<T>()` fills, `value?` becomes a statement
expression whose failure arm returns the enclosing function's failure variant,
`asm!` becomes a GCC extended asm statement, and a generic use prints as the
instance name (`Vec<int>` is `Vec_int`).
What C cannot spell stays out rather than being guessed: a generic function or
record prints nothing, because an instance is a symbol of its own, and a trait
method that only has a default body stops the translation with a diagnostic.

Options: `-I`, `-D name[=value]`, `-U name`, `-o file`, `--instances <file>`,
`-O0`..`-O3`
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

`shycc` links libshy by default because it owns the whole Shy link step, and
passes `--gc-sections` so only the reachable functions stay; `-llibshy` and
`-lfloat` are still accepted and never duplicated. On a hosted LLVM target
`shycc` links libshy's core, alloc, and std over the system libc. `shyc
--emit=llvm` alone stops at LLVM IR, so the libraries come from whatever links
the IR.

The preprocessor defines `__SIZEOF_POINTER__` (4 on Shy, 8 on every LLVM
target) for hosted and freestanding builds, and `__shy__` only for the Shy
target.

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
Several bounds may be written with `+`. A bound may also name a parameterized
trait: `fn convert<T: From<int>>(T value) -> ...` or `fn bridge<T, U: From<T>>`.
The trait arguments resolve in the same concrete type-parameter environment as
the function, and the call is accepted only when the unit contains the matching
`impl From<int> for Type` (or `impl<T> From<T> for ...`). The check is
compile-time only: the call still becomes the ordinary monomorphized function,
with no dispatch or ABI change. A bound on a `const` parameter, a generic
record, a generic enum, or a generic trait, and a bound on a generic method's
own parameter each get an explicit diagnostic instead.

A generic record takes methods through a template `impl`:
`impl<T> Box<T> { T get(self *b) { return b->value; } }`, where the short
`impl Box<T>` form writes the same parameters. A method lookup on one concrete
instance unifies the target with that instance's own arguments and lowers
every method it uses to an ordinary function with a translation-unit internal
symbol, so `Box<int> b; b.get();` stays a direct call. A target may reorder or
repeat its parameters, as in `impl<T, U> Trait for Pair<U, T>`, and its
arguments may be concrete, nested, pointer, or array types:
`impl<T> Trait for Pair<T, int>` applies only to a `Pair<..., int>`, and
`impl<T> Trait for Box<Vec<T>>` binds `T` through `Box<Vec<int>>`. A template
whose concrete parts do not agree with the receiver does not apply, so
disjoint targets such as `Pair<T, int>` and `Pair<T, long>` can coexist.
Repeating a parameter is checked against the concrete receiver, so
`Pair<T, T>` accepts `Pair<int, int>` and reports `Pair<int, long>`. An impl
parameter that never appears in the target is rejected.

A generic impl may implement a trait: `impl<T> Show for Box<T> { ... }`, or
with reordered bindings `impl<T, U> Convert<T> for Pair<U, T> { ... }`. The
trait arguments resolve after the impl parameters bind, and a materialized
implementation is keyed by trait + concrete receiver + resolved trait arguments.
`Show for Box<int>` and `Show for Box<long>` are therefore distinct, and so are
`Convert<int> for Pair<U, T>` and `Convert<long> for Pair<U, T>`. A record may
have several generic inherent impls when their targets are disjoint; two
templates that both match one receiver are reported when that receiver is
instantiated. It may also have generic impls of several different traits. A
trait bound on an impl parameter still gets an explicit diagnostic, and a
second generic impl of the same trait arguments and concrete receiver is
reported when the receiver is instantiated.

A method may declare generic parameters of its own:
`fn identity<U>(self *b, U value) -> U`. Such a method is a template inside its
impl and instantiates through the same cache as a generic function, with the
receiver bindings and method arguments merged. A call may write them
(`b.identity<int>(1)`, `Box<int>::identity<int>(&b, 1)`) or let them be
inferred from the value arguments (`b.identity(1)`). A method parameter that
shadows an impl parameter is rejected.

`trait Convert { fn convert<T>(self *x, T value) -> T; }` declares a generic
trait method. The implementation must declare the same number and kinds of
method parameters. The two signatures are compared with one fresh placeholder
type per parameter, so a concrete type cannot stand in for a parameter and two
different parameters can never be confused. Static dispatch works: a concrete
receiver call, a `Circle::convert<int>(&c, value)` static call, and a call
inside a generic function bounded by the trait all instantiate the ordinary
implementation method. A generic trait method has no single function pointer,
so a trait that declares one cannot be used as a value type; that use gets a
diagnostic naming the method. Trait bounds on a generic method parameter and
const generic parameters on a trait method are not implemented yet.

`trait Drawable { void draw(self *value); }` declares method signatures, and
`impl Drawable for Circle { ... }` checks that the implementation defines exactly
those methods with matching signatures. A method of a concrete
`impl Trait for Type` lowers to `____Trait__Type__method`, and a parameterized
trait adds one `__tN` (type argument) or `__n<bits>` (`const` argument) part per
resolved trait argument: `impl From<int> for Target` gives
`____From__Target__from__tN`. A method of a materialized generic impl keeps an
internal symbol that names the trait, its arguments, and the concrete receiver.
The trait name
and arguments are part of the symbol, so two traits may declare the same method
name for one type, and one trait may be implemented for several argument
values. An inherent `impl Type` keeps the plain `____Type__method` spelling.
A call on a concrete type stays statically dispatched. An inherent method wins
when it has the same name as a trait method, and a receiver call whose name is
provided by more than one trait implementation is diagnosed as ambiguous.

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
same trait and trait arguments for one concrete type, and a trait with a
generic method used as a value type, each get an explicit diagnostic. The same
method name in two traits is allowed; the table is built from each trait's own
implementation, while a receiver call needs an unambiguous name. Construction
at file scope must follow the `impl` in source order;
function bodies are lowered after the whole unit is collected, so their
construction does not depend on declaration order. There is no other dynamic
dispatch mechanism.

`--emit=c-header` prints a forward `typedef` for every trait and the matching
`struct` of function pointers, with `self` erased to `void`, so a C caller sees
the same layout. Implementation methods still have no exported prototypes: a C
translation unit that builds a table by hand must declare the
`____Trait__Type__method` symbols itself, including their resolved trait
argument suffixes.

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
`impl From<Source> for Target`; there is no implicit error-code guess. Several
sources may convert to one target: each `impl From<Source>` has its own symbol
and the conversion is selected by the actual source and target types. A
payload-free `Ok` produces no value, so `?` on it is only usable as a
statement.

`#[result_niche]` on a generic enum declaration enables the compact
`Result<void, E>` representation when `E` is a payload-free enum without a
zero enumerator. The object has `E`'s underlying integer size and alignment:
zero means `Ok`, and nonzero is the `Err` payload. `#[option_niche]` enables
compact `Option<T *>`: the object is a pointer, NULL means `None`, and a
non-null value is `Some`. The attributes also accept the commented forms
`// #[result_niche]` and `// #[option_niche]`. Export instance tables only
select concrete types and aliases; they do not carry these attributes.

These are C-compatible representation contracts. In particular, `Some(NULL)`
is equivalent to `None`; it does not preserve a separate null payload.
Ineligible instances retain their ordinary tag and union representation.

`switch` works with logical variant labels for both compact representations:
`case Option<int *>::None:` / `case Option<int *>::Some:`, and
`case Result<void, Error>::Ok:` / `case Result<void, Error>::Err:`. Payloads
remain accessible through `.payload.Some` and `.payload.Err`. The scrutinee is
evaluated once, then a zero/nonzero comparison supplies the logical variant
tag. `.tag` remains the raw stored value. `?` propagates failure on NULL for
Option and on nonzero for Result. Generated C headers and wrappers use the
same pointer/integer types, with no extra tag or union. Global initialisers
keep the payload-enum restriction.

A file-scope `use` is an include plus a symbol-table import. It expands the
module header before parsing, so declarations are available without repeating
the module path:

```c
use core::option::Option;
use core::option::*;
use c::stdio::*;
use crate::include::api::*;
use self::include::local::*;
```

A named path such as `use core::option::Option;` includes
`core/option.shyh` and checks that `Option` is declared. A glob includes the
module and imports its file-scope declarations. The `c::` view resolves to a
plain `.h` (for example, `c/stdio.h`); other modules resolve to `.shyh`, while
generated C headers record the corresponding `.h` include. `crate::` paths start at the nearest repository/manifest ancestor, falling
back to matching `-I` roots or the main source directory; `self::` paths are
relative to the importing file. Included declarations feed the same parser, lowerer, symbol
index, and LSP analysis as textual includes. `use` itself emits no runtime
symbol, and generated C output carries the resolved header include.

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
