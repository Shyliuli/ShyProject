//! Shy-specific lowering: `impl` methods, RAII drops, and `asm!` bindings.
//!
//! Methods are plain functions named `____Type__method`. `self` in a method
//! signature denotes the implemented record type, so `self *s` is an ordinary
//! pointer parameter. Locals whose type has a `drop` method are dropped in
//! reverse declaration order at scope exit, `return`, `break`, and `continue`.

use super::{ExprKind, ExprVal, FnLower, Resolved};
use crate::ast;
use crate::ir;
use crate::source::{Diagnostic, Span};

impl super::Lowerer {
    pub(super) fn collect_impl(&mut self, block: &ast::ImplBlock) -> Result<(), Diagnostic> {
        let ty = self.resolve_type_name(&block.self_type)?;
        if !self.types.is_record(ty) {
            return Err(Diagnostic::new(
                block.self_type.span,
                "impl target must be a struct or union type",
            ));
        }
        let name = self.impl_name(&block.self_type)?;
        let saved = self.impl_context.clone();
        self.impl_context = Some((ty, name.clone()));
        let result = (|| {
            for method in &block.methods {
                self.collect_function(method, Some((ty, name.clone())))?;
            }
            Ok(())
        })();
        self.impl_context = saved;
        result
    }

    fn impl_name(&self, type_name: &ast::TypeName) -> Result<String, Diagnostic> {
        if type_name.declarator.is_some() {
            return Err(Diagnostic::new(
                type_name.span,
                "impl target must be a plain type name",
            ));
        }
        for specifier in &type_name.specs.type_specs {
            match specifier {
                ast::TypeSpecifier::TypedefName(name) => return Ok(name.text.clone()),
                ast::TypeSpecifier::Record(record) => {
                    if let Some(tag) = &record.tag {
                        return Ok(tag.text.clone());
                    }
                }
                _ => {}
            }
        }
        Err(Diagnostic::new(
            type_name.span,
            "impl target must name a struct or union type",
        ))
    }
}

