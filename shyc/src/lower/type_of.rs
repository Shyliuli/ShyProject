//! Type-only expression analysis.
//!
//! `sizeof`, `_Alignof`, `case` types, and conditional-expression typing need
//! the C type of an expression without evaluating it. This is deliberately
//! separate from lowering: a `sizeof(x++)` must not increment `x`.

use super::const_eval::ConstKind;
use super::{Lowerer, MethodInfo};
use crate::ast;
use crate::sema::types::{Builtin, TypeId, TypeKind};
use crate::source::{Diagnostic, Span};

/// A local-name lookup supplied by the function lowerer.
pub(super) type LocalLookup<'a> = &'a mut dyn FnMut(&str) -> Option<TypeId>;

/// One payload enum variant resolved from `Enum::Variant`.
pub(super) struct ScopedVariant {
    /// The payload enum that owns the variant.
    pub ty: TypeId,
    /// The tag value stored when this variant is active.
    pub tag: i64,
    /// The type stored at the payload offset, or `None` for a plain variant.
    pub payload: Option<TypeId>,
}

/// Which built-in payload enum shape a `?` operand has.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum TryFamily {
    Option,
    Result,
}

impl TryFamily {
    pub(super) fn article(self) -> &'static str {
        match self {
            TryFamily::Option => "an Option",
            TryFamily::Result => "a Result",
        }
    }
}

/// The structural `Option`/`Result` shape of one payload enum.
///
/// The shape is read from the enum's variants, not from its name: `None` plus
/// `Some(payload)` is an Option, and `Err(error)` plus `Ok(payload)` is a
/// Result. A generic instance such as `Option<int>` is an ordinary concrete
/// payload enum by the time it reaches here, so both families work the same.
#[derive(Clone, Copy)]
pub(super) struct TryShape {
    pub family: TryFamily,
    /// The payload type `?` yields. `None` for a payload-free `Ok`.
    pub success: Option<TypeId>,
    /// The error payload stored by the failure variant, if it has one.
    pub failure: Option<TypeId>,
    pub failure_tag: i64,
}

