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
//! lookup on one concrete record instance unifies the impl target against that
//! record's own arguments and registers one ordinary function per method with
//! an internal symbol naming the receiver. A method body is only lowered when
//! a lookup asks for it, so an instance compiles exactly the methods it uses.
//! A generic impl target may reorder or repeat its parameters, as in
//! `impl<T, U> Trait for Pair<U, T>`, and it may mix them with concrete,
//! nested, pointer, and array shapes, as in `impl<T> Trait for Pair<T, int>`
//! or `impl<T> Trait for Box<Vec<T>>`. Matching binds each impl parameter from
//! its position in the concrete receiver, skips a template whose concrete
//! parts do not agree, and reports a parameter that the target repeats with
//! two different types.
//!
//! A method may declare generic parameters of its own. The method is stored as
//! a template inside its impl and instantiated through the same cache as a
//! generic function; the receiver bindings and the method's arguments are
//! merged into the environment its body resolves under. A trait implementation
//! may be generic too, and its trait arguments resolve once the impl parameters
//! are bound. A materialized trait implementation is keyed by the trait, the
//! concrete receiver, and those resolved trait arguments, so several traits and
//! several parameterizations of one trait can share a receiver.

use std::collections::HashMap;
use std::collections::hash_map::Entry;

use super::{
    FunctionInstance, GenericArgKey, GenericEnv, GenericFunction, GenericMethod, InstanceKey,
    Lowerer, PendingFunction, ResolvedSpecs,
};
use crate::ast;
use crate::sema::types::{Builtin, IntegerRank, TypeId, TypeKind};
use crate::source::{Diagnostic, Span};

/// How deep generic instantiation may nest before lowering gives up.
const MAX_INSTANTIATION_DEPTH: u32 = 64;

/// The shape of one template parameter or impl target type, with its type
/// parameters open.
///
/// Inference resolves a parameter type to this shape instead of a concrete
/// type: a `T` stays [`TypePattern::Param`], so matching the argument types is
/// what binds it. A generic impl target uses the same shapes, but writes
/// concrete parts as [`TypePattern::Exact`] so a receiver only matches when
/// those parts agree. Inference keeps a part that cannot bind anything as
/// [`TypePattern::Concrete`], which accepts every argument; the normal argument
/// check after instantiation still verifies it.
enum TypePattern {
    /// One type parameter of the template, by position.
    Param(usize),
    /// A concrete type, with nothing to infer.
    Concrete,
    /// A concrete type that a receiver must equal.
    Exact(TypeId),
    /// A pointer to another shape.
    Pointer(Box<TypePattern>),
    /// An array of another shape. `len` is `None` when the length does not
    /// take part in matching and `Some` when it must agree.
    Array {
        element: Box<TypePattern>,
        len: Option<u32>,
    },
    /// `Name<args>` naming a generic record or enum instance.
    Generic { name: String, args: Vec<PatternArg> },
}

impl TypePattern {
    /// Whether this shape contains a type parameter that matching can bind.
    fn is_open(&self) -> bool {
        match self {
            TypePattern::Param(_) => true,
            TypePattern::Concrete | TypePattern::Exact(_) => false,
            TypePattern::Pointer(inner) | TypePattern::Array { element: inner, .. } => {
                inner.is_open()
            }
            TypePattern::Generic { args, .. } => args.iter().any(|argument| match argument {
                PatternArg::Type(pattern) => pattern.is_open(),
                PatternArg::Const(_) => false,
            }),
        }
    }
}

/// One argument of a generic type use inside a pattern.
enum PatternArg {
    /// A type argument, matched against the instance's own type argument.
    Type(TypePattern),
    /// A `const` argument. Inference leaves it open; a generic impl target
    /// either binds a parameter or requires one exact value.
    Const(Option<ConstPattern>),
}

/// The shape of one `const` generic argument.
enum ConstPattern {
    /// One const parameter of the impl, by position.
    Param(usize),
    /// One concrete integer value.
    Value(i64),
}

/// One top-level argument of a generic impl target.
enum TargetArg {
    Type(TypePattern),
    Const(ConstPattern),
}

/// The parameter bindings gathered while matching one pattern.
struct Bindings {
    types: Vec<Option<(TypeId, usize)>>,
    consts: Vec<Option<(i64, usize)>>,
}

impl Bindings {
    fn new(params: usize) -> Bindings {
        Bindings {
            types: vec![None; params],
            consts: vec![None; params],
        }
    }
}

