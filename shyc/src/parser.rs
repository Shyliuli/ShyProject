//! Handwritten recursive-descent parser for ShyC.
//!
//! Pipeline position: `source` -> `lexer` -> preprocessor -> **`parser`** ->
//! sema.
//!
//! The parser turns the preprocessed token stream into the syntax-only
//! [`ast::TranslationUnit`]. It does not resolve names, compute types, lay out
//! aggregates, insert drops, or generate code. It does keep the two name
//! scopes that *syntax* needs to disambiguate C:
//!
//! - **type names** (`typedef` names and `struct`/`union`/`enum` tags), so
//!   `Counter c;` and `(Counter *)p` parse as a declaration and a cast;
//! - **value names** (parameters, block locals, enumerators), so a local
//!   variable that shadows a typedef makes that spelling a value again.
//!
//! Shy predeclares top-level typedefs and tags for the whole translation unit,
//! including definitions written later in the file. [`decl::prescan`] collects
//! them in one cheap first pass; the full parse then runs with that knowledge.
//! Forward *calls* need no help at all: a call is just an unresolved name.
//!
//! Unsupported spellings become [`Diagnostic`]s; they are never skipped. The
//! list of constructs the tree deliberately omits lives in [`crate::ast`].
//!
//! Remaining syntax restrictions, reported honestly:
//!
//! - a block-local `typedef` must be declared before it is used, because only
//!   file-scope names are predeclared for the whole unit (this is ordinary C
//!   scoping, not a Shy pre-scan rule);
//! - a tag declared inside a struct/union body is registered in the enclosing
//!   syntax scope, while C ends its scope at the end of the body. The parser
//!   can therefore accept a few programs that semantic analysis rejects; it
//!   never loses a tag that a valid program may use.

mod comptime;
mod cursor;
mod decl;
mod expr;
mod init;
mod stmt;
mod ty;

pub use cursor::is_keyword;
pub use ty::builtin_type;

use std::collections::HashSet;

use crate::ast;
use crate::lexer::{Token, TokenKind};
use crate::source::{Diagnostic, FileId, Span};

