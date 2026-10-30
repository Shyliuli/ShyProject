//! LLVM spellings: types, constants, symbol names, alignment, and output.
//!
//! Everything here is plain text. The backend has no LLVM bindings; it only
//! writes assembly that `llc`/`clang` can parse.

use crate::ir::{Constant, Layout, Type};
use crate::source::{Diagnostic, FileId, Span};

/// Indented line writer. One level is two spaces.
pub(super) struct Writer {
    pub(super) out: String,
    indent: usize,
}

impl Writer {
    pub(super) fn new() -> Writer {
        Writer {
            out: String::new(),
            indent: 0,
        }
    }

    pub(super) fn line(&mut self, text: impl AsRef<str>) {
        for _ in 0..self.indent {
            self.out.push_str("  ");
        }
        self.out.push_str(text.as_ref());
        self.out.push('\n');
    }

    pub(super) fn blank(&mut self) {
        self.out.push('\n');
    }

    pub(super) fn indent(&mut self) {
        self.indent += 1;
    }

    pub(super) fn dedent(&mut self) {
        self.indent -= 1;
    }
}

/// LLVM name of an IR type.
///
/// LLVM has no opaque byte blob, so an aggregate becomes `[size x i8]`. That
/// keeps the byte size but loses every type detail; the frontend owns layout.
pub(super) fn ty(ty: Type) -> String {
    match ty {
        Type::Void => "void".to_string(),
        Type::I1 => "i1".to_string(),
        Type::I8 => "i8".to_string(),
        Type::I16 => "i16".to_string(),
        Type::I32 => "i32".to_string(),
        Type::I64 => "i64".to_string(),
        Type::F32 => "float".to_string(),
        Type::F64 => "double".to_string(),
        Type::Ptr => "ptr".to_string(),
        Type::Aggregate { size, .. } => format!("[{size} x i8]"),
    }
}

/// Integer type wide enough to hold a pointer on the selected target.
pub(super) fn ptr_int_ty(layout: Layout) -> Type {
    if layout.pointer_bytes <= 4 {
        Type::I32
    } else {
        Type::I64
    }
}

/// LLVM name of [`ptr_int_ty`].
pub(super) fn ptr_int_name(layout: Layout) -> &'static str {
    if layout.pointer_bytes <= 4 {
        "i32"
    } else {
        "i64"
    }
}

/// Bit mask of a target pointer.
pub(super) fn ptr_mask(layout: Layout) -> u64 {
    if layout.pointer_bytes >= 8 {
        u64::MAX
    } else {
        (1u64 << (layout.pointer_bytes * 8)) - 1
    }
}

/// Natural alignment of a type, never zero.
pub(super) fn align_of(ty: Type, layout: Layout) -> u32 {
    ty.align(layout).unwrap_or(1).max(1)
}

/// A constant without its type, for positions that already state the type
/// (`store`, binary operands, `ret`, phi incoming values, switch cases).
///
/// Floating-point values use LLVM's hexadecimal form so the exact bits survive;
/// `f32` is printed as the 16-digit `f64` spelling, which is what LLVM itself
/// prints for `float` constants.
pub(super) fn literal(constant: Constant, layout: Layout) -> String {
    match constant.ty {
        Type::Void | Type::Aggregate { .. } => "zeroinitializer".to_string(),
        Type::I1 => {
            if constant.as_u64() != 0 {
                "true".to_string()
            } else {
                "false".to_string()
            }
        }
        Type::I8 | Type::I16 | Type::I32 | Type::I64 => constant.as_i64().to_string(),
        Type::F32 => {
            let value = f32::from_bits(constant.bits as u32) as f64;
            format!("0x{:016x}", value.to_bits())
        }
        Type::F64 => format!("0x{:016x}", constant.bits),
        Type::Ptr => {
            let bits = constant.as_u64() & ptr_mask(layout);
            pointer_constant_bits(bits, layout)
        }
    }
}

/// A constant with its type, for positions that need it (call arguments,
/// `switch` case values, `load`/`store` types).
pub(super) fn constant(constant: Constant, layout: Layout) -> String {
    format!("{} {}", ty(constant.ty), literal(constant, layout))
}

/// Reinterpret a constant's bits as `wanted`. Integer targets re-sign-extend,
/// so `i32 -1` becomes `i64 -1` instead of `4294967295`.
pub(super) fn retyped(constant: Constant, wanted: Type) -> Constant {
    if wanted.is_integer() {
        Constant::int(wanted, constant.as_i64())
    } else {
        Constant::raw(wanted, constant.bits)
    }
}

/// A `ptr` constant for an address given as integer bits.
pub(super) fn pointer_constant_bits(bits: u64, layout: Layout) -> String {
    let bits = bits & ptr_mask(layout);
    if bits == 0 {
        "null".to_string()
    } else {
        format!("inttoptr ({} {} to ptr)", ptr_int_name(layout), bits)
    }
}

/// `@name`, quoted when the name is not a plain LLVM identifier.
pub(super) fn symbol(name: &str) -> String {
    format!("@{}", quoted(name))
}

/// An identifier body, quoted and byte-escaped when needed.
pub(super) fn quoted(name: &str) -> String {
    let plain = !name.is_empty()
        && name
            .bytes()
            .enumerate()
            .all(|(index, byte)| is_plain_identifier_byte(byte, index));
    if plain {
        return name.to_string();
    }
    let mut text = String::from("\"");
    for byte in name.bytes() {
        if byte.is_ascii_graphic() && byte != b'"' && byte != b'\\' {
            text.push(byte as char);
        } else {
            text.push_str(&format!("\\{byte:02X}"));
        }
    }
    text.push('"');
    text
}

fn is_plain_identifier_byte(byte: u8, index: usize) -> bool {
    if byte.is_ascii_digit() {
        return index > 0;
    }
    matches!(
        byte,
        b'a'..=b'z' | b'A'..=b'Z' | b'_' | b'$' | b'.' | b'-'
    )
}

/// Default span for diagnostics about constructs with no useful location.
pub(super) fn error(span: Option<Span>, message: impl Into<String>) -> Diagnostic {
    Diagnostic::new(
        span.unwrap_or(Span {
            file: FileId(0),
            start: 0,
            end: 0,
        }),
        message,
    )
}
