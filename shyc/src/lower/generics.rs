//! Generic function registration and instantiation.
//!
//! `fn name<T>(...)` is a compile-time template. A call that writes concrete
//! arguments, `name<int>(x)`, re-resolves the template under a type scope that
//! binds `T`, producing one ordinary concrete function with a translation unit
//! internal symbol. `const` parameters bind an integer value the same way.
//! Instances are cached by name and concrete arguments, so a repeated call, and
//! the type-only analysis before lowering it, share the same function.
//!
//! Inference, generic methods, and traits are not implemented; each names its
//! construct in a diagnostic.

use std::collections::HashMap;
use std::collections::hash_map::Entry;

use super::{
    FunctionInstance, GenericArgKey, GenericEnv, InstanceKey, Lowerer, PendingFunction,
    ResolvedSpecs,
};
use crate::ast;
use crate::sema::types::{Builtin, TypeId};
use crate::source::{Diagnostic, Span};

/// How deep generic instantiation may nest before lowering gives up.
const MAX_INSTANTIATION_DEPTH: u32 = 64;

impl Lowerer {
    /// Registers every generic function template before anything is resolved.
    ///
    /// A call may appear before the definition it instantiates, so all
    /// templates are collected in one pass first. A prototype registers without
    /// a declaration: its body arrives with the definition. Generic functions
    /// never enter the file-scope name table, because only an instantiation
    /// creates a concrete function.
    pub(super) fn register_generic_functions(
        &mut self,
        unit: &ast::TranslationUnit,
    ) -> Result<(), Diagnostic> {
        for item in &unit.items {
            match item {
                ast::Item::Function(function) => {
                    if generic_declarator(&function.declarator).is_some() {
                        self.register_generic_function(&function.declarator, Some(function))?;
                    }
                }
                ast::Item::Decl(declaration) => {
                    for init in &declaration.declarators {
                        if generic_declarator(&init.declarator).is_some() {
                            self.register_generic_function(&init.declarator, None)?;
                        }
                    }
                }
                // A generic method has no file-scope template yet.
                ast::Item::Impl(_) => {}
            }
        }
        Ok(())
    }

    fn register_generic_function(
        &mut self,
        declarator: &ast::Declarator,
        decl: Option<&ast::FunctionDecl>,
    ) -> Result<(), Diagnostic> {
        let Some(function) = generic_declarator(declarator) else {
            return Ok(());
        };
        let Some(name) = &declarator.name else {
            return Err(Diagnostic::new(
                declarator.span,
                "a generic function needs a name",
            ));
        };
        match self.generic_functions.entry(name.text.clone()) {
            Entry::Vacant(entry) => {
                entry.insert(super::GenericFunction {
                    params: function.generics.clone(),
                    decl: decl.cloned(),
                });
            }
            Entry::Occupied(mut entry) => {
                // A prototype after a definition adds nothing, and a repeated
                // prototype is harmless. The first parameters stay in force.
                if let Some(decl) = decl {
                    if entry.get().decl.is_some() {
                        return Err(Diagnostic::new(
                            declarator.span,
                            format!("redefinition of generic function '{}'", name.text),
                        ));
                    }
                    entry.get_mut().decl = Some(decl.clone());
                }
            }
        }
        Ok(())
    }

