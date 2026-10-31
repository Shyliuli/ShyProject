//! Constant-expression evaluation for array bounds, enum values, `case`
//! labels, static initializers, and `sizeof`/`_Alignof`.
//!
//! The result keeps the C type, because conversions must follow the usual
//! arithmetic conversions even when everything is known at compile time.
//! Address constants carry an optional symbol: `Some` is a link-time address
//! that becomes an [`ir::Relocation`], `None` is an absolute integer address
//! from a cast.

use super::Lowerer;
use super::types::NumericLiteral;
use crate::ast;
use crate::ir;
use crate::sema::types::{TypeId, TypeKind};
use crate::source::{Diagnostic, Span};

pub(super) enum ConstKind {
    Int(u64),
    Float(u64),
    Address { symbol: Option<String>, addend: i64 },
}

pub(super) struct ConstValue {
    pub ty: TypeId,
    pub kind: ConstKind,
}

impl ConstValue {
    fn int(ty: TypeId, bits: u64) -> ConstValue {
        ConstValue {
            ty,
            kind: ConstKind::Int(bits),
        }
    }

    fn address(ty: TypeId, symbol: Option<String>, addend: i64) -> ConstValue {
        ConstValue {
            ty,
            kind: ConstKind::Address { symbol, addend },
        }
    }
}

impl Lowerer {
    pub(super) fn eval_const_int(&mut self, expr: &ast::Expr) -> Result<i64, Diagnostic> {
        let value = self.eval_const(expr)?;
        match value.kind {
            ConstKind::Int(bits) => {
                if self.is_unsigned(value.ty) {
                    Ok(bits as i64)
                } else {
                    Ok(sign_extend(bits, self.int_width(value.ty)))
                }
            }
            _ => Err(Diagnostic::new(
                expr.span(),
                "expression is not an integer constant expression",
            )),
        }
    }

    /// Evaluate an enumerator initializer as a mathematical integer.
    ///
    /// Unlike [`Self::eval_const_int`], an unsigned value wider than `i64`
    /// stays positive, so the caller can range-check it against an unsigned
    /// enum underlying type without mistaking the top bit for a sign bit.
    pub(super) fn eval_enum_value(&mut self, expr: &ast::Expr) -> Result<i128, Diagnostic> {
        let value = self.eval_const(expr)?;
        let ConstKind::Int(bits) = value.kind else {
            return Err(Diagnostic::new(
                expr.span(),
                "enumerator value is not an integer constant expression",
            ));
        };
        Ok(if self.is_unsigned(value.ty) {
            i128::from(bits)
        } else {
            i128::from(sign_extend(bits, self.int_width(value.ty)))
        })
    }

