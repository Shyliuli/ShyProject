//! Directive dispatch, `#include` resolution, `#pragma`, and `#error`.

use std::path::PathBuf;

use super::MAX_INCLUDE_DEPTH;
use super::Preprocessor;
use super::canonical_key;
use super::macros::{ExpToken, is_punct, spelling};
use crate::lexer::{Token, TokenKind};
use crate::source::{Diagnostic, FileId, Span};

impl Preprocessor<'_> {
    /// Runs one non-conditional, active directive.
    pub(crate) fn handle_directive(
        &mut self,
        file: FileId,
        name: &str,
        arguments: &[Token],
        hash_span: Span,
    ) -> Result<(), Diagnostic> {
        match name {
            "include" => self.handle_include(file, arguments, hash_span),
            "define" => {
                if file == self.root {
                    self.defines.push(define_line(arguments));
                }
                self.define_directive(arguments, hash_span)
            }
            "undef" => self.undef_directive(arguments, hash_span),
            "error" => {
                let text = arguments.iter().map(spelling).collect::<Vec<_>>().join(" ");
                let message = if text.is_empty() {
                    "#error".to_string()
                } else {
                    format!("#error {text}")
                };
                Err(Diagnostic::new(hash_span, message))
            }
            // There is no warning channel in the public API yet; ignoring
            // #warning keeps otherwise valid translation units building.
            "warning" => Ok(()),
            "pragma" => self.handle_pragma(file, arguments),
            "line" => Err(Diagnostic::new(
                hash_span,
                "#line directives are not supported",
            )),
            "include_next" => Err(Diagnostic::new(hash_span, "#include_next is not supported")),
            other => Err(Diagnostic::new(
                hash_span,
                format!("unknown preprocessing directive `#{other}`"),
            )),
        }
    }

    pub(super) fn handle_include(
        &mut self,
        current: FileId,
        arguments: &[Token],
        hash_span: Span,
    ) -> Result<(), Diagnostic> {
        let name = match parse_include_name(arguments)? {
            Some(name) => name,
            None => {
                // A macro is allowed to expand to the header name.
                let expanded =
                    self.expand_all(arguments.iter().cloned().map(ExpToken::plain).collect())?;
                let tokens: Vec<Token> = expanded
                    .into_iter()
                    .map(|expanded| {
                        let span = expanded.span();
                        let mut token = expanded.token;
                        token.span = span;
                        token
                    })
                    .collect();
                match parse_include_name(&tokens)? {
                    Some(name) => name,
                    None => {
                        return Err(Diagnostic::new(
                            hash_span,
                            "expected \"file\" or <file> after #include",
                        ));
                    }
                }
            }
        };

        if current == self.root {
            self.includes.push(super::Include {
                path: name.path.clone(),
                quoted: name.quoted,
            });
        }

        let relative = PathBuf::from(&name.path);
        let mut found = None;
        let mut searched = Vec::new();
        if relative.is_absolute() {
            if relative.is_file() {
                found = Some(relative);
            } else {
                searched.push(name.path.clone());
            }
        } else {
            if name.quoted {
                let including = &self.sources.file(current).path;
                if let Some(directory) = including.parent() {
                    let candidate = directory.join(&relative);
                    searched.push(candidate.display().to_string());
                    if candidate.is_file() {
                        found = Some(candidate);
                    }
                }
            }
            if found.is_none() {
                for directory in &self.options.include_dirs {
                    let candidate = directory.join(&relative);
                    searched.push(candidate.display().to_string());
                    if candidate.is_file() {
                        found = Some(candidate);
                        break;
                    }
                }
            }
        }

        let Some(path) = found else {
            let detail = if searched.is_empty() {
                String::new()
            } else {
                format!(" (searched {})", searched.join(", "))
            };
            return Err(Diagnostic::new(
                name.span,
                format!("#include file not found: \"{}\"{}", name.path, detail),
            ));
        };

        let key = canonical_key(&path);
        if self.once_files.contains(&key) {
            return Ok(());
        }
        if self.include_stack.len() >= MAX_INCLUDE_DEPTH {
            let chain = self
                .include_stack
                .iter()
                .rev()
                .take(8)
                .map(|file| self.sources.file(*file).path.display().to_string())
                .collect::<Vec<_>>()
                .join(" -> ");
            return Err(Diagnostic::new(
                name.span,
                format!(
                    "include nesting exceeds {MAX_INCLUDE_DEPTH} while including \"{}\"; \
                     this usually means a recursive #include without an include guard \
                     (chain: {chain})",
                    name.path
                ),
            ));
        }

        let included = if let Some(&file) = self.file_ids.get(&key) {
            file
        } else {
            let text = std::fs::read_to_string(&path).map_err(|error| {
                Diagnostic::new(
                    name.span,
                    format!("cannot read include file \"{}\": {error}", path.display()),
                )
            })?;
            let file = self.sources.add(path.clone(), text);
            self.file_ids.insert(key.clone(), file);
            self.file_keys.insert(file, key.clone());
            file
        };

        let tokens = self.lex_file(included)?;
        self.process_file(included, tokens, false)
    }

    fn handle_pragma(&mut self, file: FileId, arguments: &[Token]) -> Result<(), Diagnostic> {
        let Some(Token {
            kind: TokenKind::Ident(name),
            span,
            ..
        }) = arguments.first()
        else {
            // An empty pragma or an implementation-specific one is ignored.
            return Ok(());
        };
        if name == "pack" {
            return Err(Diagnostic::new(
                *span,
                "#pragma pack is not supported: struct/union packing is not implemented",
            ));
        }
        if name != "once" {
            return Ok(());
        }
        if arguments.len() > 1 {
            return Err(Diagnostic::new(
                arguments[1].span,
                "unexpected tokens after #pragma once",
            ));
        }
        let key = self
            .file_keys
            .get(&file)
            .cloned()
            .unwrap_or_else(|| canonical_key(&self.sources.file(file).path));
        self.once_files.insert(key);
        Ok(())
    }
}