/// Parses one preprocessed token stream into a syntax-only translation unit.
///
/// The stream may still contain [`crate::lexer::TokenKind::Newline`] tokens; the cursor
/// skips them. Exactly one [`crate::lexer::TokenKind::Eof`] is expected at the end.
/// A compile-time construct the parser consumed, kept for tools.
///
/// The parser replaces a const fn call and a name!(...) macro with their
/// expansion, so neither name survives in the AST. The index records them here
/// so hover and go-to-definition still work.
#[derive(Clone, Debug)]
pub struct ComptimeSite {
    pub kind: ComptimeSiteKind,
    pub name: String,
    /// The name as written at this site.
    pub span: Span,
    /// For a call, where the const fn it names is declared.
    pub declaration: Option<Span>,
    /// For a macro, the function its expansion calls.
    pub expansion: Option<String>,
    /// For a declaration, the text from const to the body, for hover.
    pub signature: Option<Span>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ComptimeSiteKind {
    /// A const fn declaration.
    Declaration,
    /// A call to a const fn, including a detached generator call.
    Call,
    /// A println!, format!, or scanln! invocation.
    Macro,
}

pub fn parse(tokens: &[Token]) -> Result<ast::TranslationUnit, Diagnostic> {
    parse_with_sites(tokens).map(|(unit, _)| unit)
}

/// Parses a unit and reports the compile-time sites it expanded.
pub fn parse_with_sites(
    tokens: &[Token],
) -> Result<(ast::TranslationUnit, Vec<ComptimeSite>), Diagnostic> {
    // First pass: collect declarations, run the generators, and learn the
    // final world. A function body may query that world, so when generators
    // exist the unit is parsed a second time with the world in hand.
    let global = decl::prescan(tokens);
    let mut first = Parser::new(tokens.to_vec(), false);
    first.scopes[0].typedefs = global.typedefs.clone();
    first.scopes[0].tags = global.tags.clone();
    first.scopes[0].traits = global.traits.clone();
    let mut unit = first.translation_unit()?;
    let sites = first.comptime_sites.clone();
    let Some(world) = first.final_world.take() else {
        return Ok((unit, sites));
    };
    // Nothing in a body asked a compile-time question, so the second pass would
    // produce the same unit.
    if !first.saw_body_call.get() {
        return Ok((unit, sites));
    }
    // Re-parse only the bodies that asked such a question; when that cannot be
    // done safely, fall back to parsing the whole unit again.
    if !first.needs_full_pass && first.reparse_bodies(tokens, &world, &mut unit) {
        return Ok((unit, sites));
    }
    let mut second = Parser::new(tokens.to_vec(), false);
    second.scopes[0].typedefs = global.typedefs;
    second.scopes[0].tags = global.tags;
    second.scopes[0].traits = global.traits;
    second.body_world = Some(world);
    let unit = second.translation_unit()?;
    Ok((unit, second.comptime_sites))
}

/// File-scope type names of an already parsed unit.
///
/// The explicit generic instance table ([`crate::export_instances`]) names a
/// generic template with arguments that are parsed after the unit exists. The
/// parser needs to know which identifiers spell types to tell a type argument
/// from a constant one, so this collects the same names the prescan would.
#[derive(Default)]
pub struct TypeNames {
    typedefs: HashSet<String>,
    tags: HashSet<String>,
    traits: HashSet<String>,
}

/// Collects the file-scope type names of `unit`.
pub fn type_names(unit: &ast::TranslationUnit) -> TypeNames {
    let mut names = TypeNames::default();
    for item in &unit.items {
        match item {
            ast::Item::Decl(declaration) => {
                collect_specs_type_names(&declaration.specs, &mut names);
                if declaration.specs.storage == Some(ast::StorageClass::Typedef) {
                    for declarator in &declaration.declarators {
                        if let Some(name) = &declarator.declarator.name {
                            names.typedefs.insert(name.text.clone());
                        }
                    }
                }
            }
            ast::Item::Function(function) => collect_specs_type_names(&function.specs, &mut names),
            ast::Item::Impl(_) | ast::Item::Use(_) => {}
            ast::Item::Trait(declaration) => {
                names.traits.insert(declaration.name.text.clone());
            }
        }
    }
    names
}

/// Collects every typedef name, tag, and trait a specifier list mentions.
fn collect_specs_type_names(specs: &ast::DeclSpecs, names: &mut TypeNames) {
    for specifier in &specs.type_specs {
        match specifier {
            ast::TypeSpecifier::TypedefName(name) => {
                names.typedefs.insert(name.text.clone());
            }
            ast::TypeSpecifier::Generic(generic) => {
                names.typedefs.insert(generic.name.text.clone());
                for argument in &generic.args {
                    if let ast::GenericArgKind::Type(type_name) = &argument.kind {
                        collect_type_name_names(type_name, names);
                    }
                }
            }
            ast::TypeSpecifier::Record(record) => {
                if let Some(tag) = &record.tag {
                    names.tags.insert(tag.text.clone());
                }
                for member in record.members.iter().flatten() {
                    collect_specs_type_names(&member.specs, names);
                }
            }
            ast::TypeSpecifier::Enum(enumeration) => {
                if let Some(tag) = &enumeration.tag {
                    names.tags.insert(tag.text.clone());
                }
                for enumerator in enumeration.enumerators.iter().flatten() {
                    if let Some(payload) = &enumerator.payload {
                        collect_type_name_names(payload, names);
                    }
                }
            }
            ast::TypeSpecifier::Atomic(type_name) => collect_type_name_names(type_name, names),
            ast::TypeSpecifier::Builtin(_) | ast::TypeSpecifier::SelfType => {}
        }
    }
}

fn collect_type_name_names(type_name: &ast::TypeName, names: &mut TypeNames) {
    collect_specs_type_names(&type_name.specs, names);
}

/// Parses a standalone `<...>` generic argument list with the file-scope type
/// names of an already parsed unit in scope.
///
/// The explicit instance table is not a translation unit, but its arguments
/// are exactly the generic arguments a use site writes. Reusing the parser
/// keeps nested types and constant expressions spelled the same way instead of
/// a second, weaker grammar. The slice starts with `<`; trailing tokens are
/// rejected.
pub fn parse_generic_args(
    tokens: &[Token],
    names: &TypeNames,
) -> Result<Vec<ast::GenericArg>, Diagnostic> {
    let mut end = tokens.to_vec();
    if !matches!(end.last().map(|token| &token.kind), Some(TokenKind::Eof)) {
        let span = end.last().map_or(
            Span {
                file: FileId(0),
                start: 0,
                end: 0,
            },
            |token| token.span,
        );
        end.push(Token {
            kind: TokenKind::Eof,
            span,
            at_line_start: false,
            leading_space: false,
        });
    }
    let mut parser = Parser::new(end, false);
    parser.scopes[0].typedefs = names.typedefs.clone();
    parser.scopes[0].tags = names.tags.clone();
    parser.scopes[0].traits = names.traits.clone();
    let args = parser.generic_args()?;
    if !parser.at_eof() {
        return Err(Diagnostic::new(
            parser.here(),
            "unexpected tokens after a generic argument list",
        ));
    }
    Ok(args)
}

/// Parser state shared by every grammar file.
pub(crate) struct Parser {
    tokens: Vec<Token>,
    /// Index of the next token; may point at a `Newline` the cursor skips.
    pos: usize,
    /// Span of the most recently consumed token.
    last: Span,
    /// Nesting depth of a generic argument list being parsed. Inside one, `>`
    /// and `>=` are closers, not comparison operators.
    angle_depth: usize,
    /// `#[result_niche]` seen before the item currently being parsed.
    pending_enum_niche: bool,
    /// `#[option_niche]` seen before the item currently being parsed.
    pending_option_niche: bool,
    /// Name scopes, innermost last. `scopes[0]` is file scope.
    pub(crate) scopes: Vec<Scope>,
    /// Nesting depth of `impl` and `trait` bodies; `self` is a type only
    /// inside one.
    impl_depth: usize,
    /// Prescan mode: unknown identifiers count as type names, and function
    /// bodies, aggregate bodies, and initializers are skipped.
    prescan: bool,
    /// `const fn name(...) { ... }` declarations seen so far, in source order.
    pub(crate) const_fns: Vec<comptime::ConstFn>,
    /// Declarations a function body may query: the world left after the
    /// generators settled. Absent during the first pass, which computes it.
    pub(crate) body_world: Option<World>,
    /// The world this pass produced, kept so a second pass can use it.
    pub(crate) final_world: Option<World>,
    /// Set when a function body evaluated a const fn during this pass, which is
    /// the only case that needs a second look.
    pub(crate) saw_body_call: std::cell::Cell<bool>,
    /// Set when re-parsing a fragment changed its shape, so the whole unit must
    /// be parsed again instead.
    pub(crate) needs_full_pass: bool,
    /// Compile-time sites seen so far, for tools that need the syntax the
    /// expansion removed.
    pub(crate) comptime_sites: Vec<ComptimeSite>,
    /// Span of the body being parsed, innermost last.
    pub(crate) body_stack: Vec<Span>,
    /// Span of the function body being parsed, innermost last. A call inside a
    /// nested block still belongs to this body for re-parsing.
    pub(crate) function_bodies: Vec<Span>,
    /// Bodies that contain a const fn call: only these need re-parsing once the
    /// declaration world is settled.
    pub(crate) body_calls: std::cell::RefCell<Vec<Span>>,
    /// Declarator produced by a type-position expansion, waiting to be joined
    /// with the declarator the declaration writes after it.
    pub(crate) pending_declarator: Option<ast::Declarator>,
}

/// Records the type names a specifier list mentions, so a later generator can
/// query them with `declared`.
fn collect_spec_names(specs: &ast::DeclSpecs, world: &mut World) {
    for specifier in &specs.type_specs {
        match specifier {
            ast::TypeSpecifier::TypedefName(name) => {
                world.names.insert(name.text.clone());
            }
            ast::TypeSpecifier::Record(record) => {
                if let Some(tag) = &record.tag {
                    world.names.insert(tag.text.clone());
                }
            }
            ast::TypeSpecifier::Enum(enumeration) => {
                if let Some(tag) = &enumeration.tag {
                    world.names.insert(tag.text.clone());
                }
            }
            _ => {}
        }
    }
}

/// One `impl Trait for Type` declaration as the world indexes it.
struct ImplDecl {
    trait_name: String,
    owner: String,
    /// One entry per impl parameter: the trait bounds written on it.
    bounds: Vec<Vec<String>>,
    /// True when the owner was written with arguments, as in `Vec<T>`.
    owner_has_args: bool,
}

impl ImplDecl {
    fn is_generic(&self) -> bool {
        self.owner_has_args || !self.bounds.is_empty()
    }
}

/// Reads an `impl` block into the shape the world stores.
fn impl_declaration(block: &ast::ImplBlock) -> Option<ImplDecl> {
    let trait_name = trait_key(block.trait_name.as_ref()?, &block.trait_args);
    let mut owner = None;
    let mut owner_has_args = false;
    let mut builtin = BuiltinName::default();
    for specifier in &block.self_type.specs.type_specs {
        match specifier {
            ast::TypeSpecifier::TypedefName(name) => owner = Some(name.text.clone()),
            ast::TypeSpecifier::Record(record) => {
                owner = record.tag.as_ref().map(|tag| tag.text.clone());
            }
            ast::TypeSpecifier::Generic(generic_spec) => {
                owner = Some(generic_spec.name.text.clone());
                owner_has_args = true;
            }
            // `impl Show for int` names a builtin type, which has no tag.
            ast::TypeSpecifier::Builtin(keyword) => builtin.add(*keyword),
            _ => {}
        }
    }
    if owner.is_none() {
        owner = builtin.spelling();
    }
    let bounds = block
        .generics
        .iter()
        .map(|parameter| {
            parameter
                .bounds
                .iter()
                .map(|bound| bound.name.text.clone())
                .collect()
        })
        .collect();
    Some(ImplDecl {
        trait_name,
        owner: owner?,
        bounds,
        owner_has_args,
    })
}

/// Normalizes a trait name written as a query string.
///
/// Spaces are removed and numeric arguments are canonicalized, so
/// `implements("Table", "At<0x10>")` asks about the same instantiation as
/// `At<16>`.
fn normalize_trait_query(text: &str) -> String {
    let text = text.replace(' ', "");
    let Some(open) = text.find('<') else {
        return text;
    };
    let Some(close) = text.rfind('>') else {
        return text;
    };
    let base = &text[..open];
    let inner = &text[open + 1..close];
    let args = inner
        .split(',')
        .map(|argument| {
            integer_value(argument).map_or_else(|| argument.to_string(), |value| value.to_string())
        })
        .collect::<Vec<_>>()
        .join(",");
    format!("{base}<{args}>")
}
/// Canonical text of a trait name with its arguments, spaces removed.
///
/// `impl Show<int> for X` and `implements("X", "Show<int>")` must agree, so both
/// sides go through this function.
fn trait_key(name: &ast::Name, args: &[ast::GenericArg]) -> String {
    if args.is_empty() {
        return name.text.clone();
    }
    let rendered = args
        .iter()
        .map(generic_arg_text)
        .collect::<Vec<_>>()
        .join(",");
    format!("{}<{rendered}>", name.text)
}

fn generic_arg_text(argument: &ast::GenericArg) -> String {
    match &argument.kind {
        ast::GenericArgKind::Type(type_name) => type_name_text(type_name),
        ast::GenericArgKind::Const(expression) => const_arg_text(expression),
    }
}

/// Canonical text of a constant generic argument.
///
/// Numbers are normalized, so `Convert<0x10>` and `Convert<16>` are the same
/// instantiation; a character argument uses its byte value.
fn const_arg_text(expression: &ast::Expr) -> String {
    match expression {
        ast::Expr::Number(literal) => integer_value(&literal.text)
            .map_or_else(|| literal.text.clone(), |value| value.to_string()),
        ast::Expr::Char(literal) => literal
            .raw
            .as_bytes()
            .first()
            .map_or_else(String::new, |byte| i64::from(*byte).to_string()),
        ast::Expr::Name(name) => name.text.clone(),
        ast::Expr::Unary {
            op: ast::UnaryOp::Minus,
            operand,
            ..
        } => match operand.as_ref() {
            ast::Expr::Number(literal) => {
                integer_value(&literal.text).map_or_else(String::new, |value| (-value).to_string())
            }
            _ => String::new(),
        },
        _ => String::new(),
    }
}

/// The integer a literal spells, ignoring any suffix.
fn integer_value(text: &str) -> Option<i64> {
    let trimmed = text.trim_end_matches(|character: char| character.is_ascii_alphabetic());
    if let Some(hex) = trimmed
        .strip_prefix("0x")
        .or_else(|| trimmed.strip_prefix("0X"))
    {
        return i64::from_str_radix(hex, 16).ok();
    }
    if let Some(binary) = trimmed
        .strip_prefix("0b")
        .or_else(|| trimmed.strip_prefix("0B"))
    {
        return i64::from_str_radix(binary, 2).ok();
    }
    trimmed.parse::<i64>().ok()
}

/// Best-effort spelling of a type name, used only as an index key.
fn type_name_text(name: &ast::TypeName) -> String {
    let mut parts = Vec::new();
    for specifier in &name.specs.type_specs {
        parts.push(match specifier {
            ast::TypeSpecifier::TypedefName(inner) => inner.text.clone(),
            ast::TypeSpecifier::Record(record) => record
                .tag
                .as_ref()
                .map(|tag| tag.text.clone())
                .unwrap_or_default(),
            ast::TypeSpecifier::Enum(enumeration) => enumeration
                .tag
                .as_ref()
                .map(|tag| tag.text.clone())
                .unwrap_or_default(),
            ast::TypeSpecifier::Builtin(builtin) => builtin_word(*builtin).to_string(),
            ast::TypeSpecifier::Generic(generic) => {
                let args = generic
                    .args
                    .iter()
                    .map(generic_arg_text)
                    .collect::<Vec<_>>()
                    .join(",");
                format!("{}<{args}>", generic.name.text)
            }
            ast::TypeSpecifier::Atomic(inner) => type_name_text(inner),
            _ => String::new(),
        });
    }
    let mut text = parts.join(" ").replace(' ', "");
    if let Some(declarator) = &name.declarator {
        for derived in &declarator.derived {
            if matches!(derived, ast::DerivedDeclarator::Pointer(_)) {
                text.push('*');
            }
        }
    }
    text
}

fn builtin_word(builtin: ast::BuiltinType) -> &'static str {
    match builtin {
        ast::BuiltinType::Void => "void",
        ast::BuiltinType::Bool => "bool",
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
/// The builtin type keywords of one declaration, in canonical form.
///
/// `unsigned int`, `int unsigned` and `unsigned` are the same type, so the
/// world indexes one spelling for all of them.
#[derive(Default)]
struct BuiltinName {
    void_: bool,
    bool_: bool,
    char_: bool,
    short_: bool,
    int_: bool,
    longs: usize,
    float_: bool,
    double_: bool,
    signed: Option<bool>,
}

impl BuiltinName {
    fn add(&mut self, keyword: ast::BuiltinType) {
        match keyword {
            ast::BuiltinType::Void => self.void_ = true,
            ast::BuiltinType::Bool => self.bool_ = true,
            ast::BuiltinType::Char => self.char_ = true,
            ast::BuiltinType::Short => self.short_ = true,
            ast::BuiltinType::Int => self.int_ = true,
            ast::BuiltinType::Long => self.longs += 1,
            ast::BuiltinType::Float => self.float_ = true,
            ast::BuiltinType::Double => self.double_ = true,
            ast::BuiltinType::Signed => self.signed = Some(true),
            ast::BuiltinType::Unsigned => self.signed = Some(false),
        }
    }

    fn is_empty(&self) -> bool {
        !(self.void_
            || self.bool_
            || self.char_
            || self.short_
            || self.int_
            || self.longs > 0
            || self.float_
            || self.double_)
    }

    /// The canonical spelling: `int` for `signed int`, `unsigned long` for
    /// `long unsigned`, and so on.
    fn spelling(&self) -> Option<String> {
        if self.is_empty() {
            return None;
        }
        if self.void_ {
            return Some("void".to_string());
        }
        if self.bool_ {
            return Some("bool".to_string());
        }
        if self.float_ {
            return Some("float".to_string());
        }
        if self.double_ {
            return Some(if self.longs > 0 {
                "long double".to_string()
            } else {
                "double".to_string()
            });
        }
        if self.char_ {
            return Some(match self.signed {
                Some(false) => "unsigned char".to_string(),
                Some(true) => "signed char".to_string(),
                None => "char".to_string(),
            });
        }
        let base = if self.longs >= 2 {
            "long long"
        } else if self.longs == 1 {
            "long"
        } else if self.short_ {
            "short"
        } else {
            "int"
        };
        Some(match self.signed {
            Some(false) => format!("unsigned {base}"),
            _ => base.to_string(),
        })
    }
}

/// Canonical spelling of a type name written as a string.
///
/// A query such as `implements("int unsigned", "Show")` is normalized with the
/// same rules the declarations use.
fn normalize_type_name(text: &str) -> String {
    let text = text.trim();
    if text.contains('<') {
        return text.to_string();
    }
    let mut builtin = BuiltinName::default();
    for word in text.split_whitespace() {
        match word {
            "void" => builtin.add(ast::BuiltinType::Void),
            "bool" | "_Bool" => builtin.add(ast::BuiltinType::Bool),
            "char" => builtin.add(ast::BuiltinType::Char),
            "short" => builtin.add(ast::BuiltinType::Short),
            "int" => builtin.add(ast::BuiltinType::Int),
            "long" => builtin.add(ast::BuiltinType::Long),
            "float" => builtin.add(ast::BuiltinType::Float),
            "double" => builtin.add(ast::BuiltinType::Double),
            "signed" => builtin.add(ast::BuiltinType::Signed),
            "unsigned" => builtin.add(ast::BuiltinType::Unsigned),
            _ => return text.to_string(),
        }
    }
    builtin.spelling().unwrap_or_else(|| text.to_string())
}
/// Splits `Vec<int, char>` into its base name and top-level arguments.
fn split_type_name(text: &str) -> (String, Vec<String>) {
    let text = text.trim();
    let Some(open) = text.find('<') else {
        return (text.to_string(), Vec::new());
    };
    let base = text[..open].trim().to_string();
    let inner = text[open + 1..].trim_end_matches('>');
    let mut args = Vec::new();
    let mut depth = 0usize;
    let mut start = 0usize;
    for (index, character) in inner.char_indices() {
        match character {
            '<' | '(' | '[' => depth += 1,
            '>' | ')' | ']' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                args.push(inner[start..index].trim().to_string());
                start = index + 1;
            }
            _ => {}
        }
    }
    let last = inner[start..].trim();
    if !last.is_empty() {
        args.push(last.to_string());
    }
    (base, args)
}

/// One generator's output, kept so a fragment whose bodies query the world can
/// be parsed again once the world settles.
struct Fragment {
    tokens: Vec<Token>,
    span: Span,
    /// Where its items start in the generated list, and how many there are.
    start: usize,
    count: usize,
    /// Set when a body inside the fragment evaluated a const fn.
    needs_world: bool,
}
/// One deferred item generator.
#[derive(Clone)]
struct Generator {
    index: usize,
    tokens: Vec<Token>,
    span: Span,
    /// Where the generator stands in the schedule.
    state: GeneratorState,
    /// The impl this generator was granted permission to provide.
    grant: Option<(String, String)>,
    /// What it was waiting for when it stopped.
    suspend: Option<comptime::Suspend>,
    /// The impl this generator once offered, kept after it is granted so a
    /// cycle can still be traced through it.
    offers: Option<(String, String)>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum GeneratorState {
    /// Ready to run.
    Ready,
    /// Queue 1: waiting for an impl.
    Waiting,
    /// Queue 2: ready to provide an impl.
    Offering,
    /// Finished.
    Done,
}

/// How one generator run ended.
enum Outcome {
    /// It produced these tokens.
    Produced(Vec<Token>),
    /// It stopped and must be rescheduled.
    Suspended(comptime::Suspend),
}

/// Round limit, so a generator cycle becomes a diagnostic instead of a hang.
const MAX_GENERATOR_ROUNDS: usize = 256;

fn empty_span() -> Span {
    Span {
        file: FileId(0),
        start: 0,
        end: 0,
    }
}

fn item_span(item: &ast::Item) -> Span {
    match item {
        ast::Item::Function(function) => function.span,
        ast::Item::Decl(declaration) => declaration.span,
        ast::Item::Trait(declaration) => declaration.span,
        ast::Item::Impl(block) => block.span,
        ast::Item::Use(import) => import.span,
    }
}

/// The declarator of the function whose body starts at `start`.
fn find_body_declarator(items: &[ast::Item], start: usize) -> Option<ast::Declarator> {
    for item in items {
        match item {
            ast::Item::Function(function) if function.body.span.start == start => {
                return Some(function.declarator.clone());
            }
            ast::Item::Impl(block) => {
                for method in &block.methods {
                    if method.body.span.start == start {
                        return Some(method.declarator.clone());
                    }
                }
            }
            _ => {}
        }
    }
    None
}

/// Replaces the body that starts at `start` with `block`.
fn patch_body(items: &mut [ast::Item], start: usize, block: ast::Block) -> bool {
    for item in items.iter_mut() {
        match item {
            ast::Item::Function(function) if function.body.span.start == start => {
                function.span = function.span.join(block.span);
                function.body = block;
                return true;
            }
            ast::Item::Impl(impl_block) => {
                for method in &mut impl_block.methods {
                    if method.body.span.start == start {
                        method.span = method.span.join(block.span);
                        method.body = block;
                        return true;
                    }
                }
            }
            _ => {}
        }
    }
    false
}
/// The declarations a generator can see.
///
/// It is a snapshot, not a live view: every call of one stage reads the same
/// `World`, and the outputs of that stage only appear in the next one.
#[derive(Clone, Default)]
pub(crate) struct World {
    /// Declared names: functions, objects, types, tags, traits, and imports.
    pub(crate) names: HashSet<String>,
    /// `(trait, type)` pairs from `impl Trait for Type` declarations.
    pub(crate) impls: HashSet<(String, String)>,
    /// Generic implementations such as `impl<T: Show> Trait for Vec<T>`, which
    /// cover the instantiations whose arguments satisfy their bounds.
    pub(crate) generic_impls: Vec<GenericImpl>,
}

/// One generic `impl` and the bounds its parameters carry.
#[derive(Clone)]
pub(crate) struct GenericImpl {
    pub(crate) trait_name: String,
    pub(crate) base: String,
    /// One entry per parameter: the trait bounds written on it.
    pub(crate) bounds: Vec<Vec<String>>,
}

impl World {
    /// Whether `trait_name` is implemented for `owner` here.
    ///
    /// An exact implementation answers directly; otherwise a generic one covers
    /// the instantiation, so `Vec<int>` is answered by `impl<T> Trait for
    /// Vec<T>`.
    pub(crate) fn implements(&self, owner: &str, trait_name: &str) -> bool {
        self.implements_within(
            &normalize_type_name(owner),
            &normalize_trait_query(trait_name),
            0,
        )
    }

