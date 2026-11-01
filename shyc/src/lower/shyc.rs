//! Shy-specific lowering: `impl` methods, traits, RAII drops, and `asm!`
//! bindings.
//!
//! An inherent method is a plain function named `____Type__method`; a trait
//! method is `____Trait__Type__method` plus one suffix part per resolved trait
//! argument, so several traits and several parameterizations of one trait can
//! implement the same method name. `self` in a method signature denotes the
//! implemented record type, so `self *s` is an ordinary pointer parameter.
//! Locals whose type has a `drop` method are dropped in reverse declaration
//! order at scope exit, `return`, `break`, and `continue`.
//!
//! A `trait` declares compile-time signatures and a transparent value shape.
//! `impl Trait for Type` checks that the method set and every signature match
//! the trait, then registers each method under its trait identity. Used as a
//! type, a trait denotes a synthetic struct with one function-pointer field per
//! method; `Trait::from<Type>()` fills it with the implementation's symbols.
//! The only erased part of a signature is `self`, which becomes `void *`, so
//! the table is plain C data with no hidden data pointer, vtable header, or
//! allocation.
//!
//! A method may declare its own type parameters. Its signature is compared
//! with the implementation after both sides bind a fresh placeholder, and each
//! concrete call instantiates the implementation as an ordinary function. Such
//! a method has no single function pointer, so a trait with a generic method
//! stays static-only and is rejected when used as a value type.

