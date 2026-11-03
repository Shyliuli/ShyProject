# ShyC language direction

This document records the language boundary before the first public release. The
compiler currently has no published language version. The independent `shyc`
frontend is the main implementation; the chibicc fork remains an early
bootstrap and ShyOS compatibility validator.

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
zero/nonzero comparison of the stored value. Existing C-style case labels and
payload access remain available: `case Option<int *>::Some:` followed by
`value.payload.Some`, or `case Result<void, Error>::Err:` followed by
`value.payload.Err`. The condition is evaluated once. `.tag` still exposes the
raw pointer or error code: compare the pointer directly, or switch on
`result.tag` to distinguish individual error codes.
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
whole `impl Trait for T` block without exporting anything.

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
  printf("%d\n", value.payload.Some);
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

A string argument is the text the program would see at run time: escapes are
already decoded, so `str_len("a\tb")` is 3 and `tokens_from_text("\"x\"")`
lexes a quoted token.

A `const fn` may take integers and strings, use `let`, `if`, `while`, `for`,
`do`/`while`, `break`, `continue`, `return`, compound assignment, arithmetic,
comparisons, indexing, `switch` (labels, case ranges like `'0' ... '9'`,
fallthrough, `default` anywhere, `break`), and call other `const fn`s. `goto` and `sizeof` are rejected with a
message that names the alternative.
Three builtins inspect strings: `str_len(text)`, `str_byte(text, index)` and
`str_eq(left, right)`, so counting or scanning a literal is normal ShyC code:

```c
const fn count_placeholders(const char *fmt) -> int {
    let mut count = 0;
    let mut i = 0;
    while (i + 1 < str_len(fmt)) {
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

long widened = (echo(long))value;   // a cast to long
int size = sizeof(echo(int));       // 4
```

A token result also fills the other syntax slots. A **generator** call stands on
its own and ends with `;`: in statement position it is a statement list, and at
file scope it is an item list. The semicolon is what tells a generator apart
from a declaration whose type the call supplies. Calling one in the wrong place
is reported by name, not as a parse error:

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

- `require_impl(type, trait)` needs an impl. It returns 1 when the impl exists;
  otherwise the generator waits in queue 1 until someone provides it. A generic
  implementation covers its instantiations: `impl<T> Show for Box<T>` answers
  `require_impl("Box<int>", "Show")`. The match also checks the argument count
  and the bounds written on the impl's parameters, so `impl<T: Show> Show for
  Box<T>` covers `Box<int>` only when `int: Show` holds. The same rule applies
  to the type checker: an impl whose bounds do not hold for the concrete
  arguments simply does not exist for that type, so `Box<Plain>.show()` is an
  error rather than a silent call. Builtin spellings are canonicalized, so
  `impl Show for unsigned int` answers `implements("int unsigned", "Show")`,
  and `long int` is the same type as `long`. Trait arguments are part of the
  key: `impl Convert<int> for Alpha` answers `implements("Alpha",
  "Convert<int>")` but not `Convert<char>` or a bare `Convert`. Constant
  arguments are canonicalized on both sides, so `At<0x10>` is `At<16>`.
- `try_provide_impl(type, trait)` offers one. It returns 0 when the impl already
  exists; otherwise the generator waits in queue 2, ready to provide it.

Every generator that can run, runs. When nothing is left to run, one waiting
provider is granted and runs again, which wakes everyone in queue 1 that needed
that impl. When the waiters and the offers form a loop — each provider needs what another
offers — the cycle is reported as the chain it is:

```text
error: dependency cycle between generators: 'A' needs 'T1' -> 'B' needs 'T2' -> 'A' needs 'T1' (cycle)
```

If queue 1 still has sleepers at the end, nothing provides what they
need and the compiler says so:

```text
error: type 'Widget' needs trait 'Marker', but nothing provides it (in const fn 'consume_marker')
```

**Generated source may generate more.** A generator's own output is parsed by
the same routine as the main file, so a generated item may call another
generator; that call joins the same queue and is scheduled by its needs, not by
where it appeared. A fragment may even declare a `const fn` of its own: the
declaration is merged into the unit and later generators can call it.

**Function bodies run last.** Once the generators settle, the world is final and
a `const fn` called inside a function body reads it (only the function
bodies that actually call one are parsed again, and a generated fragment that
calls one is parsed again by itself once the world settles): `implements(type, trait)`
answers for the generated declarations too. A body cannot block or inject a
global impl — `require_impl` there is a plain query and `try_provide_impl`
returns 0.

Two generators that emit the same `impl Trait for Type` are a conflict, even if
both asked `try_provide_impl` — the duplicate is reported at the merge instead of
one silently winning:

```text
error: duplicate implementation of 'Marker' for 'Widget'
```

A provider that was granted an impl but never produced it is reported at its own
call site:

```text
error: generator granted to provide 'Marker' for 'Widget' produced no such implementation
```

The check runs against the settled world, so a provider may emit the impl through
a generator it calls and still count.

When the granted provider is itself stuck, the message follows the chain of
providers until it reaches the end:

```text
error: type 'Widget' needs trait 'Marker', but its provider 'provider_a' is still waiting for 'Widget' to implement 'Extra', its provider 'provider_b' is still waiting for 'Widget' to implement 'Third', nothing provides it (in const fn 'consumer')
```

A provider may itself wait for another impl, so providers form a chain: one
provides what another needs, and the waiter is woken when it appears.

When several generators offer the same impl, the one that runs first provides it
and the rest find it already there and emit nothing. Generators run in source
order within a round, so the winner does not depend on how the scheduler
interleaved them. A grant stays reserved while that provider waits on something
else, so no second provider is handed the same impl in the meantime.

A provider never produces a second impl for the same type and trait, so a
hand-written impl and a generated one cannot collide:

```c
const fn provide_marker(Tokens input) -> Tokens {
    if (try_provide_impl("Widget", "Marker")) {
        return tokens_from_text(
            "impl Marker for Widget { fn marker(const self *v) -> int { return 7; } }");
    }
    return tokens_from_text("");
}