    pub(super) fn eval_const(&mut self, expr: &ast::Expr) -> Result<ConstValue, Diagnostic> {
        if let ast::Expr::Call { type_args, .. } = expr
            && let Some(argument) = type_args.first()
        {
            return Err(Diagnostic::new(
                argument.span,
                "a generic call is not a constant expression",
            ));
        }
        match expr {
            ast::Expr::Number(literal) => match self.number_literal(literal)? {
                NumericLiteral::Integer { bits, ty } => Ok(ConstValue::int(ty, bits)),
                NumericLiteral::Float { bits, ty } => Ok(ConstValue {
                    ty,
                    kind: ConstKind::Float(bits),
                }),
            },
            ast::Expr::Char(literal) => {
                let (bits, ty) = self.char_literal(literal)?;
                Ok(ConstValue::int(ty, bits))
            }
            ast::Expr::String(group) => {
                let (symbol, array) = self.string_global(group)?;
                let ty = self.decay_type(array);
                Ok(ConstValue::address(ty, Some(symbol), 0))
            }
            ast::Expr::Name(name) => {
                // A `const` generic parameter in scope is an integer constant
                // just like an enumerator, and shadows the file-scope name.
                if let Some((value, ty)) = self.lookup_const(&name.text) {
                    return Ok(ConstValue::int(ty, value as u64));
                }
                if let Some((value, _)) = self.enum_consts.get(&name.text) {
                    let ty = self.types.builtin(crate::sema::types::Builtin::Int);
                    return Ok(ConstValue::int(ty, *value as u64));
                }
                let Some(reference) = self.global_ref(&name.text) else {
                    return Err(Diagnostic::new(
                        name.span,
                        format!(
                            "'{}' is not a constant expression (local variables and calls cannot be evaluated here)",
                            name.text
                        ),
                    ));
                };
                match reference {
                    super::GlobalRef::Object(index) => {
                        let object = self.object(index);
                        if self.types.is_array(object.ty) || self.types.is_function(object.ty) {
                            let object_ty = object.ty;
                            let symbol = object.symbol.clone();
                            let ty = self.decay_type(object_ty);
                            Ok(ConstValue::address(ty, Some(symbol), 0))
                        } else {
                            Err(Diagnostic::new(
                                name.span,
                                format!("'{}' is an object, not a constant", name.text),
                            ))
                        }
                    }
                    super::GlobalRef::Function(index) => {
                        let (function_ty, symbol) = {
                            let function = self.function(index);
                            (function.ty, function.symbol.clone())
                        };
                        let ty =
                            self.types
                                .pointer_to(crate::sema::types::QualifiedType::unqualified(
                                    function_ty,
                                ));
                        Ok(ConstValue::address(ty, Some(symbol), 0))
                    }
                }
            }
            ast::Expr::ScopedName {
                qualifier,
                name,
                span,
            } => {
                let record = self.lookup_named_type(&qualifier.text).ok_or_else(|| {
                    Diagnostic::new(
                        qualifier.span,
                        format!("unknown type name '{}'", qualifier.text),
                    )
                })?;
                // A `case Enum::Variant:` label names the tag constant, not a
                // constructed payload value.
                if let Some(value) = self
                    .types
                    .enum_variant(record, &name.text)
                    .map(|variant| variant.value)
                {
                    let underlying = self.types.enum_underlying(record).ok_or_else(|| {
                        Diagnostic::new(*span, "payload enum has no underlying type")
                    })?;
                    return Ok(ConstValue::int(underlying, value as u64));
                }
                let method = self.method_of(record, &name.text).ok_or_else(|| {
                    Diagnostic::new(name.span, format!("no method '{}' on this type", name.text))
                })?;
                let ty = self
                    .types
                    .pointer_to(crate::sema::types::QualifiedType::unqualified(method.ty));
                let _ = span;
                Ok(ConstValue::address(ty, Some(method.symbol), 0))
            }
            ast::Expr::GenericScopedName {
                qualifier,
                args,
                name,
                span,
            } => {
                let enum_ty = self.generic_qualifier_type(qualifier, args)?;
                if let Some(value) = self
                    .types
                    .enum_variant(enum_ty, &name.text)
                    .map(|variant| variant.value)
                {
                    let underlying = self.types.enum_underlying(enum_ty).ok_or_else(|| {
                        Diagnostic::new(*span, "payload enum has no underlying type")
                    })?;
                    return Ok(ConstValue::int(underlying, value as u64));
                }
                let method = self.method_on(enum_ty, name)?;
                let ty = self
                    .types
                    .pointer_to(crate::sema::types::QualifiedType::unqualified(method.ty));
                Ok(ConstValue::address(ty, Some(method.symbol), 0))
            }
            ast::Expr::BuiltinTypeCall {
                name,
                type_args,
                span,
            } => {
                let value = self.eval_builtin_type_call(&name.text, type_args, *span)?;
                let ty = self.types.builtin(crate::sema::types::Builtin::Int);
                Ok(ConstValue::int(ty, value as u64))
            }
            ast::Expr::Unary { op, operand, span } => self.eval_const_unary(*op, operand, *span),
            ast::Expr::Binary {
                op,
                left,
                right,
                span,
            } => self.eval_const_binary(*op, left, right, *span),
            ast::Expr::Conditional {
                condition,
                then_expr,
                else_expr,
                span,
            } => {
                let condition_value = self.eval_const(condition)?;
                let choose_then = self.const_truth(&condition_value);
                let then_ty = self.type_of(then_expr, &mut |_| None)?;
                let else_ty = self.type_of(else_expr, &mut |_| None)?;
                let result_ty =
                    self.conditional_type(then_ty, else_ty, then_expr, else_expr, *span)?;
                let value = if choose_then {
                    self.eval_const(then_expr)?
                } else {
                    self.eval_const(else_expr)?
                };
                self.const_cast(value, result_ty, *span)
            }
            ast::Expr::Comma { left, right, .. } => {
                self.eval_const(left)?;
                self.eval_const(right)
            }
            ast::Expr::Cast {
                type_name,
                value,
                span,
            } => {
                let target = self.resolve_type_name(type_name)?;
                let value = self.eval_const(value)?;
                self.const_cast(value, target, *span)
            }
            ast::Expr::SizeofType { type_name, span } => {
                let ty = self.resolve_type_name(type_name)?;
                let size = self.size_of(ty, *span)?;
                let result = self.types.builtin(crate::sema::types::Builtin::UnsignedInt);
                Ok(ConstValue::int(result, size as u64))
            }
            ast::Expr::SizeofExpr { value, span } => {
                let ty = self.type_of(value, &mut |_| None)?;
                if self.types.is_function(ty) || self.types.is_void(ty) {
                    return Err(Diagnostic::new(
                        *span,
                        "`sizeof` needs a complete object type",
                    ));
                }
                let size = self.size_of(ty, *span)?;
                let result = self.types.builtin(crate::sema::types::Builtin::UnsignedInt);
                Ok(ConstValue::int(result, size as u64))
            }
            ast::Expr::AlignofType { type_name, span } => {
                let ty = self.resolve_type_name(type_name)?;
                let align = self.align_of(ty, *span)?;
                let result = self.types.builtin(crate::sema::types::Builtin::UnsignedInt);
                Ok(ConstValue::int(result, align as u64))
            }
            ast::Expr::AlignofExpr { value, span } => {
                let ty = self.type_of(value, &mut |_| None)?;
                let align = self.align_of(ty, *span)?;
                let result = self.types.builtin(crate::sema::types::Builtin::UnsignedInt);
                Ok(ConstValue::int(result, align as u64))
            }
            ast::Expr::Try { span, .. } => Err(Diagnostic::new(
                *span,
                "`?` propagation is not a constant expression",
            )),
            _ => Err(Diagnostic::new(
                expr.span(),
                "expression is not a constant expression",
            )),
        }
    }

