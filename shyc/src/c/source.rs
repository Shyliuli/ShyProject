//! Translating a parsed unit back into C source.
//!
//! The preprocessor already inlined includes and the parser already expanded
//! compile-time calls, so the unit here is ordinary C plus the ShyC sugar this
//! module resolves:
//!
//! - `let` becomes an inferred C local (`__auto_type`);
//! - an `impl` method becomes a mangled function, and a method call becomes a
//!   call to it: `p.add(1)` is `Point_add(&p, 1)` and `p->add(1)` is
//!   `Point_add(p, 1)`;
//! - a trait becomes a structure of function pointers, and `Trait::from<T>()`
//!   becomes the constructor that fills one;
//! - `value?` becomes a statement expression that returns the failure variant;
//! - `asm!` becomes a GCC extended asm statement;
//! - a payload enum reads `value.Variant` and builds its internal `.tag` and
//!   `.payload.Variant`, while a niche enum stores its payload directly.
//!
//! Everything else is ordinary C and prints as written. A generic function or
//! record prints nothing: C has no template, and an instance of one is a
//! separate symbol with its own name (`Vec_int`). The compiler decided which
//! instances exist and what each one is called, so this module never invents a
//! name for a call, a member, or a variant.

use std::collections::HashMap;

use crate::ast::{
    self, AsmStmt, AssignOp, BinaryOp, Block, DeclSpecs, Declaration, Declarator,
    DerivedDeclarator, EnumSpec, Expr, ForInit, FunctionDecl, GenericArg, GenericArgKind,
    ImplBlock, Initializer, InitializerKind, Item, MemberOp, Name, PostfixOp, RecordSpec, Stmt,
    TraitDecl, TranslationUnit, TypeName, TypeSpecifier, UnaryOp,
};
use crate::c::names;
use crate::c::types::{
    Spelling, declarator_text_in, generic_spec_text, specs_text_in, trait_value_possible,
    trait_value_text, type_name_text, type_name_text_with,
};
use crate::lower::{InstancePlan, Lowered, PayloadEnumPlan, VariantPlan};
use crate::source::{Diagnostic, Span};

/// Render the whole unit as one C translation unit.
pub fn render(unit: &TranslationUnit, lowered: &Lowered) -> Result<String, Diagnostic> {
    Backend::new(unit, lowered).render(unit)
}

/// One C type, as this backend needs it: a base name and a pointer depth.
///
/// Resolution here is about names, not layout. Whether a value is a record, a
/// trait value, or a pointer to one is all a method call has to know, so
/// qualifiers, arrays, and function types stay out of this type.
#[derive(Clone, Debug)]
struct Ty {
    base: String,
    pointers: usize,
}

impl Ty {
    fn from_text(text: &str) -> Ty {
        let (base, pointers) = names::split(text);
        Ty { base, pointers }
    }

    fn pointer(&self) -> Ty {
        Ty {
            base: self.base.clone(),
            pointers: self.pointers + 1,
        }
    }

    fn pointee(&self) -> Ty {
        Ty {
            base: self.base.clone(),
            pointers: self.pointers.saturating_sub(1),
        }
    }
}

/// What the unit declares, in the shape method resolution needs.
struct Backend<'a> {
    /// The type of every file-scope object, by name.
    objects: HashMap<String, Ty>,
    /// The return type of every function and mangled method, by name.
    functions: HashMap<String, Ty>,
    /// The fields of every record with a definition, by base name.
    records: HashMap<String, Vec<(String, Ty)>>,
    /// The enumerators of every plain C enum with a definition, by base name.
    /// A payload enum has no C enumerator, so it is described by
    /// `payload_enums` instead.
    enums: HashMap<String, Vec<String>>,
    /// The impl blocks of every receiver type, by base name.
    impls: HashMap<String, Vec<&'a ImplBlock>>,
    /// Every trait declaration, by name.
    traits: HashMap<String, &'a TraitDecl>,
    /// What the compiler made of this unit: every specialization, the symbol it
    /// gave each function and call, and the C name of each type.
    lowered: &'a Lowered,
    /// The C type name of every generic instance the compiler materialized,
    /// keyed by the source spelling the printer sees (Vec<int>).
    type_names: HashMap<String, String>,
    /// Generic free functions, by name.
    templates: HashMap<String, &'a FunctionDecl>,
    /// Generic record definitions, by name.
    generic_records: HashMap<String, &'a RecordSpec>,
    /// Generic enum definitions, by name.
    generic_enums: HashMap<String, &'a EnumSpec>,
    /// The payload enums the compiler lowered, by base name. A hand-written
    /// payload enum is declared under its tag and a generic one under the name
    /// the instance plan gave it.
    payload_enums: HashMap<String, &'a PayloadEnumPlan>,
    /// The fields of every record the compiler lowered, by base name. A member
    /// read of an instantiated record resolves its type through this table.
    record_fields: HashMap<String, &'a [(String, String)]>,
    /// The C name of each `#[args_builtin]` struct.
    args_builtin: std::collections::HashSet<String>,
    /// Every symbol the C output names, in any pass. A definition, prototype,
    /// or declaration this set does not hold is not part of the output: nothing
    /// uses it, and a file-local function nothing uses is dead code the C
    /// compiler warns about.
    used: std::cell::RefCell<std::collections::HashSet<String>>,
    /// The impl blocks of a generic record, by the record's name.
    generic_impls: HashMap<String, Vec<&'a ImplBlock>>,
    /// Specializations already queued, so one instance prints once.
    instances: std::cell::RefCell<Vec<Instance>>,
}

/// One specialization the C output needs: a template and concrete arguments.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Instance {
    /// The template's name, as written in the source.
    name: String,
    /// The concrete arguments, in source spelling (int, Vec<int>).
    args: Vec<String>,
    /// The receiver of a generic method; None for a record or free function.
    receiver: Option<String>,
}

impl Instance {
    /// The C name of this instance, from the compiler's plan.
    fn c_name(&self, backend: &Backend<'_>) -> String {
        if let Some(plan) = backend.plan(&self.name, &self.args, self.receiver.as_deref()) {
            if let Some(symbol) = &plan.symbol {
                return names::symbol(symbol);
            }
            if let Some(name) = &plan.type_name {
                return name.clone();
            }
        }
        // A plan the compiler never made: keep the template name so the C
        // compiler reports the missing definition.
        self.name.clone()
    }
}