impl Lowerer {
    pub(super) fn type_of(
        &mut self,
        expr: &ast::Expr,
        local: LocalLookup<'_>,
    ) -> Result<TypeId, Diagnostic> {
        match expr {
            ast::Expr::Number(literal) => Ok(match self.number_literal(literal)? {
                super::types::NumericLiteral::Integer { ty, .. } => ty,
                super::types::NumericLiteral::Float { ty, .. } => ty,
            }),
            ast::Expr::Char(literal) => Ok(self.char_literal(literal)?.1),
            ast::Expr::String(group) => self.string_type(group),
            ast::Expr::Name(name) => {
                if let Some(ty) = local(&name.text) {
                    return Ok(ty);
                }
                if let Some((_, ty)) = self.lookup_const(&name.text) {
                    return Ok(ty);
                }
                if self.enum_consts.contains_key(&name.text) {
                    return Ok(self.types.builtin(Builtin::Int));
                }
                match self.global_ref(&name.text) {
                    Some(super::GlobalRef::Object(index)) => Ok(self.object(index).ty),
                    Some(super::GlobalRef::Function(index)) => Ok(self.function(index).ty),
                    None if name.text == "__func__" || name.text == "__FUNCTION__" => {
                        let function = self.current_function_name.clone().unwrap_or_default();
                        let len = function.len() as u32 + 1;
                        let element = self.types.builtin(Builtin::Char);
                        self.types
                            .array_of(
                                crate::sema::types::QualifiedType::unqualified(element),
                                Some(len),
                            )
                            .map_err(|error| {
                                Diagnostic::new(
                                    name.span,
                                    format!("invalid __func__ type: {error}"),
                                )
                            })
                    }
                    None if self.generic_functions.contains_key(&name.text) => {
                        Err(Diagnostic::new(
                            name.span,
                            format!(
                                "generic function '{}' needs explicit generic arguments",
                                name.text
                            ),
                        ))
                    }
                    None => Err(Diagnostic::new(
                        name.span,
                        format!("use of undeclared identifier '{}'", name.text),
                    )),
                }
            }
            ast::Expr::ScopedName {
                qualifier, name, ..
            } => match self.scoped_variant(qualifier, name) {
                Some(variant) => Ok(variant.ty),
                None => Ok(self.scoped_method(qualifier, name)?.ty),
            },
            ast::Expr::GenericScopedName {
                qualifier,
                args,
                name,
                ..
            } => {
                let ty = self.generic_qualifier_type(qualifier, args)?;
                match self.variant_of(ty, name) {
                    Some(variant) => Ok(variant.ty),
                    None => Ok(self.method_on(ty, name)?.ty),
                }
            }
            ast::Expr::BuiltinTypeCall { .. } => Ok(self.types.builtin(Builtin::Int)),
            ast::Expr::Unary { op, operand, span } => {
                let operand_ty = self.type_of(operand, local)?;
                match op {
                    ast::UnaryOp::AddressOf => Ok(self.pointer_to(operand_ty)),
                    ast::UnaryOp::Dereference => {
                        let decayed = self.decay_type(operand_ty);
                        match self.pointee(decayed) {
                            Some(pointee) => Ok(pointee.ty),
                            None => {
                                Err(Diagnostic::new(*span, "unary `*` needs a pointer operand"))
                            }
                        }
                    }
                    ast::UnaryOp::Plus | ast::UnaryOp::Minus | ast::UnaryOp::BitNot => {
                        if self.types.is_arithmetic(operand_ty) {
                            Ok(self.integer_promotion(operand_ty))
                        } else {
                            Err(Diagnostic::new(
                                *span,
                                "unary arithmetic operator needs an arithmetic operand",
                            ))
                        }
                    }
                    ast::UnaryOp::LogicalNot => Ok(self.types.builtin(Builtin::Int)),
                    ast::UnaryOp::PreIncrement | ast::UnaryOp::PreDecrement => Ok(operand_ty),
                }
            }
            ast::Expr::Postfix { operand, .. } => self.type_of(operand, local),
            ast::Expr::Try { value, span } => {
                let value_ty = self.type_of(value, local)?;
                let shape = self.try_shape(value_ty).ok_or_else(|| {
                    Diagnostic::new(
                        *span,
                        "`?` needs an Option (`None`/`Some`) or Result (`Err`/`Ok`) payload enum",
                    )
                })?;
                shape.success.ok_or_else(|| {
                    Diagnostic::new(*span, "`?` needs a success payload; this Result has none")
                })
            }
            ast::Expr::Binary {
                op,
                left,
                right,
                span,
            } => {
                let left_ty = self.type_of(left, local)?;
                let right_ty = self.type_of(right, local)?;
                self.binary_type(*op, left_ty, right_ty, *span)
            }
            ast::Expr::Assignment { left, .. } => self.type_of(left, local),
            ast::Expr::Conditional {
                then_expr,
                else_expr,
                span,
                ..
            } => {
                let then_ty = self.type_of(then_expr, local)?;
                let else_ty = self.type_of(else_expr, local)?;
                self.conditional_type(then_ty, else_ty, then_expr, else_expr, *span)
            }
            ast::Expr::Comma { right, .. } => {
                let ty = self.type_of(right, local)?;
                Ok(self.decay_type(ty))
            }
            ast::Expr::Call {
                callee,
                type_args,
                span,
                ..
            } => {
                // `Enum::Variant(value)` constructs an aggregate; it is not a
                // function call, so it never reaches the signature path.
                if type_args.is_empty()
                    && let ast::Expr::ScopedName {
                        qualifier, name, ..
                    } = callee.as_ref()
                    && let Some(variant) = self.scoped_variant(qualifier, name)
                {
                    return Ok(variant.ty);
                }
                // `Option<int>::Some(value)` without call-site type arguments
                // is a payload construction, not a call.
                if type_args.is_empty()
                    && let ast::Expr::GenericScopedName {
                        qualifier,
                        args,
                        name,
                        ..
                    } = callee.as_ref()
                    && let Some(variant) = {
                        let ty = self.generic_qualifier_type(qualifier, args)?;
                        self.variant_of(ty, name)
                    }
                {
                    return Ok(variant.ty);
                }
                // Type-only analysis instantiates through the same cache as
                // lowering, so `sizeof(identity<int>(1))` names the instance
                // the call will later use.
                let function_ty = if type_args.is_empty() {
                    self.callee_function_type(callee, local)?
                } else {
                    self.generic_call_target(callee, type_args, *span)?.ty
                };
                match self.types.kind(function_ty) {
                    TypeKind::Function { result, .. } => Ok(*result),
                    _ => Err(Diagnostic::new(
                        expr.span(),
                        "called object is not a function",
                    )),
                }
            }
            ast::Expr::Index { array, index, span } => {
                let left_ty = self.type_of(array, local)?;
                if self.types.is_array(left_ty) || self.types.is_pointer(left_ty) {
                    return self.index_type(left_ty, *span);
                }
                let right_ty = self.type_of(index, local)?;
                self.index_type(right_ty, *span)
            }
            ast::Expr::Member {
                object,
                name,
                op,
                span,
            } => self.member_type(object, name, *op, *span, local),
            ast::Expr::Cast { type_name, .. } => self.resolve_type_name(type_name),
            ast::Expr::SizeofType { .. }
            | ast::Expr::SizeofExpr { .. }
            | ast::Expr::AlignofType { .. }
            | ast::Expr::AlignofExpr { .. } => Ok(self.types.builtin(Builtin::UnsignedInt)),
            ast::Expr::CompoundLiteral { type_name, .. } => self.resolve_type_name(type_name),
            ast::Expr::StatementExpr { block, .. } => match tail_expr(block) {
                Some(expr) => {
                    // A statement expression declares its locals inside its
                    // own block, and those declarations exist only in the AST
                    // until the block is lowered. When the surrounding context
                    // needs the type before lowering (`?:`, `sizeof`), read
                    // the declarations straight from the block so `({ T x;
                    // ...; x; })` still types as `T`.
                    let mut declared: Vec<(String, TypeId)> = Vec::new();
                    for statement in &block.stmts {
                        let ast::Stmt::Decl(declaration) = statement else {
                            continue;
                        };
                        if declaration.specs.storage == Some(ast::StorageClass::Typedef) {
                            continue;
                        }
                        let specs = self.resolve_specs(&declaration.specs)?;
                        for item in &declaration.declarators {
                            let resolved = self.resolve_declarator(
                                specs.ty,
                                specs.qualifiers,
                                &item.declarator,
                            )?;
                            if let Some(name) = &item.declarator.name {
                                declared.push((name.text.clone(), resolved.ty));
                            }
                        }
                    }
                    let mut scoped = |name: &str| -> Option<TypeId> {
                        declared
                            .iter()
                            .rev()
                            .find(|(declared, _)| declared == name)
                            .map(|(_, ty)| *ty)
                            .or_else(|| local(name))
                    };
                    let ty = self.type_of(expr, &mut scoped)?;
                    Ok(self.decay_type(ty))
                }
                None => Ok(self.types.builtin(Builtin::Void)),
            },
        }
    }