    fn implements_within(&self, owner: &str, trait_name: &str, depth: usize) -> bool {
        if depth > 8 {
            return false;
        }
        if self
            .impls
            .contains(&(trait_name.to_string(), owner.to_string()))
        {
            return true;
        }
        let (base, args) = split_type_name(owner);
        for generic in &self.generic_impls {
            if generic.trait_name != trait_name
                || generic.base != base
                || generic.bounds.len() != args.len()
            {
                continue;
            }
            let satisfied = generic.bounds.iter().zip(&args).all(|(bounds, argument)| {
                bounds
                    .iter()
                    .all(|bound| self.implements_within(argument, bound, depth + 1))
            });
            if satisfied {
                return true;
            }
        }
        false
    }

    fn collect(&mut self, items: &[ast::Item]) {
        for item in items {
            match item {
                ast::Item::Function(function) => {
                    if let Some(name) = &function.declarator.name {
                        self.names.insert(name.text.clone());
                    }
                    collect_spec_names(&function.specs, self);
                }
                ast::Item::Decl(declaration) => {
                    collect_spec_names(&declaration.specs, self);
                    for declarator in &declaration.declarators {
                        if let Some(name) = &declarator.declarator.name {
                            self.names.insert(name.text.clone());
                        }
                    }
                }
                ast::Item::Trait(declaration) => {
                    self.names.insert(declaration.name.text.clone());
                }
                ast::Item::Impl(block) => {
                    let Some(declared) = impl_declaration(block) else {
                        continue;
                    };
                    self.names.insert(declared.owner.clone());
                    self.names.insert(declared.trait_name.clone());
                    if declared.is_generic() {
                        self.generic_impls.push(GenericImpl {
                            trait_name: declared.trait_name,
                            base: declared.owner,
                            bounds: declared.bounds,
                        });
                    } else {
                        self.impls.insert((declared.trait_name, declared.owner));
                    }
                }
                ast::Item::Use(import) => {
                    if let Some(name) = import.path.last() {
                        self.names.insert(name.text.clone());
                    }
                }
            }
        }
    }
}

/// One syntax-level name scope.
#[derive(Clone, Default)]
pub(crate) struct Scope {
    pub(crate) typedefs: HashSet<String>,
    pub(crate) values: HashSet<String>,
    pub(crate) tags: HashSet<String>,
    /// Trait names. The parser only needs them to accept a trait name where a
    /// type name would appear, so that semantic analysis can reject a trait
    /// value with its own diagnostic instead of the parser reporting an
    /// unknown type.
    pub(crate) traits: HashSet<String>,
}

impl Parser {
    fn new(tokens: Vec<Token>, prescan: bool) -> Self {
        let last = tokens.first().map_or(
            Span {
                file: FileId(0),
                start: 0,
                end: 0,
            },
            |token| token.span,
        );
        Parser {
            tokens,
            pos: 0,
            last,
            angle_depth: 0,
            pending_enum_niche: false,
            pending_option_niche: false,
            scopes: vec![Scope::default()],
            impl_depth: 0,
            prescan,
            const_fns: Vec::new(),
            body_world: None,
            final_world: None,
            saw_body_call: std::cell::Cell::new(false),
            needs_full_pass: false,
            comptime_sites: Vec::new(),
            body_stack: Vec::new(),
            function_bodies: Vec::new(),
            body_calls: std::cell::RefCell::new(Vec::new()),
            pending_declarator: None,
        }
    }

