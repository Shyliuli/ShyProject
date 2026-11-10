//! Language server for ShyC.
//!
//! The server owns no grammar of its own: [`analysis`] runs the `shyc`
//! compiler frontend over the open buffer, and the LSP methods below only
//! present that result in the editor.

mod analysis;
mod index;
mod probe;
mod text;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Deserialize;
use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer, LspService, Server};

use shyc::source::Span;

use crate::analysis::{Analysis, Settings};
use crate::index::SymKind;
use crate::probe::Answers;
use crate::text::LineIndex;

// Semantic token type indexes. They must match `legend()`.
const TOK_TYPE: u32 = 1;
const TOK_ENUM: u32 = 3;
const TOK_INTERFACE: u32 = 4;
const TOK_STRUCT: u32 = 5;
const TOK_TYPE_PARAMETER: u32 = 6;
const TOK_PARAMETER: u32 = 7;
const TOK_VARIABLE: u32 = 8;
const TOK_PROPERTY: u32 = 9;
const TOK_ENUM_MEMBER: u32 = 10;
const TOK_FUNCTION: u32 = 11;
const TOK_METHOD: u32 = 12;
const TOK_MACRO: u32 = 13;
const TOK_KEYWORD: u32 = 14;
const TOK_MODIFIER: u32 = 15;
const TOK_STRING: u32 = 17;
const TOK_NUMBER: u32 = 18;
const TOK_OPERATOR: u32 = 19;

const MOD_DECLARATION: u32 = 1 << 0;

struct Document {
    text: String,
    version: Option<i32>,
    path: PathBuf,
}

/// A probe run waits for the typing to pause before it starts.
const DEBOUNCE: Duration = Duration::from_millis(300);

#[derive(Default)]
struct ProbeState {
    /// The answers of the last finished probe. They stay in use while a probe
    /// for newer text runs, so generated items do not flicker.
    answers: Option<Arc<Answers>>,
    /// Answers already produced for one buffer content.
    done: HashMap<u64, Arc<Answers>>,
    /// Bumped on every edit; a probe for older text must not publish.
    generation: u64,
    /// Content hash a probe is running for, so a save or a repeat edit does
    /// not start a second one.
    running: Option<u64>,
    /// The last failure that named a source location, with its content hash.
    failure: Option<(u64, usize, String)>,
}

struct Backend {
    client: Client,
    documents: Arc<Mutex<HashMap<Url, Document>>>,
    cache: Arc<Mutex<HashMap<Url, Arc<Analysis>>>>,
    settings: Arc<Mutex<Settings>>,
    root: Arc<Mutex<Option<PathBuf>>>,
    published: Arc<Mutex<HashMap<Url, HashSet<Url>>>>,
    probes: Arc<Mutex<HashMap<Url, ProbeState>>>,
}