    /// Instantiates `name<args>` into a concrete function and queues its body.
    pub(super) fn instantiate_generic_function(
        &mut self,
        name: &ast::Name,
        args: &[ast::GenericArg],
    ) -> Result<FunctionInstance, Diagnostic> {
        if self.instantiation_depth >= MAX_INSTANTIATION_DEPTH {
            return Err(Diagnostic::new(
                name.span,
                format!(
                    "generic function instantiation is nested more than \
                     {MAX_INSTANTIATION_DEPTH} levels deep"
                ),
            ));
        }
        let Some(template) = self.generic_functions.get(&name.text).cloned() else {
            if self.global_ref(&name.text).is_some() {
                return Err(Diagnostic::new(
                    name.span,
                    format!("function '{}' is not generic", name.text),
                ));
            }
            return Err(Diagnostic::new(
                name.span,
                format!("unknown generic function '{}'", name.text),
            ));
        };
        if args.len() != template.params.len() {
            return Err(Diagnostic::new(
                name.span,
                format!(
                    "generic function '{}' expects {} argument(s), but {} were given",
                    name.text,
                    template.params.len(),
                    args.len()
                ),
            ));
        }
        // Arguments resolve in the caller's scope, never the template's.
        let concrete = self.resolve_generic_args(&name.text, &template.params, args)?;
        let key = InstanceKey {
            name: name.text.clone(),
            args: concrete,
        };
        if let Some(instance) = self.function_instances.get(&key) {
            return Ok(instance.clone());
        }
        let Some(mut decl) = template.decl.clone() else {
            return Err(Diagnostic::new(
                name.span,
                format!(
                    "generic function '{}' has no body to instantiate",
                    name.text
                ),
            ));
        };
        // The parameters are bound by the type scope below, so what resolves
        // here is an ordinary concrete declaration.
        for derived in &mut decl.declarator.derived {
            if let ast::DerivedDeclarator::Function(function) = derived {
                function.generics.clear();
            }
        }
        let symbol = self.generic_function_symbol(&name.text, &key.args);
        let ty = self.resolve_generic_function_type(&template.params, &key.args, &decl)?;
        let instance = FunctionInstance {
            symbol: symbol.clone(),
            ty,
        };
        // Cache before the body is lowered: a recursive call must already find
        // the instance, and lowering the body may instantiate more.
        self.function_instances
            .insert(key.clone(), instance.clone());
        let env = self.generic_env(&template.params, &key.args);
        self.pending.push(PendingFunction {
            decl,
            symbol,
            ty,
            impl_context: None,
            env: Some(env),
            internal: true,
        });
        Ok(instance)
    }

    /// Resolves `args` against a template's `params` in the caller's scope.
    ///
    /// A type argument becomes a type identity and a `const` argument becomes
    /// the value of an integer constant expression. The kinds must line up, so
    /// a type cannot bind a `const` parameter and vice versa.
    pub(super) fn resolve_generic_args(
        &mut self,
        template_name: &str,
        params: &[ast::GenericParam],
        args: &[ast::GenericArg],
    ) -> Result<Vec<GenericArgKey>, Diagnostic> {
        let mut resolved = Vec::with_capacity(args.len());
        for (arg, parameter) in args.iter().zip(params) {
            let key = match (&arg.kind, &parameter.kind) {
                (ast::GenericArgKind::Type(type_name), ast::GenericParamKind::Type) => {
                    GenericArgKey::Type(self.resolve_type_name(type_name)?)
                }
                (ast::GenericArgKind::Const(value), ast::GenericParamKind::Const) => {
                    let value = self.eval_const_int(value).map_err(|error| {
                        Diagnostic::new(
                            error.span,
                            format!(
                                "generic argument for const parameter '{}' of '{}' must be \
                                 an integer constant expression: {}",
                                parameter.name.text, template_name, error.message
                            ),
                        )
                    })?;
                    GenericArgKey::Int(value)
                }
                (ast::GenericArgKind::Type(_), ast::GenericParamKind::Const) => {
                    return Err(Diagnostic::new(
                        arg.span,
                        format!(
                            "generic parameter '{}' of '{}' expects a const argument, not a type",
                            parameter.name.text, template_name
                        ),
                    ));
                }
                (ast::GenericArgKind::Const(_), ast::GenericParamKind::Type) => {
                    return Err(Diagnostic::new(
                        arg.span,
                        format!(
                            "generic parameter '{}' of '{}' expects a type argument, not a \
                             constant",
                            parameter.name.text, template_name
                        ),
                    ));
                }
            };
            resolved.push(key);
        }
        Ok(resolved)
    }

    /// The concrete bindings a template's arguments install in a scope.
    pub(super) fn generic_env(
        &self,
        params: &[ast::GenericParam],
        args: &[GenericArgKey],
    ) -> GenericEnv {
        let int_ty = self.types.builtin(Builtin::Int);
        let mut env = GenericEnv {
            types: HashMap::new(),
            consts: HashMap::new(),
        };
        for (parameter, argument) in params.iter().zip(args) {
            match argument {
                GenericArgKey::Type(ty) => {
                    env.types.insert(parameter.name.text.clone(), *ty);
                }
                GenericArgKey::Int(value) => {
                    env.consts
                        .insert(parameter.name.text.clone(), (*value, int_ty));
                }
            }
        }
        env
    }