impl<'a> Backend<'a> {
    fn new(unit: &'a TranslationUnit, lowered: &'a Lowered) -> Backend<'a> {
        let mut type_names = HashMap::new();
        for plan in &lowered.instances {
            if let Some(name) = &plan.type_name {
                type_names.insert(instance_key(&plan.name, &plan.args), name.clone());
            }
        }
        // A payload enum is found by the type name C spells it with, which is a
        // tag for a hand-written enum and the plan's name for a generic one.
        let mut payload_enums = HashMap::new();
        for plan in &lowered.payload_enums {
            payload_enums.insert(names::split(&plan.name).0, plan);
        }
        let mut record_fields = HashMap::new();
        for (name, fields) in &lowered.record_fields {
            record_fields.insert(names::split(name).0, fields.as_slice());
        }
        let mut backend = Backend {
            objects: HashMap::new(),
            functions: HashMap::new(),
            records: HashMap::new(),
            enums: HashMap::new(),
            impls: HashMap::new(),
            traits: HashMap::new(),
            lowered,
            type_names,
            templates: HashMap::new(),
            generic_records: HashMap::new(),
            generic_enums: HashMap::new(),
            payload_enums,
            record_fields,
            args_builtin: lowered
                .args_builtin
                .iter()
                .map(|name| names::split(name).0)
                .collect(),
            used: std::cell::RefCell::new(std::collections::HashSet::new()),
            generic_impls: HashMap::new(),
            instances: std::cell::RefCell::new(Vec::new()),
        };
        for item in &unit.items {
            match item {
                Item::Decl(declaration) => backend.declaration(declaration),
                Item::Function(function) => {
                    if let Some(name) = &function.declarator.name {
                        let ty = backend.ty_of(&function.specs, &function.declarator);
                        backend.functions.insert(name.text.clone(), ty);
                        if is_template(&function.declarator) {
                            backend.templates.insert(name.text.clone(), function);
                        }
                    }
                }
                Item::Impl(block) => backend.impl_block(block),
                Item::Trait(declaration) => {
                    backend
                        .traits
                        .insert(declaration.name.text.clone(), declaration);
                }
                Item::Use(_) => {}
            }
        }
        // The compiler already decided which specializations exist, so the work
        // list is its plan rather than a search of the source: one instance per
        // generic function, and one per generic method. A record or enum instance
        // is a type, printed from its plan instead.
        for plan in &lowered.instances {
            if plan.receiver.is_none() {
                if plan.type_name.is_none() && plan.symbol.is_some() {
                    backend.instances.borrow_mut().push(Instance {
                        name: plan.name.clone(),
                        args: plan.args.clone(),
                        receiver: None,
                    });
                }
                continue;
            }
            // A concrete impl block prints its own methods, inherited defaults
            // included; only a template's methods and a generic method of a
            // concrete impl are printed from an instance.
            let Some((_, declarator, _)) = backend.method_template(plan) else {
                continue;
            };
            let from_template = plan.impl_name.as_deref().is_some_and(|name| {
                backend
                    .generic_impls
                    .contains_key(name.split('<').next().unwrap_or(name))
            });
            if from_template || !generic_parameters(declarator).is_empty() {
                backend.instances.borrow_mut().push(Instance {
                    name: plan.name.clone(),
                    args: plan.args.clone(),
                    receiver: plan.receiver.clone(),
                });
            }
        }
        backend
    }

    /// How this output spells a type, with self as self_name.
    fn spelling<'s>(&'s self, self_name: &'s str) -> Spelling<'s> {
        Spelling {
            self_name,
            instances: Some(&self.type_names),
            bindings: None,
            generics: true,
            preserve_generic_args: false,
        }
    }

    fn declaration(&mut self, declaration: &'a Declaration) {
        for specifier in &declaration.specs.type_specs {
            match specifier {
                TypeSpecifier::Record(record) => self.record(record),
                TypeSpecifier::Enum(enumeration) => self.enumeration(enumeration),
                _ => {}
            }
        }
        for declarator in &declaration.declarators {
            let Some(name) = &declarator.declarator.name else {
                continue;
            };
            let ty = self.ty_of(&declaration.specs, &declarator.declarator);
            if has_function(&declarator.declarator) {
                self.functions.insert(name.text.clone(), ty);
            } else {
                self.objects.insert(name.text.clone(), ty);
            }
        }
    }

    fn record(&mut self, record: &'a RecordSpec) {
        let (Some(tag), Some(members)) = (&record.tag, &record.members) else {
            return;
        };
        if !record.generics.is_empty() {
            // A template: each use names an instance, which is printed once the
            // work list reaches it.
            self.generic_records.insert(tag.text.clone(), record);
            return;
        }
        let mut fields = Vec::new();
        for member in members {
            for declarator in &member.declarators {
                let Some(name) = &declarator.declarator.name else {
                    continue;
                };
                fields.push((
                    name.text.clone(),
                    self.ty_of(&member.specs, &declarator.declarator),
                ));
            }
        }
        self.records.insert(tag.text.clone(), fields);
    }

    fn enumeration(&mut self, enumeration: &'a EnumSpec) {
        let (Some(tag), Some(enumerators)) = (&enumeration.tag, &enumeration.enumerators) else {
            return;
        };
        if !enumeration.generics.is_empty() {
            // A template: each use names an instance, which the plan describes.
            self.generic_enums.insert(tag.text.clone(), enumeration);
            return;
        }
        // A payload enum has no C enumerators: its variants are tags and
        // payloads, which the compiler's plan describes.
        if enumerators
            .iter()
            .any(|enumerator| enumerator.payload.is_some())
        {
            return;
        }
        let names = enumerators
            .iter()
            .map(|enumerator| enumerator.name.text.clone())
            .collect();
        self.enums.insert(tag.text.clone(), names);
    }

    fn impl_block(&mut self, block: &'a ImplBlock) {
        if let Some(receiver) = generic_receiver(block) {
            self.generic_impls.entry(receiver).or_default().push(block);
            return;
        }
        let receiver = Ty::from_text(&type_name_text(&block.self_type)).base;
        for method in &block.methods {
            if let Some(name) = &method.declarator.name {
                let ty = self.ty_of(&method.specs, &method.declarator);
                let symbol = self.symbol(&receiver, &name.text, trait_of(block), method.span);
                self.functions.insert(symbol, ty);
            }
        }
        self.impls.entry(receiver).or_default().push(block);
    }

    /// The declared type behind one specifier list and declarator.
    ///
    /// A function declarator answers with its return type: only a call asks for
    /// it, and the parameters are never part of a name.
    fn ty_of(&self, specs: &DeclSpecs, declarator: &Declarator) -> Ty {
        self.ty_with(specs, declarator, self.spelling("void"))
    }

    /// The declared type behind one specifier list, spelled as the caller needs
    /// it: a specialization resolves its parameters, a method its receiver.
    fn ty_with(&self, specs: &DeclSpecs, declarator: &Declarator, spelling: Spelling) -> Ty {
        match function_position(declarator) {
            Some(position) => Ty::from_text(&self.return_type_text_at(specs, declarator, position)),
            None => {
                // The declared name is not part of the type: only the
                // operators around it are.
                let mut anonymous = declarator.clone();
                anonymous.name = None;
                Ty::from_text(&format!(
                    "{} {}",
                    specs_text_in(specs, spelling),
                    declarator_text_in(&anonymous, spelling)
                ))
            }
        }
    }

    /// The type a function returns, spelled for this output.
    fn return_type_text(&self, specs: &DeclSpecs, declarator: &Declarator) -> String {
        let position = function_position(declarator).unwrap_or(0);
        self.return_type_text_at(specs, declarator, position)
    }

    fn return_type_text_at(
        &self,
        specs: &DeclSpecs,
        declarator: &Declarator,
        position: usize,
    ) -> String {
        let spelling = self.spelling("void");
        let returned = Declarator {
            span: declarator.span,
            name: None,
            derived: declarator.derived[position + 1..].to_vec(),
        };
        let base = specs_text_in(specs, spelling);
        let suffix = declarator_text_in(&returned, spelling);
        if suffix.is_empty() {
            base
        } else {
            format!("{base} {suffix}")
        }
    }

    /// The field of a record, following one implicit pointer dereference.
    ///
    /// A record the unit declared answers from its own definition; one the
    /// compiler instantiated answers from the plan, so a member of a generic
    /// instance resolves too.
    fn field(&self, ty: &Ty, name: &str) -> Option<Ty> {
        if let Some(fields) = self.records.get(&ty.base) {
            return fields
                .iter()
                .find(|(field, _)| field == name)
                .map(|(_, ty)| ty.clone());
        }
        self.record_fields
            .get(&ty.base)?
            .iter()
            .find(|(field, _)| field == name)
            .map(|(_, ty)| Ty::from_text(ty))
    }

    /// Whether one plan describes the method of this receiver.
    ///
    /// A receiver is named by its type, so `struct shyc_E41` and the C base
    /// name the printer resolved both answer for the same type.
    fn names_receiver(plan: &InstancePlan, receiver: &str) -> bool {
        let base = names::split(receiver).0;
        [&plan.impl_name, &plan.receiver]
            .into_iter()
            .flatten()
            .any(|name| names::split(name).0 == base)
    }

    /// The compiler's plan for one method of one receiver.
    ///
    /// A receiver may have an inherent method and a trait method of one name. A
    /// call reaches the inherent one, so it asks without a trait first; a
    /// definition names the trait it belongs to instead.
    fn method_plan(
        &self,
        receiver: &str,
        name: &str,
        trait_name: Option<&str>,
    ) -> Option<&'a InstancePlan> {
        self.lowered.instances.iter().find(|plan| {
            plan.name == name
                && plan.trait_name.as_deref() == trait_name
                && Self::names_receiver(plan, receiver)
        })
    }

    /// The C symbol of one method, as a call reaches it: the receiver's type and
    /// the method's name, which one span cannot tell apart when a macro
    /// expansion or a generated fragment made several calls.
    fn method(&self, receiver: &str, name: &str) -> Option<String> {
        let plan = self.method_plan(receiver, name, None).or_else(|| {
            self.lowered.instances.iter().find(|plan| {
                plan.name == name
                    && plan.trait_name.is_some()
                    && Self::names_receiver(plan, receiver)
            })
        })?;
        Some(names::symbol(plan.symbol.as_ref()?))
    }

    /// The C symbol of one method definition: the compiler's answer for this
    /// receiver, trait, and name.
    fn symbol(&self, receiver: &str, name: &str, trait_name: Option<&str>, span: Span) -> String {
        if let Some(symbol) = self
            .method_plan(receiver, name, trait_name)
            .and_then(|plan| plan.symbol.clone())
        {
            return names::symbol(&symbol);
        }
        if let Some(symbol) = self.lowered.declaration_symbols.get(&span) {
            return names::symbol(symbol);
        }
        // Neither: a method the compiler never lowered, which only happens for a
        // construct it rejected, so the plain name is enough to report it in C.
        name.to_string()
    }

    /// The linkage a method is printed with.
    ///
    /// The compiler gives a file-local method an internal symbol, and that is
    /// exactly what C's `static` means; a method that adopted a header
    /// prototype keeps the external symbol the header declared.
    fn linkage(&self, span: Span, is_pub: bool) -> &'static str {
        let file_local = self
            .lowered
            .declaration_symbols
            .get(&span)
            .map_or(!is_pub, |symbol| symbol.starts_with(".L."));
        if file_local { "static " } else { "" }
    }

    /// Record that the C output names one symbol.
    fn mark_used(&self, symbol: &str) {
        self.used.borrow_mut().insert(symbol.to_string());
    }

    /// Whether the C output names one symbol, so its declaration belongs here.
    fn used_symbol(&self, symbol: &str) -> bool {
        self.used.borrow().contains(symbol)
    }

    /// Whether the C output has to print one definition.
    ///
    /// An externally visible definition stays, because another unit may be the
    /// one that uses it; a file-local one that nothing names is dead code, and
    /// the C compiler reports it. A file-local symbol is `.L.module.name` before
    /// the C spelling, and `_L_module_name` after it.
    fn declares(&self, symbol: &str) -> bool {
        let file_local = symbol.starts_with(".L.") || symbol.starts_with("_L_");
        !file_local || self.used_symbol(symbol)
    }

    /// The symbol a call to one generic function instance resolved to.
    ///
    /// A template has one symbol per instance, so the callee's name alone cannot
    /// name the call. The compiler resolved it to a concrete instance, and this
    /// table is that answer, kept for the tuple that identifies the call: its
    /// span, which the source wrote, and the callee's name, which tells two
    /// calls one macro expansion produced apart.
    fn call_symbol(&self, span: Span, name: &str) -> Option<String> {
        self.lowered
            .call_symbols
            .get(&(span, name.to_string()))
            .map(|symbol| names::symbol(symbol))
    }

    /// The C name of one file-scope function: the compiler's symbol for it.
    ///
    /// A function is looked up by its C name, not by a span: one span may cover
    /// several functions when a macro expansion or a generated fragment produced
    /// them, and a name always has exactly one symbol.
    fn function_symbol(&self, name: &str) -> Option<String> {
        self.lowered
            .function_symbols
            .get(name)
            .map(|symbol| names::symbol(symbol))
    }

    /// The C name of one file-scope function, or the name itself when the name
    /// is not one (a local, a file-scope object, or an enumerator).
    fn symbol_of(&self, name: &str) -> String {
        self.function_symbol(name)
            .unwrap_or_else(|| name.to_string())
    }

    /// The methods to print for one block: the ones it defines, then the trait
    /// defaults it inherits.
    fn methods(&self, block: &'a ImplBlock) -> Vec<Method<'a>> {
        let mut methods = block
            .methods
            .iter()
            .map(|method| Method {
                span: method.span,
                specs: &method.specs,
                declarator: &method.declarator,
                body: &method.body,
            })
            .collect::<Vec<_>>();
        let Some(trait_name) = &block.trait_name else {
            return methods;
        };
        let Some(declaration) = self.traits.get(&trait_name.text) else {
            return methods;
        };
        for method in &declaration.methods {
            let Some(body) = &method.body else {
                continue;
            };
            let Some(name) = &method.declarator.name else {
                continue;
            };
            if is_template(&method.declarator) || find_method(block, &name.text).is_some() {
                continue;
            }
            methods.push(Method {
                span: method.span,
                specs: &method.specs,
                declarator: &method.declarator,
                body,
            });
        }
        methods
    }

    /// The return type of one method of one receiver.
    fn method_ty(&self, receiver: &str, name: &str) -> Option<Ty> {
        let blocks = self.impls.get(receiver)?;
        for block in blocks {
            if let Some(method) = find_method(block, name) {
                return Some(self.ty_of(&method.specs, &method.declarator));
            }
            if let Some(prototype) = find_prototype(block, name) {
                return Some(self.ty_of(&prototype.specs, &prototype.declarator));
            }
            if let Some(trait_name) = &block.trait_name
                && let Some(declaration) = self.traits.get(&trait_name.text)
                && let Some(method) = declaration.methods.iter().find(|method| {
                    method
                        .declarator
                        .name
                        .as_ref()
                        .is_some_and(|method_name| method_name.text == name)
                })
            {
                return Some(self.ty_of(&method.specs, &method.declarator));
            }
        }
        None
    }

    /// The payload enum a C type names, from the compiler's plan.
    fn enum_plan(&self, ty: &Ty) -> Option<&'a PayloadEnumPlan> {
        self.payload_enums.get(&ty.base).copied()
    }

    /// The C name of the payload enum a qualifier names.
    ///
    /// A hand-written enum is named by its tag; a generic one by the instance
    /// plan, whose name the type printer already uses for that type.
    fn enum_name(
        &self,
        qualifier: &Name,
        args: &[GenericArg],
        spelling: Spelling,
    ) -> Option<String> {
        if args.is_empty() {
            return self
                .payload_enums
                .contains_key(&names::split(&qualifier.text).0)
                .then(|| qualifier.text.clone());
        }
        let key = instance_key(&qualifier.text, &generic_argument_text(args, spelling));
        self.type_names.get(&key).cloned()
    }

    /// The payload enum and the variant one `Enum::Variant` names.
    fn variant(
        &self,
        qualifier: &Name,
        args: &[GenericArg],
        spelling: Spelling,
        name: &str,
    ) -> Option<(&'a PayloadEnumPlan, &'a VariantPlan)> {
        let enum_name = self.enum_name(qualifier, args, spelling)?;
        let plan = self.payload_enums.get(&names::split(&enum_name).0)?;
        let variant = plan.variants.iter().find(|variant| variant.name == name)?;
        Some((plan, variant))
    }

    /// The return type of one trait method.
    fn trait_method_ty(&self, trait_name: &str, name: &str) -> Option<Ty> {
        let declaration = self.traits.get(trait_name)?;
        let method = declaration.methods.iter().find(|method| {
            method
                .declarator
                .name
                .as_ref()
                .is_some_and(|method_name| method_name.text == name)
        })?;
        Some(self.ty_of(&method.specs, &method.declarator))
    }

    fn render(&self, unit: &TranslationUnit) -> Result<String, Diagnostic> {
        // A definition is printed only when something names it, and printing it
        // can name more, so the unit is printed until the set of named symbols
        // stops growing; the last pass is the output.
        let mut output = String::new();
        loop {
            output.clear();
            let before = self.used.borrow().len();
            self.emit(unit, &mut output)?;
            if self.used.borrow().len() == before {
                return Ok(std::mem::take(&mut output));
            }
        }
    }

    /// Print the unit, its declarations, and every specialization it needs.
    fn emit(&self, unit: &TranslationUnit, output: &mut String) -> Result<(), Diagnostic> {
        output.push_str("/* Generated by shyc --emit=c. */\n\n");
        // A record's tag is also its type name in ShyC, so every C record gets
        // the typedef a caller needs before anything mentions it, and every
        // instance the plans named gets the struct tag a pointer may use. The
        // definitions themselves follow below, once their members can be
        // complete. This comes first because a trait method signature may name a
        // record.
        let mut records = String::new();
        for item in &unit.items {
            if let Item::Decl(declaration) = item {
                if let Some(text) = forward_record(&declaration.specs) {
                    records.push_str(&text);
                    records.push('\n');
                }
            }
        }
        for plan in &self.lowered.instances {
            let Some(name) = &plan.type_name else {
                continue;
            };
            if name.starts_with("struct ") || name.starts_with("union ") {
                records.push_str(&format!("{name};\n"));
            }
        }
        if !records.is_empty() {
            output.push_str(&records);
            output.push('\n');
        }
        // Trait value structures come first, so any declaration or body below
        // can name a trait type whatever its source order.
        let mut traits = String::new();
        for item in &unit.items {
            if let Item::Trait(declaration) = item {
                if declaration.generics.is_empty() && !declaration.methods.is_empty() {
                    let name = &declaration.name.text;
                    traits.push_str(&format!("typedef struct {name} {name};\n"));
                    traits.push_str(&trait_value_text(declaration));
                    traits.push('\n');
                }
            }
        }
        if !traits.is_empty() {
            output.push_str(&traits);
            output.push('\n');
        }
        // A prototype and a member both need their types complete, so every
        // type definition comes before them, ordered so a type a definition
        // stores by value is complete first.
        let types = self.type_blocks(unit)?;
        if !types.is_empty() {
            output.push_str(&types);
            output.push('\n');
        }
        // The generic instances the unit needs: one prototype per function and
        // per method, before any body that calls one.
        let instances = self.instance_declarations()?;
        if !instances.is_empty() {
            output.push_str(&instances);
            output.push('\n');
        }
        // Methods and trait constructors are defined wherever their impl block
        // appears, but a body before it calls them, so they are declared first.
        // A file-scope function needs the same: C has no implicit declaration,
        // and a function defined later is called before its definition.
        let mut prototypes = String::new();
        for item in &unit.items {
            match item {
                Item::Function(function) => prototypes.push_str(&self.function_prototype(function)),
                Item::Impl(block) => prototypes.push_str(&self.impl_prototypes(block)?),
                _ => {}
            }
        }
        if !prototypes.is_empty() {
            output.push_str(&prototypes);
            output.push('\n');
        }
        for item in &unit.items {
            let text = match item {
                // A type declaration was printed above, with the other types.
                Item::Decl(declaration) if is_type_declaration(declaration) => String::new(),
                Item::Decl(declaration) => self.item_declaration(declaration)?,
                Item::Function(function) => self.function(function)?,
                Item::Impl(block) => self.impl_text(block)?,
                // A trait lives only at compile time; its value structure was
                // printed above. An import was resolved while preprocessing.
                Item::Trait(_) | Item::Use(_) => String::new(),
            };
            let text = text.trim_end();
            if !text.is_empty() {
                output.push_str(text);
                output.push_str("\n\n");
            }
        }
        self.instantiate(output)
    }

    /// Every C type definition the output prints, ordered so a type one stores
    /// by value is complete before it.
    fn type_blocks(&self, unit: &TranslationUnit) -> Result<String, Diagnostic> {
        let mut blocks = Vec::new();
        // An instance layout belongs to the compiler: it printed the header's
        // types, so it prints this one too.
        for plan in &self.lowered.instances {
            let (Some(name), Some(layout)) = (&plan.type_name, &plan.layout) else {
                continue;
            };
            let name = names::split(name).0;
            blocks.push(TypeBlock {
                needs: self
                    .lowered
                    .layout_needs
                    .get(&name)
                    .cloned()
                    .unwrap_or_default(),
                name,
                text: layout.clone(),
            });
        }
        for item in &unit.items {
            let Item::Decl(declaration) = item else {
                continue;
            };
            if !is_type_declaration(declaration) {
                continue;
            }
            let text = self.item_declaration(declaration)?;
            if text.is_empty() {
                continue;
            }
            let name = declared_type_name(declaration).unwrap_or_default();
            blocks.push(TypeBlock {
                needs: self
                    .lowered
                    .layout_needs
                    .get(&name)
                    .cloned()
                    .unwrap_or_default(),
                name,
                text,
            });
        }
        Ok(order_type_blocks(blocks))
    }

    /// Print one file-scope declaration as a definition, initializer included.
    fn item_declaration(&self, declaration: &Declaration) -> Result<String, Diagnostic> {
        if is_template_declaration(declaration) {
            return Ok(String::new());
        }
        let spelling = self.spelling("void");
        let prefix = storage_prefix(declaration.specs.storage);
        let specs = format!("{prefix}{}", specs_text_in(&declaration.specs, spelling));
        if declaration.declarators.is_empty() {
            // An enum's tag names its type too, and C has no forward enum
            // declaration, so the typedef is written with the definition.
            return Ok(match &declaration.specs.type_specs[..] {
                [TypeSpecifier::Enum(enumeration)] => match &enumeration.tag {
                    Some(tag) => format!("typedef {specs} {};", tag.text),
                    None => format!("{specs};"),
                },
                _ => format!("{specs};"),
            });
        }
        let mut globals = Body::new(self, None);
        let mut declarators = Vec::new();
        for declarator in &declaration.declarators {
            // A declaration that defines nothing is only printed when this unit
            // uses the name: an included header's declaration of a host library
            // function would otherwise repeat, and possibly contradict, the
            // declaration the C compiler already has.
            if declarator.init.is_none()
                && declaration.specs.storage != Some(ast::StorageClass::Typedef)
                && (has_function(&declarator.declarator)
                    || declaration.specs.storage == Some(ast::StorageClass::Extern))
                && let Some(name) = &declarator.declarator.name
                && !self.used_symbol(&self.symbol_of(&name.text))
            {
                continue;
            }
            // A function reference in C names the symbol its definition does.
            let printed = match &declarator.declarator.name {
                Some(name) => renamed(&declarator.declarator, &self.symbol_of(&name.text)),
                None => declarator.declarator.clone(),
            };
            let mut text = declarator_text_in(&printed, spelling);
            if let Some(initializer) = &declarator.init {
                text.push_str(" = ");
                text.push_str(&globals.initializer(initializer)?);
            }
            declarators.push(text);
        }
        // Every declarator was a declaration this unit does not use.
        if declarators.is_empty() {
            return Ok(String::new());
        }
        Ok(format!("{specs} {};", declarators.join(", ")))
    }

    /// The name a function is written under: the compiler's symbol for it.
    fn function_declarator(&self, function: &FunctionDecl) -> Declarator {
        match &function.declarator.name {
            Some(name) => renamed(&function.declarator, &self.symbol_of(&name.text)),
            None => function.declarator.clone(),
        }
    }

    /// The prototype a later definition needs, or nothing for a template.
    fn function_prototype(&self, function: &FunctionDecl) -> String {
        let Some(name) = &function.declarator.name else {
            return String::new();
        };
        if is_template(&function.declarator)
            || self.entry_main(function).is_some()
            || !self.declares(&self.symbol_of(&name.text))
        {
            return String::new();
        }
        format!(
            "{} {};\n",
            specs_text_in(&function.specs, self.spelling("void")),
            declarator_text_in(&self.function_declarator(function), self.spelling("void"))
        )
    }

    fn function(&self, function: &FunctionDecl) -> Result<String, Diagnostic> {
        // A generic function is a template: C has no spelling for it, and each
        // instance is a symbol of its own, named by the instance table.
        if is_template(&function.declarator) {
            return Ok(String::new());
        }
        if let Some(name) = &function.declarator.name
            && !self.declares(&self.symbol_of(&name.text))
        {
            return Ok(String::new());
        }
        let spelling = self.spelling("void");
        // The definition carries the symbol the compiler gave it, which is what
        // a call site prints.
        let declarator = self.function_declarator(function);
        let entry = self.entry_main(function);
        let signature = match &entry {
            // The C runtime starts a program at `int main(int argc, char **argv)`.
            Some(_) => "int main(int argc, char **argv)".to_string(),
            None => format!(
                "{} {}",
                specs_text_in(&function.specs, spelling),
                declarator_text_in(&declarator, spelling)
            ),
        };
        let mut body = Body::new(
            self,
            Some(self.return_type_text(&function.specs, &function.declarator)),
        );
        body.parameters(&function.declarator);
        let mut text = body.render_block(&function.body)?;
        if let Some((parameter, ty)) = &entry {
            // A `#[args_builtin]` parameter receives the struct's fields one
            // after another, so the body rebuilds the struct from the two C
            // parameters the runtime passed.
            let at = text.find('\n').map_or(1, |index| index + 1);
            text.insert_str(at, &format!("    {ty} {parameter} = {{argc, argv}};\n"));
        }
        Ok(format!("{signature} {text}"))
    }

    /// The parameter of the program's entry point, when it takes the
    /// `#[args_builtin]` struct by value.
    fn entry_main(&self, function: &FunctionDecl) -> Option<(String, String)> {
        let name = function.declarator.name.as_ref()?;
        if name.text != "main" {
            return None;
        }
        let position = function_position(&function.declarator)?;
        let DerivedDeclarator::Function(declaration) = function.declarator.derived.get(position)?
        else {
            return None;
        };
        let [parameter] = &declaration.params[..] else {
            return None;
        };
        let declarator = parameter.declarator.as_ref()?;
        let parameter_name = declarator.name.as_ref()?.text.clone();
        if !declarator.derived.is_empty() {
            return None;
        }
        let spelling = specs_text_in(&parameter.specs, self.spelling("void"));
        let ty = names::split(&spelling).0;
        self.args_builtin
            .contains(&ty)
            .then_some((parameter_name, ty))
    }

    /// The declarations a call needs before the impl block's definitions.
    fn impl_prototypes(&self, block: &'a ImplBlock) -> Result<String, Diagnostic> {
        if !block.generics.is_empty() {
            return Ok(String::new());
        }
        let receiver = Ty::from_text(&type_name_text(&block.self_type)).base;
        let spelling = self.spelling(&receiver);
        let mut output = String::new();
        for method in self.methods(block) {
            let Some(name) = &method.declarator.name else {
                continue;
            };
            let symbol = self.symbol(&receiver, &name.text, trait_of(block), method.span);
            if !self.declares(&symbol) {
                continue;
            }
            let declarator = renamed(method.declarator, &symbol);
            let linkage = self.linkage(method.span, method.specs.is_pub);
            output.push_str(&format!(
                "{linkage}{} {};\n",
                specs_text_in(method.specs, spelling),
                declarator_text_in(&declarator, spelling)
            ));
        }
        for prototype in &block.prototypes {
            let Some(name) = &prototype.declarator.name else {
                continue;
            };
            let symbol = self.symbol(&receiver, &name.text, trait_of(block), prototype.span);
            if !self.declares(&symbol) {
                continue;
            }
            let declarator = renamed(&prototype.declarator, &symbol);
            // A prototype names something defined elsewhere, so only `static`
            // makes it file-local.
            let linkage = self.linkage(prototype.span, prototype.specs.is_pub);
            output.push_str(&format!(
                "{linkage}{} {};\n",
                specs_text_in(&prototype.specs, spelling),
                declarator_text_in(&declarator, spelling)
            ));
        }
        if let Some(trait_name) = &block.trait_name
            && self.trait_value(trait_name)
        {
            let symbol = names::trait_value(&trait_name.text, &receiver);
            if self.used_symbol(&symbol) {
                let name = &trait_name.text;
                output.push_str(&format!(
                    "static {name} {symbol}(void);
"
                ));
            }
        }
        Ok(output)
    }

    /// Whether a trait in this unit can be a runtime value.
    fn trait_value(&self, trait_name: &Name) -> bool {
        self.traits
            .get(&trait_name.text)
            .is_some_and(|declaration| trait_value_possible(declaration))
    }

    /// Print the functions and the trait constructor of one impl block.
    fn impl_text(&self, block: &'a ImplBlock) -> Result<String, Diagnostic> {
        if !block.generics.is_empty() {
            return Ok(String::new());
        }
        let receiver = Ty::from_text(&type_name_text(&block.self_type)).base;
        let spelling = self.spelling(&receiver);
        let mut output = String::new();
        for method in self.methods(block) {
            let Some(name) = &method.declarator.name else {
                continue;
            };
            let symbol = self.symbol(&receiver, &name.text, trait_of(block), method.span);
            if !self.declares(&symbol) {
                continue;
            }
            let declarator = renamed(method.declarator, &symbol);
            let linkage = self.linkage(method.span, method.specs.is_pub);
            let signature = format!(
                "{linkage}{} {}",
                specs_text_in(method.specs, spelling),
                declarator_text_in(&declarator, spelling)
            );
            let mut body = Body::named(
                self,
                Some(self.return_type_text(method.specs, method.declarator)),
                receiver.clone(),
            );
            body.parameters(method.declarator);
            output.push_str(&format!("{signature} {}", body.render_block(method.body)?));
            output.push_str("\n\n");
        }
        if let Some(trait_name) = &block.trait_name
            && self.trait_value(trait_name)
            && self.first_trait_impl(block, &receiver, &trait_name.text)
        {
            let symbol = names::trait_value(&trait_name.text, &receiver);
            if self.used_symbol(&symbol) {
                output.push_str(&self.trait_constructor(block, &receiver, &trait_name.text)?);
            }
        }
        Ok(output)
    }

    /// Whether `block` is the first implementation of its trait for its
    /// receiver.
    ///
    /// A declaration block and the definition block that continues it are two
    /// blocks of one implementation; the trait-value constructor is printed
    /// once, from the first of them.
    fn first_trait_impl(&self, block: &ImplBlock, receiver: &str, trait_name: &str) -> bool {
        self.impls
            .get(receiver)
            .and_then(|blocks| {
                blocks.iter().find(|other| {
                    other
                        .trait_name
                        .as_ref()
                        .is_some_and(|name| name.text == trait_name)
                })
            })
            .is_some_and(|first| first.span == block.span)
    }

    /// The constructor that fills a trait value with one implementation.
    fn trait_constructor(
        &self,
        block: &ImplBlock,
        receiver: &str,
        trait_name: &str,
    ) -> Result<String, Diagnostic> {
        let Some(declaration) = self.traits.get(trait_name) else {
            return Err(Diagnostic::new(
                block.span,
                format!("trait '{trait_name}' is not declared in this unit"),
            ));
        };
        let symbol = names::trait_value(trait_name, receiver);
        let mut output =
            format!("static {trait_name} {symbol}(void) {{\n    {trait_name} value;\n");
        for method in &declaration.methods {
            let Some(name) = &method.declarator.name else {
                continue;
            };
            // The implementation is either written in the block or inherited
            // from the trait's default body; both are methods of this receiver,
            // and the compiler named both.
            let implementation = match self.methods(block).into_iter().find(|method| {
                method
                    .declarator
                    .name
                    .as_ref()
                    .is_some_and(|method| method.text == name.text)
            }) {
                Some(implementation) => {
                    self.symbol(receiver, &name.text, Some(trait_name), implementation.span)
                }
                // A header may only declare the method; the constructor then
                // names the same external symbol the definition uses.
                None => match find_prototype(block, &name.text) {
                    Some(prototype) => {
                        self.symbol(receiver, &name.text, Some(trait_name), prototype.span)
                    }
                    None => {
                        return Err(Diagnostic::new(
                            method.span,
                            format!(
                                "'impl {trait_name} for {receiver}' does not define '{}'",
                                name.text
                            ),
                        ));
                    }
                },
            };
            self.mark_used(&implementation);
            output.push_str(&format!(
                "    value.{} = (typeof(value.{})){implementation};\n",
                name.text, name.text
            ));
        }
        output.push_str("    return value;\n}\n\n");
        Ok(output)
    }
}