impl Backend {
    fn new(client: Client) -> Self {
        Backend {
            client,
            documents: Arc::new(Mutex::new(HashMap::new())),
            cache: Arc::new(Mutex::new(HashMap::new())),
            settings: Arc::new(Mutex::new(Settings::default())),
            root: Arc::new(Mutex::new(None)),
            published: Arc::new(Mutex::new(HashMap::new())),
            probes: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    async fn document(&self, uri: &Url) -> Option<(String, Option<i32>, PathBuf)> {
        if let Some((text, version, path)) = {
            let documents = self.documents.lock().unwrap();
            documents.get(uri).map(|document| {
                (
                    document.text.clone(),
                    document.version,
                    document.path.clone(),
                )
            })
        } {
            return Some((text, version, path));
        }

        let path = uri.to_file_path().ok()?;
        let text = std::fs::read_to_string(&path).ok()?;
        let version = None;
        self.documents.lock().unwrap().insert(
            uri.clone(),
            Document {
                text: text.clone(),
                version,
                path: path.clone(),
            },
        );
        Some((text, version, path))
    }

    async fn analysis(&self, uri: &Url) -> Option<Arc<Analysis>> {
        let (text, version, path) = self.document(uri).await?;
        let answers = self.answers_for(uri, content_hash(&text));
        if let Some(cached) = self.cache.lock().unwrap().get(uri) {
            if cached.version == version
                && cached.text == text
                && same_answers(&cached.answers, &answers)
            {
                return Some(cached.clone());
            }
        }

        let settings = self.settings.lock().unwrap().clone();
        let root = self.root.lock().unwrap().clone();
        let uri = uri.clone();
        let analysis = tokio::task::spawn_blocking(move || {
            Analysis::build(
                uri,
                path,
                text,
                version,
                &settings,
                root.as_deref(),
                answers,
            )
        })
        .await
        .ok()?;

        self.cache
            .lock()
            .unwrap()
            .insert(analysis.uri.clone(), analysis.clone());
        Some(analysis)
    }

    fn answers_for(&self, uri: &Url, hash: u64) -> Option<Arc<Answers>> {
        let probes = self.probes.lock().unwrap();
        let state = probes.get(uri)?;
        state
            .done
            .get(&hash)
            .cloned()
            .or_else(|| state.answers.clone())
    }

    fn probe_failure(&self, uri: &Url, analysis: &Analysis) -> Option<shyc::source::Diagnostic> {
        let probes = self.probes.lock().unwrap();
        let (hash, offset, message) = probes.get(uri)?.failure.as_ref()?;
        if *hash != content_hash(&analysis.text) {
            return None;
        }
        Some(shyc::source::Diagnostic::new(
            Span {
                file: analysis.root,
                start: *offset,
                end: *offset,
            },
            message.clone(),
        ))
    }

    async fn refresh_diagnostics(&self, uri: Url) {
        let enabled = self.settings.lock().unwrap().diagnostics;
        let Some(analysis) = self.analysis(&uri).await else {
            self.client.publish_diagnostics(uri, Vec::new(), None).await;
            return;
        };

        let failure = self.probe_failure(&uri, &analysis);
        let grouped = grouped_diagnostics(&analysis, &uri, enabled, failure.as_ref());
        publish_grouped(&self.client, &self.published, uri, grouped).await;
    }

    async fn refresh_open_documents(&self) {
        let uris: Vec<Url> = self.documents.lock().unwrap().keys().cloned().collect();
        for uri in uris {
            self.refresh_diagnostics(uri.clone()).await;
            self.schedule_probe(uri, Some(DEBOUNCE)).await;
        }
    }

    /// Run the probe rounds for this buffer after the editor pauses, or now
    /// when the file was just saved.
    async fn schedule_probe(&self, uri: Url, delay: Option<Duration>) {
        let Some((text, version, path)) = self.document(&uri).await else {
            return;
        };
        let hash = content_hash(&text);
        let generation = {
            let mut probes = self.probes.lock().unwrap();
            let state = probes.entry(uri.clone()).or_default();
            if state.running == Some(hash) {
                return;
            }
            // New text invalidates a probe for older text even when its own
            // answers are already cached.
            state.generation += 1;
            if state.done.contains_key(&hash) {
                return;
            }
            state.running = Some(hash);
            state.generation
        };

        let settings = self.settings.lock().unwrap().clone();
        let root = self.root.lock().unwrap().clone();
        let include_dirs = analysis::search_dirs(&path, root.as_deref(), &settings);
        let defines = settings.defines.clone();
        let program = locate_shycc(
            settings.shycc_path.as_deref(),
            &path,
            root.as_deref(),
            settings.libshy_path.as_deref(),
        );
        let enabled = settings.diagnostics;
        let client = self.client.clone();
        let cache = self.cache.clone();
        let published = self.published.clone();
        let probes = self.probes.clone();
        let documents = self.documents.clone();

        tokio::spawn(async move {
            if let Some(delay) = delay {
                tokio::time::sleep(delay).await;
            }
            let current = probes
                .lock()
                .unwrap()
                .get(&uri)
                .is_some_and(|state| state.generation == generation);
            if !current {
                release_probe(&probes, &uri, hash);
                return;
            }

            let probe_path = path.clone();
            let probe_text = text.clone();
            let outcome = tokio::task::spawn_blocking(move || {
                probe::run(&program, &probe_path, &probe_text, &include_dirs, &defines)
            })
            .await;
            let Ok(outcome) = outcome else {
                release_probe(&probes, &uri, hash);
                return;
            };

            let (current, answers) = {
                let mut probes = probes.lock().unwrap();
                let Some(state) = probes.get_mut(&uri) else {
                    return;
                };
                if state.running == Some(hash) {
                    state.running = None;
                }
                if outcome.success {
                    if let Some(answers) = &outcome.answers {
                        if state.done.len() >= 8 {
                            state.done.clear();
                        }
                        state.done.insert(hash, answers.clone());
                        if state.generation == generation {
                            state.answers = Some(answers.clone());
                            state.failure = None;
                        }
                    }
                }
                if state.generation == generation {
                    if let Some((offset, message)) = &outcome.failure {
                        state.failure = Some((hash, *offset, message.clone()));
                    }
                }
                (
                    state.generation == generation,
                    state
                        .done
                        .get(&hash)
                        .cloned()
                        .or_else(|| state.answers.clone()),
                )
            };
            if !current {
                return;
            }
            if outcome.answers.is_none() && outcome.failure.is_none() {
                return;
            }
            if !documents.lock().unwrap().contains_key(&uri) {
                return;
            }

            let failure = {
                let probes = probes.lock().unwrap();
                probes
                    .get(&uri)
                    .and_then(|state| state.failure.as_ref())
                    .map(|(_, offset, message)| (*offset, message.clone()))
            };
            let analysis_uri = uri.clone();
            let analysis = tokio::task::spawn_blocking(move || {
                Analysis::build(
                    analysis_uri,
                    path,
                    text,
                    version,
                    &settings,
                    root.as_deref(),
                    answers,
                )
            })
            .await;
            let Ok(analysis) = analysis else {
                return;
            };
            cache
                .lock()
                .unwrap()
                .insert(analysis.uri.clone(), analysis.clone());
            let failure = failure.map(|(offset, message)| {
                shyc::source::Diagnostic::new(
                    Span {
                        file: analysis.root,
                        start: offset,
                        end: offset,
                    },
                    message,
                )
            });
            let grouped = grouped_diagnostics(&analysis, &uri, enabled, failure.as_ref());
            publish_grouped(&client, &published, uri, grouped).await;
        });
    }

    fn clear_cache(&self, uri: &Url) {
        self.cache.lock().unwrap().remove(uri);
    }

    fn clear_probes(&self) {
        self.probes.lock().unwrap().clear();
    }
}

fn release_probe(probes: &Mutex<HashMap<Url, ProbeState>>, uri: &Url, hash: u64) {
    if let Some(state) = probes.lock().unwrap().get_mut(uri) {
        if state.running == Some(hash) {
            state.running = None;
        }
    }
}

/// The `shycc` driver to run probe rounds with.
fn locate_shycc(
    configured: Option<&Path>,
    document: &Path,
    root: Option<&Path>,
    libshy: Option<&Path>,
) -> PathBuf {
    if let Some(configured) = configured {
        return configured.to_path_buf();
    }
    if let Some(configured) = std::env::var_os("SHYCC") {
        return PathBuf::from(configured);
    }
    let name = format!("shycc{}", std::env::consts::EXE_SUFFIX);
    let mut candidates = Vec::new();
    if let Ok(current) = std::env::current_exe() {
        if let Some(directory) = current.parent() {
            candidates.push(directory.join(&name));
        }
    }
    if let Some(root) = root {
        candidates.push(root.join("target/debug").join(&name));
        candidates.push(root.join("target/release").join(&name));
    }
    // A configured libshy usually sits in the checkout that built the driver.
    if let Some(libshy) = libshy {
        let libshy = if libshy.join("libshy").is_dir() {
            libshy.join("libshy")
        } else {
            libshy.to_path_buf()
        };
        for ancestor in libshy.ancestors() {
            candidates.push(ancestor.join("target/debug").join(&name));
            candidates.push(ancestor.join("target/release").join(&name));
        }
    }
    for ancestor in document.ancestors().skip(1) {
        candidates.push(ancestor.join("target/debug").join(&name));
        candidates.push(ancestor.join("target/release").join(&name));
    }
    candidates
        .into_iter()
        .find(|candidate| candidate.is_file())
        .unwrap_or_else(|| PathBuf::from(name))
}

fn content_hash(text: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
}

fn same_answers(left: &Option<Arc<Answers>>, right: &Option<Arc<Answers>>) -> bool {
    match (left, right) {
        (None, None) => true,
        (Some(left), Some(right)) => Arc::ptr_eq(left, right),
        _ => false,
    }
}

fn grouped_diagnostics(
    analysis: &Analysis,
    uri: &Url,
    enabled: bool,
    failure: Option<&shyc::source::Diagnostic>,
) -> HashMap<Url, Vec<Diagnostic>> {
    let mut grouped: HashMap<Url, Vec<Diagnostic>> = HashMap::new();
    grouped.entry(uri.clone()).or_default();
    if enabled {
        let mut seen = HashSet::new();
        for diagnostic in &analysis.diagnostics {
            let target = analysis
                .uri_of(diagnostic.span.file)
                .unwrap_or_else(|| uri.clone());
            if !seen.insert((target.clone(), diagnostic.span, diagnostic.message.clone())) {
                continue;
            }
            grouped
                .entry(target)
                .or_default()
                .push(lsp_diagnostic(analysis, diagnostic));
        }
        // The probe runs the same parser over the same buffer, so its failure
        // is the analysis error again; report the construct once.
        if let Some(failure) = failure {
            let duplicate = analysis
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message == failure.message);
            let target = analysis
                .uri_of(failure.span.file)
                .unwrap_or_else(|| uri.clone());
            if !duplicate && seen.insert((target.clone(), failure.span, failure.message.clone())) {
                grouped
                    .entry(target)
                    .or_default()
                    .push(lsp_diagnostic(analysis, failure));
            }
        }
    }
    grouped
}

async fn publish_grouped(
    client: &Client,
    published: &Mutex<HashMap<Url, HashSet<Url>>>,
    uri: Url,
    grouped: HashMap<Url, Vec<Diagnostic>>,
) {
    let current: HashSet<Url> = grouped.keys().cloned().collect();
    let previous = published
        .lock()
        .unwrap()
        .insert(uri, current.clone())
        .unwrap_or_default();

    for (target, diagnostics) in grouped {
        client.publish_diagnostics(target, diagnostics, None).await;
    }
    for stale in previous.difference(&current) {
        client
            .publish_diagnostics(stale.clone(), Vec::new(), None)
            .await;
    }
}

#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    async fn initialize(&self, params: InitializeParams) -> Result<InitializeResult> {
        let root = params
            .root_uri
            .as_ref()
            .and_then(|uri| uri.to_file_path().ok())
            .or_else(|| {
                params
                    .workspace_folders
                    .as_ref()
                    .and_then(|folders| folders.first())
                    .and_then(|folder| folder.uri.to_file_path().ok())
            });
        *self.root.lock().unwrap() = root;

