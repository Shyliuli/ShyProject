# libshy

libshy is ShyC's standard library. Like Rust's, it has a C library at the
bottom and three layers above it:

| View | Contents |
|---|---|
| `c/` | the C include view: only `.h`, and only C-library headers — `stdio`, `stdlib`, `string`, `ctype`, `unistd`, `fcntl`, `stddef`, `stdint`, `stdtype`, `stdarg` |
| `core/` | ShyC declarations and their C spelling (`str.shyh`, `buffer.shyh`, `option.{shyh,h}`, `result.{shyh,h}`), plus `core.shyc` |
| `alloc/` | `vec.shyh`, `string.shyh`, `debug.shyh`, `display.shyh`, `alloc.shyc` |
| `std/` | `fs.shyh`, `io.shyh`, `fmt.shyh`, `env.{shyh,shyc}`, `std.shyc` |
| `libc/` | the C library in ShyC spelling: `*.shyh` declarations and the `libc.c` implementation |
| `sys/` | the platform calls: `shy.shyc` for bare Shy, `user.shyc` and `user_start.shyc` for `shy-user` |

The C library layer is written as plain C (`libc.c` uses `->`, never ShyC's
automatic dereference): a C implementation should read as C. ShyC sugar is
desugared on the way *out*, by the compiler, not carried in the source.

`c/*.h` are generated from the `.shyh` sources in `libc/` and `core/` with
`shyc --emit=c-header` and committed; `make libshy-headers` regenerates them.
`c/stddef.h` and `c/stdint.h` are hand-written routers because their contents
follow the target pointer width, which a generated header would freeze.

The C layer is libc-compatible, so ordinary C code compiles against it.
`<stdtype.h>` adds Rust-style aliases such as `i32`, `u64`, and `usize`.

## Platforms

`libc/libc.c` contains no assembly. Every platform service goes through six
POSIX calls: `read`, `write`, `close`, `_exit`, `sbrk` (`<unistd.h>`), and
`open` (`<fcntl.h>`).

| Platform | Platform calls | C layer | core/alloc/std |
|---|---|---|---|
| bare Shy | `sys/shy.shyc` | `libc/libc.c` | linked |
| `shy-user` | `sys/user.shyc`, `sys/user_start.shyc` | `libc/libc.c` | linked |
| ShyOS | system calls in `projects/os/user` | `libc/libc.c` | linked |
| hosted LLVM target | system libc | system libc | linked |

On bare Shy, `read` returns 0 at the end of input, `open` fails because there
is no file system (so `File::open` and `File::create` return
`Err(OpenFailed)`), and the heap grows from `_end` up to the stack at
`0x00fff000`. `free` keeps nothing: `malloc` is a bump allocator.

`shy-user` puts the host behind the same ShyISA code: `sys/user.shyc` forwards
`open`, `read`, `write`, `close`, `_exit`, and `sbrk` to the host as
syscalls, so `File::open`, `read_to_string`, and command-line arguments work
there. Build it with `--target=shy-user` and run it with
`shyemu-user prog.sfs [args...]`.

Streams are unbuffered. `FILE` is opaque, so on a hosted target `stdin`,
`stdout`, and `stderr` are the system library's.

## Bytes, endpoints, and formatting

`Buffer` (`core/buffer.shyh`) is a writable byte region: `data()`, `len()`,
`reserve(n)` for more room after the current length, and `set_len(n)` to commit.
`String` grows in `reserve`; `BufferView` wraps a fixed array. A generic
`B: Buffer` therefore serves memory, and the same code works for any storage.

`Reader` and `Writer` (`core/buffer.shyh`) are endpoints. A device implements
`read`/`write`; the default methods `read_buffer`, `write_buffer`, `getc`,
`getline`, `write_str`, `newline`, and `write_line` build the rest, so a UART
driver only supplies the two raw operations. `String` is a `Writer` too: its
`write` appends, so the same helpers can fill memory instead of a stream.
`getline` appends the line without
its newline and returns the number of bytes it added, so reusing a buffer means
resetting it first. A line longer than a fixed `BufferView` is delivered in
pieces, the way `fgets` truncates:

```c
StrReader reader = StrReader::new(Str::from_cstr("first\nsecond\n"));
String line = String::new();
while (reader.getline(&line) > 0) {
    print_line(&line);
    line.set_len(0);
}
```

`std/io.shyh` gives `File`, `FileReader`, `FileWriter`, `StrReader`, and the
replaceable global endpoints behind the stream-less helpers `print`,
`print_line`, `getc`, and `get_line`.

`read_stream(FILE *f)` returns everything left in a stdio stream as a
`String`.

`File` is a descriptor: its only field is the `int` the C library returned, so
its layout is the C one. `File::open` reads an existing file, `File::create`
creates or truncates one, `read` reads up to `max_len` bytes,
`read_to_string` returns everything left as `Result<String, IoError>`,
`write_all` writes every byte, and `close` closes the descriptor. `IoError`
names the failed step (`OpenFailed`, `ReadFailed`, `WriteFailed`); a bare
machine has no errno, so every failed open is `OpenFailed`. A `File` built
from a descriptor the platform provides, such as `File {0}` for stdin or
`File {1}` for stdout, still works on a bare machine.

`std/fs.shyh` adds the one-call helpers `read_to_string(path)` and
`write_file(path, text)`. Both take the path as a `Str`, the latter returning 1
when every byte was written:

```c
#include <stdio.h>
#include <std/fs.shyh>

int main(void) {
    Str path = Str::from_cstr("notes.txt");
    String text = String::from_cstr("hello\n");
    write_file(path, text.as_str());
    text.free();

    let read = read_to_string(path);
    if (read.is_err()) {
        printf("cannot read: %d\n", read.unwrap_err());
        return 1;
    }
    String back = read.unwrap();
    printf("%s", back.as_cstr());
    back.free();
    return 0;
}
```

`core/option.shyh` and `core/result.shyh` carry the two payload enums and their
methods, so a caller does not have to spell the tag out:

```c
#include <stdio.h>
#include <std/fs.shyh>

int main(void) {
    Str path = Str::from_cstr("missing.txt");
    let read = read_to_string(path);
    if (read.is_err()) {
        printf("failed: %d\n", read.unwrap_err());
        return 1;
    }
    String text = read.unwrap();
    printf("%s", text.as_cstr());
    text.free();
    return 0;
}
```

`Option<T>` has `is_some`, `is_none`, `unwrap`, and `unwrap_or`; `Result<T, E>`
has `is_ok`, `is_err`, `unwrap`, `unwrap_err`, and `unwrap_or`. `unwrap` aborts
when the value is absent, `unwrap_err` aborts when it is present, and
`unwrap_or` returns its fallback instead. `Option<T *>` is the
bare pointer, where `NULL` is `None` and `Some(NULL)` is therefore `None`; a
`Result<void, E>` whose `E` has no zero enumerator is `E`'s integer, where zero
is `Ok`. Both layouts come from `#[option_niche]` and `#[result_niche]` on the
declaration, and both stay invisible to callers. A `Result<void, E>` has no
`unwrap_or`, because a fallback of type `void` cannot be declared.

`Debug` and `Display` (`alloc/debug.shyh`, `alloc/display.shyh`) render a value
into a `String`; both are traits, so a struct opts in with `impl Debug for T`.
Traits can be implemented for scalars and enums too, so `impl Display for int`
is a normal declaration.

`std/fmt.shyh` formats with `{}` (Display), `{:?}` (Debug), `{name}`, and
literal `{{`/`}}`. `println!` and `format!` are compiled away: the compiler
checks the literal format string and expands the call into a `String` filled
with the literal pieces plus one statically dispatched Display/Debug call per
argument, so nothing is parsed at run time. `writeln!(writer, ...)` builds the
same line and writes it through any `Writer`, then frees the temporary
`String`; the writer expression is evaluated once, so `writeln!(stdout_writer(),
...)` and `writeln!(&text, ...)` both work. `scanln!` keeps a wrapper per
argument count, because `sscanf` reads the C conversions itself.

