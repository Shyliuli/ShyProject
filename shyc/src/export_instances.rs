//! Explicit generic instance export for the `shyc` driver.
//!
//! Generics are compile-time templates, so no instance crosses the C boundary
//! unless it is named here. This module owns both ends of that boundary:
//!
//! - [`parse_table`] reads the instance table (`--instances <file>`), a small
//!   compile-time input rather than C source;
//! - [`render_header_section`] and [`render_wrapper`] print the C header and the C
//!   wrapper source for the concrete instances the lowering pass produced.
//!
//! # Table syntax
//!
//! One entry per generic record, enum, or function, with the C name it is
//! exported under:
//!
//! ```text
//! Vec<int> as vec_int,
//! Array<int, 16> as int_array_16,
//! identity<int> as identity_int
//! ```
//!
//! Entries are separated by `,` or `;`. Whitespace and newlines are free, and
//! `//` and `/* ... */` comments are ignored. Generic arguments are parsed by
//! the ordinary ShyC parser, so nested arguments (`Vec<Vec<int>>`), pointer
//! types, and constant expressions (`Array<char, 2 * 8>`) keep their usual
//! spelling.
//!
//! # C names
//!
//! The table alias names the C-facing API:
//!
//! | table entry | C names |
//! | --- | --- |
//! | `Vec<int> as vec_int` | `typedef ... vec_int;`, `vec_int_<method>` |
//! | `identity<int> as identity_int` | `identity_int` |
//!
//! The concrete instance itself is exported once, under a stable mangled
//! symbol (`shyc_inst_...`); the C names above are thin wrappers that call it.
//! Two aliases for the same concrete instance therefore instantiate once and
//! share one implementation.
//!
//! A trait-qualified entry is rejected with an explicit diagnostic. Trait
//! implementation tables are not part of this export yet, and silently
//! dropping such a request would leave the caller without the symbol it asked
//! for.

use std::collections::{HashMap, HashSet};

use crate::ast::{GenericArg, Name};
use crate::lexer::{Token, TokenKind};
use crate::parser::TypeNames;
use crate::sema::types::{RecordKind, TypeId, TypeKind, TypeStore};
use crate::source::{Diagnostic, Span};

// ---------------------------------------------------------------------------
// The lowered export plan
// ---------------------------------------------------------------------------

/// One concrete instance the C boundary exposes.
#[derive(Debug)]
pub struct ExportedInstance {
    pub alias: String,
    pub kind: ExportedKind,
}

#[derive(Debug)]
pub enum ExportedKind {
    /// A concrete generic record with its inherent methods.
    Record {
        ty: TypeId,
        methods: Vec<ExportedMethod>,
    },
    /// A concrete generic enum. Its enumerators stay in ShyC spelling, so C
    /// sees only the layout or its integer representation.
    Enum { ty: TypeId },
    /// A concrete generic free function.
    Function { ty: TypeId, symbol: String },
}

/// One inherent method of an exported record instance.
#[derive(Debug)]
pub struct ExportedMethod {
    /// The ShyC method name, without the record.
    pub name: String,
    /// The concrete function type, including `self`.
    pub ty: TypeId,
    /// The public concrete symbol the wrapper calls.
    pub symbol: String,
}

/// How C spells one concrete record or enum type.
#[derive(Debug, Clone)]
pub struct CTypeName {
    /// The base type spelling used in declarations: `struct shyc_R7`, `Point`,
    /// `shyc_E3`, ...
    pub base: String,
    /// Whether the header must print this layout itself. Generic instances and
    /// synthesized names need it; a plain record declared in the input file is
    /// already printed by the ordinary `--emit=c-header` export.
    pub emit_header: bool,
}

// ---------------------------------------------------------------------------
// The instance table
// ---------------------------------------------------------------------------

/// One parsed instance table.
#[derive(Debug)]
pub struct Table {
    pub entries: Vec<Entry>,
}

/// An instance-level marker saying which niche layout the instance uses.
///
/// The marker keeps the rule out of the compiler: `#[result_niche]` asks for the
/// `Result<void, E>` zero-value form, `#[option_niche]` for the `Option<*T>`
/// null-pointer form. The shape still has to fit, or the table is rejected.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Niche {
    Result,
    Option,
}