impl FnLower<'_> {
    /// Emit the `drop` call for one local and mark it moved.
    pub(super) fn emit_drop(&mut self, local: usize) {
        let (slot, ty, symbol, active) = {
            let local = &self.locals[local];
            (
                local.slot,
                local.ty,
                local.drop_symbol.clone(),
                local.drop_active,
            )
        };
        let Some(symbol) = symbol else {
            return;
        };
        if !active {
            return;
        }
        self.locals[local].drop_active = false;
        let argument = self.address_value(ir::Address::slot(slot));
        let (signature, abi) = match self.l.drop_method(ty) {
            Some(method) => {
                let abi =
                    crate::abi_adapter::plan_target_function(&self.l.types, method.ty, self.l.abi)
                        .unwrap_or_else(|_| {
                            abi_plan::plan(
                                self.l.abi,
                                &abi_plan::FunctionSignature {
                                    ret: abi_plan::AbiType::Void,
                                    params: vec![abi_plan::AbiType::Pointer {
                                        bits: (self.l.layout.pointer_bytes * 8) as u16,
                                    }],
                                    variadic: false,
                                },
                            )
                            .expect("a pointer parameter always plans")
                        });
                (self.l.ir_signature(method.ty), abi)
            }
            None => (
                ir::Signature::new(ir::Type::Void, vec![ir::Type::Ptr]),
                abi_plan::plan(
                    self.l.abi,
                    &abi_plan::FunctionSignature {
                        ret: abi_plan::AbiType::Void,
                        params: vec![abi_plan::AbiType::Pointer {
                            bits: (self.l.layout.pointer_bytes * 8) as u16,
                        }],
                        variadic: false,
                    },
                )
                .expect("a pointer parameter always plans"),
            ),
        };
        let call = ir::Call::direct(symbol, vec![argument], signature, abi);
        if call.signature.ret.is_void() {
            self.emit_statement(ir::InstKind::Call(Box::new(call)));
        } else {
            self.emit_value(ir::InstKind::Call(Box::new(call)));
        }
    }

    /// Shy RAII assignment: drop the old value, move the right side, store it.
    pub(super) fn raii_assign(
        &mut self,
        local: usize,
        left: &ast::Expr,
        right: &ast::Expr,
        span: Span,
    ) -> Result<ExprVal, Diagnostic> {
        if let ast::Expr::Name(name) = right
            && let Ok(Resolved::Local(other)) = self.resolve_name(&name.text, name.span)
            && other == local
        {
            return Err(Diagnostic::new(
                span,
                "cannot move an object with a drop method into itself",
            ));
        }
        let lhs_ty = self.locals[local].ty;
        if self.locals[local].drop_active {
            self.emit_drop(local);
        }
        let (operand, ty) = self.rvalue(right)?;
        if !self.l.types.is_compatible(lhs_ty, ty) {
            return Err(Diagnostic::new(
                right.span(),
                "assignment type does not match the destination",
            ));
        }
        self.deactivate_if_local(right);
        let slot = self.locals[local].slot;
        self.store_value(
            ir::Address::slot(slot),
            operand,
            lhs_ty,
            self.locals[local].qualifiers,
        );
        self.locals[local].drop_active = true;
        let _ = left;
        Ok(ExprVal {
            ty: lhs_ty,
            qualifiers: super::Qualifiers::NONE,
            kind: ExprKind::Value(operand),
        })
    }

    pub(super) fn lower_asm(&mut self, asm: &ast::AsmStmt) -> Result<(), Diagnostic> {
        match asm {
            ast::AsmStmt::Raw { templates, span } => {
                let text = self.asm_text(templates, *span)?;
                self.emit_statement(ir::InstKind::InlineAsm(ir::InlineAsm::new(text)));
                Ok(())
            }
            ast::AsmStmt::Binding {
                operands,
                templates,
                span,
            } => {
                let text = self.asm_text(templates, *span)?;
                let mut bindings = Vec::new();
                let mut names: Vec<&str> = Vec::new();
                for operand in operands {
                    if names.contains(&operand.text.as_str()) {
                        return Err(Diagnostic::new(
                            operand.span,
                            format!("asm! binding '{}' is repeated", operand.text),
                        ));
                    }
                    names.push(&operand.text);
                    let (address, ty) = self.asm_operand(&operand.text, operand.span)?;
                    let address_form = text.contains(&format!("{{&{}}}", operand.text));
                    let value_form = text.contains(&format!("{{{}}}", operand.text));
                    if address_form && value_form {
                        return Err(Diagnostic::new(
                            operand.span,
                            format!(
                                "asm! binding '{}' is used both by value and by address",
                                operand.text
                            ),
                        ));
                    }
                    let bytes = self.l.size_of(ty, operand.span)?;
                    let access = if address_form {
                        ir::AsmAccess::AddressOnly
                    } else if value_form {
                        if bytes > 8 {
                            return Err(Diagnostic::new(
                                operand.span,
                                "asm! value bindings support at most 8 bytes; use the {&name} address form",
                            ));
                        }
                        ir::AsmAccess::ReadWrite
                    } else {
                        ir::AsmAccess::ReadWrite
                    };
                    bindings.push(ir::AsmBinding::new(
                        operand.text.clone(),
                        address,
                        bytes,
                        access,
                    ));
                }
                self.emit_statement(ir::InstKind::InlineAsm(ir::InlineAsm { text, bindings }));
                Ok(())
            }
        }
    }

    fn asm_text(&self, templates: &[ast::StringLit], span: Span) -> Result<String, Diagnostic> {
        let mut text = String::new();
        for part in templates {
            let value = crate::literal::string(&part.prefix, &part.raw, part.span)?;
            if value.encoding != crate::literal::Encoding::Utf8 {
                return Err(Diagnostic::new(
                    part.span,
                    "inline assembly needs a byte string literal",
                ));
            }
            text.push_str(&String::from_utf8(value.bytes).map_err(|_| {
                Diagnostic::new(part.span, "inline assembly text is not valid UTF-8")
            })?);
        }
        let _ = span;
        Ok(text)
    }

    fn asm_operand(
        &mut self,
        name: &str,
        span: Span,
    ) -> Result<(ir::Address, super::TypeId), Diagnostic> {
        match self.resolve_name(name, span)? {
            Resolved::Local(index) => {
                let local = &self.locals[index];
                Ok((ir::Address::slot(local.slot), local.ty))
            }
            Resolved::Object { symbol, ty, .. } => Ok((ir::Address::symbol(symbol), ty)),
            Resolved::Function { symbol, ty } => Ok((ir::Address::symbol(symbol), ty)),
            Resolved::EnumConstant { .. } | Resolved::FuncName { .. } => Err(Diagnostic::new(
                span,
                format!("asm! binding '{name}' is not an object"),
            )),
        }
    }
}
