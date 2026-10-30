//! Token cursor, expectation helpers, and keyword predicates.

use crate::ast::Name;
use crate::lexer::{Token, TokenKind};
use crate::source::{Diagnostic, Span};

use super::Parser;

impl Parser<'_> {
    /// Index of the `n`-th following token, ignoring `Newline` tokens.
    fn index_ahead(&self, count: usize) -> usize {
        let mut index = self.pos;
        let mut seen = 0;
        loop {
            while matches!(self.tokens[index].kind, TokenKind::Newline) {
                index += 1;
            }
            if seen == count {
                return index;
            }
            seen += 1;
            index += 1;
        }
    }

    pub(crate) fn peek(&self) -> &Token {
        &self.tokens[self.index_ahead(0)]
    }

    pub(crate) fn peek_ahead(&self, count: usize) -> &Token {
        &self.tokens[self.index_ahead(count)]
    }

    pub(crate) fn bump(&mut self) -> Token {
        let index = self.index_ahead(0);
        let token = self.tokens[index].clone();
        self.last = token.span;
        self.pos = index + 1;
        token
    }

    /// Span of the token before the cursor; used to finish compound spans.
    pub(crate) fn last_span(&self) -> Span {
        self.last
    }

    pub(crate) fn here(&self) -> Span {
        self.peek().span
    }

    /// Span from `start` through the most recently consumed token.
    pub(crate) fn finish(&self, start: Span) -> Span {
        start.join(self.last)
    }

    pub(crate) fn at_eof(&self) -> bool {
        matches!(self.peek().kind, TokenKind::Eof)
    }

    pub(crate) fn at_punct(&self, punct: &str) -> bool {
        self.punct_here(0) == Some(punct)
    }

    pub(crate) fn at_punct_ahead(&self, count: usize, punct: &str) -> bool {
        self.punct_here(count) == Some(punct)
    }

    fn punct_here(&self, count: usize) -> Option<&str> {
        match &self.peek_ahead(count).kind {
            TokenKind::Punct(text) => Some(text),
            _ => None,
        }
    }

    pub(crate) fn at_ident(&self, word: &str) -> bool {
        self.ident_here() == Some(word)
    }

    pub(crate) fn ident_here(&self) -> Option<&str> {
        match &self.peek().kind {
            TokenKind::Ident(text) => Some(text),
            _ => None,
        }
    }

    pub(crate) fn ident_here_owned(&self) -> Option<String> {
        self.ident_here().map(str::to_string)
    }

    pub(crate) fn eat_punct(&mut self, punct: &str) -> bool {
        if self.at_punct(punct) {
            self.bump();
            true
        } else {
            false
        }
    }

    pub(crate) fn eat_ident(&mut self, word: &str) -> bool {
        if self.at_ident(word) {
            self.bump();
            true
        } else {
            false
        }
    }

    pub(crate) fn expect_punct(&mut self, punct: &str) -> Result<Span, Diagnostic> {
        if self.at_punct(punct) {
            Ok(self.bump().span)
        } else {
            Err(self.expected(&format!("'{punct}'")))
        }
    }

    pub(crate) fn expect_keyword(&mut self, word: &str) -> Result<Span, Diagnostic> {
        if self.at_ident(word) {
            Ok(self.bump().span)
        } else {
            Err(self.expected(&format!("'{word}'")))
        }
    }

    /// Consumes an identifier used as a name: a tag, member, parameter,
    /// label, enumerator, or declared name. Keywords are not names.
    pub(crate) fn expect_ident(&mut self) -> Result<Name, Diagnostic> {
        let Some(word) = self.ident_here_owned() else {
            return Err(self.expected("an identifier"));
        };
        if is_keyword(&word) {
            return Err(self.expected("an identifier"));
        }
        let span = self.here();
        self.bump();
        Ok(Name::new(word, span))
    }

    /// The one message helper behind every `expect_*` call.
    ///
    /// An unsupported construct parked exactly where a token was expected
    /// gets its own message instead of a generic "expected ..." one.
    pub(crate) fn expected(&self, wanted: &str) -> Diagnostic {
        let token = self.peek();
        if let TokenKind::Ident(word) = &token.kind
            && let Some(message) = unsupported_message(word)
        {
            return Diagnostic::new(token.span, message);
        }
        Diagnostic::new(
            token.span,
            format!("expected {wanted}, found {}", describe(token)),
        )
    }

    pub(crate) fn describe_here(&self) -> String {
        describe(self.peek())
    }
}

fn describe(token: &Token) -> String {
    match &token.kind {
        TokenKind::Ident(text) => format!("'{text}'"),
        TokenKind::Number(text) => format!("number '{text}'"),
        TokenKind::StringLiteral { .. } => "a string literal".to_string(),
        TokenKind::CharLiteral { .. } => "a character literal".to_string(),
        TokenKind::Punct(text) => format!("'{text}'"),
        TokenKind::Newline => "a newline".to_string(),
        TokenKind::Eof => "end of file".to_string(),
    }
}

/// Reserved words of the C grammar and of the Shy extension.
///
/// `self` is deliberately absent: it is a type name only inside `impl`, and a
/// normal identifier everywhere else.
pub(crate) fn is_keyword(word: &str) -> bool {
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
            | "alignof"
            | "__alignof"
            | "__alignof__"
            | "asm"
            | "impl"
            | "typeof"
            | "__typeof"
            | "__typeof__"
            | "__attribute"
            | "__attribute__"
            | "__declspec"
            | "__restrict"
            | "__restrict__"
            | "__inline"
            | "__inline__"
            | "__thread"
    )
}

/// Message for a construct the AST deliberately cannot represent.
pub(crate) fn unsupported_message(word: &str) -> Option<&'static str> {
    Some(match word {
        "_Alignas" => "_Alignas alignment specifiers are not supported",
        "_Static_assert" => "_Static_assert declarations are not supported",
        "_Thread_local" | "__thread" => "thread-local storage is not supported",
        "typeof" | "__typeof" | "__typeof__" => "typeof is not supported",
        "_Generic" => "_Generic selection is not supported",
        "_Complex" | "_Imaginary" => "complex and imaginary types are not supported",
        "__attribute__" | "__attribute" | "__declspec" => "attributes are not supported",
        _ => return None,
    })
}