/// One generic instantiation the C boundary exposes under `alias`.
#[derive(Debug)]
pub struct Entry {
    /// An instance-level niche marker: `#[result_niche]` or `#[option_niche]`.
    pub niche: Option<Niche>,
    /// The generic template name (`Vec`, `identity`, ...).
    pub name: Name,
    /// Concrete arguments, parsed like a use site writes them.
    pub args: Vec<GenericArg>,
    /// The C name the instance is exported under.
    pub alias: String,
    pub span: Span,
}

/// Parses an optional `#[result_niche]` or `#[option_niche]` marker.
fn parse_niche_marker(
    tokens: &[&Token],
    index: &mut usize,
) -> Result<Option<Niche>, Diagnostic> {
    if !is_punct(tokens[*index], "#") {
        return Ok(None);
    }
    let start = tokens[*index].span;
    *index += 1;
    if *index >= tokens.len() || !is_punct(tokens[*index], "[") {
        return Err(Diagnostic::new(start, "expected '[' after '#'"));
    }
    *index += 1;
    let name = token_ident(tokens[*index])
        .ok_or_else(|| Diagnostic::new(tokens[*index].span, "expected a niche marker name"))?;
    let niche = match name {
        "result_niche" => Niche::Result,
        // 布局还没实现：`Option<*T>` 目前仍是 tag + payload，C 侧不能凭空改。
        // 与其静默忽略标记，不如直接报出来。
        "option_niche" => {
            return Err(Diagnostic::new(
                tokens[*index].span,
                "#[option_niche] is not implemented yet: Option<*T> still uses the tagged layout",
            ))
        }
        other => {
            return Err(Diagnostic::new(
                tokens[*index].span,
                format!("unknown instance marker '#[{other}]'"),
            ))
        }
    };
    *index += 1;
    if *index >= tokens.len() || !is_punct(tokens[*index], "]") {
        return Err(Diagnostic::new(
            tokens[*index - 1].span,
            "expected ']' after the niche marker",
        ));
    }
    *index += 1;
    Ok(Some(niche))
}

/// Parses an instance table from a lexed token stream.
///
/// `tokens` may contain [`TokenKind::Newline`] markers. `names` carries the
/// file-scope type names of the translation unit the table belongs to, so a
/// type argument is recognized as a type and not as an expression.
pub fn parse_table(tokens: &[Token], names: &TypeNames) -> Result<Table, Diagnostic> {
    let tokens: Vec<&Token> = tokens
        .iter()
        .filter(|token| !matches!(token.kind, TokenKind::Newline | TokenKind::Eof))
        .collect();
    let mut entries = Vec::new();
    let mut aliases: HashSet<String> = HashSet::new();
    let mut index = 0;
    while index < tokens.len() {
        // An entry may be empty between two separators; a trailing separator
        // is the common case and needs no extra rule.
        if is_separator(tokens[index]) {
            index += 1;
            continue;
        }
        let start = tokens[index].span;
        let niche = match parse_niche_marker(&tokens, &mut index) {
            Ok(niche) => niche,
            Err(error) => return Err(error),
        };
        if let Some(error) = reject_qualified_entry(&tokens, index) {
            return Err(Diagnostic::new(start, error));
        }
        let Some(name) = token_ident(tokens[index]) else {
            return Err(Diagnostic::new(
                tokens[index].span,
                "expected a generic record, enum, or function name",
            ));
        };
        let name = Name::new(name, tokens[index].span);
        index += 1;
        if index >= tokens.len() || !is_punct(tokens[index], "<") {
            return Err(Diagnostic::new(
                tokens[index.saturating_sub(1)].span,
                format!(
                    "expected a generic argument list after '{}', as in 'Vec<int> as vec_int'",
                    name.text
                ),
            ));
        }
        let open = index;
        let close = matching_angle(&tokens, open).ok_or_else(|| {
            Diagnostic::new(
                tokens[open].span,
                "unterminated generic argument list in the instance table",
            )
        })?;
        let slice: Vec<Token> = tokens[open..=close]
            .iter()
            .map(|token| (*token).clone())
            .collect();
        let args = crate::parser::parse_generic_args(&slice, names)?;
        index = close + 1;
        if index < tokens.len() && is_punct(tokens[index], "::") {
            return Err(Diagnostic::new(
                tokens[index].span,
                "trait-qualified instance exports are not implemented yet: name a concrete \
                 generic record, enum, or function, as in 'Vec<int> as vec_int'",
            ));
        }
        if index >= tokens.len() || token_ident(tokens[index]).is_none_or(|word| word != "as") {
            return Err(Diagnostic::new(
                tokens
                    .get(index)
                    .map_or(tokens[close].span, |token| token.span),
                "expected 'as <alias>' after the generic instance",
            ));
        }
        index += 1;
        let Some(alias) = tokens.get(index).and_then(|token| token_ident(token)) else {
            return Err(Diagnostic::new(
                tokens
                    .get(index)
                    .map_or(tokens[close].span, |token| token.span),
                "expected a C identifier after 'as'",
            ));
        };
        let alias_span = tokens[index].span;
        index += 1;
        if index < tokens.len() && is_punct(tokens[index], "::") {
            return Err(Diagnostic::new(
                alias_span,
                "trait-qualified instance exports are not implemented yet: the alias must be \
                 one plain C identifier",
            ));
        }
        validate_alias(alias, alias_span)?;
        if !aliases.insert(alias.to_string()) {
            return Err(Diagnostic::new(
                alias_span,
                format!("duplicate instance alias '{alias}'"),
            ));
        }
        if index < tokens.len() && !is_separator(tokens[index]) {
            return Err(Diagnostic::new(
                tokens[index].span,
                "expected ',' or ';' after an instance table entry",
            ));
        }
        entries.push(Entry {
            niche,
            name,
            args,
            alias: alias.to_string(),
            span: start.join(alias_span),
        });
    }
    Ok(Table { entries })
}

