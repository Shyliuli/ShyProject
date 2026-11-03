# ShyC

ShyC is the current bare-metal C ABI and language subset for ShyISA. The
frontend is the local chibicc fork under `third_party/chibicc`.

Implementation notes for maintaining the local fork are in
`third_party/chibicc/SHY_FORK.md`.

## Goals

- Compile C source files to Shy assembly (`.shy`).
- Assemble `.shy` files to Shy object files (`.sobj`).
- Link `.sobj` files with the project linker.
- Do not link a C standard library by default.

The intended pipeline is:

```sh
third_party/chibicc/chibicc --target=shy -S -o file.shy file.c
cargo run -q -p asm -- file.shy -o file.sobj
cargo run -q -p linker -- file.sobj other.sobj -o program.sfs --sym program.sym
```

## Target Mode

Use `--target=shy` when invoking chibicc. This selects the Shy backend and
defines `__shy__`.

The compiler emits assembly only for this target. Runtime support is not linked
implicitly. If compiler helper functions are needed, compile and link the
runtime source explicitly.

## Source Line Annotations

The host Shy toolchain can optionally preserve a lightweight source-to-assembly
mapping in generated `.shy` files:

```sh
cargo run -q -p shycc -- --shy-emit-source-lines -S file.shyc -o file.shy
third_party/chibicc/chibicc --target=shy --shy-emit-source-lines -S -o file.shy file.shyc
```

When enabled, the Shy backend emits comment lines before the function symbol and
before generated code for source statements:

```asm
.section text.main
//source file.shyc:5 int main(){
.symbol main
//source file.shyc:6   return 0;
setn 1x 0
```

The format is:

```text
//source <file>:<line> <source line text>
```

These lines are ordinary Shy assembly comments, so existing assembly and linking
behavior is unchanged. They are consumed by `shy-dwarf` (via the `.shy.debug.json`
sidecar produced by `chibicc`) to generate DWARF debug info for GDB.

To produce a debug ELF alongside the `.sfs` image, use:

```sh
cargo run -q -p shycc -- --shy-emit-source-lines --shy-emit-debug-info \
  input.c -o app.sfs --sym app.sym
```

This writes `app.debug.elf`, which the emulator serves to GDB over the RSP
`qXfer:exec-file:read:` packet. GDB can then show source lines, local variables,
function parameters, and struct/array/pointer values. The option is off by default.

## Execution Model

This is a freestanding, bare-metal target.

- No hosted C standard library is assumed.
- No system calls are assumed.
- Startup is minimal.
- TLS is unsupported.

If a translation unit defines `main`, the backend emits `_start`:

- initializes `sp` to `0x00fff000`;
- calls `main` with `argc = 0` and `argv = NULL`;
- exits with `seta exit 1x`, using the value returned by `main`.

This startup can be disabled with the ShyC source extension:

```c
#![no_main]
```

The main input file can also declare resource hints for the final `.sfs`:

```c
#![mem(10M)]
#![stack(4K)]
```

`mem` and `stack` each may appear at most once per translation unit, at the
start of any source line in the main input file, including before or after
`#include` lines. The suffixes `k`, `K`, `m`, and `M` are supported. The compiler
forwards these directives to Shy assembly, the assembler stores them in `.sobj`,
and the linker sums hints across all input objects. If no object declares a
memory hint the linked image uses `32M`; if no object declares a stack hint it
uses `4K`.

When the main input file contains `#![no_main]` at the start of a source line,
the backend does not emit the automatic `_start`, does not initialize `sp`, does
not call `main`, and does not install any runtime startup code. The linked image
must provide a symbol named `_start`.

In `#![no_main]` mode, a C function named `_start` is emitted as a bare entry:

- no function prologue;
- no frame-pointer setup;
- no stack allocation;
- no parameter save area;
- no function epilogue.

This makes the first emitted instruction of `_start` come from the source. If
`_start` falls through or executes `return`, it branches to itself. A practical
entry function should initialize `sp` before using C constructs that need the
stack, or should jump/call into another C function after setting up the machine
state.

