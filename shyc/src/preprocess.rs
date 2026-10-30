//! C preprocessing for ShyC.
//!
//! Pipeline position: `source` -> `lexer` -> **`preprocess`** -> parser.
//!
//! The preprocessor consumes the tokens of one translation unit, handles
//! preprocessing directives, expands macros, and returns a flat token stream
//! for the parser. Newline tokens are not part of that stream and exactly one
//! [`TokenKind::Eof`] ends it.
//!
//! Shy's metadata attributes (`#![no_main]`, `#![mem(10M)]`,
//! `#![stack(4K)]`) are consumed from the main input file and returned in
//! [`Output::attributes`]. They are only recognised at the start of a line.
//!
//! Supported directives:
//!
//! - `#include "file"` (searching the including file's directory, then `-I`)
//!   and `#include <file>` (searching `-I` only), including macro-expanded
//!   header names;
//! - `#define`, `#undef`, object-like and function-like macros, `#` and `##`,
//!   `__VA_ARGS__`;
//! - `#if`, `#ifdef`, `#ifndef`, `#elif`, `#else`, `#endif`, with `defined`,
//!   C integer precedence, `intmax_t`/`uintmax_t` arithmetic, and
//!   short-circuit evaluation;
//! - `#pragma once`; other benign pragmas are ignored, while `#pragma pack` is
//!   diagnosed because struct/union packing is not implemented;
//! - `#error`. `#warning` is ignored because the public API has no warning
//!   channel.
//!
//! The predefined macros are `__shy__` (initially `1`), `__FILE__`, and
//! `__LINE__`. `-D` definitions and `-U` removals are applied before the root
//! file is read. An empty `-D` replacement defines an empty macro.
//!
//! Explicitly unsupported (diagnosed rather than miscompiled):
//!
//! - `#line` and GNU line markers;
//! - `#include_next`, `_Pragma`, `__has_include`, and `BitInt`-style C23
//!   preprocessor additions;
//! - GNU named variadic parameters (`#define f(args...)`); unnamed `...` and
//!   `__VA_ARGS__` work;
//! - GNU comma swallowing (`f(x, ## __VA_ARGS__)` with no variable
//!   arguments); a placemarker leaves the comma in place;
//! - digraphs (`%:`, `<%`, ...), because the lexer does not produce them;
//! - date/time predefined macros (`__DATE__`, `__TIME__`);
//! - casts and `sizeof` inside `#if`;
//! - multi-character character constants in `#if`.
//!
//! Include recursion is not special-cased: an include guard makes the second
//! inclusion harmless, while a genuinely unguarded cycle eventually stops at
//! [`MAX_INCLUDE_DEPTH`] with a diagnostic that names the include chain.

mod attrs;
mod directive;
mod expand;
mod expr;
mod macros;
mod value;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use crate::lexer::{self, Token, TokenKind};
use crate::source::{Diagnostic, FileId, SourceMap, Span};
use macros::MacroTable;

/// Maximum number of nested `#include` operations.
const MAX_INCLUDE_DEPTH: usize = 200;

/// Maximum nesting of macro-argument expansion.
const MAX_EXPANSION_DEPTH: usize = 200;

/// Preprocessor configuration. [`Default`] is an empty configuration.
#[derive(Clone, Debug, Default)]
pub struct Options {
    /// `-I` directories, searched in order.
    pub include_dirs: Vec<PathBuf>,
    /// `-D` definitions as `(name, replacement)`. The replacement is used
    /// verbatim; an empty string defines an empty macro. The command line
    /// driver is responsible for turning `-DNAME` into `(NAME, "1")` and
    /// `-DNAME=` into `(NAME, "")`.
    pub defines: Vec<(String, String)>,
    /// `-U` names. Applied after [`Options::defines`].
    pub undefines: Vec<String>,
}

/// Shy metadata collected from `#![...]` lines in the main input file.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Attributes {
    /// `#![no_main]`.
    pub no_main: bool,
    /// `#![mem(...)]`, in bytes.
    pub memory: Option<u32>,
    /// `#![stack(...)]`, in bytes.
    pub stack: Option<u32>,
}

/// The result of preprocessing one translation unit.
#[derive(Debug)]
pub struct Output {
    /// Expanded tokens without newlines, ending in exactly one `Eof`.
    pub tokens: Vec<Token>,
    /// Shy metadata from the main input file.
    pub attributes: Attributes,
}

