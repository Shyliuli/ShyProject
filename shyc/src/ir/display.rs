//! Human-readable formatting for the IR.

use super::cfg::*;
use super::function::*;
use super::ops::*;
use super::types::*;
use std::fmt;

impl fmt::Display for ValueId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "%v{}", self.0)
    }
}

impl fmt::Display for BlockId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "bb{}", self.0)
    }
}

impl fmt::Display for SlotId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "%s{}", self.0)
    }
}

impl fmt::Display for Type {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Type::Void => f.write_str("void"),
            Type::I1 => f.write_str("i1"),
            Type::I8 => f.write_str("i8"),
            Type::I16 => f.write_str("i16"),
            Type::I32 => f.write_str("i32"),
            Type::I64 => f.write_str("i64"),
            Type::F32 => f.write_str("f32"),
            Type::F64 => f.write_str("f64"),
            Type::Ptr => f.write_str("ptr"),
            Type::Aggregate { size, align } => {
                write!(f, "aggregate({}, align {})", size, align)
            }
        }
    }
}

impl fmt::Display for Constant {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ", self.ty)?;
        self.fmt_literal(f)
    }
}

impl fmt::Display for Operand {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Operand::Value(value) => write!(f, "{}", value),
            Operand::Constant(constant) => write!(f, "{}", constant),
        }
    }
}

impl fmt::Display for Address {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.base {
            AddressBase::Operand(operand) => write!(f, "[{}", operand)?,
            AddressBase::Slot(slot) => write!(f, "[slot {}", slot)?,
            AddressBase::Symbol(symbol) => write!(f, "[@{}", symbol)?,
        }
        if self.offset > 0 {
            write!(f, " + {}", self.offset)?;
        } else if self.offset < 0 {
            write!(f, " - {}", self.offset.unsigned_abs())?;
        }
        f.write_str("]")
    }
}

impl fmt::Display for AtomicOrder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            AtomicOrder::None => "none",
            AtomicOrder::Relaxed => "relaxed",
            AtomicOrder::Acquire => "acquire",
            AtomicOrder::Release => "release",
            AtomicOrder::AcqRel => "acq_rel",
            AtomicOrder::SeqCst => "seq_cst",
        })
    }
}

impl fmt::Display for Signature {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("(")?;
        for (index, ty) in self.params.iter().enumerate() {
            if index > 0 {
                f.write_str(", ")?;
            }
            write!(f, "{}", ty)?;
        }
        if self.variadic {
            if !self.params.is_empty() {
                f.write_str(", ")?;
            }
            f.write_str("...")?;
        }
        write!(f, ") -> {}", self.ret)
    }
}

impl fmt::Display for BinaryOp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            BinaryOp::Add => "add",
            BinaryOp::Sub => "sub",
            BinaryOp::Mul => "mul",
            BinaryOp::UDiv => "udiv",
            BinaryOp::SDiv => "sdiv",
            BinaryOp::URem => "urem",
            BinaryOp::SRem => "srem",
            BinaryOp::And => "and",
            BinaryOp::Or => "or",
            BinaryOp::Xor => "xor",
            BinaryOp::Shl => "shl",
            BinaryOp::LShr => "lshr",
            BinaryOp::AShr => "ashr",
            BinaryOp::FAdd => "fadd",
            BinaryOp::FSub => "fsub",
            BinaryOp::FMul => "fmul",
            BinaryOp::FDiv => "fdiv",
        })
    }
}

impl fmt::Display for UnaryOp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            UnaryOp::Neg => "neg",
            UnaryOp::FNeg => "fneg",
            UnaryOp::Not => "not",
        })
    }
}

impl fmt::Display for IntPredicate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            IntPredicate::Eq => "eq",
            IntPredicate::Ne => "ne",
            IntPredicate::Ult => "ult",
            IntPredicate::Ule => "ule",
            IntPredicate::Ugt => "ugt",
            IntPredicate::Uge => "uge",
            IntPredicate::Slt => "slt",
            IntPredicate::Sle => "sle",
            IntPredicate::Sgt => "sgt",
            IntPredicate::Sge => "sge",
        })
    }
}

impl fmt::Display for FloatPredicate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            FloatPredicate::Eq => "eq",
            FloatPredicate::Ne => "ne",
            FloatPredicate::Lt => "lt",
            FloatPredicate::Le => "le",
            FloatPredicate::Gt => "gt",
            FloatPredicate::Ge => "ge",
        })
    }
}

impl fmt::Display for CastOp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            CastOp::ZExt => "zext",
            CastOp::SExt => "sext",
            CastOp::Trunc => "trunc",
            CastOp::Bitcast => "bitcast",
            CastOp::FPToSI => "fptosi",
            CastOp::FPToUI => "fptoui",
            CastOp::SIToFP => "sitofp",
            CastOp::UIToFP => "uitofp",
            CastOp::FPTrunc => "fptrunc",
            CastOp::FPExt => "fpext",
            CastOp::PtrToInt => "ptrtoint",
            CastOp::IntToPtr => "inttoptr",
        })
    }
}

impl fmt::Display for CallTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CallTarget::Direct(symbol) => write!(f, "@{}", symbol),
            CallTarget::Indirect(operand) => write!(f, "{}", operand),
        }
    }
}

impl fmt::Display for Call {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "call {} {}", self.signature.ret, self.target)?;
        f.write_str("(")?;
        for (index, arg) in self.args.iter().enumerate() {
            if index > 0 {
                f.write_str(", ")?;
            }
            write!(f, "{}", arg)?;
        }
        f.write_str(")")
    }
}

