//! One compiler run for one editor buffer.
//!
//! The language server does not have a second grammar. It feeds the open
//! buffer through the same lexer, preprocessor, parser, and lowering pass the
//! `shyc` driver uses. Everything an editor feature needs comes from that
//! result: diagnostics, the syntax tree, and the symbol index.

use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use shyc::abi::AbiKind;
use shyc::ast;
use shyc::ir::Layout;
use shyc::lexer::{self, Token, TokenKind};
use shyc::lower::{self, StartupMode};
use shyc::parser;
use shyc::preprocess;
use shyc::source::{Diagnostic, FileId, SourceMap, Span};

use tower_lsp::lsp_types::Url;

use crate::index::{self, Index};
use crate::probe::Answers as ProbeAnswers;
use crate::text::{self, LineIndex};

#[derive(Clone, Debug)]
pub struct Settings {
    pub include_dirs: Vec<PathBuf>,
    /// Root of ShyProject/libshy. Configured separately for users editing an
    /// application repository that is not nested inside ShyProject.
    pub libshy_path: Option<PathBuf>,
    /// The `shycc` driver to run probe rounds with. `None` searches the
    /// workspace and the server's own directory.
    pub shycc_path: Option<PathBuf>,
    pub defines: Vec<(String, String)>,
    pub diagnostics: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            include_dirs: Vec::new(),
            libshy_path: None,
            shycc_path: None,
            defines: Vec::new(),
            diagnostics: true,
        }
    }
}

pub struct Analysis {
    pub uri: Url,
    pub version: Option<i32>,
    pub text: String,
    pub lines: LineIndex,
    pub sources: SourceMap,
    pub root: FileId,
    pub raw_tokens: Vec<Token>,
    pub unit: Option<ast::TranslationUnit>,
    pub diagnostics: Vec<Diagnostic>,
    pub index: Index,
    /// The include search path this parse used. Go-to-definition on a `use`
    /// line names a module file, not a symbol, so it has to search the same
    /// directories the preprocessor searched.
    pub include_dirs: Vec<PathBuf>,
    /// The probe answers this parse used, so the cache can tell two parses of
    /// the same text apart.
    pub answers: Option<Arc<ProbeAnswers>>,
}