    pub(super) fn conditional_type(
        &mut self,
        then_ty: TypeId,
        else_ty: TypeId,
        then_expr: &ast::Expr,
        else_expr: &ast::Expr,
        span: Span,
    ) -> Result<TypeId, Diagnostic> {
        let then_ty = self.decay_type(then_ty);
        let else_ty = self.decay_type(else_ty);
        if self.types.is_void(then_ty) && self.types.is_void(else_ty) {
            return Ok(then_ty);
        }
        if self.types.is_arithmetic(then_ty) && self.types.is_arithmetic(else_ty) {
            return self.usual_arithmetic(then_ty, else_ty, span);
        }
        if self.types.is_pointer(then_ty) && self.types.is_pointer(else_ty) {
            let then_void = self
                .pointee(then_ty)
                .is_some_and(|pointee| self.types.is_void(pointee.ty));
            return Ok(if then_void { then_ty } else { else_ty });
        }
        if self.types.is_pointer(then_ty) && self.is_null_pointer_constant(else_expr) {
            return Ok(then_ty);
        }
        if self.types.is_pointer(else_ty) && self.is_null_pointer_constant(then_expr) {
            return Ok(else_ty);
        }
        if self.types.is_compatible(then_ty, else_ty) {
            return Ok(then_ty);
        }
        Err(Diagnostic::new(
            span,
            "incompatible operand types in conditional expression",
        ))
    }

