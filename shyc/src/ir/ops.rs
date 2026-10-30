//! Operations, instructions, calls, and inline assembly.
//!
//! Control-flow constructs (phis and terminators) live in the `cfg` module.

use super::types::*;
use crate::source::Span;

// ---------------------------------------------------------------------------
// Operations and casts
// ---------------------------------------------------------------------------

/// Integer and floating-point binary operations.
///
/// Integer `Add`, `Sub`, and `Mul` wrap modulo the type width. `UDiv`/`URem`
/// treat operands as unsigned; `SDiv`/`SRem` treat them as signed, truncate
/// division toward zero, and give the remainder the dividend's sign.
/// Division/remainder by zero traps. Signed `MIN / -1` and `MIN % -1` trap
/// (overflow). Shifts trap when the amount is greater than or equal to the
/// bit width; the IR does not silently mask the amount. `LShr` zero-fills,
/// `AShr` sign-fills, and `Shl` shifts left. Floating operations have
/// ordinary C semantics without fast-math reassociation.
///
/// For shifts, `lhs` has the result type while `rhs` is an integer operand
/// (possibly of a different width); the amount is not masked.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BinaryOp {
    Add,
    Sub,
    Mul,
    UDiv,
    SDiv,
    URem,
    SRem,
    And,
    Or,
    Xor,
    Shl,
    LShr,
    AShr,
    FAdd,
    FSub,
    FMul,
    FDiv,
}

impl BinaryOp {
    pub fn is_float(self) -> bool {
        matches!(
            self,
            BinaryOp::FAdd | BinaryOp::FSub | BinaryOp::FMul | BinaryOp::FDiv
        )
    }

    /// Division and remainder, including floating-point division. The effects
    /// query treats all of these conservatively as potentially trapping.
    pub fn is_division(self) -> bool {
        matches!(
            self,
            BinaryOp::UDiv | BinaryOp::SDiv | BinaryOp::URem | BinaryOp::SRem | BinaryOp::FDiv
        )
    }

    pub fn is_shift(self) -> bool {
        matches!(self, BinaryOp::Shl | BinaryOp::LShr | BinaryOp::AShr)
    }
}

/// Unary scalar operations.
///
/// Integer `Neg` is two's-complement negation modulo the type width, so it
/// cannot overflow. `Not` is bitwise complement. `FNeg` flips the sign bit.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum UnaryOp {
    Neg,
    FNeg,
    Not,
}

/// Comparison predicate for integers and pointers.
///
/// `Ult`/`Ule`/`Ugt`/`Uge` are unsigned; `Slt`/`Sle`/`Sgt`/`Sge` are signed.
/// The result is `i1`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum IntPredicate {
    Eq,
    Ne,
    Ult,
    Ule,
    Ugt,
    Uge,
    Slt,
    Sle,
    Sgt,
    Sge,
}

/// Floating-point comparison with C semantics.
///
/// `Eq`, `Lt`, `Le`, `Gt`, and `Ge` are false when either operand is NaN.
/// `Ne` is true when either operand is NaN. The result is `i1`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FloatPredicate {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

/// Explicit scalar conversion.
///
/// `ZExt`/`SExt` widen integers; `Trunc` narrows. `Bitcast` keeps the bit
/// width and changes interpretation (`i32`/`f32`, `i64`/`f64`, `ptr`/`i32`).
/// `FPToSI`/`FPToUI` convert to integers and are conservatively treated as
/// trapping on out-of-range values. `SIToFP`/`UIToFP` convert from integers.
/// `FPTrunc`/`FPExt` change float width. `PtrToInt`/`IntToPtr` convert
/// between the 32-bit address type and integers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CastOp {
    ZExt,
    SExt,
    Trunc,
    Bitcast,
    FPToSI,
    FPToUI,
    SIToFP,
    UIToFP,
    FPTrunc,
    FPExt,
    PtrToInt,
    IntToPtr,
}

// ---------------------------------------------------------------------------
// Effects
// ---------------------------------------------------------------------------

