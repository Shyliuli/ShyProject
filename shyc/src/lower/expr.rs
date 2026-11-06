//! Expression lowering: names, unary and binary operators, assignment,
//! conversions, casts, `sizeof`, compound literals, and statement expressions.
//!
//! Every scalar operand in an [`ExprVal`] already has the IR type of its C
//! type. In particular a comparison result is first an `i1` and is immediately
//! zero-extended to `i32`, because its C type is `int`.

use super::call::MemberTarget;
use super::types::NumericLiteral;
use super::{ExprKind, ExprVal, FnLower, GlobalRef, LocalBinding, Resolved};
use crate::ast;
use crate::ir;
use crate::sema::types::{Builtin, Qualifiers, TypeId};
use crate::source::{Diagnostic, Span};

impl FnLower<'_> {
    // -----------------------------------------------------------------------
    // Emission helpers
    // -----------------------------------------------------------------------

    pub(super) fn ensure_block(&mut self) {
        if self.terminated {
            let block = self.f.alloc_block();
            self.block = block;
            self.terminated = false;
        }
    }

    pub(super) fn emit_value(&mut self, kind: ir::InstKind) -> ir::ValueId {
        self.ensure_block();
        self.f.push_value(self.block, kind)
    }

    pub(super) fn emit_statement(&mut self, kind: ir::InstKind) {
        self.ensure_block();
        self.f.push_statement(self.block, kind);
    }

    pub(super) fn terminate(&mut self, terminator: ir::Terminator) {
        self.ensure_block();
        self.f.block_mut(self.block).terminator = terminator;
        self.terminated = true;
    }

    pub(super) fn jump_to(&mut self, target: ir::BlockId) {
        self.terminate(ir::Terminator::jump(target));
    }

    pub(super) fn set_current(&mut self, block: ir::BlockId) {
        self.block = block;
        self.terminated = false;
    }

    pub(super) fn temp_slot(&mut self, ty: TypeId, name: &str) -> ir::SlotId {
        let ir_ty = self.l.ir_type(ty);
        let align = self.l.types.align_of(ty).unwrap_or(1);
        self.f.alloc_slot_aligned(name, ir_ty, align)
    }

    pub(super) fn address_value(&mut self, address: ir::Address) -> ir::Operand {
        let base = match address.base {
            ir::AddressBase::Operand(operand) => operand,
            ir::AddressBase::Slot(slot) => {
                ir::Operand::Value(self.emit_value(ir::InstKind::SlotAddr { slot }))
            }
            ir::AddressBase::Symbol(symbol) => {
                ir::Operand::Value(self.emit_value(ir::InstKind::SymbolAddr { symbol }))
            }
        };
        if address.offset == 0 {
            return base;
        }
        let offset = ir::Operand::Constant(ir::Constant::int(ir::Type::I32, address.offset));
        ir::Operand::Value(self.emit_value(ir::InstKind::PtrOffset { base, offset }))
    }

    pub(super) fn load_value(
        &mut self,
        address: ir::Address,
        ty: TypeId,
        qualifiers: Qualifiers,
    ) -> ir::Operand {
        let ir_ty = self.l.ir_type(ty);
        let value = self.emit_value(ir::InstKind::Load {
            address,
            ty: ir_ty,
            volatile: qualifiers.is_volatile,
            atomic: ir::AtomicOrder::None,
        });
        ir::Operand::Value(value)
    }

    pub(super) fn store_value(
        &mut self,
        address: ir::Address,
        operand: ir::Operand,
        ty: TypeId,
        qualifiers: Qualifiers,
    ) {
        let ir_ty = self.l.ir_type(ty);
        self.emit_statement(ir::InstKind::Store {
            address,
            value: operand,
            ty: ir_ty,
            volatile: qualifiers.is_volatile,
            atomic: ir::AtomicOrder::None,
        });
    }

    // -----------------------------------------------------------------------
    // Value conversion
    // -----------------------------------------------------------------------

    pub(super) fn rvalue_val(
        &mut self,
        value: ExprVal,
        span: Span,
    ) -> Result<(ir::Operand, TypeId), Diagnostic> {
        let ty = value.ty;
        self.warn_defer_copy_value(&value, span);
        match value.kind {
            ExprKind::Value(operand) => Ok((operand, ty)),
            ExprKind::Void => Err(Diagnostic::new(
                span,
                "void expression used where a value is required",
            )),
            ExprKind::Place(address) => {
                if self.l.types.is_array(ty) || self.l.types.is_function(ty) {
                    let pointer_ty = self.l.decay_type(ty);
                    Ok((self.address_value(address), pointer_ty))
                } else {
                    let operand = self.load_value(address, ty, value.qualifiers);
                    Ok((operand, ty))
                }
            }
        }
    }

    pub(super) fn rvalue(&mut self, expr: &ast::Expr) -> Result<(ir::Operand, TypeId), Diagnostic> {
        let value = self.lower_expr(expr)?;
        let span = expr.span();
        self.rvalue_val(value, span)
    }

    fn pointer_conversion_discards_qualifier(&self, from: TypeId, to: TypeId) -> bool {
        let (mut from, mut to) = (from, to);
        loop {
            let (Some(from_pointee), Some(to_pointee)) = (self.l.pointee(from), self.l.pointee(to))
            else {
                return false;
            };
            if (from_pointee.qualifiers.is_const && !to_pointee.qualifiers.is_const)
                || (from_pointee.qualifiers.is_volatile && !to_pointee.qualifiers.is_volatile)
            {
                return true;
            }
            from = from_pointee.ty;
            to = to_pointee.ty;
        }
    }

    pub(super) fn convert(
        &mut self,
        operand: ir::Operand,
        from: TypeId,
        to: TypeId,
        span: Span,
        explicit: bool,
    ) -> Result<ir::Operand, Diagnostic> {
        if self.l.types.is_pointer(to) && self.l.types.is_pointer(from) {
            if !explicit && self.pointer_conversion_discards_qualifier(from, to) {
                return Err(Diagnostic::new(
                    span,
                    "pointer conversion discards const or volatile qualifier",
                ));
            }
            return Ok(operand);
        }
        if self.l.types.is_compatible(from, to) {
            return Ok(operand);
        }
        if self.l.types.is_niche_enum(from) || self.l.types.is_niche_enum(to) {
            let raw_from = if self.l.types.is_niche_enum(from) {
                self.l
                    .types
                    .enum_underlying(from)
                    .expect("niche representation")
            } else {
                from
            };
            let raw_to = if self.l.types.is_niche_enum(to) {
                self.l
                    .types
                    .enum_underlying(to)
                    .expect("niche representation")
            } else {
                to
            };
            return self.convert(operand, raw_from, raw_to, span, explicit);
        }
        if self.l.types.is_void(from) || self.l.types.is_void(to) {
            return Err(Diagnostic::new(span, "cannot convert a void value"));
        }
        if self.l.types.is_aggregate(from) || self.l.types.is_aggregate(to) {
            return Err(Diagnostic::new(span, "incompatible aggregate conversion"));
        }
        if self.l.types.is_bool(to) {
            return self.boolean_operand(operand, from, span);
        }
        if self.l.types.is_integer(to) && self.l.types.is_integer(from) {
            let from_ir = self.l.ir_type(from);
            let to_ir = self.l.ir_type(to);
            let from_bits = from_ir.integer_bits().unwrap_or(32);
            let to_bits = to_ir.integer_bits().unwrap_or(32);
            if from_bits == to_bits {
                return Ok(operand);
            }
            let op = if to_bits < from_bits {
                ir::CastOp::Trunc
            } else if self.l.is_unsigned(from) {
                ir::CastOp::ZExt
            } else {
                ir::CastOp::SExt
            };
            let value = self.emit_value(ir::InstKind::Cast {
                op,
                value: operand,
                to: to_ir,
            });
            return Ok(ir::Operand::Value(value));
        }
        if self.l.types.is_floating(to) && self.l.types.is_integer(from) {
            let op = if self.l.is_unsigned(from) {
                ir::CastOp::UIToFP
            } else {
                ir::CastOp::SIToFP
            };
            let to_ir = self.l.ir_type(to);
            let value = self.emit_value(ir::InstKind::Cast {
                op,
                value: operand,
                to: to_ir,
            });
            return Ok(ir::Operand::Value(value));
        }
        if self.l.types.is_integer(to) && self.l.types.is_floating(from) {
            let op = if self.l.is_unsigned(to) {
                ir::CastOp::FPToUI
            } else {
                ir::CastOp::FPToSI
            };
            let to_ir = self.l.ir_type(to);
            let value = self.emit_value(ir::InstKind::Cast {
                op,
                value: operand,
                to: to_ir,
            });
            return Ok(ir::Operand::Value(value));
        }
        if self.l.types.is_floating(to) && self.l.types.is_floating(from) {
            let from_bits = self.l.types.size_of(from).unwrap_or(8) * 8;
            let to_bits = self.l.types.size_of(to).unwrap_or(8) * 8;
            if from_bits == to_bits {
                return Ok(operand);
            }
            let op = if to_bits < from_bits {
                ir::CastOp::FPTrunc
            } else {
                ir::CastOp::FPExt
            };
            let to_ir = self.l.ir_type(to);
            let value = self.emit_value(ir::InstKind::Cast {
                op,
                value: operand,
                to: to_ir,
            });
            return Ok(ir::Operand::Value(value));
        }
        if self.l.types.is_pointer(to) && self.l.types.is_integer(from) {
            let is_zero = matches!(
                operand,
                ir::Operand::Constant(constant) if constant.bits == 0
            );
            if !explicit && !is_zero {
                return Err(Diagnostic::new(
                    span,
                    "integer to pointer conversion requires a cast",
                ));
            }
            let bits = self.l.layout.pointer_bytes * 8;
            let integer = self.integer_to_ir_width(operand, from, bits, span)?;
            let value = self.emit_value(ir::InstKind::Cast {
                op: ir::CastOp::IntToPtr,
                value: integer,
                to: ir::Type::Ptr,
            });
            return Ok(ir::Operand::Value(value));
        }
        if self.l.types.is_integer(to) && self.l.types.is_pointer(from) {
            if !explicit {
                return Err(Diagnostic::new(
                    span,
                    "pointer to integer conversion requires a cast",
                ));
            }
            let bits = self.l.layout.pointer_bytes * 8;
            let ir_ty = if bits == 64 {
                ir::Type::I64
            } else {
                ir::Type::I32
            };
            let integer = self.emit_value(ir::InstKind::Cast {
                op: ir::CastOp::PtrToInt,
                value: operand,
                to: ir_ty,
            });
            let integer_ty = self.l.types.builtin(if bits == 64 {
                Builtin::UnsignedLongLong
            } else {
                Builtin::UnsignedInt
            });
            return self.convert(ir::Operand::Value(integer), integer_ty, to, span, true);
        }
        Err(Diagnostic::new(span, "cannot convert between these types"))
    }

    fn integer_to_ir_width(
        &mut self,
        operand: ir::Operand,
        from: TypeId,
        width: u32,
        span: Span,
    ) -> Result<ir::Operand, Diagnostic> {
        let from_bits = self.l.ir_type(from).integer_bits().unwrap_or(32);
        let target = match width {
            8 => ir::Type::I8,
            16 => ir::Type::I16,
            32 => ir::Type::I32,
            _ => ir::Type::I64,
        };
        if from_bits == width {
            return Ok(operand);
        }
        let op = if width < from_bits {
            ir::CastOp::Trunc
        } else if self.l.is_unsigned(from) {
            ir::CastOp::ZExt
        } else {
            ir::CastOp::SExt
        };
        let value = self.emit_value(ir::InstKind::Cast {
            op,
            value: operand,
            to: target,
        });
        let _ = span;
        Ok(ir::Operand::Value(value))
    }

    pub(super) fn boolean_operand(
        &mut self,
        operand: ir::Operand,
        from: TypeId,
        span: Span,
    ) -> Result<ir::Operand, Diagnostic> {
        if self.l.types.is_niche_enum(from) {
            let underlying = self
                .l
                .types
                .enum_underlying(from)
                .expect("niche representation");
            return self.boolean_operand(operand, underlying, span);
        }
        if self.l.types.is_bool(from) {
            return Ok(operand);
        }
        if self.l.types.is_integer(from) {
            let ty = self.l.ir_type(from);
            let zero = ir::Operand::Constant(ir::Constant::zero(ty));
            let value = self.emit_value(ir::InstKind::ICmp {
                pred: ir::IntPredicate::Ne,
                lhs: operand,
                rhs: zero,
                ty,
            });
            return Ok(ir::Operand::Value(value));
        }
        if self.l.types.is_pointer(from) {
            let zero = ir::Operand::Constant(ir::Constant::zero(ir::Type::Ptr));
            let value = self.emit_value(ir::InstKind::ICmp {
                pred: ir::IntPredicate::Ne,
                lhs: operand,
                rhs: zero,
                ty: ir::Type::Ptr,
            });
            return Ok(ir::Operand::Value(value));
        }
        if self.l.types.is_floating(from) {
            let ty = self.l.ir_type(from);
            let zero = ir::Operand::Constant(ir::Constant::zero(ty));
            let value = self.emit_value(ir::InstKind::FCmp {
                pred: ir::FloatPredicate::Ne,
                lhs: operand,
                rhs: zero,
                ty,
            });
            return Ok(ir::Operand::Value(value));
        }
        Err(Diagnostic::new(span, "condition needs a scalar value"))
    }

    pub(super) fn bool_to_int(&mut self, operand: ir::Operand) -> ir::Operand {
        let value = self.emit_value(ir::InstKind::Cast {
            op: ir::CastOp::ZExt,
            value: operand,
            to: ir::Type::I32,
        });
        ir::Operand::Value(value)
    }

    pub(super) fn int_type(&self) -> TypeId {
        self.l.types.builtin(Builtin::Int)
    }

    // -----------------------------------------------------------------------
    // Names
    // -----------------------------------------------------------------------

    pub(super) fn resolve_name(&mut self, name: &str, span: Span) -> Result<Resolved, Diagnostic> {
        for scope in self.scopes.iter().rev() {
            let Some(binding) = scope.bindings.get(name) else {
                continue;
            };
            return match binding {
                LocalBinding::Local(index) => Ok(Resolved::Local(*index)),
                LocalBinding::Object {
                    symbol,
                    ty,
                    qualifiers,
                } => Ok(Resolved::Object {
                    symbol: symbol.clone(),
                    ty: *ty,
                    qualifiers: *qualifiers,
                }),
                LocalBinding::Function { symbol, ty } => Ok(Resolved::Function {
                    symbol: symbol.clone(),
                    ty: *ty,
                }),
            };
        }
        // A `const` generic parameter in scope is an integer constant, and it
        // shadows a file-scope enumerator of the same name.
        if let Some((value, ty)) = self.l.lookup_const(name) {
            return Ok(Resolved::EnumConstant { value, ty });
        }
        if let Some((value, _)) = self.l.enum_consts.get(name) {
            return Ok(Resolved::EnumConstant {
                value: *value,
                ty: self.int_type(),
            });
        }
        match self.l.global_ref(name) {
            Some(GlobalRef::Object(index)) => {
                let object = self.l.object(index);
                Ok(Resolved::Object {
                    symbol: object.symbol.clone(),
                    ty: object.ty,
                    qualifiers: object.qualifiers,
                })
            }
            Some(GlobalRef::Function(index)) => {
                let function = self.l.function(index);
                Ok(Resolved::Function {
                    symbol: function.symbol.clone(),
                    ty: function.ty,
                })
            }
            None => {
                if name == "__func__" || name == "__FUNCTION__" {
                    let (symbol, ty) = self.function_name_string()?;
                    Ok(Resolved::FuncName { symbol, ty })
                } else if self.l.generic_functions.contains_key(name) {
                    Err(Diagnostic::new(
                        span,
                        format!(
                            "generic function '{name}' needs explicit generic arguments to be used as a value"
                        ),
                    ))
                } else if let Some(macro_name) = format_macro_wrapper(&name) {
                    // `println!("x")` becomes `println0("x")`, which lives in
                    // the formatting header rather than in the language.
                    Err(Diagnostic::new(
                        span,
                        format!(
                            "'{name}' comes from the '{macro_name}!' macro; add #include <std/fmt.shyh>"
                        ),
                    ))
                } else {
                    Err(Diagnostic::new(
                        span,
                        format!("use of undeclared identifier '{name}'"),
                    ))
                }
            }
        }
    }

    fn function_name_string(&mut self) -> Result<(String, TypeId), Diagnostic> {
        if let Some(cached) = &self.func_name_global {
            return Ok(cached.clone());
        }
        let name = self.l.current_function_name.clone().unwrap_or_default();
        let pair = self.l.function_name_global(&name);
        self.func_name_global = Some(pair.clone());
        Ok(pair)
    }

    fn lower_name(&mut self, name: &ast::Name) -> Result<ExprVal, Diagnostic> {
        match self.resolve_name(&name.text, name.span)? {
            Resolved::Local(index) => {
                let local = &self.locals[index];
                Ok(ExprVal {
                    ty: local.ty,
                    qualifiers: local.qualifiers,
                    kind: ExprKind::Place(ir::Address::slot(local.slot)),
                })
            }
            Resolved::Object {
                symbol,
                ty,
                qualifiers,
            } => Ok(ExprVal {
                ty,
                qualifiers,
                kind: ExprKind::Place(ir::Address::symbol(symbol)),
            }),
            Resolved::Function { symbol, ty } => Ok(ExprVal {
                ty,
                qualifiers: Qualifiers::NONE,
                kind: ExprKind::Place(ir::Address::symbol(symbol)),
            }),
            Resolved::EnumConstant { value, ty } => Ok(ExprVal {
                ty,
                qualifiers: Qualifiers::NONE,
                kind: ExprKind::Value(ir::Operand::Constant(ir::Constant::uint(
                    self.l.ir_type(ty),
                    value as u64,
                ))),
            }),
            Resolved::FuncName { symbol, ty } => Ok(ExprVal {
                ty,
                qualifiers: Qualifiers::CONST,
                kind: ExprKind::Place(ir::Address::symbol(symbol)),
            }),
        }
    }

    // -----------------------------------------------------------------------
    // Dispatch
    // -----------------------------------------------------------------------

    pub(super) fn lower_expr(&mut self, expr: &ast::Expr) -> Result<ExprVal, Diagnostic> {
        match expr {
            ast::Expr::Number(literal) => match self.l.number_literal(literal)? {
                NumericLiteral::Integer { bits, ty } => Ok(ExprVal {
                    ty,
                    qualifiers: Qualifiers::NONE,
                    kind: ExprKind::Value(ir::Operand::Constant(ir::Constant::uint(
                        self.l.ir_type(ty),
                        bits,
                    ))),
                }),
                NumericLiteral::Float { bits, ty } => Ok(ExprVal {
                    ty,
                    qualifiers: Qualifiers::NONE,
                    kind: ExprKind::Value(ir::Operand::Constant(ir::Constant::float(
                        self.l.ir_type(ty),
                        bits,
                    ))),
                }),
            },
            ast::Expr::Char(literal) => {
                let (bits, ty) = self.l.char_literal(literal)?;
                Ok(ExprVal {
                    ty,
                    qualifiers: Qualifiers::NONE,
                    kind: ExprKind::Value(ir::Operand::Constant(ir::Constant::uint(
                        self.l.ir_type(ty),
                        bits,
                    ))),
                })
            }
            ast::Expr::String(group) => {
                let (symbol, ty) = self.l.string_global(group)?;
                Ok(ExprVal {
                    ty,
                    qualifiers: Qualifiers::NONE,
                    kind: ExprKind::Place(ir::Address::symbol(symbol)),
                })
            }
            ast::Expr::Name(name) => self.lower_name(name),
            ast::Expr::ScopedName {
                qualifier,
                name,
                span,
            } => {
                if let Some(variant) = self.l.scoped_variant(qualifier, name) {
                    if variant.payload.is_some() && !self.l.types.is_niche_enum(variant.ty) {
                        return self.variant_tag_value(&variant, *span);
                    }
                    return self.construct_payload_enum(qualifier, name, variant, &[], *span);
                }
                let method = self.l.scoped_method(qualifier, name)?;
                Ok(ExprVal {
                    ty: method.ty,
                    qualifiers: Qualifiers::NONE,
                    kind: ExprKind::Place(ir::Address::symbol(method.symbol)),
                })
            }
            ast::Expr::GenericScopedName {
                qualifier,
                args,
                name,
                span,
            } => {
                let ty = self.l.generic_qualifier_type(qualifier, args)?;
                if let Some(variant) = self.l.variant_of(ty, name) {
                    if variant.payload.is_some() && !self.l.types.is_niche_enum(variant.ty) {
                        return self.variant_tag_value(&variant, *span);
                    }
                    return self.construct_payload_enum(qualifier, name, variant, &[], *span);
                }
                let method = self.l.method_on(ty, name)?;
                Ok(ExprVal {
                    ty: method.ty,
                    qualifiers: Qualifiers::NONE,
                    kind: ExprKind::Place(ir::Address::symbol(method.symbol)),
                })
            }
            ast::Expr::BuiltinTypeCall {
                name,
                type_args,
                span,
            } => {
                let value = self
                    .l
                    .eval_builtin_type_call(&name.text, type_args, *span)?;
                let ty = self.int_type();
                Ok(ExprVal {
                    ty,
                    qualifiers: Qualifiers::NONE,
                    kind: ExprKind::Value(ir::Operand::Constant(ir::Constant::int(
                        ir::Type::I32,
                        value as i64,
                    ))),
                })
            }
            ast::Expr::Unary { op, operand, span } => self.lower_unary(*op, operand, *span),
            ast::Expr::Postfix { op, operand, span } => {
                let delta = match op {
                    ast::PostfixOp::Increment => 1,
                    ast::PostfixOp::Decrement => -1,
                };
                self.lower_incdec(operand, delta, true, *span)
            }
            ast::Expr::Try { value, span } => self.lower_try(value, *span),
            ast::Expr::Binary {
                op,
                left,
                right,
                span,
            } => self.lower_binary(*op, left, right, *span),
            ast::Expr::Assignment {
                op,
                left,
                right,
                span,
            } => self.lower_assignment(*op, left, right, *span),
            ast::Expr::Conditional {
                condition,
                then_expr,
                else_expr,
                span,
            } => self.lower_conditional(condition, then_expr, else_expr, *span),
            ast::Expr::Comma { left, right, .. } => {
                self.discard(left)?;
                let value = self.lower_expr(right)?;
                self.rvalue_val(value, right.span())
                    .map(|(operand, ty)| ExprVal {
                        ty,
                        qualifiers: Qualifiers::NONE,
                        kind: ExprKind::Value(operand),
                    })
            }
            ast::Expr::Call {
                callee,
                type_args,
                args,
                span,
            } => self.lower_call(callee, type_args, args, *span),
            ast::Expr::Index { array, index, span } => self.lower_index(array, index, *span),
            ast::Expr::Member {
                object,
                name,
                op,
                span,
            } => self.lower_member(object, name, *op, *span),
            ast::Expr::Cast {
                type_name,
                value,
                span,
            } => self.lower_cast(type_name, value, *span),
            ast::Expr::SizeofType { type_name, span } => {
                let ty = self.l.resolve_type_name(type_name)?;
                let size = self.l.size_of(ty, *span)?;
                Ok(self.size_constant(size, *span))
            }
            ast::Expr::SizeofExpr { value, span } => {
                let ty = self.type_of(value)?;
                if self.l.types.is_function(ty) || self.l.types.is_void(ty) {
                    return Err(Diagnostic::new(
                        *span,
                        "`sizeof` needs a complete object type",
                    ));
                }
                let size = self.l.size_of(ty, *span)?;
                Ok(self.size_constant(size, *span))
            }
            ast::Expr::AlignofType { type_name, span } => {
                let ty = self.l.resolve_type_name(type_name)?;
                let align = self.l.align_of(ty, *span)?;
                Ok(self.size_constant(align, *span))
            }
            ast::Expr::AlignofExpr { value, span } => {
                let ty = self.type_of(value)?;
                let align = self.l.align_of(ty, *span)?;
                Ok(self.size_constant(align, *span))
            }
            ast::Expr::CompoundLiteral {
                type_name,
                init,
                span,
            } => {
                let ty = self.l.resolve_type_name(type_name)?;
                if !self.l.types.is_complete(ty) {
                    return Err(Diagnostic::new(
                        *span,
                        "compound literal needs a complete object type",
                    ));
                }
                let slot = self.temp_slot(ty, "");
                self.init_local_at(ir::Address::slot(slot), ty, init)?;
                Ok(ExprVal {
                    ty,
                    qualifiers: Qualifiers::NONE,
                    kind: ExprKind::Place(ir::Address::slot(slot)),
                })
            }
            ast::Expr::StatementExpr { block, span: _ } => match self.lower_block_value(block)? {
                Some((operand, ty)) => Ok(ExprVal {
                    ty,
                    qualifiers: Qualifiers::NONE,
                    kind: ExprKind::Value(operand),
                }),
                None => Ok(ExprVal {
                    ty: self.l.types.builtin(Builtin::Void),
                    qualifiers: Qualifiers::NONE,
                    kind: ExprKind::Void,
                }),
            },
        }
    }

    fn size_constant(&mut self, value: u32, _span: Span) -> ExprVal {
        let ty = self.l.types.builtin(Builtin::UnsignedInt);
        ExprVal {
            ty,
            qualifiers: Qualifiers::NONE,
            kind: ExprKind::Value(ir::Operand::Constant(ir::Constant::uint(
                ir::Type::I32,
                value as u64,
            ))),
        }
    }

    pub(super) fn type_of(&mut self, expr: &ast::Expr) -> Result<TypeId, Diagnostic> {
        let scopes = &self.scopes;
        let locals = &self.locals;
        self.l.type_of(expr, &mut |name| {
            scopes
                .iter()
                .rev()
                .find_map(|scope| match scope.bindings.get(name)? {
                    LocalBinding::Local(index) => Some(locals[*index].ty),
                    LocalBinding::Object { ty, .. } => Some(*ty),
                    LocalBinding::Function { ty, .. } => Some(*ty),
                })
        })
    }

    pub(super) fn discard(&mut self, expr: &ast::Expr) -> Result<(), Diagnostic> {
        let value = self.lower_expr(expr)?;
        if value.qualifiers.is_volatile
            && let ExprKind::Place(address) = value.kind
        {
            self.load_value(address, value.ty, value.qualifiers);
        }
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Unary operators
    // -----------------------------------------------------------------------

    fn lower_unary(
        &mut self,
        op: ast::UnaryOp,
        operand: &ast::Expr,
        span: Span,
    ) -> Result<ExprVal, Diagnostic> {
        match op {
            ast::UnaryOp::AddressOf => {
                let value = self.lower_expr(operand)?;
                let pointee = value.ty;
                match value.kind {
                    ExprKind::Place(address) => {
                        let ty = self.l.pointer_to(pointee);
                        Ok(ExprVal {
                            ty,
                            qualifiers: Qualifiers::NONE,
                            kind: ExprKind::Value(self.address_value(address)),
                        })
                    }
                    ExprKind::Value(_) => Err(Diagnostic::new(
                        span,
                        "cannot take the address of an rvalue",
                    )),
                    ExprKind::Void => Err(Diagnostic::new(
                        span,
                        "cannot take the address of a void expression",
                    )),
                }
            }
            ast::UnaryOp::Dereference => {
                let (operand, pointer_ty) = self.rvalue(operand)?;
                let Some(pointee) = self.l.pointee(pointer_ty) else {
                    return Err(Diagnostic::new(span, "unary `*` needs a pointer operand"));
                };
                Ok(ExprVal {
                    ty: pointee.ty,
                    qualifiers: pointee.qualifiers,
                    kind: ExprKind::Place(super::address_operand(operand)),
                })
            }
            ast::UnaryOp::Plus => {
                let (operand, ty) = self.rvalue(operand)?;
                if !self.l.types.is_arithmetic(ty) {
                    return Err(Diagnostic::new(
                        span,
                        "unary `+` needs an arithmetic operand",
                    ));
                }
                let promoted = self.l.integer_promotion(ty);
                let operand = self.convert(operand, ty, promoted, span, false)?;
                Ok(ExprVal {
                    ty: promoted,
                    qualifiers: Qualifiers::NONE,
                    kind: ExprKind::Value(operand),
                })
            }
            ast::UnaryOp::Minus => {
                let (operand, ty) = self.rvalue(operand)?;
                if self.l.types.is_floating(ty) {
                    let value = self.emit_value(ir::InstKind::Unary {
                        op: ir::UnaryOp::FNeg,
                        value: operand,
                        ty: self.l.ir_type(ty),
                    });
                    return Ok(ExprVal {
                        ty,
                        qualifiers: Qualifiers::NONE,
                        kind: ExprKind::Value(ir::Operand::Value(value)),
                    });
                }
                if !self.l.types.is_integer(ty) {
                    return Err(Diagnostic::new(
                        span,
                        "unary `-` needs an arithmetic operand",
                    ));
                }
                let promoted = self.l.integer_promotion(ty);
                let operand = self.convert(operand, ty, promoted, span, false)?;
                let value = self.emit_value(ir::InstKind::Unary {
                    op: ir::UnaryOp::Neg,
                    value: operand,
                    ty: self.l.ir_type(promoted),
                });
                Ok(ExprVal {
                    ty: promoted,
                    qualifiers: Qualifiers::NONE,
                    kind: ExprKind::Value(ir::Operand::Value(value)),
                })
            }
            ast::UnaryOp::LogicalNot => {
                let (operand, ty) = self.rvalue(operand)?;
                let truth = self.boolean_operand(operand, ty, span)?;
                let truth = self.emit_value(ir::InstKind::Unary {
                    op: ir::UnaryOp::Not,
                    value: truth,
                    ty: ir::Type::I1,
                });
                let integer = self.bool_to_int(ir::Operand::Value(truth));
                Ok(ExprVal {
                    ty: self.int_type(),
                    qualifiers: Qualifiers::NONE,
                    kind: ExprKind::Value(integer),
                })
            }
            ast::UnaryOp::BitNot => {
                let (operand, ty) = self.rvalue(operand)?;
                if !self.l.types.is_integer(ty) {
                    return Err(Diagnostic::new(span, "unary `~` needs an integer operand"));
                }
                let promoted = self.l.integer_promotion(ty);
                let operand = self.convert(operand, ty, promoted, span, false)?;
                let value = self.emit_value(ir::InstKind::Unary {
                    op: ir::UnaryOp::Not,
                    value: operand,
                    ty: self.l.ir_type(promoted),
                });
                Ok(ExprVal {
                    ty: promoted,
                    qualifiers: Qualifiers::NONE,
                    kind: ExprKind::Value(ir::Operand::Value(value)),
                })
            }
            ast::UnaryOp::PreIncrement => self.lower_incdec(operand, 1, false, span),
            ast::UnaryOp::PreDecrement => self.lower_incdec(operand, -1, false, span),
        }
    }

    pub(super) fn lower_expr_for_write(
        &mut self,
        expression: &ast::Expr,
    ) -> Result<ExprVal, Diagnostic> {
        match expression {
            ast::Expr::Member {
                object,
                name,
                op,
                span,
            } => self.lower_member_mode(object, name, *op, *span, true),
            _ => self.lower_expr(expression),
        }
    }

    fn lower_incdec(
        &mut self,
        operand: &ast::Expr,
        delta: i32,
        postfix: bool,
        span: Span,
    ) -> Result<ExprVal, Diagnostic> {
        let value = self.lower_expr_for_write(operand)?;
        let (address, ty, qualifiers) = self.place_of(value, span)?;
        if qualifiers.is_const {
            return Err(Diagnostic::new(span, "cannot modify a const object"));
        }
        self.check_writable(&address, span)?;
        if self.l.types.is_array(ty) {
            return Err(Diagnostic::new(span, "cannot increment an array"));
        }
        let old = self.load_value(address.clone(), ty, qualifiers);
        let one = ir::Operand::Constant(ir::Constant::int(ir::Type::I32, 1));
        let binary_op = if delta > 0 {
            ast::BinaryOp::Add
        } else {
            ast::BinaryOp::Subtract
        };
        let result = self.binary_operands(binary_op, old, ty, one, self.int_type(), span)?;
        let (new_value, new_ty) = self.rvalue_val(result, span)?;
        let new_value = self.convert(new_value, new_ty, ty, span, false)?;
        self.store_value(address, new_value, ty, qualifiers);
        Ok(ExprVal {
            ty,
            qualifiers: Qualifiers::NONE,
            kind: ExprKind::Value(if postfix { old } else { new_value }),
        })
    }

    // -----------------------------------------------------------------------
    // Binary operators
    // -----------------------------------------------------------------------

    fn lower_binary(
        &mut self,
        op: ast::BinaryOp,
        left: &ast::Expr,
        right: &ast::Expr,
        span: Span,
    ) -> Result<ExprVal, Diagnostic> {
        if matches!(op, ast::BinaryOp::LogicalAnd | ast::BinaryOp::LogicalOr) {
            return self.lower_logical(op, left, right, span);
        }
        let (left_operand, left_ty) = self.rvalue(left)?;
        let (right_operand, right_ty) = self.rvalue(right)?;
        self.binary_operands(op, left_operand, left_ty, right_operand, right_ty, span)
    }

    pub(super) fn binary_operands(
        &mut self,
        op: ast::BinaryOp,
        left: ir::Operand,
        left_ty: TypeId,
        right: ir::Operand,
        right_ty: TypeId,
        span: Span,
    ) -> Result<ExprVal, Diagnostic> {
        if is_comparison(op) {
            return self.lower_comparison(op, left, left_ty, right, right_ty, span);
        }
        if matches!(op, ast::BinaryOp::ShiftLeft | ast::BinaryOp::ShiftRight) {
            return self.lower_shift(op, left, left_ty, right, right_ty, span);
        }
        if self.l.types.is_pointer(left_ty) || self.l.types.is_pointer(right_ty) {
            return self.lower_pointer_binary(op, left, left_ty, right, right_ty, span);
        }
        let common = self.l.usual_arithmetic(left_ty, right_ty, span)?;
        let left = self.convert(left, left_ty, common, span, false)?;
        let right = self.convert(right, right_ty, common, span, false)?;
        let float = self.l.types.is_floating(common);
        let binary = match op {
            ast::BinaryOp::Add => {
                if float {
                    ir::BinaryOp::FAdd
                } else {
                    ir::BinaryOp::Add
                }
            }
            ast::BinaryOp::Subtract => {
                if float {
                    ir::BinaryOp::FSub
                } else {
                    ir::BinaryOp::Sub
                }
            }
            ast::BinaryOp::Multiply => {
                if float {
                    ir::BinaryOp::FMul
                } else {
                    ir::BinaryOp::Mul
                }
            }
            ast::BinaryOp::Divide => {
                if float {
                    ir::BinaryOp::FDiv
                } else if self.l.is_unsigned(common) {
                    ir::BinaryOp::UDiv
                } else {
                    ir::BinaryOp::SDiv
                }
            }
            ast::BinaryOp::Remainder => {
                if float {
                    return Err(Diagnostic::new(
                        span,
                        "remainder `%` needs integer operands",
                    ));
                }
                if self.l.is_unsigned(common) {
                    ir::BinaryOp::URem
                } else {
                    ir::BinaryOp::SRem
                }
            }
            ast::BinaryOp::BitAnd => {
                if float {
                    return Err(Diagnostic::new(span, "bitwise `&` needs integer operands"));
                }
                ir::BinaryOp::And
            }
            ast::BinaryOp::BitOr => {
                if float {
                    return Err(Diagnostic::new(span, "bitwise `|` needs integer operands"));
                }
                ir::BinaryOp::Or
            }
            ast::BinaryOp::BitXor => {
                if float {
                    return Err(Diagnostic::new(span, "bitwise `^` needs integer operands"));
                }
                ir::BinaryOp::Xor
            }
            _ => return Err(Diagnostic::new(span, "unsupported binary operator")),
        };
        let value = self.emit_value(ir::InstKind::Binary {
            op: binary,
            lhs: left,
            rhs: right,
            ty: self.l.ir_type(common),
        });
        Ok(ExprVal {
            ty: common,
            qualifiers: Qualifiers::NONE,
            kind: ExprKind::Value(ir::Operand::Value(value)),
        })
    }

    fn lower_comparison(
        &mut self,
        op: ast::BinaryOp,
        left: ir::Operand,
        left_ty: TypeId,
        right: ir::Operand,
        right_ty: TypeId,
        span: Span,
    ) -> Result<ExprVal, Diagnostic> {
        if self.l.types.is_pointer(left_ty) || self.l.types.is_pointer(right_ty) {
            let (left, right) =
                self.pointer_comparison_operands(left, left_ty, right, right_ty, span)?;
            let predicate = match op {
                ast::BinaryOp::Equal => ir::IntPredicate::Eq,
                ast::BinaryOp::NotEqual => ir::IntPredicate::Ne,
                ast::BinaryOp::Less => ir::IntPredicate::Ult,
                ast::BinaryOp::LessEqual => ir::IntPredicate::Ule,
                ast::BinaryOp::Greater => ir::IntPredicate::Ugt,
                ast::BinaryOp::GreaterEqual => ir::IntPredicate::Uge,
                _ => unreachable!(),
            };
            let value = self.emit_value(ir::InstKind::ICmp {
                pred: predicate,
                lhs: left,
                rhs: right,
                ty: ir::Type::Ptr,
            });
            let integer = self.bool_to_int(ir::Operand::Value(value));
            return Ok(ExprVal {
                ty: self.int_type(),
                qualifiers: Qualifiers::NONE,
                kind: ExprKind::Value(integer),
            });
        }
        if !self.l.types.is_arithmetic(left_ty) || !self.l.types.is_arithmetic(right_ty) {
            return Err(Diagnostic::new(
                span,
                "comparison needs arithmetic or pointer operands",
            ));
        }
        let common = self.l.usual_arithmetic(left_ty, right_ty, span)?;
        let left = self.convert(left, left_ty, common, span, false)?;
        let right = self.convert(right, right_ty, common, span, false)?;
        if self.l.types.is_floating(common) {
            let predicate = match op {
                ast::BinaryOp::Equal => ir::FloatPredicate::Eq,
                ast::BinaryOp::NotEqual => ir::FloatPredicate::Ne,
                ast::BinaryOp::Less => ir::FloatPredicate::Lt,
                ast::BinaryOp::LessEqual => ir::FloatPredicate::Le,
                ast::BinaryOp::Greater => ir::FloatPredicate::Gt,
                ast::BinaryOp::GreaterEqual => ir::FloatPredicate::Ge,
                _ => unreachable!(),
            };
            let value = self.emit_value(ir::InstKind::FCmp {
                pred: predicate,
                lhs: left,
                rhs: right,
                ty: self.l.ir_type(common),
            });
            let integer = self.bool_to_int(ir::Operand::Value(value));
            return Ok(ExprVal {
                ty: self.int_type(),
                qualifiers: Qualifiers::NONE,
                kind: ExprKind::Value(integer),
            });
        }
        let unsigned = self.l.is_unsigned(common);
        let predicate = match (op, unsigned) {
            (ast::BinaryOp::Equal, _) => ir::IntPredicate::Eq,
            (ast::BinaryOp::NotEqual, _) => ir::IntPredicate::Ne,
            (ast::BinaryOp::Less, true) => ir::IntPredicate::Ult,
            (ast::BinaryOp::LessEqual, true) => ir::IntPredicate::Ule,
            (ast::BinaryOp::Greater, true) => ir::IntPredicate::Ugt,
            (ast::BinaryOp::GreaterEqual, true) => ir::IntPredicate::Uge,
            (ast::BinaryOp::Less, false) => ir::IntPredicate::Slt,
            (ast::BinaryOp::LessEqual, false) => ir::IntPredicate::Sle,
            (ast::BinaryOp::Greater, false) => ir::IntPredicate::Sgt,
            (ast::BinaryOp::GreaterEqual, false) => ir::IntPredicate::Sge,
            _ => unreachable!(),
        };
        let value = self.emit_value(ir::InstKind::ICmp {
            pred: predicate,
            lhs: left,
            rhs: right,
            ty: self.l.ir_type(common),
        });
        let integer = self.bool_to_int(ir::Operand::Value(value));
        Ok(ExprVal {
            ty: self.int_type(),
            qualifiers: Qualifiers::NONE,
            kind: ExprKind::Value(integer),
        })
    }

    fn pointer_comparison_operands(
        &mut self,
        left: ir::Operand,
        left_ty: TypeId,
        right: ir::Operand,
        right_ty: TypeId,
        span: Span,
    ) -> Result<(ir::Operand, ir::Operand), Diagnostic> {
        if self.l.types.is_pointer(left_ty) && self.l.types.is_pointer(right_ty) {
            return Ok((left, right));
        }
        if self.l.types.is_pointer(left_ty)
            && self.l.types.is_integer(right_ty)
            && is_zero_operand(right)
        {
            let right = self.convert(right, right_ty, left_ty, span, false)?;
            return Ok((left, right));
        }
        if self.l.types.is_pointer(right_ty)
            && self.l.types.is_integer(left_ty)
            && is_zero_operand(left)
        {
            let left = self.convert(left, left_ty, right_ty, span, false)?;
            return Ok((left, right));
        }
        Err(Diagnostic::new(
            span,
            "comparison between a pointer and a non-zero integer",
        ))
    }

    fn lower_shift(
        &mut self,
        op: ast::BinaryOp,
        left: ir::Operand,
        left_ty: TypeId,
        right: ir::Operand,
        right_ty: TypeId,
        span: Span,
    ) -> Result<ExprVal, Diagnostic> {
        if !self.l.types.is_integer(left_ty) || !self.l.types.is_integer(right_ty) {
            return Err(Diagnostic::new(span, "shift needs integer operands"));
        }
        let left_promoted = self.l.integer_promotion(left_ty);
        let right_promoted = self.l.integer_promotion(right_ty);
        let left = self.convert(left, left_ty, left_promoted, span, false)?;
        let right = self.convert(right, right_ty, right_promoted, span, false)?;
        let binary = match op {
            ast::BinaryOp::ShiftLeft => ir::BinaryOp::Shl,
            _ if self.l.is_unsigned(left_promoted) => ir::BinaryOp::LShr,
            _ => ir::BinaryOp::AShr,
        };
        let value = self.emit_value(ir::InstKind::Binary {
            op: binary,
            lhs: left,
            rhs: right,
            ty: self.l.ir_type(left_promoted),
        });
        Ok(ExprVal {
            ty: left_promoted,
            qualifiers: Qualifiers::NONE,
            kind: ExprKind::Value(ir::Operand::Value(value)),
        })
    }

    fn lower_pointer_binary(
        &mut self,
        op: ast::BinaryOp,
        left: ir::Operand,
        left_ty: TypeId,
        right: ir::Operand,
        right_ty: TypeId,
        span: Span,
    ) -> Result<ExprVal, Diagnostic> {
        match op {
            ast::BinaryOp::Add => {
                if self.l.types.is_pointer(left_ty) && self.l.types.is_integer(right_ty) {
                    let value = self.pointer_offset(left, right, right_ty, left_ty, false, span)?;
                    Ok(ExprVal {
                        ty: left_ty,
                        qualifiers: Qualifiers::NONE,
                        kind: ExprKind::Value(value),
                    })
                } else if self.l.types.is_integer(left_ty) && self.l.types.is_pointer(right_ty) {
                    let value = self.pointer_offset(right, left, left_ty, right_ty, false, span)?;
                    Ok(ExprVal {
                        ty: right_ty,
                        qualifiers: Qualifiers::NONE,
                        kind: ExprKind::Value(value),
                    })
                } else {
                    Err(Diagnostic::new(span, "invalid pointer addition"))
                }
            }
            ast::BinaryOp::Subtract => {
                if self.l.types.is_pointer(left_ty) && self.l.types.is_integer(right_ty) {
                    let value = self.pointer_offset(left, right, right_ty, left_ty, true, span)?;
                    Ok(ExprVal {
                        ty: left_ty,
                        qualifiers: Qualifiers::NONE,
                        kind: ExprKind::Value(value),
                    })
                } else if self.l.types.is_pointer(left_ty) && self.l.types.is_pointer(right_ty) {
                    let pointee = self.l.pointee(left_ty).ok_or_else(|| {
                        Diagnostic::new(span, "pointer difference needs a pointee type")
                    })?;
                    let size = self.l.size_of(pointee.ty, span)?.max(1);
                    let int_ir = if self.l.layout.pointer_bytes == 8 {
                        ir::Type::I64
                    } else {
                        ir::Type::I32
                    };
                    let left_int = self.emit_value(ir::InstKind::Cast {
                        op: ir::CastOp::PtrToInt,
                        value: left,
                        to: int_ir,
                    });
                    let right_int = self.emit_value(ir::InstKind::Cast {
                        op: ir::CastOp::PtrToInt,
                        value: right,
                        to: int_ir,
                    });
                    let difference = self.emit_value(ir::InstKind::Binary {
                        op: ir::BinaryOp::Sub,
                        lhs: ir::Operand::Value(left_int),
                        rhs: ir::Operand::Value(right_int),
                        ty: int_ir,
                    });
                    let scaled = if size != 1 {
                        let divisor =
                            ir::Operand::Constant(ir::Constant::int(int_ir, i64::from(size)));
                        self.emit_value(ir::InstKind::Binary {
                            op: ir::BinaryOp::SDiv,
                            lhs: ir::Operand::Value(difference),
                            rhs: divisor,
                            ty: int_ir,
                        })
                    } else {
                        difference
                    };
                    let int_ty = self.int_type();
                    let result = if int_ir == ir::Type::I64 {
                        let value = self.emit_value(ir::InstKind::Cast {
                            op: ir::CastOp::Trunc,
                            value: ir::Operand::Value(scaled),
                            to: ir::Type::I32,
                        });
                        ir::Operand::Value(value)
                    } else {
                        ir::Operand::Value(scaled)
                    };
                    Ok(ExprVal {
                        ty: int_ty,
                        qualifiers: Qualifiers::NONE,
                        kind: ExprKind::Value(result),
                    })
                } else {
                    Err(Diagnostic::new(span, "invalid pointer subtraction"))
                }
            }
            _ => Err(Diagnostic::new(
                span,
                "invalid operator for pointer operands",
            )),
        }
    }

    /// The record an object denotes and its address.
    ///
    /// ShyC dereferences automatically, so a member access on a pointer reaches
    /// the record through any number of pointer levels: `p.b` on `T ***p` is
    /// `(*(*(*p))).b`.
    pub(super) fn record_and_address(
        &mut self,
        object_value: ExprVal,
        span: Span,
    ) -> Result<(TypeId, ir::Address, Qualifiers), Diagnostic> {
        let mut object_qualifiers = object_value.qualifiers;
        let mut ty = object_value.ty;
        let mut value = object_value;
        while !self.l.types.is_record(ty) {
            let Some(pointee) = self.l.pointee(ty) else {
                return Err(Diagnostic::new(
                    span,
                    "member access needs a struct or union",
                ));
            };
            // A pointer to a const record makes the field const too.
            object_qualifiers.is_const |= pointee.qualifiers.is_const;
            object_qualifiers.is_volatile |= pointee.qualifiers.is_volatile;
            let (operand, _) = self.rvalue_val(value, span)?;
            let address = super::address_operand(operand);
            if self.l.types.is_record(pointee.ty) {
                return Ok((pointee.ty, address, object_qualifiers));
            }
            // Still a pointer: load it and continue.
            let loaded = self.load_value(address, pointee.ty, Qualifiers::NONE);
            ty = pointee.ty;
            value = ExprVal {
                ty,
                qualifiers: Qualifiers::NONE,
                kind: ExprKind::Value(loaded),
            };
        }
        Ok((ty, self.object_address(value, span)?, object_qualifiers))
    }

    fn pointer_offset(
        &mut self,
        base: ir::Operand,
        index: ir::Operand,
        index_ty: TypeId,
        pointer_ty: TypeId,
        negative: bool,
        span: Span,
    ) -> Result<ir::Operand, Diagnostic> {
        let pointee = self
            .l
            .pointee(pointer_ty)
            .ok_or_else(|| Diagnostic::new(span, "pointer arithmetic needs a pointee type"))?;
        // GNU C treats arithmetic on `void *` as arithmetic on `char *`, and
        // every C library header relies on it (`(void *)(p + n)`).
        let size = if self.l.types.is_void(pointee.ty) {
            1
        } else {
            self.l.size_of(pointee.ty, span)?
        };
        if size == 0 {
            return Err(Diagnostic::new(
                span,
                "pointer arithmetic on a zero-sized type",
            ));
        }
        let int_ty = self.int_type();
        let index = self.convert(index, index_ty, int_ty, span, false)?;
        let scaled = if size == 1 {
            index
        } else {
            let size_operand =
                ir::Operand::Constant(ir::Constant::int(ir::Type::I32, i64::from(size)));
            ir::Operand::Value(self.emit_value(ir::InstKind::Binary {
                op: ir::BinaryOp::Mul,
                lhs: index,
                rhs: size_operand,
                ty: ir::Type::I32,
            }))
        };
        let offset = if negative {
            ir::Operand::Value(self.emit_value(ir::InstKind::Unary {
                op: ir::UnaryOp::Neg,
                value: scaled,
                ty: ir::Type::I32,
            }))
        } else {
            scaled
        };
        Ok(ir::Operand::Value(
            self.emit_value(ir::InstKind::PtrOffset { base, offset }),
        ))
    }

    fn lower_logical(
        &mut self,
        op: ast::BinaryOp,
        left: &ast::Expr,
        right: &ast::Expr,
        span: Span,
    ) -> Result<ExprVal, Diagnostic> {
        let (left_operand, left_ty) = self.rvalue(left)?;
        let condition = self.boolean_operand(left_operand, left_ty, span)?;
        let int_ty = self.int_type();
        let result_slot = self.temp_slot(int_ty, "");
        let right_block = self.f.alloc_block();
        let short_block = self.f.alloc_block();
        let end_block = self.f.alloc_block();
        let (then_block, else_block) = if op == ast::BinaryOp::LogicalAnd {
            (right_block, short_block)
        } else {
            (short_block, right_block)
        };
        self.terminate(ir::Terminator::branch(condition, then_block, else_block));

        self.set_current(right_block);
        let (right_operand, right_ty) = self.rvalue(right)?;
        let condition = self.boolean_operand(right_operand, right_ty, span)?;
        let value = self.bool_to_int(condition);
        self.store_value(
            ir::Address::slot(result_slot),
            value,
            int_ty,
            Qualifiers::NONE,
        );
        self.jump_to(end_block);

        self.set_current(short_block);
        let value = ir::Operand::Constant(ir::Constant::int(
            ir::Type::I32,
            i64::from(op == ast::BinaryOp::LogicalOr),
        ));
        self.store_value(
            ir::Address::slot(result_slot),
            value,
            int_ty,
            Qualifiers::NONE,
        );
        self.jump_to(end_block);

        self.set_current(end_block);
        let result = self.load_value(ir::Address::slot(result_slot), int_ty, Qualifiers::NONE);
        Ok(ExprVal {
            ty: int_ty,
            qualifiers: Qualifiers::NONE,
            kind: ExprKind::Value(result),
        })
    }

    fn lower_conditional(
        &mut self,
        condition: &ast::Expr,
        then_expr: &ast::Expr,
        else_expr: &ast::Expr,
        span: Span,
    ) -> Result<ExprVal, Diagnostic> {
        let then_ty = self.type_of(then_expr)?;
        let else_ty = self.type_of(else_expr)?;
        let result_ty = self
            .l
            .conditional_type(then_ty, else_ty, then_expr, else_expr, span)?;
        let (condition_operand, condition_ty) = self.rvalue(condition)?;
        let condition_operand = self.boolean_operand(condition_operand, condition_ty, span)?;
        let then_block = self.f.alloc_block();
        let else_block = self.f.alloc_block();
        let end_block = self.f.alloc_block();
        self.terminate(ir::Terminator::branch(
            condition_operand,
            then_block,
            else_block,
        ));
        let void = self.l.types.is_void(result_ty);
        let slot = if void {
            None
        } else {
            Some(self.temp_slot(result_ty, ""))
        };
        self.set_current(then_block);
        self.lower_branch(then_expr, result_ty, slot, span)?;
        self.jump_to(end_block);
        self.set_current(else_block);
        self.lower_branch(else_expr, result_ty, slot, span)?;
        self.jump_to(end_block);
        self.set_current(end_block);
        if void {
            return Ok(ExprVal {
                ty: result_ty,
                qualifiers: Qualifiers::NONE,
                kind: ExprKind::Void,
            });
        }
        let slot = slot.expect("non-void conditional has a result slot");
        let operand = self.load_value(ir::Address::slot(slot), result_ty, Qualifiers::NONE);
        Ok(ExprVal {
            ty: result_ty,
            qualifiers: Qualifiers::NONE,
            kind: ExprKind::Value(operand),
        })
    }

    fn lower_branch(
        &mut self,
        expr: &ast::Expr,
        result_ty: TypeId,
        slot: Option<ir::SlotId>,
        span: Span,
    ) -> Result<(), Diagnostic> {
        let Some(slot) = slot else {
            self.discard(expr)?;
            return Ok(());
        };
        let (operand, ty) = self.rvalue(expr)?;
        let operand = self.convert(operand, ty, result_ty, span, false)?;
        self.store_value(
            ir::Address::slot(slot),
            operand,
            result_ty,
            Qualifiers::NONE,
        );
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Assignment
    // -----------------------------------------------------------------------

    /// Bit-field access: the field shares a storage unit with its neighbours,
    /// so a read masks and shifts, and a write clears and sets the same bits.
    pub(super) fn bit_field_value(
        &mut self,
        address: ir::Address,
        ty: TypeId,
        width: u32,
        bit_offset: u32,
        span: Span,
    ) -> ir::Operand {
        let unit = self.load_value(address, ty, Qualifiers::NONE);
        let unit_ty = self.l.ir_type(ty);
        let bits = self.l.size_of(ty, span).unwrap_or(4) * 8;
        let mut value = unit;
        if bit_offset > 0 {
            value = self.shift_bits(value, bit_offset, ir::BinaryOp::LShr, unit_ty);
        }
        if width < bits {
            let mask = if width >= 32 {
                u64::from(u32::MAX)
            } else {
                (1u64 << width) - 1
            };
            value = self.mask_bits(value, mask, unit_ty);
        }
        if !self.l.is_unsigned(ty) && width < bits {
            // Sign-extend from the field width.
            value = self.shift_bits(value, bits - width, ir::BinaryOp::Shl, unit_ty);
            value = self.shift_bits(value, bits - width, ir::BinaryOp::AShr, unit_ty);
        }
        value
    }

    fn shift_bits(
        &mut self,
        value: ir::Operand,
        amount: u32,
        op: ir::BinaryOp,
        ty: ir::Type,
    ) -> ir::Operand {
        let amount = ir::Operand::Constant(ir::Constant::int(ty, i64::from(amount)));
        ir::Operand::Value(self.emit_value(ir::InstKind::Binary {
            op,
            lhs: value,
            rhs: amount,
            ty,
        }))
    }

    fn mask_bits(&mut self, value: ir::Operand, mask: u64, ty: ir::Type) -> ir::Operand {
        let mask = ir::Operand::Constant(ir::Constant::int(ty, mask as i64));
        ir::Operand::Value(self.emit_value(ir::InstKind::Binary {
            op: ir::BinaryOp::And,
            lhs: value,
            rhs: mask,
            ty,
        }))
    }

    /// Reject a write to the object of a `let` local that is not `mut`.
    pub(super) fn check_writable_local(&self, index: usize, span: Span) -> Result<(), Diagnostic> {
        let local = &self.locals[index];
        if !local.readonly {
            return Ok(());
        }
        Err(Diagnostic::new(
            span,
            format!(
                "cannot assign to '{}': it is a read-only `let` local; declare it as `let mut {}`",
                local.name, local.name
            ),
        ))
    }

    /// Reject a direct store whose address names a read-only `let` local.
    ///
    /// Only a slot-based address names the object itself. A store through a
    /// pointer (`*p = v`, `p->field = v`, `p[i] = v`) keeps an operand base and
    /// stays ordinary C aliasing, so `mut` never turns into ownership.
    pub(super) fn check_writable(
        &self,
        address: &ir::Address,
        span: Span,
    ) -> Result<(), Diagnostic> {
        let ir::AddressBase::Slot(slot) = &address.base else {
            return Ok(());
        };
        match self
            .locals
            .iter()
            .position(|local| local.slot == *slot && local.readonly)
        {
            Some(index) => self.check_writable_local(index, span),
            None => Ok(()),
        }
    }

    fn lower_assignment(
        &mut self,
        op: ast::AssignOp,
        left: &ast::Expr,
        right: &ast::Expr,
        span: Span,
    ) -> Result<ExprVal, Diagnostic> {
        if op == ast::AssignOp::Assign
            && let ast::Expr::Name(name) = left
            && let Ok(Resolved::Local(local)) = self.resolve_name(&name.text, name.span)
            && self.locals[local].moved_slot.is_some()
        {
            // A new value replaces the old one without dropping it; the copy is
            // still dropped at scope exit. C value semantics, plus a warning.
            self.warn_defer_copy(local, left.span());
        }
        if let ast::Expr::Member { object, name, .. } = left
            && self
                .type_of(object)
                .ok()
                .and_then(|object_ty| {
                    let record = if self.l.types.is_record(object_ty) {
                        object_ty
                    } else {
                        self.l.pointee(object_ty)?.ty
                    };
                    self.l.bit_field(record, &name.text)
                })
                .is_some()
        {
            return Err(Diagnostic::new(
                span,
                "writing a bit-field is not supported yet",
            ));
        }
        // The destination is lowered in write mode, so a projected field takes
        // the mutable DerefMut path rather than the const Deref one.
        let left_value = self.lower_expr_for_write(left)?;
        let (address, ty, qualifiers) = self.place_of(left_value, span)?;
        if qualifiers.is_const {
            return Err(Diagnostic::new(span, "cannot assign to a const object"));
        }
        self.check_writable(&address, span)?;
        if self.l.types.is_array(ty) {
            return Err(Diagnostic::new(span, "cannot assign to an array"));
        }
        if op == ast::AssignOp::Assign {
            let (operand, operand_ty) = self.rvalue(right)?;
            if self.l.types.is_aggregate(ty) {
                if !self.l.types.is_compatible(ty, operand_ty) {
                    return Err(Diagnostic::new(span, "incompatible aggregate assignment"));
                }
                self.store_value(address, operand, ty, qualifiers);
                return Ok(ExprVal {
                    ty,
                    qualifiers: Qualifiers::NONE,
                    kind: ExprKind::Value(operand),
                });
            }
            let operand = self.convert(operand, operand_ty, ty, span, false)?;
            self.store_value(address.clone(), operand, ty, qualifiers);
            return Ok(ExprVal {
                ty,
                qualifiers: Qualifiers::NONE,
                kind: ExprKind::Value(operand),
            });
        }
        let binary = assign_binary(op).expect("compound assignment has an operator");
        let old = self.load_value(address.clone(), ty, qualifiers);
        let (right_operand, right_ty) = self.rvalue(right)?;
        let result = self.binary_operands(binary, old, ty, right_operand, right_ty, span)?;
        let (result_operand, result_ty) = self.rvalue_val(result, span)?;
        let result_operand = self.convert(result_operand, result_ty, ty, span, false)?;
        self.store_value(address, result_operand, ty, qualifiers);
        Ok(ExprVal {
            ty,
            qualifiers: Qualifiers::NONE,
            kind: ExprKind::Value(result_operand),
        })
    }

    pub(super) fn place_of(
        &mut self,
        value: ExprVal,
        span: Span,
    ) -> Result<(ir::Address, TypeId, Qualifiers), Diagnostic> {
        match value.kind {
            ExprKind::Place(address) => Ok((address, value.ty, value.qualifiers)),
            ExprKind::Value(_) => Err(Diagnostic::new(span, "expression is not an lvalue")),
            ExprKind::Void => Err(Diagnostic::new(span, "void expression is not an lvalue")),
        }
    }

    /// Read a defer local's value and set its moved flag: the builtin
    /// `.move()`, and what `return name` means.
    pub(super) fn move_defer_local(&mut self, local: usize) -> ExprVal {
        let (slot, ty, qualifiers, moved) = {
            let local = &self.locals[local];
            (
                local.slot,
                local.ty,
                local.qualifiers,
                local.moved_slot.expect("defer local"),
            )
        };
        let operand = self.load_value(ir::Address::slot(slot), ty, qualifiers);
        let boolean = self.l.types.builtin(Builtin::Bool);
        let one = ir::Operand::Constant(ir::Constant::int(ir::Type::I1, 1));
        self.store_value(ir::Address::slot(moved), one, boolean, Qualifiers::NONE);
        ExprVal {
            ty,
            qualifiers,
            kind: ExprKind::Value(operand),
        }
    }

    /// Warn that one defer local is used by value.
    pub(super) fn warn_defer_copy(&mut self, local: usize, span: Span) {
        if self.allow_defer_copy {
            return;
        }
        let name = self.locals[local].name.clone();
        let message = format!(
            "'{name}' is a defer variable copied by value; it is still dropped at scope exit, so the copy may dangle. Write {name}.move(), {name}.clone() or &{name}. To silence, add #[allow(defer_copy)] to the statement, fn or impl, or #![allow(defer_copy)] to the file."
        );
        self.l.warnings.push(Diagnostic::new(span, message));
    }

    /// Warn when `value` is a whole defer local read by value.
    fn warn_defer_copy_value(&mut self, value: &ExprVal, span: Span) {
        if let ExprKind::Place(address) = &value.kind {
            let address = address.clone();
            let ty = value.ty;
            self.warn_defer_copy_address(&address, ty, span);
        }
    }

    /// Warn when `address` holds a whole defer local of type `ty`.
    ///
    /// The type check keeps a field at offset 0 (which starts at the same
    /// address) from counting as a copy of the object.
    pub(super) fn warn_defer_copy_address(
        &mut self,
        address: &ir::Address,
        ty: TypeId,
        span: Span,
    ) {
        if address.offset != 0 {
            return;
        }
        let ir::AddressBase::Slot(slot) = &address.base else {
            return;
        };
        let local = self
            .locals
            .iter()
            .position(|local| local.moved_slot.is_some() && local.slot == *slot && local.ty == ty);
        if let Some(local) = local {
            self.warn_defer_copy(local, span);
        }
    }

    // -----------------------------------------------------------------------
    // Casts, member shapes, and calls used by other modules
    // -----------------------------------------------------------------------

    fn lower_cast(
        &mut self,
        type_name: &ast::TypeName,
        value: &ast::Expr,
        span: Span,
    ) -> Result<ExprVal, Diagnostic> {
        let target = self.l.resolve_type_name(type_name)?;
        if self.l.types.is_void(target) {
            self.discard(value)?;
            return Ok(ExprVal {
                ty: target,
                qualifiers: Qualifiers::NONE,
                kind: ExprKind::Void,
            });
        }
        let (operand, from) = self.rvalue(value)?;
        let operand = self.convert(operand, from, target, span, true)?;
        Ok(ExprVal {
            ty: target,
            qualifiers: Qualifiers::NONE,
            kind: ExprKind::Value(operand),
        })
    }

    /// The caller has already lowered the object: lowering it again here made
    /// `f().field` emit `f` twice.
    pub(super) fn member_target(
        &mut self,
        object_value: ExprVal,
        name: &ast::Name,
        op: ast::MemberOp,
        span: Span,
        mutable: bool,
    ) -> Result<MemberTarget, Diagnostic> {
        let object_ty = object_value.ty;
        if op == ast::MemberOp::Arrow && !self.l.types.is_pointer(object_ty) {
            return Err(Diagnostic::new(
                span,
                "`->` needs a pointer to a struct or union",
            ));
        }
        // ShyC dereferences automatically: `p.field` on a pointer means
        // `(*p).field`, the same as `p->field`. A `Deref`/`DerefMut`
        // implementation projects one more step, so `e.len` reads the target's
        // field after calling the projection method.
        let mut object_value = object_value;
        let mut last_error = None;
        for _ in 0..super::deref::MAX_AUTO_DEREF {
            let object_volatile = object_value.qualifiers.is_volatile;
            let (record, address, object_qualifiers) =
                self.record_and_address(object_value, span)?;
            let (field_ty, offset) = match self.l.field_of(record, &name.text, name.span) {
                Ok(found) => found,
                Err(error) => {
                    let Some(step) = self.l.deref_step(record, mutable, span)? else {
                        if mutable && self.l.deref_step(record, false, span)?.is_some() {
                            return Err(Diagnostic::new(
                                span,
                                "cannot assign through Deref; implement DerefMut for the field",
                            ));
                        }
                        return Err(error);
                    };
                    let receiver = ExprVal {
                        ty: record,
                        qualifiers: object_qualifiers,
                        kind: ExprKind::Place(address),
                    };
                    let step_name = ast::Name::new(step.name, name.span);
                    object_value = self.lower_method_call_value(
                        receiver,
                        &step_name,
                        ast::MemberOp::Dot,
                        step.method,
                        &[],
                        span,
                    )?;
                    last_error = Some(error);
                    continue;
                }
            };
            let field_qualifiers = self.l.field_qualifiers(record, &name.text);
            let qualifiers = Qualifiers {
                is_const: field_qualifiers.is_const || object_qualifiers.is_const,
                is_volatile: field_qualifiers.is_volatile || object_volatile,
                is_restrict: false,
                is_atomic: false,
            };
            let address = address.offset_by(i64::from(offset));
            if mutable {
                self.check_writable(&address, span)?;
            }
            return Ok(MemberTarget {
                address,
                ty: field_ty,
                qualifiers,
            });
        }
        Err(last_error
            .unwrap_or_else(|| Diagnostic::new(span, format!("no member named '{}'", name.text))))
    }

    pub(super) fn object_address(
        &mut self,
        value: ExprVal,
        span: Span,
    ) -> Result<ir::Address, Diagnostic> {
        match value.kind {
            ExprKind::Place(address) => Ok(address),
            // A niche `Result<void, E>` is a scalar value, but `?` still reads
            // it through an address, so it spills like an aggregate.
            ExprKind::Value(operand)
                if self.l.types.is_aggregate(value.ty) || self.l.types.is_niche_enum(value.ty) =>
            {
                let slot = self.temp_slot(value.ty, "");
                self.store_value(ir::Address::slot(slot), operand, value.ty, Qualifiers::NONE);
                Ok(ir::Address::slot(slot))
            }
            _ => Err(Diagnostic::new(
                span,
                "cannot take the address of this expression",
            )),
        }
    }
}

fn is_comparison(op: ast::BinaryOp) -> bool {
    matches!(
        op,
        ast::BinaryOp::Equal
            | ast::BinaryOp::NotEqual
            | ast::BinaryOp::Less
            | ast::BinaryOp::LessEqual
            | ast::BinaryOp::Greater
            | ast::BinaryOp::GreaterEqual
    )
}

fn is_zero_operand(operand: ir::Operand) -> bool {
    matches!(operand, ir::Operand::Constant(constant) if constant.bits == 0)
}

pub(super) fn assign_binary(op: ast::AssignOp) -> Option<ast::BinaryOp> {
    match op {
        ast::AssignOp::Assign => None,
        ast::AssignOp::Add => Some(ast::BinaryOp::Add),
        ast::AssignOp::Subtract => Some(ast::BinaryOp::Subtract),
        ast::AssignOp::Multiply => Some(ast::BinaryOp::Multiply),
        ast::AssignOp::Divide => Some(ast::BinaryOp::Divide),
        ast::AssignOp::Remainder => Some(ast::BinaryOp::Remainder),
        ast::AssignOp::ShiftLeft => Some(ast::BinaryOp::ShiftLeft),
        ast::AssignOp::ShiftRight => Some(ast::BinaryOp::ShiftRight),
        ast::AssignOp::BitAnd => Some(ast::BinaryOp::BitAnd),
        ast::AssignOp::BitXor => Some(ast::BinaryOp::BitXor),
        ast::AssignOp::BitOr => Some(ast::BinaryOp::BitOr),
    }
}

/// The macro a generated wrapper name came from, as in `println3`.
fn format_macro_wrapper(name: &str) -> Option<&'static str> {
    // The statement expression a `println!` or `format!` expands to lands on
    // these, so a missing header is reported the same way.
    match name {
        "fmt_begin" | "fmt_display" | "fmt_debug" | "fmt_line" => return Some("println"),
        _ => {}
    }
    for prefix in ["println", "format", "scanln"] {
        let Some(rest) = name.strip_prefix(prefix) else {
            continue;
        };
        if !rest.is_empty() && rest.bytes().all(|byte| byte.is_ascii_digit()) {
            return Some(prefix);
        }
    }
    None
}