/// Preprocesses `file` and returns its expanded token stream and metadata.
pub fn preprocess(
    sources: &mut SourceMap,
    file: FileId,
    options: &Options,
) -> Result<Output, Diagnostic> {
    let root_key = canonical_key(&sources.file(file).path);
    let mut preprocessor = Preprocessor {
        sources,
        options,
        root: file,
        macros: MacroTable::default(),
        output: Vec::new(),
        attributes: Attributes::default(),
        include_stack: Vec::new(),
        file_ids: HashMap::from([(root_key.clone(), file)]),
        file_keys: HashMap::from([(file, root_key)]),
        token_cache: HashMap::new(),
        once_files: HashSet::new(),
        disabled_builtins: HashSet::new(),
        expansion_depth: 0,
    };
    preprocessor.prepare()?;

    let tokens = preprocessor.lex_file(file)?;
    preprocessor.process_file(file, tokens, true)?;

    let end = preprocessor.sources.file(file).text.len();
    preprocessor.output.push(Token {
        kind: TokenKind::Eof,
        span: Span {
            file,
            start: end,
            end,
        },
        at_line_start: false,
        leading_space: false,
    });
    Ok(Output {
        tokens: preprocessor.output,
        attributes: preprocessor.attributes,
    })
}

/// State shared by the directive, expansion, and expression modules.
pub(crate) struct Preprocessor<'a> {
    pub(crate) sources: &'a mut SourceMap,
    pub(crate) options: &'a Options,
    pub(crate) root: FileId,
    pub(crate) macros: MacroTable,
    pub(crate) output: Vec<Token>,
    pub(crate) attributes: Attributes,
    pub(crate) include_stack: Vec<FileId>,
    /// Canonical path -> source id, so the same header has one [`FileId`].
    pub(crate) file_ids: HashMap<PathBuf, FileId>,
    /// Source id -> canonical path.
    pub(crate) file_keys: HashMap<FileId, PathBuf>,
    pub(crate) token_cache: HashMap<FileId, Rc<Vec<Token>>>,
    pub(crate) once_files: HashSet<PathBuf>,
    pub(crate) disabled_builtins: HashSet<String>,
    pub(crate) expansion_depth: usize,
}

