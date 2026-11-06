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
//! `#![stack(4K)]`, `#![disable_nonexhaustive_switch]`,
//! `#![disable_rustic_mut]`) are consumed from the main input file and returned
//! in [`Output::attributes`]. They are only recognised at the start of a line.
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
    /// `#![disable_nonexhaustive_switch]`: allow a `switch` on a plain C enum
    /// without a `default` label. Payload enums stay closed.
    pub disable_nonexhaustive_switch: bool,
    /// `#![disable_rustic_mut]`: keep ordinary C writable defaults, so a `let`
    /// local without `mut` may be assigned. This is a checking choice only; the
    /// ABI and object layout are unchanged either way.
    pub disable_rustic_mut: bool,
    /// `#![disable_default_static]`: restore the C linkage default, so only
    /// `static` gives a file-local symbol. Without it a Shy `fn` or method is
    /// file-local by default.
    pub disable_default_static: bool,
    /// `#![allow(defer_copy)]`: no by-value copy warning for this unit.
    pub allow_defer_copy: bool,
}

/// One `#include` directive written in the main input file.
///
/// `--emit=c-header` carries these lines into the generated header so a C
/// consumer sees the same dependencies, in the same order, as the Shy source.
/// Includes of included files are not recorded: they are an implementation
/// detail of the header that was included, not of the main file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Include {
    /// Header name exactly as written: `stddef.h` or `c/stdio.shyh`.
    pub path: String,
    /// True for `#include "file"`, false for `#include <file>`.
    pub quoted: bool,
}

/// The result of preprocessing one translation unit.
#[derive(Debug)]
pub struct Output {
    /// Expanded tokens without newlines, ending in exactly one `Eof`.
    pub tokens: Vec<Token>,
    /// Shy metadata from the main input file.
    pub attributes: Attributes,
    /// `#include` directives of the main input file, in source order.
    pub includes: Vec<Include>,
    /// `#define` lines of the main input file, in source order, as C text.
    pub defines: Vec<String>,
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
        includes: Vec::new(),
        defines: Vec::new(),
        include_stack: Vec::new(),
        file_ids: HashMap::from([(root_key.clone(), file)]),
        file_keys: HashMap::from([(file, root_key)]),
        token_cache: HashMap::new(),
        once_files: HashSet::new(),
        guard_macro: None,
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
        includes: preprocessor.includes,
        defines: preprocessor.defines,
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
    /// Main-file `#include` directives, filled by [`directive`].
    pub(crate) includes: Vec<Include>,
    /// Main-file `#define` lines, filled by [`directive`].
    pub(crate) defines: Vec<String>,
    pub(crate) include_stack: Vec<FileId>,
    /// Canonical path -> source id, so the same header has one [`FileId`].
    pub(crate) file_ids: HashMap<PathBuf, FileId>,
    /// Source id -> canonical path.
    pub(crate) file_keys: HashMap<FileId, PathBuf>,
    pub(crate) token_cache: HashMap<FileId, Rc<Vec<Token>>>,
    pub(crate) once_files: HashSet<PathBuf>,
    /// The main file's own include guard, kept out of generated C headers.
    pub(crate) guard_macro: Option<String>,
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
        if root {
            // A generated C header must not re-export the source header's
            // include guard: the two views are separate files, and defining the
            // same macro in both would make the later include a no-op.
            self.guard_macro = include_guard(tokens);
        }
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

            // `// #![...]`：C 文件里没法写裸属性，注释形式对 C 工具完全无害，
            // 而 shyc 照样认。行首、`//`、然后是 `#![`（词法器不产生空格 token）。
            if token.at_line_start && macros::is_punct(token, "/") {
                if let Some(hash) = comment_attribute_hash(tokens, i) {
                    let end = line_end(tokens, hash);
                    let active = conditional_active(&conditions);
                    if active {
                        if !root {
                            return Err(Diagnostic::new(
                                tokens[hash].span,
                                "Shy #![...] attributes are only allowed in the main input file",
                            ));
                        }
                        self.handle_attribute(&tokens[hash..end], tokens[hash].span)?;
                    }
                    i = end;
                    continue;
                }
            }

