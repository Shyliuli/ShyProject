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

use crate::abi::{CallPlan, Location, RegisterClass, ReturnPlan};
use crate::ir::{Function, Type};
use crate::source::{Diagnostic, Span};

/// The plan lowering attached to this function.
pub(crate) fn plan_function(function: &Function, _span: Span) -> Result<CallPlan, Diagnostic> {
    Ok(function.abi.clone())
}

/// Map an argument register slot index (0..7) to its Shy register name.
pub(crate) const ARGUMENT_REGISTERS: [&str; 8] = ["4x", "5x", "6x", "7x", "8x", "9x", "ax", "bx"];

pub(crate) fn argument_register(index: u16) -> Option<&'static str> {
    ARGUMENT_REGISTERS.get(index as usize).copied()
}

/// Map a return register slot index to its Shy register name.
pub(crate) fn return_register(index: u16) -> Option<&'static str> {
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