    fn eval_const_unary(
        &mut self,
        op: ast::UnaryOp,
        operand: &ast::Expr,
        span: Span,
    ) -> Result<ConstValue, Diagnostic> {
        match op {
            ast::UnaryOp::AddressOf => {
                let (symbol, addend) = self.const_address(operand)?;
                let pointee = self.type_of(operand, &mut |_| None)?;
                let ty = self.pointer_to(pointee);
                Ok(ConstValue::address(ty, symbol, addend))
            }
            ast::UnaryOp::Plus => {
                let value = self.eval_const(operand)?;
                if self.types.is_arithmetic(value.ty) {
                    let promoted = self.integer_promotion(value.ty);
                    self.const_cast(value, promoted, span)
                } else {
                    Ok(value)
                }
            }
            ast::UnaryOp::Minus => {
                let value = self.eval_const(operand)?;
                if self.types.is_arithmetic(value.ty) {
                    let promoted = self.integer_promotion(value.ty);
                    let value = self.const_cast(value, promoted, span)?;
                    match value.kind {
                        ConstKind::Int(bits) => {
                            let width = self.int_width(value.ty);
                            Ok(ConstValue::int(
                                value.ty,
                                mask_width(bits.wrapping_neg(), width),
                            ))
                        }
                        ConstKind::Float(bits) => Ok(ConstValue {
                            ty: value.ty,
                            kind: ConstKind::Float(self.float_bits_negate(value.ty, bits)),
                        }),
                        ConstKind::Address { .. } => {
                            Err(Diagnostic::new(span, "cannot negate an address constant"))
                        }
                    }
                } else {
                    Err(Diagnostic::new(
                        span,
                        "unary `-` needs an arithmetic operand",
                    ))
                }
            }
            ast::UnaryOp::LogicalNot => {
                let value = self.eval_const(operand)?;
                let truth = self.const_truth(&value);
                let ty = self.types.builtin(crate::sema::types::Builtin::Int);
                Ok(ConstValue::int(ty, u64::from(!truth)))
            }
            ast::UnaryOp::BitNot => {
                let value = self.eval_const(operand)?;
                if !self.types.is_integer(value.ty) {
                    return Err(Diagnostic::new(span, "unary `~` needs an integer operand"));
                }
                let promoted = self.integer_promotion(value.ty);
                let value = self.const_cast(value, promoted, span)?;
                let width = self.int_width(value.ty);
                match value.kind {
                    ConstKind::Int(bits) => Ok(ConstValue::int(value.ty, mask_width(!bits, width))),
                    _ => Err(Diagnostic::new(span, "unary `~` needs an integer operand")),
                }
            }
            ast::UnaryOp::Dereference => Err(Diagnostic::new(
                span,
                "reading memory is not a constant expression",
            )),
            ast::UnaryOp::PreIncrement | ast::UnaryOp::PreDecrement => Err(Diagnostic::new(
                span,
                "increment is not a constant expression",
            )),
        }
    }

