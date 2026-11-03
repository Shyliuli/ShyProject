//! File-scope declarations, global data, and local variable initialization.
//!
//! Global objects are built as target bytes plus relocations, because the IR
//! exposes raw initialized memory. Local objects get a slot and, when they
//! have an initializer, explicit stores or a `MemSet`/`MemCpy` sequence. A
//! `let` local is the same kind of slot; only the inferred type and the
//! read-only flag differ.

use std::collections::HashSet;

use super::const_eval::ConstKind;
use super::generics::generic_declarator;
use super::types::ResolvedSpecsWithName;
use super::{FnLower, GlobalRef, Local, Lowerer, ObjectInfo, PendingFunction, ResolvedSpecs};
use crate::ast;
use crate::ir;
use crate::sema::types::{QualifiedType, Qualifiers, TypeId, TypeKind};
use crate::source::{Diagnostic, Span};

impl Lowerer {
    pub(super) fn collect(&mut self, unit: &ast::TranslationUnit) -> Result<(), Diagnostic> {
        // Traits only describe compile-time signatures, so they are known
        // before any implementation is resolved.
        self.register_traits(unit)?;
        self.register_generic_records(unit)?;
        self.register_generic_enums(unit)?;
        self.register_generic_impls(unit)?;
        self.register_generic_functions(unit)?;
        for item in &unit.items {
            match item {
                ast::Item::Decl(declaration) => self.collect_declaration(declaration)?,
                ast::Item::Function(function) => self.collect_function(function, None)?,
                ast::Item::Impl(block) => self.collect_impl(block)?,
                // Registered by `register_traits`; nothing to emit.
                ast::Item::Trait(_) => {}
                // Checked after the loop, once every file-scope name exists.
                ast::Item::Use(_) => {}
            }
        }
        self.check_imports(unit)?;
        // Every file-scope type is known now, so trait signatures that never
        // met an implementation still get their types checked.
        self.check_trait_signatures(unit)?;
        Ok(())
    }

    /// Check named imports after preprocessing has loaded their module headers.
    /// Includes share the ordinary C file-scope namespace; a glob needs no
    /// additional name check. Repeating a named import in the same source file
    /// is diagnosed independently of imports in other headers.
    fn check_imports(&self, unit: &ast::TranslationUnit) -> Result<(), Diagnostic> {
        let mut imported = HashSet::new();
        for item in &unit.items {
            let ast::Item::Use(import) = item else {
                continue;
            };
            if import.glob {
                continue;
            }
            let Some(name) = import.path.last() else {
                continue;
            };
            if !imported.insert((import.span.file, name.text.as_str())) {
                return Err(Diagnostic::new(
                    import.span,
                    format!("duplicate import of '{}'", name.text),
                ));
            }
            if !self.is_file_scope_name(&name.text) {
                let path = import
                    .path
                    .iter()
                    .map(|segment| segment.text.as_str())
                    .collect::<Vec<_>>()
                    .join("::");
                return Err(Diagnostic::new(
                    import.span,
                    format!(
                        "cannot import '{path}': no file-scope declaration named '{}' is visible",
                        name.text
                    ),
                ));
            }
        }
        Ok(())
    }

    /// Whether `name` is declared at file scope: a type name, tag, trait,
    /// enumerator, generic template, object, or function.
    fn is_file_scope_name(&self, name: &str) -> bool {
        self.lookup_typedef(name).is_some()
            || self.lookup_tag(name).is_some()
            || self.traits.contains_key(name)
            || self.enum_consts.contains_key(name)
            || self.generic_records.contains_key(name)
            || self.generic_enums.contains_key(name)
            || self.generic_functions.contains_key(name)
            || self.globals.contains_key(name)
    }

    /// Registers every generic record definition before anything is resolved.
    ///
    /// A use may appear before the definition it instantiates, so all templates
    /// are collected in one pass first. Generic records never enter the plain
    /// tag table: only an instantiation creates a record type.
    fn register_generic_records(&mut self, unit: &ast::TranslationUnit) -> Result<(), Diagnostic> {
        for item in &unit.items {
            let ast::Item::Decl(declaration) = item else {
                continue;
            };
            let Some(record) = generic_definition(&declaration.specs) else {
                continue;
            };
            let Some(tag) = &record.tag else {
                return Err(Diagnostic::new(
                    record.span,
                    "a generic record definition needs a tag",
                ));
            };
            if record.members.is_none() {
                return Err(Diagnostic::new(
                    record.span,
                    format!("generic record '{}' needs a definition", tag.text),
                ));
            }
            if self.generic_records.contains_key(&tag.text) {
                return Err(Diagnostic::new(
                    record.span,
                    format!("redefinition of generic record '{}'", tag.text),
                ));
            }
            if let Some(bound) = record
                .generics
                .iter()
                .find_map(|parameter| parameter.bounds.first())
            {
                return Err(Diagnostic::new(
                    bound.span,
                    format!(
                        "trait bounds on generic record '{}' are not implemented yet",
                        tag.text
                    ),
                ));
            }
            self.generic_records.insert(
                tag.text.clone(),
                super::GenericRecord {
                    params: record.generics.clone(),
                    spec: record.clone(),
                },
            );
        }
        Ok(())
    }

    /// Registers every generic enum definition before anything is resolved.
    ///
    /// Like a generic record, a generic enum never enters the plain tag table:
    /// only an instantiation creates an enum type.
    fn register_generic_enums(&mut self, unit: &ast::TranslationUnit) -> Result<(), Diagnostic> {
        for item in &unit.items {
            let ast::Item::Decl(declaration) = item else {
                continue;
            };
            let Some(enumeration) = generic_enum_definition(&declaration.specs) else {
                continue;
            };
            let Some(tag) = &enumeration.tag else {
                return Err(Diagnostic::new(
                    enumeration.span,
                    "a generic enum definition needs a tag",
                ));
            };
            if enumeration.enumerators.is_none() {
                return Err(Diagnostic::new(
                    enumeration.span,
                    format!("generic enum '{}' needs a definition", tag.text),
                ));
            }
            if self.generic_enums.contains_key(&tag.text) {
                return Err(Diagnostic::new(
                    enumeration.span,
                    format!("redefinition of generic enum '{}'", tag.text),
                ));
            }
            if let Some(bound) = enumeration
                .generics
                .iter()
                .find_map(|parameter| parameter.bounds.first())
            {
                return Err(Diagnostic::new(
                    bound.span,
                    format!(
                        "trait bounds on generic enum '{}' are not implemented yet",
                        tag.text
                    ),
                ));
            }
            self.generic_enums.insert(
                tag.text.clone(),
                super::GenericEnum {
                    params: enumeration.generics.clone(),
                    spec: enumeration.clone(),
                },
            );
        }
        Ok(())
    }

