# libshy

`libshy` is the Shy bare-metal C support library. It follows familiar libc
header names where useful, but only exposes functions that can be implemented
without an OS, file descriptors, files, processes, locale, signals, or threads.

Header naming:

- `.shyh` is the native ShyC header suffix.
- `.h` wrappers are kept for C compatibility.

Initial scope:

- `<stdio.shyh>` / `<stdio.h>`: console output and buffer formatting only.
- `<string.shyh>` / `<string.h>`: memory and string helpers.
- `<ctype.shyh>` / `<ctype.h>`: ASCII character classification and case conversion.
- `<stdlib.shyh>` / `<stdlib.h>`: integer conversion and bare-metal termination helpers.
- `<stdint.shyh>` / `<stdint.h>`: fixed-width integer typedefs for the Shy ABI.
- `<stdtype.shyh>` / `<stdtype.h>`: Rust-style aliases such as `i32`, `u64`, `usize`, and `f32`.

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

Code that uses frontend floating-point operations still needs the current helper
runtime via `-lfloat`.

Out of scope until Shy has an OS/device layer:

- `FILE` and `f*` stream APIs.
- file operations such as `fopen`, `fread`, `fwrite`, and `remove`.
- process/environment APIs such as `system`, `getenv`, and `atexit`.
- locale-dependent behavior.