impl Analysis {
    pub fn build(
        uri: Url,
        path: PathBuf,
        text: String,
        version: Option<i32>,
        settings: &Settings,
        root: Option<&Path>,
        answers: Option<Arc<ProbeAnswers>>,
    ) -> Arc<Analysis> {
        let lines = LineIndex::new(&text);
        let mut sources = SourceMap::default();
        let file = sources.add(path.clone(), text.clone());

        let mut diagnostics = Vec::new();
        // Highlighting keeps the tokens read so far, so a half-typed literal
        // only blanks its own line instead of the whole file.
        let (raw_tokens, lex_error) = lexer::lex_partial(&sources, file);
        diagnostics.extend(lex_error);

        let mut defines = settings.defines.clone();
        // The data model the `shyc` binary gives its own preprocessor for the
        // Shy target. The probe runs that binary, so the editor must feed the
        // same macros or the two parses number their source files differently.
        defines.push(("__SIZEOF_POINTER__".to_string(), "4".to_string()));
        let options = preprocess::Options {
            include_dirs: search_dirs(&path, root, settings),
            defines,
            undefines: Vec::new(),
        };
        let include_dirs = options.include_dirs.clone();

        let mut attributes = preprocess::Attributes::default();
        let mut sites = Vec::new();
        // Spans of the items a probe answer generated, for the index to format
        // their signatures from the tree instead of the generator call text.
        let mut generated = Vec::new();
        // A call the probe answered expands to its real items and values; an
        // unanswered one stays a placeholder that only exists to let the rest
        // of the file parse.
        let mut unresolved = false;
        // A buffer being edited is usually missing the statement under the
        // cursor. The recovered tree is only for the index: lowering it would
        // check code the user has not finished writing.
        let mut recovered = false;
        let unit = match preprocess::preprocess(&mut sources, file, &options) {
            Ok(output) => {
                attributes = output.attributes;
                let tokens = Rc::new(output.tokens);
                match parse_with_answers(&tokens, answers.as_deref()) {
                    Ok((parsed, spans)) => {
                        sites = parsed.sites;
                        generated = spans;
                        unresolved = !parsed.requests.is_empty();
                        Some(parsed.unit)
                    }
                    Err(error) => {
                        diagnostics.push(error.clone());
                        match recover_parse(&tokens, file, &error, answers.as_deref()) {
                            Some((parsed, spans)) => {
                                sites = parsed.sites;
                                generated = spans;
                                recovered = true;
                                Some(parsed.unit)
                            }
                            None => None,
                        }
                    }
                }
            }
            Err(error) => {
                diagnostics.push(error);
                None
            }
        };

        // Lowering a placeholder reports errors about the stand-in, not about
        // the program, so wait until the probe has answered every call.
        if !unresolved && !recovered {
            if let Some(unit) = &unit {
                let lower_options = lower::Options {
                    module_name: path
                        .file_stem()
                        .and_then(|stem| stem.to_str())
                        .unwrap_or("module")
                        .to_string(),
                    layout: Layout::SHY,
                    startup: if attributes.no_main {
                        StartupMode::Manual
                    } else {
                        StartupMode::Main
                    },
                    abi: AbiKind::ShyV1,
                    disable_nonexhaustive_switch: attributes.disable_nonexhaustive_switch,
                    disable_rustic_mut: attributes.disable_rustic_mut,
                    disable_default_static: attributes.disable_default_static,
                    allow_defer_copy: attributes.allow_defer_copy,
                    exports: Vec::new(),
                };
                match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    lower::lower(unit, &lower_options)
                })) {
                    Ok(Ok(_)) => {}
                    Ok(Err(error)) => diagnostics.push(error),
                    Err(_) => diagnostics.push(Diagnostic::new(
                        Span {
                            file,
                            start: 0,
                            end: 0,
                        },
                        "the compiler panicked while checking this file",
                    )),
                }
            }
        }

        let index = unit
            .as_ref()
            .map(|unit| index::build(&sources, unit, &sites, &generated))
            .unwrap_or_default();

        Arc::new(Analysis {
            uri,
            version,
            text,
            lines,
            sources,
            root: file,
            raw_tokens,
            unit,
            diagnostics,
            index,
            include_dirs,
            answers,
        })
    }

    pub fn range(&self, span: Span) -> tower_lsp::lsp_types::Range {
        let file = self.sources.file(span.file);
        let lines = LineIndex::new(&file.text);
        lines.range(&file.text, span.start, span.end)
    }

    pub fn uri_of(&self, file: FileId) -> Option<Url> {
        text::uri_for_path(&self.sources.file(file).path)
    }

    pub fn target_at(&self, offset: usize) -> Option<usize> {
        self.index.definition_at(self.root, offset)
    }
}

/// Parse once to learn where the unanswered calls stand, then again with the
/// probe answers carrying the span of the call each one belongs to.
fn parse_with_answers(
    tokens: &parser::TokenStream,
    answers: Option<&ProbeAnswers>,
) -> Result<(parser::ParseOutput, Vec<Span>), Diagnostic> {
    let parsed = parser::parse_for_probe(tokens, parser::Answers::default())?;
    match answers {
        Some(answers) => {
            let (answers, generated) = answers.build(&parsed);
            let parsed = parser::parse_for_probe(tokens, answers)?;
            Ok((parsed, generated))
        }
        None => Ok((parsed, Vec::new())),
    }
}

/// Parses the longest prefix that still parses when the whole buffer does not.
///
/// While a statement is being typed, everything above it is still valid. The
/// parser stops at the first error, so without this fallback every editor
/// feature would go blank until the line is finished. The unfinished tail is
/// dropped, the blocks it leaves open are closed, and the resulting tree only
/// feeds the symbol index; the caller keeps reporting the real error.
fn recover_parse(
    tokens: &parser::TokenStream,
    file: FileId,
    error: &Diagnostic,
    answers: Option<&ProbeAnswers>,
) -> Option<(parser::ParseOutput, Vec<Span>)> {
    if error.span.file != file {
        return None;
    }
    let error_index = tokens
        .iter()
        .position(|token| token.span.file == file && token.span.start >= error.span.start)
        .unwrap_or(tokens.len());
    let span = Span {
        file,
        start: error.span.start,
        end: error.span.start,
    };

    let mut attempts = 0;
    for cut in (0..error_index).rev().filter(|&index| {
        matches!(&tokens[index].kind, TokenKind::Punct(punct) if punct == ";" || punct == "}")
    }) {
        attempts += 1;
        if attempts > 16 {
            break;
        }
        let mut prefix = tokens[..=cut].to_vec();
        close_delimiters(&mut prefix, span);
        prefix.push(Token {
            kind: TokenKind::Eof,
            span,
            at_line_start: false,
            leading_space: false,
        });
        if let Ok(parsed) = parse_with_answers(&Rc::new(prefix), answers) {
            return Some(parsed);
        }
    }
    None
}