    pub(super) fn is_null_pointer_constant(&mut self, expr: &ast::Expr) -> bool {
        let Ok(value) = self.eval_const(expr) else {
            return false;
        };
        self.types.is_integer(value.ty) && matches!(value.kind, ConstKind::Int(0))
    }

    fn binary_type(
        &mut self,
        op: ast::BinaryOp,
        left_ty: TypeId,
        right_ty: TypeId,
        span: Span,
    ) -> Result<TypeId, Diagnostic> {
        let left_ty = self.decay_type(left_ty);
        let right_ty = self.decay_type(right_ty);
        match op {
            ast::BinaryOp::LogicalAnd | ast::BinaryOp::LogicalOr => {
                Ok(self.types.builtin(Builtin::Int))
            }
            ast::BinaryOp::Equal
            | ast::BinaryOp::NotEqual
            | ast::BinaryOp::Less
            | ast::BinaryOp::LessEqual
            | ast::BinaryOp::Greater
            | ast::BinaryOp::GreaterEqual => Ok(self.types.builtin(Builtin::Int)),
            ast::BinaryOp::ShiftLeft | ast::BinaryOp::ShiftRight => {
                if self.types.is_integer(left_ty) && self.types.is_integer(right_ty) {
                    Ok(self.integer_promotion(left_ty))
                } else {
                    Err(Diagnostic::new(span, "shift needs integer operands"))
                }
            }
            ast::BinaryOp::Add => {
                if self.types.is_pointer(left_ty) && self.types.is_integer(right_ty) {
                    Ok(left_ty)
                } else if self.types.is_integer(left_ty) && self.types.is_pointer(right_ty) {
                    Ok(right_ty)
                } else {
                    self.usual_arithmetic(left_ty, right_ty, span)
                }
            }
            ast::BinaryOp::Subtract => {
                if self.types.is_pointer(left_ty) && self.types.is_integer(right_ty) {
                    Ok(left_ty)
                } else if self.types.is_pointer(left_ty) && self.types.is_pointer(right_ty) {
                    Ok(self.types.builtin(Builtin::Int))
                } else {
                    self.usual_arithmetic(left_ty, right_ty, span)
                }
            }
            _ => {
                if !self.types.is_arithmetic(left_ty) || !self.types.is_arithmetic(right_ty) {
                    return Err(Diagnostic::new(span, "operator needs arithmetic operands"));
                }
                if op == ast::BinaryOp::Remainder
                    && (self.types.is_floating(left_ty) || self.types.is_floating(right_ty))
                {
                    return Err(Diagnostic::new(
                        span,
                        "remainder `%` needs integer operands",
                    ));
                }
                self.usual_arithmetic(left_ty, right_ty, span)
            }
        }
    }

