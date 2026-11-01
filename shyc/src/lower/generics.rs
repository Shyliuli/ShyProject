//! Generic function and generic impl registration, inference, and
//! instantiation.
//!
//! `fn name<T>(...)` is a compile-time template. A call that writes concrete
//! arguments, `name<int>(x)`, re-resolves the template under a type scope that
//! binds `T`, producing one ordinary concrete function with a translation unit
//! internal symbol. `const` parameters bind an integer value the same way.
//! A call that writes no type arguments, `name(x)`, infers them from the
//! ordinary value arguments: each template parameter type is matched against
//! the argument expression types, and an inferred call instantiates through
//! the same cache as an explicit one. Instances are cached by name and
//! concrete arguments, so a repeated call, and the type-only analysis before
//! lowering it, share the same function.
//!
//! A `const` parameter and a type parameter that occurs only inside a function
//! type are not inferred; such a call names the parameter and asks for explicit
//! generic arguments.
//!
//! `impl<T> Box<T>` (or `impl Box<T>`) is the same kind of template. A method
//! lookup on one concrete record instance binds the impl parameters to that
//! record's own arguments and registers one ordinary function per method with
//! an internal symbol naming the receiver. A method body is only lowered when
//! a lookup asks for it, so an instance compiles exactly the methods it uses.
//! A method cannot declare generic parameters of its own, and a trait
//! implementation cannot be generic yet.

use std::collections::HashMap;
use std::collections::hash_map::Entry;

use super::{
    FunctionInstance, GenericArgKey, GenericEnv, GenericFunction, InstanceKey, Lowerer,
    PendingFunction, ResolvedSpecs,
};
use crate::ast;
use crate::sema::types::{Builtin, IntegerRank, TypeId, TypeKind};
use crate::source::{Diagnostic, Span};

/// How deep generic instantiation may nest before lowering gives up.
const MAX_INSTANTIATION_DEPTH: u32 = 64;

/// The shape of one template parameter type, with its type parameters open.
///
/// Inference resolves a parameter type to this shape instead of a concrete
/// type: a `T` stays [`TypePattern::Param`], so matching the argument types is
/// what binds it. A part that cannot bind anything is [`TypePattern::Concrete`],
/// which accepts every argument; the normal argument check after instantiation
/// still verifies it.
enum TypePattern {
    /// One type parameter of the template, by position.
    Param(usize),
    /// A concrete type, with nothing to infer.
    Concrete,
    /// A pointer to another shape.
    Pointer(Box<TypePattern>),
    /// An array of another shape. The length never binds a parameter.
    Array(Box<TypePattern>),
    /// `Name<args>` naming a generic record or enum instance.
    Generic { name: String, args: Vec<PatternArg> },
}

impl TypePattern {
    /// Whether this shape contains a type parameter that inference can bind.
    fn is_open(&self) -> bool {
        match self {
            TypePattern::Param(_) => true,
            TypePattern::Concrete => false,
            TypePattern::Pointer(inner) | TypePattern::Array(inner) => inner.is_open(),
            TypePattern::Generic { args, .. } => args.iter().any(|argument| match argument {
                PatternArg::Type(pattern) => pattern.is_open(),
                PatternArg::Const => false,
            }),
        }
    }
}

/// One argument of a generic type use inside a parameter type.
enum PatternArg {
    /// A type argument, matched against the instance's own type argument.
    Type(TypePattern),
    /// A `const` argument, which never binds during inference.
    Const,
}

