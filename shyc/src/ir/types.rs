//! Identifiers, types, constants, operands, addresses, slots, and signatures.

use std::fmt;

// ---------------------------------------------------------------------------
// Identifiers
// ---------------------------------------------------------------------------

/// The identity of one SSA value in a function.
///
/// Values are unique per function and are never renumbered.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ValueId(pub u32);

/// The identity of one basic block in a function.
///
/// Block IDs remain stable even when a dead block is removed. The removed ID
/// is not reused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BlockId(pub u32);

/// The identity of one stack slot (a memory-form local) in a function.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SlotId(pub u32);

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// A scalar or memory-only IR type.
///
/// Integer widths are exact bit widths. `i1` is the type of comparison
/// results; it occupies one byte when stored in memory. `ptr` is a 32-bit
/// ShyISA address, not a host pointer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Type {
    Void,
    I1,
    I8,
    I16,
    I32,
    I64,
    F32,
    F64,
    Ptr,
    /// An opaque memory region. Only `size` and `align` are known; the
    /// frontend keeps the real C layout. See the module-level aggregate note.
    Aggregate {
        size: u32,
        align: u32,
    },
}

impl Type {
    /// Build an opaque aggregate type from a byte size and alignment.
    pub fn aggregate(size: u32, align: u32) -> Type {
        Type::Aggregate { size, align }
    }

    /// Storage size in bytes, or `None` for `void`.
    ///
    /// These widths belong to ShyC's target data model, never to the host.
    pub fn size(self) -> Option<u32> {
        match self {
            Type::Void => None,
            Type::I1 | Type::I8 => Some(1),
            Type::I16 => Some(2),
            Type::I32 | Type::F32 | Type::Ptr => Some(4),
            Type::I64 | Type::F64 => Some(8),
            Type::Aggregate { size, .. } => Some(size),
        }
    }

    /// Natural alignment in bytes, or `None` for `void`.
    pub fn align(self) -> Option<u32> {
        match self {
            Type::Void => None,
            Type::I1 | Type::I8 => Some(1),
            Type::I16 => Some(2),
            Type::I32 | Type::F32 | Type::Ptr => Some(4),
            Type::I64 | Type::F64 => Some(8),
            Type::Aggregate { align, .. } => Some(align),
        }
    }

    pub fn is_void(self) -> bool {
        matches!(self, Type::Void)
    }

    /// True for `i1`, `i8`, `i16`, `i32`, and `i64`.
    ///
    /// Pointers are deliberately not integers here: pointer arithmetic is
    /// [`InstKind::PtrOffset`](crate::ir::InstKind::PtrOffset), not
    /// [`BinaryOp::Add`](crate::ir::BinaryOp::Add).
    pub fn is_integer(self) -> bool {
        matches!(
            self,
            Type::I1 | Type::I8 | Type::I16 | Type::I32 | Type::I64
        )
    }

    pub fn is_float(self) -> bool {
        matches!(self, Type::F32 | Type::F64)
    }

    pub fn is_pointer(self) -> bool {
        matches!(self, Type::Ptr)
    }

    /// True for values that may appear as ordinary scalar operands.
    pub fn is_scalar(self) -> bool {
        self.is_integer() || self.is_float() || self.is_pointer()
    }

    pub fn is_aggregate(self) -> bool {
        matches!(self, Type::Aggregate { .. })
    }

