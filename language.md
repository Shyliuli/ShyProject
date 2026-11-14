# ShyC language direction

The language is at version **0.1.0**: the first release boundary, still open to
change before 1.0. This document records that boundary. The independent `shyc`
frontend is the only implementation.

## C compatibility

ShyC keeps C's direct memory model:

- linkage is the one deliberate difference: C declarations are externally
  visible unless `static`, while a ShyC `fn`, method, or global is file-local
  unless `pub` (see *Linkage and `pub`* below), and
  `#![disable_default_static]` restores the C default for one file;
- pointers are ordinary addresses and may alias;
- there is no ownership, borrowing, lifetime, garbage collection, or hidden
  allocation model;
- `const` keeps its C meaning and may appear in the usual C positions;
- structures, integer types, pointer types, and ABI-visible layouts remain
  transparent;
- `defer` is explicit scope cleanup: a local declared `defer` calls its type's
  `drop(self *value)` when the scope ends, in reverse declaration order.
  Ordinary locals are never dropped, and `defer` implies no ownership, borrow
  checking, or move checking;
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
header. A trait implementation whose receiver is concrete has one definition
site, so its methods may be declared in a header and defined in a source file,
like a plain method of an inherent impl. `const N` is a compile-time integer
parameter:

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

`Enum::Variant` always names a variant tag, including for ordinary C enums. The
current tag is queried with `value.type()` for every enum kind. ShyC source does
not expose `.tag`; the storage representation remains an ABI detail. A single
variant test can therefore be written directly as an `if`:

```c
if (token.type() == Token::Keyword) {
    /* use token.Keyword */
} else if (token.type() == Token::Eof) {
    /* stop */
}
```

One payload is constructed with parentheses and named fields with braces:
`Token::Keyword("fn")` or `Token::Keyword { text: "fn" }`. A payload-free variant
uses an empty argument list, as in `Token::Eof()`. Payloads are read directly as
`token.Keyword`; there is no `payload` wrapper.

`switch` remains the control-flow construct. It supports the existing `default`
and `_` forms, and case ranges written `case low ... high:`, which dispatch
through a comparison chain instead of a switch table. Overlapping labels and a
backwards range are diagnosed, not silently ordered. Closed payload enums can be checked for exhaustive coverage;
ordinary C enums are open and missing `default` is a diagnostic. The implemented
`#![disable_nonexhaustive_switch]` file attribute disables the ordinary-enum
diagnostic; a local attribute form is not available yet. Integer conversion
follows the C-compatible enum rules and may be warned about; it is never a
hidden runtime operation.

## Result, Option, and `?`

`Result<T, E>` and `Option<T>` are library types. Their declarations opt into
compact C-compatible representations using `#[result_niche]` and
`#[option_niche]`; these attributes are not part of the export instance table.
They may also be written as `// #[result_niche]` and `// #[option_niche]`.

For a marked `Result<void, E>`, a payload-free error enum with no zero
enumerator permits the representation of `E`'s underlying integer: zero is
`Ok`, and a nonzero value is `Err`. For a marked `Option<T *>`, the
representation is the pointer itself: NULL is `None` and a non-null pointer is
`Some`. This is an explicit C interop convention: `Some(NULL)` is equivalent
to `None`, rather than a distinct value. Instances that do not meet these
conditions retain the ordinary tag and payload union.

`switch` on either compact enum dispatches on its logical variant, using a
zero/nonzero comparison of the stored value. Payload access is direct:
`case Option<int *>::Some:` followed by `value.Some`, or
`case Result<void, Error>::Err:` followed by `value.Err`. The condition is
evaluated once. `value.type()` returns the logical variant without exposing the
raw pointer or error code.
`?` treats NULL as Option failure and a nonzero error code as Result failure.
Generated C interfaces use the same pointer or integer representation. A raw
pointer may be passed to or received from its compatible compact Option type.
Pointer/integer conversions require the same explicit casts as ordinary C
pointers; a pointer Option is not an arithmetic integer.

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

A method may carry a default body. An `impl` that does not define the method
inherits that body with `self` bound to the implementing type, so one required
method is enough for the rest of the interface:

```c
trait Sequence {
    int next(self *s);
    int count(self *s) {
        int n = 0;
        while (self.next() != 0) {
            n = n + 1;
        }
        return n;
    }
}

impl Sequence for Countdown {
    int next(self *s) { /* ... */ }   // count comes from the trait
}
```

Defining the method in the `impl` still overrides the default. An inherited
method is an ordinary method of the implementation: `c.count()` dispatches
like any other, and `Sequence::from<Countdown>()` fills its function-pointer
field in the trait value.

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

## Linkage and `pub`

`mut` opens up writability; `pub` opens up visibility. Both follow the same
shape: the conservative default is implicit, and opening it up is written out.

| Declaration | Default | `pub` | `static` |
|---|---|---|---|
| `fn` function | file-local | exported | file-local |
| impl method | file-local | exported | file-local |
| global object | file-local | exported | file-local |
| struct/union field | always accessible | — | — |

A file-local symbol is scoped to its translation unit, so two units may each
define their own `helper` without colliding, and a header may carry a private
implementation. Struct and union fields are C style: they carry no `pub` and
are always accessible.

A prototype is never file-local — it names something defined elsewhere — and a
definition in the same unit adopts the prototype's symbol.

`#![disable_default_static]` restores C's linkage default for one file, so only
`static` gives a file-local symbol and `pub` becomes an empty token. It is the
linkage twin of `#![disable_rustic_mut]`: nothing about the ABI, object layout,
or calling convention changes, and a C-compatible library is expected to carry
it. A file that must stay valid C writes it inside a comment —
`// #![disable_default_static]` — which C tools ignore and shyc still reads.

Layers are expected to pick one style and keep it: a C-compatible layer keeps C
declarations plus the commented attribute, while a ShyC layer writes `pub` on
what it exports and leaves everything else file-local. Generated C headers stay
plain C: they carry neither `pub` nor `mut`.

Visibility is written per declaration, not per block: an `impl` block marks each
method it exports, and a file lists the fields it exports next to the fields it
keeps to itself. A trait implementation follows the same rule as an inherent
one: its methods are file-local unless they say `pub`, so a header may carry a
whole `impl Trait for T` block without exporting anything. The methods of a
concrete `impl Trait for T` may also be declared without a body; the definition
in one source file adopts the symbol the declaration gave it. A generic impl is
instantiated where it is used, so its methods keep their bodies.

```c
// bare Shy: 默认私有
fn helper(void) -> int {         // 本单元
    return 1;
}

pub fn entry(void) -> int {      // 对外
    return helper();
}

struct Counter {
    int value;
};

impl Counter {
    pub fn bump(self *c) {       // 每个方法自己写 pub
        c.value = c.value + 1;
    }

    fn reset(self *c) {          // 不写就只在本单元
        c.value = 0;
    }
}

// C-compatible library
#![disable_default_static]

fn helper(void) -> int;          // 对外（C 默认）
static fn local(void) -> int {   // 只在本文件
    return 1;
}
```

`pub` on a prototype adds nothing: a prototype always names an external symbol.
A definition that follows an `extern` declaration adopts the declared symbol, so
the declaration is the one place that decides the name.

## Imports

A file-scope `use` loads a module header and imports declarations into the
translation unit's symbol table:

```c
use core::option::Option;
use c::stdio::*;

int main(void) {
  Option<int> value = Option<int>::Some(1);
  printf("%d\n", value.Some);
  return 0;
}
```

A named import such as `use core::option::Option;` loads
`core/option.shyh` and checks the named declaration. A glob imports the
module's file-scope declarations. `c::` paths resolve to C-compatible `.h`
headers; `core::`, `alloc::`, and `std::` paths resolve to `.shyh` declarations.
`crate::` paths resolve from the nearest ancestor containing `.git` or
`Cargo.toml`; outside a repository they use a matching include root (`-I`),
then the main source directory. `self::` paths resolve relative to the
importing source file. C header generation
records the corresponding `.h` include for ShyC modules.

Use expansion happens in preprocessing, before parsing and lowering, so LSP
analysis sees the included declarations and diagnostics. `use` remains in the
syntax tree for the symbol index, but contributes no runtime symbol or storage.