        let options: InitOptions = params
            .initialization_options
            .and_then(|value| serde_json::from_value(value).ok())
            .unwrap_or_default();
        *self.settings.lock().unwrap() = options.into_settings(file_settings());

        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Options(
                    TextDocumentSyncOptions {
                        open_close: Some(true),
                        change: Some(TextDocumentSyncKind::FULL),
                        will_save: None,
                        will_save_wait_until: None,
                        save: Some(TextDocumentSyncSaveOptions::Supported(true)),
                    },
                )),
                completion_provider: Some(CompletionOptions {
                    resolve_provider: Some(false),
                    trigger_characters: Some(vec![
                        ".".to_string(),
                        ":".to_string(),
                        ">".to_string(),
                    ]),
                    all_commit_characters: None,
                    work_done_progress_options: WorkDoneProgressOptions::default(),
                    completion_item: None,
                }),
                definition_provider: Some(OneOf::Left(true)),
                hover_provider: Some(HoverProviderCapability::Simple(true)),
                document_symbol_provider: Some(OneOf::Left(true)),
                semantic_tokens_provider: Some(
                    SemanticTokensServerCapabilities::SemanticTokensOptions(
                        SemanticTokensOptions {
                            work_done_progress_options: WorkDoneProgressOptions::default(),
                            legend: legend(),
                            range: Some(false),
                            full: Some(SemanticTokensFullOptions::Bool(true)),
                        },
                    ),
                ),
                ..ServerCapabilities::default()
            },
            server_info: Some(ServerInfo {
                name: "shyc-lsp".to_string(),
                version: Some(env!("CARGO_PKG_VERSION").to_string()),
            }),
        })
    }

    async fn initialized(&self, _: InitializedParams) {
        self.client
            .log_message(MessageType::INFO, "ShyC language server ready")
            .await;
        let libshy = self
            .settings
            .lock()
            .unwrap()
            .libshy_path
            .clone()
            .or_else(shyc::config::shylib);
        let message = match libshy {
            Some(path) => format!("libshy: {}", path.display()),
            None => "libshy: not configured (set shyc.libshyPath, ~/.shycserver/config.toml, or ~/.shycc.toml)"
                .to_string(),
        };
        self.client.log_message(MessageType::INFO, message).await;
    }

    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let document = params.text_document;
        let Some(path) = document.uri.to_file_path().ok() else {
            return;
        };
        self.documents.lock().unwrap().insert(
            document.uri.clone(),
            Document {
                text: document.text,
                version: Some(document.version),
                path,
            },
        );
        self.clear_cache(&document.uri);
        self.refresh_diagnostics(document.uri.clone()).await;
        self.schedule_probe(document.uri, None).await;
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let Some(change) = params.content_changes.into_iter().last() else {
            return;
        };
        let uri = params.text_document.uri;
        if let Some(document) = self.documents.lock().unwrap().get_mut(&uri) {
            document.text = change.text;
            document.version = Some(params.text_document.version);
        }
        self.clear_cache(&uri);
        self.refresh_diagnostics(uri.clone()).await;
        self.schedule_probe(uri, Some(DEBOUNCE)).await;
    }

    async fn did_save(&self, params: DidSaveTextDocumentParams) {
        self.clear_cache(&params.text_document.uri);
        self.refresh_diagnostics(params.text_document.uri.clone())
            .await;
        self.schedule_probe(params.text_document.uri, None).await;
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        let uri = params.text_document.uri;
        self.documents.lock().unwrap().remove(&uri);
        self.clear_cache(&uri);
        self.probes.lock().unwrap().remove(&uri);
        let previously = self.published.lock().unwrap().remove(&uri);
        if let Some(previously) = previously {
            for target in previously {
                self.client
                    .publish_diagnostics(target, Vec::new(), None)
                    .await;
            }
        }
    }

    async fn did_change_configuration(&self, params: DidChangeConfigurationParams) {
        if let Some(settings) = parse_settings(&params.settings) {
            *self.settings.lock().unwrap() = settings;
            self.cache.lock().unwrap().clear();
            self.clear_probes();
            self.refresh_open_documents().await;
        }
    }

    async fn did_change_watched_files(&self, params: DidChangeWatchedFilesParams) {
        if params.changes.iter().any(|change| {
            change.uri.path().ends_with(".shyh")
                || change.uri.path().ends_with(".h")
                || change.uri.path().ends_with(".shyc")
        }) {
            self.cache.lock().unwrap().clear();
            self.clear_probes();
            self.refresh_open_documents().await;
        }
    }

    async fn completion(&self, params: CompletionParams) -> Result<Option<CompletionResponse>> {
        let position = params.text_document_position.position;
        let uri = params.text_document_position.text_document.uri;
        let Some(analysis) = self.analysis(&uri).await else {
            return Ok(None);
        };
        let offset = analysis.lines.offset(&analysis.text, position);
        let items = completions(&analysis, offset);
        Ok(Some(CompletionResponse::Array(items)))
    }

    async fn hover(&self, params: HoverParams) -> Result<Option<Hover>> {
        let position = params.text_document_position_params.position;
        let uri = params.text_document_position_params.text_document.uri;
        let Some(analysis) = self.analysis(&uri).await else {
            return Ok(None);
        };
        let offset = analysis.lines.offset(&analysis.text, position);
        let Some(symbol) = symbol_at(&analysis, offset) else {
            return Ok(None);
        };
        let mut markdown = format!("```c\n{}\n```", symbol.detail);
        if symbol.kind == SymKind::Method {
            if let Some(container) = &symbol.container {
                markdown.push_str(&format!("\n\nMethod of `{container}`."));
            }
        }
        Ok(Some(Hover {
            contents: HoverContents::Markup(MarkupContent {
                kind: MarkupKind::Markdown,
                value: markdown,
            }),
            range: Some(analysis.range(symbol.span)),
        }))
    }

    async fn goto_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        let position = params.text_document_position_params.position;
        let uri = params.text_document_position_params.text_document.uri;
        let Some(analysis) = self.analysis(&uri).await else {
            return Ok(None);
        };
        let offset = analysis.lines.offset(&analysis.text, position);

        if let Some(target) = analysis.target_at(offset) {
            if let Some(location) = symbol_location(&analysis, target) {
                return Ok(Some(GotoDefinitionResponse::Scalar(location)));
            }
        }

        if let Some((start, end)) = text::word_span(&analysis.text, offset) {
            let name = &analysis.text[start..end];
            let locations: Vec<Location> = analysis
                .index
                .lookup(name)
                .iter()
                .filter_map(|&id| symbol_location(&analysis, id))
                .collect();
            match locations.len() {
                1 => return Ok(Some(GotoDefinitionResponse::Scalar(locations[0].clone()))),
                count if count > 1 => return Ok(Some(GotoDefinitionResponse::Array(locations))),
                _ => {}
            }
        }

        // A `use` or `#include` line names a file, not a symbol: open the
        // module header when no declaration in the line matched.
        match include_location(&analysis, offset) {
            Some(location) => Ok(Some(GotoDefinitionResponse::Scalar(location))),
            None => Ok(None),
        }
    }

    async fn document_symbol(
        &self,
        params: DocumentSymbolParams,
    ) -> Result<Option<DocumentSymbolResponse>> {
        let uri = params.text_document.uri;
        let Some(analysis) = self.analysis(&uri).await else {
            return Ok(None);
        };
        Ok(Some(DocumentSymbolResponse::Nested(document_symbols(
            &analysis,
        ))))
    }

    async fn semantic_tokens_full(
        &self,
        params: SemanticTokensParams,
    ) -> Result<Option<SemanticTokensResult>> {
        let uri = params.text_document.uri;
        let Some(analysis) = self.analysis(&uri).await else {
            return Ok(None);
        };
        Ok(Some(SemanticTokensResult::Tokens(SemanticTokens {
            result_id: None,
            data: semantic_tokens(&analysis),
        })))
    }
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct InitOptions {
    libshy_path: Option<String>,
    shycc_path: Option<String>,
    include_dirs: Option<Vec<String>>,
    defines: Option<Vec<String>>,
    diagnostics_enabled: Option<bool>,
}