/// Why one argument could not be matched against a parameter pattern.
enum UnifyError {
    /// The argument type does not have the shape the parameter requires.
    Mismatch,
    /// An earlier argument already bound the parameter to another type.
    Conflict {
        parameter: usize,
        first_argument: usize,
    },
}

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
                // A generic method has no file-scope template yet, and a trait
                // method is only a signature.
                ast::Item::Impl(_) | ast::Item::Trait(_) => {}
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
        let template = self.generic_function_template(name)?;
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
        self.instantiate_concrete(name, &template, concrete)
    }

    /// Registers every generic impl template before anything is resolved.
    ///
    /// A method lookup on a concrete record instance materializes the template
    /// it finds here, so the whole pass runs before any body is lowered.
    pub(super) fn register_generic_impls(
        &mut self,
        unit: &ast::TranslationUnit,
    ) -> Result<(), Diagnostic> {
        for item in &unit.items {
            let ast::Item::Impl(block) = item else {
                continue;
            };
            if block.generics.is_empty() {
                continue;
            }
            self.register_generic_impl(block)?;
        }
        Ok(())
    }

    fn register_generic_impl(&mut self, block: &ast::ImplBlock) -> Result<(), Diagnostic> {
        if block.trait_name.is_some() {
            return Err(Diagnostic::new(
                block.span,
                "generic trait implementations are not implemented yet",
            ));
        }
        if let Some(bound) = block
            .generics
            .iter()
            .find_map(|parameter| parameter.bounds.first())
        {
            return Err(Diagnostic::new(
                bound.span,
                "trait bounds on a generic impl are not implemented yet",
            ));
        }
        for (index, parameter) in block.generics.iter().enumerate() {
            let repeated = block.generics[..index]
                .iter()
                .any(|previous| previous.name.text == parameter.name.text);
            if repeated {
                return Err(Diagnostic::new(
                    parameter.span,
                    format!(
                        "redefinition of generic impl parameter '{}'",
                        parameter.name.text
                    ),
                ));
            }
        }
        let Some(generic) = generic_impl_target(&block.self_type) else {
            return Err(Diagnostic::new(
                block.self_type.span,
                "a generic impl target must name a generic record, as in 'impl<T> Box<T>'",
            ));
        };
        let Some(record) = self.generic_records.get(&generic.name.text) else {
            return Err(Diagnostic::new(
                generic.name.span,
                format!("'{}' is not a generic record", generic.name.text),
            ));
        };
        if generic.args.len() != record.params.len() {
            return Err(Diagnostic::new(
                block.self_type.span,
                format!(
                    "generic impl for '{}' names {} argument(s), but the record has {} \
                     parameter(s)",
                    generic.name.text,
                    generic.args.len(),
                    record.params.len()
                ),
            ));
        }
        let in_order = generic.args.len() == block.generics.len()
            && generic
                .args
                .iter()
                .zip(&block.generics)
                .all(|(arg, parameter)| argument_names_parameter(arg, parameter));
        if !in_order {
            return Err(Diagnostic::new(
                block.self_type.span,
                format!(
                    "generic impl target must list the impl's parameters in order, as in \
                     'impl<T> {}<T>'",
                    generic.name.text
                ),
            ));
        }
        if self.generic_impls.contains_key(&generic.name.text) {
            return Err(Diagnostic::new(
                block.span,
                format!(
                    "redefinition of the generic impl for '{}'",
                    generic.name.text
                ),
            ));
        }
        self.generic_impls.insert(
            generic.name.text.clone(),
            super::GenericImpl {
                params: block.generics.clone(),
                block: block.clone(),
            },
        );
        Ok(())
    }

    /// Materializes the generic impl whose receiver is `record`, once.
    ///
    /// The record's own concrete arguments bind the impl parameters, and every
    /// method becomes an ordinary function whose symbol names the receiver, so
    /// method lookup continues to see plain [`super::MethodInfo`] values and
    /// calls keep going through the direct-call machinery.
    pub(super) fn instantiate_generic_impl(&mut self, record: TypeId) -> Result<(), Diagnostic> {
        if self.generic_impl_instances.contains(&record) {
            return Ok(());
        }
        let Some(key) = self.record_instance_key(record) else {
            return Ok(());
        };
        let Some(template) = self.generic_impls.get(&key.name).cloned() else {
            return Ok(());
        };
        for (parameter, argument) in template.params.iter().zip(&key.args) {
            let kind_matches = matches!(
                (&parameter.kind, argument),
                (ast::GenericParamKind::Type, GenericArgKey::Type(_))
                    | (ast::GenericParamKind::Const, GenericArgKey::Int(_))
            );
            if !kind_matches {
                return Err(Diagnostic::new(
                    parameter.span,
                    format!(
                        "generic impl for '{}' takes '{}' as a {} parameter, but the record's \
                         argument is a {}; write the impl parameters before the target",
                        key.name,
                        parameter.name.text,
                        match parameter.kind {
                            ast::GenericParamKind::Type => "type",
                            ast::GenericParamKind::Const => "const",
                        },
                        match argument {
                            GenericArgKey::Type(_) => "type",
                            GenericArgKey::Int(_) => "constant",
                        }
                    ),
                ));
            }
        }
        if self.instantiation_depth >= MAX_INSTANTIATION_DEPTH {
            return Err(Diagnostic::new(
                template.block.span,
                format!(
                    "generic impl instantiation is nested more than \
                     {MAX_INSTANTIATION_DEPTH} levels deep"
                ),
            ));
        }
        // Mark first: collecting the methods resolves their signatures, which
        // must find this record's impl already present instead of recursing.
        self.generic_impl_instances.insert(record);
        self.instantiation_depth += 1;
        self.push_type_scope();
        self.bind_generic_args(&template.params, &key.args);
        let saved = self.impl_context.clone();
        self.impl_context = Some((record, key.name.clone()));
        let result = (|| {
            let instance = super::ImplInstance {
                params: template.params.clone(),
                args: key.args.clone(),
            };
            for method in &template.block.methods {
                self.collect_function(
                    method,
                    Some(super::ImplTarget {
                        ty: record,
                        name: key.name.clone(),
                        instance: Some(instance.clone()),
                    }),
                )?;
            }
            Ok(())
        })();
        self.impl_context = saved;
        self.pop_type_scope();
        self.instantiation_depth -= 1;
        if result.is_err() {
            self.generic_impl_instances.remove(&record);
        }
        result
    }

    /// The instantiation key of a concrete record, when it came from a generic
    /// record template.
    fn record_instance_key(&self, record: TypeId) -> Option<InstanceKey> {
        self.record_instances
            .iter()
            .find_map(|(key, instance)| (*instance == record).then(|| key.clone()))
    }

    /// Deterministic internal symbol for one method of one materialized impl.
    pub(super) fn generic_impl_method_symbol(
        &self,
        impl_name: &str,
        method: &str,
        record: TypeId,
    ) -> String {
        self.internal_symbol(&format!(
            "generic.impl.{}.{}.{}",
            Self::sanitize_ident(impl_name),
            Self::sanitize_ident(method),
            record.index()
        ))
    }

    /// Whether a bare name denotes a template call whose arguments are inferred
    /// from the value arguments.
    ///
    /// An ordinary function of the same name keeps winning the plain lookup,
    /// so a call stays a C call when both exist.
    pub(super) fn is_inferred_generic_call(&self, name: &str) -> bool {
        self.generic_functions.contains_key(name) && self.global_ref(name).is_none()
    }

    /// Instantiates the generic function `name` whose type parameters are
    /// inferred from the ordinary value arguments.
    ///
    /// Each template parameter type becomes a [`TypePattern`] with its type
    /// parameters left open, and the argument types are unified against those
    /// patterns. Only a `T`, `T *`, `T [N]`, and `Name<T>` shape binds a
    /// parameter. A `const` parameter, a parameter type that mentions a type
    /// parameter only inside a function type, and an argument list that leaves
    /// a parameter unbound or binds it to two incompatible types all report
    /// that the call needs explicit generic arguments.
    pub(super) fn infer_generic_function(
        &mut self,
        name: &ast::Name,
        arguments: &[(Span, TypeId)],
    ) -> Result<FunctionInstance, Diagnostic> {
        let template = self.generic_function_template(name)?;
        if let Some(parameter) = template
            .params
            .iter()
            .find(|parameter| parameter.kind == ast::GenericParamKind::Const)
        {
            return Err(Diagnostic::new(
                name.span,
                format!(
                    "generic function '{}' needs explicit generic arguments: its const \
                     parameter '{}' cannot be inferred from a value argument",
                    name.text, parameter.name.text
                ),
            ));
        }
        let Some(decl) = template.decl.as_ref() else {
            return Err(Diagnostic::new(
                name.span,
                format!(
                    "generic function '{}' needs explicit generic arguments: it has no \
                     declaration to infer from",
                    name.text
                ),
            ));
        };
        let Some(function) = generic_declarator(&decl.declarator) else {
            return Err(Diagnostic::new(
                decl.declarator.span,
                format!(
                    "generic function '{}' is not a generic function declaration",
                    name.text
                ),
            ));
        };
        let mut patterns = Vec::with_capacity(function.params.len());
        for (index, parameter) in function.params.iter().enumerate() {
            // `(void)` names no parameter, exactly as in `resolve_params`.
            if index == 0
                && function.params.len() == 1
                && parameter.declarator.is_none()
                && is_void_spec(&parameter.specs)
            {
                continue;
            }
            patterns.push(self.parameter_pattern(parameter, &template.params));
        }
        let mut bindings: Vec<Option<(TypeId, usize)>> = vec![None; template.params.len()];
        for (index, (span, ty)) in arguments.iter().enumerate() {
            let Some(pattern) = patterns.get(index) else {
                break;
            };
            let ty = self.decay_type(*ty);
            match self.unify_pattern(pattern, ty, index, &mut bindings) {
                Ok(()) => {}
                Err(UnifyError::Mismatch) => {
                    return Err(Diagnostic::new(
                        *span,
                        format!(
                            "cannot infer the generic arguments of '{}' from argument {}: \
                             the argument does not match the parameter type; write \
                             explicit generic arguments",
                            name.text,
                            index + 1
                        ),
                    ));
                }
                Err(UnifyError::Conflict {
                    parameter,
                    first_argument,
                }) => {
                    return Err(Diagnostic::new(
                        *span,
                        format!(
                            "cannot infer generic parameter '{}' of '{}': argument {} \
                             needs a different type than argument {}; write explicit \
                             generic arguments",
                            template.params[parameter].name.text,
                            name.text,
                            index + 1,
                            first_argument + 1
                        ),
                    ));
                }
            }
        }
        let mut concrete = Vec::with_capacity(template.params.len());
        for (index, parameter) in template.params.iter().enumerate() {
            let Some((ty, _)) = bindings[index] else {
                return Err(Diagnostic::new(
                    name.span,
                    format!(
                        "cannot infer generic parameter '{}' of '{}' from the arguments; \
                         write explicit generic arguments",
                        parameter.name.text, name.text
                    ),
                ));
            };
            concrete.push(GenericArgKey::Type(ty));
        }
        self.instantiate_concrete(name, &template, concrete)
    }

    /// The template a call names, or the diagnostic for a name that is not one.
    fn generic_function_template(&self, name: &ast::Name) -> Result<GenericFunction, Diagnostic> {
        if let Some(template) = self.generic_functions.get(&name.text) {
            return Ok(template.clone());
        }
        if self.global_ref(&name.text).is_some() {
            return Err(Diagnostic::new(
                name.span,
                format!("function '{}' is not generic", name.text),
            ));
        }
        Err(Diagnostic::new(
            name.span,
            format!("unknown generic function '{}'", name.text),
        ))
    }

    /// Verifies every trait bound once the concrete type arguments are known.
    ///
    /// A bound is satisfied by an `impl Bound for Type` already collected for
    /// the unit: the same [`Self::trait_impls`] table that rejects a duplicate
    /// implementation answers the check. Only a bound that writes no type
    /// arguments is supported, matching the AST's `Vec<Name>` bounds.
    fn check_generic_bounds(
        &self,
        function: &str,
        span: Span,
        params: &[ast::GenericParam],
        args: &[GenericArgKey],
    ) -> Result<(), Diagnostic> {
        for (parameter, argument) in params.iter().zip(args) {
            let GenericArgKey::Type(ty) = argument else {
                continue;
            };
            for bound in &parameter.bounds {
                let Some(info) = self.traits.get(&bound.text) else {
                    return Err(Diagnostic::new(
                        bound.span,
                        format!(
                            "trait bound '{}: {}' on generic function '{}' names unknown \
                             trait '{}'",
                            parameter.name.text, bound.text, function, bound.text
                        ),
                    ));
                };
                if !info.params.is_empty() {
                    return Err(Diagnostic::new(
                        bound.span,
                        format!(
                            "trait bound '{}: {}' on generic function '{}' needs type \
                             arguments, which are not implemented yet",
                            parameter.name.text, bound.text, function
                        ),
                    ));
                }
                let implemented = self.impls.get(ty).is_some_and(|implementation| {
                    self.trait_impls.contains(&(
                        bound.text.clone(),
                        Vec::new(),
                        implementation.name.clone(),
                    ))
                });
                if !implemented {
                    return Err(Diagnostic::new(
                        span,
                        format!(
                            "type parameter '{}' of generic function '{}' requires trait \
                             '{}', but '{}' does not implement it",
                            parameter.name.text,
                            function,
                            bound.text,
                            self.concrete_type_name(*ty)
                        ),
                    ));
                }
            }
        }
        Ok(())
    }

    /// A readable spelling of a concrete type for a bound diagnostic.
    ///
    /// The type store keeps no names, so a record or enum is named from the
    /// metadata that already refers to it: its implementation, its generic
    /// instance, or the scope that declared its tag.
    fn concrete_type_name(&self, ty: TypeId) -> String {
        match self.types.kind(ty) {
            TypeKind::Void => "void".to_string(),
            TypeKind::Bool => "bool".to_string(),
            TypeKind::Integer { rank, unsigned } => {
                let base = match rank {
                    IntegerRank::Char => "char",
                    IntegerRank::Short => "short",
                    IntegerRank::Int => "int",
                    IntegerRank::Long => "long",
                    IntegerRank::LongLong => "long long",
                };
                if *unsigned {
                    format!("unsigned {base}")
                } else {
                    base.to_string()
                }
            }
            TypeKind::Float32 => "float".to_string(),
            TypeKind::Float64 => "double".to_string(),
            TypeKind::Pointer { pointee } => {
                format!("{} *", self.concrete_type_name(pointee.ty))
            }
            TypeKind::Array { element, .. } => {
                format!("{} []", self.concrete_type_name(element.ty))
            }
            TypeKind::Function { .. } => "function".to_string(),
            TypeKind::Record { .. } | TypeKind::Enum(_) | TypeKind::PayloadEnum(_) => self
                .named_type(ty)
                .unwrap_or_else(|| match self.types.kind(ty) {
                    TypeKind::Record { .. } => "struct".to_string(),
                    _ => "enum".to_string(),
                }),
        }
    }

    /// A source name already attached to a record or enum type, if any.
    fn named_type(&self, ty: TypeId) -> Option<String> {
        if let Some(implementation) = self.impls.get(&ty) {
            return Some(implementation.name.clone());
        }
        if let Some((key, _)) = self
            .record_instances
            .iter()
            .find(|(_, instance)| **instance == ty)
        {
            return Some(key.name.clone());
        }
        if let Some((key, _)) = self
            .enum_instances
            .iter()
            .find(|(_, instance)| **instance == ty)
        {
            return Some(key.name.clone());
        }
        self.type_scopes.iter().rev().find_map(|scope| {
            scope
                .tags
                .iter()
                .find(|(_, declared)| **declared == ty)
                .map(|(name, _)| name.clone())
        })
    }

    /// The shared instantiation tail: an explicit and an inferred call differ
    /// only in how the concrete arguments were obtained.
    fn instantiate_concrete(
        &mut self,
        name: &ast::Name,
        template: &GenericFunction,
        concrete: Vec<GenericArgKey>,
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
        let key = InstanceKey {
            name: name.text.clone(),
            args: concrete,
        };
        if let Some(instance) = self.function_instances.get(&key) {
            return Ok(instance.clone());
        }
        self.check_generic_bounds(&name.text, name.span, &template.params, &key.args)?;
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

    /// The shape of one parameter type, with the template's type parameters
    /// left open.
    fn parameter_pattern(
        &self,
        parameter: &ast::ParamDecl,
        type_params: &[ast::GenericParam],
    ) -> TypePattern {
        let pattern = match &parameter.declarator {
            Some(declarator) => self.declarator_pattern(
                self.spec_pattern(&parameter.specs, type_params),
                &declarator.derived,
            ),
            None => self.spec_pattern(&parameter.specs, type_params),
        };
        // C parameter adjustment: an array parameter becomes a pointer to its
        // element. A function parameter is already `Concrete` above.
        match pattern {
            TypePattern::Array(element) => TypePattern::Pointer(element),
            other => other,
        }
    }

    /// The shape of a parameter's base type.
    ///
    /// Only a bare type-parameter name and a generic use that mentions one can
    /// bind, so every other specifier list stays unconstrained.
    fn spec_pattern(
        &self,
        specs: &ast::DeclSpecs,
        type_params: &[ast::GenericParam],
    ) -> TypePattern {
        let [specifier] = specs.type_specs.as_slice() else {
            return TypePattern::Concrete;
        };
        match specifier {
            ast::TypeSpecifier::TypedefName(name) => {
                let parameter = type_params.iter().position(|parameter| {
                    parameter.kind == ast::GenericParamKind::Type
                        && parameter.name.text == name.text
                });
                match parameter {
                    Some(index) => TypePattern::Param(index),
                    None => TypePattern::Concrete,
                }
            }
            ast::TypeSpecifier::Generic(generic) => {
                let args = generic
                    .args
                    .iter()
                    .map(|argument| match &argument.kind {
                        ast::GenericArgKind::Type(type_name) => {
                            PatternArg::Type(self.type_name_pattern(type_name, type_params))
                        }
                        // A const argument never binds a type parameter, so
                        // only its kind matters here.
                        ast::GenericArgKind::Const(_) => PatternArg::Const,
                    })
                    .collect();
                let pattern = TypePattern::Generic {
                    name: generic.name.text.clone(),
                    args,
                };
                if pattern.is_open() {
                    pattern
                } else {
                    TypePattern::Concrete
                }
            }
            _ => TypePattern::Concrete,
        }
    }

    fn type_name_pattern(
        &self,
        type_name: &ast::TypeName,
        type_params: &[ast::GenericParam],
    ) -> TypePattern {
        let base = self.spec_pattern(&type_name.specs, type_params);
        match &type_name.declarator {
            Some(declarator) => self.declarator_pattern(base, &declarator.derived),
            None => base,
        }
    }

    /// Apply a declarator's operators to a shape, from the name outward.
    fn declarator_pattern(
        &self,
        base: TypePattern,
        derived: &[ast::DerivedDeclarator],
    ) -> TypePattern {
        let mut pattern = base;
        for operator in derived.iter().rev() {
            pattern = match operator {
                ast::DerivedDeclarator::Pointer(_) if pattern.is_open() => {
                    TypePattern::Pointer(Box::new(pattern))
                }
                ast::DerivedDeclarator::Array(_) if pattern.is_open() => {
                    TypePattern::Array(Box::new(pattern))
                }
                // A type parameter behind a function type has no value shape to
                // match, so the whole part stays unconstrained.
                _ => TypePattern::Concrete,
            };
        }
        pattern
    }

    /// Binds the open type parameters of `pattern` against one argument type.
    fn unify_pattern(
        &self,
        pattern: &TypePattern,
        ty: TypeId,
        argument: usize,
        bindings: &mut [Option<(TypeId, usize)>],
    ) -> Result<(), UnifyError> {
        match pattern {
            TypePattern::Concrete => Ok(()),
            TypePattern::Param(index) => match bindings[*index] {
                None => {
                    bindings[*index] = Some((ty, argument));
                    Ok(())
                }
                Some((existing, _)) if self.types.is_compatible(existing, ty) => Ok(()),
                Some((_, first_argument)) => Err(UnifyError::Conflict {
                    parameter: *index,
                    first_argument,
                }),
            },
            TypePattern::Pointer(pointee) => {
                let TypeKind::Pointer { pointee: actual } = self.types.kind(ty) else {
                    return Err(UnifyError::Mismatch);
                };
                self.unify_pattern(pointee, actual.ty, argument, bindings)
            }
            TypePattern::Array(element) => {
                let TypeKind::Array {
                    element: actual, ..
                } = self.types.kind(ty)
                else {
                    return Err(UnifyError::Mismatch);
                };
                self.unify_pattern(element, actual.ty, argument, bindings)
            }
            TypePattern::Generic { name, args } => {
                let Some(actual_args) = self.generic_instance_args(name, ty) else {
                    return Err(UnifyError::Mismatch);
                };
                for (pattern, actual) in args.iter().zip(actual_args) {
                    let PatternArg::Type(pattern) = pattern else {
                        continue;
                    };
                    let GenericArgKey::Type(actual) = actual else {
                        continue;
                    };
                    self.unify_pattern(pattern, *actual, argument, bindings)?;
                }
                Ok(())
            }
        }
    }

    /// Concrete arguments of the instantiation that produced `ty`, when `ty` is
    /// one instance of the generic record or enum called `name`.
    ///
    /// The instance tables already hold this mapping, so inference never has to
    /// instantiate a pattern with symbolic arguments just to compare record
    /// identities.
    fn generic_instance_args(&self, name: &str, ty: TypeId) -> Option<&[GenericArgKey]> {
        if let Some(args) = self.record_instances.iter().find_map(|(key, instance)| {
            (key.name == name && *instance == ty).then_some(key.args.as_slice())
        }) {
            return Some(args);
        }
        self.enum_instances.iter().find_map(|(key, instance)| {
            (key.name == name && *instance == ty).then_some(key.args.as_slice())
        })
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
                "a method call cannot take generic arguments; only a generic function name can",
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

/// The generic target of `impl<T> Box<T>`, when the type name is exactly one
/// generic use with no declarator.
fn generic_impl_target(type_name: &ast::TypeName) -> Option<&ast::GenericSpec> {
    if type_name.declarator.is_some() {
        return None;
    }
    match type_name.specs.type_specs.as_slice() {
        [ast::TypeSpecifier::Generic(generic)] => Some(generic),
        _ => None,
    }
}

/// Whether one target argument is exactly the impl parameter it binds.
fn argument_names_parameter(arg: &ast::GenericArg, parameter: &ast::GenericParam) -> bool {
    match (&arg.kind, &parameter.kind) {
        (ast::GenericArgKind::Type(type_name), ast::GenericParamKind::Type) => {
            type_name.declarator.is_none()
                && matches!(
                    type_name.specs.type_specs.as_slice(),
                    [ast::TypeSpecifier::TypedefName(name)]
                        if name.text == parameter.name.text
                )
        }
        (ast::GenericArgKind::Const(ast::Expr::Name(name)), ast::GenericParamKind::Const) => {
            name.text == parameter.name.text
        }
        _ => false,
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

/// Whether a parameter's specifier list is exactly `void`, as in `f(void)`.
fn is_void_spec(specs: &ast::DeclSpecs) -> bool {
    matches!(
        specs.type_specs.as_slice(),
        [ast::TypeSpecifier::Builtin(ast::BuiltinType::Void)]
    )
}