    fn collect_declaration(&mut self, declaration: &ast::Declaration) -> Result<(), Diagnostic> {
        // The registration pass already holds the definition, so a generic
        // definition has nothing left to resolve. Declarators next to one would
        // use the generic type without its arguments.
        let generic_record = generic_definition(&declaration.specs);
        let generic_enum = generic_enum_definition(&declaration.specs);
        if generic_record.is_some() || generic_enum.is_some() {
            if let Some(init) = declaration.declarators.first() {
                let kind = if generic_enum.is_some() {
                    "enum"
                } else {
                    "record"
                };
                return Err(Diagnostic::new(
                    init.span,
                    format!("a generic {kind} needs arguments before it can be used"),
                ));
            }
            return Ok(());
        }
        // A `fn` template is not a concrete function yet; its prototype has no
        // symbol and no file-scope name until an instantiation needs one.
        if declaration
            .declarators
            .iter()
            .any(|init| generic_declarator(&init.declarator).is_some())
        {
            return Ok(());
        }
        for init in &declaration.declarators {
            self.reject_generic_declarator(&init.declarator)?;
        }
        let ResolvedSpecs { ty, qualifiers } = self.resolve_specs(&declaration.specs)?;
        let storage = declaration.specs.storage;
        if storage == Some(ast::StorageClass::Typedef) {
            for init in &declaration.declarators {
                if init.init.is_some() {
                    return Err(Diagnostic::new(
                        init.span,
                        "a typedef cannot have an initializer",
                    ));
                }
                let resolved = self.resolve_declarator(ty, qualifiers, &init.declarator)?;
                let Some(name) = resolved.name else {
                    return Err(Diagnostic::new(
                        init.span,
                        "typedef declaration needs a name",
                    ));
                };
                self.declare_typedef(&name, resolved.ty, init.span)?;
            }
            return Ok(());
        }
        if storage == Some(ast::StorageClass::Auto) || storage == Some(ast::StorageClass::Register)
        {
            return Err(Diagnostic::new(
                declaration.span,
                "`auto` and `register` are not allowed at file scope",
            ));
        }
        for init in &declaration.declarators {
            let resolved = self.resolve_object_type(
                ty,
                qualifiers,
                &init.declarator,
                init.init.as_ref(),
                init.span,
            )?;
            let Some(name) = resolved.name else {
                return Err(Diagnostic::new(init.span, "declaration needs a name"));
            };
            if self.types.is_function(resolved.ty) {
                if init.init.is_some() {
                    return Err(Diagnostic::new(
                        init.span,
                        "a function cannot have an initializer",
                    ));
                }
                // 这里只是一条声明（原型），名字要按对外符号走。
                let symbol = self.declared_function_symbol(
                    &name,
                    storage,
                    declaration.specs.is_shy_fn,
                    declaration.specs.is_pub,
                    false,
                );
                self.declare_function_symbol(&name, &symbol, resolved.ty, false, init.span)?;
            } else {
                self.collect_object(
                    &name,
                    resolved.ty,
                    resolved.qualifiers,
                    storage,
                    declaration.specs.is_pub,
                    init.init.as_ref(),
                    init.span,
                )?;
            }
        }
        Ok(())
    }

    pub(super) fn collect_function(
        &mut self,
        declaration: &ast::FunctionDecl,
        target: Option<super::ImplTarget>,
    ) -> Result<(), Diagnostic> {
        if generic_declarator(&declaration.declarator).is_some() {
            let Some(target) = target else {
                // A file-scope template was registered by the collection
                // pre-pass; only an instantiation creates a concrete function.
                return Ok(());
            };
            return self.collect_generic_method(declaration, target);
        }
        self.reject_generic_declarator(&declaration.declarator)?;
        let ResolvedSpecs { ty, qualifiers } = self.resolve_specs(&declaration.specs)?;
        let resolved = self.resolve_declarator(ty, qualifiers, &declaration.declarator)?;
        let Some(name) = resolved.name else {
            return Err(Diagnostic::new(
                declaration.span,
                "function definition needs a name",
            ));
        };
        let TypeKind::Function { .. } = self.types.kind(resolved.ty) else {
            return Err(Diagnostic::new(
                declaration.span,
                "declarator is not a function",
            ));
        };
        // An `impl` method lives in its own namespace, so the mangled symbol is
        // also its name-table key. A file-scope function is keyed by its C name
        // so that later declarations resolve to it; only the emitted symbol
        // differs for `static`.
        let (key, symbol) = match &target {
            Some(target) => {
                let symbol = match (&target.instance, &target.trait_impl) {
                    (Some(_), Some(trait_impl)) => self.generic_trait_impl_method_symbol(
                        trait_impl,
                        &target.name,
                        &name,
                        target.ty,
                    ),
                    (Some(_), None) => {
                        self.generic_impl_method_symbol(&target.name, &name, target.ty)
                    }
                    (None, Some(trait_impl)) => {
                        let symbol = self.trait_impl_method_symbol(trait_impl, &target.name, &name);
                        // A method follows the same rule as a free function:
                        // `pub` exports it, everything else stays file-local, so
                        // a header can define a trait implementation without
                        // duplicating the symbol in every translation unit.
                        if self.file_local(
                            declaration.specs.storage,
                            declaration.specs.is_shy_fn,
                            declaration.specs.is_pub,
                            true,
                        ) {
                            self.internal_symbol(&symbol)
                        } else {
                            symbol
                        }
                    }
                    (None, None) => self.inherent_method_symbol(
                        &target.name,
                        &name,
                        declaration.specs.storage,
                        declaration.specs.is_shy_fn,
                        declaration.specs.is_pub,
                        true,
                    ),
                };
                (symbol.clone(), symbol)
            }
            None => {
                let symbol = self.declared_function_symbol(
                    &name,
                    declaration.specs.storage,
                    declaration.specs.is_shy_fn,
                    declaration.specs.is_pub,
                    true,
                );
                (name.clone(), symbol)
            }
        };
        self.declare_function_symbol(&key, &symbol, resolved.ty, true, declaration.span)?;
        // A method of a materialized generic impl keeps only its signature:
        // the body is queued when a lookup first needs it, so an instance
        // compiles the methods it uses. Everything else is lowered now.
        let plain = target
            .as_ref()
            .is_none_or(|target| target.instance.is_none());
        let deferred = match &target {
            Some(target) => target
                .instance
                .as_ref()
                .map(|instance| super::DeferredMethod {
                    decl: declaration.clone(),
                    env: self.generic_env(&instance.params, &instance.args),
                    impl_context: (target.ty, target.name.clone()),
                }),
            None => None,
        };
        if let Some(target) = &target {
            let info = self
                .impls
                .entry(target.ty)
                .or_insert_with(|| super::ImplInfo::new(target.name.clone()));
            match &target.trait_impl {
                Some(trait_impl) => {
                    let set = info.trait_methods.entry(trait_impl.clone()).or_default();
                    if set.methods.contains_key(&name) || set.generic_methods.contains_key(&name) {
                        return Err(Diagnostic::new(
                            declaration.span,
                            format!("redefinition of method '{name}'"),
                        ));
                    }
                    set.methods.insert(
                        name,
                        super::MethodInfo {
                            symbol: symbol.clone(),
                            ty: resolved.ty,
                            deferred,
                        },
                    );
                }
                None => {
                    // A prototype of the same method may come first; a second
                    // definition was already rejected by declare_function_symbol.
                    let conflict = info.generic_methods.contains_key(&name)
                        || info
                            .methods
                            .get(&name)
                            .is_some_and(|previous| previous.symbol != symbol);
                    if conflict {
                        return Err(Diagnostic::new(
                            declaration.span,
                            format!("redefinition of method '{name}'"),
                        ));
                    }
                    info.methods.insert(
                        name,
                        super::MethodInfo {
                            symbol: symbol.clone(),
                            ty: resolved.ty,
                            deferred,
                        },
                    );
                }
            }
        }
        if plain {
            self.pending.push(PendingFunction {
                decl: declaration.clone(),
                symbol,
                ty: resolved.ty,
                impl_context: target
                    .as_ref()
                    .map(|target| (target.ty, target.name.clone())),
                env: None,
                internal: false,
            });
        }
        Ok(())
    }

    /// `____Type__method`, scoped to the translation unit when `static`, so a
    /// header can also define a private helper method.
    fn inherent_method_symbol(
        &self,
        type_name: &str,
        method: &str,
        storage: Option<ast::StorageClass>,
        is_shy_fn: bool,
        is_pub: bool,
        defined: bool,
    ) -> String {
        let symbol = format!(
            "____{}__{}",
            Self::sanitize_ident(type_name),
            Self::sanitize_ident(method)
        );
        // 头文件里的原型先登记过：定义沿用原型定下的符号，两边才一致。
        if let Some(GlobalRef::Function(index)) = self.global_ref(&symbol) {
            return self.function(index).symbol.clone();
        }
        if self.file_local(storage, is_shy_fn, is_pub, defined) {
            self.internal_symbol(&symbol)
        } else {
            symbol
        }
    }

