//! Instruction and terminator emission.
//!
//! These are methods on the same function-body emitter; they live in their own
//! file only to keep each file to one screenful of concerns.

use super::Body;
use crate::ir::*;
use crate::llvm::types::*;
use crate::source::{Diagnostic, Span};
use std::collections::HashSet;

impl Body<'_> {
    pub(super) fn emit_inst(&mut self, inst: &Inst) -> Result<(), Diagnostic> {
        match &inst.kind {
            InstKind::Load {
                address,
                ty: load_ty,
                volatile,
                atomic,
            } => {
                if load_ty.is_void() {
                    return Err(self.void_error(inst, "load"));
                }
                if atomic.is_atomic() && !load_ty.is_scalar() {
                    return Err(error(
                        inst.span,
                        format!(
                            "function `{}`: atomic load of `{}` is not representable in LLVM IR",
                            self.function.name,
                            ty(*load_ty)
                        ),
                    ));
                }
                let address = self.address(address)?;
                let prefix = self.result_prefix(inst, "load")?;
                let mut text = format!("{prefix}load ");
                if atomic.is_atomic() {
                    text.push_str("atomic ");
                    if *volatile {
                        text.push_str("volatile ");
                    }
                    text.push_str(&format!("{}, {address}", ty(*load_ty)));
                    text.push_str(&format!(
                        " {}, align {}",
                        load_order(*atomic),
                        self.natural_align(*load_ty)
                    ));
                } else {
                    if *volatile {
                        text.push_str("volatile ");
                    }
                    text.push_str(&format!("{}, {address}", ty(*load_ty)));
                }
                self.w.line(text);
            }
            InstKind::Store {
                address,
                value,
                ty: store_ty,
                volatile,
                atomic,
            } => {
                if store_ty.is_void() {
                    return Err(self.void_error(inst, "store"));
                }
                if atomic.is_atomic() && !store_ty.is_scalar() {
                    return Err(error(
                        inst.span,
                        format!(
                            "function `{}`: atomic store of `{}` is not representable in LLVM IR",
                            self.function.name,
                            ty(*store_ty)
                        ),
                    ));
                }
                let address = self.address(address)?;
                let value = self.as_untyped(*value, *store_ty)?;
                let mut text = String::from("store ");
                if atomic.is_atomic() {
                    text.push_str("atomic ");
                    if *volatile {
                        text.push_str("volatile ");
                    }
                    text.push_str(&format!("{} {value}, {address}", ty(*store_ty)));
                    text.push_str(&format!(
                        " {}, align {}",
                        store_order(*atomic),
                        self.natural_align(*store_ty)
                    ));
                } else {
                    if *volatile {
                        text.push_str("volatile ");
                    }
                    text.push_str(&format!("{} {value}, {address}", ty(*store_ty)));
                }
                self.w.line(text);
            }
            InstKind::PtrOffset { base, offset } => {
                let prefix = self.result_prefix(inst, "ptr_offset")?;
                let base = self.pointer_operand(*base)?;
                let index = self.index_operand(*offset)?;
                self.w
                    .line(format!("{prefix}getelementptr i8, {base}, {index}"));
            }
            InstKind::SymbolAddr { symbol: name } => {
                let prefix = self.result_prefix(inst, "symbol_addr")?;
                self.w.line(format!(
                    "{prefix}getelementptr i8, ptr {}, i64 0",
                    symbol(name)
                ));
            }
            InstKind::SlotAddr { slot } => {
                let prefix = self.result_prefix(inst, "slot_addr")?;
                self.w.line(format!(
                    "{prefix}getelementptr i8, ptr {}, i64 0",
                    self.slot_name(*slot)
                ));
            }
            InstKind::Binary {
                op,
                lhs,
                rhs,
                ty: result_ty,
            } => {
                let prefix = self.result_prefix(inst, "binary operation")?;
                let lhs = self.as_untyped(*lhs, *result_ty)?;
                let rhs = self.as_untyped(*rhs, *result_ty)?;
                self.w.line(format!(
                    "{prefix}{} {} {lhs}, {rhs}",
                    binary_op(*op),
                    ty(*result_ty)
                ));
            }
            InstKind::Unary {
                op,
                value,
                ty: result_ty,
            } => {
                let prefix = self.result_prefix(inst, "unary operation")?;
                let value = self.as_untyped(*value, *result_ty)?;
                let text = match op {
                    UnaryOp::Neg => format!("{prefix}sub {} 0, {value}", ty(*result_ty)),
                    UnaryOp::FNeg => format!("{prefix}fneg {} {value}", ty(*result_ty)),
                    UnaryOp::Not => format!("{prefix}xor {} {value}, -1", ty(*result_ty)),
                };
                self.w.line(text);
            }
            InstKind::ICmp {
                pred,
                lhs,
                rhs,
                ty: operand_ty,
            } => {
                let prefix = self.result_prefix(inst, "icmp")?;
                let lhs = self.as_untyped(*lhs, *operand_ty)?;
                let rhs = self.as_untyped(*rhs, *operand_ty)?;
                self.w.line(format!(
                    "{prefix}icmp {pred} {} {lhs}, {rhs}",
                    ty(*operand_ty)
                ));
            }
            InstKind::FCmp {
                pred,
                lhs,
                rhs,
                ty: operand_ty,
            } => {
                let prefix = self.result_prefix(inst, "fcmp")?;
                let lhs = self.as_untyped(*lhs, *operand_ty)?;
                let rhs = self.as_untyped(*rhs, *operand_ty)?;
                self.w.line(format!(
                    "{prefix}fcmp {} {} {lhs}, {rhs}",
                    float_predicate(*pred),
                    ty(*operand_ty)
                ));
            }
            InstKind::Cast { op, value, to } => {
                self.emit_cast(inst, *op, *value, *to)?;
            }
            InstKind::Call(call) => self.emit_call(inst, call)?,
            InstKind::MemCpy { dst, src, bytes } => {
                let dst = self.address(dst)?;
                let src = self.address(src)?;
                let length = self.length_constant(*bytes);
                self.w.line(format!(
                    "call void @llvm.memcpy.p0.p0.{}({dst}, {src}, {length}, i1 false)",
                    ptr_int_name(self.options.layout)
                ));
            }
            InstKind::MemSet { dst, byte, bytes } => {
                let dst = self.address(dst)?;
                let length = self.length_constant(*bytes);
                self.w.line(format!(
                    "call void @llvm.memset.p0.{}({dst}, i8 {byte}, {length}, i1 false)",
                    ptr_int_name(self.options.layout)
                ));
            }
            InstKind::InlineAsm(asm) => {
                return Err(error(
                    inst.span,
                    format!(
                        "function `{}`: inline assembly cannot be represented in LLVM IR \
                         (Shy-specific register bindings): {:?}",
                        self.function.name, asm.text
                    ),
                ));
            }
        }
        Ok(())
    }

    fn emit_cast(
        &mut self,
        inst: &Inst,
        op: CastOp,
        value: Operand,
        to: Type,
    ) -> Result<(), Diagnostic> {
        let from = self.function.operand_type(value);
        let source = self.text(value);
        let prefix = self.result_prefix(inst, "cast")?;
        let (from_name, to_name) = (ty(from), ty(to));
        let layout = self.options.layout;
        let text = match op {
            CastOp::ZExt | CastOp::SExt => {
                if !from.is_integer() || !to.is_integer() {
                    return Err(self.cast_error(inst, op, from, to));
                }
                if from == to {
                    // `zext i32 %x to i32` is rejected by LLVM; `or 0` is the
                    // identity for every bit pattern.
                    format!("{prefix}or {to_name} {source}, 0")
                } else if from.integer_bits() < to.integer_bits() {
                    let op = if matches!(op, CastOp::ZExt) {
                        "zext"
                    } else {
                        "sext"
                    };
                    format!("{prefix}{op} {from_name} {source} to {to_name}")
                } else {
                    return Err(self.cast_error(inst, op, from, to));
                }
            }
            CastOp::Trunc => {
                if !from.is_integer()
                    || !to.is_integer()
                    || from == to
                    || from.integer_bits() < to.integer_bits()
                {
                    return Err(self.cast_error(inst, op, from, to));
                }
                format!("{prefix}trunc {from_name} {source} to {to_name}")
            }
            CastOp::Bitcast => {
                if from.is_aggregate() || to.is_aggregate() {
                    return Err(error(
                        inst.span,
                        format!(
                            "function `{}`: LLVM cannot bitcast to or from the opaque aggregate \
                             type used for `{}`",
                            self.function.name,
                            ty(if from.is_aggregate() { from } else { to })
                        ),
                    ));
                }
                if from.size(layout) != to.size(layout) {
                    return Err(self.cast_error(inst, op, from, to));
                }
                format!("{prefix}bitcast {from_name} {source} to {to_name}")
            }
            CastOp::FPToSI | CastOp::FPToUI => {
                if !from.is_float() || !to.is_integer() {
                    return Err(self.cast_error(inst, op, from, to));
                }
                format!("{prefix}{} {from_name} {source} to {to_name}", cast_op(op))
            }
            CastOp::SIToFP | CastOp::UIToFP => {
                if !from.is_integer() || !to.is_float() {
                    return Err(self.cast_error(inst, op, from, to));
                }
                format!("{prefix}{} {from_name} {source} to {to_name}", cast_op(op))
            }
            CastOp::FPTrunc => {
                if !from.is_float() || !to.is_float() || from.size(layout) <= to.size(layout) {
                    return Err(self.cast_error(inst, op, from, to));
                }
                format!("{prefix}fptrunc {from_name} {source} to {to_name}")
            }
            CastOp::FPExt => {
                if !from.is_float() || !to.is_float() || from.size(layout) >= to.size(layout) {
                    return Err(self.cast_error(inst, op, from, to));
                }
                format!("{prefix}fpext {from_name} {source} to {to_name}")
            }
            CastOp::PtrToInt => {
                if !from.is_pointer() || !to.is_integer() {
                    return Err(self.cast_error(inst, op, from, to));
                }
                format!("{prefix}ptrtoint {from_name} {source} to {to_name}")
            }
            CastOp::IntToPtr => {
                if !from.is_integer() || !to.is_pointer() {
                    return Err(self.cast_error(inst, op, from, to));
                }
                format!("{prefix}inttoptr {from_name} {source} to ptr")
            }
        };
        self.w.line(text);
        Ok(())
    }

    fn emit_call(&mut self, inst: &Inst, call: &Call) -> Result<(), Diagnostic> {
        let prefix = if call.signature.ret.is_void() {
            String::new()
        } else {
            self.result_prefix(inst, "call")?
        };
        let callee = self.callee(call, inst.span)?;
        let layout = self.options.layout;
        let ret = call.signature.ret;
        let sret = self.abi.by_reference(ret, layout);
        let mut arguments = Vec::new();
        let mut result_buffer = None;
        if sret {
            let buffer = self.temp();
            self.w
                .line(format!("{buffer} = alloca {}, align 8", ty(ret)));
            arguments.push(format!(
                "ptr sret({}) {} {buffer}",
                super::super::abi::array(ret, layout),
                super::super::abi::alignment(ret, layout)
            ));
            result_buffer = Some(buffer);
        }
        for (index, argument) in call.args.iter().enumerate() {
            let wanted = call
                .signature
                .params
                .get(index)
                .copied()
                .unwrap_or_else(|| self.function.operand_type(*argument));
            if self.abi.by_reference(wanted, layout) {
                // A memory argument is a pointer; the value lives in a scratch
                // slot so the callee sees the IR aggregate as one object.
                let slot = self.temp();
                let value = self.as_untyped(*argument, wanted)?;
                self.w.line(format!(
                    "{slot} = alloca {}, align {}",
                    super::super::abi::array(wanted, layout),
                    wanted.align(layout).unwrap_or(1).max(1)
                ));
                self.w
                    .line(format!("store {} {value}, ptr {slot}", ty(wanted)));
                arguments.push(if self.abi.copies() {
                    format!(
                        "ptr byval({}) {} {slot}",
                        super::super::abi::array(wanted, layout),
                        super::super::abi::alignment(wanted, layout)
                    )
                } else {
                    format!("ptr {slot}")
                });
            } else {
                arguments.push(self.as_typed(*argument, wanted)?);
            }
        }
        let arguments = arguments.join(", ");
        if let Some(buffer) = result_buffer {
            self.w.line(format!("call void {callee}({arguments})"));
            let prefix = self.result_prefix(inst, "call")?;
            self.w
                .line(format!("{prefix}load {}, ptr {buffer}", ty(ret)));
            return Ok(());
        }
        let text = if call.signature.variadic {
            let mut params: Vec<String> = call
                .signature
                .params
                .iter()
                .map(|param| ty(*param))
                .collect();
            params.push("...".to_string());
            format!(
                "{prefix}call {} ({}) {callee}({arguments})",
                ty(call.signature.ret),
                params.join(", ")
            )
        } else {
            format!(
                "{prefix}call {} {callee}({arguments})",
                ty(call.signature.ret)
            )
        };
        self.w.line(text);
        Ok(())
    }

    fn callee(&mut self, call: &Call, span: Option<Span>) -> Result<String, Diagnostic> {
        let operand = match &call.target {
            CallTarget::Direct(name) => return Ok(symbol(name)),
            CallTarget::Indirect(operand) => *operand,
        };
        let operand_ty = self.function.operand_type(operand);
        match operand {
            Operand::Value(value) if operand_ty.is_pointer() => Ok(self.value(value)),
            Operand::Constant(constant) if operand_ty.is_pointer() => {
                let name = self.temp();
                let bits = constant.as_u64() & ptr_mask(self.options.layout);
                self.w.line(format!(
                    "{name} = inttoptr {} {bits} to ptr",
                    ptr_int_name(self.options.layout)
                ));
                Ok(name)
            }
            Operand::Value(value) if operand_ty.is_integer() => {
                let name = self.temp();
                self.w.line(format!(
                    "{name} = inttoptr {} {} to ptr",
                    ty(operand_ty),
                    self.value(value)
                ));
                Ok(name)
            }
            Operand::Constant(constant) if operand_ty.is_integer() => {
                let name = self.temp();
                let bits = constant.as_u64() & ptr_mask(self.options.layout);
                self.w.line(format!(
                    "{name} = inttoptr {} {bits} to ptr",
                    ptr_int_name(self.options.layout)
                ));
                Ok(name)
            }
            _ => Err(error(
                span,
                format!(
                    "function `{}`: indirect call target has type `{}`, which is not a pointer",
                    self.function.name,
                    ty(operand_ty)
                ),
            )),
        }
    }

    pub(super) fn emit_terminator(&mut self, terminator: &Terminator) -> Result<(), Diagnostic> {
        match terminator {
            Terminator::Jump { target, .. } => {
                self.w.line(format!("br label {}", self.label(*target)));
            }
            Terminator::Branch {
                condition,
                then_block,
                else_block,
                span,
            } => {
                let condition_ty = self.function.operand_type(*condition);
                if condition_ty != Type::I1 {
                    return Err(error(
                        *span,
                        format!(
                            "function `{}`: branch condition has type `{}`, LLVM requires i1",
                            self.function.name,
                            ty(condition_ty)
                        ),
                    ));
                }
                self.w.line(format!(
                    "br i1 {}, label {}, label {}",
                    self.text(*condition),
                    self.label(*then_block),
                    self.label(*else_block)
                ));
            }
            Terminator::Switch {
                value,
                ty: switch_ty,
                default,
                cases,
                span,
            } => {
                if !switch_ty.is_integer() {
                    return Err(error(
                        *span,
                        format!(
                            "function `{}`: switch on `{}`, which is not an integer type",
                            self.function.name,
                            ty(*switch_ty)
                        ),
                    ));
                }
                let value = self.as_untyped(*value, *switch_ty)?;
                self.w.line(format!(
                    "switch {} {value}, label {} [",
                    ty(*switch_ty),
                    self.label(*default)
                ));
                self.w.indent();
                let mut seen = HashSet::new();
                for case in cases {
                    let case_value = Constant::int(*switch_ty, case.value.as_i64());
                    if !seen.insert(case_value.as_u64()) {
                        return Err(error(
                            *span,
                            format!(
                                "function `{}`: duplicate switch case value {}; LLVM rejects \
                                 duplicate cases",
                                self.function.name,
                                case.value.as_i64()
                            ),
                        ));
                    }
                    self.w.line(format!(
                        "{}, label {}",
                        constant(case_value, self.options.layout),
                        self.label(case.target)
                    ));
                }
                self.w.dedent();
                self.w.line("]");
            }
            Terminator::Return { value, span } => {
                let ret = self.function.signature.ret;
                if ret.is_void() {
                    if value.is_some() {
                        return Err(error(
                            *span,
                            format!(
                                "function `{}`: return with a value from a void function",
                                self.function.name
                            ),
                        ));
                    }
                    self.w.line("ret void");
                } else {
                    let Some(value) = value else {
                        return Err(error(
                            *span,
                            format!(
                                "function `{}`: return without a value of type `{}`",
                                self.function.name,
                                ty(ret)
                            ),
                        ));
                    };
                    let value = self.as_untyped(*value, ret)?;
                    if self.abi.by_reference(ret, self.options.layout) {
                        self.w.line(format!("store {} {value}, ptr %sret", ty(ret)));
                        self.w.line("ret void");
                    } else {
                        self.w.line(format!("ret {} {value}", ty(ret)));
                    }
                }
            }
            Terminator::Unreachable { .. } => self.w.line("unreachable"),
        }
        Ok(())
    }
}

