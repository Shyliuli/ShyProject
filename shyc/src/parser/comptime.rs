//! Compile-time declarations for `const fn`.
//!
//! A `const fn` is an ordinary ShyC function that runs while compiling: its
//! body is compiled as ordinary code and executed in the emulator, never
//! walked by the parser. This module only records the declaration, so the
//! driver can rebuild it as a function for a probe round; every call becomes a
//! probe request (see [`crate::parser::probe`]).

use crate::ast;
use crate::lexer::Token;
use crate::source::Span;

/// How a parameter value is passed into a compile-time function.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ParamKind {
    /// An integer or boolean.
    Int,
    /// A string: `const char *`, `Str`, or `String`.
    Str,
    /// The call's argument tokens, captured as written.
    Tokens,
}

/// One `const fn` seen in the translation unit.
///
/// The whole declaration is kept — specifiers, declarator, and body — so a
/// probe emitter can rebuild an ordinary function from it and reuse the
/// signature, generics, and parameter types the user wrote. The other fields
/// are a summary of that declaration, derived when it is recorded.
#[derive(Clone)]
pub struct ConstFn {
    pub name: String,
    /// Where the name is declared, for tools that jump to it.
    pub span: Span,
    pub params: Vec<(String, ParamKind)>,
    /// True when the declared return type is `Tokens`, so a call may stand in
    /// a type position and be expanded there.
    pub returns_tokens: bool,
    pub specs: ast::DeclSpecs,
    pub declarator: ast::Declarator,
    pub body: ast::Block,
}

impl ConstFn {
    /// The declaration as an ordinary function definition.
    ///
    /// Probe mode compiles a `const fn` through the normal pipeline, which
    /// takes [`ast::Item`]s; this is the item it needs. The span runs from the
    /// start of the declarator to the closing brace: the item is an ordinary
    /// function, so the leading `const` is not part of it.
    pub fn function_item(&self) -> ast::Item {
        ast::Item::Function(ast::FunctionDecl {
            span: self.declarator.span.join(self.body.span),
            specs: self.specs.clone(),
            declarator: self.declarator.clone(),
            body: self.body.clone(),
            allow_defer_copy: false,
        })
    }
}

/// The source spelling of one token, for a compile-time string result.
///
/// A probe bakes the same spellings into the image it runs, so the compiler
/// and the probe would disagree if this had its own copy.
pub(crate) fn token_text(token: &Token) -> String {
    crate::lexer::spelling(token)
}
