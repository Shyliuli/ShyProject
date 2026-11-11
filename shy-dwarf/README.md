# shy-dwarf

`shy-dwarf` builds a small ELF/DWARF file for GDB from ShyISA compiler debug
metadata. It is normally invoked by `shycc` when both debug flags are enabled:

```sh
target/bin/shycc --shy-emit-source-lines --shy-emit-debug-info \
  input.c -o app.sfs --sym app.sym
```

This writes `app.debug.elf` next to `app.sfs` and advertises it to GDB through
the emulator's `qXfer:exec-file:read:` RSP packet, so `target remote` is enough;
no manual `add-symbol-file` is required.

The generated DWARF currently includes:

- `DW_TAG_compile_unit` with source file directory information
- `DW_TAG_subprogram` per function (with low/high PC and `DW_AT_frame_base`)
- Source line tables (`.debug_line`)
- Call frame information (`.debug_frame`) for GDB `bt` unwinding across ShyC
  frames
- `DW_TAG_base_type`, `DW_TAG_pointer_type`, `DW_TAG_array_type`,
  `DW_TAG_structure_type` / `DW_TAG_union_type`, `DW_TAG_typedef`,
  `DW_TAG_enumeration_type` for types
- `DW_TAG_variable` and `DW_TAG_formal_parameter` for locals and parameters,
  using `DW_OP_breg15` offsets (frame base is `fx` / `r15`)

Manual use:

```sh
cargo run -q -p shy-dwarf -- \
  --asm app.shy \
  --debug app.shy.debug.json \
  --sym app.sym \
  -o app.debug.elf
```

Multiple `--asm`/`--debug` pairs are accepted for multi-file links.

Backtraces use ShyC's standard function prologue (`pusha fx; seta fx sp`) and
the ShyISA call convention where `calln`/`calla` push the return PC on the
upward-growing stack. Because the debugger runs through GDB's MIPS compatibility
target, the generated CFI uses a synthetic negated CFA only for frame ordering;
the caller PC and caller `fx` are recovered from the real stack addresses
(`fx - 8` and `fx - 4` after the prologue). This lets GDB unwind nested C calls
with `bt`.