Example:

```c
#![no_main]

void kmain(void) {
    asm("oututfn 79\noututfn 75\noututfn 10\nsetn exit 0");
}

void _start(void) {
    asm("setn sp 0x00fff000\ncalln kmain");
}
```

## Data Model

ShyC uses an ILP32-style pointer model with 64-bit `long`:

| C type | Size |
| --- | ---: |
| `char` | 8 bits |
| `short` | 16 bits |
| `int` | 32 bits |
| `long` | 64 bits |
| pointer | 32 bits |
| `size_t` | 32 bits |
| `ptrdiff_t` | 32 bits |

ShyISA addresses are 32-bit, so pointer values are represented directly as
32-bit machine addresses. `long` remains 64-bit for code that needs a wider
integer type. This means ShyC is not LP64 even when a host compiler used to
build tools is LP64:

- `sizeof(void *) == 4`;
- `sizeof(size_t) == 4`;
- `sizeof(ptrdiff_t) == 4`;
- `sizeof(long) == 8`.

The libshy headers follow this model. `<stdint.h>` provides fixed-width
integer names such as `int32_t`, `uint64_t`, `intptr_t`, and `uintptr_t`.
`intptr_t` is `int`, and `uintptr_t` is `unsigned int`. `<stdtype.h>` provides
Rust-style aliases:

| Alias | Type |
| --- | --- |
| `i8`, `i16`, `i32`, `i64` | signed fixed-width integers |
| `u8`, `u16`, `u32`, `u64` | unsigned fixed-width integers |
| `isize` | `ptrdiff_t` |
| `usize` | `size_t` |
| `f32` | `float` |
| `f64` | `double` |

## Calling Convention (ABI v1)

This is the host chibicc backend's ABI and the contract for the independent
Rust backend under development.
The stack grows upward; stack pointers and argument blocks are 4-byte aligned.
Object layout and padding inside a struct or union are unchanged by argument
passing. Memory representation is big-endian.

### Registers and results

Scalar return values use:

- `1x` for 32-bit and smaller values;
- `1x` low 32 bits and `2x` high 32 bits for 64-bit values.

`fx` is the frame pointer and is callee-saved. All other general-purpose
registers (`0x` through `ex`) and the comparison state `rs` are caller-saved.
The callee restores `sp` to its entry value before `ret`; after `ret`, the
caller releases its own stack arguments. Inline assembly in an ordinary
function must preserve `fx` and restore `sp` before leaving the assembly block.
The bare `_start` entry is exempt from this frame convention.

Struct and union results use caller-allocated storage. A hidden pointer to this
storage occupies the first argument register (`4x`); explicit scalar arguments
then start at `5x`. The callee copies the result into that storage. Scalar
result registers do not carry a struct result.

### Argument placement

Visit arguments in source order, with the hidden result pointer first:

1. Scalar arguments use the next available slots in `4x, 5x, 6x, 7x, 8x, 9x,
   ax, bx`. A value of 32 bits or less uses one slot; a 64-bit value uses two
   slots, low word first. Narrow integer arguments are sign- or zero-extended
   according to their type.
2. A scalar that does not fit entirely in the remaining registers goes on the
   stack. It is never split between registers and memory. Unused register slots
   remain available to later scalar arguments.
3. Structs and unions are always copied by value onto the stack, including
   small aggregates. They do not consume argument registers. The callee receives
   an independent value: changing its parameter must not change the caller's
   object. Arrays and functions undergo the usual parameter-to-pointer adjustment.

Every stack argument occupies `round_up(sizeof(argument), 4)` bytes. Its object
bytes begin at the start of its block in native big-endian memory order; unused
padding at the end of the block has no specified value. The argument block is
an ABI transport area, not a promise of stronger object alignment. The callee
copies the value into its own parameter object.

