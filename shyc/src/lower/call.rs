//! Member access, subscripts, and calls (including Shy method calls).
//!
//! Dot access auto-dereferences one pointer level, matching Shy. Instance
//! methods are ordinary functions whose first parameter is the receiver; the
//! call passes the receiver explicitly and targets the mangled symbol
//! (`____Type__method` for an inherent method, with the trait name in front
//! for a trait method). An inherent method wins over a trait method, and two
//! matching trait methods make a receiver call ambiguous.

use super::{DerefStep, ExprKind, ExprVal, FnLower, FunctionInstance};
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

    /// Lower `value.Variant` for a payload enum.
    fn lower_payload_enum_member(
        &mut self,
        object: &ast::Expr,
        name: &ast::Name,
        span: Span,
    ) -> Result<Option<ExprVal>, Diagnostic> {
        let object_ty = self.type_of(object)?;
        let Some(enum_ty) = self.l.payload_enum_target(object_ty) else {
            return Ok(None);
        };
        let payload = self
            .l
            .types
            .enum_variant(enum_ty, &name.text)
            .map(|variant| variant.payload);
        if let Some(payload) = payload {
            let value = self.lower_expr(object)?;
            let (address, qualifiers) = self.payload_enum_address(value, span)?;
            let Some(payload_ty) = payload else {
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
        Err(Diagnostic::new(
            span,
            format!(
                "payload enum has no member '{}'; use '.type()' or '.Variant'",
                name.text
            ),
        ))
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

    /// The tag constant a bare variant name denotes.
    ///
    /// A bare variant name is its tag. Construction always writes parentheses
    /// or a field initializer.
    pub(super) fn variant_tag_value(
        &mut self,
        variant: &super::type_of::ScopedVariant,
        span: Span,
    ) -> Result<ExprVal, Diagnostic> {
        let tag_ty = self
            .l
            .enum_tag_type(variant.ty)
            .ok_or_else(|| Diagnostic::new(span, "type is not an enum"))?;
        let operand = ir::Operand::Constant(ir::Constant::int(self.l.ir_type(tag_ty), variant.tag));
        Ok(ExprVal {
            ty: tag_ty,
            qualifiers: Qualifiers::NONE,
            kind: ExprKind::Value(operand),
        })
    }

    fn lower_enum_type(
        &mut self,
        object: &ast::Expr,
        enum_ty: TypeId,
        span: Span,
    ) -> Result<ExprVal, Diagnostic> {
        let value = self.lower_expr(object)?;
        let (address, qualifiers) = self.enum_address(value, span)?;
        let tag_ty = self
            .l
            .enum_tag_type(enum_ty)
            .ok_or_else(|| Diagnostic::new(span, "type is not an enum"))?;
        if self.l.types.is_niche_enum(enum_ty) {
            let underlying = self
                .l
                .types
                .enum_underlying(enum_ty)
                .ok_or_else(|| Diagnostic::new(span, "niche enum has no representation"))?;
            let raw = self.load_value(address, underlying, qualifiers);
            let raw_ty = self.l.ir_type(underlying);
            let nonzero = self.emit_value(ir::InstKind::ICmp {
                pred: ir::IntPredicate::Ne,
                lhs: raw,
                rhs: ir::Operand::Constant(ir::Constant::zero(raw_ty)),
                ty: raw_ty,
            });
            return Ok(ExprVal {
                ty: tag_ty,
                qualifiers: Qualifiers::NONE,
                kind: ExprKind::Value(self.bool_to_int(ir::Operand::Value(nonzero))),
            });
        }
        let load_ty = if self.l.types.is_payload_enum(enum_ty) {
            self.l
                .types
                .enum_underlying(enum_ty)
                .ok_or_else(|| Diagnostic::new(span, "payload enum has no underlying type"))?
        } else {
            enum_ty
        };
        let operand = self.load_value(address, load_ty, qualifiers);
        Ok(ExprVal {
            ty: tag_ty,
            qualifiers: Qualifiers::NONE,
            kind: ExprKind::Value(operand),
        })
    }

    fn enum_address(
        &mut self,
        mut value: ExprVal,
        span: Span,
    ) -> Result<(ir::Address, Qualifiers), Diagnostic> {
        loop {
            if self.l.types.enum_info(value.ty).is_some() {
                let qualifiers = value.qualifiers;
                let address = match value.kind {
                    ExprKind::Place(address) => address,
                    ExprKind::Value(operand) => {
                        let slot = self.temp_slot(value.ty, "");
                        self.store_value(
                            ir::Address::slot(slot),
                            operand,
                            value.ty,
                            Qualifiers::NONE,
                        );
                        ir::Address::slot(slot)
                    }
                    ExprKind::Void => {
                        return Err(Diagnostic::new(span, "void has no enum type"));
                    }
                };
                return Ok((address, qualifiers));
            }
            let Some(pointee) = self.l.pointee(value.ty) else {
                return Err(Diagnostic::new(span, "type() needs an enum object"));
            };
            let (operand, _) = self.rvalue_val(value, span)?;
            let address = super::address_operand(operand);
            if self.l.types.enum_info(pointee.ty).is_some() {
                return Ok((address, pointee.qualifiers));
            }
            let loaded = self.load_value(address, pointee.ty, pointee.qualifiers);
            value = ExprVal {
                ty: pointee.ty,
                qualifiers: pointee.qualifiers,
                kind: ExprKind::Value(loaded),
            };
        }
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

    pub(super) fn construct_struct_variant(
        &mut self,
        qualifier: &ast::Name,
        type_args: &[ast::GenericArg],
        variant_name: &ast::Name,
        fields: &[ast::VariantFieldInit],
        span: Span,
    ) -> Result<ExprVal, Diagnostic> {
        let ty = if type_args.is_empty() {
            self.l
                .lookup_named_type(&qualifier.text)
                .ok_or_else(|| Diagnostic::new(qualifier.span, "unknown enum type"))?
        } else {
            self.l.generic_qualifier_type(qualifier, type_args)?
        };
        let variant = self
            .l
            .variant_of(ty, variant_name)
            .ok_or_else(|| Diagnostic::new(variant_name.span, "unknown enum variant"))?;
        let payload_ty = variant.payload.ok_or_else(|| {
            Diagnostic::new(span, "a unit variant cannot be initialized with fields")
        })?;
        if !self.l.types.is_record(payload_ty) {
            return Err(Diagnostic::new(
                span,
                "this variant does not have struct fields",
            ));
        }
        let record_fields = self.l.record_fields(payload_ty);
        let mut initialized = vec![false; record_fields.len()];
        let size = self.l.size_of(ty, span)?;
        let slot = self.temp_slot(ty, "");
        self.emit_statement(ir::InstKind::MemSet {
            dst: ir::Address::slot(slot),
            byte: 0,
            bytes: size,
        });
        let underlying = self
            .l
            .types
            .enum_underlying(ty)
            .ok_or_else(|| Diagnostic::new(span, "payload enum has no underlying type"))?;
        self.store_value(
            ir::Address::slot(slot),
            ir::Operand::Constant(ir::Constant::int(self.l.ir_type(underlying), variant.tag)),
            underlying,
            Qualifiers::NONE,
        );
        let payload_offset = self
            .l
            .types
            .enum_info(ty)
            .map(|info| info.payload_offset)
            .unwrap_or(0);
        for field in fields {
            let index = record_fields
                .iter()
                .position(|(name, _, _)| name == &field.name.text)
                .ok_or_else(|| {
                    Diagnostic::new(
                        field.name.span,
                        format!("variant has no field '{}'", field.name.text),
                    )
                })?;
            if initialized[index] {
                return Err(Diagnostic::new(
                    field.name.span,
                    format!("field '{}' is initialized twice", field.name.text),
                ));
            }
            initialized[index] = true;
            let (_, field_ty, field_offset) = record_fields[index];
            let address =
                ir::Address::slot(slot).offset_by(i64::from(payload_offset + field_offset));
            let value = self.lower_expr(&field.value)?;
            if self.l.types.is_aggregate(field_ty) {
                if !self.l.types.is_compatible(value.ty, field_ty) {
                    return Err(Diagnostic::new(
                        field.value.span(),
                        "field initializer type does not match the field",
                    ));
                }
                self.copy_aggregate(address, value, field_ty, field.value.span())?;
            } else {
                let source_ty = value.ty;
                let (operand, _) = self.rvalue_val(value, field.value.span())?;
                let operand =
                    self.convert(operand, source_ty, field_ty, field.value.span(), false)?;
                self.store_value(address, operand, field_ty, Qualifiers::NONE);
            }
        }
        if let Some(index) = initialized.iter().position(|done| !done) {
            return Err(Diagnostic::new(
                span,
                format!("field '{}' is missing", record_fields[index].0),
            ));
        }
        Ok(ExprVal {
            ty,
            qualifiers: Qualifiers::NONE,
            kind: ExprKind::Place(ir::Address::slot(slot)),
        })
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
            let (instance, steps) = self.generic_callee_instance(callee, type_args, span)?;
            if let ast::Expr::Member {
                object, name, op, ..
            } = callee
            {
                return self
                    .lower_generic_method_call(object, name, *op, instance, &steps, args, span);
            }
            return self.lower_instance_call(callee, instance, args, span);
        }
        if args.is_empty()
            && let ast::Expr::Member { object, name, .. } = callee
            && name.text == "type"
        {
            let object_ty = self.type_of(object)?;
            let enum_ty = self
                .l
                .enum_target(object_ty)
                .ok_or_else(|| Diagnostic::new(span, "type() needs an enum object"))?;
            return self.lower_enum_type(object, enum_ty, span);
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
            return self.lower_instance_call(callee, instance, args, span);
        }
        if let ast::Expr::Member {
            object, name, op, ..
        } = callee
            && name.text == "move"
        {
            return self.lower_defer_move(object, *op, args, span);
        }
        if let ast::Expr::Member {
            object, name, op, ..
        } = callee
        {
            let object_ty = self.type_of(object)?;
            if let Some(resolved) = self.l.method_with_autoderef(object_ty, name)? {
                let receiver =
                    self.lower_receiver_steps(object, name, *op, &resolved.steps, span)?;
                let method_op = if resolved.steps.is_empty() {
                    *op
                } else {
                    ast::MemberOp::Dot
                };
                return self.lower_method_call_value(
                    receiver,
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
        if let Some((instance, steps)) = self.inferred_method_instance(callee, args)? {
            if let ast::Expr::Member {
                object, name, op, ..
            } = callee
            {
                return self
                    .lower_generic_method_call(object, name, *op, instance, &steps, args, span);
            }
            return self.lower_instance_call(callee, instance, args, span);
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
    ) -> Result<(FunctionInstance, Vec<DerefStep>), Diagnostic> {
        match callee {
            ast::Expr::Name(name) => Ok((
                self.l.instantiate_generic_function(name, type_args)?,
                Vec::new(),
            )),
            ast::Expr::Member { object, name, .. } => {
                let object_ty = self.type_of(object)?;
                if self.l.method_with_autoderef(object_ty, name)?.is_some() {
                    return Err(Diagnostic::new(
                        span,
                        format!("method '{}' is not generic", name.text),
                    ));
                }
                let Some(path) = self.l.generic_method_path(object_ty, name)? else {
                    return Err(Diagnostic::new(
                        span,
                        format!("no generic method '{}' on this type", name.text),
                    ));
                };
                Ok((
                    self.l
                        .instantiate_generic_method(path.owner, name, type_args)?,
                    path.steps,
                ))
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
                Ok((
                    self.l.instantiate_generic_method(record, name, type_args)?,
                    Vec::new(),
                ))
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
                Ok((
                    self.l.instantiate_generic_method(ty, name, type_args)?,
                    Vec::new(),
                ))
            }
            _ => Ok((
                self.l.generic_call_target(callee, type_args, span)?,
                Vec::new(),
            )),
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
    ) -> Result<Option<(FunctionInstance, Vec<DerefStep>)>, Diagnostic> {
        let (record, name, skip_params, steps) = match callee {
            ast::Expr::Member { object, name, .. } => {
                let object_ty = self.type_of(object)?;
                let Some(path) = self.l.generic_method_path(object_ty, name)? else {
                    return Ok(None);
                };
                (path.owner, name, 1, path.steps)
            }
            ast::Expr::ScopedName {
                qualifier, name, ..
            } => {
                let Some(ty) = self.l.lookup_named_type(&qualifier.text) else {
                    return Ok(None);
                };
                if self.l.method_of(ty, name)?.is_some()
                    || !self.l.has_generic_method(ty, &name.text)?
                {
                    return Ok(None);
                }
                (ty, name, 0, Vec::new())
            }
            ast::Expr::GenericScopedName {
                qualifier,
                args,
                name,
                ..
            } => {
                let ty = self.l.generic_qualifier_type(qualifier, args)?;
                if self.l.method_of(ty, name)?.is_some()
                    || !self.l.has_generic_method(ty, &name.text)?
                {
                    return Ok(None);
                }
                (ty, name, 0, Vec::new())
            }
            _ => return Ok(None),
        };
        let mut arguments = Vec::with_capacity(args.len());
        for argument in args {
            arguments.push((argument.span(), self.type_of(argument)?));
        }
        Ok(Some((
            self.l
                .infer_generic_method(record, name, &arguments, skip_params)?,
            steps,
        )))
    }

    /// Lower a call through one already instantiated generic function.
    ///
    /// The C export names the call the way the compiler resolved it: one
    /// template has one symbol per instance, so the callee's name alone cannot
    /// say which symbol a call reaches.
    fn lower_instance_call(
        &mut self,
        callee: &ast::Expr,
        instance: FunctionInstance,
        args: &[ast::Expr],
        span: Span,
    ) -> Result<ExprVal, Diagnostic> {
        if let ast::Expr::Name(name) = callee {
            self.l
                .call_symbols
                .insert((span, name.text.clone()), instance.symbol.clone());
        }
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
        steps: &[DerefStep],
        args: &[ast::Expr],
        span: Span,
    ) -> Result<ExprVal, Diagnostic> {
        let receiver = self.lower_receiver_steps(object, name, op, steps, span)?;
        let method_op = if steps.is_empty() {
            op
        } else {
            ast::MemberOp::Dot
        };
        self.lower_method_call_value(
            receiver,
            name,
            method_op,
            super::MethodInfo {
                symbol: instance.symbol,
                ty: instance.ty,
                deferred: None,
            },
            args,
            span,
        )
    }

    /// Lower the `Deref` projections a method lookup found before the method
    /// itself is called.
    fn lower_receiver_steps(
        &mut self,
        object: &ast::Expr,
        name: &ast::Name,
        op: ast::MemberOp,
        steps: &[DerefStep],
        span: Span,
    ) -> Result<ExprVal, Diagnostic> {
        let mut receiver = self.lower_expr(object)?;
        for (index, step) in steps.iter().enumerate() {
            let step_name = ast::Name::new(step.name, name.span);
            let step_op = if index == 0 { op } else { ast::MemberOp::Dot };
            receiver = self.lower_method_call_value(
                receiver,
                &step_name,
                step_op,
                step.method.clone(),
                &[],
                span,
            )?;
        }
        Ok(receiver)
    }

    pub(super) fn lower_method_call_value(
        &mut self,
        object_value: ExprVal,
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
            // Stop at the type self names, even when it is a pointer: a
            // pointer impl takes the pointer value itself, so its pointee's
            // constness is not the receiver's.
            while !self.l.types.is_compatible(receiver_ty, receiver.ty) {
                let Some(pointee) = self.l.pointee(receiver_ty) else {
                    break;
                };
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
        let receiver = if self.l.types.is_record(object_ty)
            || (self.l.types.is_pointer(object_ty)
                && self
                    .l
                    .pointee(first_param)
                    .is_some_and(|expected| self.l.types.is_compatible(expected.ty, object_ty)))
        {
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
        Ok(result)
    }

    /// `name.move()`: read a defer local and set its moved flag, so the scope's
    /// exit drops skip it.
    fn lower_defer_move(
        &mut self,
        object: &ast::Expr,
        op: ast::MemberOp,
        args: &[ast::Expr],
        span: Span,
    ) -> Result<ExprVal, Diagnostic> {
        if !args.is_empty() {
            return Err(Diagnostic::new(span, "move() takes no arguments"));
        }
        if op == ast::MemberOp::Arrow {
            return Err(Diagnostic::new(
                span,
                "move() needs a variable declared with `defer`",
            ));
        }
        let ast::Expr::Name(name) = object else {
            return Err(Diagnostic::new(
                object.span(),
                "move() needs a variable declared with `defer`",
            ));
        };
        let instruction = "move() needs a variable declared with `defer`";
        let Ok(super::Resolved::Local(local)) = self.resolve_name(&name.text, name.span) else {
            return Err(Diagnostic::new(name.span, instruction));
        };
        if self.locals[local].moved_slot.is_none() {
            return Err(Diagnostic::new(
                name.span,
                format!(
                    "'{}' is not a defer variable; .move() only applies to one",
                    name.text
                ),
            ));
        }
        Ok(self.move_defer_local(local))
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
        // Only main expands an #[args_builtin] parameter into its fields.
        let expand_args = matches!(&target, ir::CallTarget::Direct(symbol) if symbol == "main");
        let mut arguments = arguments;
        if expand_args {
            arguments = self.expand_args_arguments(function_ty, arguments);
        }
        let signature = self.l.ir_signature(function_ty, expand_args);
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
            expand_args,
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

    /// main's #[args_builtin] argument becomes its fields, so the call matches
    /// the flattened signature. Any other argument passes through unchanged.
    fn expand_args_arguments(
        &mut self,
        function_ty: TypeId,
        arguments: Vec<ir::Operand>,
    ) -> Vec<ir::Operand> {
        let TypeKind::Function { params, .. } = self.l.types.kind(function_ty) else {
            return arguments;
        };
        let params = params.clone();
        let mut expanded = Vec::with_capacity(arguments.len());
        let mut arguments = arguments.into_iter();
        for param in params {
            let Some(argument) = arguments.next() else {
                break;
            };
            let Some(fields) = self.l.args_builtin_fields(param) else {
                expanded.push(argument);
                continue;
            };
            let slot = self.temp_slot(param, "");
            self.store_value(ir::Address::slot(slot), argument, param, Qualifiers::NONE);
            for (field_ty, offset) in fields {
                let address = ir::Address::slot(slot).offset_by(i64::from(offset));
                expanded.push(self.load_value(address, field_ty, Qualifiers::NONE));
            }
        }
        expanded.extend(arguments);
        expanded
    }
}