fn cast_op(op: CastOp) -> &'static str {
    match op {
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
    }
}

fn binary_op(op: BinaryOp) -> &'static str {
    match op {
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
    }
}

/// C comparison semantics: ordered predicates are false on NaN, `une` is true.
fn float_predicate(pred: FloatPredicate) -> &'static str {
    match pred {
        FloatPredicate::Eq => "oeq",
        FloatPredicate::Ne => "une",
        FloatPredicate::Lt => "olt",
        FloatPredicate::Le => "ole",
        FloatPredicate::Gt => "ogt",
        FloatPredicate::Ge => "oge",
    }
}

/// LLVM has no `release` load and no `acquire` store. Strengthening the order
/// is sound, so those invert to the strongest legal ordering for the access.
fn load_order(order: AtomicOrder) -> &'static str {
    match order {
        AtomicOrder::Relaxed => "monotonic",
        AtomicOrder::Acquire | AtomicOrder::Release | AtomicOrder::AcqRel => "acquire",
        AtomicOrder::SeqCst | AtomicOrder::None => "seq_cst",
    }
}

fn store_order(order: AtomicOrder) -> &'static str {
    match order {
        AtomicOrder::Relaxed => "monotonic",
        AtomicOrder::Acquire | AtomicOrder::Release | AtomicOrder::AcqRel => "release",
        AtomicOrder::SeqCst | AtomicOrder::None => "seq_cst",
    }
}
