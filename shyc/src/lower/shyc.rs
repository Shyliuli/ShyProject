//! Shy-specific lowering: `impl` methods, traits, RAII drops, and `asm!`
//! bindings.
//!
//! Methods are plain functions named `____Type__method`. `self` in a method
//! signature denotes the implemented record type, so `self *s` is an ordinary
//! pointer parameter. Locals whose type has a `drop` method are dropped in
//! reverse declaration order at scope exit, `return`, `break`, and `continue`.
//!
//! A `trait` is compile-time metadata only. `impl Trait for Type` checks that
//! the method set and every signature match the trait, then registers the
//! methods exactly like an ordinary `impl Type`; dispatch stays static and no
//! object or vtable is allocated.

use super::{ExprKind, ExprVal, FnLower, Resolved, ResolvedSpecs, TraitInfo};
use crate::ast;
use crate::ir;
use crate::sema::types::{RecordKind, TypeId};
use crate::source::{Diagnostic, Span};

impl super::Lowerer {
    /// Registers every `trait` declaration before any implementation is
    /// checked. A method is kept as syntax because `self` must be resolved
    /// against the type in an `impl Trait for Type` block.
    pub(super) fn register_traits(
        &mut self,
        unit: &ast::TranslationUnit,
    ) -> Result<(), Diagnostic> {
        for item in &unit.items {
            let ast::Item::Trait(declaration) = item else {
                continue;
            };
            if self.traits.contains_key(&declaration.name.text) {
                return Err(Diagnostic::new(
                    declaration.name.span,
                    format!("redefinition of trait '{}'", declaration.name.text),
                ));
            }
            for (index, method) in declaration.methods.iter().enumerate() {
                let Some(name) = &method.declarator.name else {
                    return Err(Diagnostic::new(
                        method.span,
                        "a trait method declaration needs a name",
                    ));
                };
                if !matches!(
                    method.declarator.derived.first(),
                    Some(ast::DerivedDeclarator::Function(_))
                ) {
                    return Err(Diagnostic::new(
                        method.span,
                        format!("trait member '{}' must be a method declaration", name.text),
                    ));
                }
                if method.specs.storage.is_some() {
                    return Err(Diagnostic::new(
                        method.specs.span,
                        "a trait method cannot have a storage class",
                    ));
                }
                if let Some(ast::DerivedDeclarator::Function(function)) =
                    method.declarator.derived.first()
                    && !function.generics.is_empty()
                {
                    return Err(Diagnostic::new(
                        function.span,
                        "generic trait methods are not implemented yet",
                    ));
                }
                let duplicate = declaration.methods[..index].iter().any(|previous| {
                    previous
                        .declarator
                        .name
                        .as_ref()
                        .is_some_and(|previous| previous.text == name.text)
                });
                if duplicate {
                    return Err(Diagnostic::new(
                        name.span,
                        format!("redefinition of trait method '{}'", name.text),
                    ));
                }
            }
            self.traits.insert(
                declaration.name.text.clone(),
                TraitInfo {
                    methods: declaration.methods.clone(),
                },
            );
        }
        Ok(())
    }

    /// Type-checks every trait signature after all file-scope types exist.
    ///
    /// An implementation resolves the same signatures against its own `self`,
    /// so declarations without an implementation still get their parameter and
    /// return types checked here. An empty record stands in for `self`; its
    /// identity never escapes this check.
    pub(super) fn check_trait_signatures(
        &mut self,
        unit: &ast::TranslationUnit,
    ) -> Result<(), Diagnostic> {
        if self.traits.is_empty() {
            return Ok(());
        }
        let placeholder = match self.trait_self_type {
            Some(ty) => ty,
            None => {
                let ty = self
                    .types
                    .define_record(RecordKind::Struct, Vec::new())
                    .expect("an empty record always has a layout");
                self.trait_self_type = Some(ty);
                ty
            }
        };
        let saved = self.impl_context.clone();
        self.impl_context = Some((placeholder, String::new()));
        let result = (|| {
            for item in &unit.items {
                let ast::Item::Trait(declaration) = item else {
                    continue;
                };
                for method in &declaration.methods {
                    self.trait_method_type(method).map_err(|error| {
                        Diagnostic::new(
                            error.span,
                            format!("in trait '{}': {}", declaration.name.text, error.message),
                        )
                    })?;
                }
            }
            Ok(())
        })();
        self.impl_context = saved;
        result
    }

