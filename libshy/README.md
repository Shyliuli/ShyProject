# libshy

libshy is ShyC's standard library. Like Rust's, it has a C library at the
bottom and three layers above it:

| View | Contents |
|---|---|
| `c/` | the C include view: only `.h`, and only C-library headers — `stdio`, `stdlib`, `string`, `ctype`, `unistd`, `fcntl`, `stddef`, `stdint`, `stdtype`, `stdarg` |
| `core/` | ShyC declarations and their C spelling (`str.shyh`, `option.{shyh,h}`, `result.{shyh,h}`), plus `core.shyc` |
| `alloc/` | `vec.shyh`, `string.shyh`, `alloc.shyc` |
| `std/` | `fs.shyh`, `io.shyh`, `std.shyc` |
| `libc/` | the C library in ShyC spelling: `*.shyh` declarations and the `libc.c` implementation |
| `sys/` | the bare-machine platform calls, `shy.shyc` |

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
| ShyOS | system calls in `projects/os/user` | `libc/libc.c` | linked |
| hosted LLVM target | system libc | system libc | linked |

On bare Shy, `read` returns 0 at the end of input, `open` fails because there
is no file system, and the heap grows from `_end` up to the stack at
`0x00fff000`. `free` keeps nothing: `malloc` is a bump allocator.

Streams are unbuffered. `FILE` is opaque, so on a hosted target `stdin`,
`stdout`, and `stderr` are the system library's.

## Using it

`shycc` links the whole library by default and drops the functions a program
never reaches; `-nostdlib` turns it off.

```c
#include <stdio.h>
#include <alloc/vec.shyh>
#include <std/io.shyh>

int main(void) {
    String text = read_to_string(stdin);
    Vec<int> lines = {0};
    for (int i = 0; i < text.len; i++) {
        if (text.data[i] == '\n') {
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

Both print `2 lines`. `read_file(path)` from `<std/fs.shyh>` returns
`Option<String>`, which is `None` on bare Shy.

`Str` is a borrowed pointer and length; print one with
`printf("%.*s", s.len, s.ptr)`. `Vec<T>` is generic, so `alloc/vec.shyh`
carries its whole definition; the other types declare their methods in the
header and define them once in the layer's `.shyc`.

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