    /// Registers a bodiless method of an inherent impl: the method table gets
    /// its signature and the symbol is declared but not defined, exactly like
    /// a C prototype. The definition may follow in this unit or another one.
    pub(super) fn collect_method_prototype(
        &mut self,
        prototype: &ast::MethodPrototype,
        ty: TypeId,
        type_name: &str,
    ) -> Result<(), Diagnostic> {
        self.reject_generic_declarator(&prototype.declarator)?;
        let ResolvedSpecs {
            ty: base,
            qualifiers,
        } = self.resolve_specs(&prototype.specs)?;
        let resolved = self.resolve_declarator(base, qualifiers, &prototype.declarator)?;
        let Some(name) = resolved.name else {
            return Err(Diagnostic::new(
                prototype.span,
                "method declaration needs a name",
            ));
        };
        if !self.types.is_function(resolved.ty) {
            return Err(Diagnostic::new(
                prototype.span,
                "declarator is not a function",
            ));
        }
        let symbol = self.inherent_method_symbol(
            type_name,
            &name,
            prototype.specs.storage,
            prototype.specs.is_shy_fn,
            prototype.specs.is_pub,
            false,
        );
        self.declare_function_symbol(&symbol, &symbol, resolved.ty, false, prototype.span)?;
        let info = self
            .impls
            .entry(ty)
            .or_insert_with(|| super::ImplInfo::new(type_name.to_string()));
        if info.generic_methods.contains_key(&name)
            || info
                .methods
                .get(&name)
                .is_some_and(|previous| previous.symbol != symbol)
        {
            return Err(Diagnostic::new(
                prototype.span,
                format!("conflicting declaration of method '{name}'"),
            ));
        }
        info.methods.entry(name).or_insert(super::MethodInfo {
            symbol,
            ty: resolved.ty,
            deferred: None,
        });
        Ok(())
    }

    /// Registers one generic method of an impl as a template.
    ///
    /// A generic method has no single concrete signature, so it is not entered
    /// in the ordinary method table. The first call that names or infers its
    /// arguments instantiates it through the generic function machinery.
    fn collect_generic_method(
        &mut self,
        declaration: &ast::FunctionDecl,
        target: super::ImplTarget,
    ) -> Result<(), Diagnostic> {
        let Some(function) = generic_declarator(&declaration.declarator) else {
            return Ok(());
        };
        let Some(name) = &declaration.declarator.name else {
            return Err(Diagnostic::new(
                declaration.span,
                "method definition needs a name",
            ));
        };
        for (index, parameter) in function.generics.iter().enumerate() {
            if function.generics[..index]
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
            if target.instance.as_ref().is_some_and(|instance| {
                instance
                    .params
                    .iter()
                    .any(|outer| outer.name.text == parameter.name.text)
            }) {
                return Err(Diagnostic::new(
                    parameter.span,
                    format!(
                        "generic method parameter '{}' shadows an impl parameter of the same \
                         name",
                        parameter.name.text
                    ),
                ));
            }
        }
        let env = match &target.instance {
            Some(instance) => self.generic_env(&instance.params, &instance.args),
            None => super::GenericEnv {
                types: Default::default(),
                consts: Default::default(),
            },
        };
        let info = self
            .impls
            .entry(target.ty)
            .or_insert_with(|| super::ImplInfo::new(target.name.clone()));
        let generic_methods = match &target.trait_impl {
            Some(trait_impl) => {
                &mut info
                    .trait_methods
                    .entry(trait_impl.clone())
                    .or_default()
                    .generic_methods
            }
            None => &mut info.generic_methods,
        };
        if generic_methods.contains_key(&name.text) {
            return Err(Diagnostic::new(
                declaration.span,
                format!("redefinition of method '{}'", name.text),
            ));
        }
        generic_methods.insert(
            name.text.clone(),
            super::GenericMethod {
                params: function.generics.clone(),
                decl: declaration.clone(),
                env,
                impl_context: (target.ty, target.name),
                trait_impl: target.trait_impl,
            },
        );
        Ok(())
    }

    /// The emitted symbol a declaration of `name` would use.
    ///
    /// An existing function keeps the symbol its first declaration chose, so a
    /// prototype and a later definition always agree.
    pub(super) fn declared_function_symbol(
        &self,
        name: &str,
        storage: Option<ast::StorageClass>,
        is_shy_fn: bool,
        is_pub: bool,
        defined: bool,
    ) -> String {
        if let Some(GlobalRef::Function(index)) = self.global_ref(name) {
            return self.function(index).symbol.clone();
        }
        if self.file_local(storage, is_shy_fn, is_pub, defined) {
            self.internal_symbol(name)
        } else {
            name.to_string()
        }
    }

    /// Whether a declaration gets a file-local symbol.
    ///
    /// `static` always does. A Shy `fn` or method does too, because ShyC makes
    /// file-local the default; `#![disable_default_static]` restores the C
    /// default for a file. A C-style declaration is only affected by `static`.
    fn file_local(
        &self,
        storage: Option<ast::StorageClass>,
        is_shy_fn: bool,
        is_pub: bool,
        defined: bool,
    ) -> bool {
        if storage == Some(ast::StorageClass::Static) {
            return true;
        }
        if is_pub {
            // 显式 `pub`：文件级默认被它关掉，就是导出。
            return false;
        }
        // 原型声明的是别处的东西，永远按对外名字走；只有带函数体的定义
        // 才吃「ShyC 默认文件级私有」这条规则。
        is_shy_fn && defined && !self.disable_default_static
    }

    /// Register a function, checking redeclaration compatibility.
    ///
    /// `name` is the C identifier and the key of the file-scope name table;
    /// `symbol` is the name the backend emits, which differs for a `static`
    /// function. The first declaration fixes the emitted symbol.
    pub(super) fn declare_function_symbol(
        &mut self,
        name: &str,
        symbol: &str,
        ty: TypeId,
        defined: bool,
        span: Span,
    ) -> Result<usize, Diagnostic> {
        match self.global_ref(name) {
            Some(GlobalRef::Function(index)) => {
                let (previous_ty, previous_defined) = {
                    let function = self.function(index);
                    (function.ty, function.defined)
                };
                if !self.types.is_compatible(previous_ty, ty) {
                    return Err(Diagnostic::new(
                        span,
                        format!("conflicting declaration of function '{name}'"),
                    ));
                }
                if defined && previous_defined {
                    return Err(Diagnostic::new(
                        span,
                        format!("redefinition of function '{name}'"),
                    ));
                }
                self.functions[index].defined = previous_defined || defined;
                Ok(index)
            }
            Some(GlobalRef::Object(_)) => Err(Diagnostic::new(
                span,
                format!("'{name}' is redeclared as a function"),
            )),
            None => {
                let index = self.functions.len();
                self.functions.push(super::FunctionInfo {
                    symbol: symbol.to_string(),
                    ty,
                    defined,
                });
                self.globals
                    .insert(name.to_string(), GlobalRef::Function(index));
                Ok(index)
            }
        }
    }

