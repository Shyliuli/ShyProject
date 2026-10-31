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

use std::collections::HashSet;

use crate::ast;
use crate::lexer::Token;
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
    parser.translation_unit()
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
    /// Name scopes, innermost last. `scopes[0]` is file scope.
    pub(crate) scopes: Vec<Scope>,
    /// Nesting depth of `impl` bodies; `self` is a type only inside one.
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

    /// Whether `name` currently spells a type (typedef name or Shy tag).
    ///
    /// Typedefs and ordinary identifiers share one namespace in C, so the
    /// innermost ordinary declaration of a name shadows an outer typedef: that
    /// is what makes a parameter or block local named `T` turn `T *p;` back
    /// into an expression. Tags have their own namespace, so a variable with
    /// the same spelling does not hide `struct T` used as Shy's `T` type name.
    pub(crate) fn is_type_name(&self, name: &str) -> bool {
        for scope in self.scopes.iter().rev() {
            if scope.typedefs.contains(name) {
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

    pub(crate) fn declare_value(&mut self, name: &str) {
        let innermost = self.scopes.len() - 1;
        self.scopes[innermost].values.insert(name.to_string());
    }

    pub(crate) fn declare_tag(&mut self, name: &str) {
        let innermost = self.scopes.len() - 1;
        self.scopes[innermost].tags.insert(name.to_string());
    }

    pub(crate) fn push_scope(&mut self) {
        self.scopes.push(Scope::default());
    }

    pub(crate) fn pop_scope(&mut self) {
        self.scopes.pop();
    }
}