    fn eval_const_binary(
        &mut self,
        op: ast::BinaryOp,
        left: &ast::Expr,
        right: &ast::Expr,
        span: Span,
    ) -> Result<ConstValue, Diagnostic> {
        if matches!(op, ast::BinaryOp::LogicalAnd | ast::BinaryOp::LogicalOr) {
            let left = self.eval_const(left)?;
            let right = self.eval_const(right)?;
            let left = self.const_truth(&left);
            let right = self.const_truth(&right);
            let truth = if op == ast::BinaryOp::LogicalAnd {
                left && right
            } else {
                left || right
            };
            let ty = self.types.builtin(crate::sema::types::Builtin::Int);
            return Ok(ConstValue::int(ty, u64::from(truth)));
        }

        let left = self.eval_const(left)?;
        let right = self.eval_const(right)?;
        let left_ty = left.ty;
        let right_ty = right.ty;

        let pointer_operand = if matches!(left.kind, ConstKind::Address { .. }) {
            Some((&left, &right, true))
        } else if matches!(right.kind, ConstKind::Address { .. }) {
            Some((&right, &left, false))
        } else {
            None
        };
        if let Some((address, integer, address_on_left)) = pointer_operand {
            return self.const_pointer_binary(op, address, integer, address_on_left, span);
        }

        let common = self.usual_arithmetic(left_ty, right_ty, span)?;
        let left = self.const_cast(left, common, span)?;
        let right = self.const_cast(right, common, span)?;
        let width = self.int_width(common);
        let unsigned = self.is_unsigned(common);
        match (&left.kind, &right.kind) {
            (ConstKind::Float(a), ConstKind::Float(b)) => {
                let left_float = self.float_value(common, *a);
                let right_float = self.float_value(common, *b);
                let value = match op {
                    ast::BinaryOp::Add => left_float + right_float,
                    ast::BinaryOp::Subtract => left_float - right_float,
                    ast::BinaryOp::Multiply => left_float * right_float,
                    ast::BinaryOp::Divide => left_float / right_float,
                    ast::BinaryOp::Equal
                    | ast::BinaryOp::NotEqual
                    | ast::BinaryOp::Less
                    | ast::BinaryOp::LessEqual
                    | ast::BinaryOp::Greater
                    | ast::BinaryOp::GreaterEqual => {
                        let truth = match op {
                            ast::BinaryOp::Equal => left_float == right_float,
                            ast::BinaryOp::NotEqual => left_float != right_float,
                            ast::BinaryOp::Less => left_float < right_float,
                            ast::BinaryOp::LessEqual => left_float <= right_float,
                            ast::BinaryOp::Greater => left_float > right_float,
                            _ => left_float >= right_float,
                        };
                        let ty = self.types.builtin(crate::sema::types::Builtin::Int);
                        return Ok(ConstValue::int(ty, u64::from(truth)));
                    }
                    _ => {
                        return Err(Diagnostic::new(
                            span,
                            "invalid operator for floating constants",
                        ));
                    }
                };
                Ok(ConstValue {
                    ty: common,
                    kind: ConstKind::Float(self.float_bits(common, value)),
                })
            }
            (ConstKind::Int(a), ConstKind::Int(b)) => {
                let result = match op {
                    ast::BinaryOp::Add => a.wrapping_add(*b),
                    ast::BinaryOp::Subtract => a.wrapping_sub(*b),
                    ast::BinaryOp::Multiply => a.wrapping_mul(*b),
                    ast::BinaryOp::Divide => {
                        if *b == 0 {
                            return Err(Diagnostic::new(
                                span,
                                "division by zero in a constant expression",
                            ));
                        }
                        if unsigned {
                            a / b
                        } else {
                            (sign_extend(*a, width) / sign_extend(*b, width)) as u64
                        }
                    }
                    ast::BinaryOp::Remainder => {
                        if *b == 0 {
                            return Err(Diagnostic::new(
                                span,
                                "remainder by zero in a constant expression",
                            ));
                        }
                        if unsigned {
                            a % b
                        } else {
                            (sign_extend(*a, width) % sign_extend(*b, width)) as u64
                        }
                    }
                    ast::BinaryOp::BitAnd => a & b,
                    ast::BinaryOp::BitOr => a | b,
                    ast::BinaryOp::BitXor => a ^ b,
                    ast::BinaryOp::ShiftLeft | ast::BinaryOp::ShiftRight => {
                        let amount = *b;
                        if amount >= u64::from(width) {
                            return Err(Diagnostic::new(
                                span,
                                "shift amount is out of range in a constant expression",
                            ));
                        }
                        if op == ast::BinaryOp::ShiftLeft {
                            (a << amount) & mask_width(u64::MAX, width)
                        } else if unsigned {
                            a >> amount
                        } else {
                            ((sign_extend(*a, width) >> amount) as u64)
                                & mask_width(u64::MAX, width)
                        }
                    }
                    ast::BinaryOp::Equal
                    | ast::BinaryOp::NotEqual
                    | ast::BinaryOp::Less
                    | ast::BinaryOp::LessEqual
                    | ast::BinaryOp::Greater
                    | ast::BinaryOp::GreaterEqual => {
                        let truth = match (unsigned, op) {
                            (_, ast::BinaryOp::Equal) => a == b,
                            (_, ast::BinaryOp::NotEqual) => a != b,
                            (true, ast::BinaryOp::Less) => a < b,
                            (true, ast::BinaryOp::LessEqual) => a <= b,
                            (true, ast::BinaryOp::Greater) => a > b,
                            (true, ast::BinaryOp::GreaterEqual) => a >= b,
                            (false, ast::BinaryOp::Less) => {
                                sign_extend(*a, width) < sign_extend(*b, width)
                            }
                            (false, ast::BinaryOp::LessEqual) => {
                                sign_extend(*a, width) <= sign_extend(*b, width)
                            }
                            (false, ast::BinaryOp::Greater) => {
                                sign_extend(*a, width) > sign_extend(*b, width)
                            }
                            (false, ast::BinaryOp::GreaterEqual) => {
                                sign_extend(*a, width) >= sign_extend(*b, width)
                            }
                            _ => unreachable!(),
                        };
                        let ty = self.types.builtin(crate::sema::types::Builtin::Int);
                        return Ok(ConstValue::int(ty, u64::from(truth)));
                    }
                    ast::BinaryOp::LogicalAnd | ast::BinaryOp::LogicalOr => unreachable!(),
                };
                Ok(ConstValue::int(common, mask_width(result, width)))
            }
            (ConstKind::Address { .. }, _) | (_, ConstKind::Address { .. }) => {
                Err(Diagnostic::new(span, "invalid operands to binary operator"))
            }
            _ => Err(Diagnostic::new(span, "invalid operands to binary operator")),
        }
    }