impl InitOptions {
    /// The workspace options win, but an empty value falls back to the user
    /// configuration file so the editor only has to configure a workspace once.
    fn into_settings(self, base: Settings) -> Settings {
        Settings {
            libshy_path: self
                .libshy_path
                .map(PathBuf::from)
                .filter(|path| !path.as_os_str().is_empty())
                .or(base.libshy_path),
            shycc_path: self
                .shycc_path
                .map(PathBuf::from)
                .filter(|path| !path.as_os_str().is_empty())
                .or(base.shycc_path),
            include_dirs: non_empty_paths(self.include_dirs).unwrap_or(base.include_dirs),
            defines: non_empty_defines(self.defines).unwrap_or(base.defines),
            diagnostics: self.diagnostics_enabled.unwrap_or(base.diagnostics),
        }
    }
}

fn non_empty_paths(values: Option<Vec<String>>) -> Option<Vec<PathBuf>> {
    let values: Vec<PathBuf> = values
        .unwrap_or_default()
        .into_iter()
        .map(|value| PathBuf::from(value.trim()))
        .filter(|value| !value.as_os_str().is_empty())
        .collect();
    (!values.is_empty()).then_some(values)
}

fn non_empty_defines(values: Option<Vec<String>>) -> Option<Vec<(String, String)>> {
    let values = parse_defines(values.unwrap_or_default());
    (!values.is_empty()).then_some(values)
}

/// `~/.shycserver/config.toml`, shared by every workspace this editor opens.
fn config_file() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join(".shycserver").join("config.toml"))
}

/// Reads the user configuration; a missing or unreadable file means defaults.
///
/// The file is a flat subset of TOML: `libshy_path = "/path"`, and
/// `include_dirs`/`defines` arrays. `#` starts a comment outside a string.
fn file_settings() -> Settings {
    let Some(path) = config_file() else {
        return Settings::default();
    };
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Settings::default();
    };
    let values = parse_config(&text);
    Settings {
        libshy_path: values
            .get("libshy_path")
            .and_then(|values| values.first())
            .filter(|value| !value.is_empty())
            .map(PathBuf::from),
        shycc_path: values
            .get("shycc_path")
            .and_then(|values| values.first())
            .filter(|value| !value.is_empty())
            .map(PathBuf::from),
        include_dirs: values
            .get("include_dirs")
            .map(|values| values.iter().map(PathBuf::from).collect())
            .unwrap_or_default(),
        defines: values
            .get("defines")
            .map(|values| parse_defines(values.clone()))
            .unwrap_or_default(),
        diagnostics: values
            .get("diagnostics")
            .and_then(|values| values.first())
            .map(|value| value != "false")
            .unwrap_or(true),
    }
}

/// Parses the flat `key = value` / `key = ["a", "b"]` form used above.
fn parse_config(text: &str) -> std::collections::HashMap<String, Vec<String>> {
    let mut values = std::collections::HashMap::new();
    for line in text.lines() {
        let line = strip_comment(line).trim();
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim();
        let entries = if value.starts_with('[') {
            value
                .trim_start_matches('[')
                .trim_end_matches(']')
                .split(',')
                .map(unquote)
                .filter(|entry| !entry.is_empty())
                .collect()
        } else {
            let entry = unquote(value);
            if entry.is_empty() {
                Vec::new()
            } else {
                vec![entry]
            }
        };
        values.insert(key.trim().to_string(), entries);
    }
    values
}

/// Cuts a `#` comment, ignoring `#` characters inside a quoted string.
fn strip_comment(line: &str) -> &str {
    let mut quoted = false;
    for (index, character) in line.char_indices() {
        match character {
            '"' => quoted = !quoted,
            '#' if !quoted => return &line[..index],
            _ => {}
        }
    }
    line
}