    fn collect_object(
        &mut self,
        name: &str,
        ty: TypeId,
        qualifiers: Qualifiers,
        storage: Option<ast::StorageClass>,
        is_pub: bool,
        init: Option<&ast::Initializer>,
        span: Span,
    ) -> Result<(), Diagnostic> {
        let is_extern = storage == Some(ast::StorageClass::Extern);
        // 全局变量和函数同一套规则：`pub` 导出，`static` 私有，
        // 两者都不写就是本单元私有（`#![disable_default_static]` 切回 C 默认）。
        // `extern` 声明的是别处的东西，名字永远按对外符号走。
        let symbol = if storage == Some(ast::StorageClass::Static)
            || (!is_pub && !is_extern && !self.disable_default_static)
        {
            self.internal_symbol(name)
        } else {
            name.to_string()
        };
        let defined = init.is_some() || !is_extern;
        let index = match self.global_ref(name) {
            Some(GlobalRef::Function(_)) => {
                return Err(Diagnostic::new(
                    span,
                    format!("'{name}' is redeclared as an object"),
                ));
            }
            Some(GlobalRef::Object(index)) => {
                let (previous_ty, previous_init) = {
                    let object = self.object(index);
                    (object.ty, object.has_init)
                };
                if previous_init && init.is_some() {
                    return Err(Diagnostic::new(
                        span,
                        format!("redefinition of global object '{name}'"),
                    ));
                }
                if !self.types.is_compatible(previous_ty, ty) {
                    let incomplete_array = matches!(
                        self.types.kind(previous_ty),
                        TypeKind::Array { len: None, .. }
                    ) && self.types.is_array(ty);
                    if !incomplete_array {
                        return Err(Diagnostic::new(
                            span,
                            format!("conflicting declaration of global object '{name}'"),
                        ));
                    }
                    if init.is_none() {
                        return Err(Diagnostic::new(
                            span,
                            "array declaration without a bound after a bounded declaration",
                        ));
                    }
                }
                if init.is_some() || self.object(index).data.is_none() {
                    self.objects[index].ty = ty;
                }
                if init.is_some() {
                    self.objects[index].has_init = true;
                }
                index
            }
            None => {
                let index = self.objects.len();
                self.objects.push(ObjectInfo {
                    symbol: symbol.clone(),
                    ty,
                    qualifiers,
                    has_init: init.is_some(),
                    data: None,
                });
                self.globals
                    .insert(name.to_string(), GlobalRef::Object(index));
                index
            }
        };
        if !defined {
            return Ok(());
        }
        if init.is_none()
            && matches!(
                self.types.kind(self.objects[index].ty),
                TypeKind::Array { len: None, .. }
            )
        {
            return Err(Diagnostic::new(
                span,
                format!("tentative definition of incomplete array '{name}'"),
            ));
        }
        if init.is_some() || self.objects[index].data.is_none() {
            let object_ty = self.objects[index].ty;
            let object_qualifiers = self.objects[index].qualifiers;
            let global =
                self.build_global_data(&symbol, object_ty, object_qualifiers, init, span)?;
            let data = self.module.globals.len();
            self.module.add_global(global);
            self.objects[index].data = Some(data);
        }
        Ok(())
    }

    fn build_global_data(
        &mut self,
        symbol: &str,
        ty: TypeId,
        qualifiers: Qualifiers,
        init: Option<&ast::Initializer>,
        span: Span,
    ) -> Result<ir::Global, Diagnostic> {
        let size = self.size_of(ty, span)?;
        let align = self.align_of(ty, span)?.max(1);
        let mut global = ir::Global::with_bytes(symbol, align, vec![0; size as usize]);
        global.readonly = qualifiers.is_const;
        if let Some(init) = init {
            self.write_global_init(&mut global, ty, 0, init)?;
        }
        Ok(global)
    }

    // -----------------------------------------------------------------------
    // Global initializer bytes
    // -----------------------------------------------------------------------

    fn write_global_init(
        &mut self,
        global: &mut ir::Global,
        ty: TypeId,
        offset: u32,
        init: &ast::Initializer,
    ) -> Result<(), Diagnostic> {
        if self.types.is_payload_enum(ty) {
            return Err(Diagnostic::new(
                init.span,
                "a global payload enum initializer is not supported yet",
            ));
        }
        match &init.kind {
            ast::InitializerKind::Expr(expr) => {
                if self.types.is_array(ty) {
                    if let ast::Expr::String(group) = &**expr {
                        return self.write_string_bytes(global, ty, offset, group);
                    }
                    return Err(Diagnostic::new(
                        init.span,
                        "an array initializer must be a string literal or a brace-enclosed list",
                    ));
                }
                if !self.types.is_scalar(ty)
                    && let ast::Expr::Call {
                        callee,
                        type_args,
                        args,
                        span: call_span,
                    } = &**expr
                    && args.is_empty()
                    && let ast::Expr::ScopedName {
                        qualifier, name, ..
                    } = callee.as_ref()
                    && name.text == "from"
                    && self.is_trait_value_name(&qualifier.text)
                {
                    return self.write_trait_table_init(
                        global, ty, offset, qualifier, type_args, *call_span,
                    );
                }
                if !self.types.is_scalar(ty) {
                    return Err(Diagnostic::new(
                        init.span,
                        "this aggregate initializer needs a brace-enclosed list",
                    ));
                }
                let value = self.eval_const(expr)?;
                self.write_global_scalar(global, ty, offset, value, init.span)
            }
            ast::InitializerKind::List(entries) => {
                if self.types.is_scalar(ty) {
                    let Some(entry) = entries.first() else {
                        return Ok(());
                    };
                    if entries.len() > 1 {
                        return Err(Diagnostic::new(
                            init.span,
                            "too many initializers for a scalar",
                        ));
                    }
                    if !entry.designators.is_empty() {
                        return Err(Diagnostic::new(
                            entry.span,
                            "a designator is not valid for a scalar initializer",
                        ));
                    }
                    return self.write_global_init(global, ty, offset, &entry.value);
                }
                if self.has_initializable_storage(ty) && self.is_zero_aggregate_initializer(entries)
                {
                    // The byte image starts zeroed, so `{0}` leaves it as is.
                    return Ok(());
                }
                self.write_global_list(global, ty, offset, entries, init.span)
            }
        }
    }

    /// Whether an aggregate has at least one leaf a brace list can initialize.
    ///
    /// The `{0}` shortcut must not make an empty record look initialized, so it
    /// only applies when there is storage to zero.
    fn has_initializable_storage(&self, ty: TypeId) -> bool {
        if self.types.is_array(ty) {
            self.array_len(ty) > 0
        } else if self.types.is_record(ty) {
            !self.record_fields(ty).is_empty()
        } else {
            false
        }
    }

    /// Whether `entries` is C's zero idiom for an aggregate, a single `{0}`.
    ///
    /// C applies the scalar to the first leaf and zero-initializes every other
    /// leaf, so the whole object is zero bytes. General brace elision is not
    /// implemented here: a second entry would belong to the first leaf's
    /// siblings, which the top-level field mapping cannot express, so only the
    /// one-entry form is accepted. Its meaning is exactly C's.
    pub(super) fn is_zero_aggregate_initializer(
        &mut self,
        entries: &[ast::InitializerEntry],
    ) -> bool {
        let [entry] = entries else {
            return false;
        };
        if !entry.designators.is_empty() {
            return false;
        }
        let ast::InitializerKind::Expr(expr) = &entry.value.kind else {
            return false;
        };
        let Ok(value) = self.eval_const(expr) else {
            return false;
        };
        match value.kind {
            ConstKind::Int(bits) | ConstKind::Float(bits) => bits == 0,
            ConstKind::Address {
                symbol: None,
                addend: 0,
            } => true,
            ConstKind::Address { .. } => false,
        }
    }

