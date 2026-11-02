//! One compiler run for one editor buffer.
//!
//! The language server does not have a second grammar. It feeds the open
//! buffer through the same lexer, preprocessor, parser, and lowering pass the
//! `shyc` driver uses. Everything an editor feature needs comes from that
//! result: diagnostics, the syntax tree, and the symbol index.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use shyc::abi::AbiKind;
use shyc::ast;
use shyc::ir::Layout;
use shyc::lexer::{self, Token};
use shyc::lower::{self, StartupMode};
use shyc::parser;
use shyc::preprocess;
use shyc::source::{Diagnostic, FileId, SourceMap, Span};

use tower_lsp::lsp_types::Url;

use crate::index::{self, Index};
use crate::text::{self, LineIndex};

#[derive(Clone, Debug)]
pub struct Settings {
    pub include_dirs: Vec<PathBuf>,
    /// Root of ShyProject/libshy. Configured separately for users editing an
    /// application repository that is not nested inside ShyProject.
    pub libshy_path: Option<PathBuf>,
    pub defines: Vec<(String, String)>,
    pub diagnostics: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            include_dirs: Vec::new(),
            libshy_path: None,
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
}

impl Analysis {
    pub fn build(
        uri: Url,
        path: PathBuf,
        text: String,
        version: Option<i32>,
        settings: &Settings,
        root: Option<&Path>,
    ) -> Arc<Analysis> {
        let lines = LineIndex::new(&text);
        let mut sources = SourceMap::default();
        let file = sources.add(path.clone(), text.clone());

        let mut diagnostics = Vec::new();
        let raw_tokens = lexer::lex(&sources, file).unwrap_or_else(|error| {
            diagnostics.push(error);
            Vec::new()
        });

        let mut include_dirs = include_dirs(&path, root, &settings.include_dirs);
        if let Some(libshy) = settings.libshy_path.as_deref() {
            // Accept the repository root as well as the library directory.
            let libshy = if libshy.join("libshy").is_dir() {
                libshy.join("libshy")
            } else {
                libshy.to_path_buf()
            };
            push_dir(&mut include_dirs, &libshy);
            push_dir(&mut include_dirs, &libshy.join("c"));
            push_dir(&mut include_dirs, &libshy.join("include"));
        }
        let options = preprocess::Options {
            include_dirs,
            defines: settings.defines.clone(),
            undefines: Vec::new(),
        };

        let mut attributes = preprocess::Attributes::default();
        let unit = match preprocess::preprocess(&mut sources, file, &options) {
            Ok(output) => {
                attributes = output.attributes;
                match parser::parse(&output.tokens) {
                    Ok(unit) => Some(unit),
                    Err(error) => {
                        diagnostics.push(error);
                        None
                    }
                }
            }
            Err(error) => {
                diagnostics.push(error);
                None
            }
        };

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

        let index = unit
            .as_ref()
            .map(|unit| index::build(&sources, unit))
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
    for ancestor in ancestors {
        for relative in ["libshy/c", "libshy", "libshy/include", "include", "user"] {
            push_dir(&mut dirs, &ancestor.join(relative));
        }
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
        .find(|directory| {
            directory.join(".git").exists() || directory.join("Cargo.toml").is_file()
        })
        .map(Path::to_path_buf)
}
