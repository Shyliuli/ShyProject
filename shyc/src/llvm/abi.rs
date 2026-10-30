//! Reading the target ABI plan that lowering attached to the IR.
//!
//! Every rule that decides where an argument or a return value lives — System V
//! eightbytes, AArch64 homogeneous float aggregates, RISC-V field flattening,
//! memory passing, hidden return pointers — belongs to `abi-plan`. Lowering
//! asks for the plan while field layout is still known and stores it in the IR,
//! so this module only turns plan parts into LLVM types.
use abi_plan::{CallPlan, Location, Passing, RegisterClass, ReturnPlan, ValuePart};

use crate::ir::{Layout, Type};

/// One register-sized piece of an aggregate.
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

#[derive(Debug)]
pub(super) enum Shape {
    /// The value crosses the boundary as its IR type.
    Value,
    /// One LLVM argument or result per plan part.
    Registers(Vec<Chunk>),
    /// Passed by reference; `byval` means the callee owns a copy.
    Indirect { byval: bool },
}

pub(super) fn param(plan: &CallPlan, index: usize, ty: Type) -> Shape {
    let Some(argument) = plan.args.get(index) else {
        return Shape::Value;
    };
    match &argument.passing {
        Passing::Direct { parts } if ty.is_aggregate() => Shape::Registers(chunks(parts)),
        Passing::Indirect { byval, .. } => Shape::Indirect { byval: *byval },
        _ => Shape::Value,
    }
}

/// The return shape, and whether the function takes a hidden pointer.
pub(super) fn returns(plan: &CallPlan, ty: Type) -> (bool, Shape) {
    match &plan.return_value {
        ReturnPlan::Indirect { .. } => (true, Shape::Indirect { byval: false }),
        ReturnPlan::Direct { parts } if ty.is_aggregate() => {
            (false, Shape::Registers(chunks(parts)))
        }
        _ => (false, Shape::Value),
    }
}

fn chunks(parts: &[ValuePart]) -> Vec<Chunk> {
    parts
        .iter()
        .map(|part| Chunk {
            offset: part.source_offset,
            size: part.size,
            float: matches!(
                part.location,
                Location::Register(register) if register.class == RegisterClass::Fpr
            ),
        })
        .collect()
}

/// The LLVM form of an aggregate: a byte array of its size.
pub(super) fn array(ty: Type, layout: Layout) -> String {
    format!("[{} x i8]", ty.size(layout).unwrap_or(0))
}

/// The `align` attribute that follows a `byval`/`sret` type.
pub(super) fn alignment(ty: Type, layout: Layout) -> String {
    format!("align {}", ty.align(layout).unwrap_or(1).max(1))
}