    fn const_pointer_binary(
        &mut self,
        op: ast::BinaryOp,
        address: &ConstValue,
        integer: &ConstValue,
        address_on_left: bool,
        span: Span,
    ) -> Result<ConstValue, Diagnostic> {
        let ConstKind::Address { symbol, addend } = &address.kind else {
            unreachable!();
        };
        if let ConstKind::Address {
            symbol: other_symbol,
            addend: other_addend,
        } = &integer.kind
        {
            let same_symbol = match (symbol, other_symbol) {
                (Some(a), Some(b)) => a == b,
                (None, None) => true,
                _ => false,
            };
            if !same_symbol {
                return Err(Diagnostic::new(
                    span,
                    "arithmetic between unrelated address constants is not constant",
                ));
            }
            let pointee = self.pointee_of_value(address, span)?;
            let size = self.size_of(pointee, span)?.max(1) as i64;
            return match op {
                ast::BinaryOp::Subtract if address_on_left => {
                    let difference = addend - other_addend;
                    let ty = self.types.builtin(crate::sema::types::Builtin::Int);
                    Ok(ConstValue::int(ty, (difference / size) as u64))
                }
                ast::BinaryOp::Equal
                | ast::BinaryOp::NotEqual
                | ast::BinaryOp::Less
                | ast::BinaryOp::LessEqual
                | ast::BinaryOp::Greater
                | ast::BinaryOp::GreaterEqual => {
                    let truth = match op {
                        ast::BinaryOp::Equal => addend == other_addend,
                        ast::BinaryOp::NotEqual => addend != other_addend,
                        ast::BinaryOp::Less => addend < other_addend,
                        ast::BinaryOp::LessEqual => addend <= other_addend,
                        ast::BinaryOp::Greater => addend > other_addend,
                        _ => addend >= other_addend,
                    };
                    let ty = self.types.builtin(crate::sema::types::Builtin::Int);
                    Ok(ConstValue::int(ty, u64::from(truth)))
                }
                _ => Err(Diagnostic::new(
                    span,
                    "invalid operator between address constants",
                )),
            };
        }
        let ConstKind::Int(bits) = &integer.kind else {
            return Err(Diagnostic::new(
                span,
                "pointer arithmetic needs an integer constant",
            ));
        };
        let bits = *bits;
        let pointee = self.pointee_of_value(address, span)?;
        let size = self.size_of(pointee, span)?.max(1) as i64;
        let index = sign_extend(bits, self.int_width(integer.ty));
        let scaled = index
            .checked_mul(size)
            .ok_or_else(|| Diagnostic::new(span, "address constant overflow"))?;
        match op {
            ast::BinaryOp::Add => Ok(ConstValue::address(
                address.ty,
                symbol.clone(),
                addend + scaled,
            )),
            ast::BinaryOp::Subtract if address_on_left => Ok(ConstValue::address(
                address.ty,
                symbol.clone(),
                addend - scaled,
            )),
            ast::BinaryOp::Equal | ast::BinaryOp::NotEqual => {
                if symbol.is_some() {
                    return Err(Diagnostic::new(
                        span,
                        "comparison of a link-time address with an integer is not constant",
                    ));
                }
                let value = *addend;
                let truth = if op == ast::BinaryOp::Equal {
                    value == scaled
                } else {
                    value != scaled
                };
                let ty = self.types.builtin(crate::sema::types::Builtin::Int);
                Ok(ConstValue::int(ty, u64::from(truth)))
            }
            _ => Err(Diagnostic::new(
                span,
                "invalid operator between a pointer and an integer",
            )),
        }
    }

