# libshy

`libshy` is the Shy bare-metal C support library. It follows familiar libc
header names where useful, but only exposes functions that can be implemented
without an OS, file descriptors, files, processes, locale, signals, or threads.

Header layout:

- `include/c/` is the C compatibility layer: `ctype`, `stdio`, `stdlib`,
  `string`, and `unistd`.
- `include/core/` is the language/runtime foundation: `stddef`, `stdint`,
  `stdarg`, `stdtype`, `option`, and `result`.
- `include/*.h` and `include/*.shyh` are compatibility wrappers. They keep the
  flat names such as `<stdio.h>` and `<stdint.shyh>` working with
  `-Ilibshy/include`, and route them into `c/` or `core/`.

`.shyh` is the native ShyC header suffix; `.h` is the C-compatible spelling.
`shycc` adds only `libshy/include` to the search path, so existing sources and
ShyOS builds do not need to change their includes.

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
