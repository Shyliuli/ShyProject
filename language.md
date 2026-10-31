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
}

impl Vec {
    fn push<T>(self *v, T value);
    fn get<T>(self *v, i32 index) -> T;
    fn size(self *v) -> i32;
}

fn identity<T>(T value) -> T;
```

`impl` remains a method-binding collection. It does not itself carry a generic
parameter; individual methods may be generic. `const N` is a compile-time
integer parameter:

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
ordinary C enums are open and missing `default` is a diagnostic. The diagnostic
can be disabled with a file-level or local `disable_nonexhaustive_switch`
attribute. Integer conversion follows the C-compatible enum rules and may be
warned about; it is never a hidden runtime operation.

## Result, Option, and `?`

`Result<T, E>` and `Option<T>` are library types. `Result<void, E>` may use an
integer niche when `E` is a payload-free enum that does not contain zero; zero
then represents success. If the condition is not met, the compiler uses the
ordinary transparent tagged representation and may issue a configurable
warning.

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
not use a hidden `dyn` object, allocation, or garbage collection. Closed
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

A Rustic unit defaults declarations and pointer targets to read-only access;
`mut` explicitly enables writes. `#![disable_rustic_mut]` makes the unit use
ordinary C writable defaults. This only affects checking and optimization; it
does not change the ABI, object layout, pointer representation, or lifetime.

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
- an exported generic must name concrete type and `const` arguments;
- `repr(C)`-style layout is required for a structure intended for external C;
- a trait value is a structure of function pointers;
- a payload enum is a visible tag and union payload;
- `Result<void, E>` is an integer only when its niche rule is valid;
- `mut`, Rustic defaults, and type-checking attributes are erased at the ABI.

The compiler already renders both directions for the plain-C subset:
`shyc --emit=c-header` prints an include-guarded header of the translation unit's
typedefs, record and enum definitions, non-`static` prototypes, and `extern`
object declarations, and `shyc --emit=c` prints the preprocessed unit as plain C
without the `#![...]` attribute lines. `static` declarations and `impl` blocks
are not exported yet: a method needs a concrete instantiated symbol before it can
appear in a C header.

## Backend boundary

`shyc` owns the language frontend and the typed IR. Its Shy and LLVM backends
consume the same IR and ABI plans. The chibicc fork is retained only for the
pre-release bootstrap and existing ShyOS validation path; it is not a target for
new Rustic language features.
