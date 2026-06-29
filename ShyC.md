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
behavior is unchanged. They are intended as input for future debug-info
generation and emulator debugger support. The option is off by default.

## Execution Model

This is a freestanding, bare-metal target.

- No hosted C standard library is assumed.
- No system calls are assumed.
- Startup is minimal.
- TLS is unsupported.

If a translation unit defines `main`, the backend emits `_start`:

- initializes `sp` to `0x00fff000`;
- calls `main`;
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

## Calling Convention

Scalar return values use:

- `1x` for 32-bit and smaller values;
- `1x` low 32 bits and `2x` high 32 bits for 64-bit values.

Function arguments are passed in 32-bit slots:

```text
4x, 5x, 6x, 7x, 8x, 9x, ax, bx
```

Values that are 64-bit wide consume two slots, low word first, then high word.
Calls that need more slots than this are not part of the current ABI.

`fx` is the frame pointer. The Shy stack grows upward. Function prologues save
and restore the caller frame pointer.

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
- function calls within the current argument-slot ABI;
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
- arbitrary ABI spill arguments;
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
Method symbols are lowered as:

```text
____TYPE__method
```

For example, `Counter::new` lowers to `____Counter__new`.

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

If a type implements `drop(self *s)`, local values of that type are treated as
RAII values. ShyC inserts `drop` at scope exits, `return`, `break`, and
`continue`. Passing or assigning such a value by value moves it; using a moved
value is an error. Calling `.drop()` manually disables the automatic drop for
that local value.

`goto` out of a scope containing active RAII locals is rejected.

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