/// One separator between instance table entries.
fn is_separator(token: &Token) -> bool {
    is_punct(token, ",") || is_punct(token, ";")
}

fn is_punct(token: &Token, punct: &str) -> bool {
    matches!(&token.kind, TokenKind::Punct(text) if text == punct)
}

fn token_ident(token: &Token) -> Option<&str> {
    match &token.kind {
        TokenKind::Ident(text) => Some(text),
        _ => None,
    }
}

/// Rejects a trait-qualified entry before it is parsed as a plain instance.
///
/// The supported form is `Name<args> as alias`. A trait qualifier (`Trait for
/// Name<args>`, `Name<args>::Trait`, or an `impl`/`trait` prefix) needs a trait
/// implementation table, which this export does not generate yet. Reporting it
/// here is better than exporting nothing and letting the linker fail.
fn reject_qualified_entry(tokens: &[&Token], start: usize) -> Option<String> {
    let mut depth = 0usize;
    let mut index = start;
    while index < tokens.len() {
        let token = tokens[index];
        if is_punct(token, "<") {
            depth += 1;
        } else if is_punct(token, ">") {
            depth = depth.saturating_sub(1);
        } else if is_punct(token, ">>") {
            depth = depth.saturating_sub(2);
        } else if depth == 0 {
            if token_ident(token).is_some_and(|word| word == "for") {
                return Some(
                    "trait-qualified instance exports are not implemented yet: name a concrete \
                     generic record, enum, or function, as in 'Vec<int> as vec_int'"
                        .to_string(),
                );
            }
            if is_punct(token, "::") {
                return Some(
                    "trait-qualified instance exports are not implemented yet: name a concrete \
                     generic record, enum, or function, as in 'Vec<int> as vec_int'"
                        .to_string(),
                );
            }
            if token_ident(token).is_some_and(|word| word == "as") {
                return None;
            }
            if token_ident(token).is_some_and(|word| word == "impl" || word == "trait") {
                return Some(
                    "trait-qualified instance exports are not implemented yet: name a concrete \
                     generic record, enum, or function, as in 'Vec<int> as vec_int'"
                        .to_string(),
                );
            }
        }
        if is_separator(token) && depth == 0 {
            return None;
        }
        index += 1;
    }
    None
}

/// The index of the `>` closing the `<` at `open`, counting nested angles.
///
/// A `>>` token closes two levels, which is how `Vec<Vec<int>>` is lexed.
fn matching_angle(tokens: &[&Token], open: usize) -> Option<usize> {
    let mut depth = 0usize;
    for (index, token) in tokens.iter().enumerate().skip(open) {
        if is_punct(token, "<") {
            depth += 1;
        } else if is_punct(token, ">") {
            depth = depth.saturating_sub(1);
        } else if is_punct(token, ">>") {
            if depth <= 2 {
                return Some(index);
            }
            depth -= 2;
        }
        if depth == 0 {
            return Some(index);
        }
    }
    None
}