```c
#include <std/fmt.shyh>

int main(void) {
    int n = 0;
    char word[16];
    scanln!("%d %15s", &n, word);
    println!("n={} word={:?}", n, Str::from_cstr(word));
    return 0;
}
```

## Using it

`shycc` links the whole library by default and drops the functions a program
never reaches; `-nostdlib` turns it off.

```c
#include <stdio.h>
#include <alloc/vec.shyh>
#include <std/io.shyh>

int main(void) {
    String text = read_stream(stdin);
    Str view = text.as_str();
    Vec<int> lines = {0};
    for (int i = 0; i < view.len; i++) {
        if (view.data[i] == '\n') {
            lines.push(i);
        }
    }
    printf("%d lines\n", lines.len);
    return 0;
}
```

```sh
target/bin/shycc app.shyc -o app.sfs
printf 'a\nb\n' | target/bin/shyemu app.sfs
target/bin/shycc --target=x86_64-pc-linux-gnu app.shyc -o app
printf 'a\nb\n' | ./app
```

Both print `2 lines`. `read_to_string(path)` from `<std/fs.shyh>` returns
`Result<String, IoError>`, which is `Err(OpenFailed)` on bare Shy.

`Str` is a borrowed pointer and length; print one with
`printf("%.*s", s.len, s.data)`. `trim` drops leading and trailing ASCII
whitespace, `split(separator)` returns a `StrSplit` whose `next()` yields
`Option<Str>` pieces, and `parse_int` reads a decimal `int` or returns `None`;
`eq`, `eq_cstr`, `starts_with`, `ends_with`, `find`, and `slice` are the rest.

`String` owns a NUL-terminated byte buffer that a read-only `Deref<Str>` exposes,
so `s.len()`, `s.is_empty()`, `s.eq_cstr("x")`, and `s.starts_with("x")` work
directly on the owner; `as_str()` returns the `Str` view for field access. Its
layout is the `Str` view followed by an `int cap`, so C sees
`struct { const char *data; int len; int cap; }`. Create one with
`String::new()`, `String::from_str(s)`, `String::from_cstr("x")`, or
`String::with_capacity(n)`; append with `push`, `push_str`, `push_cstr`, or
`push_str(format!(...).as_str())`; `as_str` gives the `Str` view, `as_cstr` a
`const char *` that is always NUL-terminated, `clear` empties it, `clone`
copies the bytes, and `free` releases the buffer. A plain local is not dropped
automatically, so the owner calls `free` when it is done; `defer String s = ...`
drops it at scope exit through `String::drop`, which calls `free`. Neither type
validates UTF-8:
`push` appends one byte. A `Str` copied out of a `String` is a snapshot that a
later `push`, `push_str`, or `push_cstr` may invalidate by reallocating the
buffer. `Vec<T>` is generic, so `alloc/vec.shyh` carries its whole definition;
`defer Vec<int> v = {0}` drops it at scope exit through `Vec::drop`, which
frees the buffer. The other types declare their methods in the header and
define them once in the layer's `.shyc`.

## Process arguments

`<std/env.shyh>` declares `Args`, the `#[args_builtin]` struct whose layout is
`int main(int argc, char **argv)`. A `main` that takes it by value receives the
two scalars, so the same source starts on a hosted target and under
`shyemu-user` without a wrapper. `len()` is the argument count, `get(i)` one
argument, `iter()` an `ArgsIter` whose `next()` yields `Option<Str>`,
`has_flag("--verbose")` tests a flag, and `value_of("-o")` returns the argument
after it. Bare Shy has no process arguments, so `argc` is 0 there.

