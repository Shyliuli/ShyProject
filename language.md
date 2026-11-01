# ShyC language direction

This document records the language boundary before the first public release. The
compiler currently has no published language version. The independent `shyc`
frontend is the main implementation; the chibicc fork remains an early
bootstrap and ShyOS compatibility validator.

## C compatibility

ShyC keeps C's direct memory model:

- pointers are ordinary addresses and may alias;
- there is no ownership, borrowing, lifetime, garbage collection, or hidden
  allocation model;
- `const` keeps its C meaning and may appear in the usual C positions;
- structures, integer types, pointer types, and ABI-visible layouts remain
  transparent;
- `drop` is deterministic scope cleanup, like C++ RAII, and does not imply
  ownership;
- every advanced type has a concrete C ABI representation or must be wrapped
  explicitly before crossing a C boundary.

A translation unit using the compatibility mode keeps C's default writable
semantics. Rustic checks are opt-in per translation unit and never change the
ABI.

## Generic types and functions

Generic declarations are compile-time constructs. Instantiation produces an
ordinary concrete type and ordinary concrete functions; no runtime type
parameter or hidden allocation is introduced.

```c
struct Vec<T> {
    T *data;
    i32 len;
    i32 cap;
};

impl Vec<T> {
    fn push(self *v, T value) { /* ... */ }
    fn get(self *v, i32 index) -> T { return v.data[index]; }
}

fn identity<T>(T value) -> T { return value; }
```

`impl Vec<T>` binds `T` for every method of the block; a method may add type
parameters of its own. A generic impl is instantiated in each translation unit
that uses it, so its methods are always written with their bodies, usually in a
header. `const N` is a compile-time integer parameter:

```c
struct Array<T, const N> {
    T data[N];
}
```

Type arguments may be inferred from arguments or an expected return type. If
there is insufficient information, the program must write the arguments.

## Enums and switch

Existing C enums remain integer-compatible and keep C-compatible storage by
default. Explicit storage is available with `enum(u8)`, `enum(u16)`, `enum(u32)`,
or `enum(u64)`.

Payload enums use a transparent `tag + max-payload` representation. The tag is
chosen as narrowly as possible unless an underlying type is explicitly given.

`switch` remains the control-flow construct. It supports the existing `default`
and `_` forms. Closed payload enums can be checked for exhaustive coverage;
ordinary C enums are open and missing `default` is a diagnostic. The implemented
`#![disable_nonexhaustive_switch]` file attribute disables the ordinary-enum
diagnostic; a local attribute form is not available yet. Integer conversion
follows the C-compatible enum rules and may be warned about; it is never a
hidden runtime operation.

## Result, Option, and `?`

`Result<T, E>` and `Option<T>` are library types. `Result<void, E>` uses an
integer niche when `E` is a payload-free enum that does not contain zero; zero
then represents success. If the condition is not met, the compiler uses the
ordinary transparent tagged representation. The niche is the error enum's
integer: `switch` on it is not available because its cases would compare the
`Err` tag with an error code, so switch on the error value instead.

`?` propagates only when the error types are equal or a later `From<Source>`
trait implementation explicitly converts the source error to the destination
error. There is no implicit error-code guessing.

## Traits

A trait is a compile-time interface and a runtime C-shaped function-pointer
interface when used as a value. A generic constraint is statically dispatched:

```c
trait Drawable {
    void draw(self *value);
}

fn render<T: Drawable>(T *value);
```

A trait value is equivalent to a transparent function-pointer structure. It does
not use a hidden `dyn` object, allocation, or garbage collection. Construction
is explicit: `Drawable::from<Circle>()` fills the table with the `impl` method
symbols, and `self *` is erased to `void *` because the implementation's
`Circle *` has the same C ABI. Closed
runtime polymorphism can instead use a tagged enum and an `impl Trait for Enum`
implementation with an exhaustive `switch`.

## `let`, `mut`, and `const`

`let` is type inference, similar to C++ `auto`:

```c
let value = expression;
let mut value = expression;
```

ShyC does not have bindings, ownership, or reference semantics. `mut` only
controls whether the object is writable in a Rustic translation unit; aliases
remain ordinary C aliases. `const` remains exactly C's type qualifier.

The frontend currently implements the `let` form itself: a `let` local without
`mut` is read-only, and a write through a pointer stays ordinary C aliasing.
Ordinary C declarations and pointer targets keep C's writable defaults until
the remaining Rustic checks land. `#![disable_rustic_mut]` makes the unit use
ordinary C writable defaults for `let` too. This only affects checking and
optimization; it does not change the ABI, object layout, pointer representation,
or lifetime.

## Slices and library types