/// Appends the closers of every `{`, `(`, and `[` the prefix leaves open.
fn close_delimiters(tokens: &mut Vec<Token>, span: Span) {
    let mut open: Vec<&'static str> = Vec::new();
    for token in tokens.iter() {
        match &token.kind {
            TokenKind::Punct(punct) if punct == "{" => open.push("}"),
            TokenKind::Punct(punct) if punct == "(" => open.push(")"),
            TokenKind::Punct(punct) if punct == "[" => open.push("]"),
            TokenKind::Punct(punct) if punct == "}" || punct == ")" || punct == "]" => {
                open.pop();
            }
            _ => {}
        }
    }
    for closer in open.into_iter().rev() {
        tokens.push(Token {
            kind: TokenKind::Punct(closer.to_string()),
            span,
            at_line_start: false,
            leading_space: false,
        });
    }
}

/// The include search path of one buffer. A probe round runs the compiler in
/// another process, so it needs the same directories spelled on its command
/// line.
pub(crate) fn search_dirs(path: &Path, root: Option<&Path>, settings: &Settings) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    // The same order the `shyc` binary builds: the compiler's bundled headers,
    // the configured library layers, then the project's own `-I` directories.
    let configured_libshy = shyc::config::shylib();
    if let Some(compiler) = configured_libshy
        .as_deref()
        .and_then(Path::parent)
        .map(|root| root.join("shyc/include"))
    {
        push_dir(&mut dirs, &compiler);
    }
    let libshy = settings
        .libshy_path
        .as_deref()
        .map(|path| {
            // Accept the repository root as well as the library directory.
            if path.join("libshy").is_dir() {
                path.join("libshy")
            } else {
                path.to_path_buf()
            }
        })
        .or(configured_libshy);
    if let Some(libshy) = libshy {
        push_dir(&mut dirs, &libshy.join("c"));
        push_dir(&mut dirs, &libshy);
        push_dir(&mut dirs, &libshy.join("include"));
    }
    for dir in include_dirs(path, root, &settings.include_dirs) {
        push_dir(&mut dirs, &dir);
    }
    dirs
}

fn include_dirs(path: &Path, root: Option<&Path>, extra: &[PathBuf]) -> Vec<PathBuf> {
    let mut dirs = Vec::new();

    let base = root.or_else(|| path.parent());
    for dir in extra {
        let dir = if dir.is_absolute() {
            dir.clone()
        } else if let Some(base) = base {
            base.join(dir)
        } else {
            dir.clone()
        };
        push_dir(&mut dirs, &dir);
    }

    if let Some(root) = root {
        push_dir(&mut dirs, root);
    }

    // `crate::` imports are rooted at the repository, not at whichever
    // workspace folder the editor happened to open.
    if let Some(root) = repo_root(path) {
        push_dir(&mut dirs, &root);
    }

    let mut ancestors: Vec<&Path> = path.ancestors().skip(1).collect();
    if let Some(root) = root {
        if !ancestors.contains(&root) {
            ancestors.push(root);
        }
    }
    // Same lookup order as the Makefile's `-I` flags: the generated `c/`
    // headers come first, the flat `include/` compatibility wrappers last.
    // Reversing them pins <stdio.h> to a stale wrapper.
    for relative in ["libshy/c", "libshy", "include", "user"] {
        for ancestor in &ancestors {
            push_dir(&mut dirs, &ancestor.join(relative));
        }
    }
    for ancestor in &ancestors {
        push_dir(&mut dirs, &ancestor.join("libshy/include"));
    }

    dirs
}

fn push_dir(dirs: &mut Vec<PathBuf>, dir: &Path) {
    if !dir.is_dir() {
        return;
    }
    let dir = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
    if !dirs.contains(&dir) {
        dirs.push(dir);
    }
}

/// The repository that owns `path`: the nearest ancestor holding `.git` or a
/// `Cargo.toml`. This is how the compiler resolves `crate::` imports too.
fn repo_root(path: &Path) -> Option<PathBuf> {
    path.ancestors()
        .skip(1)
        .find(|directory| directory.join(".git").exists() || directory.join("Cargo.toml").is_file())
        .map(Path::to_path_buf)
}
