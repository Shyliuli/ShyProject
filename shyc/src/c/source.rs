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
//! - `asm!` becomes a GCC extended asm statement.
//!
//! Everything else is ordinary C and prints as written. A generic function or
//! record prints nothing: C has no template, and an instance of one is a
//! separate symbol with its own name (`Vec_int`).

use std::collections::HashMap;

use crate::ast::{
    self, AsmStmt, AssignOp, BinaryOp, Block, DeclSpecs, Declaration, Declarator,
    DerivedDeclarator, Expr, ForInit, FunctionDecl, GenericArg, GenericArgKind, ImplBlock,
    Initializer, InitializerKind, Item, MemberOp, Name, PostfixOp, RecordSpec, Stmt, TraitDecl,
    TranslationUnit, TypeName, TypeSpecifier, UnaryOp,
};
use crate::c::names;
use crate::c::types::{
    Mention, Spelling, declarator_text_in, record_instance_text, specs_text_in,
    trait_value_possible, trait_value_text, type_name_text,
};
use crate::lower::{InstancePlan, Lowered};
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
    /// The enumerators of every enum with a definition, by base name.
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
    /// The impl blocks of a generic record, by the record's name.
    generic_impls: HashMap<String, Vec<&'a ImplBlock>>,
    /// Generic uses seen while printing: the work list of specializations.
    mentions: std::cell::RefCell<Vec<Mention>>,
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
}

