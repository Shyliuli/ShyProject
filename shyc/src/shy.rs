//! ShyISA backend: lower the typed ShyC IR to Shy assembly text.
//!
//! The output uses only the syntax the existing `asm` crate accepts:
//!
//! ```text
//! ___DEFINE___
//! (empty)
//! ___DATA___
//! .section data.<symbol>
//! .symbol <symbol>
//! <symbol>(<offset>) [byte, ...]
//! <symbol>(<offset>) <target>(<addend>)
//! ___CODE___
//! .section text.<symbol>
//! .symbol <symbol>
//! <instructions>
//! ```
//!
//! Every code section is a `text.*` section, every exported symbol is
//! unique within the module, and no directive other than `.section`,
//! `.symbol`, labels, data records, and instructions is emitted.
//!
//! # Supported IR
//!
//! - globals: raw big-endian bytes plus 32-bit symbol relocations;
//! - functions: slots, frame pointer, parameters, returns;
//! - values: 32-bit, 64-bit pairs, `i1`/`i8`/`i16`, and pointers;
//! - all integer `BinaryOp`s, `UnaryOp::Neg`/`Not`, all integer `ICmp`
//!   predicates, integer casts, `Load`, `Store`, `PtrOffset`, `SlotAddr`,
//!   `SymbolAddr`, `MemCpy`, `MemSet`, `Call`, `Jump`, `Branch`, `Switch`,
//!   `Return`, `Unreachable`, and scalar phi nodes;
//! - aggregate load/store/call/return as byte copies, with the Shy ABI v1
//!   hidden `sret` pointer;
//! - automatic `_start` (`setn sp`; `calln main`; `seta exit 1x`) or manual
//!   startup.
//!
//! # Diagnosed, not silently dropped
//!
//! Floating-point operations and signatures, atomics, inline assembly,
//! variadic functions and calls, aggregate phi nodes, and byte-swapped casts
//! are rejected with a [`Diagnostic`].
//!
//! The backend uses `abi-plan`'s `ShyV1` classifier for argument and return
//! placement. It does not emit debug/source mappings with file names or line
//! numbers because the IR carries byte spans, not a source map; with
//! `emit_source_comments` it emits a best-effort `//source file:<offset>`
//! comment before instructions that have a span.

mod abi;
mod call;
mod cmp;
mod frame;
mod function;
mod inst;
mod mem;
mod ops;
mod runtime;

use std::collections::HashSet;

use crate::ir::Startup;
use crate::source::{Diagnostic, FileId, Span};

/// Backend options.
#[derive(Clone, Copy, Debug)]
pub struct Options {
    pub layout: crate::ir::Layout,
    pub emit_source_comments: bool,
}

/// Emit Shy assembly for one IR module.
pub fn emit(
    module: &crate::ir::Module,
    options: &Options,
) -> Result<String, crate::source::Diagnostic> {
    if options.layout.pointer_bytes != 4 || options.layout.pointer_align != 4 {
        return Err(error(
            "the Shy backend requires the 32-bit ShyISA pointer layout",
        ));
    }
    let mut writer = Writer {
        module,
        options,
        out: String::new(),
    };
    writer.run()?;
    Ok(writer.out)
}

fn fallback_span() -> Span {
    Span {
        file: FileId(0),
        start: 0,
        end: 0,
    }
}

fn error(message: impl Into<String>) -> Diagnostic {
    Diagnostic::new(fallback_span(), message)
}

struct Writer<'a> {
    module: &'a crate::ir::Module,
    options: &'a Options,
    out: String,
}