    fn translation_unit(&mut self) -> Result<ast::TranslationUnit, Diagnostic> {
        let mut items = Vec::new();
        let mut generators: Vec<Generator> = Vec::new();
        self.parse_unit_items(&mut items, &mut generators)?;
        let (generated, world) = self.run_generators(&items, generators)?;
        items.extend(generated);
        self.final_world = Some(world);
        Ok(ast::TranslationUnit { items })
    }

    /// Parses items until the end of the stream, collecting the generators.
    ///
    /// The same routine runs for the main file and for a fragment a generator
    /// produced, so generated source may contain generator calls of its own;
    /// they join the queue instead of being parsed as ordinary declarations.
    fn parse_unit_items(
        &mut self,
        items: &mut Vec<ast::Item>,
        generators: &mut Vec<Generator>,
    ) -> Result<(), Diagnostic> {
        while !self.at_eof() {
            if let Some(index) = self.const_fn_generator_ahead() {
                let start = self.here();
                self.record_const_call(index, start);
                self.bump();
                let (tokens, close) = self.capture_tokens()?;
                generators.push(Generator {
                    index,
                    tokens,
                    span: start.join(close),
                    state: GeneratorState::Ready,
                    grant: None,
                    suspend: None,
                    offers: None,
                });
                continue;
            }
            // `pub` says nothing about a compile-time function: it is visible
            // wherever the header is included, so the marker is accepted and
            // ignored rather than rejected.
            if self.at_ident("pub")
                && self.at_ident_ahead(1, "const")
                && self.at_ident_ahead(2, "fn")
            {
                self.bump();
            }
            // A `const fn` produces no item of its own; its body only runs at
            // compile time, so nothing is emitted for it.
            if self.at_ident("const") && self.at_ident_ahead(1, "fn") {
                self.const_fn_declaration()?;
                continue;
            }
            items.push(self.item()?);
        }
        Ok(())
    }