```c
#include <stdio.h>
#include <std/env.shyh>

int main(Args args) {
    printf("argc %d\n", args.len());

    ArgsIter iter = args.iter();
    while (1) {
        let arg = iter.next();
        if (arg.is_none()) {
            break;
        }
        Str one = arg.unwrap();
        printf("arg [%.*s]\n", one.len, one.data);
    }

    printf("verbose %d\n", args.has_flag("--verbose"));

    let output = args.value_of("-o");
    if (output.is_none()) {
        printf("output none\n");
    } else {
        Str out = output.unwrap();
        printf("output [%.*s]\n", out.len, out.data);
    }
    return 0;
}
```

```sh
shycc --target=host app.shyc -o app
./app one --verbose -o out
shycc --target=shy-user app.shyc -o app.sfs
shyemu-user app.sfs one --verbose -o out
```

Both runs print the argument list, `verbose 1`, and `output [out]`; only
`argv[0]` differs (the program path or the image path).

## Calling the library from C

A generic template has no single C spelling: `Vec<T>` becomes one concrete
struct per element type. The instance table says which ones a C client may use.
Each layer keeps its own, next to the template it names:

```text
libshy/alloc/vec.shyinst      Vec<int> as vec_int, Vec<unsigned char> as vec_u8
libshy/core/option.shyinst    Option<int> as opt_int
libshy/core/result.shyinst    Result<int, int> as res_int
```

The table is compile-time input, never C. Three builds produce what a C client
links against:

```sh
R=$PWD
shyc --emit=c-header  -I$R/libshy/c -I$R/libshy/include \
     $R/libshy/alloc/vec.shyh --instances $R/libshy/alloc/vec.shyinst -o vec_api.h
shyc --emit=llvm --target=x86_64-pc-linux-gnu \
     -I$R/libshy/c -I$R/libshy/include \
     $R/libshy/alloc/vec.shyh --instances $R/libshy/alloc/vec.shyinst -o vec.ll
shyc --emit=c-wrapper --target=x86_64-pc-linux-gnu \
     -I$R/libshy/c -I$R/libshy/include \
     $R/libshy/alloc/vec.shyh --instances $R/libshy/alloc/vec.shyinst -o vec_wrap.c

clang -x ir -c vec.ll -o vec.o
clang -c vec_wrap.c -o vec_wrap.o
clang client.c vec_wrap.o vec.o -o client
```

```c
#include <stdio.h>
#include "vec_api.h"

int main(void) {
    vec_int v = {0};
    for (int i = 0; i < 5; i++) {
        vec_int_push(&v, i * 3);
    }
    printf("%d %d\n", v.len, *vec_int_last(&v));   /* 5 12 */
    return 0;
}
```

`vec_api.h` carries the concrete layouts (`typedef struct shyc_R28 vec_int`),
so the C client owns the storage; the `.ll` object supplies the method bodies
and `vec_wrap.c` is the thin C-named shim over their stable symbols. A payload
enum exports the same way: `Option<int>` becomes a tagged union with a `tag`
field and a `payload` union.

## Headers

`.shyh` is the native ShyC header; `.h` is its C spelling. `shycc` searches
three roots, in this order:

```text
libshy/c       # the C layer: <stdio.h>
libshy         # explicit paths: <c/stdio.h>, <core/str.shyh>, <alloc/vec.shyh>
libshy/include # flat compatibility names: <stddef.h>, <stdint.h>
```

The `.h` files in `c/` are generated from their `.shyh` with
`shyc --emit=c-header` and committed:

```sh
make libshy-headers
```

`c/stddef.h` and `c/stdint.h` are hand-written routers: their contents depend on
the target (`__SIZEOF_POINTER__`), which a generated header would freeze at one
value. `c/stdarg.h` is generated but expands to the Shy varargs builtin, so it
serves `shyc`; a plain C compiler needs its own `<stdarg.h>`.

Floating-point code on Shy also needs the soft-float runtime, `-lfloat`.
`printf` prints `%f` with simple truncation; `scanf` reads no floats.