Let `B` be `sp` immediately before the call instruction, after allocating stack
arguments. The first stack argument occupies the block immediately below `B`;
subsequent stack arguments occupy progressively lower addresses. For stack
argument `i`, let `S_i` be the cumulative padded sizes of stack arguments through
`i`. Its address is `B - S_i`. Register arguments do not contribute to `S_i`.
This order makes each named argument address independent of how many variadic
arguments follow it. Placement order does not prescribe C expression evaluation
order; nested calls must not overwrite already evaluated arguments.

After the normal callee prologue (`pusha fx; seta fx sp`), the layout is:

```text
higher addresses
  callee locals and spill slots       [fx, ...)
  saved caller frame pointer          fx - 4
  return address                      fx - 8       (= B)
  first stack argument                B - S_1
  second stack argument               B - S_2
  ...
  caller locals and temporaries
lower addresses
```

The caller reserves the entire stack-argument area **above its current `sp`**
before the call, then releases exactly that area after return. This preserves
active VLA/`alloca` storage below `sp`. While evaluating arguments, nested calls
use space above the reserved area and return with `sp` restored. There is no
stack-argument count limit in the ABI apart from available address space and
stack memory.

### Variadic functions

Variadic arguments use the same placement rules as named arguments, after the
usual default argument promotions. `va_list` is an array containing one cursor
structure: the next argument-register slot, a pointer to a saved eight-slot
register area, and a descending stack cursor. The callee initializes this state
after classifying its named parameters (including a hidden result pointer).

`va_start` copies that initial state. `va_arg` takes a scalar from the register
save area if the entire value fits; otherwise it subtracts the padded object
size from the stack cursor and reads that block. Aggregates always take the
stack path. Register-save slots preserve the low-word-first register convention;
64-bit values read there must be reconstructed into big-endian object order.
Stack values are already in native object order. `va_copy` copies the cursor
state independently.

### Compatibility

ABI v1 preserves existing scalar-only calls that fit in the eight argument
registers, including the hidden struct-result pointer convention. Aggregate
parameters and the `va_list` representation are not compatible with the old
experimental backend. Rebuild affected objects, libshy, and generated programs;
do not mix old and new aggregate/variadic implementations in one linked image.

## Symbols and Sections

Generated assembly uses:

- `text.<symbol>` for functions;
- `data.<symbol>` for global data.

Private compiler labels are renamed per input file so separate `.c` files can be
compiled independently and linked later.

## Supported C Surface

The current target is a practical bare-metal C frontend, not yet a full hosted C
implementation.

Currently supported or partially supported:

- integer, pointer, array, struct, and union frontend constructs handled by
  chibicc;
- globals and local variables;
- `if`, `while`, `for`, `do`, `switch`, `case`, and `default`;
- labels and `goto`;
- function calls using register arguments, stack overflow arguments, and by-value
  struct/union parameters;
- 32-bit arithmetic and comparisons;
- 64-bit integer values using register pairs;
- global pointer relocations;
- `char`, `short`, `int`, `long`, and pointer loads/stores;
- basic varargs using 32-bit argument slots;
- simple atomics where the operation maps directly to ShyISA support.

Unsupported or intentionally incomplete:

- hosted standard library;
- TLS;
- complete IEEE 754 behavior;
- full C11/C17 atomics semantics;
- strict compatibility with another platform ABI.

## ShyC Source Extensions

Native ShyC source files use `.shyc`; native ShyC headers use `.shyh`. The
driver still accepts `.c` and `.h` for C-compatible inputs.

ShyC performs a top-level pre-scan before parsing function bodies. The pre-scan
registers global typedefs, struct/union/enum tags, global functions, and `impl`
methods for the whole translation unit. This means calls can target functions or
methods that are written later in the file:

```c
int main(void) {
  return add1(2);
}

int add1(int x) {
  return x + 1;
}
```

Unknown names are still rejected. The pre-scan does not parse function bodies and
does not generate code for global initializers.