impl fmt::Display for AsmAccess {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            AsmAccess::Read => "r",
            AsmAccess::Write => "w",
            AsmAccess::ReadWrite => "rw",
            AsmAccess::AddressOnly => "addr",
        })
    }
}

impl fmt::Display for InlineAsm {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "asm {:?}", self.text)?;
        if !self.bindings.is_empty() {
            f.write_str(" bindings { ")?;
            for (index, binding) in self.bindings.iter().enumerate() {
                if index > 0 {
                    f.write_str(", ")?;
                }
                write!(
                    f,
                    "{:?}: {} {} {}",
                    binding.name, binding.access, binding.bytes, binding.address
                )?;
            }
            f.write_str(" }")?;
        }
        Ok(())
    }
}

impl fmt::Display for InstKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            InstKind::Load {
                address,
                ty,
                volatile,
                atomic,
            } => {
                write!(f, "load {}", ty)?;
                if *volatile {
                    f.write_str(" volatile")?;
                }
                if atomic.is_atomic() {
                    write!(f, " atomic {}", atomic)?;
                }
                write!(f, ", {}", address)
            }
            InstKind::Store {
                address,
                value,
                ty,
                volatile,
                atomic,
            } => {
                write!(f, "store {} ", ty)?;
                value.fmt_literal(f)?;
                if *volatile {
                    f.write_str(" volatile")?;
                }
                if atomic.is_atomic() {
                    write!(f, " atomic {}", atomic)?;
                }
                write!(f, ", {}", address)
            }
            InstKind::PtrOffset { base, offset } => write!(f, "ptr_offset {}, {}", base, offset),
            InstKind::SymbolAddr { symbol } => write!(f, "symbol_addr @{}", symbol),
            InstKind::SlotAddr { slot } => write!(f, "slot_addr {}", slot),
            InstKind::Binary { op, lhs, rhs, ty } => write!(f, "{} {} {}, {}", op, ty, lhs, rhs),
            InstKind::Unary { op, value, ty } => write!(f, "{} {} {}", op, ty, value),
            InstKind::ICmp { pred, lhs, rhs, ty } => {
                write!(f, "icmp {} {} {}, {}", pred, ty, lhs, rhs)
            }
            InstKind::FCmp { pred, lhs, rhs, ty } => {
                write!(f, "fcmp {} {} {}, {}", pred, ty, lhs, rhs)
            }
            InstKind::Cast { op, value, to } => write!(f, "{} {} to {}", op, value, to),
            InstKind::Call(call) => write!(f, "{}", call),
            InstKind::MemCpy { dst, src, bytes } => {
                write!(f, "memcpy {}, {}, {}", dst, src, bytes)
            }
            InstKind::MemSet { dst, byte, bytes } => {
                write!(f, "memset {}, {}, {}", dst, byte, bytes)
            }
            InstKind::InlineAsm(asm) => write!(f, "{}", asm),
        }
    }
}

impl fmt::Display for Inst {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(result) = self.result {
            write!(f, "{} = ", result)?;
        }
        write!(f, "{}", self.kind)
    }
}

impl fmt::Display for Phi {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} = phi {} ", self.result, self.ty)?;
        for (index, incoming) in self.incoming.iter().enumerate() {
            if index > 0 {
                f.write_str(", ")?;
            }
            write!(f, "[{} from {}]", incoming.value, incoming.predecessor)?;
        }
        Ok(())
    }
}

impl fmt::Display for Terminator {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Terminator::Jump { target, .. } => write!(f, "jump {}", target),
            Terminator::Branch {
                condition,
                then_block,
                else_block,
                ..
            } => {
                write!(f, "branch {}, {}, {}", condition, then_block, else_block)
            }
            Terminator::Switch {
                value,
                ty,
                default,
                cases,
                ..
            } => {
                write!(f, "switch {} ", ty)?;
                value.fmt_literal(f)?;
                write!(f, ", default {}", default)?;
                if !cases.is_empty() {
                    f.write_str(" [")?;
                    for (index, case) in cases.iter().enumerate() {
                        if index > 0 {
                            f.write_str(", ")?;
                        }
                        write!(f, "{} -> {}", case.value, case.target)?;
                    }
                    f.write_str("]")?;
                }
                Ok(())
            }
            Terminator::Return {
                value: Some(value), ..
            } => write!(f, "return {}", value),
            Terminator::Return { value: None, .. } => f.write_str("return"),
            Terminator::Unreachable { .. } => f.write_str("unreachable"),
        }
    }
}

impl fmt::Display for Function {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.fmt_indented(f, "")
    }
}

impl fmt::Display for Global {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.fmt_indented(f, "")
    }
}

impl fmt::Display for Startup {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Startup::Auto {
                entry,
                stack_pointer,
            } => {
                write!(f, "auto(entry=@{}, sp=0x{:08x})", entry, stack_pointer)
            }
            Startup::Manual => f.write_str("manual"),
        }
    }
}

impl fmt::Display for Resources {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.memory_bytes {
            Some(bytes) => write!(f, "mem={}", bytes)?,
            None => f.write_str("mem=default")?,
        }
        match self.stack_bytes {
            Some(bytes) => write!(f, " stack={}", bytes)?,
            None => f.write_str(" stack=default")?,
        }
        Ok(())
    }
}

impl fmt::Display for Module {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "module {:?} {{", self.name)?;
        writeln!(f, "  startup: {}", self.startup)?;
        writeln!(f, "  resources: {}", self.resources)?;
        for global in &self.globals {
            writeln!(f)?;
            global.fmt_indented(f, "  ")?;
        }
        for function in &self.functions {
            writeln!(f)?;
            function.fmt_indented(f, "  ")?;
        }
        writeln!(f, "}}")
    }
}