/// The storage class a declaration keeps in the C output.
///
/// `specs_text_in` prints the type only, so the prefix belongs here: without it
/// an `extern FILE *stdout;` from a library header becomes a tentative
/// definition, and the generated object file then defines a zeroed `stdout`
/// that hides the stream the C library set up.
fn storage_prefix(storage: Option<ast::StorageClass>) -> &'static str {
    match storage {
        Some(ast::StorageClass::Typedef) => "typedef ",
        Some(ast::StorageClass::Extern) => "extern ",
        Some(ast::StorageClass::Static) => "static ",
        Some(ast::StorageClass::Auto) | Some(ast::StorageClass::Register) | None => "",
    }
}

/// Whether a declaration declares a template rather than naming an instance.
///
/// A generic function or a generic record definition has no C spelling, so it
/// prints nothing. A declaration that merely mentions an instance does print:
/// its type is a concrete name such as Box_int.
fn is_template(declarator: &Declarator) -> bool {
    declarator.derived.iter().any(|derived| match derived {
        DerivedDeclarator::Function(function) => !function.generics.is_empty(),
        _ => false,
    })
}

fn is_template_declaration(declaration: &Declaration) -> bool {
    for specifier in &declaration.specs.type_specs {
        match specifier {
            TypeSpecifier::Record(record) if !record.generics.is_empty() => return true,
            TypeSpecifier::Enum(enumeration) if !enumeration.generics.is_empty() => return true,
            _ => {}
        }
    }
    declaration
        .declarators
        .iter()
        .any(|declarator| is_template(&declarator.declarator))
}