Struct and union tags are also usable as type names in ShyC. This removes the
usual C boilerplate `typedef struct Name Name;`:

```c
struct Counter {
  int value;
};

Counter c;
```

Methods are written in an `impl` block:

```c
impl Counter {
  Counter new(int value) {
    Counter c;
    c.value = value;
    return c;
  }

  int add(self *s, int n) {
    s.value = s.value + n;
    return s.value;
  }
}
```

`self` is valid only inside an `impl` block and names the implemented type.
An inherent method symbol is:

```text
____TYPE__method
```

For example, `Counter::new` lowers to `____Counter__new`. A trait method adds
the trait name, and a parameterized trait appends its resolved arguments, so
implementations of several traits or several trait argument values never
collide:

```text
____TRAIT__TYPE__method
____TRAIT__TYPE__method__tN        // one type argument
____TRAIT__TYPE__method__n<bits>   // one const argument
```

For example, `impl Drawable for Counter` gives `____Drawable__Counter__draw`,
and `impl From<int> for Counter` gives `____From__Counter__from__tN`.

Static-style calls use `Type::method(args...)`. Instance calls use dot syntax and
pass the receiver as the first `self` argument:

```c
Counter c = Counter::new(3);
c.add(4);

Counter *p = &c;
p.add(5);
```

The dot operator also auto-dereferences struct and union pointers for both field
access and method calls.

A record can expose another object through `Deref<Target>` / `DerefMut<Target>`
(`<core/deref.shyh>`). When the pointer walk above finds no member, the compiler
follows one such implementation, up to eight links, so a wrapper behaves like
the object it projects:

```c
#include <core/deref.shyh>
#include <core/str.shyh>

struct Entry {
    Str name;
    int id;
};

impl Deref<Str> for Entry {
    fn deref(const self *s) -> const Str * {
        return &s.name;
    }
}

int main(void) {
    Entry e = { Str::from_cstr("shy"), 7 };
    if (!e.starts_with("sh")) return 1;
    if (e.len != 3) return 2;
    return 0;
}
```

Reading a projected field or calling a method whose receiver is `const self *`
needs `Deref`; writing a projected field or calling a method whose receiver is a
mutable `self *` needs `DerefMut`, which must pair with a `Deref` of the same
target. The traits return ordinary C pointers: they add no ownership, borrowing,
or lifetime checking, and the returned pointer must designate a live target for
the duration of the access. A record may implement each trait once; a second
`Deref` implementation makes that projection ambiguous.

### Linkage

ShyC declarations are **file-local by default**, the same shape as `let` being
read-only by default: the conservative choice is implicit, and opening it up is
written out.

| Declaration | Default | `pub` | `static` |
|---|---|---|---|
| `fn` function | file-local | exported | file-local |
| impl method | file-local | exported | file-local |
| global object | file-local | exported | file-local |
| struct/union field | always accessible | — | — |

`pub` is accepted in front of every `fn`, method and global. Fields are C
style: they carry no `pub` and are always accessible.

A prototype is never file-local: it names something defined elsewhere, and a
definition in the same unit adopts the prototype's symbol.

`#![disable_default_static]` restores the C default for one file, so only
`static` gives a file-local symbol and `pub` becomes an empty token. It is the
linkage twin of `#![disable_rustic_mut]`, and it is what a C-compatible
library wants.

A file that must stay valid C can write the attribute inside a line comment —
`// #![disable_default_static]` on its own line, optionally indented. C tools
see an ordinary comment, and shyc still reads it:

```c
// #![disable_default_static]
int fputs(const char *s, FILE *f) { /* C 写法的实现 */ }
```

The C-compatible layer of a library uses exactly this; the ShyC-style layers
write `pub` on what they export instead:

```c
#![disable_default_static]   // 这个文件按 C 的默认来
fn helper(void) -> int;      // 对外
static fn local(void) -> int { return 1; }   // 只在本文件
```

A method of an inherent impl may be declared without a body, like a C
prototype. A header declares the methods and one translation unit defines them:

```c
// str.shyh
struct Str { const char *ptr; int len; };
impl Str {
  fn eq(self *s, Str other) -> int;
}

// str.shyc
#include "str.shyh"
impl Str {
  fn eq(self *s, Str other) -> int { /* ... */ }
}
```

A header can define a helper or a whole trait implementation without a
duplicate symbol in every unit that includes it: a `fn` is already file-local,
so nothing has to be written for the private case, and `pub` is the only way to
export one. `static` stays available as the explicit spelling of the same
linkage, which is what a file under `#![disable_default_static]` needs.
Methods of a generic or trait impl always have bodies; a generic impl is
compiled where it is used. A trait declaration may give a method a default
body, and an `impl` that omits the method inherits it.

If a type implements `drop(self *s)`, local values of that type are treated as
RAII values. ShyC inserts `drop` at scope exits, `return`, `break`, and
`continue`. Passing or assigning such a value by value moves it; using a moved
value is an error. Calling `.drop()` manually disables the automatic drop for
that local value.

`goto` out of a scope containing active RAII locals is rejected.

### Imports

A file-scope `use` loads a module header and makes its declarations available
without repeating the module path at each use:

```c
use core::option::Option;
use c::stdio::*;

int main(void) {
  Option<int> value = Option<int>::Some(1);
  printf("%d\n", value.payload.Some);
  return 0;
}
```

A named path such as `use core::option::Option;` includes
`core/option.shyh` and makes `Option` available without a `core::option::`
prefix. A glob such as `use core::option::*;` includes the same module and
makes its declarations available. `use c::stdio::*;` includes the C view,
`c/stdio.h`. The compiler resolves `crate::include::api::*` from the nearest ancestor
containing `.git` or `Cargo.toml`. Outside a repository it falls back to a
matching `-I` root or the main source directory, while
`self::include::api::*` is resolved relative to the importing file. ShyC module
imports load `.shyh` declarations, and their C-header output records the
matching `.h` include. Include expansion happens before parsing, lowering and
symbol indexing, so the LSP sees the imported declarations too.

A named import checks that its final name exists in the included file-scope
symbol table; a glob imports the module's file-scope declarations. `use`
contributes no runtime symbol or storage; the generated C header carries the
resolved include.

## Floating Point

Floating point is implemented through software helper calls where needed. The
current runtime source is:

```text
third_party/chibicc/shy_runtime_softfloat.c
```

The runtime is intentionally approximate for now. It is good enough for basic
bare-metal code that needs simple float operations, but it is not a strict
IEEE 754 implementation.

Compile and link this runtime explicitly when code uses floating point helpers.

## Atomics

ShyISA currently provides a minimal atomic primitive. ShyC maps only operations
that can be represented with the available instruction support.

`atoma` is enough for simple lock-style bare-metal code, but it is not a full
replacement for all C atomic operations. Compare-and-swap and the full C memory
model need either stronger ISA support or runtime conventions.

## Inline Assembly

Two inline assembly forms are supported.

Raw chibicc-style assembly:

```c
asm("oututfa 4x");
```

Shy register-binding assembly:

```c
asm!(a, b) {
    "adda {a} {b}"
};
```

The `asm!` binding form loads each listed scalar variable into fixed argument
registers before emitting the assembly. Placeholders such as `{a}` and `{b}` are
replaced with the allocated registers. After the assembly block, bound scalar
variables are written back.

ShyISA register names are also address operands. For example, if `{a}` expands
to `4x`, then `oututfa {a}` becomes `oututfa 4x`, which reads the value stored in
register `4x` and emits it as a UTF-8 code point. `oututfn {a}` would treat the
register address itself as an immediate number, so it would emit code point `4`,
not the value of the C variable.

Address bindings are written with `&`:

```c
asm!(lock, v) {
    "atoma {&lock} {v}"
};
```

`{&lock}` expands to the address of `lock` and is not written back as a value.
This form is intended for primitives such as spin locks.
