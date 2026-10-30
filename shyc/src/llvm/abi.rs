//! C ABI shapes for aggregates that a target passes in memory.
//!
//! The IR records an aggregate only as `(size, align)`. That is enough for the
//! one rule every supported target shares: an aggregate larger than 16 bytes
//! is passed by reference, and System V additionally copies it into the callee
//! frame (`byval`) and returns it through a hidden pointer (`sret`). Smaller
//! aggregates need per-eightbyte classification, which the IR cannot express,
//! so they keep the opaque byte-array form.
use crate::ir::{Layout, Type};

/// How AArch64 passes an aggregate that is not memory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Arm {
    /// One SIMD register per element, all of the same floating type.
    Hfa { elem: &'static str, count: u32 },
    /// One or two general registers holding the bytes.
    Integers,
}

impl Arm {
    pub(super) fn llvm_type(self, size: u32) -> String {
        match self {
            Arm::Hfa { elem, count } => format!("[{count} x {elem}]"),
            Arm::Integers => {
                if size <= 8 {
                    "i64".to_string()
                } else {
                    "[2 x i64]".to_string()
                }
            }
        }
    }
}

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

/// One register-sized piece of a small aggregate.
#[derive(Clone, Copy, Debug)]
pub(super) struct Chunk {
    pub offset: u32,
    pub size: u32,
    pub float: bool,
}

impl Chunk {
    /// The LLVM type that carries this piece in its register.
    pub(super) fn llvm_type(self) -> String {
        if self.float {
            if self.size > 4 {
                "double".to_string()
            } else {
                "float".to_string()
            }
        } else {
            match self.size {
                1 => "i8".to_string(),
                2 => "i16".to_string(),
                3 | 4 => "i32".to_string(),
                _ => "i64".to_string(),
            }
        }
    }
}

impl Kind {
    /// Split a small aggregate into its eightbytes, System V style.
    ///
    /// `None` keeps the IR form: the value is a scalar or memory, or the
    /// target classifies small aggregates by rules this backend does not
    /// model yet.
    pub(super) fn chunks(self, ty: Type, layout: Layout) -> Option<Vec<Chunk>> {
        if self != Kind::SysV || !ty.is_aggregate() {
            return None;
        }
        let size = ty.size(layout)?;
        if size == 0 || size > 16 {
            return None;
        }
        let floats = ty.aggregate_floats();
        Some(
            (0..size.div_ceil(8))
                .map(|index| Chunk {
                    offset: index * 8,
                    size: size.saturating_sub(index * 8).min(8),
                    float: floats & (1 << index) != 0,
                })
                .collect(),
        )
    }

    /// The AArch64 rule for a small aggregate.
    ///
    /// A homogeneous aggregate of one floating type travels in one SIMD
    /// register per element, even past 16 bytes. Everything else up to 16 bytes
    /// travels in one or two general registers. The IR mask plus the alignment
    /// identifies the element type: an all-float aggregate whose alignment is 4
    /// holds floats, and one aligned to 8 holds doubles.
    pub(super) fn arm_small(self, ty: Type, layout: Layout) -> Option<Arm> {
        if self != Kind::Aapcs64 || !ty.is_aggregate() {
            return None;
        }
        let size = ty.size(layout)?;
        let align = ty.align(layout)?;
        let eightbytes = size.div_ceil(8);
        let floats = ty.aggregate_floats();
        let covered = if eightbytes >= 8 {
            u8::MAX
        } else {
            (1u8 << eightbytes) - 1
        };
        if size > 0
            && floats & covered == covered
            && (align == 4 || align == 8)
            && size % align == 0
            && size / align <= 4
        {
            return Some(Arm::Hfa {
                elem: if align == 4 { "float" } else { "double" },
                count: size / align,
            });
        }
        if size <= 16 {
            return Some(Arm::Integers);
        }
        None
    }

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