/// Conservative optimization properties of an instruction.
///
/// `side_effects` means the instruction is observable: it performs memory,
/// I/O, atomic, or opaque machine work and cannot be removed or duplicated.
/// `may_trap` means it can fault at run time (invalid memory, division by
/// zero, an invalid shift, a bad float-to-int conversion). By default a
/// trap-capable instruction must not be speculated, hoisted above a
/// conditional, or dropped by DCE just because its result is unused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Effects {
    pub side_effects: bool,
    pub may_trap: bool,
}

impl Effects {
    pub const PURE: Effects = Effects {
        side_effects: false,
        may_trap: false,
    };

    /// True when the instruction can be removed if its result is unused.
    pub fn is_pure(self) -> bool {
        !self.side_effects && !self.may_trap
    }

    /// Alias for [`Effects::is_pure`], worded for DCE callers.
    pub fn is_removable(self) -> bool {
        self.is_pure()
    }
}

// ---------------------------------------------------------------------------
// Calls
// ---------------------------------------------------------------------------

/// A direct symbol call or an indirect call through a pointer value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CallTarget {
    Direct(String),
    Indirect(Operand),
}

/// A call instruction payload.
///
/// Calls are conservatively side-effecting and may trap. The IR does not add
/// attributes such as `readnone`; a later whole-program analysis can refine
/// that. Extra arguments to a variadic callee are simply appended to `args`;
/// each is an ordinary operand, so its type is known even though it is not in
/// `signature.params`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Call {
    pub target: CallTarget,
    pub args: Vec<Operand>,
    pub signature: Signature,
    /// Argument and return placement for the target ABI this module is built
    /// for, planned by `abi-plan` while the field layout was still available.
    pub abi: abi_plan::CallPlan,
}

impl Call {
    pub fn direct(
        symbol: impl Into<String>,
        args: Vec<Operand>,
        signature: Signature,
        abi: abi_plan::CallPlan,
    ) -> Call {
        Call {
            target: CallTarget::Direct(symbol.into()),
            args,
            signature,
            abi,
        }
    }

    pub fn indirect(
        callee: Operand,
        args: Vec<Operand>,
        signature: Signature,
        abi: abi_plan::CallPlan,
    ) -> Call {
        Call {
            target: CallTarget::Indirect(callee),
            args,
            signature,
            abi,
        }
    }

    pub fn for_each_use(&self, f: &mut impl FnMut(&Operand)) {
        if let CallTarget::Indirect(callee) = &self.target {
            f(callee);
        }
        for arg in &self.args {
            f(arg);
        }
    }

    pub fn for_each_use_mut(&mut self, f: &mut impl FnMut(&mut Operand)) {
        if let CallTarget::Indirect(callee) = &mut self.target {
            f(callee);
        }
        for arg in &mut self.args {
            f(arg);
        }
    }
}

// ---------------------------------------------------------------------------
// Inline assembly
// ---------------------------------------------------------------------------

/// How a bound inline-assembly name is used.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AsmAccess {
    /// Read the local's bytes into a register before the assembly.
    Read,
    /// Write the register's bytes back to the local after the assembly.
    Write,
    /// Read before and write back after.
    ReadWrite,
    /// Substitute the local's address; no value is loaded or written back.
    AddressOnly,
}

/// A placeholder binding such as `{a}` or `{&lock}`.
///
/// Bindings point at **memory**, never at SSA values. This is the key rule
/// that keeps inline assembly from silently defining or clobbering SSA
/// values: the lowerer loads from `address` before the assembly and stores
/// back after it (`Read`/`Write`/`ReadWrite`), or only passes the address
/// (`AddressOnly`). `address` is normally a slot base, optionally with a
/// byte offset for a field. The assembly text is opaque.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AsmBinding {
    pub name: String,
    pub address: Address,
    /// Number of bytes to load/store. Ignored for `AddressOnly`.
    pub bytes: u32,
    pub access: AsmAccess,
}

impl AsmBinding {
    pub fn new(
        name: impl Into<String>,
        address: Address,
        bytes: u32,
        access: AsmAccess,
    ) -> AsmBinding {
        AsmBinding {
            name: name.into(),
            address,
            bytes,
            access,
        }
    }
}

