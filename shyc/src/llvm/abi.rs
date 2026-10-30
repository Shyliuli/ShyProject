//! C ABI shapes for aggregates that a target passes in memory.
//!
//! The IR records an aggregate only as `(size, align)`. That is enough for the
//! one rule every supported target shares: an aggregate larger than 16 bytes
//! is passed by reference, and System V additionally copies it into the callee
//! frame (`byval`) and returns it through a hidden pointer (`sret`). Smaller
//! aggregates need per-eightbyte classification, which the IR cannot express,
//! so they keep the opaque byte-array form.
use crate::ir::{Layout, Type};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Kind {
    SysV,
    Windows,
    Aapcs64,
    Riscv64,
    LoongArch64,
}

pub(super) fn kind(triple: &str) -> Kind {
    match triple.split('-').next().unwrap_or(triple) {
        "x86_64" if triple.contains("windows") => Kind::Windows,
        "x86_64" => Kind::SysV,
        "aarch64" => Kind::Aapcs64,
        "riscv64" => Kind::Riscv64,
        "loongarch64" => Kind::LoongArch64,
        _ => Kind::SysV,
    }
}

impl Kind {
    /// Aggregates over two eightbytes travel as a pointer instead of a value.
    pub(super) fn by_reference(self, ty: Type, layout: Layout) -> bool {
        ty.is_aggregate() && ty.size(layout).unwrap_or(0) > 16
    }

    /// Only System V and Windows copy the aggregate into the callee frame.
    /// AArch64, RISC-V, and LoongArch pass a pointer to the caller's temporary.
    pub(super) fn copies(self) -> bool {
        matches!(self, Kind::SysV | Kind::Windows)
    }
}

/// The LLVM form of an aggregate: a byte array of its size.
pub(super) fn array(ty: Type, layout: Layout) -> String {
    format!("[{} x i8]", ty.size(layout).unwrap_or(0))
}

/// The `align` attribute that follows a `byval`/`sret` type.
pub(super) fn alignment(ty: Type, layout: Layout) -> String {
    format!("align {}", ty.align(layout).unwrap_or(1).max(1))
}