use super::{ExprKind, ExprVal, FnLower, Resolved, ResolvedSpecs, TraitInfo};
use crate::ast;
use crate::ir;
use crate::sema::types::{Builtin, QualifiedType, RecordField, RecordKind, TypeId, TypeKind};
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
            if let Some(bound) = declaration
                .generics
                .iter()
                .find_map(|parameter| parameter.bounds.first())
            {
                return Err(Diagnostic::new(
                    bound.span,
                    format!(
                        "trait bounds on generic trait '{}' are not implemented yet",
                        declaration.name.text
                    ),
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
                for (index, parameter) in method.generics.iter().enumerate() {
                    if method.generics[..index]
                        .iter()
                        .any(|previous| previous.name.text == parameter.name.text)
                    {
                        return Err(Diagnostic::new(
                            parameter.span,
                            format!(
                                "redefinition of generic method parameter '{}'",
                                parameter.name.text
                            ),
                        ));
                    }
                    if let Some(bound) = parameter.bounds.first() {
                        return Err(Diagnostic::new(
                            bound.span,
                            format!(
                                "trait bounds on generic trait method '{}' are not \
                                 implemented yet",
                                name.text
                            ),
                        ));
                    }
                    if parameter.kind == ast::GenericParamKind::Const {
                        return Err(Diagnostic::new(
                            parameter.span,
                            format!(
                                "const generic parameters on trait method '{}' are not \
                                 implemented yet",
                                name.text
                            ),
                        ));
                    }
                    if declaration
                        .generics
                        .iter()
                        .any(|trait_parameter| trait_parameter.name.text == parameter.name.text)
                    {
                        return Err(Diagnostic::new(
                            parameter.span,
                            format!(
                                "generic method parameter '{}' shadows a parameter of trait \
                                 '{}'",
                                parameter.name.text, declaration.name.text
                            ),
                        ));
                    }
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
                    params: declaration.generics.clone(),
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
                // A generic trait's signatures are checked against each
                // concrete implementation, where its parameters are bound.
                if !declaration.generics.is_empty() {
                    continue;
                }
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
        // A template impl is registered by `register_generic_impls`; only a
        // concrete receiver instantiates its methods.
        if !block.generics.is_empty() {
            return Ok(());
        }
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
                if block.trait_args.len() != info.params.len() {
                    return Err(Diagnostic::new(
                        block.span,
                        format!(
                            "trait '{}' expects {} type argument(s)",
                            trait_name.text,
                            info.params.len()
                        ),
                    ));
                }
                let args = if info.params.is_empty() {
                    Vec::new()
                } else {
                    self.resolve_generic_args(&trait_name.text, &info.params, &block.trait_args)?
                };
                if self
                    .trait_impls
                    .contains(&(trait_name.text.clone(), args.clone(), ty))
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
                Some((trait_name.clone(), info, args))
            }
            None => None,
        };
        let trait_impl = trait_info
            .as_ref()
            .map(|(trait_name, _, args)| super::TraitImplKey {
                name: trait_name.text.clone(),
                args: args.clone(),
            });
        let saved = self.impl_context.clone();
        self.impl_context = Some((ty, name.clone()));
        // The trait's concrete arguments are in scope while its method
        // signatures are compared with the implementation.
        self.push_type_scope();
        if let Some((_, info, args)) = &trait_info {
            self.bind_generic_args(&info.params, args);
        }
        let result = (|| {
            for method in &block.methods {
                self.collect_function(
                    method,
                    Some(super::ImplTarget {
                        ty,
                        name: name.clone(),
                        instance: None,
                        trait_impl: trait_impl.clone(),
                    }),
                )?;
            }
            if let Some((trait_name, info, _)) = &trait_info {
                self.check_impl_signatures(block, trait_name, info, ty, trait_impl.as_ref())?;
            }
            Ok(())
        })();
        self.pop_type_scope();
        self.impl_context = saved;
        if result.is_ok()
            && let Some((trait_name, _, args)) = &trait_info
        {
            self.trait_impls
                .insert((trait_name.text.clone(), args.clone(), ty));
            self.register_from_conversion(trait_name, args, ty)?;
        }
        result
    }

    /// Records the concrete `impl From<Source> for Target` method that `?`
    /// calls when the propagated error types differ.
    ///
    /// The conversion is ordinary trait metadata: the implementation is
    /// checked like any other, and only the resulting symbol and function type
    /// are kept. There is no compiler-invented method or hidden conversion.
    pub(super) fn register_from_conversion(
        &mut self,
        trait_name: &ast::Name,
        args: &[super::GenericArgKey],
        target: TypeId,
    ) -> Result<(), Diagnostic> {
        if trait_name.text != "From" {
            return Ok(());
        }
        let [super::GenericArgKey::Type(source)] = args else {
            return Ok(());
        };
        let key = super::TraitImplKey {
            name: trait_name.text.clone(),
            args: args.to_vec(),
        };
        let Some(method) = self.trait_method_named(target, &key, "from")? else {
            return Ok(());
        };
        let TypeKind::Function { params, result, .. } = self.types.kind(method.ty) else {
            return Ok(());
        };
        if params.len() == 1 && self.types.is_compatible(*result, target) {
            self.from_conversions.insert((*source, target), method);
        }
        Ok(())
    }

    /// Checks that the implementation defines exactly the trait's methods.
    /// Signatures are checked later, after each method has a resolved type.
    pub(super) fn check_impl_method_names(
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
    ///
    /// A generic trait method is compared after both sides bind the same
    /// placeholder arguments: only the parameter count and kinds matter at this
    /// point, because every call later instantiates the implementation with
    /// concrete arguments and checks the call against that instance.
    pub(super) fn check_impl_signatures(
        &mut self,
        block: &ast::ImplBlock,
        trait_name: &ast::Name,
        info: &TraitInfo,
        implemented: TypeId,
        trait_impl: Option<&super::TraitImplKey>,
    ) -> Result<(), Diagnostic> {
        let methods = trait_impl
            .and_then(|key| {
                self.impls
                    .get(&implemented)
                    .and_then(|info| info.trait_methods.get(key))
            })
            .cloned();
        for trait_method in &info.methods {
            let Some(trait_method_name) = &trait_method.declarator.name else {
                continue;
            };
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
            // Both signatures bind the same fresh placeholder per parameter,
            // so a concrete return type cannot hide behind a placeholder that
            // happens to be `int` on both sides.
            let placeholders = self.method_placeholder_args(&trait_method.generics)?;
            let expected = self.trait_method_type_with_args(trait_method, &placeholders)?;
            let actual = if trait_method.generics.is_empty() {
                if let Some(method) = methods
                    .as_ref()
                    .and_then(|set| set.methods.get(&trait_method_name.text))
                {
                    method.ty
                } else {
                    if methods.as_ref().is_some_and(|set| {
                        set.generic_methods.contains_key(&trait_method_name.text)
                    }) {
                        return Err(Diagnostic::new(
                            span,
                            format!(
                                "method '{}' is generic in the implementation but not in \
                                 trait '{}'",
                                trait_method_name.text, trait_name.text
                            ),
                        ));
                    }
                    return Err(Diagnostic::new(
                        span,
                        format!(
                            "method '{}' signature does not match trait '{}'",
                            trait_method_name.text, trait_name.text
                        ),
                    ));
                }
            } else {
                let Some(generic) = methods
                    .as_ref()
                    .and_then(|set| set.generic_methods.get(&trait_method_name.text))
                    .cloned()
                else {
                    return Err(Diagnostic::new(
                        span,
                        format!(
                            "method '{}' must declare {} generic parameter(s) to match \
                             trait '{}'",
                            trait_method_name.text,
                            trait_method.generics.len(),
                            trait_name.text
                        ),
                    ));
                };
                if generic.params.len() != trait_method.generics.len() {
                    return Err(Diagnostic::new(
                        span,
                        format!(
                            "method '{}' declares {} generic parameter(s), but trait '{}' \
                             declares {}",
                            trait_method_name.text,
                            generic.params.len(),
                            trait_name.text,
                            trait_method.generics.len()
                        ),
                    ));
                }
                if let Some(parameter) = generic.params.iter().find(|parameter| {
                    info.params
                        .iter()
                        .any(|trait_parameter| trait_parameter.name.text == parameter.name.text)
                }) {
                    return Err(Diagnostic::new(
                        parameter.span,
                        format!(
                            "generic method parameter '{}' shadows a parameter of trait '{}'",
                            parameter.name.text, trait_name.text
                        ),
                    ));
                }
                for (index, (trait_parameter, impl_parameter)) in trait_method
                    .generics
                    .iter()
                    .zip(&generic.params)
                    .enumerate()
                {
                    if trait_parameter.kind != impl_parameter.kind {
                        return Err(Diagnostic::new(
                            span,
                            format!(
                                "generic parameter {} of method '{}' is a {} in trait '{}' \
                                 but a {} in the implementation",
                                index + 1,
                                trait_method_name.text,
                                match trait_parameter.kind {
                                    ast::GenericParamKind::Type => "type parameter",
                                    ast::GenericParamKind::Const => "const parameter",
                                },
                                trait_name.text,
                                match impl_parameter.kind {
                                    ast::GenericParamKind::Type => "type parameter",
                                    ast::GenericParamKind::Const => "const parameter",
                                }
                            ),
                        ));
                    }
                }
                self.generic_method_signature(&generic.decl, &generic.params, &placeholders)?
            };
            if !self.types.is_compatible(expected, actual) {
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

    /// Whether `name` denotes a trait value rather than an ordinary type.
    ///
    /// A real type with the same spelling wins, matching type resolution: the
    /// trait value record is not entered in the tag or typedef tables.
    pub(super) fn is_trait_value_name(&self, name: &str) -> bool {
        self.traits.contains_key(name) && self.lookup_named_type(name).is_none()
    }

    /// The synthetic record a trait name denotes when it is used as a value.
    ///
    /// The record has one field per method, named after the method, and each
    /// field is a pointer to that method's function type with `self` erased to
    /// `void`. That is exactly one pointer per method: no hidden data pointer,
    /// no vtable header, and no allocation.
    pub(super) fn trait_value_type(
        &mut self,
        trait_name: &ast::Name,
    ) -> Result<TypeId, Diagnostic> {
        if let Some(&ty) = self.trait_values.get(&trait_name.text) {
            return Ok(ty);
        }
        let Some(info) = self.traits.get(&trait_name.text).cloned() else {
            return Err(Diagnostic::new(
                trait_name.span,
                format!("unknown trait '{}'", trait_name.text),
            ));
        };
        if !info.params.is_empty() {
            return Err(Diagnostic::new(
                trait_name.span,
                format!(
                    "generic trait '{}' cannot be used as a value; \
                     write its type arguments",
                    trait_name.text
                ),
            ));
        }
        // Publish the incomplete record first: a method signature may mention
        // this same trait through a pointer.
        let ty = self.types.record_forward(RecordKind::Struct);
        self.trait_values.insert(trait_name.text.clone(), ty);
        let result = self.complete_trait_value(ty, trait_name, &info);
        if result.is_err() {
            self.trait_values.remove(&trait_name.text);
        }
        result?;
        Ok(ty)
    }

    fn complete_trait_value(
        &mut self,
        ty: TypeId,
        trait_name: &ast::Name,
        info: &TraitInfo,
    ) -> Result<(), Diagnostic> {
        if info.methods.is_empty() {
            return Err(Diagnostic::new(
                trait_name.span,
                format!(
                    "trait '{}' has no methods, so it has no C function-pointer layout",
                    trait_name.text
                ),
            ));
        }
        if let Some(method) = info
            .methods
            .iter()
            .find(|method| !method.generics.is_empty())
        {
            let name = method
                .declarator
                .name
                .as_ref()
                .map_or("<unnamed>", |name| name.text.as_str());
            return Err(Diagnostic::new(
                trait_name.span,
                format!(
                    "trait '{}' cannot be used as a value: generic method '{}' is \
                     static-only, so it has no single function pointer; call it on a \
                     concrete type instead",
                    trait_name.text, name
                ),
            ));
        }
        let saved = self.impl_context.clone();
        self.impl_context = Some((self.types.builtin(Builtin::Void), String::new()));
        let result = (|| {
            let mut fields = Vec::new();
            for method in &info.methods {
                let Some(name) = &method.declarator.name else {
                    continue;
                };
                check_trait_value_self(trait_name, method)?;
                let function_ty = self.trait_method_type(method).map_err(|error| {
                    Diagnostic::new(
                        error.span,
                        format!("in trait '{}': {}", trait_name.text, error.message),
                    )
                })?;
                let pointer = self
                    .types
                    .pointer_to(QualifiedType::unqualified(function_ty));
                fields.push(RecordField::new(
                    name.text.clone(),
                    QualifiedType::unqualified(pointer),
                ));
            }
            self.types.complete_record(ty, fields).map_err(|error| {
                Diagnostic::new(
                    trait_name.span,
                    format!(
                        "trait '{}' cannot be used as a value: {error}",
                        trait_name.text
                    ),
                )
            })?;
            Ok(())
        })();
        self.impl_context = saved;
        result
    }

    /// Resolve `Trait::from<Type>()` into the trait value record plus the
    /// concrete function symbol that fills each of its fields.
    pub(super) fn trait_table(
        &mut self,
        trait_name: &ast::Name,
        type_args: &[ast::GenericArg],
        span: Span,
    ) -> Result<(TypeId, Vec<(String, String)>), Diagnostic> {
        if type_args.len() != 1 {
            return Err(Diagnostic::new(
                span,
                format!(
                    "trait value construction '{}::from<Type>()' takes exactly one type argument",
                    trait_name.text
                ),
            ));
        }
        let ast::GenericArgKind::Type(concrete_name) = &type_args[0].kind else {
            return Err(Diagnostic::new(
                type_args[0].span,
                "the argument of `Trait::from<Type>()` must be a type",
            ));
        };
        let value_ty = self.trait_value_type(trait_name)?;
        let concrete = self.resolve_type_name(concrete_name)?;
        // A generic trait impl materializes on first use; the table is built
        // from concrete method symbols like any other implementation.
        self.instantiate_generic_impl(concrete)?;
        let Some(impl_info) = self.impls.get(&concrete) else {
            return Err(Diagnostic::new(
                concrete_name.span,
                format!(
                    "trait '{}' is not implemented for this type",
                    trait_name.text
                ),
            ));
        };
        let impl_name = impl_info.name.clone();
        // A trait value has no place for trait type arguments, so only the
        // argument-free implementation is a candidate. Different traits are
        // kept apart by their own keys.
        let trait_impl = super::TraitImplKey {
            name: trait_name.text.clone(),
            args: Vec::new(),
        };
        if !self
            .trait_impls
            .contains(&(trait_name.text.clone(), Vec::new(), concrete))
        {
            return Err(Diagnostic::new(
                concrete_name.span,
                format!(
                    "trait '{}' is not implemented for this type",
                    trait_name.text
                ),
            ));
        }
        let Some(info) = self.traits.get(&trait_name.text).cloned() else {
            return Err(Diagnostic::new(
                trait_name.span,
                format!("unknown trait '{}'", trait_name.text),
            ));
        };
        let mut methods = Vec::new();
        for method in &info.methods {
            let Some(name) = &method.declarator.name else {
                continue;
            };
            // The lookup also queues a deferred generic impl body, which the
            // table's symbol needs before the lowerer emits any function.
            let Some(found) = self.trait_method_of(concrete, &trait_impl, name)? else {
                return Err(Diagnostic::new(
                    concrete_name.span,
                    format!(
                        "implementation of trait '{}' for '{}' is missing method '{}'",
                        trait_name.text, impl_name, name.text
                    ),
                ));
            };
            methods.push((name.text.clone(), found.symbol.clone()));
        }
        Ok((value_ty, methods))
    }

    /// The function type of a trait method with `self` bound to the
    /// implementing type currently in `impl_context`.
    ///
    /// A generic method's own parameters are bound to placeholders: the
    /// signature shape is checked once, and every concrete call instantiates
    /// the implementation through the generic method machinery.
    pub(super) fn trait_method_type(
        &mut self,
        method: &ast::TraitMethod,
    ) -> Result<TypeId, Diagnostic> {
        let args = self.method_placeholder_args(&method.generics)?;
        self.trait_method_type_with_args(method, &args)
    }

    /// The function type of a trait method with explicit method arguments.
    fn trait_method_type_with_args(
        &mut self,
        method: &ast::TraitMethod,
        args: &[super::GenericArgKey],
    ) -> Result<TypeId, Diagnostic> {
        let ty =
            self.resolve_method_type(&method.specs, &method.declarator, &method.generics, args)?;
        if !self.types.is_function(ty) {
            return Err(Diagnostic::new(
                method.span,
                "trait member is not a method declaration",
            ));
        }
        Ok(ty)
    }

    /// The function type of one implementation method with its own generic
    /// parameters bound to the shared comparison placeholders.
    fn generic_method_signature(
        &mut self,
        declaration: &ast::FunctionDecl,
        params: &[ast::GenericParam],
        args: &[super::GenericArgKey],
    ) -> Result<TypeId, Diagnostic> {
        self.resolve_method_type(&declaration.specs, &declaration.declarator, params, args)
    }

    /// Resolve one method signature with the method's own generic parameters
    /// bound. The declarator's own generics are cleared: they are represented
    /// by `params`, which are bound before the return and parameter types are
    /// resolved.
    fn resolve_method_type(
        &mut self,
        specs: &ast::DeclSpecs,
        declarator: &ast::Declarator,
        params: &[ast::GenericParam],
        args: &[super::GenericArgKey],
    ) -> Result<TypeId, Diagnostic> {
        self.push_type_scope();
        self.bind_generic_args(params, args);
        let mut declarator = declarator.clone();
        for derived in &mut declarator.derived {
            if let ast::DerivedDeclarator::Function(function) = derived {
                function.generics.clear();
            }
        }
        let resolved = (|| {
            let ResolvedSpecs { ty, qualifiers } = self.resolve_specs(specs)?;
            self.resolve_declarator(ty, qualifiers, &declarator)
        })();
        self.pop_type_scope();
        resolved.map(|resolved| resolved.ty)
    }

    /// Placeholder concrete arguments for a signature-shape comparison.
    ///
    /// Each type parameter gets its own fresh empty record, so two different
    /// parameters are never compatible and a concrete type cannot accidentally
    /// equal a placeholder. Const parameters get distinct integers. None of
    /// these values escapes the comparison.
    fn method_placeholder_args(
        &mut self,
        params: &[ast::GenericParam],
    ) -> Result<Vec<super::GenericArgKey>, Diagnostic> {
        let mut args = Vec::with_capacity(params.len());
        for (index, parameter) in params.iter().enumerate() {
            match parameter.kind {
                ast::GenericParamKind::Type => {
                    let ty = self
                        .types
                        .define_record(RecordKind::Struct, Vec::new())
                        .map_err(|error| {
                            Diagnostic::new(
                                parameter.span,
                                format!("cannot create a signature placeholder: {error}"),
                            )
                        })?;
                    args.push(super::GenericArgKey::Type(ty));
                }
                ast::GenericParamKind::Const => {
                    args.push(super::GenericArgKey::Int(index as i64 + 1));
                }
            }
        }
        Ok(args)
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

/// Whether a declaration specifier list names Shy's `self` type.
fn has_self_type(specs: &ast::DeclSpecs) -> bool {
    specs
        .type_specs
        .iter()
        .any(|specifier| matches!(specifier, ast::TypeSpecifier::SelfType))
}

/// Whether a method return type is written as a pointer to `self`.
///
/// `self *clone()` is `[Function, Pointer]`: the function operator is applied
/// first, then the pointer makes the return type a pointer.
fn return_self_is_pointer(declarator: &ast::Declarator) -> bool {
    matches!(
        declarator.derived.as_slice(),
        [
            ast::DerivedDeclarator::Function(_),
            ast::DerivedDeclarator::Pointer(_),
            ..
        ]
    )
}

/// Whether a parameter is written as a pointer to `self`.
fn parameter_self_is_pointer(param: &ast::ParamDecl) -> bool {
    param.declarator.as_ref().is_some_and(|declarator| {
        matches!(
            declarator.derived.first(),
            Some(ast::DerivedDeclarator::Pointer(_))
        )
    })
}

/// Reject the two signatures a trait value cannot represent: `self` by value
/// as the receiver and `self` by value as the return type.
///
/// A trait value stores function pointers, so the erased signature replaces
/// `self` with `void`. `self *` then becomes `void *`, which is
/// pointer-compatible with the implementation's `Circle *`. A by-value
/// `self` would become a `void` parameter or return, which is not a value.
fn check_trait_value_self(
    trait_name: &ast::Name,
    method: &ast::TraitMethod,
) -> Result<(), Diagnostic> {
    if has_self_type(&method.specs) && !return_self_is_pointer(&method.declarator) {
        return Err(Diagnostic::new(
            method.span,
            format!(
                "trait '{}' cannot be used as a value: method returns `self` by value; \
                 write `self *` so the trait table can erase it to `void *`",
                trait_name.text
            ),
        ));
    }
    let Some(ast::DerivedDeclarator::Function(function)) = method.declarator.derived.first() else {
        return Ok(());
    };
    for param in &function.params {
        if has_self_type(&param.specs) && !parameter_self_is_pointer(param) {
            return Err(Diagnostic::new(
                param.span,
                format!(
                    "trait '{}' cannot be used as a value: method takes `self` by value; \
                     write `self *` so the trait table can erase it to `void *`",
                    trait_name.text
                ),
            ));
        }
    }
    Ok(())
}

impl FnLower<'_> {
    /// Materialize `Trait::from<Type>()` as an ordinary record value whose
    /// fields are function pointers.
    pub(super) fn lower_trait_table(
        &mut self,
        trait_name: &ast::Name,
        type_args: &[ast::GenericArg],
        span: Span,
    ) -> Result<ExprVal, Diagnostic> {
        let (ty, methods) = self.l.trait_table(trait_name, type_args, span)?;
        let slot = self.temp_slot(ty, "");
        for (name, symbol) in methods {
            let (field_ty, offset) = self.l.field_of(ty, &name, span)?;
            let value = ir::Operand::Value(self.emit_value(ir::InstKind::SymbolAddr { symbol }));
            self.store_value(
                ir::Address::slot(slot).with_offset(i64::from(offset)),
                value,
                field_ty,
                super::Qualifiers::NONE,
            );
        }
        Ok(ExprVal {
            ty,
            qualifiers: super::Qualifiers::NONE,
            kind: ExprKind::Place(ir::Address::slot(slot)),
        })
    }

    /// Emit the `drop` call for one local and mark it moved.
    pub(super) fn emit_drop(&mut self, local: usize) {
        let (slot, drop_ty, symbol, active) = {
            let local = &self.locals[local];
            (
                local.slot,
                local.drop_ty,
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
        let (signature, abi) = match drop_ty {
            Some(drop_ty) => {
                let abi =
                    crate::abi_adapter::plan_target_function(&self.l.types, drop_ty, self.l.abi)
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
                (self.l.ir_signature(drop_ty), abi)
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