/// Inline assembly.
///
/// The instruction is a full barrier: it is side-effecting, may trap, and the
/// optimizer must not move memory operations across it. No clobber list is
/// stored because the IR takes the maximally conservative position: all
/// registers and memory may be clobbered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InlineAsm {
    pub text: String,
    pub bindings: Vec<AsmBinding>,
}

impl InlineAsm {
    pub fn new(text: impl Into<String>) -> InlineAsm {
        InlineAsm {
            text: text.into(),
            bindings: Vec::new(),
        }
    }

    pub fn with_binding(mut self, binding: AsmBinding) -> InlineAsm {
        self.bindings.push(binding);
        self
    }

    pub fn for_each_use(&self, f: &mut impl FnMut(&Operand)) {
        for binding in &self.bindings {
            binding.address.for_each_use(&mut *f);
        }
    }

    pub fn for_each_use_mut(&mut self, f: &mut impl FnMut(&mut Operand)) {
        for binding in &mut self.bindings {
            binding.address.for_each_use_mut(&mut *f);
        }
    }
}

// ---------------------------------------------------------------------------
// Instructions
// ---------------------------------------------------------------------------

/// One non-terminator instruction body.
///
/// A value-producing variant has a known result type; see
/// [`InstKind::result_type`]. `Load` and `Store` are scalar or whole-aggregate
/// memory accesses. `MemCpy`/`MemSet` are the explicit aggregate copy/zero
/// helpers. Aggregate values may only flow through the limited positions
/// documented on [`Type::Aggregate`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InstKind {
    /// Read `ty` from memory.
    ///
    /// `volatile` accesses are observable and must not be removed, merged, or
    /// reordered with other volatile accesses. `atomic` is `None` for a
    /// non-atomic access. Atomic accesses are scalar only; a volatile
    /// aggregate access is lowered as a byte copy.
    Load {
        address: Address,
        ty: Type,
        volatile: bool,
        atomic: AtomicOrder,
    },
    /// Write `value` as `ty` to memory. Always side-effecting.
    Store {
        address: Address,
        value: Operand,
        ty: Type,
        volatile: bool,
        atomic: AtomicOrder,
    },
    /// Byte pointer arithmetic: `base + offset`. The result is `ptr`.
    ///
    /// Both inputs are operands, so the offset may be a runtime index. The
    /// frontend scales array indices by the element size before emitting
    /// this; the offset is always in bytes. The offset operand is an integer
    /// and only its low 32 bits take part in the address addition. Addition
    /// wraps in the 32-bit address space and does not trap; only the eventual
    /// load/store checks the address.
    PtrOffset { base: Operand, offset: Operand },
    /// Materialize the link-time address of a symbol as a `ptr` value.
    ///
    /// Use this when the address itself must be an SSA value, for example a
    /// function pointer or a pointer passed to a call. A direct global access
    /// can instead use [`AddressBase::Symbol`] in its address.
    SymbolAddr { symbol: String },
    /// Materialize the frame address of a stack slot as a `ptr` value.
    ///
    /// This is the address-of-local operation. Use `PtrOffset` for field or
    /// element offsets. A direct access to the slot can instead use
    /// [`AddressBase::Slot`].
    SlotAddr { slot: SlotId },
    /// Integer or floating-point binary operation. The result type is `ty`.
    Binary {
        op: BinaryOp,
        lhs: Operand,
        rhs: Operand,
        ty: Type,
    },
    /// Unary operation. The result type is `ty`.
    Unary {
        op: UnaryOp,
        value: Operand,
        ty: Type,
    },
    /// Integer/pointer comparison. The result is `i1`.
    ICmp {
        pred: IntPredicate,
        lhs: Operand,
        rhs: Operand,
        ty: Type,
    },
    /// Floating-point comparison with C semantics. The result is `i1`.
    FCmp {
        pred: FloatPredicate,
        lhs: Operand,
        rhs: Operand,
        ty: Type,
    },
    /// Explicit conversion. The result type is `to`.
    Cast {
        op: CastOp,
        value: Operand,
        to: Type,
    },
    /// Direct or indirect call. The result is the signature's return value,
    /// or no value for `void`.
    Call(Box<Call>),
    /// Copy `bytes` bytes from `src` to `dst`. The ranges must not partially
    /// overlap; an exact same-range copy is allowed. Side-effecting and
    /// potentially trapping.
    MemCpy {
        dst: Address,
        src: Address,
        bytes: u32,
    },
    /// Fill `bytes` bytes at `dst` with `byte`. Side-effecting and potentially
    /// trapping. This is how aggregate zeroing is represented.
    MemSet { dst: Address, byte: u8, bytes: u32 },
    /// Opaque inline assembly; a full barrier. It defines no SSA value and
    /// communicates only through the bound memory locations.
    InlineAsm(InlineAsm),
}