/// Whether a declaration only names types: a typedef, or a tag definition with
/// no initializer of its own.
///
/// C wants to see a type before a prototype that mentions it, so these are
/// printed before the prototypes however late they stand in the source.
fn is_type_declaration(declaration: &Declaration) -> bool {
    let names_types = declaration.specs.storage == Some(ast::StorageClass::Typedef)
        || declaration
            .specs
            .type_specs
            .iter()
            .any(|specifier| match specifier {
                TypeSpecifier::Record(record) => record.members.is_some(),
                TypeSpecifier::Enum(enumeration) => enumeration.enumerators.is_some(),
                _ => false,
            });
    names_types
        && declaration
            .declarators
            .iter()
            .all(|declarator| declarator.init.is_none())
}

/// One C type definition and the names of the types it stores by value.
struct TypeBlock {
    /// The C name the definition introduces.
    name: String,
    text: String,
    /// The C names of the types it needs complete first.
    needs: Vec<String>,
}

/// Print the type definitions so a type a definition stores by value is
/// complete before it.
///
/// A pointer only needs the forward declarations printed first, so a cycle is
/// not an error here: the definitions left over keep their own order, and the
/// host compiler reports anything the cycle really leaves incomplete.
fn order_type_blocks(blocks: Vec<TypeBlock>) -> String {
    let defined: std::collections::HashSet<String> =
        blocks.iter().map(|block| block.name.clone()).collect();
    let mut remaining = blocks;
    let mut done = std::collections::HashSet::new();
    let mut output = String::new();
    while !remaining.is_empty() {
        let mut ready = Vec::new();
        let mut index = 0;
        while index < remaining.len() {
            let unmet = {
                let block = &remaining[index];
                block.needs.iter().any(|need| {
                    need != &block.name && defined.contains(need) && !done.contains(need)
                })
            };
            if unmet {
                index += 1;
            } else {
                ready.push(remaining.remove(index));
            }
        }
        if ready.is_empty() {
            // A cycle of by-value members: keep the order they came in.
            ready = std::mem::take(&mut remaining);
        }
        for block in ready {
            done.insert(block.name.clone());
            output.push_str(&block.text);
            output.push_str("\n\n");
        }
    }
    output
}

/// The C name a type declaration introduces: its tag, or the name it declares.
fn declared_type_name(declaration: &Declaration) -> Option<String> {
    for specifier in &declaration.specs.type_specs {
        match specifier {
            TypeSpecifier::Record(record) => {
                if let Some(tag) = &record.tag {
                    return Some(tag.text.clone());
                }
            }
            TypeSpecifier::Enum(enumeration) => {
                if let Some(tag) = &enumeration.tag {
                    return Some(tag.text.clone());
                }
            }
            _ => {}
        }
    }
    declaration.declarators.iter().find_map(|declarator| {
        declarator
            .declarator
            .name
            .as_ref()
            .map(|name| name.text.clone())
    })
}

/// The forward typedef of a record definition, if the specifier list is one.
fn forward_record(specs: &DeclSpecs) -> Option<String> {
    let [TypeSpecifier::Record(record)] = &specs.type_specs[..] else {
        return None;
    };
    let tag = record.tag.as_ref()?;
    if record.members.is_none() || !record.generics.is_empty() {
        return None;
    }
    let keyword = match record.kind {
        ast::RecordKind::Struct => "struct",
        ast::RecordKind::Union => "union",
    };
    Some(format!("typedef {keyword} {tag} {tag};", tag = tag.text))
}

/// The key a generic use is looked up under: the source spelling.
fn instance_key(name: &str, args: &[String]) -> String {
    format!("{name}<{}>", args.join(", "))
}

/// The concrete arguments of a generic use, spelled the way this output spells
/// types: a type argument through the type printer, a `const` one as written.
fn generic_argument_text(args: &[GenericArg], spelling: Spelling) -> Vec<String> {
    args.iter()
        .map(|arg| match &arg.kind {
            GenericArgKind::Type(type_name) => type_name_text_with(type_name, spelling),
            GenericArgKind::Const(value) => crate::c::types::const_argument_text(value),
        })
        .collect()
}

/// The record name of a generic impl block, if the block implements one.
fn generic_receiver(block: &ImplBlock) -> Option<String> {
    match &block.self_type.specs.type_specs[..] {
        [TypeSpecifier::Generic(generic)] => Some(generic.name.text.clone()),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Specializations
// ---------------------------------------------------------------------------

impl<'a> Backend<'a> {
    fn is_template(&self, name: &str) -> bool {
        self.templates.contains_key(name)
            || self.generic_records.contains_key(name)
            || self.generic_enums.contains_key(name)
    }

    /// The compiler's plan for one specialization.
    fn plan(
        &self,
        name: &str,
        args: &[String],
        receiver: Option<&str>,
    ) -> Option<&'a InstancePlan> {
        self.lowered.instances.iter().find(|plan| {
            plan.name == name && plan.args == args && plan.receiver.as_deref() == receiver
        })
    }

    /// The type parameters of a template, in declaration order.
    fn parameters(&self, name: &str) -> Vec<String> {
        if let Some(record) = self.generic_records.get(name) {
            return record
                .generics
                .iter()
                .map(|param| param.name.text.clone())
                .collect();
        }
        if let Some(enumeration) = self.generic_enums.get(name) {
            return enumeration
                .generics
                .iter()
                .map(|param| param.name.text.clone())
                .collect();
        }
        if let Some(function) = self.templates.get(name) {
            return generic_parameters(&function.declarator);
        }
        Vec::new()
    }

    /// One instance's type parameters bound to its arguments.
    fn bindings(&self, instance: &Instance) -> HashMap<String, String> {
        self.parameters(&instance.name)
            .into_iter()
            .zip(instance.args.iter().cloned())
            .collect()
    }

    /// How a specialization spells types: its bindings, no parameter lists.
    fn instance_spelling<'s>(
        &'s self,
        self_name: &'s str,
        bindings: &'s HashMap<String, String>,
    ) -> Spelling<'s> {
        Spelling {
            self_name,
            instances: Some(&self.type_names),
            bindings: Some(bindings),
            generics: false,
            preserve_generic_args: false,
        }
    }

    /// The prototype each specialization needs before its body: one per
    /// generic function and one per method of a materialized impl.
    fn instance_declarations(&self) -> Result<String, Diagnostic> {
        let mut output = String::new();
        for instance in self.instances.borrow().clone() {
            if let Some(receiver) = &instance.receiver {
                output.push_str(&self.method_instance_declaration(&instance, receiver)?);
                continue;
            }
            if let Some(function) = self.templates.get(&instance.name) {
                let bindings = self.bindings(&instance);
                let name = instance.c_name(self);
                if !self.used_symbol(&name) {
                    continue;
                }
                let spelling = self.instance_spelling("void", &bindings);
                let declarator = renamed(&function.declarator, &name);
                output.push_str(&format!(
                    "static {} {};\n",
                    specs_text_in(&function.specs, spelling),
                    declarator_text_in(&declarator, spelling)
                ));
            }
        }
        Ok(output)
    }

    /// The bodies of every specialization, and the ones they need in turn.
    fn instantiate(&self, output: &mut String) -> Result<(), Diagnostic> {
        let mut index = 0;
        loop {
            let instance = {
                let instances = self.instances.borrow();
                match instances.get(index) {
                    Some(instance) => instance.clone(),
                    None => break,
                }
            };
            index += 1;
            let name = instance.c_name(self);
            // An instance is a file-local function: one the output never names
            // is dead code, and the C compiler reports it.
            if !self.used_symbol(&name) {
                continue;
            }
            if let Some(receiver) = &instance.receiver {
                output.push_str(&self.method_instance_body(&instance, receiver)?);
                continue;
            }
            if let Some(function) = self.templates.get(&instance.name) {
                let bindings = self.bindings(&instance);
                let spelling = self.instance_spelling(&name, &bindings);
                let declarator = renamed(&function.declarator, &name);
                // A specialization of a free function has no receiver, so a
                // call inside it resolves against the span alone.
                let mut body = Body::instance(
                    self,
                    Some(return_type_in(
                        &function.specs,
                        &function.declarator,
                        spelling,
                    )),
                    "void".to_string(),
                    bindings.clone(),
                );
                body.parameters(&function.declarator);
                output.push_str(&format!(
                    "static {} {} {}\n\n",
                    specs_text_in(&function.specs, spelling),
                    declarator_text_in(&declarator, spelling),
                    body.render_block(&function.body)?
                ));
            }
        }
        Ok(())
    }
}

/// The type parameters written on a function declarator, in order.
fn generic_parameters(declarator: &Declarator) -> Vec<String> {
    let Some(position) = function_position(declarator) else {
        return Vec::new();
    };
    let Some(DerivedDeclarator::Function(function)) = declarator.derived.get(position) else {
        return Vec::new();
    };
    function
        .generics
        .iter()
        .map(|param| param.name.text.clone())
        .collect()
}

/// A function's return type under one spelling.
fn return_type_in(specs: &DeclSpecs, declarator: &Declarator, spelling: Spelling) -> String {
    let position = function_position(declarator).unwrap_or(0);
    let returned = Declarator {
        span: declarator.span,
        name: None,
        derived: declarator.derived[position + 1..].to_vec(),
    };
    let base = specs_text_in(specs, spelling);
    let suffix = declarator_text_in(&returned, spelling);
    if suffix.is_empty() {
        base
    } else {
        format!("{base} {suffix}")
    }
}

impl<'a> Backend<'a> {
    /// The template a generic method instance specializes.
    ///
    /// A trait method's default body lives in the trait declaration; an
    /// inherent generic method lives in the impl template of its record.
    fn method_template(
        &self,
        plan: &InstancePlan,
    ) -> Option<(&'a DeclSpecs, &'a Declarator, &'a Block)> {
        if let Some(trait_name) = &plan.trait_name {
            let declaration = self.traits.get(trait_name)?;
            let method = declaration.methods.iter().find(|method| {
                method
                    .declarator
                    .name
                    .as_ref()
                    .is_some_and(|name| name.text == plan.name)
            })?;
            return Some((&method.specs, &method.declarator, method.body.as_ref()?));
        }
        let impl_name = plan.impl_name.as_deref()?;
        let base = impl_name.split('<').next().unwrap_or(impl_name);
        for block in self.generic_impls.get(base)?.iter() {
            if let Some(method) = find_method(block, &plan.name) {
                return Some((&method.specs, &method.declarator, &method.body));
            }
        }
        None
    }

    /// The type arguments of one generic method instance: the method's own and,
    /// for a record instance, the receiver's.
    fn method_bindings(&self, plan: &InstancePlan) -> HashMap<String, String> {
        let mut bindings = HashMap::new();
        if let Some((_, declarator, _)) = self.method_template(plan) {
            for (parameter, arg) in generic_parameters(declarator).iter().zip(&plan.args) {
                bindings.insert(parameter.clone(), arg.clone());
            }
        }
        // The receiver's instance binds the impl's own type parameters: the
        // template may be written on a record or on an enum.
        if let Some(receiver) = &plan.receiver
            && let Some(type_plan) = self
                .lowered
                .instances
                .iter()
                .find(|other| other.type_name.as_deref() == Some(receiver.as_str()))
        {
            for (parameter, arg) in self
                .parameters(&type_plan.name)
                .into_iter()
                .zip(&type_plan.args)
            {
                bindings.insert(parameter, arg.clone());
            }
        }
        bindings
    }

    /// The declaration of one generic method instance.
    fn method_instance_declaration(
        &self,
        instance: &Instance,
        receiver: &str,
    ) -> Result<String, Diagnostic> {
        let Some(plan) = self.plan(&instance.name, &instance.args, Some(receiver)) else {
            return Ok(String::new());
        };
        let Some((specs, declarator, _)) = self.method_template(plan) else {
            return Ok(String::new());
        };
        let Some(symbol) = &plan.symbol else {
            return Ok(String::new());
        };
        let name = names::symbol(symbol);
        if !self.used_symbol(&name) {
            return Ok(String::new());
        }
        let bindings = self.method_bindings(plan);
        let spelling = self.instance_spelling(receiver, &bindings);
        let declarator = renamed(declarator, &name);
        Ok(format!(
            "static {} {};\n",
            specs_text_in(specs, spelling),
            declarator_text_in(&declarator, spelling)
        ))
    }

    /// The body of one generic method instance.
    fn method_instance_body(
        &self,
        instance: &Instance,
        receiver: &str,
    ) -> Result<String, Diagnostic> {
        let Some(plan) = self.plan(&instance.name, &instance.args, Some(receiver)) else {
            return Ok(String::new());
        };
        let Some((specs, declarator, body)) = self.method_template(plan) else {
            return Ok(String::new());
        };
        let Some(symbol) = &plan.symbol else {
            return Ok(String::new());
        };
        let bindings = self.method_bindings(plan);
        let spelling = self.instance_spelling(receiver, &bindings);
        let declarator = renamed(declarator, &names::symbol(symbol));
        let returned = return_type_in(specs, &declarator, spelling);
        let mut printed =
            Body::instance(self, Some(returned), receiver.to_string(), bindings.clone());
        printed.parameters(&declarator);
        Ok(format!(
            "static {} {} {}\n\n",
            specs_text_in(specs, spelling),
            declarator_text_in(&declarator, spelling),
            printed.render_block(body)?
        ))
    }
}

