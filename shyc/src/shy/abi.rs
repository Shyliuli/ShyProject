//! Adapter between ShyC's source-level signatures and `abi-plan`.
//!
//! The IR deliberately carries no ABI facts. This module maps IR types to the
//! `abi-plan` vocabulary and asks `abi-plan` for the Shy ABI v1 call plan, so
//! argument placement and return classification are not reimplemented here.
//!
//! Only the parts the Shy backend can execute are accepted:
//! - scalar arguments in the eight argument registers `4x`..`bx`;
//! - scalar stack blocks below the call's `sp`;
//! - aggregate arguments as by-value stack copies;
//! - scalar returns in `1x` (and `2x` for the high word);
//! - aggregate returns through the hidden pointer in `4x`.
//!
//! Floating-point values and variadic signatures are diagnosed earlier because
//! the backend does not implement them.

use crate::abi::{
    AbiKind, AbiType, CallPlan, FunctionSignature, Location, RegisterClass, ReturnPlan, plan,
};
use crate::ir::{Function, Layout, Signature, Type};
use crate::source::{Diagnostic, Span};

/// Map one IR type to the ABI classifier's type.
///
/// The IR does not record integer signedness, and the ShyV1 classifier only
/// consults sizes, so signedness is irrelevant here.
fn abi_type(ty: Type, layout: Layout) -> AbiType {
    match ty {
        Type::Void => AbiType::Void,
        Type::I1 => AbiType::Bool,
        Type::I8 => AbiType::Integer {
            bits: 8,
            signed: false,
        },
        Type::I16 => AbiType::Integer {
            bits: 16,
            signed: false,
        },
        Type::I32 => AbiType::Integer {
            bits: 32,
            signed: false,
        },
        Type::I64 => AbiType::Integer {
            bits: 64,
            signed: false,
        },
        Type::F32 => AbiType::Float { bits: 32 },
        Type::F64 => AbiType::Float { bits: 64 },
        Type::Ptr => AbiType::Pointer {
            bits: (layout.pointer_bytes * 8) as u16,
        },
        // `abi-plan` recognises Struct/Union as aggregate but keeps Opaque as a
        // scalar shape, so an IR aggregate must become a field-less Struct.
        Type::Aggregate { size, align } => AbiType::Struct {
            fields: Vec::new(),
            size,
            align: align as u16,
        },
    }
}

/// Ask `abi-plan` for the Shy ABI v1 plan of one IR signature.
pub(crate) fn plan_signature(
    signature: &Signature,
    layout: Layout,
    span: Span,
) -> Result<CallPlan, Diagnostic> {
    let abi_signature = FunctionSignature {
        ret: abi_type(signature.ret, layout),
        params: signature
            .params
            .iter()
            .map(|ty| abi_type(*ty, layout))
            .collect(),
        variadic: signature.variadic,
    };
    plan(AbiKind::ShyV1, &abi_signature)
        .map_err(|error| Diagnostic::new(span, format!("Shy ABI v1: {error}")))
}

pub(crate) fn plan_function(function: &Function, span: Span) -> Result<CallPlan, Diagnostic> {
    plan_signature(&function.signature, function.layout, span)
}

/// Plan a call from the argument types actually passed.
///
/// A variadic call carries more arguments than the signature names, and every
/// one of them occupies slots by the same rules, so the plan is built from the
/// real list rather than from the fixed parameter list.
pub(crate) fn plan_call(
    signature: &Signature,
    arguments: &[crate::ir::Type],
    layout: Layout,
    span: Span,
) -> Result<CallPlan, Diagnostic> {
    let abi_signature = FunctionSignature {
        ret: abi_type(signature.ret, layout),
        params: arguments.iter().map(|ty| abi_type(*ty, layout)).collect(),
        variadic: signature.variadic,
    };
    plan(AbiKind::ShyV1, &abi_signature)
        .map_err(|error| Diagnostic::new(span, format!("Shy ABI v1: {error}")))
}

/// Map an argument register slot index (0..7) to its Shy register name.
pub(crate) const ARGUMENT_REGISTERS: [&str; 8] = ["4x", "5x", "6x", "7x", "8x", "9x", "ax", "bx"];

pub(crate) fn argument_register(index: u16) -> Option<&'static str> {
    ARGUMENT_REGISTERS.get(index as usize).copied()
}

/// Map a return register slot index to its Shy register name.
fn return_register(index: u16) -> Option<&'static str> {
    match index {
        1 => Some("1x"),
        2 => Some("2x"),
        _ => None,
    }
}

/// Visit every `(register, source_offset)` pair in a direct return plan.
pub(crate) fn direct_return_parts(plan: &CallPlan) -> Result<Vec<(&'static str, u32)>, String> {
    let ReturnPlan::Direct { parts } = &plan.return_value else {
        return Err("unsupported return classification".into());
    };
    let mut registers = Vec::with_capacity(parts.len());
    for part in parts {
        let Location::Register(register) = part.location else {
            return Err("return value is not in a register".into());
        };
        if register.class != RegisterClass::Gpr {
            return Err("return value is not in a general register".into());
        }
        let name = return_register(register.index)
            .ok_or_else(|| format!("unsupported return register slot {}", register.index))?;
        registers.push((name, part.source_offset));
    }
    Ok(registers)
}

/// Mask for a narrow ABI value part, or `None` when the part fills a word.
pub(crate) fn part_mask(ty: Type, size: u32) -> Option<u32> {
    match (ty, size) {
        (Type::I1, 1) => Some(1),
        (Type::I8, 1) => Some(0xff),
        (Type::I16, 2) => Some(0xffff),
        _ => None,
    }
}

/// Mask for a narrow scalar return value.
pub(crate) fn return_mask(ty: Type) -> Option<u32> {
    match ty {
        Type::I1 => Some(1),
        Type::I8 => Some(0xff),
        Type::I16 => Some(0xffff),
        _ => None,
    }
}