    /// Runs the item generators to a fixpoint.
    ///
    /// A generator that needs an impl waits in queue 1; one that can provide an
    /// impl waits in queue 2. When nothing else can run, providers are granted
    /// one at a time, which wakes whoever was waiting for that impl. If queue 1
    /// still has sleepers at the end, no one provides what they need.
    fn run_generators(
        &mut self,
        items: &[ast::Item],
        mut generators: Vec<Generator>,
    ) -> Result<(Vec<ast::Item>, World), Diagnostic> {
        let mut world = World::default();
        world.names.extend(self.scopes[0].typedefs.iter().cloned());
        world.names.extend(self.scopes[0].tags.iter().cloned());
        world.names.extend(self.scopes[0].traits.iter().cloned());
        world.collect(items);

        let mut generated = Vec::new();
        let mut fragments: Vec<Fragment> = Vec::new();
        // Impls that were granted to a provider, so a provider that never
        // actually emits one gets a precise diagnostic.
        let mut granted_impls: Vec<((String, String), Span)> = Vec::new();
        let mut rounds = 0usize;
        loop {
            rounds += 1;
            if rounds > MAX_GENERATOR_ROUNDS {
                return Err(Diagnostic::new(
                    generators
                        .first()
                        .map_or(items.first().map_or_else(empty_span, item_span), |g| g.span),
                    "item generation did not settle: too many rounds",
                ));
            }
            let mut progress = false;
            // Run everything that is ready.
            for position in 0..generators.len() {
                if !matches!(generators[position].state, GeneratorState::Ready) {
                    continue;
                }
                let grant = generators[position].grant.clone();
                let generator = generators[position].clone();
                match self.run_generator(&generator, &world, grant)? {
                    Outcome::Produced(tokens) => {
                        generators[position].state = GeneratorState::Done;
                        progress = true;
                        if tokens.is_empty() {
                            continue;
                        }
                        let span = generators[position].span;
                        let name = self.const_fns[generator.index].name.clone();
                        let (stage_items, more_generators, saw_call) = match self
                            .parse_item_fragment(tokens.clone(), span, None)
                        {
                            Ok(result) => result,
                            Err(error) => {
                                // A macro that produces statements belongs in
                                // a function body, not at file scope.
                                if self.parse_statement_fragment(tokens.clone(), span).is_ok() {
                                    return Err(Diagnostic::new(
                                        span,
                                        format!(
                                            "const fn '{name}' produced statements, which cannot stand at file scope; call it inside a function"
                                        ),
                                    ));
                                }
                                return Err(self.fragment_error(error, generator.index));
                            }
                        };
                        // A generated impl may not collide with one that already
                        // exists, no matter which generator produced it.
                        for item in &stage_items {
                            let ast::Item::Impl(block) = item else {
                                continue;
                            };
                            let Some(declared) = impl_declaration(block) else {
                                continue;
                            };
                            let already = if declared.is_generic() {
                                world.generic_impls.iter().any(|existing| {
                                    existing.trait_name == declared.trait_name
                                        && existing.base == declared.owner
                                        && existing.bounds.len() == declared.bounds.len()
                                })
                            } else {
                                world.impls.contains(&(
                                    declared.trait_name.clone(),
                                    declared.owner.clone(),
                                ))
                            };
                            if already {
                                return Err(Diagnostic::new(
                                    block.span,
                                    format!(
                                        "duplicate implementation of '{}' for '{}'",
                                        declared.trait_name, declared.owner
                                    ),
                                ));
                            }
                        }
                        world.collect(&stage_items);
                        let start = generated.len();
                        generated.extend(stage_items);
                        fragments.push(Fragment {
                            tokens,
                            span,
                            start,
                            count: generated.len() - start,
                            needs_world: saw_call,
                        });
                        // Generated source may generate more: those calls join
                        // the same queue and are scheduled by their needs.
                        generators.extend(more_generators);
                    }
                    Outcome::Suspended(suspend) => {
                        generators[position].suspend = Some(suspend.clone());
                        generators[position].state = match suspend {
                            comptime::Suspend::Needs { .. } => GeneratorState::Waiting,
                            comptime::Suspend::Offers { owner, trait_name } => {
                                generators[position].offers = Some((owner, trait_name));
                                GeneratorState::Offering
                            }
                        };
                        progress = true;
                    }
                }
            }
            // Wake the waiters whose impl has just appeared (queue 1).
            for position in 0..generators.len() {
                if !matches!(generators[position].state, GeneratorState::Waiting) {
                    continue;
                }
                let Some(comptime::Suspend::Needs { owner, trait_name }) =
                    generators[position].suspend.clone()
                else {
                    continue;
                };
                if world.impls.contains(&(trait_name, owner)) {
                    generators[position].state = GeneratorState::Ready;
                    progress = true;
                }
            }
            // Wake one provider of an impl nobody has provided (queue 2).
            let mut granted = false;
            for position in 0..generators.len() {
                if !matches!(generators[position].state, GeneratorState::Offering) {
                    continue;
                }
                let Some(comptime::Suspend::Offers { owner, trait_name }) =
                    generators[position].suspend.clone()
                else {
                    continue;
                };
                if world.impls.contains(&(trait_name.clone(), owner.clone())) {
                    // Somebody else provided it; this offer simply ends.
                    generators[position].state = GeneratorState::Done;
                    progress = true;
                    continue;
                }
                // An impl already granted to a generator that is still waiting
                // stays reserved, so two providers never both get permission.
                let reserved = generators.iter().enumerate().any(|(other, generator)| {
                    other != position
                        && !matches!(generator.state, GeneratorState::Done)
                        && generator.grant.as_ref() == Some(&(owner.clone(), trait_name.clone()))
                });
                if reserved {
                    continue;
                }
                granted_impls.push((
                    (owner.clone(), trait_name.clone()),
                    generators[position].span,
                ));
                generators[position].grant = Some((owner, trait_name));
                generators[position].state = GeneratorState::Ready;
                granted = true;
                progress = true;
                break;
            }
            if !progress {
                let _ = granted;
                break;
            }
        }

        // A fragment whose bodies asked about the world is parsed again now that
        // the world is settled, so only generated source is re-read.
        if fragments.iter().any(|fragment| fragment.needs_world) {
            let mut rebuilt = Vec::new();
            let mut cursor = 0usize;
            for fragment in &fragments {
                if !fragment.needs_world {
                    continue;
                }
                rebuilt.extend(generated[cursor..fragment.start].iter().cloned());
                let (items, _, _) =
                    self.parse_item_fragment(fragment.tokens.clone(), fragment.span, Some(&world))?;
                // The fragment must keep its shape; otherwise the unit is
                // parsed again as a whole.
                if items.len() != fragment.count {
                    self.needs_full_pass = true;
                    return Ok((generated, world));
                }
                rebuilt.extend(items);
                cursor = fragment.start + fragment.count;
            }
            rebuilt.extend(generated[cursor..].iter().cloned());
            generated = rebuilt;
        }

        // A cycle of "provide X, need Y" is reported as the chain it is.
        if let Some(cycle) = self.dependency_cycle(&generators) {
            return Err(Diagnostic::new(cycle.0, cycle.1));
        }

        // A granted provider must actually provide the impl it claimed, so a
        // typo cannot silently starve its waiters. A provider that emitted it
        // through a nested generator still counts: the world is what matters.
        for ((owner, trait_name), span) in &granted_impls {
            if world.implements(owner, trait_name) {
                continue;
            }
            let stuck = generators.iter().any(|generator| {
                matches!(generator.state, GeneratorState::Waiting)
                    && generator.grant.as_ref() == Some(&(owner.clone(), trait_name.clone()))
            });
            if stuck {
                // The queue-1 report below explains a stuck provider better.
                continue;
            }
            return Err(Diagnostic::new(
                *span,
                format!(
                    "generator granted to provide '{trait_name}' for '{owner}' produced no such implementation"
                ),
            ));
        }

        // Queue 1 still has sleepers: nothing provides what they need.
        for generator in &generators {
            if let GeneratorState::Waiting = generator.state
                && let Some(comptime::Suspend::Needs { owner, trait_name }) = &generator.suspend
            {
                let name = self
                    .const_fns
                    .get(generator.index)
                    .map_or("a generator", |function| function.name.as_str());
                // A granted provider that is still asleep names the real
                // blocker, which is more useful than "emitted nothing".
                let blocked_by = self.blocking_chain(&generators, owner, trait_name);
                let base = match blocked_by {
                    Some(blocked) => {
                        format!("type '{owner}' needs trait '{trait_name}', but {blocked}")
                    }
                    None => format!(
                        "type '{owner}' needs trait '{trait_name}', but nothing provides it"
                    ),
                };
                let message = format!("{base} (in const fn '{name}')");
                return Err(Diagnostic::new(generator.span, message));
            }
        }
        Ok((generated, world))
    }