    pub(super) fn collect_impl(&mut self, block: &ast::ImplBlock) -> Result<(), Diagnostic> {
        let ty = self.resolve_type_name(&block.self_type)?;
        if !self.types.is_record(ty) {
            return Err(Diagnostic::new(
                block.self_type.span,
                "impl target must be a struct or union type",
            ));
        }
        let name = self.impl_name(&block.self_type)?;
        let trait_info = match &block.trait_name {
            Some(trait_name) => {
                let Some(info) = self.traits.get(&trait_name.text).cloned() else {
                    return Err(Diagnostic::new(
                        trait_name.span,
                        format!("unknown trait '{}'", trait_name.text),
                    ));
                };
                if self
                    .trait_impls
                    .contains(&(trait_name.text.clone(), name.clone()))
                {
                    return Err(Diagnostic::new(
                        block.span,
                        format!(
                            "trait '{}' is already implemented for '{}'",
                            trait_name.text, name
                        ),
                    ));
                }
                self.check_impl_method_names(block, trait_name, &info, &name)?;
                Some((trait_name.clone(), info))
            }
            None => None,
        };
        let saved = self.impl_context.clone();
        self.impl_context = Some((ty, name.clone()));
        let result = (|| {
            for method in &block.methods {
                self.collect_function(method, Some((ty, name.clone())))?;
            }
            if let Some((trait_name, info)) = &trait_info {
                self.check_impl_signatures(block, trait_name, info, ty)?;
            }
            Ok(())
        })();
        self.impl_context = saved;
        if result.is_ok()
            && let Some((trait_name, _)) = &trait_info
        {
            self.trait_impls
                .insert((trait_name.text.clone(), name.clone()));
        }
        result
    }

    /// Checks that the implementation defines exactly the trait's methods.
    /// Signatures are checked later, after each method has a resolved type.
    fn check_impl_method_names(
        &self,
        block: &ast::ImplBlock,
        trait_name: &ast::Name,
        info: &TraitInfo,
        type_name: &str,
    ) -> Result<(), Diagnostic> {
        for method in &block.methods {
            let Some(name) = &method.declarator.name else {
                return Err(Diagnostic::new(
                    method.span,
                    "method definition needs a name",
                ));
            };
            let declared = info.methods.iter().any(|trait_method| {
                trait_method
                    .declarator
                    .name
                    .as_ref()
                    .is_some_and(|trait_name| trait_name.text == name.text)
            });
            if !declared {
                return Err(Diagnostic::new(
                    name.span,
                    format!(
                        "method '{}' is not declared by trait '{}'",
                        name.text, trait_name.text
                    ),
                ));
            }
        }
        for trait_method in &info.methods {
            let Some(trait_method_name) = &trait_method.declarator.name else {
                continue;
            };
            let implemented = block.methods.iter().any(|method| {
                method
                    .declarator
                    .name
                    .as_ref()
                    .is_some_and(|name| name.text == trait_method_name.text)
            });
            if !implemented {
                return Err(Diagnostic::new(
                    block.span,
                    format!(
                        "implementation of trait '{}' for '{}' is missing method '{}'",
                        trait_name.text, type_name, trait_method_name.text
                    ),
                ));
            }
        }
        Ok(())
    }

    /// Resolves each trait method against the implemented type (`self`) and
    /// compares it with the implementation's function type.
    fn check_impl_signatures(
        &mut self,
        block: &ast::ImplBlock,
        trait_name: &ast::Name,
        info: &TraitInfo,
        implemented: TypeId,
    ) -> Result<(), Diagnostic> {
        for trait_method in &info.methods {
            let Some(trait_method_name) = &trait_method.declarator.name else {
                continue;
            };
            let expected = self.trait_method_type(trait_method)?;
            let actual = self
                .impls
                .get(&implemented)
                .and_then(|info| info.methods.get(&trait_method_name.text))
                .map(|method| method.ty)
                .expect("every trait method was collected");
            if !self.types.is_compatible(expected, actual) {
                let span = block
                    .methods
                    .iter()
                    .find(|method| {
                        method
                            .declarator
                            .name
                            .as_ref()
                            .is_some_and(|name| name.text == trait_method_name.text)
                    })
                    .map_or(block.span, |method| method.declarator.span);
                return Err(Diagnostic::new(
                    span,
                    format!(
                        "method '{}' signature does not match trait '{}'",
                        trait_method_name.text, trait_name.text
                    ),
                ));
            }
        }
        Ok(())
    }

    /// The function type of a trait method with `self` bound to the
    /// implementing type currently in `impl_context`.
    fn trait_method_type(&mut self, method: &ast::TraitMethod) -> Result<TypeId, Diagnostic> {
        let ResolvedSpecs { ty, qualifiers } = self.resolve_specs(&method.specs)?;
        let resolved = self.resolve_declarator(ty, qualifiers, &method.declarator)?;
        if !self.types.is_function(resolved.ty) {
            return Err(Diagnostic::new(
                method.span,
                "trait member is not a method declaration",
            ));
        }
        Ok(resolved.ty)
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
        self.check_writable_local(local, span)?;
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