impl InstKind {
    /// Type of the SSA value this instruction defines, or `None` when it is a
    /// statement. The frontend must allocate the result value with this type.
    pub fn result_type(&self) -> Option<Type> {
        match self {
            InstKind::Load { ty, .. } => Some(*ty),
            InstKind::PtrOffset { .. }
            | InstKind::SymbolAddr { .. }
            | InstKind::SlotAddr { .. } => Some(Type::Ptr),
            InstKind::Binary { ty, .. } | InstKind::Unary { ty, .. } => Some(*ty),
            InstKind::ICmp { .. } | InstKind::FCmp { .. } => Some(Type::I1),
            InstKind::Cast { to, .. } => Some(*to),
            InstKind::Call(call) => {
                if call.signature.ret.is_void() {
                    None
                } else {
                    Some(call.signature.ret)
                }
            }
            InstKind::Store { .. }
            | InstKind::MemCpy { .. }
            | InstKind::MemSet { .. }
            | InstKind::InlineAsm(_) => None,
        }
    }

    /// Conservative effect classification. See [`Effects`].
    pub fn effects(&self) -> Effects {
        match self {
            InstKind::Load {
                volatile, atomic, ..
            } => Effects {
                side_effects: *volatile || atomic.is_atomic(),
                may_trap: true,
            },
            InstKind::Store { .. } => Effects {
                side_effects: true,
                may_trap: true,
            },
            InstKind::PtrOffset { .. }
            | InstKind::SymbolAddr { .. }
            | InstKind::SlotAddr { .. } => Effects::PURE,
            InstKind::Binary { op, .. } => Effects {
                side_effects: false,
                may_trap: op.is_division() || op.is_shift(),
            },
            InstKind::Unary { .. } => Effects::PURE,
            InstKind::ICmp { .. } | InstKind::FCmp { .. } => Effects::PURE,
            InstKind::Cast { op, .. } => Effects {
                side_effects: false,
                may_trap: matches!(op, CastOp::FPToSI | CastOp::FPToUI),
            },
            InstKind::Call(_) => Effects {
                side_effects: true,
                may_trap: true,
            },
            InstKind::MemCpy { .. } | InstKind::MemSet { .. } => Effects {
                side_effects: true,
                may_trap: true,
            },
            InstKind::InlineAsm(_) => Effects {
                side_effects: true,
                may_trap: true,
            },
        }
    }

    pub fn has_side_effects(&self) -> bool {
        self.effects().side_effects
    }

    pub fn may_trap(&self) -> bool {
        self.effects().may_trap
    }

    /// Inline assembly is the IR's only explicitly defined full barrier.
    /// Calls are opaque side effects, so conservative passes must also avoid
    /// moving memory operations across them.
    pub fn is_full_barrier(&self) -> bool {
        matches!(self, InstKind::InlineAsm(_))
    }