    fn write_global_list(
        &mut self,
        global: &mut ir::Global,
        ty: TypeId,
        offset: u32,
        entries: &[ast::InitializerEntry],
        span: Span,
    ) -> Result<(), Diagnostic> {
        if self.types.is_array(ty) {
            let Some(element) = self.array_element(ty) else {
                return Ok(());
            };
            let len = self.array_len(ty);
            let element_size = self.size_of(element.ty, span)?;
            let mut cursor = 0u32;
            for entry in entries {
                let (sub_ty, sub_offset, top_index) = if entry.designators.is_empty() {
                    let index = cursor;
                    (element.ty, index * element_size, index)
                } else {
                    self.designator_target(ty, &entry.designators, entry.span)?
                };
                if top_index >= len {
                    return Err(Diagnostic::new(
                        entry.span,
                        "array initializer index is out of bounds",
                    ));
                }
                self.write_global_init(global, sub_ty, offset + sub_offset, &entry.value)?;
                cursor = top_index + 1;
            }
            return Ok(());
        }
        if self.types.is_record(ty) {
            let fields = self.record_fields(ty);
            if fields.is_empty() {
                return Err(Diagnostic::new(span, "initializer for an empty record"));
            }
            let is_union = matches!(
                self.types.kind(ty),
                TypeKind::Record {
                    kind: crate::sema::types::RecordKind::Union,
                    ..
                }
            );
            let mut cursor = 0u32;
            let mut initialized = 0usize;
            for entry in entries {
                let (sub_ty, sub_offset) = if entry.designators.is_empty() {
                    let field = fields.get(cursor as usize).ok_or_else(|| {
                        Diagnostic::new(entry.span, "too many initializers for this record")
                    })?;
                    cursor += 1;
                    (field.1, field.2)
                } else {
                    let (sub_ty, sub_offset, index) =
                        self.designator_target(ty, &entry.designators, entry.span)?;
                    cursor = index + 1;
                    (sub_ty, sub_offset)
                };
                self.write_global_init(global, sub_ty, offset + sub_offset, &entry.value)?;
                initialized += 1;
                if is_union && initialized > 1 {
                    return Err(Diagnostic::new(
                        entry.span,
                        "a union initializer may initialize only one member",
                    ));
                }
            }
            return Ok(());
        }
        Err(Diagnostic::new(span, "invalid aggregate initializer"))
    }

    fn write_string_bytes(
        &mut self,
        global: &mut ir::Global,
        ty: TypeId,
        offset: u32,
        group: &ast::StringLitGroup,
    ) -> Result<(), Diagnostic> {
        let (bytes, unit) = self.decode_string(group)?;
        let element = self.array_element(ty).map(|element| element.ty);
        let Some(element) = element else {
            return Err(Diagnostic::new(
                group.span,
                "string initializer needs an array",
            ));
        };
        if self.size_of(element, group.span)? as usize != unit {
            return Err(Diagnostic::new(
                group.span,
                "string literal element width does not match the array element",
            ));
        }
        let size = self.types.size_of(ty).unwrap_or(0);
        let start = offset as usize;
        if start >= global.bytes.len() || start + size as usize > global.bytes.len() {
            return Err(Diagnostic::new(
                group.span,
                "string initializer is out of bounds",
            ));
        }
        let copy = bytes.len().min(size as usize);
        global.bytes[start..start + copy].copy_from_slice(&bytes[..copy]);
        Ok(())
    }

    fn decode_string(&self, group: &ast::StringLitGroup) -> Result<(Vec<u8>, usize), Diagnostic> {
        let (mut bytes, unit) = self.decoded_string(group)?;
        bytes.extend(std::iter::repeat_n(0u8, unit));
        Ok((bytes, unit))
    }

    fn write_global_scalar(
        &mut self,
        global: &mut ir::Global,
        ty: TypeId,
        offset: u32,
        value: super::const_eval::ConstValue,
        span: Span,
    ) -> Result<(), Diagnostic> {
        let size = self.size_of(ty, span)?;
        if self.types.is_pointer(ty) {
            let value = self.const_cast(value, ty, span)?;
            let ConstKind::Address { symbol, addend } = &value.kind else {
                return Err(Diagnostic::new(
                    span,
                    "pointer initializer is not an address constant",
                ));
            };
            match symbol {
                Some(symbol) => {
                    if size != self.layout.pointer_bytes {
                        return Err(Diagnostic::new(
                            span,
                            format!(
                                "a pointer initializer needs {} bytes on this target, but the \
                                 pointer is {size} bytes",
                                self.layout.pointer_bytes
                            ),
                        ));
                    }
                    if offset + size > global.bytes.len() as u32 {
                        return Err(Diagnostic::new(
                            span,
                            "relocation is out of bounds for this object",
                        ));
                    }
                    global.add_relocation(offset, symbol.clone(), *addend);
                }
                None => {
                    let width = size as usize;
                    let bytes = target_bytes(*addend as u64, width, self.layout.endian);
                    put_bytes(global, offset, &bytes, span)?;
                }
            }
            return Ok(());
        }
        let value = self.const_cast(value, ty, span)?;
        match value.kind {
            ConstKind::Int(bits) => {
                let bytes = target_bytes(bits, size as usize, self.layout.endian);
                put_bytes(global, offset, &bytes, span)
            }
            ConstKind::Float(bits) => {
                let bytes = target_bytes(bits, size as usize, self.layout.endian);
                put_bytes(global, offset, &bytes, span)
            }
            ConstKind::Address { .. } => Err(Diagnostic::new(
                span,
                "an address constant cannot initialize a non-pointer object",
            )),
        }
    }