struct IncludeName {
    path: String,
    span: Span,
    quoted: bool,
}

/// Rebuild a main-file `#define` directive as one line of C text.
///
/// The lexer keeps a `leading_space` flag on every token, so the printer can
/// preserve the only whitespace that changes meaning: `#define F(x)` stays
/// function-like, while `#define F (x)` stays object-like. Everything else is
/// separated by the space the source had. A replacement list continued with
/// backslashes comes out on one line, which is still one C definition.
fn define_line(arguments: &[Token]) -> String {
    let mut text = String::from("#define");
    for (index, token) in arguments.iter().enumerate() {
        if index == 0 || token.leading_space {
            text.push(' ');
        }
        text.push_str(&spelling(token));
    }
    text
}

/// Parses `"file"` or `<file>` without macro expansion. `None` means the
/// first token is neither form, so the caller should expand macros first.
fn parse_include_name(arguments: &[Token]) -> Result<Option<IncludeName>, Diagnostic> {
    let Some(first) = arguments.first() else {
        return Ok(None);
    };
    match &first.kind {
        TokenKind::StringLiteral { raw, .. } => {
            if arguments.len() != 1 {
                return Err(Diagnostic::new(
                    first.span,
                    "unexpected tokens after the #include file name",
                ));
            }
            Ok(Some(IncludeName {
                path: raw.clone(),
                span: first.span,
                quoted: true,
            }))
        }
        TokenKind::Punct(punct) if punct == "<" => {
            let Some(offset) = arguments[1..].iter().position(|token| is_punct(token, ">")) else {
                return Err(Diagnostic::new(first.span, "missing `>` in #include"));
            };
            let close = offset + 1;
            if close + 1 != arguments.len() {
                return Err(Diagnostic::new(
                    arguments[close].span,
                    "unexpected tokens after the #include file name",
                ));
            }
            if close == 1 {
                return Err(Diagnostic::new(first.span, "empty file name in #include"));
            }
            let path = arguments[1..close].iter().map(spelling).collect::<String>();
            Ok(Some(IncludeName {
                path,
                span: first.span.join(arguments[close].span),
                quoted: false,
            }))
        }
        _ => Ok(None),
    }
}
