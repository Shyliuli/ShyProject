//! The probe half of one editor buffer.
//!
//! The driver owns the probe rounds: it compiles the const fn calls, runs the
//! probe image in the emulator, and writes the answers it collected. This
//! module gives it the buffer as it is in the editor and reads the answers
//! back, so a generated item exists in the editor as soon as it exists for a
//! real build.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use shyc::lexer::{Token, TokenKind};
use shyc::parser::{self, RequestKey};
use shyc::source::Span;

static COUNTER: AtomicUsize = AtomicUsize::new(0);

/// First line of both files of the const fn probe protocol.
const HEADER: &str = "shyc-probe 1";

/// The answers of one probe run, still keyed the way the driver wrote them.
///
/// The keys carry the driver's file ids; the tokens only get their source span
/// when the editor parses the same buffer and can name the call each answer
/// belongs to.
#[derive(Default)]
pub struct Answers {
    entries: Vec<(RequestKey, RawAnswer)>,
}

enum RawAnswer {
    Int(i64),
    Tokens(Vec<Token>),
}

impl Answers {
    /// Give every answer the span of the call it answers and hand the map to
    /// the parser.
    ///
    /// The returned spans are the items a token answer generated. Their tokens
    /// all carry the call site, which is how the index tells a generated
    /// signature from a hand-written one.
    pub fn build(&self, parsed: &parser::ParseOutput) -> (parser::Answers, Vec<Span>) {
        let mut answers = parser::Answers::default();
        let mut generated = Vec::new();
        for (key, raw) in &self.entries {
            let span = parsed
                .requests
                .iter()
                .find(|request| request.key == *key)
                .map(|request| request.site)
                .unwrap_or(Span {
                    file: key.file,
                    start: key.site,
                    end: key.site,
                });
            let answer = match raw {
                RawAnswer::Int(value) => parser::Answer::Int(*value),
                RawAnswer::Tokens(tokens) => {
                    let mut tokens = tokens.clone();
                    for token in &mut tokens {
                        token.span = span;
                    }
                    if !generated.contains(&span) {
                        generated.push(span);
                    }
                    parser::Answer::Tokens(tokens)
                }
            };
            answers.insert(key.clone(), answer);
        }
        (answers, generated)
    }
}

pub struct Outcome {
    /// Whether every round the driver started finished successfully.
    pub success: bool,
    /// The answers every finished round produced, if the driver wrote any.
    pub answers: Option<Arc<Answers>>,
    /// Where a failed probe named a source location, the offset and message.
    pub failure: Option<(usize, String)>,
}

/// Run the probe rounds for one buffer and read the answers they leave.
///
/// The buffer is written next to the operating system's temporary files, not
/// over the user's file: the driver reads a path, while the editor may hold
/// unsaved text. The original directory is first on the include path so a
/// quoted include still resolves from the file it was written in.
pub fn run(
    program: &Path,
    path: &Path,
    text: &str,
    include_dirs: &[PathBuf],
    defines: &[(String, String)],
) -> Outcome {
    let (source, answers) = temp_paths(path);
    if std::fs::write(&source, text).is_err() {
        return Outcome {
            success: false,
            answers: None,
            failure: None,
        };
    }

    let mut command = Command::new(program);
    command
        .arg("--probe-only")
        .arg(&source)
        .arg("--probe-answers")
        .arg(&answers);
    if let Some(parent) = path.parent() {
        command.arg(format!("-I{}", parent.display()));
    }
    for dir in include_dirs {
        command.arg(format!("-I{}", dir.display()));
    }
    for (name, value) in defines {
        if value.is_empty() {
            command.arg(format!("-D{name}"));
        } else {
            command.arg(format!("-D{name}={value}"));
        }
    }

    let output = command.output();
    let success = output.as_ref().is_ok_and(|output| output.status.success());
    let mut outcome = Outcome {
        success,
        answers: read_answers(&answers).map(Arc::new),
        failure: None,
    };
    if let Ok(output) = output {
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            outcome.failure = failure(&stderr, &source, path, text);
        }
    }

    let _ = std::fs::remove_file(&source);
    let _ = std::fs::remove_file(&answers);
    outcome
}

fn temp_paths(path: &Path) -> (PathBuf, PathBuf) {
    let id = COUNTER.fetch_add(1, Ordering::SeqCst);
    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or("shyc");
    let name = format!("shyc-lsp-{}-{id}.{extension}", std::process::id());
    let dir = std::env::temp_dir();
    let source = dir.join(&name);
    let answers = dir.join(format!("{name}.answers"));
    (source, answers)
}