/// The trait an impl block implements, if it implements one.
fn trait_of(block: &ImplBlock) -> Option<&str> {
    block.trait_name.as_ref().map(|name| name.text.as_str())
}

/// One method to print: the block's own definition or an inherited default.
struct Method<'a> {
    span: Span,
    specs: &'a DeclSpecs,
    declarator: &'a Declarator,
    body: &'a Block,
}

/// The position of the function operator in a declarator.
fn function_position(declarator: &Declarator) -> Option<usize> {
    declarator
        .derived
        .iter()
        .position(|derived| matches!(derived, DerivedDeclarator::Function(_)))
}

fn has_function(declarator: &Declarator) -> bool {
    function_position(declarator).is_some()
}

/// Whether C can take the address of an expression.
fn is_lvalue(expression: &Expr) -> bool {
    matches!(
        expression,
        Expr::Name(_)
            | Expr::Member { .. }
            | Expr::Index { .. }
            | Expr::Unary {
                op: UnaryOp::Dereference,
                ..
            }
    )
}

/// The bodiless declaration of one method, if the block has one.
fn find_prototype<'a>(block: &'a ImplBlock, name: &str) -> Option<&'a ast::MethodPrototype> {
    block.prototypes.iter().find(|prototype| {
        prototype
            .declarator
            .name
            .as_ref()
            .is_some_and(|prototype| prototype.text == name)
    })
}

fn find_method<'a>(block: &'a ImplBlock, name: &str) -> Option<&'a FunctionDecl> {
    block.methods.iter().find(|method| {
        method
            .declarator
            .name
            .as_ref()
            .is_some_and(|method| method.text == name)
    })
}

/// Copy a declarator under another name, keeping its parameters.
fn renamed(declarator: &Declarator, name: &str) -> Declarator {
    let mut renamed = declarator.clone();
    renamed.name = Some(Name::new(name, declarator.span));
    renamed
}

// ---------------------------------------------------------------------------
// Bodies
// ---------------------------------------------------------------------------

/// One function body, or the file scope when it prints a global initializer.
struct Body<'a, 'b> {
    backend: &'a Backend<'b>,
    /// The enclosing function's return type, spelled for a compound literal.
    /// Empty at file scope, where a propagation cannot appear.
    return_ty: Option<String>,
    /// What `self` means here: the receiver type inside an impl method, `void`
    /// everywhere else.
    self_name: String,
    /// Type parameters of the specialization this body belongs to, bound to
    /// their concrete spelling. Empty outside a specialization.
    bindings: HashMap<String, String>,
    /// Whether the body belongs to a specialization, which prints no parameter
    /// lists because C has no template.
    instance: bool,
    /// The names visible in the body, innermost scope last.
    scopes: Vec<HashMap<String, Ty>>,
    /// The `defer` variables of each block, innermost block last: the name and
    /// the receiver type its `drop` call needs.
    defers: Vec<Vec<(String, Ty)>>,
    /// `defers.len()` at the statement a `break` leaves.
    breaks: Vec<usize>,
    /// `defers.len()` at the loop a `continue` reaches.
    continues: Vec<usize>,
    /// The defer variables each `goto` leaves: keyed by the statement's span and
    /// label, innermost scope and last declaration first.
    goto_drops: HashMap<(Span, String), Vec<String>>,
    /// The payload enum each enclosing `switch` selects, innermost last. A
    /// `case` label reads its variant names from here.
    switch_enums: Vec<Option<PayloadEnumPlan>>,
}

impl<'a, 'b> Body<'a, 'b> {
    fn new(backend: &'a Backend<'b>, return_ty: Option<String>) -> Body<'a, 'b> {
        Body::named(backend, return_ty, "void".to_string())
    }

    fn named(
        backend: &'a Backend<'b>,
        return_ty: Option<String>,
        self_name: String,
    ) -> Body<'a, 'b> {
        Body {
            backend,
            return_ty,
            self_name,
            bindings: HashMap::new(),
            instance: false,
            scopes: vec![HashMap::new()],
            defers: vec![Vec::new()],
            breaks: Vec::new(),
            continues: Vec::new(),
            goto_drops: HashMap::new(),
            switch_enums: Vec::new(),
        }
    }

    /// A body printed as one specialization of a template.
    fn instance(
        backend: &'a Backend<'b>,
        return_ty: Option<String>,
        self_name: String,
        bindings: HashMap<String, String>,
    ) -> Body<'a, 'b> {
        Body {
            backend,
            return_ty,
            self_name,
            bindings,
            instance: true,
            scopes: vec![HashMap::new()],
            defers: vec![Vec::new()],
            breaks: Vec::new(),
            continues: Vec::new(),
            goto_drops: HashMap::new(),
            switch_enums: Vec::new(),
        }
    }

    /// How this body spells types.
    fn spelling(&self) -> Spelling<'_> {
        Spelling {
            self_name: &self.self_name,
            instances: Some(&self.backend.type_names),
            bindings: Some(&self.bindings),
            generics: !self.instance,
            preserve_generic_args: false,
        }
    }

    /// Bind the declared parameters, so a body can resolve their types.
    fn parameters(&mut self, declarator: &Declarator) {
        let Some(position) = function_position(declarator) else {
            return;
        };
        let Some(DerivedDeclarator::Function(function)) = declarator.derived.get(position) else {
            return;
        };
        for parameter in &function.params {
            let Some(name) = parameter
                .declarator
                .as_ref()
                .and_then(|declarator| declarator.name.as_ref())
            else {
                continue;
            };
            let ty = match &parameter.declarator {
                Some(declarator) => {
                    self.backend
                        .ty_with(&parameter.specs, declarator, self.spelling())
                }
                None => Ty::from_text(&specs_text_in(&parameter.specs, self.spelling())),
            };
            self.bind(&name.text, ty);
        }
    }

    fn bind(&mut self, name: &str, ty: Ty) {
        if let Some(scope) = self.scopes.last_mut() {
            scope.insert(name.to_string(), ty);
        }
    }

    fn local(&self, name: &str) -> Option<Ty> {
        self.scopes
            .iter()
            .rev()
            .find_map(|scope| scope.get(name).cloned())
    }

    /// Print one identifier: a local keeps its spelling, and a file-scope
    /// function takes the symbol the compiler gave it.
    fn identifier(&self, name: &Name) -> String {
        match self.local(&name.text) {
            Some(_) => name.text.clone(),
            None => {
                let symbol = self.backend.symbol_of(&name.text);
                self.backend.mark_used(&symbol);
                symbol
            }
        }
    }

    /// The guard flag of one defer variable.
    fn defer_flag(name: &str) -> String {
        format!("__shy_moved_{name}")
    }

    /// Whether a name here is a defer variable.
    fn is_defer(&self, name: &str) -> bool {
        self.defers
            .iter()
            .rev()
            .any(|scope| scope.iter().any(|(declared, _)| declared == name))
    }

    /// The guarded drop calls for every defer variable from block 'depth'
    /// outward, innermost block and last declaration first.
    fn scope_drops(&self, depth: usize) -> String {
        let mut output = String::new();
        for scope in self.defers[depth..].iter().rev() {
            for (name, ty) in scope.iter().rev() {
                let Some(symbol) = self.backend.method(&ty.base, "drop") else {
                    continue;
                };
                self.backend.mark_used(&symbol);
                output.push_str(&format!(
                    "if (!{}) {symbol}(&{name}); ",
                    Body::defer_flag(name)
                ));
            }
        }
        output
    }

    /// The move flag of the defer objects one declaration introduced.
    fn defer_declarations(&mut self, names: &[&Name], indent: &str) -> String {
        let mut output = String::new();
        let mut declared = Vec::new();
        for name in names {
            let Some(ty) = self.local(&name.text) else {
                continue;
            };
            output.push_str(&format!(
                "{indent}_Bool {} = 0;\n",
                Body::defer_flag(&name.text)
            ));
            declared.push((name.text.clone(), ty));
        }
        if let Some(scope) = self.defers.last_mut() {
            scope.extend(declared);
        }
        output
    }

    /// Render one function body, with the defer drops each goto in it leaves.
    fn render_block(&mut self, block: &Block) -> Result<String, Diagnostic> {
        self.goto_drops = collect_goto_drops(block);
        self.block(block, 0)
    }

    fn block(&mut self, block: &Block, depth: usize) -> Result<String, Diagnostic> {
        self.scopes.push(HashMap::new());
        self.defers.push(Vec::new());
        let mut output = String::from("{\n");
        for statement in &block.stmts {
            output.push_str(&self.statement(statement, depth + 1)?);
        }
        let drops = self.scope_drops(self.defers.len() - 1);
        if !drops.is_empty() {
            output.push_str(&format!("{}{drops}\n", "    ".repeat(depth + 1)));
        }
        self.defers.pop();
        self.scopes.pop();
        output.push_str(&"    ".repeat(depth));
        output.push('}');
        Ok(output)
    }

    fn statement(&mut self, statement: &Stmt, depth: usize) -> Result<String, Diagnostic> {
        let indent = "    ".repeat(depth);
        let mut output = String::new();
        match statement {
            Stmt::Empty(_) => output.push_str(&format!("{indent};\n")),
            Stmt::Expr { expr, .. } => {
                output.push_str(&format!("{indent}{};\n", self.expr(expr)?));
            }
            Stmt::Decl(declaration) => {
                output.push_str(&indent);
                output.push_str(&self.declaration(declaration)?);
                output.push('\n');
                if declaration.is_defer {
                    let names: Vec<&Name> = declaration
                        .declarators
                        .iter()
                        .filter_map(|declarator| declarator.declarator.name.as_ref())
                        .collect();
                    output.push_str(&self.defer_declarations(&names, &indent));
                }
            }
            Stmt::Let(declaration) => {
                let qualifier = if declaration.is_mut {
                    "__auto_type"
                } else {
                    "const __auto_type"
                };
                let value = self.expr(&declaration.init)?;
                if let Some(ty) = self.ty(&declaration.init) {
                    self.bind(&declaration.name.text, ty);
                }
                output.push_str(&format!(
                    "{indent}{qualifier} {} = {};\n",
                    declaration.name.text, value
                ));
                if declaration.is_defer {
                    output.push_str(&self.defer_declarations(&[&declaration.name], &indent));
                }
            }
            Stmt::Block(block) => {
                output.push_str(&indent);
                output.push_str(&self.block(block, depth)?);
                output.push('\n');
            }
            Stmt::If {
                condition,
                then_branch,
                else_branch,
                ..
            } => {
                output.push_str(&format!(
                    "{indent}if ({}) {}",
                    self.expr(condition)?,
                    self.branch_statement(then_branch, depth)?.trim_start()
                ));
                if let Some(else_branch) = else_branch {
                    output.pop();
                    output.push_str(" else ");
                    output.push_str(self.branch_statement(else_branch, depth)?.trim_start());
                }
                output.push('\n');
            }
            Stmt::While {
                condition, body, ..
            } => {
                let condition = self.expr(condition)?;
                self.breaks.push(self.defers.len());
                self.continues.push(self.defers.len());
                let body = self.branch_statement(body, depth)?.trim_start().to_string();
                self.continues.pop();
                self.breaks.pop();
                output.push_str(&format!("{indent}while ({condition}) {body}"));
                output.push('\n');
            }
            Stmt::DoWhile {
                body, condition, ..
            } => {
                self.breaks.push(self.defers.len());
                self.continues.push(self.defers.len());
                let body = self.branch_statement(body, depth)?.trim_start().to_string();
                self.continues.pop();
                self.breaks.pop();
                output.push_str(&format!("{indent}do {body} "));
                output.push_str(&format!("while ({});\n", self.expr(condition)?));
            }
            Stmt::For {
                init,
                condition,
                step,
                body,
                ..
            } => {
                let init_text = match init.as_deref() {
                    None => String::new(),
                    Some(ForInit::Expr(expr)) => self.expr(expr)?,
                    Some(ForInit::Decl(declaration)) => self
                        .declaration(declaration)?
                        .trim_end_matches(';')
                        .to_string(),
                    Some(ForInit::Let(declaration)) => {
                        let value = self.expr(&declaration.init)?;
                        if let Some(ty) = self.ty(&declaration.init) {
                            self.bind(&declaration.name.text, ty);
                        }
                        format!("__auto_type {} = {value}", declaration.name.text)
                    }
                };
                let condition_text = match condition.as_deref() {
                    Some(condition) => self.expr(condition)?,
                    None => String::new(),
                };
                let step_text = match step.as_deref() {
                    Some(step) => self.expr(step)?,
                    None => String::new(),
                };
                self.breaks.push(self.defers.len());
                self.continues.push(self.defers.len());
                let body = self.branch_statement(body, depth)?.trim_start().to_string();
                self.continues.pop();
                self.breaks.pop();
                output.push_str(&format!(
                    "{indent}for ({init_text}; {condition_text}; {step_text}) {body}"
                ));
                output.push('\n');
            }
            Stmt::Switch {
                condition, body, ..
            } => {
                let plan = self
                    .ty(condition)
                    .and_then(|ty| self.backend.enum_plan(&ty))
                    .cloned();
                let condition = match &plan {
                    // A niche stores the payload, so C switches on whether the
                    // value is the failure one: zero is the payload-free variant.
                    Some(plan) if plan.niche => format!("({}) != 0", self.expr(condition)?),
                    Some(_) => {
                        let ty = self.ty(condition).expect("payload enum type");
                        format!("{}.tag", self.object_text(condition, &ty)?)
                    }
                    None => self.expr(condition)?,
                };
                self.breaks.push(self.defers.len());
                self.switch_enums.push(plan);
                let body = self.branch_statement(body, depth)?.trim_start().to_string();
                self.switch_enums.pop();
                self.breaks.pop();
                output.push_str(&format!("{indent}switch ({condition}) {body}"));
                output.push('\n');
            }
            Stmt::Case {
                value, end, stmt, ..
            } => {
                let label = match self.case_label(value)? {
                    Some(label) => label,
                    None => self.expr(value)?,
                };
                output.push_str(&format!("{indent}case {label}"));
                if let Some(end) = end {
                    output.push_str(&format!(" ... {}", self.expr(end)?));
                }
                output.push_str(": ");
                output.push_str(self.statement(stmt, depth)?.trim_start());
            }
            Stmt::Default { stmt, .. } => {
                output.push_str(&format!("{indent}default: "));
                output.push_str(self.statement(stmt, depth)?.trim_start());
            }
            Stmt::Label { name, stmt, .. } => {
                output.push_str(&format!("{indent}{}: ", name.text));
                output.push_str(self.statement(stmt, depth)?.trim_start());
            }
            Stmt::Goto { label, span, .. } => {
                // A goto that leaves defer scopes drops them first, the same
                // way the IR backends do.
                let mut drops = String::new();
                if let Some(names) = self.goto_drops.get(&(*span, label.text.clone())) {
                    for name in names.clone() {
                        let Some(ty) = self.local(&name) else {
                            continue;
                        };
                        let Some(symbol) = self.backend.method(&ty.base, "drop") else {
                            continue;
                        };
                        self.backend.mark_used(&symbol);
                        drops.push_str(&format!(
                            "if (!{}) {symbol}(&{name}); ",
                            Body::defer_flag(&name)
                        ));
                    }
                }
                output.push_str(&self.jump_statement(
                    &indent,
                    drops,
                    &format!("goto {};", label.text),
                ));
            }
            Stmt::Break(_) => {
                let depth = self.breaks.last().copied().unwrap_or(0);
                output.push_str(&self.jump_statement(&indent, self.scope_drops(depth), "break;"));
            }
            Stmt::Continue(_) => {
                let depth = self.continues.last().copied().unwrap_or(0);
                output.push_str(&self.jump_statement(
                    &indent,
                    self.scope_drops(depth),
                    "continue;",
                ));
            }
            Stmt::Return { value, .. } => {
                let value = match value {
                    Some(value) => {
                        let mut printed = self.expr(value)?;
                        if let Expr::Name(name) = value
                            && self.is_defer(&name.text)
                        {
                            printed = format!("({} = 1, {printed})", Body::defer_flag(&name.text));
                        }
                        Some(printed)
                    }
                    None => None,
                };
                output.push_str(&self.return_statement(value, depth));
            }
            Stmt::Asm(asm) => {
                output.push_str(&format!("{indent}{};\n", asm_text(asm)?));
            }
            Stmt::AllowDeferCopy { stmt, .. } => {
                output.push_str(&self.statement(stmt, depth)?);
            }
        }
        Ok(output)
    }

    /// One branch of an if, loop, or switch. A \`defer\` declaration written
    /// there without braces owns only that statement, so it prints as a braced
    /// block of its own and drops at the end of the block.
    fn branch_statement(&mut self, statement: &Stmt, depth: usize) -> Result<String, Diagnostic> {
        if !statement.is_defer_declaration() {
            return self.statement(statement, depth);
        }
        let indent = "    ".repeat(depth);
        let mut output = String::from("{\n");
        self.scopes.push(HashMap::new());
        self.defers.push(Vec::new());
        output.push_str(&self.statement(statement, depth + 1)?);
        let drops = self.scope_drops(self.defers.len() - 1);
        if !drops.is_empty() {
            output.push_str(&format!("{}{drops}\n", "    ".repeat(depth + 1)));
        }
        self.defers.pop();
        self.scopes.pop();
        output.push_str(&format!("{indent}}}"));
        Ok(output)
    }

    /// One jump statement (goto, break, continue, return) that drops the defer
    /// variables it leaves first. The braces keep the drops and the jump
    /// together when the statement is a brace-less branch of an if, loop, or
    /// case label.
    fn jump_statement(&self, indent: &str, drops: String, text: &str) -> String {
        if drops.is_empty() {
            format!("{indent}{text}\n")
        } else {
            format!("{indent}{{ {drops}{text} }}\n")
        }
    }

    /// A return that drops the defer variables still in scope first. The value
    /// is evaluated into a temporary, so it is read before the drops run.
    fn return_statement(&self, value: Option<String>, depth: usize) -> String {
        let indent = "    ".repeat(depth);
        let drops = self.scope_drops(0);
        if drops.is_empty() {
            return match value {
                Some(value) => format!("{indent}return {value};\n"),
                None => format!("{indent}return;\n"),
            };
        }
        match (value, self.return_ty.clone()) {
            (Some(value), Some(return_ty)) => format!(
                "{indent}{{ {return_ty} __shy_return = {value}; {drops}return __shy_return; }}\n"
            ),
            (Some(value), None) => format!("{indent}{{ {drops}return {value}; }}\n"),
            (None, _) => format!("{indent}{{ {drops}return; }}\n"),
        }
    }

    fn declaration(&mut self, declaration: &Declaration) -> Result<String, Diagnostic> {
        let specs = format!(
            "{}{}",
            storage_prefix(declaration.specs.storage),
            specs_text_in(&declaration.specs, self.spelling())
        );
        // A block-scope function declaration names the same function the
        // file scope does, so it prints under the same symbol.
        let mut declarators = Vec::new();
        for declarator in &declaration.declarators {
            let declares_function = function_position(&declarator.declarator) == Some(0);
            let printed = match &declarator.declarator.name {
                Some(name) if declares_function => {
                    renamed(&declarator.declarator, &self.backend.symbol_of(&name.text))
                }
                _ => declarator.declarator.clone(),
            };
            let mut text = declarator_text_in(&printed, self.spelling());
            if let Some(initializer) = &declarator.init {
                text.push_str(" = ");
                text.push_str(&self.initializer(initializer)?);
            }
            // A function declaration is not a local object: a call through the
            // name still reaches the function.
            if let Some(name) = &declarator.declarator.name
                && !declares_function
            {
                let ty = self.backend.ty_with(
                    &declaration.specs,
                    &declarator.declarator,
                    self.spelling(),
                );
                self.bind(&name.text, ty);
            }
            declarators.push(text);
        }
        Ok(format!("{specs} {};", declarators.join(", ")))
    }

    fn initializer(&mut self, initializer: &Initializer) -> Result<String, Diagnostic> {
        match &initializer.kind {
            InitializerKind::Expr(expr) => self.expr(expr),
            InitializerKind::List(entries) => {
                let mut output = String::from("{");
                for (index, entry) in entries.iter().enumerate() {
                    if index > 0 {
                        output.push_str(", ");
                    }
                    for designator in &entry.designators {
                        match designator {
                            ast::Designator::Field(name) => {
                                output.push_str(&format!(".{} = ", name.text));
                            }
                            ast::Designator::Index(expr) => {
                                output.push_str(&format!("[{}] = ", self.expr(expr)?));
                            }
                        }
                    }
                    output.push_str(&self.initializer(&entry.value)?);
                }
                output.push('}');
                Ok(output)
            }
        }
    }
}

