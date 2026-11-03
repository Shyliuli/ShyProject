//! Convert ShyC semantic function types into `abi-plan` signatures.
//!
//! `abi-plan` is compiler- and target-independent. This module is the one
//! place that knows both models: it reads functions, records, and arrays from
//! a [`TypeStore`] and asks `abi-plan` for the Shy ABI v1 [`CallPlan`].
//!
//! Lowering plans every function and call here while field layout is still
//! known and stores the plan in the IR (`ir::Function.abi`, `ir::Call`); the
//! backends move bytes according to it. Planning cannot wait for the backends,
//! because `ir::Type::Aggregate` keeps only `(size, align)` and cannot recover
//! the field or element shapes this conversion needs.
//!
//! Top-level qualifiers do not change an ABI, so they are dropped. Incomplete
//! types are rejected rather than given a made-up size.

use abi_plan::{AbiError, AbiField, AbiKind, AbiType, CallPlan, FunctionSignature};

use crate::sema::types::{RecordKind, TypeId, TypeKind, TypeStore};

/// Plan one ShyC function for the target ABI the IR is being built for.
///
/// Lowering calls this while field layout is still available; the backends read
/// the plan back from the IR instead of re-deriving a convention from
/// `(size, align)`, which cannot describe field order.
pub fn plan_target_function(
    types: &TypeStore,
    function_ty: TypeId,
    kind: AbiKind,
) -> Result<CallPlan, AbiError> {
    abi_plan::plan(kind, &function_signature(types, function_ty)?)
}

/// Plan a call, including arguments the signature does not name.
///
/// A variadic call passes more arguments than the prototype describes; those
/// extras are planned from the IR type they lowered to, which keeps their byte
/// layout but loses field order.
pub fn plan_target_call(
    types: &TypeStore,
    function_ty: TypeId,
    extras: &[crate::ir::Type],
    layout: crate::ir::Layout,
    kind: AbiKind,
) -> Result<CallPlan, AbiError> {
    let mut signature = function_signature(types, function_ty)?;
    for extra in extras {
        signature.params.push(ir_abi_type(*extra, layout));
    }
    abi_plan::plan(kind, &signature)
}

fn ir_abi_type(ty: crate::ir::Type, layout: crate::ir::Layout) -> AbiType {
    use crate::ir::Type;
    match ty {
        Type::F32 => AbiType::Float { bits: 32 },
        Type::F64 => AbiType::Float { bits: 64 },
        Type::Ptr => AbiType::Pointer {
            bits: (layout.pointer_bytes * 8) as u16,
        },
        Type::I1 => AbiType::Bool,
        // Field order is gone by now, so an aggregate travels as one opaque
        // object; this only affects aggregates passed through `...`.
        Type::Aggregate { size, align, .. } => AbiType::Struct {
            fields: Vec::new(),
            size,
            align: align as u16,
        },
        other => AbiType::Integer {
            bits: other.integer_bits().unwrap_or(32) as u16,
            signed: false,
        },
    }
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
        TypeKind::Integer { unsigned, .. } => Ok(AbiType::Integer {
            bits: (types.size_of(ty).unwrap_or(4) * 8) as u16,
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
        // The `Result<void, E>` niche is `E`'s integer, not an aggregate.
        TypeKind::PayloadEnum(_) if types.is_niche_enum(ty) => match types.enum_underlying(ty) {
            Some(underlying) => abi_type(types, underlying),
            None => Err(invalid("niche Result has no underlying type")),
        },
        // A payload enum is a visible aggregate in the ABI: a tag followed by
        // one union holding every payload.
        TypeKind::PayloadEnum(_) => payload_enum_type(types, ty),
        TypeKind::Record { kind, .. } => record_type(types, ty, *kind),
        TypeKind::Function { .. } => Err(invalid("function type has no object ABI shape")),
    }
}

/// The transparent `tag + union payload` ABI shape.
///
/// The payload is an [`AbiType::Union`], so it inherits `abi-plan`'s union
/// rule: a union is classified as one opaque integer aggregate. A C compiler
/// merges its members' classes instead, so a float-only payload is not
/// register-compatible with an equivalent C struct (the same limitation
/// already applies to a source-level union member).
fn payload_enum_type(types: &TypeStore, ty: TypeId) -> Result<AbiType, AbiError> {
    let info = types
        .enum_info(ty)
        .ok_or_else(|| invalid("payload enum has no layout"))?;
    let size = types
        .size_of(ty)
        .map_err(|error| invalid(error.to_string()))?;
    let align = types
        .align_of(ty)
        .map_err(|error| invalid(error.to_string()))?;

    let mut variants = Vec::new();
    for variant in &info.variants {
        if let Some(payload) = variant.payload {
            variants.push(AbiField::new(
                variant.name.clone(),
                abi_type(types, payload)?,
                0,
            ));
        }
    }

    Ok(AbiType::Struct {
        fields: vec![
            AbiField::new("tag", abi_type(types, info.underlying)?, 0),
            AbiField::new(
                "payload",
                AbiType::Union {
                    fields: variants,
                    size: info.payload_size,
                    align: info.payload_align as u16,
                },
                info.payload_offset,
            ),
        ],
        size,
        align: align as u16,
    })
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