impl Instance {
    /// The C name of this instance, from the compiler's plan.
    fn c_name(&self, backend: &Backend<'_>) -> String {
        if let Some(plan) = backend.plan(&self.name, &self.args) {
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
            generic_impls: HashMap::new(),
            mentions: std::cell::RefCell::new(Vec::new()),
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
        // list is its plan rather than a search of the source.
        for plan in &lowered.instances {
            if plan.layout.is_some() || (plan.receiver.is_none() && plan.symbol.is_some()) {
                backend.instances.borrow_mut().push(Instance {
                    name: plan.name.clone(),
                    args: plan.args.clone(),
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
            mentions: Some(&self.mentions),
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

    fn enumeration(&mut self, enumeration: &ast::EnumSpec) {
        let (Some(tag), Some(enumerators)) = (&enumeration.tag, &enumeration.enumerators) else {
            return;
        };
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
                let symbol = self.symbol(&receiver, &name.text, method.span);
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
    fn field(&self, ty: &Ty, name: &str) -> Option<Ty> {
        self.records
            .get(&ty.base)?
            .iter()
            .find(|(field, _)| field == name)
            .map(|(_, ty)| ty.clone())
    }

    /// The C symbol of one method of one receiver, when an impl block defines
    /// it or inherits a default body for it.
    ///
    /// An inherent method wins over a trait method of the same name, which is
    /// how a call with one receiver and no trait qualifier resolves.
    fn method(&self, receiver: &str, name: &str) -> Option<String> {
        let blocks = self.impls.get(receiver)?;
        let defines = |block: &ImplBlock| {
            find_method(block, name).is_some()
                || find_prototype(block, name).is_some()
                || self.inherits(block, name)
        };
        for block in blocks.iter().filter(|block| block.trait_name.is_none()) {
            if defines(block) {
                return Some(names::method(receiver, name));
            }
        }
        for block in blocks.iter().filter(|block| block.trait_name.is_some()) {
            if defines(block) {
                let trait_name = block.trait_name.as_ref()?;
                return Some(names::trait_method(receiver, &trait_name.text, name));
            }
        }
        None
    }

    /// The C symbol of one method: the compiler's answer, whether the method is
    /// a specialization or an ordinary definition.
    fn symbol(&self, receiver: &str, name: &str, span: Span) -> String {
        if let Some(symbol) = self
            .method_plan(receiver, name)
            .and_then(|plan| plan.symbol.clone())
        {
            return names::symbol(&symbol);
        }
        if let Some(symbol) = self.lowered.declaration_symbols.get(&span) {
            return names::symbol(symbol);
        }
        // Neither: a method the compiler never lowered, which only happens for a
        // construct it rejected, so any name will do to report it in C.
        names::method(receiver, name)
    }

    /// The C name of one function definition: the compiler's symbol for it.
    fn definition(&self, span: Span, fallback: &str) -> String {
        match self.lowered.declaration_symbols.get(&span) {
            Some(symbol) => names::symbol(symbol),
            None => fallback.to_string(),
        }
    }

    /// The compiler's plan for one method of one concrete receiver.
    fn method_plan(&self, receiver: &str, name: &str) -> Option<&'a InstancePlan> {
        self.lowered.instances.iter().find(|plan| {
            plan.name == name
                && (plan.impl_name.as_deref() == Some(receiver)
                    || plan.receiver.as_deref() == Some(receiver))
        })
    }

    /// Whether a block inherits a translatable default body for one method.
    ///
    /// A generic default is static-only, like a generic method, so it has no
    /// symbol a C call could reach.
    fn inherits(&self, block: &ImplBlock, name: &str) -> bool {
        let Some(trait_name) = &block.trait_name else {
            return false;
        };
        let Some(declaration) = self.traits.get(&trait_name.text) else {
            return false;
        };
        declaration.methods.iter().any(|method| {
            method
                .declarator
                .name
                .as_ref()
                .is_some_and(|method_name| method_name.text == name)
                && method.body.is_some()
                && !is_template(&method.declarator)
        })
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

    /// The C type and tag value of one payload-enum variant.
    fn variant(
        &self,
        qualifier: &Name,
        args: &[GenericArg],
        name: &str,
    ) -> Option<(String, usize)> {
        let enumerators = self.enums.get(&qualifier.text)?;
        let tag = enumerators
            .iter()
            .position(|enumerator| enumerator == name)?;
        let mut parts = Vec::new();
        for arg in args {
            let GenericArgKind::Type(type_name) = &arg.kind else {
                return None;
            };
            parts.push(type_name_text(type_name));
        }
        Some((names::instance(&qualifier.text, &parts), tag))
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
        // A specialization is printed by the same code as the unit, and a
        // specialization can need another one, so the instance list is
        // discovered on a discarded run and then printed for real.
        let mut scratch = String::new();
        self.emit(unit, &mut scratch)?;
        let mut output = String::new();
        self.emit(unit, &mut output)?;
        Ok(output)
    }

    /// Print the unit, its declarations, and every specialization it needs.
    fn emit(&self, unit: &TranslationUnit, output: &mut String) -> Result<(), Diagnostic> {
        output.push_str("/* Generated by shyc --emit=c. */\n\n");
        // A record's tag is also its type name in ShyC, so every C record gets
        // the typedef a caller needs before anything mentions it; the definition
        // itself stays in place below. This comes first because a trait method
        // signature may name a record.
        let mut records = String::new();
        for item in &unit.items {
            if let Item::Decl(declaration) = item {
                if let Some(text) = forward_record(&declaration.specs) {
                    records.push_str(&text);
                    records.push('\n');
                }
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
        // The generic instances the unit needs: a specialization is a record
        // definition, a function, or one method per impl block.
        self.drain();
        let instances = self.instance_declarations()?;
        if !instances.is_empty() {
            output.push_str(&instances);
            output.push('\n');
        }
        // Methods and trait constructors are defined wherever their impl block
        // appears, but a body before it calls them, so they are declared first.
        let mut prototypes = String::new();
        for item in &unit.items {
            if let Item::Impl(block) = item {
                prototypes.push_str(&self.impl_prototypes(block)?);
            }
        }
        if !prototypes.is_empty() {
            output.push_str(&prototypes);
            output.push('\n');
        }
        for item in &unit.items {
            let text = match item {
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
            self.drain();
        }
        self.instantiate(output)
    }

    /// Print one file-scope declaration as a definition, initializer included.
    fn item_declaration(&self, declaration: &Declaration) -> Result<String, Diagnostic> {
        if is_template_declaration(declaration) {
            return Ok(String::new());
        }
        let spelling = self.spelling("void");
        let typedef = declaration.specs.storage == Some(ast::StorageClass::Typedef);
        let prefix = if typedef { "typedef " } else { "" };
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
            let mut text = declarator_text_in(&declarator.declarator, spelling);
            if let Some(initializer) = &declarator.init {
                text.push_str(" = ");
                text.push_str(&globals.initializer(initializer)?);
            }
            declarators.push(text);
        }
        Ok(format!("{specs} {};", declarators.join(", ")))
    }

    fn function(&self, function: &FunctionDecl) -> Result<String, Diagnostic> {
        // A generic function is a template: C has no spelling for it, and each
        // instance is a symbol of its own, named by the instance table.
        if is_template(&function.declarator) {
            return Ok(String::new());
        }
        let spelling = self.spelling("void");
        // The definition carries the symbol the compiler gave it, which is what
        // a call site prints.
        let declarator = match &function.declarator.name {
            Some(name) => renamed(
                &function.declarator,
                &self.definition(function.span, &name.text),
            ),
            None => function.declarator.clone(),
        };
        let signature = format!(
            "{} {}",
            specs_text_in(&function.specs, spelling),
            declarator_text_in(&declarator, spelling)
        );
        let mut body = Body::new(
            self,
            Some(self.return_type_text(&function.specs, &function.declarator)),
        );
        body.parameters(&function.declarator);
        Ok(format!("{signature} {}", body.block(&function.body, 0)?))
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
            let symbol = self.symbol(&receiver, &name.text, method.span);
            let declarator = renamed(method.declarator, &symbol);
            let linkage = if method.specs.is_pub { "" } else { "static " };
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
            let symbol = self.symbol(&receiver, &name.text, prototype.span);
            let declarator = renamed(&prototype.declarator, &symbol);
            let linkage = if prototype.specs.is_pub {
                ""
            } else {
                "static "
            };
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
            let name = &trait_name.text;
            output.push_str(&format!(
                "static {name} {symbol}(void);
"
            ));
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
            let symbol = self.symbol(&receiver, &name.text, method.span);
            let declarator = renamed(method.declarator, &symbol);
            let linkage = if method.specs.is_pub { "" } else { "static " };
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
            output.push_str(&format!("{signature} {}", body.block(method.body, 0)?));
            output.push_str("\n\n");
        }
        if let Some(trait_name) = &block.trait_name
            && self.trait_value(trait_name)
        {
            output.push_str(&self.trait_constructor(block, &receiver, &trait_name.text)?);
        }
        Ok(output)
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
                Some(implementation) => self.symbol(receiver, &name.text, implementation.span),
                None => {
                    return Err(Diagnostic::new(
                        method.span,
                        format!(
                            "'impl {trait_name} for {receiver}' does not define '{}'",
                            name.text
                        ),
                    ));
                }
            };
            output.push_str(&format!(
                "    value.{} = (typeof(value.{})){implementation};\n",
                name.text, name.text
            ));
        }
        output.push_str("    return value;\n}\n\n");
        Ok(output)
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
    /// Queue the generic uses the printers reported.
    fn drain(&self) {
        let mentions = std::mem::take(&mut *self.mentions.borrow_mut());
        for mention in mentions {
            self.enqueue(Instance {
                name: mention.name,
                args: mention.args,
            });
        }
    }

    /// Queue one specialization, once.
    fn enqueue(&self, instance: Instance) {
        if !self.is_template(&instance.name) {
            return;
        }
        let mut instances = self.instances.borrow_mut();
        if !instances.contains(&instance) {
            instances.push(instance);
        }
    }

    fn is_template(&self, name: &str) -> bool {
        self.templates.contains_key(name) || self.generic_records.contains_key(name)
    }

    /// The compiler's plan for one specialization.
    fn plan(&self, name: &str, args: &[String]) -> Option<&'a InstancePlan> {
        self.lowered
            .instances
            .iter()
            .find(|plan| plan.name == name && plan.args == args)
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
            mentions: Some(&self.mentions),
        }
    }

    /// The declarations a specialization contributes: record definitions, and
    /// one prototype per function and method.
    fn instance_declarations(&self) -> Result<String, Diagnostic> {
        let mut output = String::new();
        for instance in self.instances.borrow().clone() {
            let bindings = self.bindings(&instance);
            let name = instance.c_name(self);
            if let Some(record) = self.generic_records.get(&instance.name) {
                let spelling = self.instance_spelling("void", &bindings);
                // The layout belongs to the compiler: it printed the header's
                // types, so it prints this instance too.
                match self
                    .plan(&instance.name, &instance.args)
                    .and_then(|plan| plan.layout.clone())
                {
                    Some(layout) => output.push_str(&layout),
                    None => output.push_str(&record_instance_text(record, &name, spelling)),
                }
                output.push('\n');
                output.push_str(&self.method_prototypes_of(&instance, &name));
            } else if let Some(function) = self.templates.get(&instance.name) {
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

    /// The prototypes of the methods a generic impl block gives one instance.
    fn method_prototypes_of(&self, instance: &Instance, name: &str) -> String {
        let mut output = String::new();
        for block in self.generic_impls.get(&instance.name).into_iter().flatten() {
            let bindings = self.block_bindings(block, instance);
            let spelling = self.instance_spelling(name, &bindings);
            for method in self.methods(block) {
                let Some(method_name) = &method.declarator.name else {
                    continue;
                };
                let symbol = self.symbol(name, &method_name.text, method.span);
                let declarator = renamed(method.declarator, &symbol);
                output.push_str(&format!(
                    "static {} {};\n",
                    specs_text_in(method.specs, spelling),
                    declarator_text_in(&declarator, spelling)
                ));
            }
        }
        output
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
            if let Some(function) = self.templates.get(&instance.name) {
                let bindings = self.bindings(&instance);
                let spelling = self.instance_spelling(&name, &bindings);
                let declarator = renamed(&function.declarator, &name);
                let mut body = Body::instance(
                    self,
                    Some(return_type_in(
                        &function.specs,
                        &function.declarator,
                        spelling,
                    )),
                    name.clone(),
                    bindings.clone(),
                );
                body.parameters(&function.declarator);
                output.push_str(&format!(
                    "static {} {} {}\n\n",
                    specs_text_in(&function.specs, spelling),
                    declarator_text_in(&declarator, spelling),
                    body.block(&function.body, 0)?
                ));
            }
            if let Some(blocks) = self.generic_impls.get(&instance.name).cloned() {
                for block in blocks {
                    let bindings = self.block_bindings(block, &instance);
                    let spelling = self.instance_spelling(&name, &bindings);
                    for method in self.methods(block) {
                        let Some(method_name) = &method.declarator.name else {
                            continue;
                        };
                        let symbol = self.symbol(&name, &method_name.text, method.span);
                        let declarator = renamed(method.declarator, &symbol);
                        let mut body = Body::instance(
                            self,
                            Some(return_type_in(method.specs, method.declarator, spelling)),
                            name.clone(),
                            bindings.clone(),
                        );
                        body.parameters(method.declarator);
                        output.push_str(&format!(
                            "static {} {} {}\n\n",
                            specs_text_in(method.specs, spelling),
                            declarator_text_in(&declarator, spelling),
                            body.block(method.body, 0)?
                        ));
                    }
                }
            }
            self.drain();
        }
        Ok(())
    }

    /// The bindings of one generic impl block for one instance.
    fn block_bindings(&self, block: &ImplBlock, instance: &Instance) -> HashMap<String, String> {
        let parameters = if block.generics.is_empty() {
            self.parameters(&instance.name)
        } else {
            block
                .generics
                .iter()
                .map(|param| param.name.text.clone())
                .collect()
        };
        parameters
            .into_iter()
            .zip(instance.args.iter().cloned())
            .collect()
    }
}

impl<'a> Backend<'a> {
    /// Bind a template's type parameters from the argument types of one call.
    ///
    /// A parameter that is exactly a type parameter, under any number of
    /// pointers, binds to the argument type with the same pointers. That is the
    /// shape these templates take: a value, a pointer to it, or a buffer the
    /// caller passes by pointer.
    fn infer(&self, function: &FunctionDecl, actuals: &[Option<Ty>]) -> Option<Instance> {
        let position = function_position(&function.declarator)?;
        let DerivedDeclarator::Function(call) = function.declarator.derived.get(position)? else {
            return None;
        };
        let parameters = generic_parameters(&function.declarator);
        let mut bindings = HashMap::new();
        for (index, parameter) in call.params.iter().enumerate() {
            let Some(actual) = actuals.get(index).and_then(|actual| actual.as_ref()) else {
                continue;
            };
            let mut declarator = parameter.declarator.clone().unwrap_or(Declarator {
                span: parameter.span,
                name: None,
                derived: Vec::new(),
            });
            declarator.name = None;
            let text = format!(
                "{} {}",
                specs_text_in(&parameter.specs, Spelling::plain()),
                declarator_text_in(&declarator, Spelling::plain())
            );
            let (base, pointers) = names::split(&text);
            if !parameters.contains(&base) || actual.pointers < pointers {
                continue;
            }
            let mut bound = actual.clone();
            for _ in 0..pointers {
                bound = bound.pointee();
            }
            bindings.insert(base, ty_text(&bound));
        }
        let args = parameters
            .iter()
            .map(|parameter| bindings.get(parameter).cloned())
            .collect::<Option<Vec<_>>>()?;
        Some(Instance {
            name: function.declarator.name.as_ref()?.text.clone(),
            args,
        })
    }
}

/// One resolved type as C spells it.
fn ty_text(ty: &Ty) -> String {
    let mut text = ty.base.clone();
    for _ in 0..ty.pointers {
        text.push_str(" *");
    }
    text
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
        }
    }

    /// How this body spells types.
    fn spelling(&self) -> Spelling<'_> {
        Spelling {
            self_name: &self.self_name,
            instances: Some(&self.backend.type_names),
            bindings: Some(&self.bindings),
            generics: !self.instance,
            mentions: Some(&self.backend.mentions),
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

    fn block(&mut self, block: &Block, depth: usize) -> Result<String, Diagnostic> {
        self.scopes.push(HashMap::new());
        let mut output = String::from("{\n");
        for statement in &block.stmts {
            output.push_str(&self.statement(statement, depth + 1)?);
        }
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
                    self.statement(then_branch, depth)?.trim_start()
                ));
                if let Some(else_branch) = else_branch {
                    output.pop();
                    output.push_str(" else ");
                    output.push_str(self.statement(else_branch, depth)?.trim_start());
                }
                output.push('\n');
            }
            Stmt::While {
                condition, body, ..
            } => {
                output.push_str(&format!(
                    "{indent}while ({}) {}",
                    self.expr(condition)?,
                    self.statement(body, depth)?.trim_start()
                ));
                output.push('\n');
            }
            Stmt::DoWhile {
                body, condition, ..
            } => {
                output.push_str(&format!(
                    "{indent}do {} ",
                    self.statement(body, depth)?.trim_start()
                ));
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
                output.push_str(&format!(
                    "{indent}for ({init_text}; {condition_text}; {step_text}) {}",
                    self.statement(body, depth)?.trim_start()
                ));
                output.push('\n');
            }
            Stmt::Switch {
                condition, body, ..
            } => {
                output.push_str(&format!(
                    "{indent}switch ({}) {}",
                    self.expr(condition)?,
                    self.statement(body, depth)?.trim_start()
                ));
                output.push('\n');
            }
            Stmt::Case {
                value, end, stmt, ..
            } => {
                output.push_str(&format!("{indent}case {}", self.expr(value)?));
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
            Stmt::Goto { label, .. } => {
                output.push_str(&format!("{indent}goto {};\n", label.text));
            }
            Stmt::Break(_) => output.push_str(&format!("{indent}break;\n")),
            Stmt::Continue(_) => output.push_str(&format!("{indent}continue;\n")),
            Stmt::Return { value, .. } => match value {
                Some(value) => output.push_str(&format!("{indent}return {};\n", self.expr(value)?)),
                None => output.push_str(&format!("{indent}return;\n")),
            },
            Stmt::Asm(asm) => {
                output.push_str(&format!("{indent}{};\n", asm_text(asm)?));
            }
        }
        Ok(output)
    }

    fn declaration(&mut self, declaration: &Declaration) -> Result<String, Diagnostic> {
        let specs = specs_text_in(&declaration.specs, self.spelling());
        let mut declarators = Vec::new();
        for declarator in &declaration.declarators {
            let mut text = declarator_text_in(&declarator.declarator, self.spelling());
            if let Some(initializer) = &declarator.init {
                text.push_str(" = ");
                text.push_str(&self.initializer(initializer)?);
            }
            if let Some(name) = &declarator.declarator.name {
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
            Expr::Name(name) => Ok(name.text.clone()),
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
            } => {
                let arrow = self.arrow(object, *op);
                Ok(format!(
                    "({}){}{}",
                    self.expr(object)?,
                    if arrow { "->" } else { "." },
                    name.text
                ))
            }
            Expr::Cast {
                type_name, value, ..
            } => Ok(format!(
                "(({}){})",
                type_name_text(type_name),
                self.expr(value)?
            )),
            Expr::SizeofType { type_name, .. } => {
                Ok(format!("sizeof({})", type_name_text(type_name)))
            }
            Expr::SizeofExpr { value, .. } => Ok(format!("sizeof({})", self.expr(value)?)),
            Expr::AlignofType { type_name, .. } => {
                Ok(format!("_Alignof({})", type_name_text(type_name)))
            }
            Expr::AlignofExpr { value, .. } => Ok(format!("_Alignof({})", self.expr(value)?)),
            Expr::StatementExpr { block, .. } => Ok(format!("({{ {} }})", self.block(block, 0)?)),
            Expr::Try { value, span } => self.propagate(value, *span),
            // A variant without a payload is a value: Option<String>::None.
            Expr::ScopedName {
                qualifier,
                name,
                span,
            } => match self.backend.variant(qualifier, &[], &name.text) {
                Some((ty, tag)) => Ok(format!("({ty}){{ .tag = {tag} }}")),
                None => Err(Diagnostic::new(
                    *span,
                    format!(
                        "'{}::{}' has no C form as a value",
                        qualifier.text, name.text
                    ),
                )),
            },
            Expr::GenericScopedName {
                qualifier,
                args: type_args,
                name,
                span,
            } => match self.backend.variant(qualifier, type_args, &name.text) {
                Some((ty, tag)) => Ok(format!("({ty}){{ .tag = {tag} }}")),
                None => Err(Diagnostic::new(
                    *span,
                    format!(
                        "'{}::{}' has no C form as a value",
                        qualifier.text, name.text
                    ),
                )),
            },
            other => Err(Diagnostic::new(
                other.span(),
                "this expression has no C form yet",
            )),
        }
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
        // The compiler resolved this call, so the C output prints the symbol it
        // chose. A method call passes its receiver first, which the source
        // arguments do not contain.
        if let Some(symbol) = self.backend.lowered.call_symbols.get(&span).cloned() {
            let mut printed = Vec::new();
            if let Expr::Member { object, op, .. } = callee {
                let receiver = match self.ty(object) {
                    Some(receiver) => self.receiver(object, &receiver, *op)?,
                    None => self.expr(object)?,
                };
                printed.push(receiver);
            }
            printed.extend(self.arguments(args)?);
            return Ok(format!(
                "{}({})",
                names::symbol(&symbol),
                printed.join(", ")
            ));
        }
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
                    let symbol = names::trait_value(&qualifier.text, &type_name_text(receiver));
                    return Ok(format!("{symbol}({})", self.arguments(args)?.join(", ")));
                }
                if let Some((ty, tag)) = self.backend.variant(qualifier, &[], &name.text) {
                    let value = self.arguments(args)?.join(", ");
                    return Ok(format!(
                        "({ty}){{ .tag = {tag}, .payload.{} = {value} }}",
                        name.text
                    ));
                }
                return match self.backend.method(&qualifier.text, &name.text) {
                    Some(symbol) => Ok(format!("{symbol}({})", self.arguments(args)?.join(", "))),
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
                    let symbol = names::trait_value(&qualifier.text, &type_name_text(receiver));
                    return Ok(format!("{symbol}({})", self.arguments(args)?.join(", ")));
                }
                if let Some((ty, tag)) = self.backend.variant(qualifier, type_args, &name.text) {
                    let value = self.arguments(args)?.join(", ");
                    return Ok(format!(
                        "({ty}){{ .tag = {tag}, .payload.{} = {value} }}",
                        name.text
                    ));
                }
                return Err(Diagnostic::new(
                    *span,
                    format!(
                        "'{}::{}' has no C form; only a variant or Trait::from<Type>() does",
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
        if let Expr::Name(name) = callee
            && let Some(function) = self.backend.templates.get(&name.text).copied()
        {
            let actuals = args.iter().map(|arg| self.ty(arg)).collect::<Vec<_>>();
            let Some(instance) = self.backend.infer(function, &actuals) else {
                return Err(Diagnostic::new(
                    span,
                    format!(
                        "cannot infer the type arguments of '{}'; name the instance instead",
                        name.text
                    ),
                ));
            };
            let symbol = instance.c_name(self.backend);
            self.backend.enqueue(instance);
            return Ok(format!("{symbol}({})", self.arguments(args)?.join(", ")));
        }
        Ok(format!(
            "{}({})",
            self.expr(callee)?,
            self.arguments(args)?.join(", ")
        ))
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
            Ok(format!("({text})"))
        } else {
            Ok(format!("&({text})"))
        }
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
        let printed = self.expr(value)?;
        if ty.base.starts_with("Option") {
            return Ok(format!(
                "({{ __auto_type __try = {printed}; switch (__try.tag) {{ case 1: break; \
                 default: return ({returned}){{0}}; }} __try.payload.Some; }})"
            ));
        }
        if ty.base.starts_with("Result") {
            return Ok(format!(
                "({{ __auto_type __try = {printed}; switch (__try.tag) {{ case 0: break; \
                 default: return ({returned}){{ .tag = 1, .payload.Err = __try.payload.Err }}; }} \
                 __try.payload.Ok; }})"
            ));
        }
        Err(Diagnostic::new(
            span,
            format!(
                "the propagation operator needs an Option or a Result, not '{}'",
                ty.base
            ),
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
            Expr::Member { object, name, .. } => self.backend.field(&self.ty(object)?, &name.text),
            Expr::Call { callee, .. } => self.callee_ty(callee),
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
            } => self.backend.method_ty(&qualifier.text, &name.text),
            Expr::GenericScopedName {
                qualifier, name, ..
            } => {
                if name.text == "from" {
                    Some(Ty {
                        base: qualifier.text.clone(),
                        pointers: 0,
                    })
                } else {
                    None
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