    fn pointee_of_value(&self, value: &ConstValue, span: Span) -> Result<TypeId, Diagnostic> {
        match self.pointee(value.ty) {
            Some(pointee) if self.types.is_complete(pointee.ty) => Ok(pointee.ty),
            _ => Err(Diagnostic::new(
                span,
                "pointer arithmetic needs a pointer to a complete object type",
            )),
        }
    }

    // -----------------------------------------------------------------------
    // Address constants
    // -----------------------------------------------------------------------

    pub(super) fn const_address(
        &mut self,
        expr: &ast::Expr,
    ) -> Result<(Option<String>, i64), Diagnostic> {
        match expr {
            ast::Expr::String(group) => {
                let (symbol, _) = self.string_global(group)?;
                Ok((Some(symbol), 0))
            }
            ast::Expr::Name(name) => match self.global_ref(&name.text) {
                Some(super::GlobalRef::Object(index)) => {
                    Ok((Some(self.object(index).symbol.clone()), 0))
                }
                Some(super::GlobalRef::Function(index)) => {
                    Ok((Some(self.function(index).symbol.clone()), 0))
                }
                None => Err(Diagnostic::new(
                    name.span,
                    format!("'{}' has no constant address", name.text),
                )),
            },
            ast::Expr::ScopedName {
                qualifier, name, ..
            } => {
                let record = self.lookup_named_type(&qualifier.text).ok_or_else(|| {
                    Diagnostic::new(
                        qualifier.span,
                        format!("unknown type name '{}'", qualifier.text),
                    )
                })?;
                let method = self.method_of(record, &name.text).ok_or_else(|| {
                    Diagnostic::new(name.span, format!("no method '{}' on this type", name.text))
                })?;
                Ok((Some(method.symbol), 0))
            }
            ast::Expr::Unary {
                op: ast::UnaryOp::Dereference,
                operand,
                ..
            } => {
                let value = self.eval_const(operand)?;
                match value.kind {
                    ConstKind::Address { symbol, addend } => Ok((symbol, addend)),
                    _ => Err(Diagnostic::new(
                        expr.span(),
                        "dereference of a non-address constant",
                    )),
                }
            }
            ast::Expr::Index { array, index, span } => {
                let base = self.eval_const(array)?;
                let base_ty = base.ty;
                let element = self
                    .array_element(base_ty)
                    .map(|element| element.ty)
                    .or_else(|| self.pointee(base_ty).map(|pointee| pointee.ty));
                let ConstKind::Address { symbol, addend } = base.kind else {
                    return Err(Diagnostic::new(
                        *span,
                        "array element does not have a constant address",
                    ));
                };
                let Some(element) = element else {
                    return Err(Diagnostic::new(*span, "subscripted value is not an array"));
                };
                let size = self.size_of(element, *span)? as i64;
                let index = self.eval_const_int(index)?;
                Ok((symbol, addend + index * size))
            }
            ast::Expr::Member {
                object,
                name,
                op,
                span,
            } => {
                let object_ty = self.type_of(object, &mut |_| None)?;
                let (symbol, base_addend) = match self.types.kind(object_ty) {
                    TypeKind::Record { .. } => self.const_address(object)?,
                    TypeKind::Pointer { .. } => {
                        let value = self.eval_const(object)?;
                        let ConstKind::Address { symbol, addend } = value.kind else {
                            return Err(Diagnostic::new(
                                *span,
                                "member base is not an address constant",
                            ));
                        };
                        (symbol, addend)
                    }
                    _ => {
                        return Err(Diagnostic::new(
                            *span,
                            "member access needs a struct or union",
                        ));
                    }
                };
                let record = self.record_of(object_ty).ok_or_else(|| {
                    Diagnostic::new(*span, "member access needs a struct or union")
                })?;
                let (_, offset) = self.field_of(record, &name.text, *span)?;
                if *op == ast::MemberOp::Arrow && !self.types.is_pointer(object_ty) {
                    return Err(Diagnostic::new(
                        *span,
                        "`->` needs a pointer to a struct or union",
                    ));
                }
                Ok((symbol, base_addend + i64::from(offset)))
            }
            ast::Expr::Cast {
                value,
                type_name,
                span,
            } => {
                let target = self.resolve_type_name(type_name)?;
                if !self.types.is_pointer(target) {
                    return Err(Diagnostic::new(
                        *span,
                        "cast of an address constant to a non-pointer type is not constant",
                    ));
                }
                self.const_address(value)
            }
            ast::Expr::Comma { right, .. } => self.const_address(right),
            ast::Expr::Try { span, .. } => Err(Diagnostic::new(
                *span,
                "`?` propagation does not have a constant address",
            )),
            ast::Expr::Conditional {
                condition,
                then_expr,
                else_expr,
                ..
            } => {
                let condition = self.eval_const(condition)?;
                if self.const_truth(&condition) {
                    self.const_address(then_expr)
                } else {
                    self.const_address(else_expr)
                }
            }
            _ => Err(Diagnostic::new(
                expr.span(),
                "expression does not have a constant address",
            )),
        }
    }