    /// Write `Trait::from<Type>()` into a global object as one relocation per
    /// function-pointer field.
    fn write_trait_table_init(
        &mut self,
        global: &mut ir::Global,
        ty: TypeId,
        offset: u32,
        trait_name: &ast::Name,
        type_args: &[ast::GenericArg],
        span: Span,
    ) -> Result<(), Diagnostic> {
        let (table_ty, methods) = self.trait_table(trait_name, type_args, span)?;
        if !self.types.is_compatible(table_ty, ty) {
            return Err(Diagnostic::new(
                span,
                "trait value initializer type does not match the destination",
            ));
        }
        for (name, symbol) in methods {
            let (field_ty, field_offset) = self.field_of(ty, &name, span)?;
            let value = super::const_eval::ConstValue::address(field_ty, Some(symbol), 0);
            self.write_global_scalar(global, field_ty, offset + field_offset, value, span)?;
        }
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Type and field helpers
    // -----------------------------------------------------------------------

    /// Resolve an object declarator, inferring an omitted array bound from the
    /// initializer (`char s[] = "..."` and `int a[] = { ... }`).
    pub(super) fn resolve_object_type(
        &mut self,
        base: TypeId,
        qualifiers: Qualifiers,
        declarator: &ast::Declarator,
        init: Option<&ast::Initializer>,
        span: Span,
    ) -> Result<ResolvedSpecsWithName, Diagnostic> {
        let name = declarator.name.as_ref().map(|name| name.text.clone());
        let outer_unspecified = matches!(
            declarator.derived.first(),
            Some(ast::DerivedDeclarator::Array(ast::ArrayDeclarator {
                length: ast::ArrayLength::Unspecified,
                ..
            }))
        );
        if outer_unspecified && let Some(init) = init {
            let (element_ty, element_qualifiers) =
                self.apply_derived(base, qualifiers, &declarator.derived[1..])?;
            let count = self.infer_array_count(init, element_ty, span)?;
            let element = QualifiedType::new(element_ty, element_qualifiers);
            let ty = self
                .types
                .array_of(element, Some(count))
                .map_err(|error| Diagnostic::new(span, format!("invalid array type: {error}")))?;
            return Ok(ResolvedSpecsWithName {
                ty,
                qualifiers: element_qualifiers,
                name,
            });
        }
        self.resolve_declarator(base, qualifiers, declarator)
    }

    fn infer_array_count(
        &mut self,
        init: &ast::Initializer,
        element: TypeId,
        span: Span,
    ) -> Result<u32, Diagnostic> {
        match &init.kind {
            ast::InitializerKind::Expr(expr) => {
                let ast::Expr::String(group) = &**expr else {
                    return Err(Diagnostic::new(
                        span,
                        "cannot infer an array bound from this initializer",
                    ));
                };
                let (bytes, unit) = self.decode_string(group)?;
                if self.size_of(element, span)? as usize != unit {
                    return Err(Diagnostic::new(
                        group.span,
                        "string literal element width does not match the array element",
                    ));
                }
                u32::try_from(bytes.len() / unit)
                    .map_err(|_| Diagnostic::new(span, "array bound is too large"))
            }
            ast::InitializerKind::List(entries) => {
                let mut count = 0u32;
                for entry in entries {
                    match entry.designators.first() {
                        Some(ast::Designator::Index(index)) => {
                            let value = self.eval_const_int(index)?;
                            if value < 0 {
                                return Err(Diagnostic::new(
                                    index.span(),
                                    "negative array initializer index",
                                ));
                            }
                            count = count.max(value as u32 + 1);
                        }
                        Some(ast::Designator::Field(name)) => {
                            return Err(Diagnostic::new(
                                name.span,
                                "a field designator is not valid for an array",
                            ));
                        }
                        None => count = count.saturating_add(1),
                    }
                }
                Ok(count)
            }
        }
    }

    pub(super) fn array_len(&self, ty: TypeId) -> u32 {
        match self.types.kind(ty) {
            TypeKind::Array { len: Some(len), .. } => *len,
            _ => 0,
        }
    }

    /// Fields of a record as `(name, type, offset)`, in declaration order.
    pub(super) fn record_fields(&self, ty: TypeId) -> Vec<(String, TypeId, u32)> {
        let Some(info) = self.types.record_info(ty) else {
            return Vec::new();
        };
        info.fields
            .iter()
            .filter_map(|field| {
                field
                    .name
                    .as_ref()
                    .map(|name| (name.clone(), field.ty.ty, field.offset))
            })
            .collect()
    }

    pub(super) fn field_qualifiers(&self, record: TypeId, name: &str) -> Qualifiers {
        let Some(info) = self.types.record_info(record) else {
            return Qualifiers::NONE;
        };
        info.fields
            .iter()
            .find(|field| field.name.as_deref() == Some(name))
            .map(|field| field.ty.qualifiers)
            .unwrap_or(Qualifiers::NONE)
    }

    pub(super) fn field_of(
        &self,
        record: TypeId,
        name: &str,
        span: Span,
    ) -> Result<(TypeId, u32), Diagnostic> {
        self.field_index(record, name, span)
            .map(|(_, ty, offset)| (ty, offset))
    }

    /// Type, storage offset, and bit placement of a bit-field member.
    pub(super) fn bit_field(&self, record: TypeId, name: &str) -> Option<(TypeId, u32, u32, u32)> {
        let info = self.types.record_info(record)?;
        let field = info
            .fields
            .iter()
            .find(|field| field.name.as_deref() == Some(name))?;
        field
            .bit_width
            .map(|width| (field.ty.ty, field.offset, width, field.bit_offset))
    }

    pub(super) fn field_index(
        &self,
        record: TypeId,
        name: &str,
        span: Span,
    ) -> Result<(usize, TypeId, u32), Diagnostic> {
        let Some(info) = self.types.record_info(record) else {
            return Err(Diagnostic::new(span, "type is not a struct or union"));
        };
        for (index, field) in info.fields.iter().enumerate() {
            if field.name.as_deref() == Some(name) {
                // A bit-field shares its storage unit with its neighbours, so
                // reading it as an ordinary member would return the wrong
                // bits. Reject the access instead of miscompiling it.
                if field.bit_width.is_some() {
                    return Err(Diagnostic::new(
                        span,
                        format!("reading or writing the bit-field '{name}' is not supported yet"),
                    ));
                }
                return Ok((index, field.ty.ty, field.offset));
            }
        }
        Err(Diagnostic::new(span, format!("no member named '{name}'")))
    }

    /// Resolve a designator path to a sub-object type, its byte offset, and the
    /// top-level cursor position used for continuation.
    fn designator_target(
        &mut self,
        ty: TypeId,
        designators: &[ast::Designator],
        span: Span,
    ) -> Result<(TypeId, u32, u32), Diagnostic> {
        let mut current = ty;
        let mut offset = 0u32;
        let mut cursor = 0u32;
        for (index, designator) in designators.iter().enumerate() {
            match designator {
                ast::Designator::Field(name) => {
                    if !self.types.is_record(current) {
                        return Err(Diagnostic::new(
                            name.span,
                            "field designator on a non-record type",
                        ));
                    }
                    let (field_index, field_ty, field_offset) =
                        self.field_index(current, &name.text, name.span)?;
                    if index == 0 {
                        cursor = field_index as u32;
                    }
                    offset = offset
                        .checked_add(field_offset)
                        .ok_or_else(|| Diagnostic::new(name.span, "initializer offset overflow"))?;
                    current = field_ty;
                }
                ast::Designator::Index(expr) => {
                    let Some(element) = self.array_element(current) else {
                        return Err(Diagnostic::new(
                            expr.span(),
                            "array designator on a non-array type",
                        ));
                    };
                    let len = self.array_len(current);
                    let value = self.eval_const_int(expr)?;
                    if value < 0 || value >= i64::from(len) {
                        return Err(Diagnostic::new(
                            expr.span(),
                            "array designator is out of bounds",
                        ));
                    }
                    let size = self.size_of(element.ty, expr.span())?;
                    if index == 0 {
                        cursor = value as u32;
                    }
                    offset = offset.checked_add(value as u32 * size).ok_or_else(|| {
                        Diagnostic::new(expr.span(), "initializer offset overflow")
                    })?;
                    current = element.ty;
                }
            }
        }
        let _ = span;
        Ok((current, offset, cursor))
    }
}

// ---------------------------------------------------------------------------
// Local declarations and initializers
// ---------------------------------------------------------------------------

impl FnLower<'_> {
    pub(super) fn lower_declaration(
        &mut self,
        declaration: &ast::Declaration,
    ) -> Result<(), Diagnostic> {
        let ResolvedSpecs { ty, qualifiers } = self.l.resolve_specs(&declaration.specs)?;
        let storage = declaration.specs.storage;
        if storage == Some(ast::StorageClass::Typedef) {
            for init in &declaration.declarators {
                if init.init.is_some() {
                    return Err(Diagnostic::new(
                        init.span,
                        "a typedef cannot have an initializer",
                    ));
                }
                let resolved = self
                    .l
                    .resolve_declarator(ty, qualifiers, &init.declarator)?;
                let Some(name) = resolved.name else {
                    return Err(Diagnostic::new(
                        init.span,
                        "typedef declaration needs a name",
                    ));
                };
                self.l.declare_typedef(&name, resolved.ty, init.span)?;
            }
            return Ok(());
        }
        for init in &declaration.declarators {
            let resolved = self.l.resolve_object_type(
                ty,
                qualifiers,
                &init.declarator,
                init.init.as_ref(),
                init.span,
            )?;
            let Some(name) = resolved.name else {
                return Err(Diagnostic::new(init.span, "declaration needs a name"));
            };
            if self.l.types.is_function(resolved.ty) {
                if init.init.is_some() {
                    return Err(Diagnostic::new(
                        init.span,
                        "a function cannot have an initializer",
                    ));
                }
                let symbol = self.l.declared_function_symbol(
                    &name,
                    storage,
                    declaration.specs.is_shy_fn,
                    declaration.specs.is_pub,
                    false,
                );
                self.l
                    .declare_function_symbol(&name, &symbol, resolved.ty, false, init.span)?;
                self.bind(
                    name.clone(),
                    super::LocalBinding::Function {
                        symbol,
                        ty: resolved.ty,
                    },
                );
                continue;
            }
            match storage {
                Some(ast::StorageClass::Extern) => {
                    self.l.collect_object(
                        &name,
                        resolved.ty,
                        resolved.qualifiers,
                        Some(ast::StorageClass::Extern),
                        declaration.specs.is_pub,
                        None,
                        init.span,
                    )?;
                    self.bind(
                        name.clone(),
                        super::LocalBinding::Object {
                            symbol: name,
                            ty: resolved.ty,
                            qualifiers: resolved.qualifiers,
                        },
                    );
                    continue;
                }
                Some(ast::StorageClass::Static) => {
                    let function = self.f.name.clone();
                    let symbol = self.l.fresh_static_symbol(&function, &name);
                    let global = self.l.build_local_static(
                        &symbol,
                        resolved.ty,
                        resolved.qualifiers,
                        init.init.as_ref(),
                        init.span,
                    )?;
                    self.l.module.add_global(global);
                    self.bind(
                        name,
                        super::LocalBinding::Object {
                            symbol,
                            ty: resolved.ty,
                            qualifiers: resolved.qualifiers,
                        },
                    );
                    continue;
                }
                _ => {}
            }
            if matches!(
                self.l.types.kind(resolved.ty),
                TypeKind::Array { len: None, .. }
            ) {
                return Err(Diagnostic::new(
                    init.span,
                    format!("local array '{name}' has no known bound"),
                ));
            }
            let ir_ty = self.l.ir_type(resolved.ty);
            let align = self.l.types.align_of(resolved.ty).unwrap_or(1);
            let slot = self.f.alloc_slot_aligned(&name, ir_ty, align);
            let drop = self.l.drop_method(resolved.ty)?;
            let drop_symbol = drop.as_ref().map(|method| method.symbol.clone());
            let drop_ty = drop.map(|method| method.ty);
            let active = drop_symbol.is_some();
            let local = self.locals.len();
            self.locals.push(Local {
                name: name.clone(),
                slot,
                ty: resolved.ty,
                qualifiers: resolved.qualifiers,
                readonly: false,
                drop_symbol,
                drop_ty,
                drop_active: active,
            });
            self.bind(name.clone(), super::LocalBinding::Local(local));
            if active {
                self.current_scope_mut().drops.push(local);
            }
            if let Some(init) = &init.init {
                self.init_local_at(ir::Address::slot(slot), resolved.ty, init)?;
                if self.locals[local].drop_symbol.is_some() {
                    self.deactivate_if_local_expr(init);
                }
            }
        }
        Ok(())
    }