/// Why one argument could not be matched against a pattern.
enum UnifyError {
    /// The argument type does not have the shape the pattern requires.
    Mismatch,
    /// An earlier position already bound the parameter to another value.
    Conflict {
        parameter: usize,
        first_argument: usize,
        first: GenericArgKey,
        second: GenericArgKey,
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
                // method is only a signature. An import declares no function.
                ast::Item::Impl(_) | ast::Item::Trait(_) | ast::Item::Use(_) => {}
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
        if let Some(trait_name) = &block.trait_name {
            let Some(info) = self.traits.get(&trait_name.text) else {
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
        }
        // A target argument may name an impl parameter anywhere, or write a
        // concrete or nested type. Only the argument kind is checked here;
        // concrete parts resolve and compare against the receiver at
        // instantiation, when every file-scope name exists.
        for (position, argument) in generic.args.iter().enumerate() {
            let actual = generic_argument_kind(argument);
            let expected = &record.params[position].kind;
            if &actual != expected {
                return Err(Diagnostic::new(
                    argument.span,
                    format!(
                        "target position {} of '{}' is a {}, but the target argument is a {}",
                        position + 1,
                        generic.name.text,
                        generic_parameter_kind_text(expected),
                        generic_argument_kind_text(&actual)
                    ),
                ));
            }
        }
        let mut used = vec![false; block.generics.len()];
        for argument in &generic.args {
            mark_target_params(argument, &block.generics, &mut used);
        }
        if let Some((_index, parameter)) = block
            .generics
            .iter()
            .enumerate()
            .find(|(index, _)| !used[*index])
        {
            return Err(Diagnostic::new(
                parameter.span,
                format!(
                    "generic impl parameter '{}' is not used in the target for '{}'; every \
                     parameter must appear in the target",
                    parameter.name.text, generic.name.text
                ),
            ));
        }
        // Several templates may target the same record with disjoint concrete
        // arguments. Overlap is caught when a concrete receiver asks for the
        // impl and both templates match it.
        self.generic_impls
            .entry(generic.name.text.clone())
            .or_default()
            .push(super::GenericImpl {
                params: block.generics.clone(),
                block: block.clone(),
            });
        Ok(())
    }

    /// Materializes every generic impl whose target matches `record`.
    ///
    /// One record may have an inherent template plus a template per trait, and
    /// several templates may target the same record with disjoint concrete
    /// arguments. The target binds the impl parameters against the record's own
    /// concrete arguments; a template whose concrete parts disagree is
    /// skipped. Each method then becomes an ordinary function whose symbol
    /// names the receiver, so method lookup continues to see plain
    /// [`super::MethodInfo`] values and calls stay direct calls.
    ///
    /// A materialized impl is remembered by concrete receiver plus trait
    /// identity and resolved trait arguments, so several traits, and several
    /// parameterizations of one trait, can coexist on one receiver.
    pub(super) fn instantiate_generic_impl(&mut self, record: TypeId) -> Result<(), Diagnostic> {
        let Some(key) = self.record_instance_key(record) else {
            return Ok(());
        };
        let Some(templates) = self.generic_impls.get(&key.name).cloned() else {
            return Ok(());
        };
        for template in &templates {
            let Some(env) = self.bind_generic_impl_target(&key, template)? else {
                continue;
            };
            // An impl whose parameter bounds do not hold for these arguments
            // simply does not apply, exactly like an unmatched target.
            if template
                .params
                .iter()
                .any(|parameter| !parameter.bounds.is_empty())
                && !self.generic_bounds_hold(&template.params, &env)?
            {
                continue;
            }
            self.materialize_generic_impl(record, &key.name, template, env)?;
        }
        Ok(())
    }

    /// Matches one impl target against the concrete receiver, binding every
    /// impl parameter. `None` means the target's concrete parts do not agree
    /// with this receiver, so the template does not apply.
    fn bind_generic_impl_target(
        &mut self,
        key: &InstanceKey,
        template: &super::GenericImpl,
    ) -> Result<Option<Vec<GenericArgKey>>, Diagnostic> {
        let target = self.impl_target_arguments(&template.block.self_type, &template.params)?;
        let mut bindings = Bindings::new(template.params.len());
        for (position, (target, actual)) in target.iter().zip(&key.args).enumerate() {
            match (target, actual) {
                (TargetArg::Type(pattern), GenericArgKey::Type(ty)) => {
                    match self.unify_pattern(pattern, *ty, position, &mut bindings) {
                        Ok(()) => {}
                        Err(UnifyError::Mismatch) => return Ok(None),
                        Err(UnifyError::Conflict {
                            parameter,
                            first_argument,
                            first,
                            second,
                        }) => {
                            return Err(self.generic_impl_conflict(
                                key,
                                template,
                                parameter,
                                first_argument,
                                position,
                                first,
                                second,
                            ));
                        }
                    }
                }
                (TargetArg::Const(ConstPattern::Param(parameter)), GenericArgKey::Int(value)) => {
                    match bindings.consts[*parameter] {
                        None => bindings.consts[*parameter] = Some((*value, position)),
                        Some((previous, _)) if previous == *value => {}
                        Some((previous, first_argument)) => {
                            return Err(self.generic_impl_conflict(
                                key,
                                template,
                                *parameter,
                                first_argument,
                                position,
                                GenericArgKey::Int(previous),
                                GenericArgKey::Int(*value),
                            ));
                        }
                    }
                }
                (TargetArg::Const(ConstPattern::Value(expected)), GenericArgKey::Int(value))
                    if expected == value => {}
                // A kind mismatch or a concrete value that differs means this
                // template targets another instantiation.
                _ => return Ok(None),
            }
        }
        let mut env = Vec::with_capacity(template.params.len());
        for (index, parameter) in template.params.iter().enumerate() {
            let binding = match parameter.kind {
                ast::GenericParamKind::Type => {
                    bindings.types[index].map(|(ty, _)| GenericArgKey::Type(ty))
                }
                ast::GenericParamKind::Const => {
                    bindings.consts[index].map(|(value, _)| GenericArgKey::Int(value))
                }
            };
            let Some(argument) = binding else {
                return Err(Diagnostic::new(
                    parameter.span,
                    format!(
                        "generic impl parameter '{}' is not bound by the target for '{}'",
                        parameter.name.text, key.name
                    ),
                ));
            };
            env.push(argument);
        }
        Ok(Some(env))
    }

    /// The diagnostic for a target that repeats one parameter with two
    /// different concrete values.
    #[allow(clippy::too_many_arguments)]
    fn generic_impl_conflict(
        &self,
        key: &InstanceKey,
        template: &super::GenericImpl,
        parameter: usize,
        first_argument: usize,
        second_argument: usize,
        first: GenericArgKey,
        second: GenericArgKey,
    ) -> Diagnostic {
        Diagnostic::new(
            template.block.self_type.span,
            format!(
                "generic impl for '{}' binds parameter '{}' to both {} and {} at target \
                 positions {} and {}; the target repeats a parameter that this receiver cannot \
                 satisfy consistently",
                key.name,
                template.params[parameter].name.text,
                self.generic_arg_text(first),
                self.generic_arg_text(second),
                first_argument + 1,
                second_argument + 1
            ),
        )
    }

    /// The target arguments of one generic impl, with concrete parts resolved
    /// in the receiver's scope.
    ///
    /// Matching happens when a concrete receiver is first needed, so every
    /// file-scope type the target names already exists. That is why the shapes
    /// are built here instead of at registration.
    fn impl_target_arguments(
        &mut self,
        type_name: &ast::TypeName,
        params: &[ast::GenericParam],
    ) -> Result<Vec<TargetArg>, Diagnostic> {
        let Some(generic) = generic_impl_target(type_name) else {
            return Err(Diagnostic::new(
                type_name.span,
                "a generic impl target must name a generic record, as in 'impl<T> Box<T>'",
            ));
        };
        let mut arguments = Vec::with_capacity(generic.args.len());
        for argument in &generic.args {
            let target = match &argument.kind {
                ast::GenericArgKind::Type(type_name) => {
                    TargetArg::Type(self.target_type_pattern(type_name, params)?)
                }
                ast::GenericArgKind::Const(expr) => {
                    TargetArg::Const(self.target_const_pattern(expr, params)?)
                }
            };
            arguments.push(target);
        }
        Ok(arguments)
    }

    /// The shape of one type in a generic impl target.
    ///
    /// A type that names no impl parameter resolves to one concrete identity.
    /// Otherwise its generic, pointer, and array structure stays open, and each
    /// concrete part becomes an exact identity a receiver must agree with.
    fn target_type_pattern(
        &mut self,
        type_name: &ast::TypeName,
        params: &[ast::GenericParam],
    ) -> Result<TypePattern, Diagnostic> {
        if !target_type_mentions_parameter(type_name, params) {
            return Ok(TypePattern::Exact(self.resolve_type_name(type_name)?));
        }
        let base = self.target_spec_pattern(&type_name.specs, params)?;
        match &type_name.declarator {
            Some(declarator) => self.target_declarator_pattern(base, declarator),
            None => Ok(base),
        }
    }

    fn target_spec_pattern(
        &mut self,
        specs: &ast::DeclSpecs,
        params: &[ast::GenericParam],
    ) -> Result<TypePattern, Diagnostic> {
        let [specifier] = specs.type_specs.as_slice() else {
            return Ok(TypePattern::Exact(self.resolve_specs(specs)?.ty));
        };
        match specifier {
            ast::TypeSpecifier::TypedefName(name) => match type_param_index(params, &name.text) {
                Some(index) => Ok(TypePattern::Param(index)),
                None => Ok(TypePattern::Exact(self.resolve_specs(specs)?.ty)),
            },
            ast::TypeSpecifier::Generic(generic) => self.target_generic_pattern(generic, params),
            _ => Ok(TypePattern::Exact(self.resolve_specs(specs)?.ty)),
        }
    }

    fn target_generic_pattern(
        &mut self,
        generic: &ast::GenericSpec,
        params: &[ast::GenericParam],
    ) -> Result<TypePattern, Diagnostic> {
        let Some(kinds) = self.generic_parameter_kinds(&generic.name.text) else {
            return Err(Diagnostic::new(
                generic.name.span,
                format!("'{}' is not a generic record or enum", generic.name.text),
            ));
        };
        if generic.args.len() != kinds.len() {
            return Err(Diagnostic::new(
                generic.span,
                format!(
                    "generic impl target names '{}' with {} argument(s), but it has {} \
                     parameter(s)",
                    generic.name.text,
                    generic.args.len(),
                    kinds.len()
                ),
            ));
        }
        let mut arguments = Vec::with_capacity(generic.args.len());
        for (argument, kind) in generic.args.iter().zip(kinds) {
            arguments.push(self.target_pattern_argument(argument, kind, params)?);
        }
        Ok(TypePattern::Generic {
            name: generic.name.text.clone(),
            args: arguments,
        })
    }

    fn target_pattern_argument(
        &mut self,
        argument: &ast::GenericArg,
        kind: ast::GenericParamKind,
        params: &[ast::GenericParam],
    ) -> Result<PatternArg, Diagnostic> {
        match (&argument.kind, kind) {
            (ast::GenericArgKind::Type(type_name), ast::GenericParamKind::Type) => Ok(
                PatternArg::Type(self.target_type_pattern(type_name, params)?),
            ),
            (ast::GenericArgKind::Const(expr), ast::GenericParamKind::Const) => Ok(
                PatternArg::Const(Some(self.target_const_pattern(expr, params)?)),
            ),
            (ast::GenericArgKind::Type(_), ast::GenericParamKind::Const) => Err(Diagnostic::new(
                argument.span,
                "generic impl target writes a type where the generic parameter is a const",
            )),
            (ast::GenericArgKind::Const(_), ast::GenericParamKind::Type) => Err(Diagnostic::new(
                argument.span,
                "generic impl target writes a constant where the generic parameter is a type",
            )),
        }
    }

    fn target_const_pattern(
        &mut self,
        expr: &ast::Expr,
        params: &[ast::GenericParam],
    ) -> Result<ConstPattern, Diagnostic> {
        if let ast::Expr::Name(name) = expr
            && let Some(index) = const_param_index(params, &name.text)
        {
            return Ok(ConstPattern::Param(index));
        }
        let value = self.eval_const_int(expr).map_err(|error| {
            Diagnostic::new(
                error.span,
                format!(
                    "generic impl target constant must be an integer constant expression: {}",
                    error.message
                ),
            )
        })?;
        Ok(ConstPattern::Value(value))
    }

    fn target_declarator_pattern(
        &mut self,
        base: TypePattern,
        declarator: &ast::Declarator,
    ) -> Result<TypePattern, Diagnostic> {
        let mut pattern = base;
        for operator in declarator.derived.iter().rev() {
            pattern = match operator {
                ast::DerivedDeclarator::Pointer(_) => TypePattern::Pointer(Box::new(pattern)),
                ast::DerivedDeclarator::Array(array) => TypePattern::Array {
                    element: Box::new(pattern),
                    len: self.array_length(array)?,
                },
                ast::DerivedDeclarator::Function(_) => {
                    return Err(Diagnostic::new(
                        declarator.span,
                        "a generic impl target cannot mention an impl parameter inside a \
                         function type",
                    ));
                }
            };
        }
        Ok(pattern)
    }

    /// The generic parameters of a registered generic record or enum, by name.
    fn generic_parameter_kinds(&self, name: &str) -> Option<Vec<ast::GenericParamKind>> {
        if let Some(record) = self.generic_records.get(name) {
            return Some(
                record
                    .params
                    .iter()
                    .map(|parameter| parameter.kind.clone())
                    .collect(),
            );
        }
        self.generic_enums.get(name).map(|enumeration| {
            enumeration
                .params
                .iter()
                .map(|parameter| parameter.kind.clone())
                .collect()
        })
    }

    /// Collects one instantiated impl block under its concrete bindings.
    fn materialize_generic_impl(
        &mut self,
        record: TypeId,
        record_name: &str,
        template: &super::GenericImpl,
        args: Vec<GenericArgKey>,
    ) -> Result<(), Diagnostic> {
        self.push_type_scope();
        self.bind_generic_args(&template.params, &args);
        let saved = self.impl_context.clone();
        self.impl_context = Some((record, record_name.to_string()));
        let result = (|| {
            let trait_info = match &template.block.trait_name {
                Some(trait_name) => {
                    let Some(info) = self.traits.get(&trait_name.text).cloned() else {
                        return Err(Diagnostic::new(
                            trait_name.span,
                            format!("unknown trait '{}'", trait_name.text),
                        ));
                    };
                    if template.block.trait_args.len() != info.params.len() {
                        return Err(Diagnostic::new(
                            template.block.span,
                            format!(
                                "trait '{}' expects {} type argument(s)",
                                trait_name.text,
                                info.params.len()
                            ),
                        ));
                    }
                    let trait_args = if info.params.is_empty() {
                        Vec::new()
                    } else {
                        self.resolve_generic_args(
                            &trait_name.text,
                            &info.params,
                            &template.block.trait_args,
                        )?
                    };
                    Some((trait_name.clone(), info, trait_args))
                }
                None => None,
            };
            let trait_impl =
                trait_info
                    .as_ref()
                    .map(|(trait_name, _, trait_args)| super::TraitImplKey {
                        name: trait_name.text.clone(),
                        args: trait_args.clone(),
                    });
            let instance_key = super::GenericImplInstanceKey {
                receiver: record,
                trait_impl: trait_impl.clone(),
            };
            // A recursive lookup reaches this same template again; its own
            // key with its own span means the body is already being collected.
            if let Some(previous) = self.generic_impl_instances.get(&instance_key) {
                if *previous == template.block.span {
                    return Ok(None);
                }
                return Err(Diagnostic::new(
                    template.block.span,
                    format!(
                        "trait '{}' is already implemented for this instantiation of '{}'",
                        trait_impl
                            .as_ref()
                            .map_or("<inherent>", |key| key.name.as_str()),
                        record_name
                    ),
                ));
            }
            if let Some((trait_name, _, trait_args)) = &trait_info
                && self
                    .trait_impls
                    .contains(&(trait_name.text.clone(), trait_args.clone(), record))
            {
                return Err(Diagnostic::new(
                    template.block.span,
                    format!(
                        "trait '{}' is already implemented for this instantiation of '{}'",
                        trait_name.text, record_name
                    ),
                ));
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
            // Mark first: collecting the methods resolves their signatures,
            // which must find this impl already present instead of recursing.
            self.generic_impl_instances
                .insert(instance_key.clone(), template.block.span);
            self.instantiation_depth += 1;
            if let Some((_, info, trait_args)) = &trait_info {
                self.bind_generic_args(&info.params, trait_args);
            }
            let collected = (|| {
                let instance = super::ImplInstance {
                    params: template.params.clone(),
                    args,
                };
                for method in &template.block.methods {
                    self.collect_function(
                        method,
                        Some(super::ImplTarget {
                            ty: record,
                            name: record_name.to_string(),
                            instance: Some(instance.clone()),
                            trait_impl: trait_impl.clone(),
                        }),
                    )?;
                }
                if let Some((_, info, _)) = &trait_info {
                    self.collect_default_methods(
                        &template.block,
                        info,
                        super::ImplTarget {
                            ty: record,
                            name: record_name.to_string(),
                            instance: Some(instance.clone()),
                            trait_impl: trait_impl.clone(),
                        },
                    )?;
                }
                if let Some((trait_name, info, _)) = &trait_info {
                    self.check_impl_method_names(&template.block, trait_name, info, record_name)?;
                    self.check_impl_signatures(
                        &template.block,
                        trait_name,
                        info,
                        record,
                        trait_impl.as_ref(),
                    )?;
                }
                Ok(())
            })();
            self.instantiation_depth -= 1;
            if collected.is_err() {
                self.generic_impl_instances.remove(&instance_key);
            }
            collected?;
            Ok(Some(trait_info))
        })();
        self.impl_context = saved;
        self.pop_type_scope();
        let trait_info = result?;
        if let Some(Some((trait_name, _, trait_args))) = trait_info {
            self.trait_impls
                .insert((trait_name.text.clone(), trait_args.clone(), record));
            self.register_from_conversion(&trait_name, &trait_args, record)?;
        }
        Ok(())
    }

    /// A readable spelling of one concrete generic argument for a diagnostic.
    fn generic_arg_text(&self, argument: GenericArgKey) -> String {
        match argument {
            GenericArgKey::Type(ty) => format!("'{}'", self.concrete_type_name(ty)),
            GenericArgKey::Int(value) => value.to_string(),
        }
    }

    /// The instantiation key of a concrete record, when it came from a generic
    /// record template.
    pub(super) fn record_instance_key(&self, record: TypeId) -> Option<InstanceKey> {
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
        let subject = format!("generic function '{}'", name.text);
        let concrete = self.infer_generic_arguments(
            &subject,
            name.span,
            &template.params,
            decl,
            arguments,
            0,
        )?;
        self.instantiate_concrete(name, &template, concrete)
    }

    /// Instantiates the generic method `name` of `record`, inferring its
    /// arguments from the call's value arguments.
    ///
    /// `skip_params` is one for a receiver call (`value.convert(x)`), whose
    /// receiver is lowered separately, and zero for a static call
    /// (`Circle::convert(value, x)`), whose first argument is the receiver.
    pub(super) fn infer_generic_method(
        &mut self,
        record: TypeId,
        name: &ast::Name,
        arguments: &[(Span, TypeId)],
        skip_params: usize,
    ) -> Result<FunctionInstance, Diagnostic> {
        let template = self.generic_method_template(record, name)?;
        let subject = format!("generic method '{}'", name.text);
        let concrete = self.infer_generic_arguments(
            &subject,
            name.span,
            &template.params,
            &template.decl,
            arguments,
            skip_params,
        )?;
        self.instantiate_concrete_method(record, name, &template, concrete)
    }

    /// The shared inference pass for generic functions and generic methods.
    ///
    /// `skip_params` lets a receiver call ignore the receiver parameter that it
    /// lowers separately, while the argument numbering in diagnostics still
    /// follows the call's own argument list.
    fn infer_generic_arguments(
        &mut self,
        subject: &str,
        span: Span,
        params: &[ast::GenericParam],
        decl: &ast::FunctionDecl,
        arguments: &[(Span, TypeId)],
        skip_params: usize,
    ) -> Result<Vec<GenericArgKey>, Diagnostic> {
        if let Some(parameter) = params
            .iter()
            .find(|parameter| parameter.kind == ast::GenericParamKind::Const)
        {
            return Err(Diagnostic::new(
                span,
                format!(
                    "{subject} needs explicit generic arguments: its const parameter \
                     '{}' cannot be inferred from a value argument",
                    parameter.name.text
                ),
            ));
        }
        let Some(function) = generic_declarator(&decl.declarator) else {
            return Err(Diagnostic::new(
                decl.declarator.span,
                format!("{subject} is not a generic declaration"),
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
            if index < skip_params {
                continue;
            }
            patterns.push(self.parameter_pattern(parameter, params));
        }
        let mut bindings = Bindings::new(params.len());
        for (index, (argument_span, ty)) in arguments.iter().enumerate() {
            let Some(pattern) = patterns.get(index) else {
                break;
            };
            let ty = self.decay_type(*ty);
            match self.unify_pattern(pattern, ty, index, &mut bindings) {
                Ok(()) => {}
                Err(UnifyError::Mismatch) => {
                    return Err(Diagnostic::new(
                        *argument_span,
                        format!(
                            "cannot infer the generic arguments of {subject} from argument \
                             {}: the argument does not match the parameter type; write \
                             explicit generic arguments",
                            index + 1
                        ),
                    ));
                }
                Err(UnifyError::Conflict {
                    parameter,
                    first_argument,
                    ..
                }) => {
                    return Err(Diagnostic::new(
                        *argument_span,
                        format!(
                            "cannot infer generic parameter '{}' of {subject}: argument {} \
                             needs a different type than argument {}; write explicit \
                             generic arguments",
                            params[parameter].name.text,
                            index + 1,
                            first_argument + 1
                        ),
                    ));
                }
            }
        }
        let mut concrete = Vec::with_capacity(params.len());
        for (index, parameter) in params.iter().enumerate() {
            let Some((ty, _)) = bindings.types[index] else {
                return Err(Diagnostic::new(
                    span,
                    format!(
                        "cannot infer generic parameter '{}' of {subject} from the arguments; \
                         write explicit generic arguments",
                        parameter.name.text
                    ),
                ));
            };
            concrete.push(GenericArgKey::Type(ty));
        }
        Ok(concrete)
    }

    /// Instantiates the generic method `name` of `record` with written generic
    /// arguments.
    pub(super) fn instantiate_generic_method(
        &mut self,
        record: TypeId,
        name: &ast::Name,
        args: &[ast::GenericArg],
    ) -> Result<FunctionInstance, Diagnostic> {
        let template = self.generic_method_template(record, name)?;
        if args.len() != template.params.len() {
            return Err(Diagnostic::new(
                name.span,
                format!(
                    "generic method '{}' expects {} argument(s), but {} were given",
                    name.text,
                    template.params.len(),
                    args.len()
                ),
            ));
        }
        // Arguments resolve in the caller's scope, never the template's.
        let concrete =
            self.resolve_generic_args(&format!("method '{}'", name.text), &template.params, args)?;
        self.instantiate_concrete_method(record, name, &template, concrete)
    }

    /// The generic method template of one concrete record, materializing the
    /// record's impl templates first.
    ///
    /// An inherent generic method wins over a trait one. Several traits may
    /// declare a generic method with the same name, and then a receiver call
    /// has no unambiguous template.
    pub(super) fn generic_method_template(
        &mut self,
        record: TypeId,
        name: &ast::Name,
    ) -> Result<GenericMethod, Diagnostic> {
        self.instantiate_generic_impl(record)?;
        let info = self.impls.get(&record);
        if let Some(template) = info.and_then(|info| info.generic_methods.get(&name.text)) {
            return Ok(template.clone());
        }
        let mut matches: Vec<(super::TraitImplKey, GenericMethod)> = info
            .into_iter()
            .flat_map(|info| info.trait_methods.iter())
            .filter_map(|(key, set)| {
                set.generic_methods
                    .get(&name.text)
                    .cloned()
                    .map(|template| (key.clone(), template))
            })
            .collect();
        match matches.len() {
            0 => {}
            1 => return Ok(matches.remove(0).1),
            _ => {
                let mut traits: Vec<String> =
                    matches.into_iter().map(|(key, _)| key.name).collect();
                traits.sort();
                traits.dedup();
                return Err(Diagnostic::new(
                    name.span,
                    format!(
                        "generic method '{}' is ambiguous: it is implemented by traits {}; \
                         an inherent method is needed",
                        name.text,
                        traits
                            .iter()
                            .map(|trait_name| format!("'{trait_name}'"))
                            .collect::<Vec<_>>()
                            .join(" and ")
                    ),
                ));
            }
        }
        if self
            .impls
            .get(&record)
            .is_some_and(|info| info.methods.contains_key(&name.text))
            || self.impls.get(&record).is_some_and(|info| {
                info.trait_methods
                    .values()
                    .any(|set| set.methods.contains_key(&name.text))
            })
        {
            return Err(Diagnostic::new(
                name.span,
                format!("method '{}' is not generic", name.text),
            ));
        }
        Err(Diagnostic::new(
            name.span,
            format!("no method '{}' on this type", name.text),
        ))
    }

    /// Whether `record` has a generic method called `name`, without producing a
    /// diagnostic when it has no method at all.
    pub(super) fn has_generic_method(
        &mut self,
        record: TypeId,
        name: &str,
    ) -> Result<bool, Diagnostic> {
        self.instantiate_generic_impl(record)?;
        Ok(self.impls.get(&record).is_some_and(|info| {
            info.generic_methods.contains_key(name)
                || info
                    .trait_methods
                    .values()
                    .any(|set| set.generic_methods.contains_key(name))
        }))
    }

    /// The shared instantiation tail for one generic method.
    fn instantiate_concrete_method(
        &mut self,
        record: TypeId,
        name: &ast::Name,
        template: &GenericMethod,
        concrete: Vec<GenericArgKey>,
    ) -> Result<FunctionInstance, Diagnostic> {
        if self.instantiation_depth >= MAX_INSTANTIATION_DEPTH {
            return Err(Diagnostic::new(
                name.span,
                format!(
                    "generic method instantiation is nested more than \
                     {MAX_INSTANTIATION_DEPTH} levels deep"
                ),
            ));
        }
        let (_, impl_name) = &template.impl_context;
        let trait_part = template.trait_impl.as_ref().map(|trait_impl| {
            let mut part = Self::sanitize_ident(&trait_impl.name);
            super::push_symbol_args(&mut part, &trait_impl.args);
            part
        });
        let key = InstanceKey {
            name: match &trait_part {
                Some(trait_part) => format!(
                    "method.{}::{}::{}.{}",
                    record.index(),
                    Self::sanitize_ident(impl_name),
                    trait_part,
                    Self::sanitize_ident(&name.text)
                ),
                None => format!(
                    "method.{}::{}.{}",
                    record.index(),
                    Self::sanitize_ident(impl_name),
                    Self::sanitize_ident(&name.text)
                ),
            },
            args: concrete,
        };
        if let Some(instance) = self.function_instances.get(&key) {
            return Ok(instance.clone());
        }
        self.check_generic_bounds(
            &format!("generic method '{}'", name.text),
            name.span,
            &template.params,
            &key.args,
        )?;
        let mut decl = template.decl.clone();
        for derived in &mut decl.declarator.derived {
            if let ast::DerivedDeclarator::Function(function) = derived {
                function.generics.clear();
            }
        }
        let symbol = self.generic_method_symbol(
            record,
            impl_name,
            &name.text,
            template.trait_impl.as_ref(),
            &key.args,
        );
        let ty = self.resolve_generic_method_type(template, &key.args, &decl)?;
        let instance = FunctionInstance {
            symbol: symbol.clone(),
            ty,
        };
        // Cache before the body is lowered, so a recursive generic method call
        // finds its own instance.
        self.function_instances
            .insert(key.clone(), instance.clone());
        let mut env = template.env.clone();
        let method_env = self.generic_env(&template.params, &key.args);
        env.types.extend(method_env.types);
        env.consts.extend(method_env.consts);
        self.pending.push(PendingFunction {
            decl,
            symbol,
            ty,
            impl_context: Some(template.impl_context.clone()),
            env: Some(env),
            internal: true,
        });
        Ok(instance)
    }

    /// Resolves one generic method's signature with the receiver bindings and
    /// the method's own arguments in scope.
    fn resolve_generic_method_type(
        &mut self,
        template: &GenericMethod,
        method_args: &[GenericArgKey],
        decl: &ast::FunctionDecl,
    ) -> Result<TypeId, Diagnostic> {
        self.instantiation_depth += 1;
        self.push_type_scope();
        let saved = self.impl_context.clone();
        self.impl_context = Some(template.impl_context.clone());
        let resolved = (|| {
            self.bind_generic_env(&template.env);
            self.bind_generic_args(&template.params, method_args);
            let ResolvedSpecs { ty, qualifiers } = self.resolve_specs(&decl.specs)?;
            let resolved = self.resolve_declarator(ty, qualifiers, &decl.declarator)?;
            Ok::<TypeId, Diagnostic>(resolved.ty)
        })();
        self.impl_context = saved;
        self.pop_type_scope();
        self.instantiation_depth -= 1;
        let ty = resolved?;
        if self.types.is_function(ty) {
            Ok(ty)
        } else {
            Err(Diagnostic::new(
                decl.span,
                "generic method declaration is not a function",
            ))
        }
    }

    /// Installs one already-concrete receiver environment into the innermost
    /// type scope.
    fn bind_generic_env(&mut self, env: &GenericEnv) {
        let scope = self.type_scopes.last_mut().expect("type scope stack");
        for (name, ty) in &env.types {
            scope.typedefs.insert(name.clone(), *ty);
        }
        for (name, binding) in &env.consts {
            scope.consts.insert(name.clone(), *binding);
        }
    }

    /// Deterministic internal symbol for one generic method instance.
    fn generic_method_symbol(
        &self,
        record: TypeId,
        impl_name: &str,
        method: &str,
        trait_impl: Option<&super::TraitImplKey>,
        args: &[GenericArgKey],
    ) -> String {
        let mut mangled = format!(
            "generic.method.{}.{}.{}",
            record.index(),
            Self::sanitize_ident(impl_name),
            Self::sanitize_ident(method)
        );
        if let Some(trait_impl) = trait_impl {
            mangled.push('.');
            mangled.push_str(&Self::sanitize_ident(&trait_impl.name));
            super::push_symbol_args(&mut mangled, &trait_impl.args);
        }
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

    /// Whether every trait bound on `params` holds for `args`.
    ///
    /// Unlike [`Self::check_generic_bounds`], an unsatisfied bound is an answer
    /// rather than a diagnostic: an impl that does not apply is skipped.
    fn generic_bounds_hold(
        &mut self,
        params: &[ast::GenericParam],
        args: &[GenericArgKey],
    ) -> Result<bool, Diagnostic> {
        for (parameter, argument) in params.iter().zip(args) {
            let GenericArgKey::Type(ty) = argument else {
                continue;
            };
            for bound in &parameter.bounds {
                let Some(info) = self.traits.get(&bound.name.text).cloned() else {
                    return Err(Diagnostic::new(
                        bound.span,
                        format!("unknown trait '{}'", bound.name.text),
                    ));
                };
                if bound.args.len() != info.params.len() {
                    return Err(Diagnostic::new(
                        bound.span,
                        format!(
                            "trait '{}' expects {} type argument(s)",
                            bound.name.text,
                            info.params.len()
                        ),
                    ));
                }
                let bound_args = if bound.args.is_empty() {
                    Vec::new()
                } else {
                    self.push_type_scope();
                    self.bind_generic_args(params, args);
                    let resolved =
                        self.resolve_generic_args(&bound.name.text, &info.params, &bound.args);
                    self.pop_type_scope();
                    resolved?
                };
                self.instantiate_generic_impl(*ty)?;
                if !self
                    .trait_impls
                    .contains(&(bound.name.text.clone(), bound_args, *ty))
                {
                    return Ok(false);
                }
            }
        }
        Ok(true)
    }

    /// Verifies every trait bound once the concrete type arguments are known.
    ///
    /// A bound is satisfied by an `impl Bound<Args> for Type` already collected
    /// for the unit: the same [`Self::trait_impls`] table that rejects a
    /// duplicate implementation answers the check. A bound may write trait
    /// arguments (`T: From<int>`, `U: From<T>`); they resolve in the concrete
    /// environment of the instantiation, so the lookup uses the same type and
    /// constant identities the implementation registered.
    fn check_generic_bounds(
        &mut self,
        subject: &str,
        span: Span,
        params: &[ast::GenericParam],
        args: &[GenericArgKey],
    ) -> Result<(), Diagnostic> {
        for (parameter, argument) in params.iter().zip(args) {
            let GenericArgKey::Type(ty) = argument else {
                continue;
            };
            for bound in &parameter.bounds {
                let Some(info) = self.traits.get(&bound.name.text).cloned() else {
                    return Err(Diagnostic::new(
                        bound.span,
                        format!(
                            "trait bound '{}: {}' on {} names unknown trait '{}'",
                            parameter.name.text, bound.name.text, subject, bound.name.text
                        ),
                    ));
                };
                if bound.args.len() != info.params.len() {
                    return Err(Diagnostic::new(
                        bound.span,
                        format!(
                            "trait bound '{}: {}' on {} gives {} trait argument(s), but trait \
                             '{}' expects {}",
                            parameter.name.text,
                            bound.name.text,
                            subject,
                            bound.args.len(),
                            bound.name.text,
                            info.params.len()
                        ),
                    ));
                }
                // The bound is written in the template's own parameters, so it
                // resolves with the concrete bindings of this instantiation.
                let bound_args = if bound.args.is_empty() {
                    Vec::new()
                } else {
                    self.push_type_scope();
                    self.bind_generic_args(params, args);
                    let resolved =
                        self.resolve_generic_args(&bound.name.text, &info.params, &bound.args);
                    self.pop_type_scope();
                    resolved?
                };
                // The bound may be satisfied by a generic trait impl that this
                // concrete type argument has not needed until now.
                self.instantiate_generic_impl(*ty)?;
                let implemented =
                    self.trait_impls
                        .contains(&(bound.name.text.clone(), bound_args.clone(), *ty));
                if !implemented {
                    return Err(Diagnostic::new(
                        span,
                        format!(
                            "type parameter '{}' of {} requires trait '{}', but '{}' does not \
                             implement it",
                            parameter.name.text,
                            subject,
                            self.bound_trait_name(&bound.name.text, &bound_args),
                            self.concrete_type_name(*ty)
                        ),
                    ));
                }
            }
        }
        Ok(())
    }

    /// `Trait<Args>` with resolved argument spellings, for a bound diagnostic.
    fn bound_trait_name(&self, name: &str, args: &[GenericArgKey]) -> String {
        if args.is_empty() {
            return name.to_string();
        }
        let args = args
            .iter()
            .map(|argument| match argument {
                GenericArgKey::Type(ty) => self.concrete_type_name(*ty),
                GenericArgKey::Int(value) => value.to_string(),
            })
            .collect::<Vec<_>>()
            .join(", ");
        format!("{name}<{args}>")
    }

    /// A readable spelling of a concrete type for a bound diagnostic.
    ///
    /// The type store keeps no names, so a record or enum is named from the
    /// metadata that already refers to it: its implementation, its generic
    /// instance, or the scope that declared its tag.
    pub(super) fn concrete_type_name(&self, ty: TypeId) -> String {
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
        self.check_generic_bounds(
            &format!("generic function '{}'", name.text),
            name.span,
            &template.params,
            &key.args,
        )?;
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
            TypePattern::Array { element, .. } => TypePattern::Pointer(element),
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
                match type_param_index(type_params, &name.text) {
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
                        ast::GenericArgKind::Const(_) => PatternArg::Const(None),
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
                ast::DerivedDeclarator::Array(_) if pattern.is_open() => TypePattern::Array {
                    element: Box::new(pattern),
                    len: None,
                },
                // A type parameter behind a function type has no value shape to
                // match, so the whole part stays unconstrained.
                _ => TypePattern::Concrete,
            };
        }
        pattern
    }

    /// Binds the open parameters of `pattern` against one argument type.
    fn unify_pattern(
        &self,
        pattern: &TypePattern,
        ty: TypeId,
        argument: usize,
        bindings: &mut Bindings,
    ) -> Result<(), UnifyError> {
        match pattern {
            TypePattern::Concrete => Ok(()),
            TypePattern::Exact(expected) => {
                if self.types.is_compatible(*expected, ty) {
                    Ok(())
                } else {
                    Err(UnifyError::Mismatch)
                }
            }
            TypePattern::Param(index) => match bindings.types[*index] {
                None => {
                    bindings.types[*index] = Some((ty, argument));
                    Ok(())
                }
                Some((existing, _)) if self.types.is_compatible(existing, ty) => Ok(()),
                Some((existing, first_argument)) => Err(UnifyError::Conflict {
                    parameter: *index,
                    first_argument,
                    first: GenericArgKey::Type(existing),
                    second: GenericArgKey::Type(ty),
                }),
            },
            TypePattern::Pointer(pointee) => {
                let TypeKind::Pointer { pointee: actual } = self.types.kind(ty) else {
                    return Err(UnifyError::Mismatch);
                };
                self.unify_pattern(pointee, actual.ty, argument, bindings)
            }
            TypePattern::Array { element, len } => {
                let TypeKind::Array {
                    element: actual,
                    len: actual_len,
                } = self.types.kind(ty)
                else {
                    return Err(UnifyError::Mismatch);
                };
                if len.is_some_and(|expected| *actual_len != Some(expected)) {
                    return Err(UnifyError::Mismatch);
                }
                self.unify_pattern(element, actual.ty, argument, bindings)
            }
            TypePattern::Generic { name, args } => {
                let Some(actual_args) = self.generic_instance_args(name, ty) else {
                    return Err(UnifyError::Mismatch);
                };
                if args.len() != actual_args.len() {
                    return Err(UnifyError::Mismatch);
                }
                for (pattern, actual) in args.iter().zip(actual_args) {
                    match (pattern, actual) {
                        (PatternArg::Type(pattern), GenericArgKey::Type(actual)) => {
                            self.unify_pattern(pattern, *actual, argument, bindings)?;
                        }
                        (
                            PatternArg::Const(Some(ConstPattern::Param(index))),
                            GenericArgKey::Int(value),
                        ) => match bindings.consts[*index] {
                            None => bindings.consts[*index] = Some((*value, argument)),
                            Some((previous, _)) if previous == *value => {}
                            Some((previous, first_argument)) => {
                                return Err(UnifyError::Conflict {
                                    parameter: *index,
                                    first_argument,
                                    first: GenericArgKey::Int(previous),
                                    second: GenericArgKey::Int(*value),
                                });
                            }
                        },
                        (
                            PatternArg::Const(Some(ConstPattern::Value(expected))),
                            GenericArgKey::Int(value),
                        ) if expected == value => {}
                        // An open const slot never binds during inference, and
                        // a mismatched kind or value means the shapes differ.
                        (PatternArg::Const(None), _) => {}
                        _ => return Err(UnifyError::Mismatch),
                    }
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

/// The position of the type parameter called `name`.
fn type_param_index(params: &[ast::GenericParam], name: &str) -> Option<usize> {
    params.iter().position(|parameter| {
        parameter.kind == ast::GenericParamKind::Type && parameter.name.text == name
    })
}

/// The position of the const parameter called `name`.
fn const_param_index(params: &[ast::GenericParam], name: &str) -> Option<usize> {
    params.iter().position(|parameter| {
        parameter.kind == ast::GenericParamKind::Const && parameter.name.text == name
    })
}

/// Whether a written generic argument is a type or a constant.
fn generic_argument_kind(argument: &ast::GenericArg) -> ast::GenericParamKind {
    match &argument.kind {
        ast::GenericArgKind::Type(_) => ast::GenericParamKind::Type,
        ast::GenericArgKind::Const(_) => ast::GenericParamKind::Const,
    }
}

fn generic_parameter_kind_text(kind: &ast::GenericParamKind) -> &'static str {
    match kind {
        ast::GenericParamKind::Type => "type parameter",
        ast::GenericParamKind::Const => "const parameter",
    }
}

fn generic_argument_kind_text(kind: &ast::GenericParamKind) -> &'static str {
    match kind {
        ast::GenericParamKind::Type => "type",
        ast::GenericParamKind::Const => "constant",
    }
}

/// Marks every impl parameter that one target argument mentions, including
/// parameters inside a nested generic use or a function type.
fn mark_target_params(argument: &ast::GenericArg, params: &[ast::GenericParam], used: &mut [bool]) {
    match &argument.kind {
        ast::GenericArgKind::Type(type_name) => mark_type_params(type_name, params, used),
        ast::GenericArgKind::Const(ast::Expr::Name(name)) => {
            if let Some(index) = const_param_index(params, &name.text) {
                used[index] = true;
            }
        }
        ast::GenericArgKind::Const(_) => {}
    }
}

fn mark_type_params(type_name: &ast::TypeName, params: &[ast::GenericParam], used: &mut [bool]) {
    mark_specs_params(&type_name.specs, params, used);
    if let Some(declarator) = &type_name.declarator {
        mark_declarator_params(declarator, params, used);
    }
}

fn mark_specs_params(specs: &ast::DeclSpecs, params: &[ast::GenericParam], used: &mut [bool]) {
    for specifier in &specs.type_specs {
        match specifier {
            ast::TypeSpecifier::TypedefName(name) => {
                if let Some(index) = type_param_index(params, &name.text) {
                    used[index] = true;
                }
            }
            ast::TypeSpecifier::Generic(generic) => {
                for argument in &generic.args {
                    mark_target_params(argument, params, used);
                }
            }
            ast::TypeSpecifier::Atomic(inner) => mark_type_params(inner, params, used),
            _ => {}
        }
    }
}

fn mark_declarator_params(
    declarator: &ast::Declarator,
    params: &[ast::GenericParam],
    used: &mut [bool],
) {
    for operator in &declarator.derived {
        let ast::DerivedDeclarator::Function(function) = operator else {
            continue;
        };
        for parameter in &function.params {
            mark_specs_params(&parameter.specs, params, used);
            if let Some(declarator) = &parameter.declarator {
                mark_declarator_params(declarator, params, used);
            }
        }
    }
}

/// Whether a target type mentions any impl parameter.
fn target_type_mentions_parameter(type_name: &ast::TypeName, params: &[ast::GenericParam]) -> bool {
    specs_mention_parameter(&type_name.specs, params)
        || type_name
            .declarator
            .as_ref()
            .is_some_and(|declarator| declarator_mentions_parameter(declarator, params))
}

fn specs_mention_parameter(specs: &ast::DeclSpecs, params: &[ast::GenericParam]) -> bool {
    specs.type_specs.iter().any(|specifier| match specifier {
        ast::TypeSpecifier::TypedefName(name) => type_param_index(params, &name.text).is_some(),
        ast::TypeSpecifier::Generic(generic) => generic
            .args
            .iter()
            .any(|argument| target_argument_mentions_parameter(argument, params)),
        ast::TypeSpecifier::Atomic(inner) => target_type_mentions_parameter(inner, params),
        _ => false,
    })
}

fn target_argument_mentions_parameter(
    argument: &ast::GenericArg,
    params: &[ast::GenericParam],
) -> bool {
    match &argument.kind {
        ast::GenericArgKind::Type(type_name) => target_type_mentions_parameter(type_name, params),
        ast::GenericArgKind::Const(ast::Expr::Name(name)) => {
            const_param_index(params, &name.text).is_some()
        }
        ast::GenericArgKind::Const(_) => false,
    }
}

fn declarator_mentions_parameter(
    declarator: &ast::Declarator,
    params: &[ast::GenericParam],
) -> bool {
    declarator.derived.iter().any(|operator| match operator {
        ast::DerivedDeclarator::Function(function) => function.params.iter().any(|parameter| {
            specs_mention_parameter(&parameter.specs, params)
                || parameter
                    .declarator
                    .as_ref()
                    .is_some_and(|declarator| declarator_mentions_parameter(declarator, params))
        }),
        _ => false,
    })
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