    /// Number of significant bits for an integer type.
    pub fn integer_bits(self) -> Option<u32> {
        match self {
            Type::I1 => Some(1),
            Type::I8 => Some(8),
            Type::I16 => Some(16),
            Type::I32 => Some(32),
            Type::I64 => Some(64),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Constants and operands
// ---------------------------------------------------------------------------

/// A raw constant with an explicit IR type.
///
/// The value is stored as bits, not as a host `usize` or host `float`. The
/// type determines the meaningful width. Constants are scalar only;
/// aggregates are always accessed through memory.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Constant {
    pub ty: Type,
    pub bits: u64,
}

impl Constant {
    /// Integer constant. `value` is truncated to `ty`'s width, so negative
    /// values become the corresponding two's-complement bit pattern.
    pub fn int(ty: Type, value: i64) -> Constant {
        Constant {
            ty,
            bits: mask_for(ty, value as u64),
        }
    }

    /// Unsigned integer constant, truncated to `ty`'s width.
    pub fn uint(ty: Type, value: u64) -> Constant {
        Constant {
            ty,
            bits: mask_for(ty, value),
        }
    }

    /// Raw bit pattern. `bits` is masked to `ty`'s width.
    pub fn raw(ty: Type, bits: u64) -> Constant {
        Constant {
            ty,
            bits: mask_for(ty, bits),
        }
    }

    /// Floating-point constant from an IEEE bit pattern.
    ///
    /// `ty` must be `f32` or `f64`; `f32` keeps the low 32 bits.
    pub fn float(ty: Type, bits: u64) -> Constant {
        Constant {
            ty,
            bits: mask_for(ty, bits),
        }
    }

    /// `f32` constant, preserving its exact bits.
    pub fn f32(value: f32) -> Constant {
        Constant {
            ty: Type::F32,
            bits: value.to_bits() as u64,
        }
    }

    /// `f64` constant, preserving its exact bits.
    pub fn f64(value: f64) -> Constant {
        Constant {
            ty: Type::F64,
            bits: value.to_bits(),
        }
    }

    pub fn zero(ty: Type) -> Constant {
        Constant { ty, bits: 0 }
    }

    /// Value interpreted as an unsigned integer, masked to the type width.
    pub fn as_u64(self) -> u64 {
        mask_for(self.ty, self.bits)
    }

    /// Value interpreted as a signed integer and sign-extended to `i64`.
    ///
    /// For pointer-typed constants this returns the raw 32-bit address as a
    /// positive `i64`.
    pub fn as_i64(self) -> i64 {
        let bits = self.as_u64();
        match self.ty.integer_bits() {
            Some(width) if width < 64 => {
                let shift = 64 - width;
                ((bits << shift) as i64) >> shift
            }
            _ => bits as i64,
        }
    }

    pub fn as_f32(self) -> f32 {
        f32::from_bits(self.bits as u32)
    }

    pub fn as_f64(self) -> f64 {
        f64::from_bits(self.bits)
    }

    /// Literal value without its type prefix, for instructions that already
    /// state the type (`store`, `switch`).
    pub(super) fn fmt_literal(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.ty {
            Type::I1 => write!(f, "{}", self.bits != 0),
            Type::F32 => write!(f, "0x{:08x}", self.bits as u32),
            Type::F64 => write!(f, "0x{:016x}", self.bits),
            Type::Ptr => write!(f, "0x{:08x}", self.bits as u32),
            ty if ty.is_integer() => write!(f, "{}", self.as_i64()),
            _ => write!(f, "0x{:x}", self.bits),
        }
    }
}

/// Mask `bits` to the storage width of `ty`. Non-integer types only mask
/// `f32`; other widths pass through unchanged.
fn mask_for(ty: Type, bits: u64) -> u64 {
    match ty {
        Type::I1 => bits & 1,
        Type::I8 => bits & 0xff,
        Type::I16 => bits & 0xffff,
        Type::I32 | Type::F32 | Type::Ptr => bits & 0xffff_ffff,
        Type::I64 | Type::F64 => bits,
        Type::Void | Type::Aggregate { .. } => bits,
    }
}

/// An instruction input: either an SSA value or an inline raw constant.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Operand {
    Value(ValueId),
    Constant(Constant),
}

impl Operand {
    pub fn value(value: ValueId) -> Operand {
        Operand::Value(value)
    }

    pub fn constant(constant: Constant) -> Operand {
        Operand::Constant(constant)
    }

    pub fn is_value(self) -> bool {
        matches!(self, Operand::Value(_))
    }

    pub fn as_value(self) -> Option<ValueId> {
        match self {
            Operand::Value(value) => Some(value),
            Operand::Constant(_) => None,
        }
    }

    /// Replace uses of `old` with `new`.
    pub fn replace_value(&mut self, old: ValueId, new: Operand) {
        let matches = match self {
            Operand::Value(value) => *value == old,
            Operand::Constant(_) => false,
        };
        if matches {
            *self = new;
        }
    }

    /// Literal value without a type prefix, for instructions that already
    /// state the type (`store`, `switch`).
    pub(super) fn fmt_literal(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Operand::Value(value) => write!(f, "{}", value),
            Operand::Constant(constant) => constant.fmt_literal(f),
        }
    }
}

impl From<ValueId> for Operand {
    fn from(value: ValueId) -> Operand {
        Operand::Value(value)
    }
}

impl From<Constant> for Operand {
    fn from(constant: Constant) -> Operand {
        Operand::Constant(constant)
    }
}

// ---------------------------------------------------------------------------
// Addresses and memory attributes
// ---------------------------------------------------------------------------

/// The base of a memory address.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AddressBase {
    /// A pointer/integer value or pointer constant.
    Operand(Operand),
    /// The frame address of one stack slot.
    Slot(SlotId),
    /// The link-time address of a symbol.
    Symbol(String),
}