    // -----------------------------------------------------------------------
    // Conversions and small helpers
    // -----------------------------------------------------------------------

    pub(super) fn const_cast(
        &self,
        value: ConstValue,
        target: TypeId,
        span: Span,
    ) -> Result<ConstValue, Diagnostic> {
        if value.ty == target {
            return Ok(value);
        }
        if self.types.is_bool(target) {
            let truth = self.const_truth(&value);
            return Ok(ConstValue::int(target, u64::from(truth)));
        }
        match value.kind {
            ConstKind::Int(bits) => {
                if self.types.is_integer(target) {
                    // C conversion preserves the value: widening a signed
                    // constant sign-extends (`case -1:` of a `long` switch is
                    // all ones, not 0xffffffff), while widening an unsigned
                    // one zero-extends. Narrowing just truncates either way.
                    let bits = if self.is_unsigned(value.ty) {
                        mask_width(bits, self.int_width(target))
                    } else {
                        let sign_extended = sign_extend(bits, self.int_width(value.ty)) as u64;
                        mask_width(sign_extended, self.int_width(target))
                    };
                    Ok(ConstValue::int(target, bits))
                } else if self.types.is_floating(target) {
                    let value = if self.is_unsigned(value.ty) {
                        bits as f64
                    } else {
                        sign_extend(bits, self.int_width(value.ty)) as f64
                    };
                    Ok(ConstValue {
                        ty: target,
                        kind: ConstKind::Float(self.float_bits(target, value)),
                    })
                } else if self.types.is_pointer(target) {
                    let bits = mask_width(bits, self.pointer_width() * 8);
                    Ok(ConstValue::address(target, None, bits as i64))
                } else {
                    Err(Diagnostic::new(
                        span,
                        "cannot convert an integer constant to this type",
                    ))
                }
            }
            ConstKind::Float(bits) => {
                let value = self.float_value(value.ty, bits);
                if self.types.is_floating(target) {
                    Ok(ConstValue {
                        ty: target,
                        kind: ConstKind::Float(self.float_bits(target, value)),
                    })
                } else if self.types.is_integer(target) {
                    let integer = if self.is_unsigned(target) {
                        value as u64
                    } else {
                        value as i64 as u64
                    };
                    Ok(ConstValue::int(
                        target,
                        mask_width(integer, self.int_width(target)),
                    ))
                } else {
                    Err(Diagnostic::new(
                        span,
                        "cannot convert a floating constant to this type",
                    ))
                }
            }
            ConstKind::Address { symbol, addend } => {
                if self.types.is_pointer(target) {
                    Ok(ConstValue::address(target, symbol, addend))
                } else if self.types.is_integer(target) {
                    if symbol.is_some() {
                        return Err(Diagnostic::new(
                            span,
                            "a link-time address cannot be converted to an integer at compile time",
                        ));
                    }
                    Ok(ConstValue::int(
                        target,
                        mask_width(addend as u64, self.int_width(target)),
                    ))
                } else {
                    Err(Diagnostic::new(
                        span,
                        "cannot convert an address constant to this type",
                    ))
                }
            }
        }
    }