    /// Follows a granted impl from holder to holder, so a stuck waiter reports
    /// the whole chain instead of one link.
    fn blocking_chain(
        &self,
        generators: &[Generator],
        owner: &str,
        trait_name: &str,
    ) -> Option<String> {
        let mut segments = Vec::new();
        let mut current = (owner.to_string(), trait_name.to_string());
        let mut seen: Vec<(String, String)> = Vec::new();
        loop {
            if seen.len() >= 8 || seen.contains(&current) {
                break;
            }
            seen.push(current.clone());
            let holder = generators.iter().find(|generator| {
                !matches!(generator.state, GeneratorState::Done)
                    && generator.grant.as_ref() == Some(&current)
            });
            let Some(holder) = holder else {
                if segments.is_empty() {
                    return None;
                }
                segments.push("nothing provides it".to_string());
                break;
            };
            let name = self
                .const_fns
                .get(holder.index)
                .map_or("a generator", |function| function.name.as_str());
            match &holder.suspend {
                Some(comptime::Suspend::Needs { owner, trait_name }) => {
                    segments.push(format!(
                        "its provider '{name}' is still waiting for '{owner}' to implement '{trait_name}'"
                    ));
                    current = (owner.clone(), trait_name.clone());
                }
                _ => {
                    segments.push(format!("its provider '{name}' has not run yet"));
                    break;
                }
            }
        }
        Some(segments.join(", "))
    }

