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
pub fn parse(tokens: &[Token]) -> Result<ast::TranslationUnit, Diagnostic> {
    let global = decl::prescan(tokens);
    let mut parser = Parser::new(tokens.to_vec(), false);
    parser.scopes[0].typedefs = global.typedefs;
    parser.scopes[0].tags = global.tags;
    parser.scopes[0].traits = global.traits;
    parser.translation_unit()
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
}

/// One syntax-level name scope.
#[derive(Default)]
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
        }
    }

    fn translation_unit(&mut self) -> Result<ast::TranslationUnit, Diagnostic> {
        let mut items = Vec::new();
        while !self.at_eof() {
            items.push(self.item()?);
        }
        Ok(ast::TranslationUnit { items })
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
