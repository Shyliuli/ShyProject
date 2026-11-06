//! A lightweight symbol index built from the compiler's syntax tree.
//!
//! The compiler already parses names and keeps a [`Span`] on every one. This
//! module walks that tree once to collect
//!
//! - definitions (types, traits, functions, methods, fields, enumerators,
//!   globals, parameters, and locals), and
//! - references that can be matched to those definitions by lexical scope.
//!
//! It deliberately does not re-run type checking. The editor only needs the
//! spelling of a type for member completion and the identity of a declaration
//! for go-to-definition, so a textual scope walk is enough.

use std::collections::{HashMap, HashSet};

use shyc::ast::{self, Name};
use shyc::source::{FileId, SourceMap, Span};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SymKind {
    Struct,
    Union,
    Enum,
    Trait,
    TypeAlias,
    Function,
    Method,
    Field,
    Variant,
    Global,
    Local,
    Param,
    TypeParam,
    ConstParam,
    Label,
    /// A const fn declaration, a call to one, or a println!/format!/scanln!.
    Comptime,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Scope {
    pub file: FileId,
    pub start: usize,
    pub end: usize,
}

#[derive(Clone, Debug)]
pub struct Sym {
    pub name: String,
    pub kind: SymKind,
    pub span: Span,
    /// Human-readable declaration, already formatted for hover and completion.
    pub detail: String,
    /// The declared type, if this symbol has one. Used to resolve members.
    pub type_text: Option<String>,
    /// The `impl` type or trait that owns a method, or the record/enum that
    /// owns a field or enumerator.
    pub container: Option<String>,
    /// Visibility range for parameters, locals, and generic parameters.
    pub scope: Option<Scope>,
}

#[derive(Clone, Copy, Debug)]
pub struct Reference {
    pub span: Span,
    pub target: usize,
}

#[derive(Default)]
pub struct Index {
    pub symbols: Vec<Sym>,
    pub by_name: HashMap<String, Vec<usize>>,
    pub refs: Vec<Reference>,
    ref_spans: HashSet<(FileId, usize, usize)>,
}

impl Index {
    pub fn push(&mut self, symbol: Sym) -> usize {
        let id = self.symbols.len();
        self.by_name
            .entry(symbol.name.clone())
            .or_default()
            .push(id);
        self.symbols.push(symbol);
        id
    }

    pub fn add_ref(&mut self, span: Span, target: usize) {
        if !self.ref_spans.insert((span.file, span.start, span.end)) {
            return;
        }
        self.refs.push(Reference { span, target });
    }

    pub fn lookup(&self, name: &str) -> &[usize] {
        self.by_name.get(name).map(Vec::as_slice).unwrap_or(&[])
    }

    pub fn symbol(&self, id: usize) -> &Sym {
        &self.symbols[id]
    }

    pub fn definition_at(&self, file: FileId, offset: usize) -> Option<usize> {
        if let Some(reference) = self.refs.iter().find(|reference| {
            reference.span.file == file
                && reference.span.start <= offset
                && offset <= reference.span.end
        }) {
            return Some(reference.target);
        }
        self.symbols.iter().position(|symbol| {
            symbol.span.file == file && symbol.span.start <= offset && offset <= symbol.span.end
        })
    }

    /// Parameters, locals, and generic parameters whose scope contains `offset`.
    pub fn visible_symbols(&self, file: FileId, offset: usize) -> Vec<usize> {
        self.symbols
            .iter()
            .enumerate()
            .filter(|(_, symbol)| {
                matches!(
                    symbol.kind,
                    SymKind::Param | SymKind::Local | SymKind::TypeParam | SymKind::ConstParam
                ) && symbol.span.start <= offset
                    && symbol.scope.is_some_and(|scope| {
                        scope.file == file && scope.start <= offset && offset <= scope.end
                    })
            })
            .map(|(id, _)| id)
            .collect()
    }

    pub fn in_container(&self, container: &str, prefix: &str) -> Vec<usize> {
        self.symbols
            .iter()
            .enumerate()
            .filter(|(_, symbol)| {
                symbol.name.starts_with(prefix)
                    && symbol
                        .container
                        .as_deref()
                        .is_some_and(|owner| owner == container)
            })
            .map(|(id, _)| id)
            .collect()
    }
}

pub fn build(
    sources: &SourceMap,
    unit: &ast::TranslationUnit,
    sites: &[shyc::parser::ComptimeSite],
    generated: &[Span],
) -> Index {
    let mut collector = Collector {
        sources,
        generated,
        index: Index::default(),
        frames: Vec::new(),
        container: None,
    };
    collector.collect_file_scope(unit);
    collector.collect_references(unit);
    collector.collect_comptime(sites);
    collector.index
}

struct Frame {
    names: HashMap<String, usize>,
    symbols: Vec<usize>,
    start: usize,
}

struct Collector<'a> {
    sources: &'a SourceMap,
    /// Spans of the items a probe answer generated: every token of such an
    /// item carries its generator call site instead of real source text.
    generated: &'a [Span],
    index: Index,
    frames: Vec<Frame>,
    container: Option<String>,
}