/// Checks the alias spelling and rejects the C keywords.
fn validate_alias(alias: &str, span: Span) -> Result<(), Diagnostic> {
    let mut characters = alias.chars();
    let valid_start = characters
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic() || first == '_');
    let valid_rest = characters.all(|rest| rest.is_ascii_alphanumeric() || rest == '_');
    if !valid_start || !valid_rest {
        return Err(Diagnostic::new(
            span,
            format!("instance alias '{alias}' is not a valid C identifier"),
        ));
    }
    if is_c_keyword(alias) {
        return Err(Diagnostic::new(
            span,
            format!("instance alias '{alias}' is a C keyword; choose another name"),
        ));
    }
    Ok(())
}

/// C keywords, including the C11 and common C23 spellings.
fn is_c_keyword(word: &str) -> bool {
    matches!(
        word,
        "auto"
            | "break"
            | "case"
            | "char"
            | "const"
            | "continue"
            | "default"
            | "do"
            | "double"
            | "else"
            | "enum"
            | "extern"
            | "float"
            | "for"
            | "goto"
            | "if"
            | "inline"
            | "int"
            | "long"
            | "register"
            | "restrict"
            | "return"
            | "short"
            | "signed"
            | "sizeof"
            | "static"
            | "struct"
            | "switch"
            | "typedef"
            | "union"
            | "unsigned"
            | "void"
            | "volatile"
            | "while"
            | "_Alignas"
            | "_Alignof"
            | "_Atomic"
            | "_Bool"
            | "_Complex"
            | "_Generic"
            | "_Imaginary"
            | "_Noreturn"
            | "_Static_assert"
            | "_Thread_local"
            | "alignas"
            | "alignof"
            | "bool"
            | "constexpr"
            | "false"
            | "nullptr"
            | "static_assert"
            | "thread_local"
            | "true"
            | "typeof"
            | "typeof_unqual"
    )
}

// ---------------------------------------------------------------------------
// C output
// ---------------------------------------------------------------------------

/// Prints the instance declarations for the generated C header.
///
/// The section is inserted inside the ordinary header's `extern "C"` block,
/// after the declarations of the translation unit, so a plain typedef declared
/// in the source is already visible. Concrete generic record and enum layouts
/// are printed here; plain records are printed by `--emit=c-header` itself.
pub fn render_header_section(lowered: &crate::lower::Lowered) -> String {
    let mut printer = CPrinter::new(lowered, false);
    printer
        .output
        .push_str("/* Explicit generic instances from the instance table. */\n\n");
    printer.layouts();
    printer.alias_typedefs();
    printer.prototypes();
    printer.output
}

/// Prints the C wrapper source for the exported instances.
///
/// Every C-facing alias is a one-line wrapper that calls the concrete exported
/// symbol, so two aliases of one instance share one implementation. The file
/// is self-contained: it repeats the layouts it needs instead of including the
/// generated header, because the two outputs may be written to unrelated
/// paths.
pub fn render_wrapper(lowered: &crate::lower::Lowered) -> String {
    let mut printer = CPrinter::new(lowered, true);
    printer.output.push_str(
        "/* Generated by shyc --emit=c-wrapper.\n\
         *\n\
         * One wrapper per instance-table alias. Each wrapper calls the stable\n\
         * public symbol of the concrete instantiation; that symbol name is an\n\
         * internal mangling and is not part of the C interface. */\n\n",
    );
    printer.layouts();
    printer.concrete_prototypes();
    printer.wrappers();
    printer.output
}

/// One C declaration printer over the lowered type arena.
struct CPrinter<'a> {
    types: &'a TypeStore,
    names: &'a HashMap<TypeId, CTypeName>,
    exports: &'a [ExportedInstance],
    /// The wrapper repeats every layout; the header only prints the types that
    /// the ordinary AST export does not already spell.
    repeat_plain_layouts: bool,
    output: String,
}