fn unquote(value: &str) -> String {
    value
        .trim()
        .trim_matches(|character| character == '"' || character == '\'')
        .to_string()
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SettingsPayload {
    libshy_path: Option<String>,
    shycc_path: Option<String>,
    include_dirs: Option<Vec<String>>,
    defines: Option<Vec<String>>,
    diagnostics_enabled: Option<bool>,
}

#[derive(Default, Deserialize)]
struct WrappedSettings {
    shyc: Option<SettingsPayload>,
}

fn parse_settings(value: &serde_json::Value) -> Option<Settings> {
    let payload = if value.get("shyc").is_some() {
        serde_json::from_value::<WrappedSettings>(value.clone())
            .ok()?
            .shyc?
    } else {
        serde_json::from_value::<SettingsPayload>(value.clone()).ok()?
    };
    let base = file_settings();
    Some(Settings {
        libshy_path: payload
            .libshy_path
            .map(PathBuf::from)
            .filter(|path| !path.as_os_str().is_empty())
            .or(base.libshy_path),
        shycc_path: payload
            .shycc_path
            .map(PathBuf::from)
            .filter(|path| !path.as_os_str().is_empty())
            .or(base.shycc_path),
        include_dirs: non_empty_paths(payload.include_dirs).unwrap_or(base.include_dirs),
        defines: non_empty_defines(payload.defines).unwrap_or(base.defines),
        diagnostics: payload.diagnostics_enabled.unwrap_or(base.diagnostics),
    })
}

fn parse_defines(values: Vec<String>) -> Vec<(String, String)> {
    values
        .into_iter()
        .filter_map(|value| {
            let (name, replacement) = value.split_once('=').unwrap_or((&value, "1"));
            let name = name.trim();
            if name.is_empty() {
                None
            } else {
                Some((name.to_string(), replacement.to_string()))
            }
        })
        .collect()
}

fn legend() -> SemanticTokensLegend {
    SemanticTokensLegend {
        token_types: vec![
            SemanticTokenType::NAMESPACE,
            SemanticTokenType::TYPE,
            SemanticTokenType::CLASS,
            SemanticTokenType::ENUM,
            SemanticTokenType::INTERFACE,
            SemanticTokenType::STRUCT,
            SemanticTokenType::TYPE_PARAMETER,
            SemanticTokenType::PARAMETER,
            SemanticTokenType::VARIABLE,
            SemanticTokenType::PROPERTY,
            SemanticTokenType::ENUM_MEMBER,
            SemanticTokenType::FUNCTION,
            SemanticTokenType::METHOD,
            SemanticTokenType::MACRO,
            SemanticTokenType::KEYWORD,
            SemanticTokenType::MODIFIER,
            SemanticTokenType::COMMENT,
            SemanticTokenType::STRING,
            SemanticTokenType::NUMBER,
            SemanticTokenType::OPERATOR,
        ],
        token_modifiers: vec![
            SemanticTokenModifier::DECLARATION,
            SemanticTokenModifier::READONLY,
        ],
    }
}

fn lsp_diagnostic(analysis: &Analysis, diagnostic: &shyc::source::Diagnostic) -> Diagnostic {
    let mut range = analysis.range(diagnostic.span);
    if range.start == range.end {
        range.end.character += 1;
    }
    Diagnostic {
        range,
        severity: Some(DiagnosticSeverity::ERROR),
        code: None,
        code_description: None,
        source: Some("shyc".to_string()),
        message: diagnostic.message.clone(),
        related_information: None,
        tags: None,
        data: None,
    }
}

fn include_location(analysis: &Analysis, offset: usize) -> Option<Location> {
    let line_start = analysis.text[..offset]
        .rfind('\n')
        .map_or(0, |index| index + 1);
    let line_end = analysis.text[offset..]
        .find('\n')
        .map_or(analysis.text.len(), |index| offset + index);
    let line = analysis.text.get(line_start..line_end)?.trim();
    let source_path = &analysis.sources.file(analysis.root).path;
    let (include_name, relative) = if let Some(rest) = line.strip_prefix("#include") {
        let rest = rest.trim();
        let name = rest.trim_matches(|ch| ch == '"' || ch == '<' || ch == '>');
        (name.to_string(), rest.starts_with('"'))
    } else if let Some(rest) = line.strip_prefix("use ") {
        let path = rest.trim_end_matches(';').trim();
        let segments: Vec<&str> = path.split("::").collect();
        let module = if segments.last() == Some(&"*") {
            &segments[..segments.len() - 1]
        } else {
            &segments[..segments.len().saturating_sub(1)]
        };
        if module.is_empty() {
            return None;
        }
        let module = module.strip_prefix(&["crate"]).unwrap_or(module);
        let module = module.strip_prefix(&["self"]).unwrap_or(module);
        if module.is_empty() {
            return None;
        }
        let base = module.join("/");
        let suffix = if module.first() == Some(&"c") {
            ".h"
        } else {
            ".shyh"
        };
        (format!("{base}{suffix}"), module.first() == Some(&"self"))
    } else {
        return None;
    };
    let mut dirs = Vec::new();
    if relative {
        dirs.push(source_path.parent()?.to_path_buf());
    }
    dirs.extend(analysis.include_dirs.iter().cloned());
    let path = dirs
        .into_iter()
        .map(|dir| dir.join(&include_name))
        .find(|path| path.is_file())?;
    Some(Location {
        uri: text::uri_for_path(&path)?,
        range: Range::default(),
    })
}

fn symbol_location(analysis: &Analysis, id: usize) -> Option<Location> {
    let symbol = analysis.index.symbol(id);
    let file = analysis.sources.file(symbol.span.file);
    Some(Location {
        uri: text::uri_for_path(&file.path)?,
        range: LineIndex::new(&file.text).range(&file.text, symbol.span.start, symbol.span.end),
    })
}

fn symbol_at(analysis: &Analysis, offset: usize) -> Option<&index::Sym> {
    if let Some(target) = analysis.target_at(offset) {
        return Some(analysis.index.symbol(target));
    }
    let (start, end) = text::word_span(&analysis.text, offset)?;
    let name = &analysis.text[start..end];
    if let Some(id) = analysis
        .index
        .visible_symbols(analysis.root, offset)
        .into_iter()
        .rev()
        .find(|&id| analysis.index.symbol(id).name == name)
    {
        return Some(analysis.index.symbol(id));
    }
    analysis
        .index
        .lookup(name)
        .first()
        .map(|&id| analysis.index.symbol(id))
}

fn token_type(kind: SymKind) -> u32 {
    match kind {
        SymKind::Struct | SymKind::Union => TOK_STRUCT,
        SymKind::Enum => TOK_ENUM,
        SymKind::Trait => TOK_INTERFACE,
        SymKind::TypeAlias => TOK_TYPE,
        SymKind::Function => TOK_FUNCTION,
        SymKind::Method => TOK_METHOD,
        SymKind::Field => TOK_PROPERTY,
        SymKind::Global => TOK_VARIABLE,
        SymKind::Variant => TOK_ENUM_MEMBER,
        SymKind::Local => TOK_VARIABLE,
        SymKind::Param => TOK_PARAMETER,
        SymKind::TypeParam | SymKind::ConstParam => TOK_TYPE_PARAMETER,
        SymKind::Label => TOK_VARIABLE,
        SymKind::Comptime => TOK_MACRO,
    }
}

/// Best-effort type of an identifier the symbol index does not know.
///
/// A buffer is highlighted while it is being typed, so a parse failure is
/// normal, not exceptional. Without this the file would lose every
/// identifier color until the next successful parse, because the index is
/// empty then. The token stream still separates `.field`, `call(` and a
/// plain name, which is enough to keep the file readable.
fn guess_token_type(
    word: &str,
    previous: Option<&shyc::lexer::Token>,
    next: Option<&shyc::lexer::Token>,
) -> u32 {
    fn punct(token: Option<&shyc::lexer::Token>) -> Option<&str> {
        match token.map(|token| &token.kind) {
            Some(shyc::lexer::TokenKind::Punct(punct)) => Some(punct),
            _ => None,
        }
    }

    let called = punct(next) == Some("(");
    match punct(previous) {
        Some("." | "->") if called => TOK_METHOD,
        Some("." | "->") => TOK_PROPERTY,
        _ if called => TOK_FUNCTION,
        _ if word.chars().next().is_some_and(char::is_uppercase) => TOK_TYPE,
        _ => TOK_VARIABLE,
    }
}

fn keyword_token(word: &str) -> Option<u32> {
    if shyc::parser::builtin_type(word).is_some() {
        return Some(TOK_TYPE);
    }
    if word == "self" {
        return Some(TOK_TYPE);
    }
    if matches!(
        word,
        "const"
            | "volatile"
            | "restrict"
            | "static"
            | "extern"
            | "inline"
            | "auto"
            | "register"
            | "typedef"
            | "_Atomic"
            | "_Noreturn"
    ) {
        return Some(TOK_MODIFIER);
    }
    if matches!(
        word,
        "fn" | "impl" | "trait" | "let" | "mut" | "use" | "asm"
    ) {
        return Some(TOK_KEYWORD);
    }
    if shyc::parser::is_keyword(word) {
        Some(TOK_KEYWORD)
    } else {
        None
    }
}

fn semantic_tokens(analysis: &Analysis) -> Vec<SemanticToken> {
    let root = analysis.root;
    let mut classes: HashMap<(usize, usize), (u32, u32)> = HashMap::new();

    for symbol in &analysis.index.symbols {
        if symbol.span.file == root && symbol.span.start < symbol.span.end {
            classes.insert(
                (symbol.span.start, symbol.span.end),
                (token_type(symbol.kind), MOD_DECLARATION),
            );
        }
    }
    for reference in &analysis.index.refs {
        if reference.span.file == root && reference.span.start < reference.span.end {
            let kind = analysis.index.symbol(reference.target).kind;
            classes
                .entry((reference.span.start, reference.span.end))
                .or_insert((token_type(kind), 0));
        }
    }

    let text = &analysis.text;
    let line_starts: Vec<usize> = std::iter::once(0)
        .chain(text.match_indices('\n').map(|(offset, _)| offset + 1))
        .collect();
    let is_directive: Vec<bool> = line_starts
        .iter()
        .map(|&start| {
            let end = text[start..]
                .find('\n')
                .map(|offset| start + offset)
                .unwrap_or(text.len());
            text[start..end].trim_start().starts_with('#')
        })
        .collect();

    // The fallback classifier below reads the neighbouring token, so the
    // loop walks the significant ones in order instead of the raw stream.
    let significant: Vec<usize> = analysis
        .raw_tokens
        .iter()
        .enumerate()
        .filter(|(_, token)| {
            !matches!(
                &token.kind,
                shyc::lexer::TokenKind::Newline | shyc::lexer::TokenKind::Eof
            )
        })
        .map(|(index, _)| index)
        .collect();

    let mut tokens: Vec<(u32, u32, u32, u32, u32)> = Vec::new();
    for (position, &index) in significant.iter().enumerate() {
        let token = &analysis.raw_tokens[index];
        let previous = position
            .checked_sub(1)
            .map(|earlier| &analysis.raw_tokens[significant[earlier]]);
        let next = significant
            .get(position + 1)
            .map(|&later| &analysis.raw_tokens[later]);
        let span = token.span;
        if span.file != root || span.start >= span.end {
            continue;
        }
        let line = line_starts.partition_point(|&start| start <= span.start) - 1;
        if is_directive.get(line).copied().unwrap_or(false) {
            continue;
        }

        let mut emit = |start: usize, end: usize, token_type: u32, modifiers: u32| {
            let start = analysis.lines.position(text, start);
            let end = analysis.lines.position(text, end);
            if end.line == start.line {
                if end.character > start.character {
                    tokens.push((
                        start.line,
                        start.character,
                        end.character - start.character,
                        token_type,
                        modifiers,
                    ));
                }
                return;
            }
            for line in start.line..=end.line {
                let line_start = line_starts[line as usize];
                let line_end = line_starts
                    .get(line as usize + 1)
                    .map(|next| next.saturating_sub(1))
                    .unwrap_or(text.len());
                let from = if line == start.line {
                    span.start
                } else {
                    line_start
                };
                let to = if line == end.line { span.end } else { line_end };
                let from = analysis.lines.position(text, from);
                let to = analysis.lines.position(text, to);
                if to.character > from.character {
                    tokens.push((
                        from.line,
                        from.character,
                        to.character - from.character,
                        token_type,
                        modifiers,
                    ));
                }
            }
        };

        match &token.kind {
            shyc::lexer::TokenKind::Ident(word) => {
                let (token_type, modifiers) =
                    if let Some(&class) = classes.get(&(span.start, span.end)) {
                        class
                    } else if let Some(token_type) = keyword_token(word) {
                        (token_type, 0)
                    } else if let Some(id) = analysis.index.lookup(word).first() {
                        (token_type(analysis.index.symbol(*id).kind), 0)
                    } else {
                        (guess_token_type(word, previous, next), 0)
                    };
                emit(span.start, span.end, token_type, modifiers);
            }
            shyc::lexer::TokenKind::Punct(punct) => {
                let token_type = if punct == "asm!" {
                    TOK_MACRO
                } else if matches!(
                    punct.as_str(),
                    "(" | ")" | "{" | "}" | "[" | "]" | ";" | ","
                ) {
                    continue;
                } else {
                    TOK_OPERATOR
                };
                emit(span.start, span.end, token_type, 0);
            }
            shyc::lexer::TokenKind::Number(_) => emit(span.start, span.end, TOK_NUMBER, 0),
            shyc::lexer::TokenKind::StringLiteral { .. }
            | shyc::lexer::TokenKind::CharLiteral { .. } => {
                emit(span.start, span.end, TOK_STRING, 0)
            }
            shyc::lexer::TokenKind::Newline | shyc::lexer::TokenKind::Eof => {}
        }
    }

    tokens.sort_by_key(|token| (token.0, token.1));
    tokens.dedup_by_key(|token| (token.0, token.1));

    let mut data = Vec::with_capacity(tokens.len());
    let mut previous_line = 0;
    let mut previous_start = 0;
    for (line, start, length, token_type, modifiers) in tokens {
        let delta_line = line - previous_line;
        let delta_start = if delta_line == 0 {
            start - previous_start
        } else {
            start
        };
        data.push(SemanticToken {
            delta_line,
            delta_start,
            length,
            token_type,
            token_modifiers_bitset: modifiers,
        });
        previous_line = line;
        previous_start = start;
    }
    data
}

#[derive(Default)]
struct CompletionContext {
    qualifier: Option<String>,
    receiver: Option<String>,
    prefix: String,
}

fn completion_context(text: &str, offset: usize) -> CompletionContext {
    let before = &text[..offset];
    let word_start = before
        .rfind(|character: char| !(character.is_alphanumeric() || character == '_'))
        .map(|index| index + 1)
        .unwrap_or(0);
    let prefix = before[word_start..].to_string();
    let head = before[..word_start].trim_end();

    if let Some(qualifier) = head.strip_suffix("::") {
        let qualifier = strip_generic_arguments(qualifier.trim_end());
        return CompletionContext {
            qualifier: Some(qualifier),
            receiver: None,
            prefix,
        };
    }
    if let Some(receiver) = head.strip_suffix("->") {
        return CompletionContext {
            qualifier: None,
            receiver: trailing_identifier(receiver),
            prefix,
        };
    }
    if let Some(receiver) = head.strip_suffix('.') {
        return CompletionContext {
            qualifier: None,
            receiver: trailing_identifier(receiver),
            prefix,
        };
    }
    CompletionContext {
        qualifier: None,
        receiver: None,
        prefix,
    }
}

fn strip_generic_arguments(text: &str) -> String {
    let mut depth = 0;
    let mut cut = text.len();
    for (index, character) in text.char_indices().rev() {
        match character {
            '>' => depth += 1,
            '<' if depth > 0 => {
                depth -= 1;
                if depth == 0 {
                    cut = index;
                }
            }
            _ => {}
        }
    }
    trailing_identifier(&text[..cut]).unwrap_or_else(|| text.to_string())
}

fn trailing_identifier(text: &str) -> Option<String> {
    let text = text.trim_end();
    let start = text
        .rfind(|character: char| !(character.is_alphanumeric() || character == '_'))
        .map(|index| index + 1)
        .unwrap_or(0);
    let name = &text[start..];
    if name.is_empty() {
        None
    } else {
        Some(name.to_string())
    }
}

fn completions(analysis: &Analysis, offset: usize) -> Vec<CompletionItem> {
    let context = completion_context(&analysis.text, offset);
    let prefix = context.prefix.as_str();
    let mut items = Vec::new();

    if let Some(qualifier) = &context.qualifier {
        for id in analysis.index.in_container(qualifier, prefix) {
            let symbol = analysis.index.symbol(id);
            if matches!(symbol.kind, SymKind::Method | SymKind::Variant) {
                items.push(completion_item(symbol, false));
            }
        }
        return dedupe(items);
    }

    if let Some(receiver) = &context.receiver {
        // `self` is not a symbol in the index; the method around the cursor
        // names the record whose members are wanted.
        let container = if receiver == "self" {
            enclosing_container(analysis, offset)
        } else {
            receiver_type(analysis, receiver, offset).map(|text| index::base_type_name(&text))
        };

        let candidates: Vec<usize> = if let Some(container) = &container {
            analysis.index.in_container(container, prefix)
        } else {
            analysis.index.lookup(prefix).to_vec()
        };
        for id in candidates {
            let symbol = analysis.index.symbol(id);
            let member = match symbol.kind {
                SymKind::Field => true,
                SymKind::Method => method_takes_receiver(symbol),
                _ => false,
            };
            if member {
                items.push(completion_item(symbol, symbol.kind == SymKind::Method));
            }
        }
        return dedupe(items);
    }

    for id in analysis.index.visible_symbols(analysis.root, offset) {
        let symbol = analysis.index.symbol(id);
        if symbol.name.starts_with(prefix) {
            items.push(completion_item(symbol, false));
        }
    }

    for symbol in &analysis.index.symbols {
        if !symbol.name.starts_with(prefix) {
            continue;
        }
        if !matches!(
            symbol.kind,
            SymKind::Function
                | SymKind::Struct
                | SymKind::Union
                | SymKind::Enum
                | SymKind::Trait
                | SymKind::TypeAlias
                | SymKind::Global
                | SymKind::Variant
        ) {
            continue;
        }
        items.push(completion_item(symbol, false));
    }

    for word in KEYWORDS {
        if word.starts_with(prefix) {
            let mut item = CompletionItem::new_simple((*word).to_string(), "keyword".to_string());
            item.kind = Some(CompletionItemKind::KEYWORD);
            items.push(item);
        }
    }

    dedupe(items)
}

fn receiver_type(analysis: &Analysis, name: &str, offset: usize) -> Option<String> {
    for id in analysis
        .index
        .visible_symbols(analysis.root, offset)
        .into_iter()
        .rev()
    {
        let symbol = analysis.index.symbol(id);
        if symbol.name == name {
            return symbol.type_text.clone();
        }
    }
    analysis
        .index
        .lookup(name)
        .iter()
        .find_map(|&id| analysis.index.symbol(id).type_text.clone())
}

fn enclosing_container(analysis: &Analysis, offset: usize) -> Option<String> {
    analysis
        .index
        .symbols
        .iter()
        .filter(|symbol| {
            symbol.kind == SymKind::Method
                && symbol.scope.is_some_and(|scope| {
                    scope.file == analysis.root && scope.start <= offset && offset <= scope.end
                })
        })
        .min_by_key(|symbol| {
            symbol
                .scope
                .map(|scope| scope.end.saturating_sub(scope.start))
                .unwrap_or(usize::MAX)
        })
        .and_then(|symbol| symbol.container.clone())
}

fn completion_item(symbol: &index::Sym, skip_self: bool) -> CompletionItem {
    let mut item = CompletionItem::new_simple(symbol.name.clone(), symbol.detail.clone());
    item.kind = Some(match symbol.kind {
        SymKind::Struct | SymKind::Union | SymKind::TypeAlias => CompletionItemKind::STRUCT,
        SymKind::Enum => CompletionItemKind::ENUM,
        SymKind::Trait => CompletionItemKind::INTERFACE,
        SymKind::Function => CompletionItemKind::FUNCTION,
        SymKind::Method => CompletionItemKind::METHOD,
        SymKind::Field => CompletionItemKind::FIELD,
        SymKind::Variant => CompletionItemKind::ENUM_MEMBER,
        SymKind::Global | SymKind::Local | SymKind::Param => CompletionItemKind::VARIABLE,
        SymKind::TypeParam | SymKind::ConstParam => CompletionItemKind::TYPE_PARAMETER,
        SymKind::Label => CompletionItemKind::REFERENCE,
        SymKind::Comptime => CompletionItemKind::FUNCTION,
    });
    if matches!(
        symbol.kind,
        SymKind::Function | SymKind::Method | SymKind::Variant
    ) {
        let snippet = call_snippet(symbol, skip_self);
        if snippet != symbol.name {
            item.insert_text = Some(snippet);
            item.insert_text_format = Some(InsertTextFormat::SNIPPET);
        }
    }
    item
}

fn method_takes_receiver(symbol: &index::Sym) -> bool {
    let Some(open) = symbol.detail.find('(') else {
        return false;
    };
    let Some(close) = symbol.detail.rfind(')') else {
        return false;
    };
    symbol.detail[open + 1..close]
        .split(',')
        .next()
        .is_some_and(|param| {
            param.split_whitespace().any(|word| {
                word.trim_matches(|character| character == '*' || character == '&') == "self"
            })
        })
}

fn call_snippet(symbol: &index::Sym, skip_self: bool) -> String {
    if symbol.kind == SymKind::Variant {
        return symbol.name.clone();
    }
    let Some(open) = symbol.detail.find('(') else {
        return symbol.name.clone();
    };
    let Some(close) = symbol.detail.rfind(')') else {
        return symbol.name.clone();
    };
    let params = &symbol.detail[open + 1..close];
    if params.trim().is_empty() || params.trim() == "void" {
        return format!("{}()", symbol.name);
    }
    let mut params: Vec<&str> = params.split(',').collect();
    if skip_self && !params.is_empty() {
        params.remove(0);
    }
    if params.is_empty() {
        return format!("{}()", symbol.name);
    }
    let args: Vec<String> = params
        .iter()
        .enumerate()
        .map(|(index, param)| {
            let name = trailing_identifier(param).unwrap_or_else(|| format!("arg{}", index + 1));
            format!("${{{}:{}}}", index + 1, name)
        })
        .collect();
    format!("{}({})", symbol.name, args.join(", "))
}

fn dedupe(items: Vec<CompletionItem>) -> Vec<CompletionItem> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for item in items {
        let key = format!("{:?}:{}", item.kind, item.label);
        if seen.insert(key) {
            out.push(item);
        }
    }
    out
}