    fn index_type(&mut self, array_ty: TypeId, span: Span) -> Result<TypeId, Diagnostic> {
        if let Some(element) = self.array_element(array_ty) {
            return Ok(element.ty);
        }
        if let Some(pointee) = self.pointee(array_ty) {
            return Ok(pointee.ty);
        }
        Err(Diagnostic::new(
            span,
            "subscripted value is not an array or pointer",
        ))
    }

    fn member_type(
        &mut self,
        object: &ast::Expr,
        name: &ast::Name,
        op: ast::MemberOp,
        span: Span,
        local: LocalLookup<'_>,
    ) -> Result<TypeId, Diagnostic> {
        // `m.payload.Variant` is the only way to name a payload; the inner
        // `m.payload` has no type of its own.
        if let ast::Expr::Member {
            object: base,
            name: payload,
            ..
        } = object
            && payload.text == "payload"
        {
            let base_ty = self.type_of(base, &mut *local)?;
            if let Some(enum_ty) = self.payload_enum_target(base_ty) {
                let variant = self
                    .types
                    .enum_variant(enum_ty, &name.text)
                    .ok_or_else(|| {
                        Diagnostic::new(
                            span,
                            format!("payload enum has no variant '{}'", name.text),
                        )
                    })?;
                return match variant.payload {
                    Some(payload) => Ok(payload),
                    None => Err(Diagnostic::new(
                        span,
                        format!("variant '{}' has no payload", name.text),
                    )),
                };
            }
        }
        let object_ty = self.type_of(object, local)?;
        if let Some(enum_ty) = self.payload_enum_target(object_ty) {
            return match name.text.as_str() {
                "tag" => self
                    .types
                    .enum_underlying(enum_ty)
                    .ok_or_else(|| Diagnostic::new(span, "payload enum has no underlying type")),
                "payload" => Err(Diagnostic::new(
                    span,
                    "a payload is read through a variant: write 'value.payload.Variant'",
                )),
                other => Err(Diagnostic::new(
                    span,
                    format!(
                        "payload enum has no field '{other}'; use '.tag' or '.payload.<Variant>'"
                    ),
                )),
            };
        }
        if op == ast::MemberOp::Arrow && !self.types.is_pointer(object_ty) {
            return Err(Diagnostic::new(
                span,
                "`->` needs a pointer to a struct or union",
            ));
        }
        let record = self
            .record_of(object_ty)
            .ok_or_else(|| Diagnostic::new(span, "member access needs a struct or union"))?;
        match self.field_of(record, &name.text, name.span) {
            Ok((ty, _)) => Ok(ty),
            Err(error) => {
                if self.method_of(record, &name.text).is_some() {
                    Err(Diagnostic::new(
                        span,
                        format!("method '{}' must be called", name.text),
                    ))
                } else {
                    Err(error)
                }
            }
        }
    }

    pub(super) fn scoped_method(
        &self,
        qualifier: &ast::Name,
        name: &ast::Name,
    ) -> Result<MethodInfo, Diagnostic> {
        let record = self.lookup_named_type(&qualifier.text).ok_or_else(|| {
            Diagnostic::new(
                qualifier.span,
                format!("unknown type name '{}'", qualifier.text),
            )
        })?;
        self.method_on(record, name)
    }

    /// The method `name` of an already resolved type.
    pub(super) fn method_on(&self, ty: TypeId, name: &ast::Name) -> Result<MethodInfo, Diagnostic> {
        self.method_of(ty, &name.text).ok_or_else(|| {
            Diagnostic::new(name.span, format!("no method '{}' on this type", name.text))
        })
    }

    /// Resolve `Type::Variant` when `qualifier` names a payload enum.
    ///
    /// `None` leaves the name to method resolution or to a diagnostic.
    pub(super) fn scoped_variant(
        &self,
        qualifier: &ast::Name,
        name: &ast::Name,
    ) -> Option<ScopedVariant> {
        let ty = self.lookup_named_type(&qualifier.text)?;
        self.variant_of(ty, name)
    }

