//! Shy metadata attributes: `#![no_main]`, `#![mem(N)]`, `#![stack(N)]`,
//! `#![disable_nonexhaustive_switch]`, `#![disable_rustic_mut]`,
//! `#![disable_default_static]`.
//!
//! Only the main input file may contain them. The size suffixes `k`, `K`,
//! `m`, and `M` are binary multiples, as documented in `ShyC.md`.

use super::Preprocessor;
use crate::lexer::{Token, TokenKind};
use crate::source::{Diagnostic, Span};

impl Preprocessor<'_> {
    /// Consumes one attribute line. `tokens` starts at `#` and ends before the
    /// newline.
    pub(crate) fn handle_attribute(
        &mut self,
        tokens: &[Token],
        hash_span: Span,
    ) -> Result<(), Diagnostic> {
        let Some(close) = tokens
            .iter()
            .position(|token| matches!(&token.kind, TokenKind::Punct(punct) if punct == "]"))
        else {
            return Err(Diagnostic::new(
                hash_span,
                "unterminated Shy attribute (expected `]`)",
            ));
        };
        if close + 1 != tokens.len() {
            return Err(Diagnostic::new(
                tokens[close + 1].span,
                "unexpected tokens after the Shy attribute",
            ));
        }

        let inside = &tokens[3..close];
        let Some(Token {
            kind: TokenKind::Ident(name),
            span,
            ..
        }) = inside.first()
        else {
            return Err(Diagnostic::new(hash_span, "empty Shy attribute"));
        };

        match name.as_str() {
            "no_main" => {
                if inside.len() != 1 {
                    return Err(Diagnostic::new(*span, "expected #![no_main]"));
                }
                if self.attributes.no_main {
                    return Err(Diagnostic::new(
                        *span,
                        "duplicate Shy #![no_main] attribute",
                    ));
                }
                self.attributes.no_main = true;
            }
            "disable_nonexhaustive_switch" => {
                if inside.len() != 1 {
                    return Err(Diagnostic::new(
                        *span,
                        "expected #![disable_nonexhaustive_switch]",
                    ));
                }
                if self.attributes.disable_nonexhaustive_switch {
                    return Err(Diagnostic::new(
                        *span,
                        "duplicate Shy #![disable_nonexhaustive_switch] attribute",
                    ));
                }
                self.attributes.disable_nonexhaustive_switch = true;
            }
            "disable_default_static" => {
                if inside.len() != 1 {
                    return Err(Diagnostic::new(
                        *span,
                        "expected #![disable_default_static]",
                    ));
                }
                if self.attributes.disable_default_static {
                    return Err(Diagnostic::new(
                        *span,
                        "duplicate Shy #![disable_default_static] attribute",
                    ));
                }
                self.attributes.disable_default_static = true;
            }
            "disable_rustic_mut" => {
                if inside.len() != 1 {
                    return Err(Diagnostic::new(*span, "expected #![disable_rustic_mut]"));
                }
                if self.attributes.disable_rustic_mut {
                    return Err(Diagnostic::new(
                        *span,
                        "duplicate Shy #![disable_rustic_mut] attribute",
                    ));
                }
                self.attributes.disable_rustic_mut = true;
            }
            "allow" => {
                let expected = [
                    TokenKind::Ident("allow".to_string()),
                    TokenKind::Punct("(".to_string()),
                    TokenKind::Ident("defer_copy".to_string()),
                    TokenKind::Punct(")".to_string()),
                ];
                if inside.len() != expected.len()
                    || !inside
                        .iter()
                        .zip(expected.iter())
                        .all(|(token, expected)| token.kind == *expected)
                {
                    return Err(Diagnostic::new(*span, "expected #![allow(defer_copy)]"));
                }
                if self.attributes.allow_defer_copy {
                    return Err(Diagnostic::new(
                        *span,
                        "duplicate Shy #![allow(defer_copy)] attribute",
                    ));
                }
                self.attributes.allow_defer_copy = true;
            }
            "mem" | "stack" => {
                let value = parse_size_attribute(inside, *span, name)?;
                let slot = if name == "mem" {
                    &mut self.attributes.memory
                } else {
                    &mut self.attributes.stack
                };
                if slot.is_some() {
                    return Err(Diagnostic::new(
                        *span,
                        format!("duplicate Shy #![{name}(...)] attribute"),
                    ));
                }
                *slot = Some(value);
            }
            other => {
                return Err(Diagnostic::new(
                    *span,
                    format!("unknown Shy attribute #![{other}]"),
                ));
            }
        }
        Ok(())
    }
}

fn parse_size_attribute(inside: &[Token], span: Span, name: &str) -> Result<u32, Diagnostic> {
    let [
        _,
        Token {
            kind: TokenKind::Punct(open),
            ..
        },
        Token {
            kind: TokenKind::Number(number),
            ..
        },
        Token {
            kind: TokenKind::Punct(close),
            ..
        },
    ] = inside
    else {
        return Err(Diagnostic::new(
            span,
            format!("expected #![{name}(<size>)]"),
        ));
    };
    if open != "(" || close != ")" {
        return Err(Diagnostic::new(
            span,
            format!("expected #![{name}(<size>)]"),
        ));
    }
    parse_size(number)
        .ok_or_else(|| Diagnostic::new(span, format!("invalid size `{number}` in #![{name}(...)]")))
}

/// Parses a byte count with an optional binary `k`/`m` suffix.
fn parse_size(text: &str) -> Option<u32> {
    let (digits, multiplier) = match text.as_bytes().last()? {
        b'k' | b'K' => (&text[..text.len() - 1], 1024u64),
        b'm' | b'M' => (&text[..text.len() - 1], 1024 * 1024),
        _ => (text, 1),
    };
    let value: u64 = digits.parse().ok()?;
    let bytes = value.checked_mul(multiplier)?;
    u32::try_from(bytes).ok()
}