    pub(super) fn const_truth(&self, value: &ConstValue) -> bool {
        match &value.kind {
            ConstKind::Int(bits) => *bits != 0,
            ConstKind::Float(bits) => self.float_value(value.ty, *bits) != 0.0,
            ConstKind::Address { symbol, addend } => symbol.is_some() || *addend != 0,
        }
    }

    pub(super) fn const_int_of_ir(
        &self,
        value: &ConstValue,
        span: Span,
    ) -> Result<ir::Constant, Diagnostic> {
        match value.kind {
            ConstKind::Int(bits) => {
                let ty = self.ir_type(value.ty);
                Ok(ir::Constant::uint(ty, bits))
            }
            _ => Err(Diagnostic::new(
                span,
                "constant must have an integer value here",
            )),
        }
    }

    fn int_width(&self, ty: TypeId) -> u32 {
        if self.types.is_bool(ty) {
            return 1;
        }
        self.types.size_of(ty).unwrap_or(4) * 8
    }

    fn pointer_width(&self) -> u32 {
        self.layout.pointer_bytes
    }

    fn float_value(&self, ty: TypeId, bits: u64) -> f64 {
        if self.types.size_of(ty) == Ok(4) {
            f64::from(f32::from_bits(bits as u32))
        } else {
            f64::from_bits(bits)
        }
    }

    fn float_bits(&self, ty: TypeId, value: f64) -> u64 {
        if self.types.size_of(ty) == Ok(4) {
            (value as f32).to_bits() as u64
        } else {
            value.to_bits()
        }
    }

    fn float_bits_negate(&self, ty: TypeId, bits: u64) -> u64 {
        if self.types.size_of(ty) == Ok(4) {
            (-f32::from_bits(bits as u32)).to_bits() as u64
        } else {
            (-f64::from_bits(bits)).to_bits()
        }
    }

    pub(super) fn eval_builtin_type_call(
        &mut self,
        name: &str,
        type_args: &[ast::TypeName],
        span: Span,
    ) -> Result<i32, Diagnostic> {
        match name {
            "__builtin_types_compatible_p" => {
                if type_args.len() != 2 {
                    return Err(Diagnostic::new(
                        span,
                        "`__builtin_types_compatible_p` needs two type arguments",
                    ));
                }
                let left = self.resolve_type_name(&type_args[0])?;
                let right = self.resolve_type_name(&type_args[1])?;
                Ok(i32::from(self.types.is_compatible(left, right)))
            }
            "__builtin_reg_class" => {
                if type_args.len() != 1 {
                    return Err(Diagnostic::new(
                        span,
                        "`__builtin_reg_class` needs one type argument",
                    ));
                }
                let ty = self.resolve_type_name(&type_args[0])?;
                let class = if self.types.is_integer(ty) || self.types.is_pointer(ty) {
                    0
                } else if self.types.is_floating(ty) {
                    1
                } else {
                    2
                };
                Ok(class)
            }
            _ => Err(Diagnostic::new(
                span,
                format!("unsupported builtin `{name}`"),
            )),
        }
    }
}

pub(super) fn mask_width(bits: u64, width: u32) -> u64 {
    if width >= 64 {
        bits
    } else {
        bits & ((1u64 << width) - 1)
    }
}

pub(super) fn sign_extend(bits: u64, width: u32) -> i64 {
    if width >= 64 {
        bits as i64
    } else {
        let shift = 64 - width;
        ((bits << shift) as i64) >> shift
    }
}