    /// Finds a cycle among the generators that are still waiting.
    ///
    /// A generator offers an impl but needs another; following "who offers what
    /// I need" from each waiting generator either ends at an impl nobody offers
    /// or comes back to where it started, which is the cycle worth reporting.
    fn dependency_cycle(&self, generators: &[Generator]) -> Option<(Span, String)> {
        use std::collections::HashMap;
        let mut offered: HashMap<(String, String), usize> = HashMap::new();
        for (position, generator) in generators.iter().enumerate() {
            if matches!(generator.state, GeneratorState::Done) {
                continue;
            }
            if let Some((owner, trait_name)) = &generator.offers {
                offered
                    .entry((owner.clone(), trait_name.clone()))
                    .or_insert(position);
            }
        }
        for generator in generators {
            let GeneratorState::Waiting = generator.state else {
                continue;
            };
            let Some(comptime::Suspend::Needs { owner, trait_name }) = &generator.suspend else {
                continue;
            };
            let mut chain = vec![format!("'{owner}' needs '{trait_name}'")];
            let mut seen: Vec<(String, String)> = Vec::new();
            let mut current = (owner.clone(), trait_name.clone());
            while seen.len() < 16 {
                if seen.contains(&current) {
                    // The last entry repeats the start; show it once, as the
                    // closing arrow of the cycle.
                    chain.pop();
                    let closing = format!("'{}' needs '{}'", current.0, current.1);
                    return Some((
                        generator.span,
                        format!(
                            "dependency cycle between generators: {} -> {closing} (cycle)",
                            chain.join(" -> ")
                        ),
                    ));
                }
                seen.push(current.clone());
                let Some(&provider) = offered.get(&current) else {
                    break;
                };
                let Some(comptime::Suspend::Needs { owner, trait_name }) =
                    &generators[provider].suspend
                else {
                    break;
                };
                chain.push(format!("'{}' needs '{trait_name}'", owner));
                current = (owner.clone(), trait_name.clone());
            }
        }
        None
    }
    /// Runs one generator, returning its tokens or the reason it stopped.
    fn run_generator(
        &self,
        generator: &Generator,
        world: &World,
        grant: Option<(String, String)>,
    ) -> Result<Outcome, Diagnostic> {
        let mut evaluator = comptime::Evaluator::with_world(&self.const_fns, world);
        evaluator.scheduling();
        if let Some((owner, trait_name)) = grant {
            evaluator.grant(owner, trait_name);
        }
        let value = evaluator.call(
            generator.index,
            &[comptime::Value::Tokens(generator.tokens.clone())],
            generator.span,
        );
        if let Some(suspend) = evaluator.take_suspend() {
            return Ok(Outcome::Suspended(suspend));
        }
        let value = value?;
        let comptime::Value::Tokens(tokens) = value else {
            return Err(Diagnostic::new(
                generator.span,
                "an item const fn must produce tokens",
            ));
        };
        Ok(Outcome::Produced(tokens))
    }

    /// Re-parses only the function bodies that evaluated a const fn.
    ///
    /// Returns false when the attempt was not safe (a call inside a nested
    /// block, or a body that cannot be located), and the caller then falls back
    /// to parsing the whole unit again.
    fn reparse_bodies(
        &self,
        tokens: &[Token],
        world: &World,
        unit: &mut ast::TranslationUnit,
    ) -> bool {
        let bodies = self.body_calls.borrow().clone();
        if bodies.is_empty() {
            return false;
        }
        for open in bodies {
            let Some(declarator) = find_body_declarator(&unit.items, open.start) else {
                return false;
            };
            let Some(block) = self.reparse_body(tokens, open, world, &declarator) else {
                return false;
            };
            if !patch_body(&mut unit.items, open.start, block) {
                return false;
            }
        }
        true
    }

    /// Parses one recorded body again with the settled world in scope.
    fn reparse_body(
        &self,
        tokens: &[Token],
        open: Span,
        world: &World,
        declarator: &ast::Declarator,
    ) -> Option<ast::Block> {
        let start = tokens
            .iter()
            .position(|token| token.span.file == open.file && token.span.start == open.start)?;
        let mut depth = 0usize;
        let mut end = None;
        for (offset, token) in tokens[start..].iter().enumerate() {
            match &token.kind {
                TokenKind::Punct(punct) if punct == "{" => depth += 1,
                TokenKind::Punct(punct) if punct == "}" => {
                    depth = depth.checked_sub(1)?;
                    if depth == 0 {
                        end = Some(start + offset);
                        break;
                    }
                }
                _ => {}
            }
        }
        let end = end?;
        let mut stream: Vec<Token> = tokens[start..=end].to_vec();
        stream.push(Token {
            kind: TokenKind::Eof,
            span: open,
            at_line_start: false,
            leading_space: false,
        });
        let mut parser = Parser::new(stream, false);
        parser.scopes = self.scopes.clone();
        parser.const_fns = self.const_fns.clone();
        parser.body_world = Some(world.clone());
        parser.block_with_params(Some(declarator)).ok()
    }