/// One inline assembly statement, in the form the host compiler accepts.
///
/// ShyC names its operands in the template (`{value}` or `{&value}`); GCC names
/// them in a constraint list instead, so each placeholder becomes `%[name]` and
/// one input per operand. A plain operand is a register input and an address is
/// a memory operand.
fn asm_text(asm: &AsmStmt) -> Result<String, Diagnostic> {
    match asm {
        AsmStmt::Raw { templates, .. } => {
            let template = templates
                .iter()
                .map(|part| part.raw.as_str())
                .collect::<String>();
            Ok(format!("__asm__(\"{template}\")"))
        }
        AsmStmt::Binding {
            span, templates, ..
        } => {
            let mut template = String::new();
            let mut inputs: Vec<(String, &str)> = Vec::new();
            for part in templates {
                let mut rest = part.raw.as_str();
                while let Some(open) = rest.find('{') {
                    template.push_str(&rest[..open]);
                    let Some(close) = rest[open..].find('}') else {
                        return Err(Diagnostic::new(
                            *span,
                            "asm! placeholder is missing its closing brace",
                        ));
                    };
                    let placeholder = &rest[open + 1..open + close];
                    // ShyC operands are read and written by the template, so
                    // they are read-write constraints: a value lives in a
                    // register and an address names memory.
                    let (name, constraint) = match placeholder.strip_prefix('&') {
                        Some(name) => (name, "+m"),
                        None => (placeholder, "+r"),
                    };
                    template.push_str(&format!("%[{name}]"));
                    if !inputs.iter().any(|(known, _)| known == name) {
                        inputs.push((name.to_string(), constraint));
                    }
                    rest = &rest[open + close + 1..];
                }
                template.push_str(rest);
            }
            if inputs.is_empty() {
                return Ok(format!("__asm__(\"{template}\")"));
            }
            let inputs = inputs
                .iter()
                .map(|(name, constraint)| format!("[{name}]\"{constraint}\"({name})"))
                .collect::<Vec<_>>()
                .join(", ");
            // A read-write operand belongs to the output list.
            Ok(format!("__asm__(\"{template}\" : {inputs})"))
        }
    }
}

// ---------------------------------------------------------------------------
// Expressions
// ---------------------------------------------------------------------------

impl<'a, 'b> Body<'a, 'b> {
    fn expr(&mut self, expression: &Expr) -> Result<String, Diagnostic> {
        match expression {
            Expr::Number(literal) => Ok(literal.text.clone()),
            Expr::Char(literal) => Ok(format!("{}'{}'", literal.prefix, literal.raw)),
            Expr::String(group) => Ok(group
                .parts
                .iter()
                .map(|part| format!("{}\"{}\"", part.prefix, part.raw))
                .collect::<Vec<_>>()
                .join(" ")),
            Expr::Name(name) => Ok(self.identifier(name)),
            Expr::Unary { op, operand, .. } => {
                Ok(format!("({}{})", unary_op_text(*op), self.expr(operand)?))
            }
            Expr::Postfix { op, operand, .. } => {
                Ok(format!("({}{})", self.expr(operand)?, postfix_op_text(*op)))
            }
            Expr::Binary {
                op, left, right, ..
            } => Ok(format!(
                "({} {} {})",
                self.expr(left)?,
                binary_op_text(*op),
                self.expr(right)?
            )),
            Expr::Assignment {
                op, left, right, ..
            } => Ok(format!(
                "({} {} {})",
                self.expr(left)?,
                assign_op_text(*op),
                self.expr(right)?
            )),
            Expr::Conditional {
                condition,
                then_expr,
                else_expr,
                ..
            } => Ok(format!(
                "({} ? {} : {})",
                self.expr(condition)?,
                self.expr(then_expr)?,
                self.expr(else_expr)?
            )),
            Expr::Comma { left, right, .. } => {
                Ok(format!("({}, {})", self.expr(left)?, self.expr(right)?))
            }
            Expr::VariantInit {
                qualifier,
                type_args,
                variant,
                fields,
                span,
            } => {
                let Some((plan, variant)) =
                    self.backend
                        .variant(qualifier, type_args, self.spelling(), &variant.text)
                else {
                    return Err(Diagnostic::new(*span, "struct variant has no C form"));
                };
                let fields = fields
                    .iter()
                    .map(|field| {
                        Ok(format!(
                            ".{} = {}",
                            field.name.text,
                            self.expr(&field.value)?
                        ))
                    })
                    .collect::<Result<Vec<_>, Diagnostic>>()?
                    .join(", ");
                Ok(format!(
                    "({}){{ .tag = {}, .payload.{} = {{ {} }} }}",
                    plan.name, variant.tag, variant.name, fields
                ))
            }
            Expr::Call {
                callee,
                args,
                type_args,
                span,
            } => self.call(callee, args, type_args, *span),
            Expr::Index { array, index, .. } => {
                Ok(format!("({})[{}]", self.expr(array)?, self.expr(index)?))
            }
            Expr::Member {
                object, name, op, ..
            } => self.member(object, name, *op),
            Expr::Cast {
                type_name, value, ..
            } => Ok(format!(
                "(({}){})",
                type_name_text_with(type_name, self.spelling()),
                self.expr(value)?
            )),
            Expr::SizeofType { type_name, .. } => Ok(format!(
                "sizeof({})",
                type_name_text_with(type_name, self.spelling())
            )),
            Expr::SizeofExpr { value, .. } => Ok(format!("sizeof({})", self.expr(value)?)),
            Expr::AlignofType { type_name, .. } => Ok(format!(
                "_Alignof({})",
                type_name_text_with(type_name, self.spelling())
            )),
            Expr::AlignofExpr { value, .. } => Ok(format!("_Alignof({})", self.expr(value)?)),
            // A statement expression yields the value of its last expression
            // statement, so the block's own braces are dropped: a block is a
            // statement of its own and yields no value.
            Expr::StatementExpr { block, .. } => {
                let text = self.block(block, 0)?;
                let inner = text
                    .strip_prefix('{')
                    .and_then(|text| text.strip_suffix('}'))
                    .unwrap_or(&text);
                Ok(format!("({{{inner}}})"))
            }
            Expr::Try { value, span } => self.propagate(value, *span),
            // A variant with a payload cannot be built without one, so its name
            // alone is its tag; a payload-free variant is a value.
            Expr::ScopedName {
                qualifier,
                name,
                span,
            } => self.variant_value(qualifier, &[], name, *span),
            Expr::GenericScopedName {
                qualifier,
                args: type_args,
                name,
                span,
            } => self.variant_value(qualifier, type_args, name, *span),
            other => Err(Diagnostic::new(
                other.span(),
                "this expression has no C form yet",
            )),
        }
    }