## Compile-time functions

`const fn` is an ordinary function that runs while compiling. A call whose
arguments are compile-time values is replaced by the value it returns, so it can
size a type, appear in a constant, or compute a literal:

```c
const fn factorial(int n) -> int {
    if (n <= 1) {
        return 1;
    }
    return n * factorial(n - 1);
}

const fn round_up(int n, int to) -> int {
    return ((n + to - 1) / to) * to;
}

int numbers[factorial(5)];        // 120 elements
char buffer[round_up(37, 16)];    // 48 bytes
```

In an expression, `name!(...)` explicitly calls a declared `const fn`.
This works for user-defined functions too: `factorial!(5)` produces `120`.
The `!` does not change the signature: value parameters receive compile-time
values, while a single `Tokens` parameter receives the argument syntax.

Ordinary `name(...)` calls also select a const fn unless a runtime function has
the same name. In that case, `name(...)` selects the runtime function and
`name!(...)` selects the const fn. The `!` is a way to choose the compile-time
function, not a marker reserved for compiler builtins.

A string argument is the text the program would see at run time: escapes are
already decoded, so a `const char *` parameter receives the three bytes of
`"a\tb"`, and `tokens_from_text("\"x\"")` lexes a quoted token.

A `const fn` is compiled like any other function and runs in an emulator while
the program is built, so its body is ordinary ShyC: pointers, globals, `goto`,
`sizeof`, loops, and the standard library all work. Nothing is interpreted, and
nothing from the header set is special-cased. Building text is therefore normal
ShyC code, which is what lets a generator write declarations the way a template
would (see `libshy/std/fmt.shyh`, which builds the `println!` expansion with
`String` and hands the finished source to `tokens_from_text`):

```c
const fn count_placeholders(const char *fmt) -> int {
    let mut count = 0;
    let mut i = 0;
    while (fmt[i] != 0) {
        if (fmt[i] == '{' && fmt[i + 1] == '}') {
            count = count + 1;
            i = i + 2;
        } else {
            i = i + 1;
        }
    }
    return count;
}
```

### Token arguments

A `const fn` may take the call's syntax instead of values. A single parameter of
type `Tokens` receives everything inside the parentheses exactly as written,
including commas; the return type `Tokens` produces syntax that is parsed back
into the expression the call stands for:

```c
#include <core/comptime.shyh>

const fn echo(Tokens input) -> Tokens {
    return input;
}

const fn count_tokens(Tokens input) -> int {
    return tokens_len(input);
}

int x = echo(2 + 3) * 2;      // (2 + 3) * 2
int n = count_tokens(a, b, c); // 5
```

A `Tokens` result may also stand where a **type** is written, which is how a
const fn names a computed type:

```c
const fn echo(Tokens input) -> Tokens {
    return input;
}

long widened = (echo(long))3;   // a cast to long
int size = sizeof(echo(int));   // 4
```

A token result also fills the other syntax slots. A **generator** call stands on
its own and ends with `;`: in statement position it is a statement list, and at
file scope it is an item list. The semicolon is what tells a generator apart
from a declaration whose type the call supplies. Arguments follow the signature
like any other call: a single `Tokens` parameter receives the syntax as
written, everything else is an ordinary expression list, so
`format_generation(8)` passes the integer 8. A `;` written last closes that
token argument the way it closes a statement, so it is not one of the tokens:
`count_tokens(a b c;)` receives three. Calling a generator in the wrong
place is reported by name, not as a parse error:

```text
error: const fn 'show' produced statements, which cannot stand at file scope; call it inside a function
error: const fn 'make_impl' produced declarations, which cannot stand inside a function; call it at file scope
```

```c
const fn make_fn(Tokens input) -> Tokens {
    return tokens_concat(
        tokens_concat(tokens_from_text("int twice(int n) { return n * 2; } int run(void) { return twice("), input),
        tokens_from_text("); }"));
}

make_fn(21);                   // generates both functions

int main(void) {
    return run() == 42 ? 0 : 1;
}
```

A token result fills a **field list** too, so a struct may declare generated
members next to written ones:

```c
const fn field(Tokens input) -> Tokens {
    return tokens_concat(
        tokens_concat(tokens_from_text("int "), input),
        tokens_from_text(";"));
}

struct Point {
    int x;
    field(y)
    field(z)
};
```

### Generators and impl dependencies

An item-level `const fn` is a **generator**. Generators are not run in source
order: they are scheduled by what they need and what they can provide.

- `require_impl(type, trait)` needs an impl. Both literal names and names
  computed by the generator use the same compiler-side world matcher. Generic
  impls check their parameter bounds: `impl<T: Show> Show for Box<T>` matches
  `Box<int>` when `int: Show` holds, and builtin spellings such as
  `int unsigned` and `unsigned int` compare equally. Trait arguments remain
  part of the key, so `Convert<int>` and `Convert<char>` are distinct.
  A missing impl suspends an item generator at the query; it does not return
  empty tokens. Once another generator's output satisfies the need, the
  suspended generator runs again against the updated world. A query inside a
  branch that is not executed never creates a dependency. Outside an item
  generator, a missing impl returns 0 without suspending.

  If no runnable work remains, compilation fails at the waiting call site,
  naming the type and trait that are still missing:

  ```text
  error: const fn 'consume_marker' waits for type 'Widget' to implement trait 'Marker', but no runnable generator provides it
  ```

- `try_provide_impl(type, trait)` offers one. It answers 0 when the impl already
  exists and 1 otherwise, so the provider emits the impl once and produces
  nothing after a later round sees it. Several generators may offer the same
  impl in one round: the earliest in source order keeps its answer, and the
  others stay unanswered until the next round, when they find the impl already
  there. Outside an item generator this query returns 0. A generator that emits
  an impl without offering it still collides with a hand-written one and is
  reported.

Ready generators of one round are asked concurrently, each in its own probe run
against the same round world. Generators in one round therefore run
independently: one does not see another's output or the impls it offers, and
`declared` and `implements` answer every one of them from that same world.
Answers are collected in source order, which is the order that decides which of
two offers of one impl keeps its answer. Each generated fragment joins the
declaration world on the next parse round, which can wake suspended generators.
Two generators that emit the same impl without successfully guarding with
`try_provide_impl` still collide at merge:

```text
error: trait 'Marker' is already implemented for 'Widget'
```

**Generated source may generate more.** A generator's own output is parsed by
the same routine as the main file, so a generated item may call another
generator. A fragment may even declare a `const fn` of its own: the declaration
is merged into the unit and later generators can call it.

**Function bodies are resolved after the generators settle.** The declarations
are parsed in one stage, expanding generators until none is left waiting, and
the function bodies are parsed in a second stage against that settled world. A
`const fn` called inside a body therefore sees every declaration the generators
produced, wherever the generator stands in the file.

A generator can call an ordinary helper that itself calls a scalar-returning
`const fn` with value arguments. In the probe, both calls are compiled as
ordinary machine code and use that round's world. In the final program, the
helper's const fn call is still replaced by its compile-time answer when the
trampoline can carry it: a plain non-template, non-variadic function of at most
eight named parameters, each `int`, `unsigned`, `_Bool`, `const char *`, or
`Tokens`, returning `int`, `unsigned`, `_Bool`, or `Tokens` by value. A value
call in a function body outside that set is not asked: it stays an ordinary
call, and the final program emits the const fn as an ordinary function, so
`char`, `long`, enum, typedef and pointer results and struct pointer parameters
work at run time. A template or variadic `const fn` cannot become an ordinary
call: its call stays a request and is reported when the trampoline cannot serve
it. A file-scope initializer holds data, not code, so a `const fn` call there
is reported the same way. A request that would run a helper whose Tokens syntax
expansion is unresolved is left unanswered and asked again once that expansion
exists, so the other requests of the round are still answered and an unused
helper costs nothing. When no request in a round can run, compilation fails at
the blocked call and names the helper:

```text
error: const fn 'gen' cannot be probed in this round: the body of 'helper' still holds an unresolved const fn syntax expansion
```

An indirect call to such a helper is caught while the probe runs, so the round
is rejected instead of answering from a missing expansion.