    /// The payload enum variant called `name`, for an already resolved type.
    pub(super) fn variant_of(&self, ty: TypeId, name: &ast::Name) -> Option<ScopedVariant> {
        let variant = self.types.enum_variant(ty, &name.text)?;
        Some(ScopedVariant {
            ty,
            tag: variant.value,
            payload: variant.payload,
        })
    }

    /// Read the structural `Option` or `Result` shape of a payload enum.
    ///
    /// Exactly two variants are required, so an unrelated enum that happens to
    /// use one of the names is not misread as a propagation shape.
    pub(super) fn try_shape(&self, ty: TypeId) -> Option<TryShape> {
        if !self.types.is_payload_enum(ty) {
            return None;
        }
        let info = self.types.enum_info(ty)?;
        if info.variants.len() != 2 {
            return None;
        }
        if let Some(none) = info
            .variants
            .iter()
            .find(|variant| variant.name == "None" && variant.payload.is_none())
            && let Some(some) = info
                .variants
                .iter()
                .find(|variant| variant.name == "Some" && variant.payload.is_some())
        {
            return Some(TryShape {
                family: TryFamily::Option,
                success: some.payload,
                failure: none.payload,
                failure_tag: none.value,
            });
        }
        let error = info.variants.iter().find(|variant| variant.name == "Err")?;
        let ok = info.variants.iter().find(|variant| variant.name == "Ok")?;
        Some(TryShape {
            family: TryFamily::Result,
            success: ok.payload,
            failure: error.payload,
            failure_tag: error.value,
        })
    }

    /// The payload enum an object of `ty` denotes, through any pointer depth.
    ///
    /// ShyC dereferences dot access automatically, so `m.tag` on a `Msg *` or
    /// `Msg **` reaches the same object as an explicit dereference would.
    pub(super) fn payload_enum_target(&self, mut ty: TypeId) -> Option<TypeId> {
        loop {
            if self.types.is_payload_enum(ty) {
                return Some(ty);
            }
            let TypeKind::Pointer { pointee } = self.types.kind(ty) else {
                return None;
            };
            ty = pointee.ty;
        }
    }

    pub(super) fn callee_function_type(
        &mut self,
        callee: &ast::Expr,
        local: LocalLookup<'_>,
    ) -> Result<TypeId, Diagnostic> {
        if let ast::Expr::Member {
            object, name, op, ..
        } = callee
        {
            let object_ty = self.type_of(object, local)?;
            if let Some(record) = self.record_of(object_ty)
                && let Some(method) = self.method_of(record, &name.text)
            {
                return Ok(method.ty);
            }
            let _ = op;
        }
        if let ast::Expr::ScopedName {
            qualifier, name, ..
        } = callee
        {
            return Ok(self.scoped_method(qualifier, name)?.ty);
        }
        if let ast::Expr::GenericScopedName {
            qualifier,
            args,
            name,
            ..
        } = callee
        {
            let ty = self.generic_qualifier_type(qualifier, args)?;
            return Ok(self.method_on(ty, name)?.ty);
        }
        let ty = self.type_of(callee, local)?;
        if self.types.is_function(ty) {
            return Ok(ty);
        }
        let decayed = self.decay_type(ty);
        if let Some(pointee) = self.pointee(decayed)
            && self.types.is_function(pointee.ty)
        {
            return Ok(pointee.ty);
        }
        Err(Diagnostic::new(
            callee.span(),
            "called object is not a function",
        ))
    }
}

/// The expression that a GNU statement expression yields, if it has one.
pub(super) fn tail_expr(block: &ast::Block) -> Option<&ast::Expr> {
    match block.stmts.last()? {
        ast::Stmt::Expr { expr, .. } => Some(expr),
        ast::Stmt::Block(block) => tail_expr(block),
        _ => None,
    }
}