    /// Print one payload-enum variant used as a value: its logical tag.
    fn variant_value(
        &mut self,
        qualifier: &Name,
        args: &[GenericArg],
        name: &Name,
        span: Span,
    ) -> Result<String, Diagnostic> {
        if let Some((plan, variant)) =
            self.backend
                .variant(qualifier, args, self.spelling(), &name.text)
        {
            let _ = plan;
            return Ok(variant.tag.to_string());
        }
        // A plain C enum names its enumerator.
        if self.backend.enums.contains_key(&qualifier.text) {
            return Ok(name.text.clone());
        }
        Err(Diagnostic::new(
            span,
            format!(
                "'{}::{}' has no C form as a value",
                qualifier.text, name.text
            ),
        ))
    }

    /// Print a member read, resolving the transparent payload-enum members.
    fn member(&mut self, object: &Expr, name: &Name, op: MemberOp) -> Result<String, Diagnostic> {
        if let Some(text) = self.payload_value(object, name)? {
            return Ok(text);
        }
        let arrow = self.arrow(object, op);
        Ok(format!(
            "({}){}{}",
            self.expr(object)?,
            if arrow { "->" } else { "." },
            name.text
        ))
    }

    /// The payload of one variant, read from the value that stores it.
    fn payload_value(&mut self, base: &Expr, name: &Name) -> Result<Option<String>, Diagnostic> {
        let Some(ty) = self.ty(base) else {
            return Ok(None);
        };
        let Some(plan) = self.backend.enum_plan(&ty) else {
            return Ok(None);
        };
        let Some(variant) = plan
            .variants
            .iter()
            .find(|variant| variant.name == name.text)
        else {
            return Ok(None);
        };
        // A payload-free variant stores nothing: the compiler reads its payload
        // as a `void` expression, and so does C.
        if variant.payload.is_none() {
            return Ok(Some("((void)0)".to_string()));
        }
        let object = self.object_text(base, &ty)?;
        if plan.niche {
            return Ok(Some(object));
        }
        Ok(Some(format!("{object}.payload.{}", name.text)))
    }

    /// The object one access goes through: ShyC's dot reads through every
    /// pointer, so C needs one dereference per pointer.
    fn object_text(&mut self, object: &Expr, ty: &Ty) -> Result<String, Diagnostic> {
        let mut text = format!("({})", self.expr(object)?);
        for _ in 0..ty.pointers {
            text = format!("(*{text})");
        }
        Ok(text)
    }

    /// The integer a `case` label denotes when the switch value is a payload
    /// enum: a bare variant name is one of that enum's variants, and
    /// `Enum::Variant` is its tag.
    fn case_label(&mut self, value: &Expr) -> Result<Option<String>, Diagnostic> {
        let Some(plan) = self.switch_enums.last().cloned().flatten() else {
            return Ok(None);
        };
        let tag = match value {
            Expr::Name(name) => {
                // A bare name that already means something keeps its meaning:
                // C's own constants are never reinterpreted as a variant.
                if self.local(&name.text).is_some()
                    || self.backend.objects.contains_key(&name.text)
                    || self.backend.functions.contains_key(&name.text)
                    || self
                        .backend
                        .enums
                        .values()
                        .any(|enumerators| enumerators.contains(&name.text))
                {
                    return Ok(None);
                }
                plan.variants
                    .iter()
                    .find(|variant| variant.name == name.text)
                    .map(|variant| variant.tag)
            }
            Expr::ScopedName {
                qualifier, name, ..
            } => self
                .backend
                .variant(qualifier, &[], self.spelling(), &name.text)
                .map(|(_, variant)| variant.tag),
            Expr::GenericScopedName {
                qualifier,
                args,
                name,
                ..
            } => self
                .backend
                .variant(qualifier, args, self.spelling(), &name.text)
                .map(|(_, variant)| variant.tag),
            _ => None,
        };
        Ok(tag.map(|tag| tag.to_string()))
    }

    /// Print a call, resolving a method name to the function its impl block
    /// defines.
    fn call(
        &mut self,
        callee: &Expr,
        args: &[Expr],
        type_args: &[GenericArg],
        span: Span,
    ) -> Result<String, Diagnostic> {
        if args.is_empty()
            && type_args.is_empty()
            && let Expr::Member { object, name, .. } = callee
            && name.text == "type"
        {
            let ty = self
                .ty(object)
                .ok_or_else(|| Diagnostic::new(span, "type() needs a known enum type"))?;
            let object = self.object_text(object, &ty)?;
            if let Some(plan) = self.backend.enum_plan(&ty) {
                if plan.niche {
                    return Ok(format!("(({object}) != 0)"));
                }
                return Ok(format!("{object}.tag"));
            }
            if self.backend.enums.contains_key(&ty.base) {
                return Ok(object);
            }
            return Err(Diagnostic::new(span, "type() needs an enum object"));
        }
        // x.move() reads a defer variable and sets its moved flag.
        if let Expr::Member {
            object, name, op, ..
        } = callee
            && name.text == "move"
        {
            let _ = op;
            if !args.is_empty() {
                return Err(Diagnostic::new(span, "move() takes no arguments"));
            }
            let Expr::Name(object_name) = object.as_ref() else {
                return Err(Diagnostic::new(
                    object.span(),
                    "move() needs a variable declared with defer",
                ));
            };
            if !self.is_defer(&object_name.text) {
                return Err(Diagnostic::new(
                    object_name.span,
                    format!(
                        "'{}' is not a defer variable; .move() only applies to one",
                        object_name.text
                    ),
                ));
            }
            let value = self.expr(object)?;
            return Ok(format!(
                "({} = 1, {value})",
                Body::defer_flag(&object_name.text)
            ));
        }
        // A name answers for itself: it is either the function the compiler
        // registered under it or a variable of its own that C calls indirectly.
        if let Expr::Name(name) = callee
            && self.local(&name.text).is_none()
        {
            if let Some(symbol) = self.backend.function_symbol(&name.text) {
                self.backend.mark_used(&symbol);
                return Ok(format!("{symbol}({})", self.arguments(args)?.join(", ")));
            }
            // A template has one symbol per instance, and the compiler named
            // the one this call reaches.
            if let Some(symbol) = self.backend.call_symbol(span, &name.text) {
                self.backend.mark_used(&symbol);
                return Ok(format!("{symbol}({})", self.arguments(args)?.join(", ")));
            }
            if !self.backend.is_template(&name.text) {
                self.backend.mark_used(&name.text);
                return Ok(format!(
                    "{}({})",
                    name.text,
                    self.arguments(args)?.join(", ")
                ));
            }
        }
        // A method call is named by its receiver's type and the method's name:
        // a macro expansion or a generated fragment gives every call it made one
        // span, so the span cannot say which call this is.
        match callee {
            Expr::Member {
                object, name, op, ..
            } => {
                if let Some(receiver) = self.ty(object) {
                    // A trait value stores function pointers, so a call through
                    // one is already C: no name is mangled.
                    if self.backend.traits.contains_key(&receiver.base) {
                        return Ok(format!(
                            "({}).{}({})",
                            self.expr(object)?,
                            name.text,
                            self.arguments(args)?.join(", ")
                        ));
                    }
                    if let Some(symbol) = self.backend.method(&receiver.base, &name.text) {
                        self.backend.mark_used(&symbol);
                        // A method takes its object by pointer, and C cannot
                        // take the address of a temporary: the call runs in the
                        // statement expression that holds the value.
                        if *op == MemberOp::Dot && receiver.pointers == 0 && !is_lvalue(object) {
                            let value = self.expr(object)?;
                            let mut printed = vec!["&__shy_self".to_string()];
                            printed.extend(self.arguments(args)?);
                            return Ok(format!(
                                "({{ __auto_type __shy_self = {value}; {symbol}({}); }})",
                                printed.join(", ")
                            ));
                        }
                        let mut printed = vec![self.receiver(object, &receiver, *op)?];
                        printed.extend(self.arguments(args)?);
                        return Ok(format!("{symbol}({})", printed.join(", ")));
                    }
                    // A receiver with an impl block cannot have an ordinary
                    // function-pointer member: the method is generic, or it is
                    // a default body that could not be translated.
                    if self.backend.impls.contains_key(&receiver.base) {
                        return Err(Diagnostic::new(
                            span,
                            format!(
                                "'{}::{}' has no C form: it is generic or its trait default is",
                                receiver.base, name.text
                            ),
                        ));
                    }
                }
            }
            // Trait::from<Type>() fills a trait value; Type::method(...) calls
            // a method of the qualifier type.
            Expr::ScopedName {
                qualifier,
                name,
                span,
            } => {
                if !type_args.is_empty() {
                    let GenericArgKind::Type(receiver) = &type_args[0].kind else {
                        return Err(Diagnostic::new(
                            *span,
                            "Trait::from<> needs a type argument",
                        ));
                    };
                    if name.text != "from" {
                        return Err(Diagnostic::new(
                            *span,
                            "a generic call has no C form; name the instance instead",
                        ));
                    }
                    let symbol = names::trait_value(
                        &qualifier.text,
                        &type_name_text_with(receiver, self.spelling()),
                    );
                    self.backend.mark_used(&symbol);
                    return Ok(format!("{symbol}({})", self.arguments(args)?.join(", ")));
                }
                if let Some(value) = self.variant_construction(qualifier, &[], name, args)? {
                    return Ok(value);
                }
                return match self.backend.method(&qualifier.text, &name.text) {
                    Some(symbol) => {
                        self.backend.mark_used(&symbol);
                        Ok(format!("{symbol}({})", self.arguments(args)?.join(", ")))
                    }
                    None => Err(Diagnostic::new(
                        *span,
                        format!(
                            "'{}::{}' is not a method or a variant declared in this unit",
                            qualifier.text, name.text
                        ),
                    )),
                };
            }
            // Trait::from<Type>(): the constructor that fills a trait value.
            Expr::GenericScopedName {
                qualifier,
                args: type_args,
                name,
                span,
            } => {
                if name.text == "from" {
                    let GenericArgKind::Type(receiver) = &type_args[0].kind else {
                        return Err(Diagnostic::new(
                            *span,
                            "Trait::from<> needs a type argument",
                        ));
                    };
                    let symbol = names::trait_value(
                        &qualifier.text,
                        &type_name_text_with(receiver, self.spelling()),
                    );
                    self.backend.mark_used(&symbol);
                    return Ok(format!("{symbol}({})", self.arguments(args)?.join(", ")));
                }
                if let Some(value) = self.variant_construction(qualifier, type_args, name, args)? {
                    return Ok(value);
                }
                // Type<args>::method(...) names one instance's method, and a
                // type parameter names the type it is bound to here.
                let instance = match self.bindings.get(&qualifier.text) {
                    Some(bound) if type_args.is_empty() => bound.clone(),
                    _ => generic_spec_text(
                        &ast::GenericSpec {
                            span: *span,
                            name: qualifier.clone(),
                            args: type_args.clone(),
                        },
                        self.spelling(),
                    ),
                };
                if let Some(symbol) = self.backend.method(&instance, &name.text) {
                    self.backend.mark_used(&symbol);
                    return Ok(format!("{symbol}({})", self.arguments(args)?.join(", ")));
                }
                return Err(Diagnostic::new(
                    *span,
                    format!(
                        "'{}::{}' has no C form; only a variant, Trait::from<Type>(),                          or a method instance does",
                        qualifier.text, name.text
                    ),
                ));
            }
            _ => {}
        }
        if !type_args.is_empty() {
            return Err(Diagnostic::new(
                span,
                "a generic call has no C form; name the instance instead",
            ));
        }
        Ok(format!(
            "{}({})",
            self.expr(callee)?,
            self.arguments(args)?.join(", ")
        ))
    }

    /// Build one payload-enum value with its payload.
    fn variant_construction(
        &mut self,
        qualifier: &Name,
        type_args: &[GenericArg],
        name: &Name,
        args: &[Expr],
    ) -> Result<Option<String>, Diagnostic> {
        let Some((plan, variant)) =
            self.backend
                .variant(qualifier, type_args, self.spelling(), &name.text)
        else {
            return Ok(None);
        };
        let value = self.arguments(args)?.join(", ");
        // A niche value is the payload itself, so building one is a cast.
        if plan.niche {
            return Ok(Some(format!("({})({value})", plan.name)));
        }
        if variant.payload.is_none() {
            return Ok(Some(format!("({}){{ .tag = {} }}", plan.name, variant.tag)));
        }
        Ok(Some(format!(
            "({}){{ .tag = {}, .payload.{} = {value} }}",
            plan.name, variant.tag, name.text
        )))
    }

    fn arguments(&mut self, args: &[Expr]) -> Result<Vec<String>, Diagnostic> {
        args.iter().map(|arg| self.expr(arg)).collect()
    }

    /// Print a method call's receiver the way the mangled function takes it.
    ///
    /// A method receives its object by pointer, so a value gets its address and
    /// a pointer is passed unchanged: ShyC dereferences automatically, which is
    /// why p.add(1) and p->add(1) mean the same thing.
    fn receiver(&mut self, object: &Expr, ty: &Ty, op: MemberOp) -> Result<String, Diagnostic> {
        let text = self.expr(object)?;
        if matches!(op, MemberOp::Arrow) || ty.pointers >= 1 {
            return Ok(format!("({text})"));
        }
        Ok(format!("&({text})"))
    }