    /// Lower `let [mut] name = expr;`.
    ///
    /// The object is an ordinary slot. Its type is the initializer's rvalue
    /// type, so arrays and functions decay and top-level qualifiers do not
    /// carry over. A non-`mut` `let` only forbids direct stores to the slot;
    /// `&name` keeps its plain C pointer type, so a C alias can still write the
    /// object. There is no borrow or ownership rule.
    pub(super) fn lower_let(&mut self, decl: &ast::LetDecl) -> Result<(), Diagnostic> {
        let span = decl.init.span();
        let value = self.lower_expr(&decl.init)?;
        if matches!(value.kind, super::ExprKind::Void) {
            return Err(Diagnostic::new(span, "a `let` initializer needs a value"));
        }
        let ty = if self.l.types.is_array(value.ty) || self.l.types.is_function(value.ty) {
            self.l.decay_type(value.ty)
        } else {
            value.ty
        };
        let name = decl.name.text.clone();
        let ir_ty = self.l.ir_type(ty);
        let align = self.l.types.align_of(ty).unwrap_or(1);
        let slot = self.f.alloc_slot_aligned(&name, ir_ty, align);
        let drop = self.l.drop_method(ty)?;
        let drop_symbol = drop.as_ref().map(|method| method.symbol.clone());
        let drop_ty = drop.map(|method| method.ty);
        let active = drop_symbol.is_some();
        let local = self.locals.len();
        self.locals.push(Local {
            name: name.clone(),
            slot,
            ty,
            qualifiers: Qualifiers::NONE,
            readonly: !decl.is_mut && !self.l.disable_rustic_mut,
            drop_symbol,
            drop_ty,
            drop_active: active,
        });
        self.bind(name, super::LocalBinding::Local(local));
        if active {
            self.current_scope_mut().drops.push(local);
        }
        if self.l.types.is_aggregate(ty) {
            self.copy_aggregate(ir::Address::slot(slot), value, ty, span)?;
        } else {
            let (operand, operand_ty) = self.rvalue_val(value, span)?;
            let operand = self.convert(operand, operand_ty, ty, span, false)?;
            self.store_value(ir::Address::slot(slot), operand, ty, Qualifiers::NONE);
        }
        if self.locals[local].drop_symbol.is_some() {
            self.deactivate_if_local(&decl.init);
        }
        Ok(())
    }

    pub(super) fn init_local_at(
        &mut self,
        address: ir::Address,
        ty: TypeId,
        init: &ast::Initializer,
    ) -> Result<(), Diagnostic> {
        let span = init.span;
        match &init.kind {
            ast::InitializerKind::Expr(expr) => {
                if let ast::Expr::String(group) = &**expr
                    && self.l.types.is_array(ty)
                {
                    return self.copy_string_to(address, ty, group);
                }
                if self.l.types.is_aggregate(ty) && !self.l.types.is_array(ty) {
                    let value = self.lower_expr(expr)?;
                    let source_ty = value.ty;
                    if !self.l.types.is_compatible(source_ty, ty) {
                        return Err(Diagnostic::new(
                            expr.span(),
                            "initializer type does not match the object type",
                        ));
                    }
                    return self.copy_aggregate(address, value, ty, expr.span());
                }
                if self.l.types.is_array(ty) {
                    return Err(Diagnostic::new(
                        expr.span(),
                        "an array initializer must be a string literal or a brace-enclosed list",
                    ));
                }
                let (operand, operand_ty) = self.rvalue(expr)?;
                let operand = self.convert(operand, operand_ty, ty, expr.span(), false)?;
                self.store_value(address, operand, ty, Qualifiers::NONE);
                Ok(())
            }
            ast::InitializerKind::List(entries) => {
                if self.l.types.is_scalar(ty) {
                    let Some(entry) = entries.first() else {
                        return Ok(());
                    };
                    if entries.len() > 1 {
                        return Err(Diagnostic::new(span, "too many initializers for a scalar"));
                    }
                    if !entry.designators.is_empty() {
                        return Err(Diagnostic::new(
                            entry.span,
                            "a designator is not valid for a scalar initializer",
                        ));
                    }
                    return self.init_local_at(address, ty, &entry.value);
                }
                if self.l.types.is_payload_enum(ty) {
                    return Err(Diagnostic::new(
                        span,
                        "a payload enum is initialized with Enum::Variant(value), not a brace list",
                    ));
                }
                let size = self.l.size_of(ty, span)?;
                if size > 0 {
                    self.emit_statement(ir::InstKind::MemSet {
                        dst: address.clone(),
                        byte: 0,
                        bytes: size,
                    });
                }
                if size > 0 && self.l.is_zero_aggregate_initializer(entries) {
                    // The object is already zero, which is what `{0}` means.
                    return Ok(());
                }
                self.init_local_list(address, ty, entries, span)
            }
        }
    }