    /// Parses a token sequence as file-scope items and generators.
    ///
    /// Generated source is parsed by the same routine as the main file, so it
    /// may contain generator calls; those join the schedule instead of being
    /// mistaken for declarations.
    fn parse_item_fragment(
        &mut self,
        mut tokens: Vec<Token>,
        span: Span,
        world: Option<&World>,
    ) -> Result<(Vec<ast::Item>, Vec<Generator>, bool), Diagnostic> {
        let needs_semicolon = !matches!(
            tokens.last().map(|token| &token.kind),
            Some(TokenKind::Punct(punct)) if punct == ";" || punct == "}"
        );
        if needs_semicolon {
            tokens.push(Token {
                kind: TokenKind::Punct(";".to_string()),
                span,
                at_line_start: false,
                leading_space: false,
            });
        }
        tokens.push(Token {
            kind: TokenKind::Eof,
            span,
            at_line_start: false,
            leading_space: false,
        });
        let mut parser = Parser::new(tokens, false);
        parser.scopes = self.scopes.clone();
        parser.const_fns = self.const_fns.clone();
        // Generated bodies query the settled world when it is known; a fragment
        // that needs it is parsed again once the world settles.
        parser.body_world = world.cloned().or_else(|| self.body_world.clone());
        let mut items = Vec::new();
        let mut generators = Vec::new();
        parser.parse_unit_items(&mut items, &mut generators)?;
        // A fragment may declare const fns of its own. They are appended, so the        // A fragment may declare const fns of its own. They are appended, so the
        // generator indices collected above stay valid in the parent table.
        let declared_from = self.const_fns.len();
        if parser.const_fns.len() > declared_from {
            self.const_fns
                .extend(parser.const_fns.into_iter().skip(declared_from));
        }
        // A body call inside the fragment also needs the settled world, so the
        // parent must know one happened.
        let saw_call = parser.saw_body_call.get();
        Ok((items, generators, saw_call))
    }

    /// Records one compile-time site for the index.
    pub(crate) fn record_comptime_site(
        &mut self,
        kind: ComptimeSiteKind,
        name: &str,
        span: Span,
        declaration: Option<Span>,
    ) {
        self.comptime_sites.push(ComptimeSite {
            kind,
            name: name.to_string(),
            span,
            declaration,
            expansion: None,
            signature: None,
        });
    }

    /// Records a const fn declaration with the text of its signature.
    pub(crate) fn record_const_fn_declaration(&mut self, name: &str, span: Span, signature: Span) {
        self.record_comptime_site(ComptimeSiteKind::Declaration, name, span, None);
        if let Some(site) = self.comptime_sites.last_mut() {
            site.signature = Some(signature);
        }
    }

    /// Records a macro invocation and the wrapper its expansion calls.
    pub(crate) fn record_macro_site(&mut self, name: &str, span: Span, expansion: &str) {
        self.comptime_sites.push(ComptimeSite {
            kind: ComptimeSiteKind::Macro,
            name: name.to_string(),
            span,
            declaration: None,
            expansion: Some(expansion.to_string()),
            signature: None,
        });
    }

    /// Records a call to the const fn at index, whose name starts at span.
    pub(crate) fn record_const_call(&mut self, index: usize, span: Span) {
        let function = &self.const_fns[index];
        let (name, declaration) = (function.name.clone(), function.span);
        self.record_comptime_site(ComptimeSiteKind::Call, &name, span, Some(declaration));
    }

    /// Adds the producing const fn to a diagnostic from generated tokens.
    pub(crate) fn fragment_error(&self, error: Diagnostic, index: usize) -> Diagnostic {
        let name = self
            .const_fns
            .get(index)
            .map_or("a const fn", |function| function.name.as_str());
        Diagnostic::new(
            error.span,
            format!("{} (from const fn '{name}')", error.message),
        )
    }

    /// The index of the const fn called at the cursor, when the next tokens are
    /// `name(...)` for a function that returns tokens.
    pub(crate) fn const_fn_call_ahead(&self) -> Option<usize> {
        let TokenKind::Ident(name) = &self.peek().kind else {
            return None;
        };
        if !self.at_punct_ahead(1, "(") {
            return None;
        }
        self.const_fns
            .iter()
            .position(|function| function.returns_tokens && &function.name == name)
    }

    /// The index of a const fn call that stands on its own as a generator.
    ///
    /// A call whose closing parenthesis is followed by `;` is a generator: it
    /// expands to items or statements. A call followed by anything else starts
    /// a declaration, where the call supplies the type instead.
    pub(crate) fn const_fn_generator_ahead(&self) -> Option<usize> {
        let index = self.const_fn_call_ahead()?;
        let mut depth = 0usize;
        let mut offset = 1;
        loop {
            match &self.peek_ahead(offset).kind {
                TokenKind::Punct(punct) if punct == "(" || punct == "[" || punct == "{" => {
                    depth += 1;
                }
                TokenKind::Punct(punct) if punct == ")" || punct == "]" || punct == "}" => {
                    depth = depth.checked_sub(1)?;
                    if depth == 0 {
                        break;
                    }
                }
                TokenKind::Eof => return None,
                _ => {}
            }
            offset += 1;
        }
        let terminates = matches!(
            &self.peek_ahead(offset + 1).kind,
            TokenKind::Punct(punct) if punct == ";"
        );
        terminates.then_some(index)
    }

    /// Whether `name` currently spells a type (typedef name, trait, or Shy
    /// tag).
    ///
    /// Typedefs and ordinary identifiers share one namespace in C, so the
    /// innermost ordinary declaration of a name shadows an outer typedef: that
    /// is what makes a parameter or block local named `T` turn `T *p;` back
    /// into an expression. Tags have their own namespace, so a variable with
    /// the same spelling does not hide `struct T` used as Shy's `T` type name.
    /// A trait is a compile-time name, but it is parsed like a typedef so its
    /// misuse reaches semantic analysis with a precise message.
    pub(crate) fn is_type_name(&self, name: &str) -> bool {
        // A const fn that returns tokens can stand where a type is written, so
        // a cast or a sizeof must parse it as a type and expand it.
        if self
            .const_fns
            .iter()
            .any(|function| function.returns_tokens && function.name == name)
        {
            return true;
        }
        for scope in self.scopes.iter().rev() {
            if scope.typedefs.contains(name) || scope.traits.contains(name) {
                return true;
            }
            if scope.values.contains(name) {
                break;
            }
        }
        self.scopes.iter().any(|scope| scope.tags.contains(name))
    }

    pub(crate) fn declare_typedef(&mut self, name: &str) {
        let innermost = self.scopes.len() - 1;
        self.scopes[innermost].typedefs.insert(name.to_string());
    }

    /// Whether `name` currently spells a value, which is what makes a bare name
    /// a const generic argument rather than a new type parameter.
    pub(crate) fn is_value_name(&self, name: &str) -> bool {
        self.scopes
            .iter()
            .rev()
            .any(|scope| scope.values.contains(name))
    }

    pub(crate) fn declare_value(&mut self, name: &str) {
        let innermost = self.scopes.len() - 1;
        self.scopes[innermost].values.insert(name.to_string());
    }

    pub(crate) fn declare_tag(&mut self, name: &str) {
        let innermost = self.scopes.len() - 1;
        self.scopes[innermost].tags.insert(name.to_string());
    }

    pub(crate) fn declare_trait(&mut self, name: &str) {
        let innermost = self.scopes.len() - 1;
        self.scopes[innermost].traits.insert(name.to_string());
    }

    pub(crate) fn push_scope(&mut self) {
        self.scopes.push(Scope::default());
    }

    pub(crate) fn pop_scope(&mut self) {
        self.scopes.pop();
    }
}