impl<'a> CPrinter<'a> {
    fn new(lowered: &'a crate::lower::Lowered, repeat_plain_layouts: bool) -> CPrinter<'a> {
        CPrinter {
            types: &lowered.types,
            names: &lowered.c_types,
            exports: &lowered.exports,
            repeat_plain_layouts,
            output: String::new(),
        }
    }

    /// Prints forward declarations and definitions of every record and enum an
    /// exported signature mentions.
    fn layouts(&mut self) {
        let mut needed: Vec<TypeId> = Vec::new();
        let mut seen = HashSet::new();
        let mut work: Vec<TypeId> = Vec::new();
        for export in self.exports {
            match &export.kind {
                ExportedKind::Record { ty, methods } => {
                    work.push(*ty);
                    for method in methods {
                        work.push(method.ty);
                    }
                }
                ExportedKind::Enum { ty } | ExportedKind::Function { ty, .. } => work.push(*ty),
            }
        }
        while let Some(ty) = work.pop() {
            if !seen.insert(ty) {
                continue;
            }
            if self.emit_layout(ty) && !needed.contains(&ty) {
                needed.push(ty);
            }
            match self.types.kind(ty) {
                TypeKind::Record { .. } => {
                    if let Some(info) = self.types.record_info(ty) {
                        for field in &info.fields {
                            work.push(field.ty.ty);
                        }
                    }
                }
                TypeKind::Enum(_) | TypeKind::PayloadEnum(_) => {
                    if let Some(info) = self.types.enum_info(ty) {
                        work.push(info.underlying);
                        for variant in &info.variants {
                            if let Some(payload) = variant.payload {
                                work.push(payload);
                            }
                        }
                    }
                }
                TypeKind::Pointer { pointee } => work.push(pointee.ty),
                TypeKind::Array { element, .. } => work.push(element.ty),
                TypeKind::Function { result, params, .. } => {
                    work.push(*result);
                    work.extend(params.iter().copied());
                }
                _ => {}
            }
        }
        if needed.is_empty() {
            return;
        }
        // Forward declarations first, so a record may point at one defined
        // later and a recursive instance needs no special case.
        let mut forward = String::new();
        for &ty in &needed {
            match self.types.kind(ty) {
                TypeKind::Record { .. } => {
                    if let Some(tag) = self.record_tag(ty) {
                        forward.push_str(&format!("{tag};\n"));
                    } else if self.typedef_named(ty) {
                        let tag = self.synthetic_tag(ty);
                        forward.push_str(&format!("typedef {tag} {};\n", self.base(ty)));
                    }
                }
                TypeKind::PayloadEnum(_) => {
                    if self.types.enum_info(ty).is_some_and(|info| info.niche) {
                        continue;
                    }
                    if let Some(tag) = self.base(ty).strip_prefix("struct ") {
                        forward.push_str(&format!("struct {tag};\n"));
                    }
                }
                _ => {}
            }
        }
        self.output.push_str(&forward);
        self.output.push('\n');
        // Definitions in dependency order: a by-value member needs its type
        // complete, while a pointer member is satisfied above.
        for ty in self.layout_order(&needed) {
            self.layout_definition(ty);
        }
    }

    /// Whether this output must print `ty`'s layout itself.
    fn emit_layout(&self, ty: TypeId) -> bool {
        match self.types.kind(ty) {
            TypeKind::Record { .. } | TypeKind::Enum(_) | TypeKind::PayloadEnum(_) => {
                self.repeat_plain_layouts
                    || !self.names.contains_key(&ty)
                    || self.names.get(&ty).is_some_and(|name| name.emit_header)
            }
            _ => false,
        }
    }

    /// Orders `types` so a by-value member's type is defined first.
    fn layout_order(&self, types: &[TypeId]) -> Vec<TypeId> {
        let mut order = Vec::new();
        let mut visited = HashSet::new();
        for &ty in types {
            self.visit_layout(ty, types, &mut visited, &mut order);
        }
        order
    }

    fn visit_layout(
        &self,
        ty: TypeId,
        types: &[TypeId],
        visited: &mut HashSet<TypeId>,
        order: &mut Vec<TypeId>,
    ) {
        if !visited.insert(ty) {
            return;
        }
        for dependency in self.value_dependencies(ty) {
            if types.contains(&dependency) {
                self.visit_layout(dependency, types, visited, order);
            }
        }
        order.push(ty);
    }

    /// Types that must be complete before `ty`'s layout: by-value members and
    /// payloads, not pointer or function-parameter types.
    fn value_dependencies(&self, ty: TypeId) -> Vec<TypeId> {
        let mut dependencies = Vec::new();
        match self.types.kind(ty) {
            TypeKind::Record { .. } => {
                if let Some(info) = self.types.record_info(ty) {
                    for field in &info.fields {
                        self.value_dependency(field.ty.ty, &mut dependencies);
                    }
                }
            }
            TypeKind::PayloadEnum(_) => {
                if let Some(info) = self.types.enum_info(ty) {
                    for variant in &info.variants {
                        if let Some(payload) = variant.payload {
                            self.value_dependency(payload, &mut dependencies);
                        }
                    }
                }
            }
            _ => {}
        }
        dependencies
    }

    fn value_dependency(&self, ty: TypeId, dependencies: &mut Vec<TypeId>) {
        match self.types.kind(ty) {
            TypeKind::Record { .. } | TypeKind::Enum(_) | TypeKind::PayloadEnum(_) => {
                dependencies.push(ty);
            }
            TypeKind::Array { element, .. } => {
                self.value_dependency(element.ty, dependencies);
            }
            _ => {}
        }
    }

    /// Prints the C definition of one record or enum layout.
    fn layout_definition(&mut self, ty: TypeId) {
        match self.types.kind(ty) {
            TypeKind::Record { .. } => {
                // A forward-declared record stays opaque; the forward
                // declaration above is all a pointer signature needs.
                if self.types.get(ty).size.is_none() {
                    return;
                }
                let head = self
                    .record_tag(ty)
                    .unwrap_or_else(|| self.synthetic_tag(ty));
                let mut fields = String::new();
                if let Some(info) = self.types.record_info(ty) {
                    for field in &info.fields {
                        let name = field.name.as_deref().unwrap_or("");
                        fields.push_str(&format!("    {};\n", self.declaration(field.ty.ty, name)));
                    }
                }
                self.output.push_str(&format!("{head} {{\n{fields}}};\n\n"));
            }
            TypeKind::PayloadEnum(_) => {
                if self.types.enum_info(ty).is_some_and(|info| info.niche) {
                    // 底层整数已经由名字注册点表达，这里没有结构体可打印。
                    return;
                }
                let base = self.base(ty);
                let tag = base
                    .strip_prefix("struct ")
                    .map(str::to_string)
                    .unwrap_or_else(|| format!("shyc_E{}", ty.index()));
                let Some(info) = self.types.enum_info(ty) else {
                    return;
                };
                let tag_type = self.declaration(info.underlying, "");
                let mut payloads = String::new();
                for variant in &info.variants {
                    if let Some(payload) = variant.payload {
                        payloads.push_str(&format!(
                            "        {} {};\n",
                            self.declaration(payload, ""),
                            variant.name
                        ));
                    }
                }
                self.output.push_str(&format!(
                    "struct {tag} {{\n    {tag_type} tag;\n    union {{\n{payloads}    }} payload;\n}};\n\n"
                ));
            }
            TypeKind::Enum(_) => {
                let Some(info) = self.types.enum_info(ty) else {
                    return;
                };
                let underlying = self.declaration(info.underlying, "");
                let base = self.base(ty);
                self.output
                    .push_str(&format!("typedef {underlying} {base};\n\n"));
            }
            _ => {}
        }
    }

    /// Prints `typedef <concrete> <alias>;` for every requested record and
    /// enum.
    fn alias_typedefs(&mut self) {
        let mut text = String::new();
        for export in self.exports {
            match &export.kind {
                ExportedKind::Record { ty, .. } | ExportedKind::Enum { ty } => {
                    text.push_str(&format!("typedef {} {};\n", self.base(*ty), export.alias));
                }
                ExportedKind::Function { .. } => {}
            }
        }
        if !text.is_empty() {
            self.output.push_str(&text);
            self.output.push('\n');
        }
    }

    /// Prints the C prototypes of the table-defined names (header).
    fn prototypes(&mut self) {
        let mut text = String::new();
        for export in self.exports {
            match &export.kind {
                ExportedKind::Record { methods, .. } => {
                    for method in methods {
                        text.push_str(&format!(
                            "{};\n",
                            self.declaration(
                                method.ty,
                                &format!("{}_{}", export.alias, method.name)
                            )
                        ));
                    }
                }
                ExportedKind::Enum { .. } => {}
                ExportedKind::Function { ty, .. } => {
                    text.push_str(&format!("{};\n", self.declaration(*ty, &export.alias)));
                }
            }
        }
        if !text.is_empty() {
            self.output.push_str(&text);
            self.output.push('\n');
        }
    }

    /// Prints declarations of the concrete symbols the wrappers call
    /// (wrapper).
    fn concrete_prototypes(&mut self) {
        let mut text = String::new();
        let mut declared = HashSet::new();
        for export in self.exports {
            match &export.kind {
                ExportedKind::Record { methods, .. } => {
                    for method in methods {
                        if declared.insert(method.symbol.as_str()) {
                            text.push_str(&format!(
                                "{};\n",
                                self.declaration(method.ty, &method.symbol)
                            ));
                        }
                    }
                }
                ExportedKind::Enum { .. } => {}
                ExportedKind::Function { ty, symbol } => {
                    if declared.insert(symbol.as_str()) {
                        text.push_str(&format!("{};\n", self.declaration(*ty, symbol)));
                    }
                }
            }
        }
        if !text.is_empty() {
            self.output.push_str(&text);
            self.output.push('\n');
        }
    }

    /// Prints one wrapper function per C-facing alias (wrapper).
    fn wrappers(&mut self) {
        for export in self.exports {
            match &export.kind {
                ExportedKind::Record { methods, .. } => {
                    for method in methods {
                        let name = format!("{}_{}", export.alias, method.name);
                        self.wrapper_function(method.ty, &name, &method.symbol);
                    }
                }
                ExportedKind::Enum { .. } => {}
                ExportedKind::Function { ty, symbol } => {
                    self.wrapper_function(*ty, &export.alias, symbol);
                }
            }
        }
    }

    /// Prints `<result> <name>(<params>) { return <symbol>(<args>); }`.
    fn wrapper_function(&mut self, ty: TypeId, name: &str, symbol: &str) {
        let TypeKind::Function { result, params, .. } = self.types.kind(ty) else {
            return;
        };
        let result = *result;
        let parameters = params
            .iter()
            .enumerate()
            .map(|(index, param)| self.declaration(*param, &format!("p{index}")))
            .collect::<Vec<_>>()
            .join(", ");
        let arguments = (0..params.len())
            .map(|index| format!("p{index}"))
            .collect::<Vec<_>>()
            .join(", ");
        let call = format!("{symbol}({arguments})");
        let result_text = self.declaration(result, "");
        self.output
            .push_str(&format!("{result_text} {name}({parameters}) {{\n"));
        if self.types.is_void(result) {
            self.output.push_str(&format!("    {call};\n"));
        } else {
            self.output.push_str(&format!("    return {call};\n"));
        }
        self.output.push_str("}\n\n");
    }

    /// The tag a record layout is defined under, or `None` for a record whose
    /// base is a C typedef name.
    fn record_tag(&self, ty: TypeId) -> Option<String> {
        let base = self.base(ty);
        if base.starts_with("struct ") || base.starts_with("union ") {
            return Some(base);
        }
        None
    }

    /// Whether a record is spelled by a C typedef name that this output must
    /// declare before it can be used to refer to the record.
    fn typedef_named(&self, ty: TypeId) -> bool {
        self.record_tag(ty).is_none()
    }

    /// A tag for a record whose C spelling is a typedef name.
    fn synthetic_tag(&self, ty: TypeId) -> String {
        format!("struct shyc_T{}", ty.index())
    }

    /// The base spelling of one record or enum type.
    fn base(&self, ty: TypeId) -> String {
        if let Some(name) = self.names.get(&ty) {
            return name.base.clone();
        }
        match self.types.kind(ty) {
            TypeKind::Record { kind, .. } => {
                let keyword = match kind {
                    RecordKind::Struct => "struct",
                    RecordKind::Union => "union",
                };
                format!("{keyword} shyc_R{}", ty.index())
            }
            TypeKind::Enum(_) => format!("shyc_E{}", ty.index()),
            TypeKind::PayloadEnum(_) => format!("struct shyc_E{}", ty.index()),
            _ => String::new(),
        }
    }

    /// Prints a full C declaration of `ty` with the given name.
    fn declaration(&self, ty: TypeId, name: &str) -> String {
        let (base, declarator) =
            self.apply(ty, crate::sema::types::Qualifiers::NONE, name.to_string());
        if declarator.is_empty() {
            base
        } else {
            format!("{base} {declarator}")
        }
    }

    /// Applies one type constructor to an already-built declarator.
    fn apply(
        &self,
        ty: TypeId,
        qualifiers: crate::sema::types::Qualifiers,
        declarator: String,
    ) -> (String, String) {
        match self.types.kind(ty) {
            TypeKind::Pointer { pointee } => {
                let mut inner = if qualifiers.is_empty() {
                    format!("*{declarator}")
                } else {
                    format!("*{} {declarator}", qualifiers_text(qualifiers))
                };
                if matches!(
                    self.types.kind(pointee.ty),
                    TypeKind::Array { .. } | TypeKind::Function { .. }
                ) {
                    inner = format!("({inner})");
                }
                self.apply(pointee.ty, pointee.qualifiers, inner)
            }
            TypeKind::Array { element, len } => {
                let suffix = match len {
                    Some(length) => format!("[{length}]"),
                    None => "[]".to_string(),
                };
                self.apply(
                    element.ty,
                    element.qualifiers,
                    format!("{declarator}{suffix}"),
                )
            }
            TypeKind::Function {
                result,
                params,
                variadic,
                prototype,
            } => {
                let mut parts = params
                    .iter()
                    .map(|param| self.declaration(*param, ""))
                    .collect::<Vec<_>>();
                if *variadic {
                    parts.push("...".to_string());
                }
                let parameters = if parts.is_empty() {
                    if *prototype {
                        "void".to_string()
                    } else {
                        String::new()
                    }
                } else {
                    parts.join(", ")
                };
                self.apply(
                    *result,
                    crate::sema::types::Qualifiers::NONE,
                    format!("{declarator}({parameters})"),
                )
            }
            _ => (self.scalar_base(ty, qualifiers), declarator),
        }
    }

    /// The spelling of a scalar type, with its top-level qualifiers.
    fn scalar_base(&self, ty: TypeId, qualifiers: crate::sema::types::Qualifiers) -> String {
        use crate::sema::types::IntegerRank;
        let base = match self.types.kind(ty) {
            TypeKind::Void => "void".to_string(),
            TypeKind::Bool => "_Bool".to_string(),
            TypeKind::Float32 => "float".to_string(),
            TypeKind::Float64 => "double".to_string(),
            TypeKind::Integer { rank, unsigned } => {
                let base = match rank {
                    IntegerRank::Char => {
                        if *unsigned {
                            "unsigned char"
                        } else {
                            // `char` is the spelling C clients already have;
                            // ShyC's signed char and plain char are one type.
                            "char"
                        }
                    }
                    IntegerRank::Short => {
                        if *unsigned {
                            "unsigned short"
                        } else {
                            "short"
                        }
                    }
                    IntegerRank::Int => {
                        if *unsigned {
                            "unsigned int"
                        } else {
                            "int"
                        }
                    }
                    IntegerRank::Long => {
                        if *unsigned {
                            "unsigned long"
                        } else {
                            "long"
                        }
                    }
                    IntegerRank::LongLong => {
                        if *unsigned {
                            "unsigned long long"
                        } else {
                            "long long"
                        }
                    }
                };
                base.to_string()
            }
            TypeKind::Record { .. } | TypeKind::Enum(_) | TypeKind::PayloadEnum(_) => self.base(ty),
            _ => "int".to_string(),
        };
        if qualifiers.is_empty() {
            base
        } else {
            format!("{} {base}", qualifiers_text(qualifiers))
        }
    }
}

fn qualifiers_text(qualifiers: crate::sema::types::Qualifiers) -> String {
    let mut words = Vec::new();
    if qualifiers.is_const {
        words.push("const");
    }
    if qualifiers.is_volatile {
        words.push("volatile");
    }
    if qualifiers.is_restrict {
        words.push("restrict");
    }
    if qualifiers.is_atomic {
        words.push("_Atomic");
    }
    words.join(" ")
}