impl Preprocessor<'_> {
    /// Installs the predefined macros and the `-D`/`-U` options.
    fn prepare(&mut self) -> Result<(), Diagnostic> {
        let span = Span {
            file: self.root,
            start: 0,
            end: 0,
        };
        let value = Token {
            kind: TokenKind::Number("1".to_string()),
            span,
            at_line_start: false,
            leading_space: false,
        };
        self.macros.insert(
            "__shy__".to_string(),
            macros::Macro::object(vec![value], span),
        );

        for (name, value) in &self.options.defines {
            if !is_identifier(name) {
                return Err(Diagnostic::new(
                    span,
                    format!("invalid macro name `{name}` in a -D option"),
                ));
            }
            let text = value.clone();
            let body = self.lex_synthetic_body("command line", &text)?;
            self.macros
                .insert(name.clone(), macros::Macro::object(body, span));
        }
        for name in &self.options.undefines {
            self.remove_macro(name);
        }
        Ok(())
    }

    /// True when `name` is defined as a macro or as a builtin.
    pub(crate) fn is_defined(&self, name: &str) -> bool {
        self.macros.contains(name)
            || (!self.disabled_builtins.contains(name) && matches!(name, "__FILE__" | "__LINE__"))
    }

    /// Removes a macro and disables a builtin of the same name.
    pub(crate) fn remove_macro(&mut self, name: &str) {
        self.macros.remove(name);
        if matches!(name, "__FILE__" | "__LINE__") {
            self.disabled_builtins.insert(name.to_string());
        }
    }

    /// Lexes a source file once and caches the token vector.
    pub(crate) fn lex_file(&mut self, file: FileId) -> Result<Rc<Vec<Token>>, Diagnostic> {
        if let Some(tokens) = self.token_cache.get(&file) {
            return Ok(Rc::clone(tokens));
        }
        let tokens = Rc::new(lexer::lex(self.sources, file)?);
        self.token_cache.insert(file, Rc::clone(&tokens));
        Ok(tokens)
    }

    /// Lexes replacement text that did not come from a user file. The
    /// synthetic file keeps every generated token's span valid.
    pub(crate) fn lex_synthetic(
        &mut self,
        purpose: &str,
        text: String,
    ) -> Result<Vec<Token>, Diagnostic> {
        let path = PathBuf::from(format!("<preprocessor {purpose}>"));
        let id = self.sources.add(path, text);
        lexer::lex(self.sources, id)
    }

    /// Lexes command-line replacement text, dropping the trailing newline and
    /// EOF token.
    fn lex_synthetic_body(&mut self, purpose: &str, text: &str) -> Result<Vec<Token>, Diagnostic> {
        let tokens = self.lex_synthetic(purpose, text.to_string())?;
        Ok(tokens
            .into_iter()
            .filter(|token| !matches!(token.kind, TokenKind::Newline | TokenKind::Eof))
            .collect())
    }

    /// Processes one file. `root` is true only for the main input file, where
    /// `#![...]` attributes are allowed.
    fn process_file(
        &mut self,
        file: FileId,
        tokens: Rc<Vec<Token>>,
        root: bool,
    ) -> Result<(), Diagnostic> {
        self.include_stack.push(file);
        let result = self.process_file_body(file, &tokens, root);
        self.include_stack.pop();
        result
    }

    fn process_file_body(
        &mut self,
        file: FileId,
        tokens: &[Token],
        root: bool,
    ) -> Result<(), Diagnostic> {
        let mut conditions: Vec<Conditional> = Vec::new();
        let mut i = 0;
        while i < tokens.len() {
            let token = &tokens[i];
            match token.kind {
                TokenKind::Eof => break,
                TokenKind::Newline => {
                    i += 1;
                    continue;
                }
                _ => {}
            }

            if is_hash(token) && token.at_line_start {
                let active = conditional_active(&conditions);
                if is_attribute_start(tokens, i) {
                    let end = line_end(tokens, i);
                    if active {
                        if !root {
                            return Err(Diagnostic::new(
                                token.span,
                                "Shy #![...] attributes are only allowed in the main input file",
                            ));
                        }
                        self.handle_attribute(&tokens[i..end], token.span)?;
                    }
                    i = end;
                    continue;
                }

                let Some(name_token) = tokens.get(i + 1) else {
                    break;
                };
                let end = line_end(tokens, i + 1);
                if matches!(name_token.kind, TokenKind::Newline | TokenKind::Eof) {
                    // A line containing only `#` is a null directive.
                    i = end;
                    continue;
                }

                if let TokenKind::Ident(name) = &name_token.kind {
                    let arguments = &tokens[i + 2..end];
                    if let Some(kind) = ConditionalKind::from_name(name) {
                        self.handle_conditional(kind, arguments, token.span, &mut conditions)?;
                        i = end;
                        continue;
                    }
                    if !active {
                        i = end;
                        continue;
                    }
                    self.handle_directive(file, name, arguments, token.span)?;
                    i = end;
                    continue;
                }

                if active {
                    if matches!(name_token.kind, TokenKind::Number(_)) {
                        return Err(Diagnostic::new(
                            token.span,
                            "#line directives and GNU line markers are not supported",
                        ));
                    }
                    return Err(Diagnostic::new(
                        token.span,
                        "invalid preprocessing directive",
                    ));
                }
                i = end;
                continue;
            }

            if !conditional_active(&conditions) {
                i += 1;
                continue;
            }

            let mut expanded = Vec::new();
            self.expand_stream(tokens, &mut i, &mut expanded)?;
            self.output.extend(expanded.into_iter().map(|expanded| {
                let span = expanded.span();
                let mut token = expanded.token;
                token.span = span;
                token
            }));
        }

        if let Some(frame) = conditions.last() {
            return Err(Diagnostic::new(
                frame.span,
                "unterminated conditional directive (missing #endif)",
            ));
        }
        Ok(())
    }

    fn handle_conditional(
        &mut self,
        kind: ConditionalKind,
        arguments: &[Token],
        span: Span,
        conditions: &mut Vec<Conditional>,
    ) -> Result<(), Diagnostic> {
        match kind {
            ConditionalKind::If => {
                let parent = conditional_active(conditions);
                let value = if parent {
                    self.eval_condition(arguments, span)?
                } else {
                    false
                };
                conditions.push(Conditional {
                    span,
                    parent_active: parent,
                    active: parent && value,
                    taken: parent && value,
                    saw_else: false,
                });
            }
            ConditionalKind::Ifdef | ConditionalKind::Ifndef => {
                let negate = matches!(kind, ConditionalKind::Ifndef);
                let [
                    Token {
                        kind: TokenKind::Ident(name),
                        ..
                    },
                ] = arguments
                else {
                    return Err(Diagnostic::new(
                        span,
                        format!(
                            "expected a single macro name after #{}",
                            if negate { "ifndef" } else { "ifdef" }
                        ),
                    ));
                };
                let parent = conditional_active(conditions);
                let value = self.is_defined(name) ^ negate;
                conditions.push(Conditional {
                    span,
                    parent_active: parent,
                    active: parent && value,
                    taken: parent && value,
                    saw_else: false,
                });
            }
            ConditionalKind::Elif => {
                let Some(index) = conditions.len().checked_sub(1) else {
                    return Err(Diagnostic::new(span, "unmatched #elif"));
                };
                if conditions[index].saw_else {
                    return Err(Diagnostic::new(span, "#elif after #else"));
                }
                if arguments.is_empty() {
                    return Err(Diagnostic::new(span, "expected an expression after #elif"));
                }
                let parent = conditions[index].parent_active;
                let taken = conditions[index].taken;
                let value = if parent && !taken {
                    self.eval_condition(arguments, span)?
                } else {
                    false
                };
                let frame = &mut conditions[index];
                frame.active = parent && !taken && value;
                frame.taken = taken || frame.active;
            }
            ConditionalKind::Else => {
                let Some(frame) = conditions.last_mut() else {
                    return Err(Diagnostic::new(span, "unmatched #else"));
                };
                if frame.saw_else {
                    return Err(Diagnostic::new(span, "duplicate #else"));
                }
                frame.saw_else = true;
                frame.active = frame.parent_active && !frame.taken;
                frame.taken = frame.taken || frame.active;
            }
            ConditionalKind::Endif => {
                if conditions.pop().is_none() {
                    return Err(Diagnostic::new(span, "unmatched #endif"));
                }
            }
        }
        Ok(())
    }
}