#[allow(deprecated)]
fn document_symbols(analysis: &Analysis) -> Vec<DocumentSymbol> {
    let Some(unit) = &analysis.unit else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for item in &unit.items {
        match item {
            shyc::ast::Item::Decl(declaration) => {
                document_symbols_for_declaration(analysis, declaration, &mut out);
            }
            shyc::ast::Item::Function(function) => {
                if function.span.file != analysis.root {
                    continue;
                }
                out.push(document_symbol_for_function(analysis, function));
            }
            shyc::ast::Item::Impl(block) => {
                if block.span.file != analysis.root {
                    continue;
                }
                let mut children = Vec::new();
                for method in &block.methods {
                    children.push(document_symbol_for_function(analysis, method));
                }
                for prototype in &block.prototypes {
                    if let Some(name) = &prototype.declarator.name {
                        children.push(DocumentSymbol {
                            name: name.text.clone(),
                            detail: Some(analysis.sources.text(prototype.specs.span).to_string()),
                            kind: SymbolKind::METHOD,
                            tags: None,
                            deprecated: None,
                            range: analysis.range(prototype.span),
                            selection_range: analysis.range(name.span),
                            children: None,
                        });
                    }
                }
                let header = analysis.sources.text(block.self_type.span);
                out.push(DocumentSymbol {
                    name: format!("impl {}", header.trim()),
                    detail: None,
                    kind: SymbolKind::NAMESPACE,
                    tags: None,
                    deprecated: None,
                    range: analysis.range(block.span),
                    selection_range: analysis.range(block.self_type.span),
                    children: Some(children),
                });
            }
            shyc::ast::Item::Trait(declaration) => {
                if declaration.span.file != analysis.root {
                    continue;
                }
                let mut children = Vec::new();
                for method in &declaration.methods {
                    if let Some(name) = &method.declarator.name {
                        children.push(DocumentSymbol {
                            name: name.text.clone(),
                            detail: None,
                            kind: SymbolKind::METHOD,
                            tags: None,
                            deprecated: None,
                            range: analysis.range(method.span),
                            selection_range: analysis.range(name.span),
                            children: None,
                        });
                    }
                }
                out.push(DocumentSymbol {
                    name: declaration.name.text.clone(),
                    detail: Some("trait".to_string()),
                    kind: SymbolKind::INTERFACE,
                    tags: None,
                    deprecated: None,
                    range: analysis.range(declaration.span),
                    selection_range: analysis.range(declaration.name.span),
                    children: Some(children),
                });
            }
            shyc::ast::Item::Use(_) => {}
        }
    }
    out
}