impl Writer<'_> {
    fn run(&mut self) -> Result<(), Diagnostic> {
        self.check_symbols()?;
        self.out.push_str("___DEFINE___\n\n___DATA___\n\n");
        self.emit_data()?;
        self.out.push_str("\n___CODE___\n\n");
        self.emit_code()?;
        Ok(())
    }

    fn line(&mut self, text: impl AsRef<str>) {
        self.out.push_str(text.as_ref());
        self.out.push('\n');
    }

    fn check_symbols(&self) -> Result<(), Diagnostic> {
        let mut seen: HashSet<&str> = HashSet::new();
        for global in &self.module.globals {
            check_name("global", &global.symbol)?;
            if !seen.insert(&global.symbol) {
                return Err(error(format!(
                    "duplicate symbol '{}' in the module",
                    global.symbol
                )));
            }
            for relocation in &global.relocations {
                check_name("relocation target", &relocation.symbol)?;
            }
        }
        for function in &self.module.functions {
            check_name("function", &function.name)?;
            if !seen.insert(&function.name) {
                return Err(error(format!(
                    "duplicate symbol '{}' in the module",
                    function.name
                )));
            }
        }

        if let Startup::Auto { entry, .. } = &self.module.startup {
            if entry.is_empty() {
                return Err(error("automatic startup has an empty entry symbol"));
            }
            // Only the translation unit that defines the entry symbol emits
            // `_start`, so the other objects of a multi-file program stay
            // startup-free and the image keeps exactly one entry section.
            if self
                .module
                .functions
                .iter()
                .any(|function| &function.name == entry)
                && seen.contains("_start")
            {
                return Err(error(
                    "automatic startup generates '_start', but the module also defines it",
                ));
            }
        }
        Ok(())
    }

    fn emit_data(&mut self) -> Result<(), Diagnostic> {
        for global in &self.module.globals {
            self.line(format!(".section data.{}", global.symbol));
            self.line(format!(".symbol {}", global.symbol));

            let mut offset = 0usize;
            for chunk in global.bytes.chunks(32) {
                let bytes = chunk
                    .iter()
                    .map(|byte| byte.to_string())
                    .collect::<Vec<_>>()
                    .join(",");
                self.line(format!("{}({offset}) [{bytes}]", global.symbol));
                offset += chunk.len();
            }

            for relocation in &global.relocations {
                let end = relocation.offset as u64 + 4;
                if end > global.bytes.len() as u64 {
                    return Err(error(format!(
                        "relocation at offset {} is outside global '{}' ({} bytes)",
                        relocation.offset,
                        global.symbol,
                        global.bytes.len()
                    )));
                }
                let addend = relocation.addend as u32;
                if addend == 0 {
                    self.line(format!(
                        "{}({}) {}",
                        global.symbol, relocation.offset, relocation.symbol
                    ));
                } else {
                    self.line(format!(
                        "{}({}) {}(0x{addend:x})",
                        global.symbol, relocation.offset, relocation.symbol
                    ));
                }
            }
        }
        Ok(())
    }

    fn emit_code(&mut self) -> Result<(), Diagnostic> {
        if let Startup::Auto {
            entry,
            stack_pointer,
        } = &self.module.startup
            && self
                .module
                .functions
                .iter()
                .any(|function| &function.name == entry)
        {
            self.line(".section text._start");
            self.line(".symbol _start");
            self.line(format!("setn sp 0x{stack_pointer:08x}"));
            self.line(format!("calln {entry}"));
            self.line("seta exit 1x");
        }

        for (index, function) in self.module.functions.iter().enumerate() {
            let text = function::emit(function, index, self.options)?;
            self.out.push_str(&text);
        }
        Ok(())
    }
}

/// Register and special-register spellings the assembler resolves in operand
/// position. A symbol with one of these names would be encoded as a register
/// instead of a relocation, so reject it rather than miscompile it.
const REGISTER_NAMES: [&str; 32] = [
    "0x", "1x", "2x", "3x", "4x", "5x", "6x", "7x", "8x", "9x", "ax", "bx", "cx", "dx", "ex", "fx",
    "pc", "segs", "sp", "tm", "status", "trap", "m1", "m2", "m3", "m4", "rs", "exit", "epc",
    "cause", "ksp", "sege",
];

fn check_name(kind: &str, name: &str) -> Result<(), Diagnostic> {
    if name.is_empty() {
        return Err(error(format!("empty {kind} symbol")));
    }
    if name.chars().any(char::is_whitespace) {
        return Err(error(format!("{kind} symbol '{name}' contains whitespace")));
    }
    if REGISTER_NAMES.contains(&name) {
        return Err(error(format!(
            "{kind} symbol '{name}' is a Shy register name"
        )));
    }
    Ok(())
}
