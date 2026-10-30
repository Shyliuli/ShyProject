//! Convert ShyC semantic function types into `abi-plan` signatures.
//!
//! `abi-plan` is compiler- and target-independent. This module is the one
//! place that knows both models: it reads functions, records, and arrays from
//! a [`TypeStore`] and asks `abi-plan` for the Shy ABI v1 [`CallPlan`].
//!
//! No backend consumes the plan yet. The exact future hook is
//! `lower/call.rs::emit_call`: it already has the sema function `TypeId` and
//! `self.l.types`, so it can call [`plan_shy_function`] and attach the result
//! to `ir::Call` (that would add a field in `ir/ops.rs`). A backend then moves
//! bytes according to `plan.args` and `plan.return_value` instead of assuming
//! a host calling convention. Planning must happen before lowering, because
//! `ir::Type::Aggregate` keeps only `(size, align)` and cannot recover the
//! field or element shapes this conversion needs.
//!
//! Top-level qualifiers do not change an ABI, so they are dropped. Incomplete
//! types are rejected rather than given a made-up size.

use abi_plan::{AbiError, AbiField, AbiKind, AbiType, CallPlan, FunctionSignature};

use crate::sema::types::{RecordKind, TypeId, TypeKind, TypeStore};

/// Plan an already-converted signature under the Shy ABI v1.
///
/// This is the narrow surface for callers that do not have a ShyC
/// [`TypeStore`], for example a simulator or a hand-built declaration.
pub fn plan_shy(signature: &FunctionSignature) -> Result<CallPlan, AbiError> {
    abi_plan::plan(AbiKind::ShyV1, signature)
}

/// Plan one ShyC function under the Shy ABI v1.
///
/// `function_ty` must be a [`TypeKind::Function`] from `types`. Aggregate
/// returns become an sret plan, aggregates and arguments beyond the eighth
/// register slot go on the stack, and a scalar wider than 64 bits is reported
/// as [`AbiError::Unsupported`].
pub fn plan_shy_function(types: &TypeStore, function_ty: TypeId) -> Result<CallPlan, AbiError> {
    plan_shy(&function_signature(types, function_ty)?)
}

/// Convert one ShyC function type without classifying it.
pub fn function_signature(
    types: &TypeStore,
    function_ty: TypeId,
) -> Result<FunctionSignature, AbiError> {
    let TypeKind::Function {
        result,
        params,
        variadic,
        ..
    } = types.kind(function_ty)
    else {
        return Err(invalid("expected a function type"));
    };
    Ok(FunctionSignature {
        ret: abi_type(types, *result)?,
        params: params
            .iter()
            .map(|&param| abi_type(types, param))
            .collect::<Result<_, _>>()?,
        variadic: *variadic,
    })
}

/// Convert one source type to its ABI shape.
pub fn abi_type(types: &TypeStore, ty: TypeId) -> Result<AbiType, AbiError> {
    match types.kind(ty) {
        TypeKind::Void => Ok(AbiType::Void),
        TypeKind::Bool => Ok(AbiType::Bool),
        TypeKind::Integer { rank, unsigned } => Ok(AbiType::Integer {
            bits: (rank.size() * 8) as u16,
            signed: !unsigned,
        }),
        TypeKind::Float32 => Ok(AbiType::Float { bits: 32 }),
        TypeKind::Float64 => Ok(AbiType::Float { bits: 64 }),
        TypeKind::Pointer { .. } => Ok(AbiType::Pointer {
            bits: (types.pointer_width() * 8) as u16,
        }),
        TypeKind::Array {
            element,
            len: Some(count),
        } => Ok(AbiType::Array {
            element: Box::new(abi_type(types, element.ty)?),
            count: *count,
        }),
        TypeKind::Array { len: None, .. } => Err(invalid("incomplete array type has no ABI shape")),
        TypeKind::Enum(_) => match types.enum_underlying(ty) {
            Some(underlying) => abi_type(types, underlying),
            None => Err(invalid("enum type has no underlying type")),
        },
        TypeKind::Record { kind, .. } => record_type(types, ty, *kind),
        TypeKind::Function { .. } => Err(invalid("function type has no object ABI shape")),
    }
}

fn record_type(types: &TypeStore, ty: TypeId, kind: RecordKind) -> Result<AbiType, AbiError> {
    let info = types
        .record_info(ty)
        .ok_or_else(|| invalid("record type has no layout"))?;
    let size = types
        .size_of(ty)
        .map_err(|error| invalid(error.to_string()))?;
    let align = types
        .align_of(ty)
        .map_err(|error| invalid(error.to_string()))?;

    let mut fields = Vec::with_capacity(info.fields.len());
    for field in &info.fields {
        let field_ty = match types.kind(field.ty.ty) {
            // A flexible array member occupies no bytes of its own; its
            // element alignment is already part of the record layout.
            TypeKind::Array { element, len: None } => {
                let align = types
                    .align_of(element.ty)
                    .map_err(|error| invalid(error.to_string()))?;
                AbiType::Opaque {
                    size: 0,
                    align: align as u16,
                }
            }
            _ => abi_type(types, field.ty.ty)?,
        };
        fields.push(AbiField {
            name: field.name.clone(),
            ty: field_ty,
            offset: field.offset,
        });
    }

    Ok(match kind {
        RecordKind::Struct => AbiType::Struct {
            fields,
            size,
            align: align as u16,
        },
        RecordKind::Union => AbiType::Union {
            fields,
            size,
            align: align as u16,
        },
    })
}

fn invalid(message: impl Into<String>) -> AbiError {
    AbiError::InvalidType(message.into())
}