    /// Installs concrete bindings into the innermost type scope.
    pub(super) fn bind_generic_args(
        &mut self,
        params: &[ast::GenericParam],
        args: &[GenericArgKey],
    ) {
        let int_ty = self.types.builtin(Builtin::Int);
        for (parameter, argument) in params.iter().zip(args) {
            // A direct insert, unlike `declare_typedef`, lets a parameter
            // shadow an outer name.
            let scope = self.type_scopes.last_mut().expect("type scope stack");
            match argument {
                GenericArgKey::Type(ty) => {
                    scope.typedefs.insert(parameter.name.text.clone(), *ty);
                }
                GenericArgKey::Int(value) => {
                    scope
                        .consts
                        .insert(parameter.name.text.clone(), (*value, int_ty));
                }
            }
        }
    }

    /// Resolves the template's return type and declarator with its parameters
    /// bound, producing the concrete function type.
    fn resolve_generic_function_type(
        &mut self,
        params: &[ast::GenericParam],
        args: &[GenericArgKey],
        decl: &ast::FunctionDecl,
    ) -> Result<TypeId, Diagnostic> {
        self.instantiation_depth += 1;
        self.push_type_scope();
        let resolved = (|| {
            self.bind_generic_args(params, args);
            let ResolvedSpecs { ty, qualifiers } = self.resolve_specs(&decl.specs)?;
            let resolved = self.resolve_declarator(ty, qualifiers, &decl.declarator)?;
            Ok::<TypeId, Diagnostic>(resolved.ty)
        })();
        self.pop_type_scope();
        self.instantiation_depth -= 1;
        let ty = resolved?;
        if self.types.is_function(ty) {
            Ok(ty)
        } else {
            Err(Diagnostic::new(
                decl.span,
                "generic declaration is not a function",
            ))
        }
    }

    /// Resolves the callee of a call that writes explicit generic arguments.
    ///
    /// Only a plain function name can name a template: a method, a scoped name,
    /// and an arbitrary callee expression have nothing to instantiate.
    pub(super) fn generic_call_target(
        &mut self,
        callee: &ast::Expr,
        type_args: &[ast::GenericArg],
        span: Span,
    ) -> Result<FunctionInstance, Diagnostic> {
        match callee {
            ast::Expr::Name(name) => self.instantiate_generic_function(name, type_args),
            ast::Expr::ScopedName { .. } | ast::Expr::Member { .. } => Err(Diagnostic::new(
                span,
                "generic method calls are not supported by lowering yet",
            )),
            _ => Err(Diagnostic::new(
                span,
                "only a generic function name can take generic arguments",
            )),
        }
    }

    /// Deterministic translation-unit local symbol for one instance.
    ///
    /// Instances never cross a translation unit, so the mangling only has to be
    /// unique inside the unit: the function name plus the resolved type
    /// identities and const values. A const is spelled with a distinct `n`
    /// prefix and its fixed-width hex bits, so it can never collide with a type
    /// index.
    fn generic_function_symbol(&self, name: &str, args: &[GenericArgKey]) -> String {
        let mut mangled = format!("generic.{}", Self::sanitize_ident(name));
        for arg in args {
            mangled.push('.');
            match arg {
                GenericArgKey::Type(ty) => mangled.push_str(&ty.index().to_string()),
                GenericArgKey::Int(value) => {
                    mangled.push_str(&format!("n{:016x}", *value as u64));
                }
            }
        }
        self.internal_symbol(&mangled)
    }
}

/// The generic function declarator, when this declarator names a `fn` template.
pub(super) fn generic_declarator(declarator: &ast::Declarator) -> Option<&ast::FunctionDeclarator> {
    match declarator.derived.first() {
        Some(ast::DerivedDeclarator::Function(function)) if !function.generics.is_empty() => {
            Some(function)
        }
        _ => None,
    }
}