    fn init_local_list(
        &mut self,
        address: ir::Address,
        ty: TypeId,
        entries: &[ast::InitializerEntry],
        span: Span,
    ) -> Result<(), Diagnostic> {
        if self.l.types.is_array(ty) {
            let Some(element) = self.l.array_element(ty) else {
                return Ok(());
            };
            let len = self.l.array_len(ty);
            let element_size = self.l.size_of(element.ty, span)?;
            let mut cursor = 0u32;
            for entry in entries {
                let (sub_ty, sub_offset, top_index) = if entry.designators.is_empty() {
                    (element.ty, cursor * element_size, cursor)
                } else {
                    self.l
                        .designator_target(ty, &entry.designators, entry.span)?
                };
                if top_index >= len {
                    return Err(Diagnostic::new(
                        entry.span,
                        "array initializer index is out of bounds",
                    ));
                }
                self.init_local_at(
                    address.clone().offset_by(i64::from(sub_offset)),
                    sub_ty,
                    &entry.value,
                )?;
                cursor = top_index + 1;
            }
            return Ok(());
        }
        if self.l.types.is_record(ty) {
            let fields = self.l.record_fields(ty);
            if fields.is_empty() {
                return Err(Diagnostic::new(span, "initializer for an empty record"));
            }
            let is_union = matches!(
                self.l.types.kind(ty),
                TypeKind::Record {
                    kind: crate::sema::types::RecordKind::Union,
                    ..
                }
            );
            let mut cursor = 0u32;
            let mut initialized = 0usize;
            for entry in entries {
                let (sub_ty, sub_offset) = if entry.designators.is_empty() {
                    let field = fields.get(cursor as usize).ok_or_else(|| {
                        Diagnostic::new(entry.span, "too many initializers for this record")
                    })?;
                    cursor += 1;
                    (field.1, field.2)
                } else {
                    let (sub_ty, sub_offset, index) =
                        self.l
                            .designator_target(ty, &entry.designators, entry.span)?;
                    cursor = index + 1;
                    (sub_ty, sub_offset)
                };
                self.init_local_at(
                    address.clone().offset_by(i64::from(sub_offset)),
                    sub_ty,
                    &entry.value,
                )?;
                initialized += 1;
                if is_union && initialized > 1 {
                    return Err(Diagnostic::new(
                        entry.span,
                        "a union initializer may initialize only one member",
                    ));
                }
            }
            return Ok(());
        }
        Err(Diagnostic::new(span, "invalid aggregate initializer"))
    }

    fn copy_string_to(
        &mut self,
        address: ir::Address,
        ty: TypeId,
        group: &ast::StringLitGroup,
    ) -> Result<(), Diagnostic> {
        let (symbol, _) = self.l.string_global(group)?;
        let (bytes, _) = self.l.decode_string(group)?;
        let size = self.l.size_of(ty, group.span)?;
        let copy = (bytes.len() as u32).min(size);
        if size > 0 {
            self.emit_statement(ir::InstKind::MemSet {
                dst: address.clone(),
                byte: 0,
                bytes: size,
            });
        }
        if copy > 0 {
            self.emit_statement(ir::InstKind::MemCpy {
                dst: address,
                src: ir::Address::symbol(symbol),
                bytes: copy,
            });
        }
        Ok(())
    }

    fn copy_aggregate(
        &mut self,
        address: ir::Address,
        value: super::ExprVal,
        ty: TypeId,
        span: Span,
    ) -> Result<(), Diagnostic> {
        let size = self.l.size_of(ty, span)?;
        match value.kind {
            super::ExprKind::Place(source) => {
                self.emit_statement(ir::InstKind::MemCpy {
                    dst: address,
                    src: source,
                    bytes: size,
                });
                Ok(())
            }
            super::ExprKind::Value(operand) => {
                self.store_value(address, operand, ty, Qualifiers::NONE);
                Ok(())
            }
            super::ExprKind::Void => Err(Diagnostic::new(
                span,
                "void value used as an aggregate initializer",
            )),
        }
    }

    // -----------------------------------------------------------------------
    // Bindings
    // -----------------------------------------------------------------------

    pub(super) fn bind(&mut self, name: String, binding: super::LocalBinding) {
        self.current_scope_mut().bindings.insert(name, binding);
    }

    pub(super) fn current_scope_mut(&mut self) -> &mut super::FnScope {
        self.scopes.last_mut().expect("scope stack")
    }

    pub(super) fn bind_parameter(
        &mut self,
        name: &str,
        ty: TypeId,
        param: ir::ValueId,
    ) -> Result<(), Diagnostic> {
        if name.is_empty() {
            return Ok(());
        }
        let ir_ty = self.l.ir_type(ty);
        let align = self.l.types.align_of(ty).unwrap_or(1);
        let slot = self.f.alloc_slot_aligned(name, ir_ty, align);
        let local = self.locals.len();
        self.locals.push(Local {
            name: name.to_string(),
            slot,
            ty,
            qualifiers: Qualifiers::NONE,
            readonly: false,
            drop_symbol: None,
            drop_ty: None,
            drop_active: false,
        });
        self.bind(name.to_string(), super::LocalBinding::Local(local));
        self.store_value(
            ir::Address::slot(slot),
            ir::Operand::Value(param),
            ty,
            Qualifiers::NONE,
        );
        Ok(())
    }

    pub(super) fn declare_va_area(&mut self) -> Result<(), Diagnostic> {
        let char_ty = self.l.types.builtin(crate::sema::types::Builtin::Char);
        let array = self
            .l
            .types
            .array_of(QualifiedType::unqualified(char_ty), Some(136))
            .map_err(|error| {
                Diagnostic::new(
                    Span {
                        file: crate::source::FileId(0),
                        start: 0,
                        end: 0,
                    },
                    format!("cannot build the variadic area: {error}"),
                )
            })?;
        let slot = self
            .f
            .alloc_slot_aligned("__va_area__", ir::Type::aggregate(136, 1), 1);
        let local = self.locals.len();
        self.locals.push(Local {
            name: "__va_area__".to_string(),
            slot,
            ty: array,
            qualifiers: Qualifiers::NONE,
            readonly: false,
            drop_symbol: None,
            drop_ty: None,
            drop_active: false,
        });
        self.bind("__va_area__".to_string(), super::LocalBinding::Local(local));
        Ok(())
    }
}

impl Lowerer {
    fn build_local_static(
        &mut self,
        symbol: &str,
        ty: TypeId,
        qualifiers: Qualifiers,
        init: Option<&ast::Initializer>,
        span: Span,
    ) -> Result<ir::Global, Diagnostic> {
        self.build_global_data(symbol, ty, qualifiers, init, span)
    }
}

fn put_bytes(
    global: &mut ir::Global,
    offset: u32,
    bytes: &[u8],
    span: Span,
) -> Result<(), Diagnostic> {
    let start = offset as usize;
    let end = start
        .checked_add(bytes.len())
        .filter(|&end| end <= global.bytes.len());
    let Some(end) = end else {
        return Err(Diagnostic::new(
            span,
            "initializer writes past the end of the object",
        ));
    };
    global.bytes[start..end].copy_from_slice(bytes);
    Ok(())
}

/// Encode the low `width` bytes of `value` in the target's byte order.
fn target_bytes(value: u64, width: usize, endian: crate::ir::Endian) -> Vec<u8> {
    let all = value.to_be_bytes();
    let tail = &all[8 - width..];
    match endian {
        crate::ir::Endian::Big => tail.to_vec(),
        crate::ir::Endian::Little => tail.iter().rev().copied().collect(),
    }
}

/// The generic record specifier in a declaration, if it declares or uses one.
fn generic_definition(specs: &ast::DeclSpecs) -> Option<&ast::RecordSpec> {
    specs.type_specs.iter().find_map(|spec| match spec {
        ast::TypeSpecifier::Record(record) if !record.generics.is_empty() => Some(record),
        _ => None,
    })
}

/// The generic enum specifier in a declaration, if it declares or uses one.
fn generic_enum_definition(specs: &ast::DeclSpecs) -> Option<&ast::EnumSpec> {
    specs.type_specs.iter().find_map(|spec| match spec {
        ast::TypeSpecifier::Enum(enumeration) if !enumeration.generics.is_empty() => {
            Some(enumeration)
        }
        _ => None,
    })
}
