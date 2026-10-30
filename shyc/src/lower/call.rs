//! Member access, subscripts, and calls (including Shy method calls).
//!
//! Dot access auto-dereferences one pointer level, matching Shy. Instance
//! methods are ordinary functions whose first parameter is the receiver; the
//! call passes the receiver explicitly and targets the mangled
//! `____Type__method` symbol.

use super::{ExprKind, ExprVal, FnLower};
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
        let target = self.member_target(object, name, op, span)?;
        Ok(ExprVal {
            ty: target.ty,
            qualifiers: target.qualifiers,
            kind: ExprKind::Place(target.address),
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
            let offset = base_address.offset + byte_offset;
            let address = base_address.with_offset(offset);
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
        args: &[ast::Expr],
        span: Span,
    ) -> Result<ExprVal, Diagnostic> {
        if let ast::Expr::Member {
            object, name, op, ..
        } = callee
        {
            let object_ty = self.type_of(object)?;
            if let Some(record) = self.l.record_of(object_ty)
                && let Some(method) = self.l.method_of(record, &name.text)
            {
                return self.lower_method_call(object, name, *op, method, args, span);
            }
        }
        if let ast::Expr::ScopedName {
            qualifier,
            name,
            span: call_span,
        } = callee
        {
            let method = self.l.scoped_method(qualifier, name)?;
            let (params, variadic) = self.function_params(method.ty, span)?;
            let operands = self.lower_arguments(args, &params, variadic, span)?;
            return self.emit_call(
                ir::CallTarget::Direct(method.symbol),
                method.ty,
                operands,
                *call_span,
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

    fn lower_method_call(
        &mut self,
        object: &ast::Expr,
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
        let object_value = self.lower_expr(object)?;
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
            let (operand, ty) = self.rvalue_val(object_value, span)?;
            if self.l.types.is_pointer(first_param) {
                self.convert(operand, ty, first_param, span, false)?
            } else {
                return Err(Diagnostic::new(
                    span,
                    "method receiver is a pointer but the self parameter is not",
                ));
            }
        } else {
            return Err(Diagnostic::new(
                span,
                "method call needs a struct, union, or pointer receiver",
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
                    if self.l.drop_method(param).is_some() {
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

    fn emit_call(
        &mut self,
        target: ir::CallTarget,
        function_ty: TypeId,
        arguments: Vec<ir::Operand>,
        span: Span,
    ) -> Result<ExprVal, Diagnostic> {
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
            ir::CallTarget::Direct(symbol) => ir::Call::direct(symbol, arguments, signature, abi),
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
