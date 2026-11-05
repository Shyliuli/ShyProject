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
mod probe;
mod stmt;
mod ty;

pub(crate) use comptime::token_text;
pub use comptime::{ConstFn, ParamKind};
pub use cursor::is_keyword;
pub use probe::{
    Answer, Answers, Arguments, GenericImplSnapshot, ImplPair, ParseOutput, Place, ProbeRequest,
    RequestKey, TrySite, WorldSnapshot, parse_for_probe, token_from_probe, token_kind_number,
};
pub use ty::builtin_type;

use std::collections::{HashMap, HashSet};

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
///
/// A `const fn` call runs in a probe round, so a parse without answers cannot
/// finish one: the call is recorded as a request and leaves a placeholder. A
/// program the round has not answered is reported here instead of being
/// compiled from that placeholder.
pub fn parse_with_sites(
    tokens: &[Token],
) -> Result<(ast::TranslationUnit, Vec<ComptimeSite>), Diagnostic> {
    let parsed = parse_for_probe(tokens, Answers::default())?;
    if let Some(request) = parsed.requests.first() {
        return Err(Diagnostic::new(
            request.site,
            format!(
                "const fn '{}' runs in a probe round, and this compile did not run one; drop --no-constfn-probe to run one",
                request.name
            ),
        ));
    }
    Ok((parsed.unit, parsed.sites))
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
    /// Set when a function body evaluated a const fn during this pass, which is
    /// the only case that needs a second look.
    pub(crate) saw_body_call: std::cell::Cell<bool>,
    /// Set when re-parsing a fragment changed its shape, so the whole unit must
    /// be parsed again instead.
    /// Compile-time sites seen so far, for tools that need the syntax the
    /// expansion removed.
    pub(crate) comptime_sites: Vec<ComptimeSite>,
    /// Span of the body being parsed, innermost last.
    pub(crate) body_stack: Vec<Span>,
    /// Declaration pass: body calls are left for the final body pass. Scalar
    /// calls remain executable in a probe-only tree; syntax-producing calls
    /// remain unresolved and cannot be executed by the probe.
    pub(crate) defer_body_calls: bool,
    /// Whether that pass left a body call unasked.
    pub(crate) bodies_deferred: bool,
    /// Span of the function body being parsed, innermost last. A call inside a
    /// nested block still belongs to this body for re-parsing.
    pub(crate) function_bodies: Vec<Span>,
    /// Function names whose bodies were left with const fn placeholders in the
    /// declaration pass.
    pub(crate) function_body_names: Vec<Option<String>>,
    /// Names of those functions, for probe safety checks.
    pub(crate) deferred_body_functions: Vec<String>,
    /// Bodies that contain a const fn call: only these need re-parsing once the
    /// declaration world is settled.
    /// Declarator produced by a type-position expansion, waiting to be joined
    /// with the declarator the declaration writes after it.
    pub(crate) pending_declarator: Option<ast::Declarator>,
    /// Set while a `const fn` body is parsed. Calls there remain ordinary
    /// function calls whose arguments are evaluated by the compiled probe.
    pub(crate) in_const_body: bool,
    /// Names one expanded format macro already used, so its locals cannot
    /// capture an identifier the arguments mention.
    pub(crate) macro_index: usize,
    /// Ordinary function bodies the evaluator may interpret for `try_run`.
    /// Probe mode: answers from an earlier round and the requests this parse
    /// collects. Absent in a normal parse, which evaluates instead.
    pub(crate) probe: Option<std::rc::Rc<std::cell::RefCell<probe::ProbeContext>>>,
    /// `try_run` calls this parse rewrote, in source order.
    pub(crate) try_sites: Vec<TrySite>,
    /// Span of a `#[const_fn_token_builtin]` waiting for its declaration.
    pub(crate) pending_token_builtin: Option<Span>,
    /// Functions declared with `#[const_fn_token_builtin]`, by name, with the
    /// span of the declaration that named them.
    pub(crate) token_builtins: HashMap<String, Span>,
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
                .map(|bound| trait_key(&bound.name, &bound.args))
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
    let mut text = crate::probe::canonical_type_name(&parts.join(" "));
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

/// Canonicalizes a sequence of builtin type keywords using declaration rules.
pub(crate) fn canonical_builtin_name(text: &str) -> Option<String> {
    let mut name = BuiltinName::default();
    for word in text.split_whitespace() {
        name.add(builtin_type(word)?);
    }
    name.spelling()
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
            saw_body_call: std::cell::Cell::new(false),
            comptime_sites: Vec::new(),
            body_stack: Vec::new(),
            defer_body_calls: false,
            bodies_deferred: false,
            function_bodies: Vec::new(),
            function_body_names: Vec::new(),
            deferred_body_functions: Vec::new(),
            pending_declarator: None,
            in_const_body: false,
            macro_index: 0,
            probe: None,
            try_sites: Vec::new(),
            pending_token_builtin: None,
            token_builtins: HashMap::new(),
        }
    }

    pub(crate) fn push_scope(&mut self) {
        self.scopes.push(Scope::default());
    }

    pub(crate) fn pop_scope(&mut self) {
        self.scopes.pop();
    }

    pub(crate) fn declare_typedef(&mut self, name: &str) {
        let innermost = self.scopes.len() - 1;
        self.scopes[innermost].typedefs.insert(name.to_string());
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

    /// Whether `name` currently spells a value, which is what makes a bare name
    /// a const generic argument rather than a new type parameter.
    pub(crate) fn is_value_name(&self, name: &str) -> bool {
        self.scopes
            .iter()
            .rev()
            .any(|scope| scope.values.contains(name))
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

    /// Records a const fn declaration with the text of its signature.
    pub(crate) fn record_const_fn_declaration(&mut self, name: &str, span: Span, signature: Span) {
        self.record_comptime_site(ComptimeSiteKind::Declaration, name, span, None);
        if let Some(site) = self.comptime_sites.last_mut() {
            site.signature = Some(signature);
        }
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

    /// The arguments of a const fn call, read the way its parameter list says.
    ///
    /// A single Tokens parameter receives everything inside the parentheses as
    /// written; every other signature parses ordinary expressions.
    pub(crate) fn const_arguments(
        &mut self,
        index: usize,
    ) -> Result<(Arguments, Span), Diagnostic> {
        let takes_tokens = {
            let params = &self.const_fns[index].params;
            params.len() == 1 && params[0].1 == ParamKind::Tokens
        };
        if takes_tokens {
            let (tokens, close) = self.capture_token_argument()?;
            return Ok((Arguments::Tokens(tokens), close));
        }
        let (expressions, close) = self.call_arguments()?;
        Ok((Arguments::Expressions(expressions), close))
    }

    /// Parses a token sequence as file-scope items.
    ///
    /// Generated source is parsed by the same routine as the main file, so it
    /// may contain const fn calls of its own; those are answered from the
    /// cache or recorded as requests.
    fn parse_item_fragment(
        &mut self,
        mut tokens: Vec<Token>,
        span: Span,
        world: Option<&World>,
    ) -> Result<(Vec<ast::Item>, bool), Diagnostic> {
        let needs_semicolon = !matches!(
            tokens.last().map(|token| &token.kind),
            Some(TokenKind::Punct(punct)) if punct == ";"
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
        parser.probe = self.probe.clone();
        parser.body_world = world.cloned().or_else(|| self.body_world.clone());
        parser.defer_body_calls = self.defer_body_calls;
        let mut items = Vec::new();
        parser.parse_unit_items(&mut items)?;
        self.bodies_deferred |= parser.bodies_deferred;
        self.deferred_body_functions
            .extend(parser.deferred_body_functions.iter().cloned());
        let declared_from = self.const_fns.len();
        if parser.const_fns.len() > declared_from {
            self.const_fns
                .extend(parser.const_fns.into_iter().skip(declared_from));
        }
        let saw_call = parser.saw_body_call.get();
        Ok((items, saw_call))
    }

    /// Parses items until the end of the stream.
    ///
    /// A const fn call at file scope is answered from the probe cache or
    /// recorded as a request; it never runs here.
    fn parse_unit_items(&mut self, items: &mut Vec<ast::Item>) -> Result<(), Diagnostic> {
        while !self.at_eof() {
            // A generator call or an expanded function can leave a standalone
            // semicolon. It is not an implicit-int declaration in the output.
            if self.eat_punct(";") {
                continue;
            }
            if let Some(index) = self.const_fn_generator_ahead() {
                let start = self.here();
                self.record_const_call(index, start);
                self.bump();
                let (arguments, close) = self.const_arguments(index)?;
                let span = start.join(close);
                self.probe_item(index, span, arguments, items)?;
                continue;
            }
            if self.at_ident("pub")
                && self.at_ident_ahead(1, "const")
                && self.at_ident_ahead(2, "fn")
            {
                self.bump();
            }
            if self.at_ident("const") && self.at_ident_ahead(1, "fn") {
                self.const_fn_declaration()?;
                continue;
            }
            items.push(self.item()?);
        }
        Ok(())
    }
}