const fn consume_marker(Tokens input) -> Tokens {
    if (require_impl("Widget", "Marker")) {
        return tokens_from_text("int use_marker(void) { return 1; }");
    }
    return tokens_from_text("");
}
```

The compile-time builtins over token sequences are `tokens_len`, `token_text`,
`tokens_text`, `tokens_slice`, `tokens_concat`, `tokens_from_int`, and
`tokens_from_text` (the last one lexes a string). `gensym(prefix)` returns a
fresh name, so a macro that declares something can avoid colliding with names
the caller wrote:

```c
const fn declare_counter(Tokens input) -> Tokens {
    let name = gensym("counter");
    return tokens_concat(
        tokens_concat(tokens_from_text("int "), tokens_from_text(name)),
        tokens_from_text(" = 0;"));
}
```

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

`const fn` does not exist at run time: it emits no symbol. An argument that is
not a compile-time constant is a diagnostic, and evaluation is limited to 512
nested calls, so a runaway recursion fails instead of hanging the compiler.

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
    show(twice(20) + 2);   /* generates print_value((20) * 2 + 2); */
    return 0;
}
```

The call inside `main` expands to a statement and prints `42`. At file scope the
same call would expand to items instead, so a macro that produces statements is
called from a function body.

A macro meant for other files lives in a header, next to the traits it targets:

```c
/* derive.shyh */
const fn derive_debug(Tokens input) -> Tokens {
    return tokens_concat(
        tokens_concat(tokens_from_text("impl Debug for "), input),
        tokens_from_text(" { fn debug(const self *value, String *out) -> int { ... } }"));
}

/* user.shyc */
#include "derive.shyc"

struct Widget { int value; };

derive_debug(Widget);
```

A compile-time function is visible wherever its header is included, so it needs
no linkage marker; `pub const fn` is accepted and means nothing. The language
server indexes these names too: hover on a declaration or a call shows the
signature, and go-to-definition on a call (including a detached generator call)
jumps to the declaration. The call goes
after the declaration it decorates, and `println!("{:?}", value)` then uses the
generated impl — the argument still has to implement both `Debug` and `Display`,
because one `FormatArg` carries both.

`println!`, `format!`, and `scanln!` are the same idea with a format string: the
compiler checks the literal and expands the call to the wrapper for its
argument count, up to eight. For `println!` and `format!` a literal format
string is checked for placeholder count, `{}` versus `{:?}`, and brace
balance; `scanln!` instead counts C conversions, because it hands its string to
`sscanf`: `scanln!("%d %d", &a, &b)`. A non-literal format string is passed
through unchecked, and a placeholder without an argument is printed as it was
written rather than reading past the arguments. Arguments must implement both traits, so a C string is
wrapped first: `println!("{}", Str::from_cstr(word))`.

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
object declarations, and `shyc --emit=c` translates the unit back to C source.
The source form is the preprocessed unit with its sugar resolved: an inferred
local becomes `__auto_type` (`const __auto_type` when it is not `mut`),
compile-time calls are already expanded, and `#![...]` attribute lines are
dropped. An `impl` block becomes one function per method, named
`Type_method` and taking its receiver first, so `p.add(1)` prints as
`Point_add(&p, 1)`; a method call through a trait value stays a call to the
function pointer it stores. A trait value is the `struct` of function pointers
that `Trait::from<T>()` fills. `value?` becomes a statement expression whose
failure arm returns the enclosing function's failure variant, `asm!` becomes a
GCC extended asm statement, and a generic use prints as the instance name
(`Vec<int>` is `Vec_int`). What C cannot spell is reported rather than
guessed: a generic function or record prints nothing because an instance is a
symbol of its own, and a trait method that only has a default body stops the
translation instead of pointing at a symbol that does not exist. A generated
header carries the main file's `#include` lines and `#define` macros, takes its
include guard from the output path, and omits declarations that came from an
included file, so it describes exactly one header. `static` declarations and
`impl` blocks are not exported by that ordinary header: a method needs a
concrete instantiated symbol before it can appear in a C header.

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