A slice is a standard-library transparent structure containing a pointer and a
length. It is not a new pointer kind and does not own memory. Bounds checking,
unchecked access, allocation, and lifetime policy belong to the library API.

## C ABI and generated interfaces

Generic instances, trait interfaces, payload enums, `Option`, `Result`, and
slices have concrete layouts. C-facing exports use those concrete layouts and
mangled concrete symbols. A generated C header may provide readable typedefs
and `static inline` wrappers around the stable symbols.

C export rules:

- uninstantiated generics cannot cross a C ABI boundary;
- an exported generic must name concrete type and `const` arguments in the
  explicit instance table (`--instances`); an instance the unit only uses
  internally is not exported;
- only listed concrete instances cross the boundary; a listed instance shares
  one implementation with every alias that names it;
- the generated header and wrapper must be built for the same `--target` as the
  implementation, because the layout depends on the target's pointer width;
- `repr(C)`-style layout is required for a structure intended for external C;
- a trait value is a structure of function pointers;
- a payload enum is a visible tag and union payload;
- `Result<void, E>` is an integer only when its niche rule is valid;
- `mut`, Rustic defaults, and type-checking attributes are erased at the ABI.

The compiler already renders both directions for the plain-C subset:
`shyc --emit=c-header` prints an include-guarded header of the translation unit's
typedefs, record and enum definitions, non-`static` prototypes, and `extern`
object declarations, and `shyc --emit=c` prints the preprocessed unit as plain C
without the `#![...]` attribute lines. A generated header carries the main
file's `#include` lines and `#define` macros, takes its include guard from the
output path, and omits declarations that came from an included file, so it
describes exactly one header. `static` declarations and `impl` blocks are not
exported by that ordinary header: a method needs a concrete instantiated symbol
before it can appear in a C header.

### Explicit generic instance table

A generic record, enum, or function crosses into C only when a separate
compile-time table names it. Pass that table with `--instances <file>`:

```text
// api.shyinst
Vec<int> as vec_int,
Array<int, 16> as int_array_16,
identity<int> as identity_int;
```

Entries are separated by `,` or `;`; newlines, `//` comments, and
`/* ... */` comments are free. Each entry names one generic template and its
concrete arguments, followed by `as` and a unique C identifier. The arguments
are parsed with the ordinary ShyC type and constant-expression grammar, so
nested arguments (`Vec<Vec<int>>`), pointer arguments (`Pair<u8 *>`), and
constant expressions (`Array<char, 2 * 8>`) keep their usual spelling. Two
aliases for the same concrete instance (`Vec<int> as vec_int, Vec<int> as vint`)
instantiate it once and share one implementation. A trait-qualified entry such
as `Drawable for Vec<int>` is rejected with an explicit diagnostic; trait
implementation tables are not exported yet. A variadic generic function and an
inherent impl whose methods declare their own type parameters are rejected too:
a C wrapper cannot forward a variadic list, and `<alias>_<method>` cannot name
one instance of a generic method.

The table is compile-time input, never C source. One implementation build and
one header/wrapper build are ordinary compiler invocations:

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

The header exports exactly the listed instances:

- each concrete generic record exports its C layout as a `typedef` under the
  alias (`typedef struct shyc_R... vec_int;`) and its inherent methods as
  `<alias>_<method>` (`vec_int_push`, `vec_int_get`);
- each concrete generic enum exports its layout or integer representation under
  the alias; enumerators stay in ShyC spelling;
- each generic free function is exported as `<alias>` (`identity_int`).

An instance the unit uses but the table does not list stays internal and has no
C name; a listed instance the unit never uses is still instantiated. The
concrete functions themselves get stable public mangled symbols
(`shyc_inst_...`), shared by every alias of one instance.
`--emit=c-wrapper` turns each alias into a thin C wrapper around the concrete
symbol and repeats the layouts it needs, so the file compiles on its own. Plain
(non-generic) types in an exported signature use the spelling the ordinary
header already prints; a generic layout dependency of a listed record (the inner
`Vec<int>` of `Vec<Vec<int>>`, for example) is printed as needed.

The header and wrapper describe the target they were generated for, so pass the
same `--target` and preprocessing options used for the implementation; pointer
width and preprocessed declarations change the concrete layout and the exported
symbol names. Without `--target` the Shy target layout (32-bit pointers) is
used.
`--emit=c-header` without `--instances` keeps the ordinary AST export
unchanged.

## Backend boundary

`shyc` owns the language frontend and the typed IR. Its Shy and LLVM backends
consume the same IR and ABI plans. The chibicc fork is retained only for the
pre-release bootstrap and existing ShyOS validation path; it is not a target for
new Rustic language features.