    /// Visit every operand read by this instruction.
    pub fn for_each_use(&self, f: &mut impl FnMut(&Operand)) {
        match self {
            InstKind::Load { address, .. } => address.for_each_use(&mut *f),
            InstKind::Store { address, value, .. } => {
                address.for_each_use(&mut *f);
                f(value);
            }
            InstKind::PtrOffset { base, offset } => {
                f(base);
                f(offset);
            }
            InstKind::SymbolAddr { .. } | InstKind::SlotAddr { .. } => {}
            InstKind::Binary { lhs, rhs, .. } => {
                f(lhs);
                f(rhs);
            }
            InstKind::Unary { value, .. } => f(value),
            InstKind::ICmp { lhs, rhs, .. } | InstKind::FCmp { lhs, rhs, .. } => {
                f(lhs);
                f(rhs);
            }
            InstKind::Cast { value, .. } => f(value),
            InstKind::Call(call) => call.for_each_use(&mut *f),
            InstKind::MemCpy { dst, src, .. } => {
                dst.for_each_use(&mut *f);
                src.for_each_use(&mut *f);
            }
            InstKind::MemSet { dst, .. } => dst.for_each_use(&mut *f),
            InstKind::InlineAsm(asm) => asm.for_each_use(&mut *f),
        }
    }

    /// Mutable form of [`InstKind::for_each_use`], used by replacement
    /// helpers. It visits memory address bases too, so an address like
    /// `[%v3 + 4]` participates in use replacement.
    pub fn for_each_use_mut(&mut self, f: &mut impl FnMut(&mut Operand)) {
        match self {
            InstKind::Load { address, .. } => address.for_each_use_mut(&mut *f),
            InstKind::Store { address, value, .. } => {
                address.for_each_use_mut(&mut *f);
                f(value);
            }
            InstKind::PtrOffset { base, offset } => {
                f(base);
                f(offset);
            }
            InstKind::SymbolAddr { .. } | InstKind::SlotAddr { .. } => {}
            InstKind::Binary { lhs, rhs, .. } => {
                f(lhs);
                f(rhs);
            }
            InstKind::Unary { value, .. } => f(value),
            InstKind::ICmp { lhs, rhs, .. } | InstKind::FCmp { lhs, rhs, .. } => {
                f(lhs);
                f(rhs);
            }
            InstKind::Cast { value, .. } => f(value),
            InstKind::Call(call) => call.for_each_use_mut(&mut *f),
            InstKind::MemCpy { dst, src, .. } => {
                dst.for_each_use_mut(&mut *f);
                src.for_each_use_mut(&mut *f);
            }
            InstKind::MemSet { dst, .. } => dst.for_each_use_mut(&mut *f),
            InstKind::InlineAsm(asm) => asm.for_each_use_mut(&mut *f),
        }
    }
}

/// An instruction together with its optional SSA result.
///
/// A statement (`Store`, `MemCpy`, `MemSet`, `InlineAsm`, or a `void` call)
/// has `result: None`. An instruction that produces a value must have a
/// result allocated with exactly the type returned by
/// [`InstKind::result_type`].
/// [`Function::push_value`](crate::ir::Function::push_value) maintains that
/// invariant.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Inst {
    pub result: Option<ValueId>,
    pub kind: InstKind,
    /// Source location for diagnostics/debug info. Generated instructions use
    /// `None`.
    pub span: Option<Span>,
}

impl Inst {
    pub fn new(kind: InstKind, result: Option<ValueId>) -> Inst {
        Inst {
            result,
            kind,
            span: None,
        }
    }

    pub fn value(result: ValueId, kind: InstKind) -> Inst {
        Inst::new(kind, Some(result))
    }

    pub fn statement(kind: InstKind) -> Inst {
        Inst::new(kind, None)
    }

    pub fn with_span(mut self, span: Span) -> Inst {
        self.span = Some(span);
        self
    }

    pub fn result_type(&self) -> Option<Type> {
        self.kind.result_type()
    }

    pub fn effects(&self) -> Effects {
        self.kind.effects()
    }

    pub fn has_side_effects(&self) -> bool {
        self.kind.has_side_effects()
    }

    pub fn may_trap(&self) -> bool {
        self.kind.may_trap()
    }

    pub fn is_full_barrier(&self) -> bool {
        self.kind.is_full_barrier()
    }

    pub fn for_each_use(&self, f: &mut impl FnMut(&Operand)) {
        self.kind.for_each_use(f);
    }

    pub fn for_each_use_mut(&mut self, f: &mut impl FnMut(&mut Operand)) {
        self.kind.for_each_use_mut(f);
    }
}