/// Read the answers file of the last finished round.
///
/// A line that does not fit the protocol is skipped: a diagnostic from the
/// probe is already the failure to report, and guessing at a truncated answer
/// would only produce a second, invented error.
fn read_answers(path: &Path) -> Option<Answers> {
    let text = std::fs::read_to_string(path).ok()?;
    let mut lines = text.lines();
    if lines.next()?.trim_end() != HEADER {
        return None;
    }
    let mut answers = Answers::default();
    for line in lines {
        let line = line.trim_end();
        if line.is_empty() {
            continue;
        }
        let mut fields = line.splitn(4, '\t');
        let (Some(key), Some(kind), Some(value)) = (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        let Some(key) = RequestKey::from_text(key) else {
            continue;
        };
        match kind {
            "int32" => {
                if let Ok(value) = value.trim().parse::<i64>() {
                    answers.entries.push((key, RawAnswer::Int(value)));
                }
            }
            "tokens" => {
                let Some(count) = value.trim().parse::<usize>().ok() else {
                    continue;
                };
                let Some(tokens) = token_answer(count, fields.next(), &key) else {
                    continue;
                };
                answers.entries.push((key, RawAnswer::Tokens(tokens)));
            }
            _ => {}
        }
    }
    Some(answers)
}

/// The tokens of one `tokens` answer. Every spelling is a committed byte
/// record; the span is a placeholder until the parse names the call.
fn token_answer(count: usize, items: Option<&str>, key: &RequestKey) -> Option<Vec<Token>> {
    let span = Span {
        file: key.file,
        start: key.site,
        end: key.site,
    };
    let mut tokens = Vec::with_capacity(count);
    for item in items.unwrap_or("").split_terminator(';') {
        let (kind, hex) = item.trim().split_once(':')?;
        let number = kind.trim().parse::<u32>().ok()?;
        let spelling = String::from_utf8(decode_hex(hex)?).ok()?;
        let kind = parser::token_from_probe(number, &spelling)?;
        let at_line_start = tokens
            .last()
            .is_none_or(|previous: &Token| matches!(previous.kind, TokenKind::Newline));
        tokens.push(Token {
            kind,
            span,
            at_line_start,
            leading_space: true,
        });
    }
    (tokens.len() == count).then_some(tokens)
}

fn decode_hex(text: &str) -> Option<Vec<u8>> {
    let text = text.trim();
    if !text.len().is_multiple_of(2) || !text.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(&text[index..index + 2], 16).ok())
        .collect()
}

/// The first diagnostic the driver printed for the buffer, if it printed one.
///
/// The driver speaks `path:line:column: message`; an emulator failure has no
/// source location and is left to the probe status instead.
fn failure(stderr: &str, source: &Path, path: &Path, text: &str) -> Option<(usize, String)> {
    for line in stderr.lines() {
        let Some((file, line_number, column, message)) = diagnostic_location(line) else {
            continue;
        };
        // `shyc` prefixes its own rendered diagnostics with its name.
        let file = file.strip_prefix("shyc: ").unwrap_or(file);
        let file = Path::new(file);
        if file != source && file != path {
            continue;
        }
        let Some(offset) = offset_at(text, line_number, column) else {
            continue;
        };
        return Some((offset, message.to_string()));
    }
    None
}

/// Split `path:line:column: message` into its parts. The message may contain
/// colons, so the location is the first `:digits:digits:` run in the line.
fn diagnostic_location(line: &str) -> Option<(&str, usize, usize, &str)> {
    let mut index = 0;
    while let Some(offset) = line[index..].find(':') {
        let colon = index + offset;
        let rest = &line[colon + 1..];
        let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
        if digits > 0 {
            let after_line = &rest[digits..];
            if let Some(after_line) = after_line.strip_prefix(':') {
                let column_digits = after_line.bytes().take_while(u8::is_ascii_digit).count();
                if column_digits > 0 {
                    if let Some(message) = after_line[column_digits..].strip_prefix(':') {
                        let line_number = rest[..digits].parse().ok()?;
                        let column = after_line[..column_digits].parse().ok()?;
                        return Some((&line[..colon], line_number, column, message.trim()));
                    }
                }
            }
        }
        index = colon + 1;
    }
    None
}

/// Byte offset of a one-based line and character column.
fn offset_at(text: &str, line: usize, column: usize) -> Option<usize> {
    let mut start = 0;
    for _ in 1..line {
        start += text[start..].find('\n')? + 1;
    }
    let end = text[start..]
        .find('\n')
        .map_or(text.len(), |offset| start + offset);
    let mut offset = start;
    for (current, character) in (1..).zip(text[start..end].chars()) {
        if current >= column {
            break;
        }
        offset += character.len_utf8();
    }
    Some(offset)
}