/// A byte address written as `base + offset`.
///
/// `offset` is a signed byte displacement. The effective 32-bit address is
/// `base + offset` modulo 2^32. The lowerer computes it without an object
/// bounds check; the eventual load/store decides whether the address is
/// valid. A symbol base becomes a link-time relocation. This form makes
/// `[slot %s0 + 4]`, `[%p - 8]`, and `[@global + 12]` all explicit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Address {
    pub base: AddressBase,
    pub offset: i64,
}

impl Address {
    pub fn new(base: AddressBase, offset: i64) -> Address {
        Address { base, offset }
    }

    pub fn value(value: ValueId) -> Address {
        Address::new(AddressBase::Operand(Operand::Value(value)), 0)
    }

    pub fn constant(constant: Constant) -> Address {
        Address::new(AddressBase::Operand(Operand::Constant(constant)), 0)
    }

    pub fn slot(slot: SlotId) -> Address {
        Address::new(AddressBase::Slot(slot), 0)
    }

    pub fn symbol(symbol: impl Into<String>) -> Address {
        Address::new(AddressBase::Symbol(symbol.into()), 0)
    }

    pub fn with_offset(mut self, offset: i64) -> Address {
        self.offset = offset;
        self
    }

    /// Visit the value/constant operand used to compute the base, if any.
    pub fn for_each_use(&self, f: &mut impl FnMut(&Operand)) {
        if let AddressBase::Operand(operand) = &self.base {
            f(operand);
        }
    }

    pub fn for_each_use_mut(&mut self, f: &mut impl FnMut(&mut Operand)) {
        if let AddressBase::Operand(operand) = &mut self.base {
            f(operand);
        }
    }
}

/// Memory ordering for an atomic load or store.
///
/// `None` means the access is not atomic. The IR is not a formal memory
/// model; it only records the source language's ordering so that the
/// optimizer can be conservative.
///
/// Atomic read-modify-write and standalone fences are not first-class
/// instructions. Lower them to
/// [`InstKind::InlineAsm`](crate::ir::InstKind::InlineAsm) with memory bindings
/// (for example ShyISA's `atoma`) or to a runtime helper call.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum AtomicOrder {
    #[default]
    None,
    Relaxed,
    Acquire,
    Release,
    AcqRel,
    SeqCst,
}

impl AtomicOrder {
    pub fn is_atomic(self) -> bool {
        self != AtomicOrder::None
    }
}

// ---------------------------------------------------------------------------
// Stack slots
// ---------------------------------------------------------------------------

/// A memory-form local. The function owns the slot; the lowerer decides its
/// frame offset. Slots keep the C address-taken/local lifetime model simple
/// and are the source of truth until `mem2reg` promotes them.
///
/// Slots are fixed-size. The IR has no `alloca` and does not represent
/// variable-length arrays; a frontend that needs dynamic stack storage must
/// use a runtime helper. `align` must be a nonzero power of two.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Slot {
    pub name: String,
    pub ty: Type,
    pub align: u32,
}

impl Slot {
    pub fn new(name: impl Into<String>, ty: Type) -> Slot {
        let align = ty.align().unwrap_or(1);
        Slot {
            name: name.into(),
            ty,
            align,
        }
    }

    pub fn with_align(name: impl Into<String>, ty: Type, align: u32) -> Slot {
        Slot {
            name: name.into(),
            ty,
            align,
        }
    }

    /// Storage size in bytes; `void` slots have size zero and are invalid in
    /// well-formed IR.
    pub fn size(&self) -> u32 {
        self.ty.size().unwrap_or(0)
    }
}

// ---------------------------------------------------------------------------
// Signatures
// ---------------------------------------------------------------------------

/// The source-level type of a function or indirect call.
///
/// This is intentionally ABI-free. `params` may be any length, 64-bit scalars
/// stay single values, and `Aggregate` parameters/returns are allowed. The
/// backend decides how many argument slots exist, which arguments go on the
/// stack, and how aggregate returns use `sret`.
///
/// Calls are expected to provide one operand per fixed parameter, plus any
/// variadic arguments, with matching types. The IR does not duplicate those
/// types in each call argument and does not verify arity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Signature {
    pub ret: Type,
    pub params: Vec<Type>,
    pub variadic: bool,
}

impl Signature {
    pub fn new(ret: Type, params: Vec<Type>) -> Signature {
        Signature {
            ret,
            params,
            variadic: false,
        }
    }

    pub fn variadic(ret: Type, params: Vec<Type>) -> Signature {
        Signature {
            ret,
            params,
            variadic: true,
        }
    }
}