The compile-time builtins over token sequences are `tokens_len`, `token_text`,
`tokens_text`, `tokens_slice`, `tokens_concat`, `tokens_from_int` and
`tokens_from_text` (the last one lexes a string). There is no `gensym`: a macro
that must not collide with a caller's name asks `declared` and picks a distinct
name itself.

### Trying a call at compile time

`try_run(f, arg..., &result)` asks the compiler to run `f` now and store its
result. It returns 1 when the call could be run and 0 when it failed, so one
macro can choose between a folded and a runtime path:

```c
int scale(int value, int factor) { return value * factor; }

const fn fold(int value) -> int {
    let mut result = 0;
    if (try_run(scale, value, 3, &result)) {
        return result;
    }
    return -1;
}

int folded = fold(14);   // 42, computed while compiling
```

Any function defined in the translation unit can be tried, not just a `const
fn`. The attempt runs in a child emulator with its own stack: when it reaches
the end, its memory writes are committed and `result` holds the value; when it
traps, runs out of budget, or makes an unsupported host request, its writes are
rolled back and the call is reported as "not now". Read-only `declared` and
`implements` queries are allowed in the child and read the same round world.
Compiler failures such as exhausted token storage or an unresolved syntax
expansion terminate compilation even inside `try_run`; they are not failed
attempts at running the user function. Arguments and the result are `int` in
this slice. A wrong argument count, an unknown function, a `const fn`
as the target, or an out-parameter that is not a local is reported as an error
instead, because that call would be wrong at run time too.

The child runs in the probe image, not in the finished program, so a tried
function has to be a pure function of its arguments: its stack and the address
of a local differ from the ones the running program would see, and a value that
depends on either differs too. Every tried call starts from the same round
state, so two calls of one function do not see each other's writes.

`declared(name)` answers whether a type name is visible in the declarations
generated so far — a snapshot, not a promise about the rest of the file. It
covers typedefs and tags from headers, traits, and anything an earlier generator
emitted. Ordering between generators is what `require_impl` and
`try_provide_impl` are for; a generator that needs another one's declaration
should request the impl, not poll `declared`. The usual shape is to fill a gap
instead of duplicating a declaration:

```c
const fn ensure_widget(Tokens input) -> Tokens {
    if (declared("Widget")) {
        return tokens_from_text("");
    }
    return tokens_from_text("struct Widget { int value; };");
}

ensure_widget();
ensure_widget();   // the second call generates nothing
```

A returned token sequence must form one complete expression; trailing tokens are
rejected instead of being silently dropped, and the diagnostic names the const
fn that produced them:

```text
error: expected an expression, found end of file (from const fn 'bad_expr')
```

A call the probe answers leaves no call behind: its answer stands in its place,
so an argument of an answered call has to be a compile-time constant, and
anything else is a diagnostic. A `const fn` that ordinary code still names at
run time is emitted as an ordinary function: a call the trampoline cannot
carry, or an address taken as `add` or `&add`, as in
`int (*fp)(int, int) = add;`. A file-scope initializer runs no code at all, so
a `const fn` the trampoline cannot carry is reported there instead of being
called. A probe run has an instruction budget and a
wall-clock limit, so a runaway recursion fails instead of hanging the compiler.
Token builders use bounded memory pools. If a pool is exhausted, compilation
fails with the pool name, requested amount, and remaining capacity; the compiler
does not accept an empty expansion as a substitute.

`shycc --constfn-steps <n>` sets the instruction budget for each probe run
(default 32000000, maximum 100000000). `--constfn-timeout-ms <n>` sets the
wall-clock limit in milliseconds (default 2000, maximum 60000).
`--constfn-rounds <n>` caps compilation rounds (default 256). All three values
must be positive. These options also apply to `--emit=c` and `--emit=c-header`;
they do not change the separate child budget used by `try_run`.

## Macros are compile-time functions

There is one compile-time mechanism, not two: `const fn`. A function that takes
`Tokens` and returns `Tokens` is a macro, because its call is replaced by the
syntax it produced:

```c
#include <stdio.h>
#include <core/comptime.shyh>

void print_value(int value) { printf("%d\n", value); }

const fn show(Tokens input) -> Tokens {
    return tokens_concat(
        tokens_concat(tokens_from_text("print_value("), input),
        tokens_from_text(");"));
}

const fn twice(Tokens input) -> Tokens {
    return tokens_concat(
        tokens_concat(tokens_from_text("("), input),
        tokens_from_text(") * 2"));
}

int main(void) {
    show(twice!(20) + 2);  /* generates print_value((20) * 2 + 2); */
    return 0;
}
```

The call inside `main` expands to a statement and prints `42`. At file scope the
same call would expand to items instead, so a macro that produces statements is
called from a function body.

A macro meant for other files lives in a header, next to the traits it targets.
For example, `alloc/debug.shyh` provides `derive_debug`:

```c
#include <std/fmt.shyh>
struct Widget { int value; };
derive_debug(Widget);

int main(void) {
    Widget widget = {42};
    println!("{:?}", widget);
    return 0;
}
```

A compile-time function is visible wherever its header is included, so it needs
no linkage marker; `pub const fn` is accepted and means nothing. The language
server indexes these names too: hover on a declaration or a call shows the
signature, and go-to-definition on a call (including a detached generator call)
jumps to the declaration. The call goes
after the declaration it decorates, and `println!("{:?}", value)` then uses the
generated impl — the argument still has to implement `Debug`.

`write!`, `writeln!`, `print!`, `println!`, `eprintln!`, and `format!` are
library const fns receiving `Tokens`. They use the same `name!(...)` call syntax
as any user-defined const fn. Ordinary token-generator calls also accept
`name(...)`; POSIX `write(...)` and formatting `write!(...)` can coexist.

The formatting const fns check the literal format string, its placeholder
count, `{}` versus `{:?}`, brace balance, and `{name}` captures. They generate
direct writes and concrete `Display`/`Debug` calls, inside a statement
expression. Neither format parsing nor intermediate strings remain at run
time. `Display` and `Debug` accept `WriterRef *`, a borrowed destination with
the `Writer` function table; nested formatting uses the same destination.

`write!(writer, ...)` writes without a newline; `writeln!` appends one.
`print!` and `println!` use the global writer; `eprintln!` uses stderr.
The destination and argument expressions are evaluated once, in order.
The result is the byte count including any newline, or -1 when output fails.
Short writes are retried, and failures stop the remaining output.
A temporary endpoint (`stdout_writer()`) or a writer's address
(`writeln!(&text, ...)` for a `String`) both work. `format!` creates a String
as the destination and returns it; the caller calls `drop()` on it.

`scanln!` is different: it hands its string to `sscanf`, so the compiler only
counts C conversions and calls a wrapper, up to eight arguments:
`scanln!("%d %d", &a, &b)`. An argument must implement the trait its
placeholder names, so a C string is wrapped first:
`println!("{}", Str::from_cstr(word))`.

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

The compiler already renders both directions: `shyc --emit=c-header` prints an
include-guarded header of the translation unit's typedefs, record and enum
definitions, non-`static` prototypes, and `extern` object declarations, and
`shyc --emit=c` lowers the unit and prints it back as C source. The source form
keeps the compiler's names, so a call and the definition it reaches print the
same symbol; the sugar is resolved on the way: an inferred local becomes
`__auto_type` (`const __auto_type` when it is not `mut`), compile-time calls
are already expanded, `#![...]` attribute lines are dropped, an `impl` block
becomes one function per method that takes its receiver first, a trait value is
the `struct` of function pointers that `Trait::from<T>()` fills, `value?`
becomes a statement expression whose failure arm returns the enclosing
function's failure variant, and `asm!` becomes a GCC extended asm statement.
Generics are specialized: a generic function, method, or record prints one
concrete definition per specialization the compiler materialized, under the
type name the instance table or the type printer gave it, so C needs no
template. A generated header carries the main file's `#include` lines and
`#define` macros, takes its include guard from the output path, and omits
declarations that came from an included file, so it describes exactly one
header. `static` declarations and `impl` blocks are not exported by that
ordinary header: a method needs a concrete instantiated symbol before it can
appear in a C header.

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

`shyc` owns the language frontend and the typed IR. Its Shy, LLVM, and C
backends consume the same frontend and ABI plans.