#[allow(deprecated)]
fn document_symbols_for_declaration(
    analysis: &Analysis,
    declaration: &shyc::ast::Declaration,
    out: &mut Vec<DocumentSymbol>,
) {
    if declaration.span.file != analysis.root {
        return;
    }
    for specifier in &declaration.specs.type_specs {
        match specifier {
            shyc::ast::TypeSpecifier::Record(record) if record.members.is_some() => {
                let Some(tag) = &record.tag else {
                    continue;
                };
                let children = record
                    .members
                    .iter()
                    .flatten()
                    .flat_map(|member| member.declarators.iter())
                    .filter_map(|declarator| {
                        let name = declarator.declarator.name.as_ref()?;
                        Some(DocumentSymbol {
                            name: name.text.clone(),
                            detail: analysis
                                .index
                                .symbols
                                .iter()
                                .find(|symbol| symbol.span == name.span)
                                .map(|symbol| symbol.detail.clone()),
                            kind: SymbolKind::FIELD,
                            tags: None,
                            deprecated: None,
                            range: analysis.range(declarator.span),
                            selection_range: analysis.range(name.span),
                            children: None,
                        })
                    })
                    .collect();
                out.push(DocumentSymbol {
                    name: tag.text.clone(),
                    detail: Some(match record.kind {
                        shyc::ast::RecordKind::Struct => "struct".to_string(),
                        shyc::ast::RecordKind::Union => "union".to_string(),
                    }),
                    kind: SymbolKind::STRUCT,
                    tags: None,
                    deprecated: None,
                    range: analysis.range(record.span),
                    selection_range: analysis.range(tag.span),
                    children: Some(children),
                });
            }
            shyc::ast::TypeSpecifier::Enum(enumeration) if enumeration.enumerators.is_some() => {
                let Some(tag) = &enumeration.tag else {
                    continue;
                };
                let children = enumeration
                    .enumerators
                    .iter()
                    .flatten()
                    .map(|enumerator| DocumentSymbol {
                        name: enumerator.name.text.clone(),
                        detail: enumerator
                            .payload
                            .as_ref()
                            .map(|payload| analysis.sources.text(payload.span).to_string()),
                        kind: SymbolKind::ENUM_MEMBER,
                        tags: None,
                        deprecated: None,
                        range: analysis.range(enumerator.span),
                        selection_range: analysis.range(enumerator.name.span),
                        children: None,
                    })
                    .collect();
                out.push(DocumentSymbol {
                    name: tag.text.clone(),
                    detail: Some("enum".to_string()),
                    kind: SymbolKind::ENUM,
                    tags: None,
                    deprecated: None,
                    range: analysis.range(enumeration.span),
                    selection_range: analysis.range(tag.span),
                    children: Some(children),
                });
            }
            _ => {}
        }
    }

    for declarator in &declaration.declarators {
        let Some(name) = &declarator.declarator.name else {
            continue;
        };
        let detail = analysis
            .index
            .symbols
            .iter()
            .find(|symbol| symbol.span == name.span)
            .map(|symbol| symbol.detail.clone());
        let kind = if declaration.specs.storage == Some(shyc::ast::StorageClass::Typedef) {
            SymbolKind::CLASS
        } else if matches!(
            declarator.declarator.derived.first(),
            Some(shyc::ast::DerivedDeclarator::Function(_))
        ) {
            SymbolKind::FUNCTION
        } else {
            SymbolKind::VARIABLE
        };
        out.push(DocumentSymbol {
            name: name.text.clone(),
            detail,
            kind,
            tags: None,
            deprecated: None,
            range: analysis.range(declarator.span),
            selection_range: analysis.range(name.span),
            children: None,
        });
    }
}