/// One open `#if`/`#ifdef`/`#ifndef` group.
struct Conditional {
    span: Span,
    /// The enclosing condition was active when this group began.
    parent_active: bool,
    /// The current branch is active.
    active: bool,
    /// Some branch of this group has already been taken.
    taken: bool,
    saw_else: bool,
}

#[derive(Clone, Copy)]
enum ConditionalKind {
    If,
    Ifdef,
    Ifndef,
    Elif,
    Else,
    Endif,
}

impl ConditionalKind {
    fn from_name(name: &str) -> Option<Self> {
        match name {
            "if" => Some(Self::If),
            "ifdef" => Some(Self::Ifdef),
            "ifndef" => Some(Self::Ifndef),
            "elif" => Some(Self::Elif),
            "else" => Some(Self::Else),
            "endif" => Some(Self::Endif),
            _ => None,
        }
    }
}

fn conditional_active(conditions: &[Conditional]) -> bool {
    conditions.last().is_none_or(|condition| condition.active)
}

fn is_hash(token: &Token) -> bool {
    macros::is_punct(token, "#")
}

fn is_attribute_start(tokens: &[Token], index: usize) -> bool {
    let Some(bang) = tokens.get(index + 1) else {
        return false;
    };
    let Some(bracket) = tokens.get(index + 2) else {
        return false;
    };
    macros::is_punct(bang, "!") && macros::is_punct(bracket, "[")
}

/// Index of the newline (or EOF) that ends the line starting at `start`.
fn line_end(tokens: &[Token], start: usize) -> usize {
    let mut index = start;
    while index < tokens.len() {
        match tokens[index].kind {
            TokenKind::Newline | TokenKind::Eof => return index,
            _ => index += 1,
        }
    }
    tokens.len()
}

/// Identity for include files: a canonical path when the file exists.
pub(crate) fn canonical_key(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn is_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first == '_' || first.is_alphabetic())
        && chars.all(|character| character == '_' || character.is_alphanumeric())
}
