# libshy

`libshy` is the Shy bare-metal C support library. It follows familiar libc
header names where useful, but only exposes functions that can be implemented
without an OS, file descriptors, files, processes, locale, signals, or threads.

Header layout:

- `c/` is the default C library layer: the native ShyC headers `ctype`,
  `stdio`, `stdlib`, `string`, and `unistd`, together with their generated C
  counterparts `c/*.h`. Both `<stdio.h>` and `<c/stdio.h>` name the same file.
- `core/` is the language/runtime layer: `stddef`, `stdint`, `stdtype`,
  `stdarg`, `option`, and `result`. `stddef`, `stdint`, and `stdtype` have
  generated `.h` files; `stdarg.h`, `option.h`, and `result.h` stay
  hand-written routers because the Shy `va_arg` expansion and generic payload
  enums have no C spelling yet. `core/` is not a default search layer: core
  basics stay available through the flat names (`<stddef.h>`, `<stdint.h>`,
  `<stdtype.h>`) while `<core/xxx.shyh>` is the explicit spelling.
- `include/` contains only compatibility wrappers. The flat names keep the
  pre-split spellings such as `<stdio.h>` and `<stdint.shyh>` working; the
  `c/` and `core/` subdirectories add the explicit path spellings
  `<c/stdio.h>` and `<core/stddef.shyh>`. All wrappers use relative includes,
  so the historical `-Ilibshy/include`-only setup still resolves every name.

`.shyh` is the native ShyC header suffix; `.h` is the C-compatible spelling.
`shycc` searches three roots, in this order:

```text
libshy/c       # default C library layer
libshy         # explicit <c/...> and <core/...> spellings only
libshy/include # flat and path compatibility wrappers
```

- `<stdio.h>` resolves directly from the default `c/` layer, and `<c/stdio.h>`
  names the same file through the explicit path. `c/` is the ordinary libc
  layer, so its names need no prefix.
- `<stddef.h>` and `<stdint.h>` are the flat core-basics spellings, kept by the
  `include/` wrappers. `<core/xxx.h>` and `<core/xxx.shyh>` use the explicit
  path. `libshy/core` is never a search root, so no core header is reachable
  under a bare name by accident.

The `.h` files in `c/` and `core/` are generated with
`shyc --emit=c-header`, never edited by hand:

```sh
make libshy-headers
```

The target generates `core/stddef.h`, `core/stdint.h`, and `core/stdtype.h`
before the `c/` headers, because the C headers include them. A generated header
carries the source header's main-file `#include` lines and `#define` macros,
derives its include guard from the output path, and prints only declarations
that belong to that source header; declarations from included files stay with
their own includes. The generated headers are committed, so a bare `cargo run
-p shycc` or `cargo test` works without running the target first.

`<stdarg.h>` is target-routed: a Shy build gets the native `core/stdarg.shyh`
descriptor, while a hosted LLVM build gets `stdarg-hosted.h` from the compiler
(`shyc` ships that file).

Initial scope:

- `<stdio.shyh>` / `<stdio.h>`: console output and buffer formatting only.
- `<string.shyh>` / `<string.h>`: memory and string helpers.
- `<ctype.shyh>` / `<ctype.h>`: ASCII character classification and case conversion.
- `<stdlib.shyh>` / `<stdlib.h>`: integer conversion and bare-metal termination helpers.
- `<unistd.shyh>` / `<unistd.h>`: byte-oriented stdin/stdout/stderr `read` and `write`.
- `<stdint.shyh>` / `<stdint.h>`: fixed-width integer typedefs for the Shy ABI.
- `<stdtype.shyh>` / `<stdtype.h>`: Rust-style aliases such as `i32`, `u64`, `usize`, and `f32`.
- `<option.shyh>` / `<option.h>` and `<result.shyh>` / `<result.h>`: ShyC
  payload-enum language types.

ABI typedefs follow ShyC's 32-bit address model: pointers, `size_t`,
`ptrdiff_t`, `intptr_t`, `uintptr_t`, `usize`, and `isize` are 32-bit. `long`,
`int64_t`, `uint64_t`, `i64`, and `u64` remain 64-bit.

Link with `shycc -llibshy` to use the implementation:

```sh
cargo run -q -p shycc -- app.shyc -llibshy -o app.sfs
```

`printf` supports integer, pointer, string, character, and simple fixed-point
`%f` formatting. Floating-point formatting is intentionally small and truncates
to the requested precision instead of doing full libc rounding. `scanf` supports
integer, string, character, `%n`, and literal `%%` conversions. Floating-point
input is not implemented yet.

`read` accepts only `STDIN_FILENO`; `write` accepts only `STDOUT_FILENO` and
`STDERR_FILENO`. Other file descriptors return `-1`.

Code that uses frontend floating-point operations still needs the current helper
runtime via `-lfloat`.

Out of scope until Shy has an OS/device layer:

- `FILE` and `f*` stream APIs.
- file operations such as `fopen`, `fread`, `fwrite`, and `remove`.
- process/environment APIs such as `system`, `getenv`, and `atexit`.
- locale-dependent behavior.