#[allow(deprecated)]
fn document_symbol_for_function(
    analysis: &Analysis,
    function: &shyc::ast::FunctionDecl,
) -> DocumentSymbol {
    let name = function
        .declarator
        .name
        .as_ref()
        .map(|name| name.text.clone())
        .unwrap_or_else(|| "function".to_string());
    let detail = analysis
        .index
        .symbols
        .iter()
        .find(|symbol| {
            function
                .declarator
                .name
                .as_ref()
                .is_some_and(|declaration| symbol.span == declaration.span)
        })
        .map(|symbol| symbol.detail.clone());
    DocumentSymbol {
        name,
        detail,
        kind: SymbolKind::FUNCTION,
        tags: None,
        deprecated: None,
        range: analysis.range(function.span),
        selection_range: analysis.range(
            function
                .declarator
                .name
                .as_ref()
                .map(|name| name.span)
                .unwrap_or(function.span),
        ),
        children: None,
    }
}

const KEYWORDS: &[&str] = &[
    "if", "else", "for", "while", "do", "switch", "case", "default", "break", "continue", "goto",
    "return", "sizeof", "alignof", "struct", "union", "enum", "typedef", "static", "extern",
    "inline", "const", "volatile", "unsigned", "signed", "long", "short", "int", "char", "void",
    "float", "double", "_Bool", "impl", "trait", "fn", "let", "mut", "use", "self", "asm!",
];

#[tokio::main]
async fn main() {
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();
    let (service, socket) = LspService::new(Backend::new);
    Server::new(stdin, stdout, socket).serve(service).await;
}
