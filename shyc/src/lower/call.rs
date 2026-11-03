//! Member access, subscripts, and calls (including Shy method calls).
//!
//! Dot access auto-dereferences one pointer level, matching Shy. Instance
//! methods are ordinary functions whose first parameter is the receiver; the
//! call passes the receiver explicitly and targets the mangled symbol
//! (`____Type__method` for an inherent method, with the trait name in front
//! for a trait method). An inherent method wins over a trait method, and two
//! matching trait methods make a receiver call ambiguous.

use super::{ExprKind, ExprVal, FnLower, FunctionInstance};
use crate::ast;
use crate::ir;
use crate::sema::types::{Qualifiers, TypeId, TypeKind};
use crate::source::{Diagnostic, Span};

pub(super) struct MemberTarget {
    pub address: ir::Address,
    pub ty: TypeId,
    pub qualifiers: Qualifiers,
}

impl FnLower<'_> {
    pub(super) fn lower_member(
        &mut self,
        object: &ast::Expr,
        name: &ast::Name,
        op: ast::MemberOp,
        span: Span,
    ) -> Result<ExprVal, Diagnostic> {
        self.lower_member_mode(object, name, op, span, false)
    }

    pub(super) fn lower_member_mode(
        &mut self,
        object: &ast::Expr,
        name: &ast::Name,
        op: ast::MemberOp,
        span: Span,
        mutable: bool,
    ) -> Result<ExprVal, Diagnostic> {
        if let Some(value) = self.lower_payload_enum_member(object, name, span)? {
            return Ok(value);
        }
        let object_ty = self.type_of(object)?;
        let record = if self.l.types.is_record(object_ty) {
            Some(object_ty)
        } else {
            self.l
                .pointee(object_ty)
                .map(|pointee| pointee.ty)
                .filter(|ty| self.l.types.is_record(*ty))
        };
        let object_value = if mutable {
            self.lower_expr_for_write(object)?
        } else {
            self.lower_expr(object)?
        };
        if let Some(record) = record
            && let Some((field_ty, offset, width, bit_offset)) =
                self.l.bit_field(record, &name.text)
        {
            let (_, base, _) = self.record_and_address(object_value, span)?;
            let address = base.offset_by(i64::from(offset));
            let value = self.bit_field_value(address, field_ty, width, bit_offset, span);
            return Ok(ExprVal {
                ty: field_ty,
                qualifiers: Qualifiers::NONE,
                kind: ExprKind::Value(value),
            });
        }
        let target = self.member_target(object_value, name, op, span, mutable)?;
        Ok(ExprVal {
            ty: target.ty,
            qualifiers: target.qualifiers,
            kind: ExprKind::Place(target.address),
        })
    }

    /// Lower `m.tag` and `m.payload.Variant` for a payload enum.
    ///
    /// The representation is transparent: the tag is the underlying integer
    /// stored at offset 0, and every payload starts at the enum's payload
    /// offset. `Ok(None)` means the object is not a payload enum, so ordinary
    /// member access continues unchanged.
    fn lower_payload_enum_member(
        &mut self,
        object: &ast::Expr,
        name: &ast::Name,
        span: Span,
    ) -> Result<Option<ExprVal>, Diagnostic> {
        // `m.payload.Variant`: the inner `m.payload` is not an object of its
        // own, so the object type is the one behind that pair.
        if let ast::Expr::Member {
            object: base,
            name: payload,
            ..
        } = object
            && payload.text == "payload"
        {
            let base_ty = self.type_of(base)?;
            if let Some(enum_ty) = self.l.payload_enum_target(base_ty) {
                let value = self.lower_expr(base)?;
                let (address, qualifiers) = self.payload_enum_address(value, span)?;
                let variant = self
                    .l
                    .types
                    .enum_variant(enum_ty, &name.text)
                    .ok_or_else(|| {
                        Diagnostic::new(
                            name.span,
                            format!("payload enum has no variant '{}'", name.text),
                        )
                    })?;
                let Some(payload_ty) = variant.payload else {
                    return Err(Diagnostic::new(
                        name.span,
                        format!("variant '{}' has no payload", name.text),
                    ));
                };
                let payload_offset = self
                    .l
                    .types
                    .enum_info(enum_ty)
                    .map(|info| info.payload_offset)
                    .unwrap_or(0);
                return Ok(Some(ExprVal {
                    ty: payload_ty,
                    qualifiers,
                    kind: ExprKind::Place(address.offset_by(i64::from(payload_offset))),
                }));
            }
        }

        let object_ty = self.type_of(object)?;
        let Some(enum_ty) = self.l.payload_enum_target(object_ty) else {
            return Ok(None);
        };
        match name.text.as_str() {
            "tag" => {
                let value = self.lower_expr(object)?;
                let (address, qualifiers) = self.payload_enum_address(value, span)?;
                let Some(underlying) = self.l.types.enum_underlying(enum_ty) else {
                    return Err(Diagnostic::new(span, "payload enum has no underlying type"));
                };
                Ok(Some(ExprVal {
                    ty: underlying,
                    qualifiers,
                    kind: ExprKind::Place(address),
                }))
            }
            "payload" => Err(Diagnostic::new(
                span,
                "a payload is read through a variant: write 'value.payload.Variant'",
            )),
            other => Err(Diagnostic::new(
                span,
                format!("payload enum has no field '{other}'; use '.tag' or '.payload.<Variant>'"),
            )),
        }
    }

    /// The address of a payload enum object, dereferencing any pointer depth
    /// for dot access. The returned qualifiers are the object's own, so a
    /// volatile payload enum keeps volatile loads and stores.
    fn payload_enum_address(
        &mut self,
        mut value: ExprVal,
        span: Span,
    ) -> Result<(ir::Address, Qualifiers), Diagnostic> {
        if self.l.types.is_payload_enum(value.ty) {
            let qualifiers = value.qualifiers;
            return Ok((self.object_address(value, span)?, qualifiers));
        }
        while let Some(pointee) = self.l.pointee(value.ty) {
            let (operand, _) = self.rvalue_val(value, span)?;
            let address = super::address_operand(operand);
            if self.l.types.is_payload_enum(pointee.ty) {
                return Ok((address, pointee.qualifiers));
            }
            // Still a pointer: load it and keep walking.
            let loaded = self.load_value(address, pointee.ty, pointee.qualifiers);
            value = ExprVal {
                ty: pointee.ty,
                qualifiers: pointee.qualifiers,
                kind: ExprKind::Value(loaded),
            };
        }
        Err(Diagnostic::new(span, "member access needs a payload enum"))
    }

    /// Build a payload enum value: zero the whole object, store the tag, then
    /// store the payload at its layout offset.
    pub(super) fn construct_payload_enum(
        &mut self,
        qualifier: &ast::Name,
        variant_name: &ast::Name,
        variant: super::type_of::ScopedVariant,
        args: &[ast::Expr],
        span: Span,
    ) -> Result<ExprVal, Diagnostic> {
        let super::type_of::ScopedVariant { ty, tag, payload } = variant;
        match (payload, args.len()) {
            (None, 0) | (Some(_), 1) => {}
            (None, _) => {
                return Err(Diagnostic::new(
                    span,
                    format!(
                        "variant '{}::{}' takes no payload",
                        qualifier.text, variant_name.text
                    ),
                ));
            }
            (Some(_), 0) => {
                return Err(Diagnostic::new(
                    span,
                    format!(
                        "variant '{}::{}' needs a payload: write '{}::{}(value)'",
                        qualifier.text, variant_name.text, qualifier.text, variant_name.text
                    ),
                ));
            }
            (Some(_), _) => {
                return Err(Diagnostic::new(
                    span,
                    format!(
                        "variant '{}::{}' takes exactly one payload value",
                        qualifier.text, variant_name.text
                    ),
                ));
            }
        }

        let payload = match payload {
            Some(payload_ty) => {
                let (operand, argument_ty) = self.rvalue(&args[0])?;
                if self.l.types.is_aggregate(payload_ty) {
                    if !self.l.types.is_compatible(payload_ty, argument_ty) {
                        return Err(Diagnostic::new(
                            args[0].span(),
                            format!(
                                "payload of variant '{}::{}' has an incompatible type",
                                qualifier.text, variant_name.text
                            ),
                        ));
                    }
                    Some((operand, payload_ty))
                } else {
                    let operand =
                        self.convert(operand, argument_ty, payload_ty, args[0].span(), false)?;
                    Some((operand, payload_ty))
                }
            }
            None => None,
        };
        self.build_payload_enum(ty, tag, payload, span)
    }

    /// Emit a payload enum object from an already lowered payload operand.
    ///
    /// The payload type must be the variant's own; constructors convert the
    /// source value first, and propagation only reaches here when the source
    /// and destination error payloads are compatible.
    pub(super) fn build_payload_enum(
        &mut self,
        ty: TypeId,
        tag: i64,
        payload: Option<(ir::Operand, TypeId)>,
        span: Span,
    ) -> Result<ExprVal, Diagnostic> {
        let size = self.l.size_of(ty, span)?;
        let slot = self.temp_slot(ty, "");
        self.emit_statement(ir::InstKind::MemSet {
            dst: ir::Address::slot(slot),
            byte: 0,
            bytes: size,
        });
        let Some(underlying) = self.l.types.enum_underlying(ty) else {
            return Err(Diagnostic::new(span, "payload enum has no underlying type"));
        };
        let tag_operand = ir::Operand::Constant(ir::Constant::int(self.l.ir_type(underlying), tag));
        self.store_value(
            ir::Address::slot(slot),
            tag_operand,
            underlying,
            Qualifiers::NONE,
        );
        if let Some((operand, payload_ty)) = payload {
            let payload_offset = self
                .l
                .types
                .enum_info(ty)
                .map(|info| info.payload_offset)
                .unwrap_or(0);
            let address = ir::Address::slot(slot).offset_by(i64::from(payload_offset));
            self.store_value(address, operand, payload_ty, Qualifiers::NONE);
        }
        Ok(ExprVal {
            ty,
            qualifiers: Qualifiers::NONE,
            kind: ExprKind::Place(ir::Address::slot(slot)),
        })
    }

    pub(super) fn lower_index(
        &mut self,
        array: &ast::Expr,
        index: &ast::Expr,
        span: Span,
    ) -> Result<ExprVal, Diagnostic> {
        let first = self.lower_expr(array)?;
        let first_ty = first.ty;
        let (base_value, index_expr) =
            if self.l.types.is_array(first_ty) || self.l.types.is_pointer(first_ty) {
                (first, index)
            } else {
                let second = self.lower_expr(index)?;
                if !self.l.types.is_array(second.ty) && !self.l.types.is_pointer(second.ty) {
                    return Err(Diagnostic::new(
                        span,
                        "subscripted value is not an array or pointer",
                    ));
                }
                (second, array)
            };
        let element = if let Some(element) = self.l.array_element(base_value.ty) {
            element
        } else if let Some(pointee) = self.l.pointee(base_value.ty) {
            pointee
        } else {
            return Err(Diagnostic::new(
                span,
                "subscripted value is not an array or pointer",
            ));
        };
        let base_address = if self.l.types.is_array(base_value.ty) {
            match base_value.kind {
                ExprKind::Place(address) => address,
                _ => {
                    let (operand, _) = self.rvalue_val(base_value, span)?;
                    super::address_operand(operand)
                }
            }
        } else {
            let (operand, _) = self.rvalue_val(base_value, span)?;
            super::address_operand(operand)
        };
        let (index_operand, index_ty) = self.rvalue(index_expr)?;
        let size = self.l.size_of(element.ty, span)?;
        if let ir::Operand::Constant(constant) = index_operand {
            let value = if self.l.is_unsigned(index_ty) {
                constant.bits as i64
            } else {
                constant.as_i64()
            };
            let byte_offset = value
                .checked_mul(i64::from(size))
                .ok_or_else(|| Diagnostic::new(span, "subscript offset overflow"))?;
            let address = base_address.offset_by(byte_offset);
            return Ok(ExprVal {
                ty: element.ty,
                qualifiers: element.qualifiers,
                kind: ExprKind::Place(address),
            });
        }
        let int_ty = self.int_type();
        let index_operand = self.convert(index_operand, index_ty, int_ty, span, false)?;
        let offset = if size == 1 {
            index_operand
        } else {
            let scale = ir::Operand::Constant(ir::Constant::int(ir::Type::I32, i64::from(size)));
            ir::Operand::Value(self.emit_value(ir::InstKind::Binary {
                op: ir::BinaryOp::Mul,
                lhs: index_operand,
                rhs: scale,
                ty: ir::Type::I32,
            }))
        };
        let base = self.address_value(base_address);
        let pointer = self.emit_value(ir::InstKind::PtrOffset { base, offset });
        Ok(ExprVal {
            ty: element.ty,
            qualifiers: element.qualifiers,
            kind: ExprKind::Place(super::address_operand(ir::Operand::Value(pointer))),
        })
    }

    pub(super) fn lower_call(
        &mut self,
        callee: &ast::Expr,
        type_args: &[ast::GenericArg],
        args: &[ast::Expr],
        span: Span,
    ) -> Result<ExprVal, Diagnostic> {
        // Explicit generic arguments name one concrete instance of a
        // template, so the callee is never lowered as a value: the instance
        // has its own direct symbol.
        if !type_args.is_empty() {
            // `Trait::from<Type>()` is the explicit construction of a trait
            // value, not a generic function call.
            if let ast::Expr::ScopedName {
                qualifier, name, ..
            } = callee
                && name.text == "from"
                && self.l.is_trait_value_name(&qualifier.text)
            {
                if !args.is_empty() {
                    return Err(Diagnostic::new(
                        span,
                        format!(
                            "trait value construction '{}::from<Type>()' takes no arguments",
                            qualifier.text
                        ),
                    ));
                }
                return self.lower_trait_table(qualifier, type_args, span);
            }
            let instance = self.generic_callee_instance(callee, type_args, span)?;
            if let ast::Expr::Member {
                object, name, op, ..
            } = callee
            {
                return self.lower_generic_method_call(object, name, *op, instance, args, span);
            }
            return self.lower_instance_call(instance, args, span);
        }
        // A template name with no written type arguments infers them from the
        // value arguments, then calls the one instance those arguments name. A
        // local binding of the same name keeps shadowing the template, as it
        // does for any ordinary lookup.
        if let ast::Expr::Name(name) = callee
            && !self.has_local_binding(&name.text)
            && self.l.is_inferred_generic_call(&name.text)
        {
            let instance = self.infer_generic_call(name, args)?;
            return self.lower_instance_call(instance, args, span);
        }
        if let ast::Expr::Member {
            object, name, op, ..
        } = callee
        {
            let object_ty = self.type_of(object)?;
            if let Some(resolved) = self.l.method_with_autoderef(object_ty, name)? {
                let mut receiver = self.lower_expr(object)?;
                for (index, step) in resolved.steps.iter().enumerate() {
                    let step_name = ast::Name::new(step.name, name.span);
                    let step_op = if index == 0 { *op } else { ast::MemberOp::Dot };
                    receiver = self.lower_method_call_value(
                        receiver,
                        None,
                        &step_name,
                        step_op,
                        step.method.clone(),
                        &[],
                        span,
                    )?;
                }
                let method_op = if resolved.steps.is_empty() {
                    *op
                } else {
                    ast::MemberOp::Dot
                };
                return self.lower_method_call_value(
                    receiver,
                    Some(object),
                    name,
                    method_op,
                    resolved.method,
                    args,
                    span,
                );
            }
        }
        // A call with no written arguments instantiates a generic method from
        // the value arguments, exactly as `name(value)` does for a generic
        // function. A concrete method of the same name already returned above.
        if let Some(instance) = self.inferred_method_instance(callee, args)? {
            if let ast::Expr::Member {
                object, name, op, ..
            } = callee
            {
                return self.lower_generic_method_call(object, name, *op, instance, args, span);
            }
            return self.lower_instance_call(instance, args, span);
        }
        if let ast::Expr::ScopedName {
            qualifier,
            name,
            span: call_span,
        } = callee
        {
            if let Some(variant) = self.l.scoped_variant(qualifier, name) {
                return self.construct_payload_enum(qualifier, name, variant, args, *call_span);
            }
            let method = self.l.scoped_method(qualifier, name)?;
            let (params, variadic) = self.function_params(method.ty, span)?;
            let operands = self.lower_arguments(args, &params, variadic, span)?;
            return self.emit_call(
                ir::CallTarget::Direct(method.symbol),
                method.ty,
                operands,
                span,
            );
        }
        if let ast::Expr::GenericScopedName {
            qualifier,
            args: generic_args,
            name,
            span: call_span,
        } = callee
        {
            let ty = self.l.generic_qualifier_type(qualifier, generic_args)?;
            if let Some(variant) = self.l.variant_of(ty, name) {
                return self.construct_payload_enum(qualifier, name, variant, args, *call_span);
            }
            let method = self.l.method_on(ty, name)?;
            let (params, variadic) = self.function_params(method.ty, span)?;
            let operands = self.lower_arguments(args, &params, variadic, span)?;
            return self.emit_call(
                ir::CallTarget::Direct(method.symbol),
                method.ty,
                operands,
                span,
            );
        }

        let value = self.lower_expr(callee)?;
        let function_ty = if self.l.types.is_function(value.ty) {
            value.ty
        } else {
            let decayed = self.l.decay_type(value.ty);
            match self.l.pointee(decayed) {
                Some(pointee) if self.l.types.is_function(pointee.ty) => pointee.ty,
                _ => {
                    return Err(Diagnostic::new(
                        callee.span(),
                        "called object is not a function",
                    ));
                }
            }
        };
        let direct = match &value.kind {
            ExprKind::Place(address) => match &address.base {
                ir::AddressBase::Symbol(symbol)
                    if address.offset == 0 && self.l.types.is_function(value.ty) =>
                {
                    Some(symbol.clone())
                }
                _ => None,
            },
            _ => None,
        };
        let target = match direct {
            Some(symbol) => ir::CallTarget::Direct(symbol),
            None => {
                let (operand, _) = self.rvalue_val(value, span)?;
                ir::CallTarget::Indirect(operand)
            }
        };
        let (params, variadic) = self.function_params(function_ty, span)?;
        let operands = self.lower_arguments(args, &params, variadic, span)?;
        self.emit_call(target, function_ty, operands, span)
    }

    /// Instantiate the callee of a call that writes explicit generic
    /// arguments, accepting a generic function name or a generic method.
    pub(super) fn generic_callee_instance(
        &mut self,
        callee: &ast::Expr,
        type_args: &[ast::GenericArg],
        span: Span,
    ) -> Result<FunctionInstance, Diagnostic> {
        match callee {
            ast::Expr::Name(name) => self.l.instantiate_generic_function(name, type_args),
            ast::Expr::Member { object, name, .. } => {
                let object_ty = self.type_of(object)?;
                let Some(record) = self.l.receiver_of(object_ty) else {
                    return Err(Diagnostic::new(
                        span,
                        "method call needs a struct, union, pointer, or scalar receiver",
                    ));
                };
                if self.l.method_with_autoderef(object_ty, name)?.is_some() {
                    return Err(Diagnostic::new(
                        span,
                        format!("method '{}' is not generic", name.text),
                    ));
                }
                self.l.instantiate_generic_method(record, name, type_args)
            }
            ast::Expr::ScopedName {
                qualifier, name, ..
            } => {
                let Some(record) = self.l.lookup_named_type(&qualifier.text) else {
                    // Reuse the ordinary static-call diagnostics: a trait name,
                    // a generic type without arguments, or an unknown type.
                    self.l.scoped_method(qualifier, name)?;
                    return Err(Diagnostic::new(
                        span,
                        format!("unknown type name '{}'", qualifier.text),
                    ));
                };
                if self.l.method_of(record, name)?.is_some() {
                    return Err(Diagnostic::new(
                        span,
                        format!("method '{}' is not generic", name.text),
                    ));
                }
                self.l.instantiate_generic_method(record, name, type_args)
            }
            ast::Expr::GenericScopedName {
                qualifier,
                args,
                name,
                ..
            } => {
                let ty = self.l.generic_qualifier_type(qualifier, args)?;
                if self.l.method_of(ty, name)?.is_some() {
                    return Err(Diagnostic::new(
                        span,
                        format!("method '{}' is not generic", name.text),
                    ));
                }
                self.l.instantiate_generic_method(ty, name, type_args)
            }
            _ => self.l.generic_call_target(callee, type_args, span),
        }
    }

    /// Instantiate a generic method from the value arguments of a call that
    /// writes no type arguments.
    ///
    /// A receiver call skips its receiver parameter, which is lowered
    /// separately; a static call includes it as its first argument.
    pub(super) fn inferred_method_instance(
        &mut self,
        callee: &ast::Expr,
        args: &[ast::Expr],
    ) -> Result<Option<FunctionInstance>, Diagnostic> {
        let (record, name, skip_params) = match callee {
            ast::Expr::Member { object, name, .. } => {
                let object_ty = self.type_of(object)?;
                let Some(record) = self.l.receiver_of(object_ty) else {
                    return Ok(None);
                };
                (record, name, 1)
            }
            ast::Expr::ScopedName {
                qualifier, name, ..
            } => {
                let Some(ty) = self.l.lookup_named_type(&qualifier.text) else {
                    return Ok(None);
                };
                let Some(record) = self.l.receiver_of(ty) else {
                    return Ok(None);
                };
                (record, name, 0)
            }
            ast::Expr::GenericScopedName {
                qualifier,
                args,
                name,
                ..
            } => {
                let ty = self.l.generic_qualifier_type(qualifier, args)?;
                let Some(record) = self.l.receiver_of(ty) else {
                    return Ok(None);
                };
                (record, name, 0)
            }
            _ => return Ok(None),
        };
        if self.l.method_with_autoderef(record, name)?.is_some()
            || !self.l.has_generic_method(record, &name.text)?
        {
            return Ok(None);
        }
        let mut arguments = Vec::with_capacity(args.len());
        for argument in args {
            arguments.push((argument.span(), self.type_of(argument)?));
        }
        Ok(Some(self.l.infer_generic_method(
            record,
            name,
            &arguments,
            skip_params,
        )?))
    }

    /// Lower a call through one already instantiated generic function.
    fn lower_instance_call(
        &mut self,
        instance: FunctionInstance,
        args: &[ast::Expr],
        span: Span,
    ) -> Result<ExprVal, Diagnostic> {
        let (params, variadic) = self.function_params(instance.ty, span)?;
        let operands = self.lower_arguments(args, &params, variadic, span)?;
        self.emit_call(
            ir::CallTarget::Direct(instance.symbol),
            instance.ty,
            operands,
            span,
        )
    }

    /// Instantiates `name(args)` without written type arguments, taking the
    /// bindings from the argument expression types.
    fn infer_generic_call(
        &mut self,
        name: &ast::Name,
        args: &[ast::Expr],
    ) -> Result<FunctionInstance, Diagnostic> {
        let mut arguments = Vec::with_capacity(args.len());
        for argument in args {
            arguments.push((argument.span(), self.type_of(argument)?));
        }
        self.l.infer_generic_function(name, &arguments)
    }

    /// Whether a parameter or block-scope declaration hides a file-scope name.
    fn has_local_binding(&self, name: &str) -> bool {
        self.scopes
            .iter()
            .rev()
            .any(|scope| scope.bindings.contains_key(name))
    }

    /// Lower a receiver call through one instantiated generic method.
    fn lower_generic_method_call(
        &mut self,
        object: &ast::Expr,
        name: &ast::Name,
        op: ast::MemberOp,
        instance: FunctionInstance,
        args: &[ast::Expr],
        span: Span,
    ) -> Result<ExprVal, Diagnostic> {
        self.lower_method_call_target(
            object,
            name,
            op,
            super::MethodInfo {
                symbol: instance.symbol,
                ty: instance.ty,
                deferred: None,
            },
            args,
            span,
        )
    }

    fn lower_method_call_target(
        &mut self,
        object: &ast::Expr,
        name: &ast::Name,
        op: ast::MemberOp,
        method: super::MethodInfo,
        args: &[ast::Expr],
        span: Span,
    ) -> Result<ExprVal, Diagnostic> {
        let object_value = self.lower_expr(object)?;
        self.lower_method_call_value(object_value, Some(object), name, op, method, args, span)
    }

    pub(super) fn lower_method_call_value(
        &mut self,
        object_value: ExprVal,
        object_expr: Option<&ast::Expr>,
        name: &ast::Name,
        op: ast::MemberOp,
        method: super::MethodInfo,
        args: &[ast::Expr],
        span: Span,
    ) -> Result<ExprVal, Diagnostic> {
        let (params, variadic) = self.function_params(method.ty, span)?;
        let Some(&first_param) = params.first() else {
            return Err(Diagnostic::new(
                span,
                format!("method '{}' has no self parameter", name.text),
            ));
        };
        if let Some(receiver) = self.l.pointee(first_param)
            && !receiver.qualifiers.is_const
        {
            let mut receiver_ty = object_value.ty;
            let mut receiver_const = object_value.qualifiers.is_const;
            while let Some(pointee) = self.l.pointee(receiver_ty) {
                receiver_const = pointee.qualifiers.is_const;
                receiver_ty = pointee.ty;
            }
            if receiver_const {
                return Err(Diagnostic::new(
                    span,
                    format!("mutable method '{}' needs a writable receiver", name.text),
                ));
            }
        }
        let object_ty = object_value.ty;
        if op == ast::MemberOp::Arrow && !self.l.types.is_pointer(object_ty) {
            return Err(Diagnostic::new(
                span,
                "`->` needs a pointer to a struct or union",
            ));
        }
        let receiver = if self.l.types.is_record(object_ty) {
            if self.l.types.is_pointer(first_param) {
                let address = self.object_address(object_value, span)?;
                self.address_value(address)
            } else if self.l.types.is_compatible(object_ty, first_param) {
                self.rvalue_val(object_value, span)?.0
            } else {
                return Err(Diagnostic::new(
                    span,
                    "method receiver type does not match its self parameter",
                ));
            }
        } else if self.l.types.is_pointer(object_ty) || self.l.types.is_array(object_ty) {
            let (mut operand, mut ty) = self.rvalue_val(object_value, span)?;
            // ShyC dereferences automatically: walk the pointer chain until it
            // reaches the record, so `r.get()` on a `P **r` passes `*r`. A
            // pointer receiver for a scalar or enum stops at that pointer, so an
            // `A *` receiver keeps its address instead of loading the value.
            while let Some(pointee) = self.l.pointee(ty) {
                let pointer_matches = match (self.l.pointee(first_param), self.l.pointee(ty)) {
                    (Some(expected), Some(actual)) => {
                        self.l.types.is_compatible(expected.ty, actual.ty)
                    }
                    _ => false,
                };
                if pointer_matches {
                    break;
                }
                if self.l.types.is_record(pointee.ty) {
                    break;
                }
                let address = super::address_operand(operand);
                operand = self.load_value(address, pointee.ty, Qualifiers::NONE);
                ty = pointee.ty;
            }
            if self.l.types.is_pointer(first_param) {
                self.convert(operand, ty, first_param, span, false)?
            } else if self.l.types.is_record(ty) {
                // The receiver arrived as a value already.
                operand
            } else {
                let Some(pointee) = self.l.pointee(ty) else {
                    return Err(Diagnostic::new(
                        span,
                        "method receiver type does not match its self parameter",
                    ));
                };
                let address = super::address_operand(operand);
                self.load_value(address, pointee.ty, Qualifiers::NONE)
            }
        } else if self.l.types.is_pointer(first_param)
            && self
                .l
                .pointee(first_param)
                .is_some_and(|pointee| self.l.types.is_compatible(pointee.ty, object_ty))
        {
            // A scalar or enum receiver has no Deref chain: its method takes
            // `self *`, so the value is spilled to a slot and its address passed.
            let address = self.object_address(object_value, span)?;
            self.address_value(address)
        } else {
            return Err(Diagnostic::new(
                span,
                "method call needs a struct, union, pointer, or scalar receiver",
            ));
        };
        let receiver_type = if self.l.types.is_pointer(first_param) {
            first_param
        } else {
            object_ty
        };
        let _ = receiver_type;
        let mut operands = vec![receiver];
        let mut extra = self.lower_arguments(args, &params[1..], variadic, span)?;
        operands.append(&mut extra);
        let result = self.emit_call(
            ir::CallTarget::Direct(method.symbol),
            method.ty,
            operands,
            span,
        )?;
        if name.text == "drop"
            && let Some(object) = object_expr
            && let ast::Expr::Name(object_name) = object
            && let Ok(super::Resolved::Local(local)) =
                self.resolve_name(&object_name.text, object_name.span)
            && self.locals[local].drop_symbol.is_some()
        {
            self.locals[local].drop_active = false;
        }
        Ok(result)
    }

    fn function_params(
        &mut self,
        function_ty: TypeId,
        span: Span,
    ) -> Result<(Vec<TypeId>, bool), Diagnostic> {
        match self.l.types.kind(function_ty) {
            TypeKind::Function {
                params, variadic, ..
            } => Ok((params.clone(), *variadic)),
            _ => Err(Diagnostic::new(span, "called object is not a function")),
        }
    }

    fn lower_arguments(
        &mut self,
        args: &[ast::Expr],
        params: &[TypeId],
        variadic: bool,
        span: Span,
    ) -> Result<Vec<ir::Operand>, Diagnostic> {
        if args.len() < params.len() {
            return Err(Diagnostic::new(span, "too few arguments in function call"));
        }
        if args.len() > params.len() && !variadic {
            return Err(Diagnostic::new(span, "too many arguments in function call"));
        }
        let mut operands = Vec::with_capacity(args.len());
        for (index, argument) in args.iter().enumerate() {
            if let Some(&param) = params.get(index) {
                if self.l.types.is_aggregate(param) {
                    let (operand, ty) = self.rvalue(argument)?;
                    if !self.l.types.is_compatible(param, ty) {
                        return Err(Diagnostic::new(
                            argument.span(),
                            "argument type does not match the parameter type",
                        ));
                    }
                    if self.l.drop_method(param)?.is_some() {
                        self.deactivate_if_local(argument);
                    }
                    operands.push(operand);
                } else {
                    let (operand, ty) = self.rvalue(argument)?;
                    let operand = self.convert(operand, ty, param, argument.span(), false)?;
                    operands.push(operand);
                }
            } else {
                operands.push(self.default_argument(argument)?);
            }
        }
        Ok(operands)
    }

    fn default_argument(&mut self, argument: &ast::Expr) -> Result<ir::Operand, Diagnostic> {
        let (operand, ty) = self.rvalue(argument)?;
        if self.l.types.is_floating(ty) && self.l.types.size_of(ty) == Ok(4) {
            let double = self.l.types.builtin(crate::sema::types::Builtin::Double);
            return self.convert(operand, ty, double, argument.span(), false);
        }
        if self.l.types.is_integer(ty) {
            let promoted = self.l.integer_promotion(ty);
            return self.convert(operand, ty, promoted, argument.span(), false);
        }
        Ok(operand)
    }

    pub(super) fn emit_call(
        &mut self,
        target: ir::CallTarget,
        function_ty: TypeId,
        arguments: Vec<ir::Operand>,
        span: Span,
    ) -> Result<ExprVal, Diagnostic> {
        // The driver's C export prints a call under the symbol resolved here,
        // so the two backends name one call the same way.
        if let ir::CallTarget::Direct(symbol) = &target {
            let context = self.l.impl_context.as_ref().map(|(_, name)| name.clone());
            self.l.call_symbols.insert((span, context), symbol.clone());
        }
        let signature = self.l.ir_signature(function_ty);
        let TypeKind::Function { result, .. } = self.l.types.kind(function_ty) else {
            return Err(Diagnostic::new(span, "called object is not a function"));
        };
        let result_ty = *result;
        // Variadic calls pass more arguments than the prototype names, so the
        // plan is built from every argument that was actually lowered.
        let extras: Vec<ir::Type> = arguments
            .iter()
            .skip(signature.params.len())
            .map(|argument| self.f.operand_type(*argument))
            .collect();
        let abi = crate::abi_adapter::plan_target_call(
            &self.l.types,
            function_ty,
            &extras,
            self.l.layout,
            self.l.abi,
        )
        .map_err(|error| Diagnostic::new(span, format!("cannot plan the target ABI: {error}")))?;
        let call = match target {
            ir::CallTarget::Direct(symbol) => {
                // `va_start` is an LLVM intrinsic: the backend expands it into
                // the target's register save sequence.
                let symbol = if symbol == "__builtin_va_start" {
                    "llvm.va_start".to_string()
                } else {
                    symbol
                };
                ir::Call::direct(symbol, arguments, signature, abi)
            }
            ir::CallTarget::Indirect(callee) => {
                ir::Call::indirect(callee, arguments, signature, abi)
            }
        };
        if self.l.types.is_void(result_ty) {
            self.emit_statement(ir::InstKind::Call(Box::new(call)));
            Ok(ExprVal {
                ty: result_ty,
                qualifiers: Qualifiers::NONE,
                kind: ExprKind::Void,
            })
        } else {
            let value = self.emit_value(ir::InstKind::Call(Box::new(call)));
            Ok(ExprVal {
                ty: result_ty,
                qualifiers: Qualifiers::NONE,
                kind: ExprKind::Value(ir::Operand::Value(value)),
            })
        }
    }
}