            // `use module::Name;` is both an import declaration for semantic
            // checking and an include of the module file before parsing.
            if matches!(&token.kind, TokenKind::Ident(name) if name == "use") {
                let end = use_statement_end(tokens, i);
                if conditional_active(&conditions)
                    && let Some(import) = parse_use_include(tokens, i, end)
                {
                    let source_path = if import.quoted {
                        let current_path = canonical_key(&self.sources.file(file).path);
                        current_path
                            .parent()
                            .unwrap_or(Path::new("."))
                            .join(&import.source_path)
                    } else if import.crate_root {
                        let input = canonical_key(&self.sources.file(self.root).path);
                        let root = input.ancestors().skip(1).find(|directory| {
                            directory.join(".git").exists()
                                || directory.join("Cargo.toml").is_file()
                        });
                        if let Some(root) = root {
                            module_header(root.join(&import.source_path))
                        } else if let Some(path) = self
                            .options
                            .include_dirs
                            .iter()
                            .map(|root| module_header(root.join(&import.source_path)))
                            .find(|path| path.is_file())
                        {
                            canonical_key(&path)
                        } else {
                            input
                                .parent()
                                .unwrap_or(Path::new("."))
                                .join(&import.source_path)
                        }
                    } else {
                        // Module paths are include-root-relative, not relative
                        // to an arbitrary file beside the importing source.
                        let path = self
                            .options
                            .include_dirs
                            .iter()
                            .map(|root| module_header(root.join(&import.source_path)))
                            .find(|path| path.is_file());
                        path.map(|path| canonical_key(&path)).ok_or_else(|| {
                            Diagnostic::new(
                                token.span,
                                format!("use module not found: '{}'", import.source_path),
                            )
                        })?
                    };
                    let source_path = module_header(source_path);
                    let include_token = Token {
                        kind: TokenKind::StringLiteral {
                            prefix: String::new(),
                            raw: source_path.to_string_lossy().into_owned(),
                        },
                        span: token.span,
                        at_line_start: false,
                        leading_space: false,
                    };
                    // Save the slot before recursion: included headers can
                    // contain their own imports, but only root includes belong
                    // in the generated C header.
                    let include_index = self.includes.len();
                    self.handle_include(file, &[include_token], token.span)?;
                    if file == self.root {
                        self.includes[include_index].path = import.header_path;
                        self.includes[include_index].quoted = import.quoted || import.crate_root;
                    }
                    self.output.extend(tokens[i..end].iter().cloned());
                    i = end;
                    continue;
                }
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

                // `#[ident]`：声明级属性（`#[result_niche]`），交给 parser 处理。
                if tokens
                    .get(i + 1)
                    .is_some_and(|token| macros::is_punct(token, "["))
                {
                    let end = line_end(tokens, i);
                    let active = conditional_active(&conditions);
                    if active {
                        self.output.extend(tokens[i..end].iter().cloned());
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

/// `//` 注释里写属性的起始位置：`/` `/` 之后任意个空格，然后 `#![`。
///
/// 返回 `#` 的下标。这样 C 兼容的源码可以保持纯 C 语法，文件级标注只存在于
/// 注释里，C 编译器看到的就是一行普通注释。
fn comment_attribute_hash(tokens: &[Token], index: usize) -> Option<usize> {
    if !macros::is_punct(tokens.get(index + 1)?, "/") {
        return None;
    }
    let cursor = index + 2;
    if !is_hash(tokens.get(cursor)?) {
        return None;
    }
    if !is_attribute_start(tokens, cursor) {
        return None;
    }
    Some(cursor)
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

/// The classic `#ifndef NAME` / `#define NAME` include guard of one file.
fn include_guard(tokens: &[Token]) -> Option<String> {
    let significant: Vec<&Token> = tokens
        .iter()
        .filter(|token| !matches!(token.kind, TokenKind::Newline))
        .take(6)
        .collect();
    let is_punct_at = |index: usize, punct: &str| {
        significant
            .get(index)
            .is_some_and(|token| macros::is_punct(token, punct))
    };
    let ident_at = |index: usize| match significant.get(index).map(|token| &token.kind) {
        Some(TokenKind::Ident(name)) => Some(name.as_str()),
        _ => None,
    };
    if !is_punct_at(0, "#") || ident_at(1) != Some("ifndef") {
        return None;
    }
    let guard = ident_at(2)?;
    if !is_punct_at(3, "#") || ident_at(4) != Some("define") || ident_at(5) != Some(guard) {
        return None;
    }
    Some(guard.to_string())
}

/// Prefer native declarations, but permit ordinary C API headers outside c/.
fn module_header(path: PathBuf) -> PathBuf {
    if path.is_file() || path.extension().is_some_and(|ext| ext == "h") {
        path
    } else {
        let c_header = path.with_extension("h");
        if c_header.is_file() { c_header } else { path }
    }
}

struct UseInclude {
    source_path: String,
    header_path: String,
    quoted: bool,
    crate_root: bool,
}

/// Convert a file-scope `use` line to the module file it includes.
///
/// `core::option::Option` and `core::option::*` both load `core/option.shyh`;
/// `c::stdio::*` loads the C view `c/stdio.h`. `crate` paths are rooted at an
/// include directory, while `self` paths are quoted and relative to this file.
fn use_statement_end(tokens: &[Token], start: usize) -> usize {
    (start..tokens.len())
        .find(|&index| {
            matches!(tokens[index].kind, TokenKind::Eof)
                || macros::is_punct(&tokens[index], ";")
                || macros::is_punct(&tokens[index], "{")
        })
        .map_or(tokens.len(), |index| index + 1)
}

fn parse_use_include(tokens: &[Token], start: usize, end: usize) -> Option<UseInclude> {
    let tokens: Vec<&Token> = tokens[start..end]
        .iter()
        .filter(|token| !matches!(token.kind, TokenKind::Newline))
        .collect();
    let end = tokens.len();
    let mut index = 1;
    let mut segments = Vec::new();
    let TokenKind::Ident(first) = &tokens.get(index)?.kind else {
        return None;
    };
    segments.push(first.clone());
    index += 1;
    let mut glob = false;
    while index < end && macros::is_punct(&tokens[index], "::") {
        index += 1;
        if index < end && macros::is_punct(&tokens[index], "*") {
            glob = true;
            index += 1;
            break;
        }
        let TokenKind::Ident(segment) = &tokens.get(index)?.kind else {
            return None;
        };
        segments.push(segment.clone());
        index += 1;
    }
    if !glob {
        // A named import's last segment is the symbol; the rest is its module.
        if segments.len() < 2 {
            return None;
        }
        segments.pop();
    }
    if index >= end || !macros::is_punct(&tokens[index], ";") || index + 1 != end {
        return None;
    }

    let quoted = segments.first().is_some_and(|segment| segment == "self");
    let crate_root = segments.first().is_some_and(|segment| segment == "crate");
    if segments
        .first()
        .is_some_and(|segment| segment == "self" || segment == "crate")
    {
        segments.remove(0);
    }
    if segments.is_empty() {
        return None;
    }
    let is_c = segments.first().is_some_and(|segment| segment == "c");
    let base = segments.join("/");
    let source_path = if is_c {
        format!("{base}.h")
    } else {
        format!("{base}.shyh")
    };
    let header_path = if is_c {
        source_path.clone()
    } else {
        format!("{base}.h")
    };
    Some(UseInclude {
        source_path,
        header_path,
        quoted,
        crate_root,
    })
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