    /// Whether a member access needs C's arrow.
    ///
    /// ShyC's dot dereferences a pointer, so p.value with a pointer p is the C
    /// p->value.
    fn arrow(&self, object: &Expr, op: MemberOp) -> bool {
        match op {
            MemberOp::Arrow => true,
            MemberOp::Dot => self.ty(object).is_some_and(|ty| ty.pointers >= 1),
        }
    }

    /// Print value?, the propagation operator.
    ///
    /// C has no expression form for an early return, so the result is a statement
    /// expression that switches on the tag: the failure arm returns the enclosing
    /// function's failure variant, and the success payload is the value of the
    /// expression. The failure variant is spelled as a compound literal, which is
    /// why the enclosing return type travels with the body.
    fn propagate(&mut self, value: &Expr, span: Span) -> Result<String, Diagnostic> {
        let Some(returned) = self.return_ty.clone() else {
            return Err(Diagnostic::new(
                span,
                "the propagation operator needs an enclosing function to return from",
            ));
        };
        let Some(ty) = self.ty(value) else {
            return Err(Diagnostic::new(
                span,
                "the propagation operator needs a known Option or Result type; declare the \
                 value's type",
            ));
        };
        let Some(plan) = self.backend.enum_plan(&ty).cloned() else {
            return Err(Diagnostic::new(
                span,
                format!(
                    "the propagation operator needs an Option or a Result, not '{}'",
                    ty.base
                ),
            ));
        };
        // The payload-free variant is the failure: `None` for an Option and
        // `Ok` for a Result, which is also what tells the two shapes apart.
        let Some(success) = plan
            .variants
            .iter()
            .find(|variant| variant.name == "Some" || variant.name == "Ok")
        else {
            return Err(Diagnostic::new(
                span,
                format!(
                    "the propagation operator needs an Option or a Result, not '{}'",
                    ty.base
                ),
            ));
        };
        let option = success.name == "Some";
        let failure_tag = plan
            .variants
            .iter()
            .find(|variant| variant.name == if option { "None" } else { "Err" })
            .map_or(0, |variant| variant.tag);
        let printed = self.expr(value)?;
        let (failure_condition, failure, payload) = if plan.niche {
            // A niche stores the payload itself: Option is None on zero, and a
            // Result is Err on any nonzero error value.
            if option {
                (
                    "!__try".to_string(),
                    format!("({returned})0"),
                    "__try".to_string(),
                )
            } else {
                (
                    "__try != 0".to_string(),
                    format!("({returned})(__try)"),
                    "((void)0)".to_string(),
                )
            }
        } else {
            (
                format!("__try.tag != {}", success.tag),
                if option {
                    format!("({returned}){{0}}")
                } else {
                    format!(
                        "({returned}){{ .tag = {failure_tag}, .payload.Err = __try.payload.Err }}"
                    )
                },
                success.payload.as_ref().map_or_else(
                    || "((void)0)".to_string(),
                    |_| format!("__try.payload.{}", success.name),
                ),
            )
        };
        Ok(format!(
            "({{ __auto_type __try = {printed}; if ({failure_condition}) {{\
             return {failure}; }} {payload}; }})"
        ))
    }

    /// The type of an expression, when this backend can name it.
    fn ty(&self, expression: &Expr) -> Option<Ty> {
        match expression {
            Expr::Name(name) => self
                .local(&name.text)
                .or_else(|| self.backend.objects.get(&name.text).cloned()),
            Expr::Unary {
                op: UnaryOp::AddressOf,
                operand,
                ..
            } => self.ty(operand).map(|ty| ty.pointer()),
            Expr::Unary {
                op: UnaryOp::Dereference,
                operand,
                ..
            } => self.ty(operand).map(|ty| ty.pointee()),
            Expr::Member { object, name, .. } => {
                let ty = self.ty(object)?;
                if let Some(plan) = self.backend.enum_plan(&ty)
                    && let Some(variant) = plan
                        .variants
                        .iter()
                        .find(|variant| variant.name == name.text)
                {
                    let payload = variant.payload.as_deref().unwrap_or("void");
                    return Some(Ty::from_text(payload));
                }
                match self.backend.enum_plan(&ty) {
                    Some(plan) if name.text == "tag" && plan.niche => {
                        Some(Ty::from_text(&plan.name))
                    }
                    Some(_) if name.text == "tag" => Some(Ty::from_text("int")),
                    _ => self.backend.field(&ty, &name.text),
                }
            }
            Expr::ScopedName {
                qualifier, name, ..
            } => self.variant_ty(qualifier, &[], &name.text),
            Expr::GenericScopedName {
                qualifier,
                args,
                name,
                ..
            } => self.variant_ty(qualifier, args, &name.text),
            // `value?` yields the success payload of the Option or Result.
            Expr::Try { value, .. } => {
                let ty = self.ty(value)?;
                let plan = self.backend.enum_plan(&ty)?;
                let success = plan
                    .variants
                    .iter()
                    .find(|variant| variant.name == "Some" || variant.name == "Ok")?;
                success.payload.as_deref().map(Ty::from_text)
            }
            // A statement expression yields its last expression statement.
            Expr::StatementExpr { block, .. } => match block.stmts.last() {
                Some(Stmt::Expr { expr, .. }) => self.ty(expr),
                _ => None,
            },
            Expr::Call { callee, args, .. } => {
                if args.is_empty()
                    && let Expr::Member { object, name, .. } = callee.as_ref()
                    && name.text == "type"
                {
                    return self.ty(object).map(|_| Ty::from_text("int"));
                }
                self.callee_ty(callee)
            }
            Expr::Cast { type_name, .. } => Some(type_name_of(type_name)),
            Expr::Index { array, .. } => self.ty(array).map(|ty| ty.pointee()),
            // A literal has the type its spelling implies, which is what
            // template inference needs from a call like twice(3).
            Expr::Number(literal) => Some(Ty::from_text(&number_type(&literal.text))),
            Expr::Char(_) => Some(Ty::from_text("char")),
            Expr::String(_) => Some(Ty {
                base: "char".to_string(),
                pointers: 1,
            }),
            _ => None,
        }
    }

    /// The type of one variant used as a value: its logical tag integer.
    fn variant_ty(&self, qualifier: &Name, args: &[GenericArg], name: &str) -> Option<Ty> {
        let (_plan, variant) = self
            .backend
            .variant(qualifier, args, self.spelling(), name)?;
        let _ = variant;
        Some(Ty::from_text("int"))
    }

    fn callee_ty(&self, callee: &Expr) -> Option<Ty> {
        match callee {
            Expr::Name(name) => self.backend.functions.get(&name.text).cloned(),
            Expr::Member { object, name, .. } => {
                let receiver = self.ty(object)?;
                if self.backend.traits.contains_key(&receiver.base) {
                    return self.backend.trait_method_ty(&receiver.base, &name.text);
                }
                self.backend.method_ty(&receiver.base, &name.text)
            }
            Expr::ScopedName {
                qualifier, name, ..
            } => self
                .variant_ty(qualifier, &[], &name.text)
                .or_else(|| self.backend.method_ty(&qualifier.text, &name.text)),
            Expr::GenericScopedName {
                qualifier,
                args,
                name,
                ..
            } => {
                if name.text == "from" {
                    Some(Ty {
                        base: qualifier.text.clone(),
                        pointers: 0,
                    })
                } else {
                    self.variant_ty(qualifier, args, &name.text)
                }
            }
            _ => None,
        }
    }
}

/// The C type a numeric literal's spelling implies.
fn number_type(text: &str) -> String {
    let lower = text.to_ascii_lowercase();
    let mut ty = String::new();
    if lower.contains('.') || lower.contains('e') && !lower.starts_with("0x") {
        ty.push_str("double");
    } else {
        ty.push_str("int");
    }
    if lower.ends_with('l') || lower.ends_with("ll") {
        ty.push_str(" long");
    }
    if lower.contains('u') {
        ty.insert_str(0, "unsigned ");
    }
    ty
}

fn type_name_of(name: &TypeName) -> Ty {
    Ty::from_text(&type_name_text(name))
}

fn unary_op_text(op: UnaryOp) -> &'static str {
    match op {
        UnaryOp::Plus => "+",
        UnaryOp::Minus => "-",
        UnaryOp::LogicalNot => "!",
        UnaryOp::BitNot => "~",
        UnaryOp::Dereference => "*",
        UnaryOp::AddressOf => "&",
        UnaryOp::PreIncrement => "++",
        UnaryOp::PreDecrement => "--",
    }
}

fn postfix_op_text(op: PostfixOp) -> &'static str {
    match op {
        PostfixOp::Increment => "++",
        PostfixOp::Decrement => "--",
    }
}

fn binary_op_text(op: BinaryOp) -> &'static str {
    match op {
        BinaryOp::Multiply => "*",
        BinaryOp::Divide => "/",
        BinaryOp::Remainder => "%",
        BinaryOp::Add => "+",
        BinaryOp::Subtract => "-",
        BinaryOp::ShiftLeft => "<<",
        BinaryOp::ShiftRight => ">>",
        BinaryOp::Less => "<",
        BinaryOp::LessEqual => "<=",
        BinaryOp::Greater => ">",
        BinaryOp::GreaterEqual => ">=",
        BinaryOp::Equal => "==",
        BinaryOp::NotEqual => "!=",
        BinaryOp::BitAnd => "&",
        BinaryOp::BitXor => "^",
        BinaryOp::BitOr => "|",
        BinaryOp::LogicalAnd => "&&",
        BinaryOp::LogicalOr => "||",
    }
}

fn assign_op_text(op: AssignOp) -> &'static str {
    match op {
        AssignOp::Assign => "=",
        AssignOp::Add => "+=",
        AssignOp::Subtract => "-=",
        AssignOp::Multiply => "*=",
        AssignOp::Divide => "/=",
        AssignOp::Remainder => "%=",
        AssignOp::ShiftLeft => "<<=",
        AssignOp::ShiftRight => ">>=",
        AssignOp::BitAnd => "&=",
        AssignOp::BitXor => "^=",
        AssignOp::BitOr => "|=",
    }
}

/// The defer variables every goto in one body leaves, keyed by the goto
/// statement's span and label and ordered innermost scope and last declaration
/// first.
///
/// C labels are function-scoped, so a goto alone does not say which blocks it
/// leaves; one walk with the same block nesting the renderer uses answers it.
fn collect_goto_drops(block: &Block) -> HashMap<(Span, String), Vec<String>> {
    struct Walk {
        next_scope: u32,
        labels: HashMap<String, Vec<(u32, Vec<String>)>>,
        gotos: Vec<((Span, String), Vec<(u32, Vec<String>)>)>,
    }

    fn declare(declaration: &Declaration, scopes: &mut [(u32, Vec<String>)]) {
        let Some(scope) = scopes.last_mut() else {
            return;
        };
        for declarator in &declaration.declarators {
            if let Some(name) = &declarator.declarator.name {
                scope.1.push(name.text.clone());
            }
        }
    }

    fn walk_block(block: &Block, walk: &mut Walk, scopes: &mut Vec<(u32, Vec<String>)>) {
        let id = walk.next_scope;
        walk.next_scope += 1;
        scopes.push((id, Vec::new()));
        for statement in &block.stmts {
            walk_statement(statement, walk, scopes);
        }
        scopes.pop();
    }

    fn walk_statement(statement: &Stmt, walk: &mut Walk, scopes: &mut Vec<(u32, Vec<String>)>) {
        match statement {
            Stmt::Decl(declaration) if declaration.is_defer => declare(declaration, scopes),
            Stmt::Let(declaration) if declaration.is_defer => {
                if let Some(scope) = scopes.last_mut() {
                    scope.1.push(declaration.name.text.clone());
                }
            }
            Stmt::Decl(_) | Stmt::Let(_) => {}
            Stmt::Block(block) => walk_block(block, walk, scopes),
            Stmt::If {
                then_branch,
                else_branch,
                ..
            } => {
                walk_statement(then_branch, walk, scopes);
                if let Some(else_branch) = else_branch {
                    walk_statement(else_branch, walk, scopes);
                }
            }
            Stmt::While { body, .. } | Stmt::DoWhile { body, .. } | Stmt::Switch { body, .. } => {
                walk_statement(body, walk, scopes)
            }
            Stmt::For { body, .. } => walk_statement(body, walk, scopes),
            Stmt::Case { stmt, .. }
            | Stmt::Default { stmt, .. }
            | Stmt::AllowDeferCopy { stmt, .. } => walk_statement(stmt, walk, scopes),
            Stmt::Label { name, stmt, .. } => {
                walk.labels.insert(name.text.clone(), scopes.clone());
                walk_statement(stmt, walk, scopes);
            }
            Stmt::Goto { label, span, .. } => {
                walk.gotos
                    .push(((*span, label.text.clone()), scopes.clone()));
            }
            Stmt::Empty(_)
            | Stmt::Expr { .. }
            | Stmt::Break(_)
            | Stmt::Continue(_)
            | Stmt::Return { .. }
            | Stmt::Asm(_) => {}
        }
    }

    let mut walk = Walk {
        next_scope: 0,
        labels: HashMap::new(),
        gotos: Vec::new(),
    };
    walk_block(block, &mut walk, &mut Vec::new());
    let Walk { labels, gotos, .. } = walk;
    let mut drops = HashMap::new();
    for (key, path) in gotos {
        let Some(label) = labels.get(&key.1) else {
            continue;
        };
        let mut common = 0;
        while common < path.len() && common < label.len() && path[common].0 == label[common].0 {
            common += 1;
        }
        let names: Vec<String> = path[common..]
            .iter()
            .rev()
            .flat_map(|(_, scope)| scope.iter().rev().cloned())
            .collect();
        drops.insert(key, names);
    }
    drops
}