impl Collector<'_> {
    fn text(&self, span: Span) -> String {
        self.sources.text(span).trim().to_string()
    }

    fn is_generated(&self, span: Span) -> bool {
        self.generated.contains(&span)
    }

    // -- scopes ---------------------------------------------------------

    fn push_frame(&mut self, start: usize) {
        self.frames.push(Frame {
            names: HashMap::new(),
            symbols: Vec::new(),
            start,
        });
    }

    fn pop_frame(&mut self, end: usize) {
        let Some(frame) = self.frames.pop() else {
            return;
        };
        for id in frame.symbols {
            let symbol = &mut self.index.symbols[id];
            symbol.scope = Some(Scope {
                file: symbol.span.file,
                start: frame.start,
                end,
            });
        }
    }

    fn declare(&mut self, symbol: Sym) -> usize {
        let id = self.index.push(symbol);
        if let Some(frame) = self.frames.last_mut() {
            frame.names.insert(self.index.symbols[id].name.clone(), id);
            frame.symbols.push(id);
        }
        id
    }

    fn scoped(&self, name: &str) -> Option<usize> {
        for frame in self.frames.iter().rev() {
            if let Some(&id) = frame.names.get(name) {
                return Some(id);
            }
        }
        None
    }

    fn add_ref(&mut self, name: &Name, target: usize) {
        self.index.add_ref(name.span, target);
    }

    fn add_value_ref(&mut self, name: &Name) {
        // `self` has no symbol of its own; it names the record the impl block
        // is for, so navigation lands on that declaration.
        if name.text == "self" {
            let target = self.container.as_deref().and_then(|container| {
                self.index
                    .lookup(container)
                    .iter()
                    .copied()
                    .find(|&id| is_type_kind(self.index.symbols[id].kind))
            });
            if let Some(target) = target {
                self.add_ref(name, target);
                return;
            }
        }
        if let Some(target) = self.scoped(&name.text) {
            self.add_ref(name, target);
            return;
        }
        if let Some(target) = self.pick(name, value_kinds) {
            self.add_ref(name, target);
        }
    }

    fn add_type_ref(&mut self, name: &Name) {
        if let Some(target) = self.scoped(&name.text) {
            if is_type_kind(self.index.symbols[target].kind) {
                self.add_ref(name, target);
            }
            return;
        }
        if let Some(target) = self.pick(name, type_kinds) {
            self.add_ref(name, target);
        }
    }

    /// The final segment of a named `use` may name a type or a value, so it
    /// resolves against every file-scope kind rather than types alone.
    fn add_import_ref(&mut self, name: &Name) {
        let target = self
            .index
            .lookup(&name.text)
            .iter()
            .copied()
            .find(|&id| is_import_kind(self.index.symbols[id].kind));
        if let Some(target) = target {
            self.add_ref(name, target);
        }
    }

    fn add_member_ref(&mut self, container: Option<&str>, name: &Name, call: bool) {
        let candidates: Vec<usize> = self
            .index
            .lookup(&name.text)
            .iter()
            .copied()
            .filter(|&id| {
                matches!(
                    self.index.symbols[id].kind,
                    SymKind::Field | SymKind::Method | SymKind::Variant
                )
            })
            .collect();
        if candidates.is_empty() {
            self.add_value_ref(name);
            return;
        }

        let owned: Vec<usize> = candidates
            .iter()
            .copied()
            .filter(|&id| {
                container
                    .is_some_and(|owner| self.index.symbols[id].container.as_deref() == Some(owner))
            })
            .collect();
        let pool = if owned.is_empty() { candidates } else { owned };

        let target = if call {
            pool.iter()
                .copied()
                .find(|&id| self.index.symbols[id].kind == SymKind::Method)
                .or_else(|| {
                    pool.iter()
                        .copied()
                        .find(|&id| self.index.symbols[id].kind == SymKind::Variant)
                })
                .or_else(|| pool.first().copied())
        } else {
            pool.iter()
                .copied()
                .find(|&id| self.index.symbols[id].kind == SymKind::Field)
                .or_else(|| {
                    pool.iter()
                        .copied()
                        .find(|&id| self.index.symbols[id].kind == SymKind::Variant)
                })
                .or_else(|| pool.first().copied())
        };

        if let Some(target) = target {
            self.add_ref(name, target);
        }
    }

    fn pick(&self, name: &Name, accepted: fn(SymKind) -> bool) -> Option<usize> {
        let ids = self.index.lookup(&name.text);
        ids.iter()
            .copied()
            .find(|&id| accepted(self.index.symbols[id].kind))
            .or_else(|| ids.first().copied())
    }

    // -- definitions ----------------------------------------------------

    fn collect_file_scope(&mut self, unit: &ast::TranslationUnit) {
        for item in &unit.items {
            match item {
                ast::Item::Decl(declaration) => self.collect_declaration(declaration),
                ast::Item::Function(function) => self.collect_function(function, None),
                ast::Item::Impl(block) => self.collect_impl(block),
                ast::Item::Trait(declaration) => self.collect_trait(declaration),
                ast::Item::Use(_) => {}
            }
        }
    }

    fn collect_declaration(&mut self, declaration: &ast::Declaration) {
        let typedef = declaration.specs.storage == Some(ast::StorageClass::Typedef);
        let hint = if typedef {
            declaration
                .declarators
                .first()
                .and_then(|declarator| declarator.declarator.name.as_ref())
                .map(|name| name.text.clone())
        } else {
            None
        };
        self.collect_specs(&declaration.specs, hint.as_deref());

        for declarator in &declaration.declarators {
            let Some(name) = &declarator.declarator.name else {
                continue;
            };
            if typedef {
                let detail = self.type_of(&declaration.specs, &declarator.declarator);
                self.index.push(Sym {
                    name: name.text.clone(),
                    kind: SymKind::TypeAlias,
                    span: name.span,
                    detail,
                    type_text: None,
                    container: None,
                    scope: None,
                });
            } else if is_function_declarator(&declarator.declarator) {
                self.collect_function_parts(
                    &declaration.specs,
                    &declarator.declarator,
                    None,
                    SymKind::Function,
                    name.span,
                );
            } else {
                let type_text = self.type_of(&declaration.specs, &declarator.declarator);
                self.index.push(Sym {
                    name: name.text.clone(),
                    kind: SymKind::Global,
                    span: name.span,
                    detail: type_text.clone(),
                    type_text: Some(type_text),
                    container: None,
                    scope: None,
                });
            }
        }
    }

    fn collect_function(&mut self, function: &ast::FunctionDecl, container: Option<&str>) {
        let kind = if container.is_some() {
            SymKind::Method
        } else {
            SymKind::Function
        };
        let Some(name) = &function.declarator.name else {
            return;
        };
        self.collect_function_parts(
            &function.specs,
            &function.declarator,
            container,
            kind,
            name.span,
        );
    }

    fn collect_impl(&mut self, block: &ast::ImplBlock) {
        let container = base_type_name(&self.text(block.self_type.span));
        for method in &block.methods {
            self.collect_function(method, Some(&container));
        }
        for prototype in &block.prototypes {
            let Some(name) = &prototype.declarator.name else {
                continue;
            };
            self.collect_function_parts(
                &prototype.specs,
                &prototype.declarator,
                Some(&container),
                SymKind::Method,
                name.span,
            );
        }
    }

    fn collect_trait(&mut self, declaration: &ast::TraitDecl) {
        let container = declaration.name.text.clone();
        self.index.push(Sym {
            name: container.clone(),
            kind: SymKind::Trait,
            span: declaration.name.span,
            detail: format!("trait {container}"),
            type_text: None,
            container: None,
            scope: None,
        });
        for method in &declaration.methods {
            let Some(name) = &method.declarator.name else {
                continue;
            };
            self.collect_function_parts(
                &method.specs,
                &method.declarator,
                Some(&container),
                SymKind::Method,
                name.span,
            );
        }
    }

    fn collect_function_parts(
        &mut self,
        specs: &ast::DeclSpecs,
        declarator: &ast::Declarator,
        container: Option<&str>,
        kind: SymKind,
        span: Span,
    ) {
        let Some(name) = &declarator.name else {
            return;
        };
        self.collect_specs(specs, None);
        if let Some(ast::DerivedDeclarator::Function(function)) = declarator.derived.first() {
            for parameter in &function.params {
                self.collect_specs(&parameter.specs, None);
            }
        }
        let (return_type, params) = if self.is_generated(span) {
            self.signature_from_tree(specs, declarator)
        } else {
            self.signature(specs, declarator)
        };
        let detail = format!("{} {}({})", return_type, name.text, params);
        self.index.push(Sym {
            name: name.text.clone(),
            kind,
            span,
            detail,
            type_text: Some(return_type),
            container: container.map(str::to_string),
            scope: None,
        });
    }

    fn collect_specs(&mut self, specs: &ast::DeclSpecs, hint: Option<&str>) {
        for specifier in &specs.type_specs {
            match specifier {
                ast::TypeSpecifier::Record(record) if record.members.is_some() => {
                    let container = record
                        .tag
                        .as_ref()
                        .map(|tag| tag.text.clone())
                        .or_else(|| hint.map(str::to_string));
                    if let (Some(tag), Some(container)) = (&record.tag, &container) {
                        let kind = match record.kind {
                            ast::RecordKind::Struct => SymKind::Struct,
                            ast::RecordKind::Union => SymKind::Union,
                        };
                        let keyword = match record.kind {
                            ast::RecordKind::Struct => "struct",
                            ast::RecordKind::Union => "union",
                        };
                        self.index.push(Sym {
                            name: tag.text.clone(),
                            kind,
                            span: tag.span,
                            detail: format!("{keyword} {container}"),
                            type_text: None,
                            container: None,
                            scope: None,
                        });
                    }

                    for member in record.members.iter().flatten() {
                        self.collect_specs(&member.specs, container.as_deref());
                        for declarator in &member.declarators {
                            let Some(field) = &declarator.declarator.name else {
                                continue;
                            };
                            let field_type = self.type_of(&member.specs, &declarator.declarator);
                            self.index.push(Sym {
                                name: field.text.clone(),
                                kind: SymKind::Field,
                                span: field.span,
                                detail: field_type.clone(),
                                type_text: Some(field_type),
                                container: container.clone(),
                                scope: None,
                            });
                        }
                    }
                }
                ast::TypeSpecifier::Enum(enumeration) if enumeration.enumerators.is_some() => {
                    let container = enumeration
                        .tag
                        .as_ref()
                        .map(|tag| tag.text.clone())
                        .or_else(|| hint.map(str::to_string));
                    if let (Some(tag), Some(container)) = (&enumeration.tag, &container) {
                        self.index.push(Sym {
                            name: tag.text.clone(),
                            kind: SymKind::Enum,
                            span: tag.span,
                            detail: format!("enum {container}"),
                            type_text: None,
                            container: None,
                            scope: None,
                        });
                    }
                    for enumerator in enumeration.enumerators.iter().flatten() {
                        let detail = enumerator
                            .payload
                            .as_ref()
                            .map(|payload| {
                                format!(
                                    "{}::{}({})",
                                    container.as_deref().unwrap_or("enum"),
                                    enumerator.name.text,
                                    self.text(payload.span)
                                )
                            })
                            .unwrap_or_else(|| {
                                format!(
                                    "{}::{}",
                                    container.as_deref().unwrap_or("enum"),
                                    enumerator.name.text
                                )
                            });
                        self.index.push(Sym {
                            name: enumerator.name.text.clone(),
                            kind: SymKind::Variant,
                            span: enumerator.name.span,
                            detail,
                            type_text: container.clone(),
                            container: container.clone(),
                            scope: None,
                        });
                    }
                }
                ast::TypeSpecifier::Record(_) | ast::TypeSpecifier::Enum(_) => {}
                ast::TypeSpecifier::Atomic(type_name) => {
                    self.collect_specs(&type_name.specs, hint);
                }
                _ => {}
            }
        }
    }

    // -- references -----------------------------------------------------

    /// Adds the compile-time names the parser consumed.
    ///
    /// A const fn declaration never reaches the AST, and a call or a macro is
    /// replaced by its expansion, so both are indexed from the parser's report.
    fn collect_comptime(&mut self, sites: &[shyc::parser::ComptimeSite]) {
        use shyc::parser::ComptimeSiteKind;

        let mut declarations: HashMap<usize, usize> = HashMap::new();
        for site in sites {
            if site.kind != ComptimeSiteKind::Declaration {
                continue;
            }
            if declarations.contains_key(&site.span.start) {
                continue;
            }
            let detail = match site.signature {
                Some(signature) => {
                    let mut text = self.sources.text(signature).trim().to_string();
                    text = text.split_whitespace().collect::<Vec<_>>().join(" ");
                    text
                }
                None => format!("const fn {}", site.name),
            };
            let id = self.index.push(Sym {
                name: site.name.clone(),
                kind: SymKind::Comptime,
                span: site.span,
                detail,
                type_text: None,
                container: None,
                scope: None,
            });
            declarations.insert(site.span.start, id);
        }

        for site in sites {
            match site.kind {
                ComptimeSiteKind::Declaration => {}
                ComptimeSiteKind::Call => {
                    let target = site
                        .declaration
                        .and_then(|declaration| declarations.get(&declaration.start).copied())
                        .or_else(|| self.index.lookup(&site.name).first().copied());
                    if let Some(target) = target {
                        self.index.add_ref(site.span, target);
                    }
                }
                ComptimeSiteKind::Macro => {
                    // Jump to the wrapper the expansion calls, so the macro has
                    // a real definition in the formatting header.
                    let target = site
                        .expansion
                        .as_deref()
                        .and_then(|name| self.index.lookup(name).first().copied());
                    if let Some(target) = target {
                        self.index.add_ref(site.span, target);
                    }
                }
            }
        }
    }

    fn collect_references(&mut self, unit: &ast::TranslationUnit) {
        for item in &unit.items {
            match item {
                ast::Item::Decl(declaration) => {
                    self.walk_specs(&declaration.specs);
                    for declarator in &declaration.declarators {
                        self.walk_declarator(&declarator.declarator, true);
                        if let Some(init) = &declarator.init {
                            self.walk_initializer(init);
                        }
                    }
                }
                ast::Item::Function(function) => self.walk_function(function, None),
                ast::Item::Impl(block) => {
                    self.push_frame(block.span.start);
                    self.declare_generics(&block.generics);
                    self.walk_type_name(&block.self_type);
                    if let Some(trait_name) = &block.trait_name {
                        self.add_type_ref(trait_name);
                    }
                    for argument in &block.trait_args {
                        self.walk_generic_arg(argument);
                    }
                    let container = base_type_name(&self.text(block.self_type.span));
                    for method in &block.methods {
                        self.walk_function(method, Some(&container));
                    }
                    for prototype in &block.prototypes {
                        self.walk_signature(&prototype.declarator);
                    }
                    self.pop_frame(block.span.end);
                }
                ast::Item::Trait(declaration) => {
                    self.push_frame(declaration.span.start);
                    self.declare_generics(&declaration.generics);
                    let container = declaration.name.text.clone();
                    for method in &declaration.methods {
                        self.walk_signature(&method.declarator);
                        // A default body runs with `self` bound to whatever
                        // type implements the trait, so its members come from
                        // the trait's own method set.
                        if let Some(body) = &method.body {
                            let previous = self.container.clone();
                            self.container = Some(container.clone());
                            self.walk_block(body);
                            self.set_symbol_scope(
                                method.declarator.name.as_ref().map(|name| name.span),
                                method.span.start,
                                body.span.end,
                            );
                            self.container = previous;
                        }
                    }
                    self.pop_frame(declaration.span.end);
                }
                ast::Item::Use(declaration) => {
                    // A named import's last segment is the imported symbol. A
                    // glob's last segment names the module, so it has no
                    // definition to navigate to.
                    if !declaration.glob {
                        if let Some(last) = declaration.path.last() {
                            self.add_import_ref(last);
                        }
                    }
                }
            }
        }
    }

    fn walk_function(&mut self, function: &ast::FunctionDecl, container: Option<&str>) {
        let scope_end = if function.body.span.start == function.body.span.end {
            function.span.end
        } else {
            function.body.span.end
        };
        let previous_container = self.container.clone();
        if container.is_some() {
            self.container = container.map(str::to_string);
        }
        self.push_frame(function.span.start);
        if let Some(ast::DerivedDeclarator::Function(declarator)) =
            function.declarator.derived.first()
        {
            self.declare_generics(&declarator.generics);
        }
        self.walk_specs(&function.specs);
        self.walk_signature(&function.declarator);
        self.walk_block(&function.body);
        self.pop_frame(scope_end);
        self.set_symbol_scope(
            function.declarator.name.as_ref().map(|name| name.span),
            function.span.start,
            scope_end,
        );
        self.container = previous_container;
    }

    /// Records where a symbol is visible, by the declaration's name span.
    fn set_symbol_scope(&mut self, span: Option<Span>, start: usize, end: usize) {
        let Some(span) = span else {
            return;
        };
        if let Some(id) = self
            .index
            .symbols
            .iter()
            .position(|symbol| symbol.span == span)
        {
            self.index.symbols[id].scope = Some(Scope {
                file: span.file,
                start,
                end,
            });
        }
    }

    fn walk_signature(&mut self, declarator: &ast::Declarator) {
        let Some(ast::DerivedDeclarator::Function(function)) = declarator.derived.first() else {
            self.walk_declarator(declarator, true);
            return;
        };

        for parameter in &function.params {
            self.walk_specs(&parameter.specs);
            let Some(declarator) = &parameter.declarator else {
                continue;
            };
            self.walk_declarator(declarator, true);
            let Some(name) = &declarator.name else {
                continue;
            };
            let type_text = self.type_of(&parameter.specs, declarator);
            self.declare(Sym {
                name: name.text.clone(),
                kind: SymKind::Param,
                span: name.span,
                detail: format!("{} {}", type_text, name.text),
                type_text: Some(type_text),
                container: None,
                scope: None,
            });
        }
    }

    fn walk_block(&mut self, block: &ast::Block) {
        self.push_frame(block.span.start);
        for statement in &block.stmts {
            self.walk_stmt(statement);
        }
        self.pop_frame(block.span.end);
    }

    fn walk_stmt(&mut self, statement: &ast::Stmt) {
        match statement {
            ast::Stmt::Empty(_) | ast::Stmt::Break(_) | ast::Stmt::Continue(_) => {}
            ast::Stmt::Expr { expr, .. } => self.walk_expr(expr),
            ast::Stmt::Decl(declaration) => self.walk_local_declaration(declaration),
            ast::Stmt::Let(declaration) => {
                let type_text = self.infer_expr_type(&declaration.init);
                self.walk_expr(&declaration.init);
                self.declare(Sym {
                    name: declaration.name.text.clone(),
                    kind: SymKind::Local,
                    span: declaration.name.span,
                    detail: match &type_text {
                        Some(text) => format!("{} {}", text, declaration.name.text),
                        None => format!("let {}", declaration.name.text),
                    },
                    type_text,
                    container: None,
                    scope: None,
                });
            }
            ast::Stmt::Block(block) => self.walk_block(block),
            ast::Stmt::If {
                condition,
                then_branch,
                else_branch,
                ..
            } => {
                self.walk_expr(condition);
                self.walk_stmt(then_branch);
                if let Some(else_branch) = else_branch {
                    self.walk_stmt(else_branch);
                }
            }
            ast::Stmt::While {
                condition, body, ..
            } => {
                self.walk_expr(condition);
                self.walk_stmt(body);
            }
            ast::Stmt::DoWhile {
                body, condition, ..
            } => {
                self.walk_stmt(body);
                self.walk_expr(condition);
            }
            ast::Stmt::For {
                init,
                condition,
                step,
                body,
                span,
            } => {
                self.push_frame(span.start);
                if let Some(init) = init {
                    match init.as_ref() {
                        ast::ForInit::Decl(declaration) => self.walk_local_declaration(declaration),
                        ast::ForInit::Expr(expr) => self.walk_expr(expr),
                        ast::ForInit::Let(declaration) => {
                            let type_text = self.infer_expr_type(&declaration.init);
                            self.walk_expr(&declaration.init);
                            self.declare(Sym {
                                name: declaration.name.text.clone(),
                                kind: SymKind::Local,
                                span: declaration.name.span,
                                detail: match &type_text {
                                    Some(text) => format!("{} {}", text, declaration.name.text),
                                    None => format!("let {}", declaration.name.text),
                                },
                                type_text,
                                container: None,
                                scope: None,
                            });
                        }
                    }
                }
                if let Some(condition) = condition {
                    self.walk_expr(condition);
                }
                if let Some(step) = step {
                    self.walk_expr(step);
                }
                self.walk_stmt(body);
                self.pop_frame(span.end);
            }
            ast::Stmt::Switch {
                condition, body, ..
            } => {
                self.walk_expr(condition);
                self.walk_stmt(body);
            }
            ast::Stmt::Case { value, stmt, .. } => {
                self.walk_expr(value);
                self.walk_stmt(stmt);
            }
            ast::Stmt::Default { stmt, .. } => self.walk_stmt(stmt),
            ast::Stmt::Label { name, stmt, .. } => {
                self.declare(Sym {
                    name: name.text.clone(),
                    kind: SymKind::Label,
                    span: name.span,
                    detail: "label".to_string(),
                    type_text: None,
                    container: None,
                    scope: None,
                });
                self.walk_stmt(stmt);
            }
            ast::Stmt::Goto { label, .. } => self.add_value_ref(label),
            ast::Stmt::Return { value, .. } => {
                if let Some(value) = value {
                    self.walk_expr(value);
                }
            }
            ast::Stmt::Asm(asm) => {
                if let ast::AsmStmt::Binding { operands, .. } = asm {
                    for operand in operands {
                        self.add_value_ref(operand);
                    }
                }
            }
            ast::Stmt::AllowDeferCopy { stmt, .. } => self.walk_stmt(stmt),
        }
    }

    fn walk_local_declaration(&mut self, declaration: &ast::Declaration) {
        self.walk_specs(&declaration.specs);
        let typedef = declaration.specs.storage == Some(ast::StorageClass::Typedef);
        for declarator in &declaration.declarators {
            self.walk_declarator(&declarator.declarator, true);
            let Some(name) = &declarator.declarator.name else {
                continue;
            };
            let type_text = self.type_of(&declaration.specs, &declarator.declarator);
            self.declare(Sym {
                name: name.text.clone(),
                kind: if typedef {
                    SymKind::TypeAlias
                } else {
                    SymKind::Local
                },
                span: name.span,
                detail: type_text.clone(),
                type_text: if typedef { None } else { Some(type_text) },
                container: None,
                scope: None,
            });
            if let Some(init) = &declarator.init {
                self.walk_initializer(init);
            }
        }
    }

    fn walk_expr(&mut self, expression: &ast::Expr) {
        match expression {
            ast::Expr::Number(_) | ast::Expr::Char(_) | ast::Expr::String(_) => {}
            ast::Expr::Name(name) => self.add_value_ref(name),
            ast::Expr::ScopedName {
                qualifier, name, ..
            } => {
                self.add_type_ref(qualifier);
                let container = qualifier.text.clone();
                self.add_member_ref(Some(&container), name, true);
            }
            ast::Expr::GenericScopedName {
                qualifier,
                args,
                name,
                ..
            } => {
                self.add_type_ref(qualifier);
                for argument in args {
                    self.walk_generic_arg(argument);
                }
                let container = qualifier.text.clone();
                self.add_member_ref(Some(&container), name, true);
            }
            ast::Expr::BuiltinTypeCall { type_args, .. } => {
                for type_name in type_args {
                    self.walk_type_name(type_name);
                }
            }
            ast::Expr::Unary { operand, .. }
            | ast::Expr::Postfix { operand, .. }
            | ast::Expr::Try { value: operand, .. } => self.walk_expr(operand),
            ast::Expr::Binary { left, right, .. }
            | ast::Expr::Assignment { left, right, .. }
            | ast::Expr::Comma { left, right, .. } => {
                self.walk_expr(left);
                self.walk_expr(right);
            }
            ast::Expr::Conditional {
                condition,
                then_expr,
                else_expr,
                ..
            } => {
                self.walk_expr(condition);
                self.walk_expr(then_expr);
                self.walk_expr(else_expr);
            }
            ast::Expr::Call {
                callee,
                type_args,
                args,
                ..
            } => {
                for argument in type_args {
                    self.walk_generic_arg(argument);
                }
                if let ast::Expr::Member {
                    object, name, op, ..
                } = callee.as_ref()
                {
                    self.walk_expr(object);
                    let container = self.infer_expr_type(object);
                    self.add_member_ref(container.as_deref(), name, true);
                    let _ = op;
                } else {
                    self.walk_expr(callee);
                }
                for argument in args {
                    self.walk_expr(argument);
                }
            }
            ast::Expr::Index { array, index, .. } => {
                self.walk_expr(array);
                self.walk_expr(index);
            }
            ast::Expr::Member { object, name, .. } => {
                self.walk_expr(object);
                let container = self.infer_expr_type(object);
                self.add_member_ref(container.as_deref(), name, false);
            }
            ast::Expr::Cast {
                type_name, value, ..
            } => {
                self.walk_type_name(type_name);
                self.walk_expr(value);
            }
            ast::Expr::SizeofType { type_name, .. } | ast::Expr::AlignofType { type_name, .. } => {
                self.walk_type_name(type_name)
            }
            ast::Expr::SizeofExpr { value, .. } | ast::Expr::AlignofExpr { value, .. } => {
                self.walk_expr(value)
            }
            ast::Expr::CompoundLiteral {
                type_name, init, ..
            } => {
                self.walk_type_name(type_name);
                self.walk_initializer(init);
            }
            ast::Expr::StatementExpr { block, .. } => self.walk_block(block),
        }
    }

    fn walk_initializer(&mut self, initializer: &ast::Initializer) {
        match &initializer.kind {
            ast::InitializerKind::Expr(expression) => self.walk_expr(expression),
            ast::InitializerKind::List(entries) => {
                for entry in entries {
                    for designator in &entry.designators {
                        match designator {
                            ast::Designator::Field(name) => {
                                let name = name.clone();
                                self.add_member_ref(None, &name, false);
                            }
                            ast::Designator::Index(expression) => self.walk_expr(expression),
                        }
                    }
                    self.walk_initializer(&entry.value);
                }
            }
        }
    }

    fn walk_specs(&mut self, specs: &ast::DeclSpecs) {
        for specifier in &specs.type_specs {
            match specifier {
                ast::TypeSpecifier::TypedefName(name) => self.add_type_ref(name),
                ast::TypeSpecifier::Generic(generic) => {
                    self.add_type_ref(&generic.name);
                    for argument in &generic.args {
                        self.walk_generic_arg(argument);
                    }
                }
                ast::TypeSpecifier::Record(record) => {
                    if record.members.is_some() {
                        self.push_frame(record.span.start);
                        self.declare_generics(&record.generics);
                        for member in record.members.iter().flatten() {
                            self.walk_specs(&member.specs);
                            for declarator in &member.declarators {
                                self.walk_declarator(&declarator.declarator, true);
                                if let Some(width) = &declarator.bit_width {
                                    self.walk_expr(width);
                                }
                            }
                        }
                        self.pop_frame(record.span.end);
                    } else if let Some(tag) = &record.tag {
                        self.add_type_ref(tag);
                    }
                }
                ast::TypeSpecifier::Enum(enumeration) => {
                    if let Some(underlying) = &enumeration.underlying {
                        self.walk_type_name(underlying);
                    }
                    if enumeration.enumerators.is_some() {
                        self.push_frame(enumeration.span.start);
                        self.declare_generics(&enumeration.generics);
                        for enumerator in enumeration.enumerators.iter().flatten() {
                            if let Some(payload) = &enumerator.payload {
                                self.walk_type_name(payload);
                            }
                            if let Some(value) = &enumerator.value {
                                self.walk_expr(value);
                            }
                        }
                        self.pop_frame(enumeration.span.end);
                    } else if let Some(tag) = &enumeration.tag {
                        self.add_type_ref(tag);
                    }
                }
                ast::TypeSpecifier::Atomic(type_name) => self.walk_type_name(type_name),
                ast::TypeSpecifier::Builtin(_) | ast::TypeSpecifier::SelfType => {}
            }
        }
    }

    fn walk_type_name(&mut self, type_name: &ast::TypeName) {
        self.walk_specs(&type_name.specs);
        if let Some(declarator) = &type_name.declarator {
            self.walk_declarator(declarator, true);
        }
    }

    fn walk_declarator(&mut self, declarator: &ast::Declarator, walk_params: bool) {
        for derived in &declarator.derived {
            match derived {
                ast::DerivedDeclarator::Pointer(_) => {}
                ast::DerivedDeclarator::Array(array) => {
                    if let ast::ArrayLength::Expr(length) = &array.length {
                        self.walk_expr(length);
                    }
                }
                ast::DerivedDeclarator::Function(function) => {
                    if walk_params {
                        self.declare_generics(&function.generics);
                        for parameter in &function.params {
                            self.walk_specs(&parameter.specs);
                            if let Some(declarator) = &parameter.declarator {
                                self.walk_declarator(declarator, true);
                            }
                        }
                    }
                }
            }
        }
    }

    fn walk_generic_arg(&mut self, argument: &ast::GenericArg) {
        match &argument.kind {
            ast::GenericArgKind::Type(type_name) => self.walk_type_name(type_name),
            ast::GenericArgKind::Const(expression) => self.walk_expr(expression),
        }
    }

    fn declare_generics(&mut self, generics: &[ast::GenericParam]) {
        for parameter in generics {
            let kind = match parameter.kind {
                ast::GenericParamKind::Type => SymKind::TypeParam,
                ast::GenericParamKind::Const => SymKind::ConstParam,
            };
            self.declare(Sym {
                name: parameter.name.text.clone(),
                kind,
                span: parameter.name.span,
                detail: match parameter.kind {
                    ast::GenericParamKind::Type => "type parameter".to_string(),
                    ast::GenericParamKind::Const => "const parameter".to_string(),
                },
                type_text: None,
                container: None,
                scope: None,
            });
            for bound in &parameter.bounds {
                self.add_type_ref(&bound.name);
                for argument in &bound.args {
                    self.walk_generic_arg(argument);
                }
            }
        }
    }

    // -- type text ------------------------------------------------------

    fn infer_expr_type(&self, expression: &ast::Expr) -> Option<String> {
        match expression {
            ast::Expr::Name(name) if name.text == "self" => self.container.clone(),
            ast::Expr::Name(name) => self
                .scoped(&name.text)
                .and_then(|id| self.index.symbols[id].type_text.clone())
                .or_else(|| {
                    self.index
                        .lookup(&name.text)
                        .iter()
                        .find_map(|&id| self.index.symbols[id].type_text.clone())
                }),
            ast::Expr::Unary {
                op: ast::UnaryOp::Dereference,
                operand,
                ..
            } => self
                .infer_expr_type(operand)
                .map(|text| strip_pointer(&text)),
            ast::Expr::Index { array, .. } => {
                self.infer_expr_type(array).map(|text| strip_pointer(&text))
            }
            ast::Expr::Member { object, name, .. } => {
                let container = self.infer_expr_type(object).map(|text| {
                    let base = base_type_name(&text);
                    if base == "self" {
                        self.container.clone().unwrap_or(base)
                    } else {
                        base
                    }
                });
                let candidates = self.index.lookup(&name.text);
                let id = candidates
                    .iter()
                    .copied()
                    .find(|&id| {
                        matches!(
                            self.index.symbols[id].kind,
                            SymKind::Field | SymKind::Method | SymKind::Variant
                        ) && container.as_deref().is_none_or(|owner| {
                            self.index.symbols[id].container.as_deref() == Some(owner)
                        })
                    })
                    .or_else(|| candidates.first().copied())?;
                self.index.symbols[id].type_text.clone()
            }
            ast::Expr::Call { callee, .. } => self.infer_expr_type(callee),
            ast::Expr::ScopedName {
                qualifier, name, ..
            }
            | ast::Expr::GenericScopedName {
                qualifier, name, ..
            } => {
                let candidates = self.index.lookup(&name.text);
                let id = candidates
                    .iter()
                    .copied()
                    .find(|&id| {
                        matches!(
                            self.index.symbols[id].kind,
                            SymKind::Method | SymKind::Variant
                        ) && self.index.symbols[id].container.as_deref()
                            == Some(qualifier.text.as_str())
                    })
                    .or_else(|| {
                        candidates.iter().copied().find(|&id| {
                            matches!(
                                self.index.symbols[id].kind,
                                SymKind::Method | SymKind::Variant
                            )
                        })
                    })?;
                self.index.symbols[id].type_text.clone()
            }
            ast::Expr::Cast { type_name, .. } | ast::Expr::CompoundLiteral { type_name, .. } => {
                Some(self.text(type_name.span))
            }
            ast::Expr::String(_) => Some("char *".to_string()),
            _ => None,
        }
    }

    fn type_of(&self, specs: &ast::DeclSpecs, declarator: &ast::Declarator) -> String {
        let base = self.specs_text(specs);
        let pointers = declarator
            .derived
            .iter()
            .filter(|derived| matches!(derived, ast::DerivedDeclarator::Pointer(_)))
            .count();
        let arrays: String = declarator
            .derived
            .iter()
            .filter_map(|derived| match derived {
                ast::DerivedDeclarator::Array(array) => Some(self.text(array.span)),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("");
        let mut text = base;
        for _ in 0..pointers {
            text.push_str(" *");
        }
        text.push_str(&arrays);
        if let Some(ast::DerivedDeclarator::Function(function)) = declarator
            .derived
            .iter()
            .find(|derived| matches!(derived, ast::DerivedDeclarator::Function(_)))
        {
            let params = function
                .params
                .iter()
                .map(|parameter| self.text(parameter.span))
                .collect::<Vec<_>>()
                .join(", ");
            text = format!("{text} (*)({params})");
        }
        text
    }

    fn return_type(&self, specs: &ast::DeclSpecs, declarator: &ast::Declarator) -> String {
        let mut text = self.specs_text(specs);
        for derived in declarator.derived.iter().skip(1) {
            match derived {
                ast::DerivedDeclarator::Pointer(_) => text.push_str(" *"),
                ast::DerivedDeclarator::Array(array) => text.push_str(&self.text(array.span)),
                ast::DerivedDeclarator::Function(_) => {}
            }
        }
        text
    }

    fn signature(&self, specs: &ast::DeclSpecs, declarator: &ast::Declarator) -> (String, String) {
        let return_type = self.return_type(specs, declarator);
        let Some(ast::DerivedDeclarator::Function(function)) = declarator.derived.first() else {
            return (return_type, String::new());
        };
        let params = function
            .params
            .iter()
            .map(|parameter| self.text(parameter.span))
            .collect::<Vec<_>>()
            .join(", ");
        let params = if params == "void" {
            String::new()
        } else {
            params
        };
        (return_type, params)
    }

    /// The signature of an item whose tokens came from a probe answer.
    ///
    /// Every token of such an item carries the generator call site, so slicing
    /// source text would print the call instead of the declaration. The tree
    /// still has the real types, names, and declarators.
    fn signature_from_tree(
        &self,
        specs: &ast::DeclSpecs,
        declarator: &ast::Declarator,
    ) -> (String, String) {
        let return_type = self.return_type(specs, declarator);
        let Some(ast::DerivedDeclarator::Function(function)) = declarator.derived.first() else {
            return (return_type, String::new());
        };
        let mut params: Vec<String> = function
            .params
            .iter()
            .map(|parameter| self.parameter_text(parameter))
            .collect();
        if function.variadic {
            params.push("...".to_string());
        }
        let params = if params.as_slice() == ["void"] {
            String::new()
        } else {
            params.join(", ")
        };
        (return_type, params)
    }

    fn parameter_text(&self, parameter: &ast::ParamDecl) -> String {
        let Some(declarator) = &parameter.declarator else {
            return self.specs_text(&parameter.specs);
        };
        if declarator
            .derived
            .iter()
            .any(|derived| matches!(derived, ast::DerivedDeclarator::Function(_)))
        {
            return self.type_of(&parameter.specs, declarator);
        }
        let mut text = self.specs_text(&parameter.specs);
        let pointers = declarator
            .derived
            .iter()
            .filter(|derived| matches!(derived, ast::DerivedDeclarator::Pointer(_)))
            .count();
        for _ in 0..pointers {
            text.push_str(" *");
        }
        if let Some(name) = &declarator.name {
            if pointers == 0 {
                text.push(' ');
            }
            text.push_str(&name.text);
        }
        for derived in &declarator.derived {
            if let ast::DerivedDeclarator::Array(array) = derived {
                match &array.length {
                    ast::ArrayLength::Unspecified => text.push_str("[]"),
                    ast::ArrayLength::Star => text.push_str("[*]"),
                    ast::ArrayLength::Expr(_) => text.push_str(&self.text(array.span)),
                }
            }
        }
        text
    }

    fn specs_text(&self, specs: &ast::DeclSpecs) -> String {
        let mut parts: Vec<String> = specs
            .qualifiers
            .iter()
            .map(|qualifier| {
                match qualifier {
                    ast::Qualifier::Const => "const",
                    ast::Qualifier::Volatile => "volatile",
                    ast::Qualifier::Restrict => "restrict",
                    ast::Qualifier::Atomic => "_Atomic",
                }
                .to_string()
            })
            .collect();

        for specifier in &specs.type_specs {
            parts.push(match specifier {
                ast::TypeSpecifier::Builtin(builtin) => builtin_text(*builtin).to_string(),
                ast::TypeSpecifier::TypedefName(name) => name.text.clone(),
                ast::TypeSpecifier::Generic(generic) => self.text(generic.span),
                ast::TypeSpecifier::Record(record) => match &record.tag {
                    Some(tag) => match record.kind {
                        ast::RecordKind::Struct => format!("struct {}", tag.text),
                        ast::RecordKind::Union => format!("union {}", tag.text),
                    },
                    None => match record.kind {
                        ast::RecordKind::Struct => "struct".to_string(),
                        ast::RecordKind::Union => "union".to_string(),
                    },
                },
                ast::TypeSpecifier::Enum(enumeration) => {
                    let mut text = "enum".to_string();
                    if let Some(underlying) = &enumeration.underlying {
                        text.push('(');
                        text.push_str(&self.text(underlying.span));
                        text.push(')');
                    }
                    if let Some(tag) = &enumeration.tag {
                        text.push(' ');
                        text.push_str(&tag.text);
                    }
                    text
                }
                ast::TypeSpecifier::Atomic(type_name) => {
                    format!("_Atomic({})", self.text(type_name.span))
                }
                ast::TypeSpecifier::SelfType => "self".to_string(),
            });
        }

        parts.join(" ")
    }
}

fn is_function_declarator(declarator: &ast::Declarator) -> bool {
    matches!(
        declarator.derived.first(),
        Some(ast::DerivedDeclarator::Function(_))
    )
}

fn is_type_kind(kind: SymKind) -> bool {
    matches!(
        kind,
        SymKind::Struct
            | SymKind::Union
            | SymKind::Enum
            | SymKind::Trait
            | SymKind::TypeAlias
            | SymKind::TypeParam
    )
}

/// Kinds that a `use path::Name;` can import: the compiler's file-scope
/// typedefs, tags, traits, enumerators, objects, and functions.
fn is_import_kind(kind: SymKind) -> bool {
    matches!(
        kind,
        SymKind::Struct
            | SymKind::Union
            | SymKind::Enum
            | SymKind::Trait
            | SymKind::TypeAlias
            | SymKind::Function
            | SymKind::Global
            | SymKind::Variant
    )
}

fn type_kinds(kind: SymKind) -> bool {
    is_type_kind(kind)
}

fn value_kinds(kind: SymKind) -> bool {
    matches!(
        kind,
        SymKind::Function
            | SymKind::Method
            | SymKind::Global
            | SymKind::Local
            | SymKind::Param
            | SymKind::Variant
            | SymKind::Label
            | SymKind::ConstParam
    )
}

fn builtin_text(builtin: ast::BuiltinType) -> &'static str {
    match builtin {
        ast::BuiltinType::Void => "void",
        ast::BuiltinType::Bool => "_Bool",
        ast::BuiltinType::Char => "char",
        ast::BuiltinType::Short => "short",
        ast::BuiltinType::Int => "int",
        ast::BuiltinType::Long => "long",
        ast::BuiltinType::Float => "float",
        ast::BuiltinType::Double => "double",
        ast::BuiltinType::Signed => "signed",
        ast::BuiltinType::Unsigned => "unsigned",
    }
}

pub fn base_type_name(text: &str) -> String {
    let mut text = text.trim().to_string();
    loop {
        let trimmed = text.trim_end();
        let Some(open) = trimmed.rfind('[') else {
            break;
        };
        if !trimmed.ends_with(']') {
            break;
        }
        text = trimmed[..open].trim_end().to_string();
    }
    if let Some(open) = text.find('<') {
        text.truncate(open);
    }
    let text = text.replace('*', " ");
    text.split_whitespace()
        .rfind(|word| {
            !matches!(
                *word,
                "const"
                    | "volatile"
                    | "restrict"
                    | "struct"
                    | "union"
                    | "enum"
                    | "static"
                    | "extern"
                    | "inline"
                    | "_Atomic"
            )
        })
        .unwrap_or("")
        .to_string()
}

fn strip_pointer(text: &str) -> String {
    text.trim().trim_end_matches('*').trim().to_string()
}
