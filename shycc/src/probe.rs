//! Opt-in const-fn probing.
//!
//! `--constfn-probe` is the only way in, and an ordinary compile never runs
//! anything. A probe is not the user's program: the compiler names the const-fn
//! calls it cannot fold, the driver links those calls into a separate probe
//! image and runs that, and the answers go back into the compiler for the next
//! round. The final `.sfs` is never executed.
//!
//! The runner is compiled in only with the `constfn-probe` feature, so a normal
//! build carries no emulator dependency. The feature decides which code exists,
//! not what a program does: running a probe still needs the runtime flag, and the
//! probe image still has to speak the emulator's HELLO handshake (magic
//! 0x50524642, ABI version 1). A scalar answer comes from a RESULT frame and a
//! token sequence from the blob the guest commits (service COMMIT); neither
//! comes from an exit code.
//!
//! One round asks about every call the compiler cannot fold, and each request
//! runs in its own emulator loaded from the same image: the trampoline
//! dispatches on the request id the driver writes into the image's selector
//! word, so the world snapshot, arena and globals of one request never affect
//! another. The runs go in parallel and their answers are collected in manifest
//! order.
//!
//! A probe that needs a service only the compiler can give (lexing text for
//! `tokens_from_text`, for example) pauses on HOST_REQUEST instead of sending a
//! RESULT. [`run_round`] keeps the emulator alive, hands each request to a host
//! handler, and writes the reply back before the guest continues.
//! [`answer_host_request`] is that ABI: echo one int, report the byte count of
//! a guest string, or lex text into the `struct Tokens` layout in guest memory.
//!
//! The file protocol with `shyc` is line based:
//!
//! ```text
//! manifest:  "shyc-probe 1", then "<id>\t<RequestKey.text>\t<i32s|i32u|bool|tokens>"
//! answers:   "shyc-probe 1", then "<key>\tint32\t<decimal>" for a scalar, or
//!            "<key>\ttokens\t<count>\t<kind>:<hex>;..." for a token sequence
//! ```
//!
//! A header-only manifest means the assembly `shyc` wrote is the ordinary user
//! unit, so no round is left to run. A probe unit supplies the linker's own
//! entry (`_start`) and need not have a `main`; the driver assumes neither and
//! only relies on that entry.

use anyhow::{Result, bail};

#[cfg(feature = "constfn-probe")]
use std::fs;
#[cfg(feature = "constfn-probe")]
use std::path::Path;
#[cfg(feature = "constfn-probe")]
use std::time::Duration;

#[cfg(feature = "constfn-probe")]
use anyhow::Context;
#[cfg(feature = "constfn-probe")]
use emu::cpu::Emu;
#[cfg(feature = "constfn-probe")]
use emu::probe::{
    HostRequest, MAX_PROBE_STEPS, MAX_PROBE_TIMEOUT, ProbeOptions, ProbeOutcome, RESULT_KIND_INT32,
    ResultFrame, Stop,
};
#[cfg(feature = "constfn-probe")]
use shyc::lexer;
#[cfg(feature = "constfn-probe")]
use shyc::lexer::TokenKind;
#[cfg(feature = "constfn-probe")]
use shyc::source::SourceMap;

/// Header that opens both files of the `shyc` probe protocol.
#[cfg(feature = "constfn-probe")]
const HEADER: &str = "shyc-probe 1";

/// Words in one host request: the op followed by four operands.
#[cfg(feature = "constfn-probe")]
const HOST_REQUEST_WORDS: usize = 5;
/// Host op 1: answer `a0` unchanged, as one big-endian word.
#[cfg(feature = "constfn-probe")]
const HOST_OP_ECHO_INT: u32 = 1;
/// Host op 2: read `a1` bytes at guest address `a0` and answer their count.
#[cfg(feature = "constfn-probe")]
const HOST_OP_UTF8_LEN: u32 = 2;
/// Host op 3: lex the `a1` bytes at guest address `a0` and materialize the
/// resulting `struct Tokens` in the scratch buffer at `a2`, capacity `a3`.
#[cfg(feature = "constfn-probe")]
const HOST_OP_TOKENS_FROM_TEXT: u32 = 3;
/// op 4: run the thunk at a0 in a child emulator and answer 1 when it
/// finished. try_run uses it to try an ordinary function in a round.
#[cfg(feature = "constfn-probe")]
const HOST_OP_TRY: u32 = 4;
/// op 5: abort on token arena exhaustion; no answer is committed.
#[cfg(feature = "constfn-probe")]
const HOST_OP_TOKEN_ALLOCATION_FAILURE: u32 = 5;
/// Ops 6 and 7 query the round world using the compiler matcher.
#[cfg(feature = "constfn-probe")]
const HOST_OP_IMPLEMENTS: u32 = 6;
#[cfg(feature = "constfn-probe")]
const HOST_OP_DECLARED: u32 = 7;
/// op 8: reject execution of an ordinary body containing syntax placeholders.
#[cfg(feature = "constfn-probe")]
const HOST_OP_UNRESOLVED_BODY: u32 = 8;
/// op 9: record one `try_provide_impl` offer (owner at a0, trait at a1) and
/// answer 1. The driver keeps only the earliest offer per pair.
#[cfg(feature = "constfn-probe")]
const HOST_OP_PROVIDE_IMPL: u32 = 9;
/// The highest `TokenKind` a committed token stream may carry: the seven kinds
/// the compiler's lexer declares (ident, number, string, char, punct, newline,
/// eof).
#[cfg(feature = "constfn-probe")]
const LAST_TOKEN_KIND: u32 = 6;
/// `sizeof(struct Tokens)`: the count and the four parallel-array pointers.
#[cfg(feature = "constfn-probe")]
const TOKENS_HEADER_BYTES: usize = 20;

/// Whether this build carries the probe runner.
#[cfg(feature = "constfn-probe")]
pub fn ensure_runner() -> Result<()> {
    Ok(())
}

#[cfg(not(feature = "constfn-probe"))]
pub fn ensure_runner() -> Result<()> {
    bail!(
        "`--constfn-probe` is unavailable: this driver was built without the \
         `constfn-probe` feature, so nothing was executed"
    )
}

/// Reject probe budgets that are zero or past the emulator's caps.
///
/// The emulator refuses such values when a round starts; checking them here
/// turns a bad flag into a usage error before anything is compiled.
#[cfg(feature = "constfn-probe")]
pub fn check_limits(steps: u64, timeout_ms: u64) -> Result<()> {
    if steps == 0 || steps > MAX_PROBE_STEPS {
        bail!("`--constfn-steps` must be in 1..={MAX_PROBE_STEPS}");
    }
    let max_timeout_ms = MAX_PROBE_TIMEOUT.as_millis() as u64;
    if timeout_ms == 0 || timeout_ms > max_timeout_ms {
        bail!("`--constfn-timeout-ms` must be in 1..={max_timeout_ms}");
    }
    Ok(())
}

/// Without the runner there is nothing to budget; the flag was refused earlier.
#[cfg(not(feature = "constfn-probe"))]
pub fn check_limits(_steps: u64, _timeout_ms: u64) -> Result<()> {
    Ok(())
}

/// The emulator options of one probe round, from the driver's budgets.
///
/// The parser has already checked them with [`check_limits`].
#[cfg(feature = "constfn-probe")]
pub fn options(steps: u64, timeout_ms: u64) -> ProbeOptions {
    ProbeOptions {
        max_steps: steps,
        timeout: Duration::from_millis(timeout_ms),
        ..ProbeOptions::default()
    }
}

/// What one manifest request asks for.
#[cfg(feature = "constfn-probe")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// One scalar word, delivered by a RESULT frame.
    Scalar(Scalar),
    /// A token sequence, delivered by the guest's COMMIT blob.
    Tokens,
}

/// How a scalar request's RESULT word is spelled in the answers file.
#[cfg(feature = "constfn-probe")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scalar {
    /// Signed 32-bit integer.
    I32S,
    /// Unsigned 32-bit integer.
    I32U,
    /// Boolean, written as 0 or 1.
    Bool,
}

#[cfg(feature = "constfn-probe")]
impl Kind {
    fn parse(text: &str) -> Option<Kind> {
        match text {
            "i32s" => Some(Kind::Scalar(Scalar::I32S)),
            "i32u" => Some(Kind::Scalar(Scalar::I32U)),
            "bool" => Some(Kind::Scalar(Scalar::Bool)),
            "tokens" => Some(Kind::Tokens),
            _ => None,
        }
    }
}

#[cfg(feature = "constfn-probe")]
impl Scalar {
    /// The decimal the answers file carries for the RESULT's low word.
    fn decimal(self, low: u32) -> String {
        match self {
            Scalar::I32S => (low as i32).to_string(),
            Scalar::I32U => low.to_string(),
            Scalar::Bool => u8::from(low != 0).to_string(),
        }
    }
}

/// One request line of the manifest.
#[cfg(feature = "constfn-probe")]
pub struct Request {
    /// RESULT id the probe unit answers with.
    pub id: u32,
    /// Stable identity of the call, as `RequestKey::text`.
    pub key: String,
    /// How to read the answer.
    pub kind: Kind,
}

/// One answer line for `shyc`.
#[cfg(feature = "constfn-probe")]
pub enum Answer {
    /// A scalar answer: `<key>\tint32\t<decimal>`.
    Scalar { key: String, value: String },
    /// A token sequence: `<key>\ttokens\t<count>\t<kind>:<hex>;...`.
    Tokens { key: String, tokens: Vec<Token> },
    /// A generator suspended until this exact impl pair exists.
    Suspended {
        key: String,
        need: bool,
        owner: String,
        trait_name: String,
    },
}

/// One token of a committed sequence.
#[cfg(feature = "constfn-probe")]
pub struct Token {
    /// The `TokenKind` discriminant: 0=ident, 1=number, 2=string, 3=char,
    /// 4=punct, 5=newline, 6=eof.
    pub kind: u32,
    /// The token's spelling bytes.
    pub spelling: Vec<u8>,
}

/// Read a manifest and return its requests in order.
#[cfg(feature = "constfn-probe")]
pub fn read_manifest(path: &Path) -> Result<Vec<Request>> {
    let text = fs::read_to_string(path)
        .with_context(|| format!("failed to read probe manifest {}", path.display()))?;
    let mut lines = text.lines();
    if lines.next() != Some(HEADER) {
        bail!(
            "bad probe manifest {}: the first line must be `{HEADER}`",
            path.display()
        );
    }
    let mut requests = Vec::new();
    for (offset, line) in lines.enumerate() {
        if line.is_empty() {
            continue;
        }
        let mut fields = line.split('\t');
        let (Some(id), Some(key), Some(kind), None) =
            (fields.next(), fields.next(), fields.next(), fields.next())
        else {
            bail!(
                "bad probe request on line {} of {}: expected `id\\tkey\\tkind`",
                offset + 2,
                path.display()
            );
        };
        let id = id.parse().with_context(|| {
            format!("bad probe id on line {} of {}", offset + 2, path.display())
        })?;
        let kind = Kind::parse(kind).with_context(|| {
            format!(
                "bad probe kind `{kind}` on line {} of {}",
                offset + 2,
                path.display()
            )
        })?;
        requests.push(Request {
            id,
            key: key.to_string(),
            kind,
        });
    }
    Ok(requests)
}

/// Write the answers file for one round.
///
/// A scalar answer keeps the `int32` line. A token sequence is written as
/// `<count>` followed by one `<kind>:<hex>;` field per token, where the hex
/// is the uppercase spelling of the token's bytes; an empty sequence stops at
/// the count and writes no field at all.
#[cfg(feature = "constfn-probe")]
pub fn write_answers(path: &Path, answers: &[Answer]) -> Result<()> {
    let mut text = String::from(HEADER);
    text.push('\n');
    for answer in answers {
        match answer {
            Answer::Scalar { key, value } => {
                text.push_str(key);
                text.push_str("\tint32\t");
                text.push_str(value);
            }
            Answer::Tokens { key, tokens } => {
                text.push_str(key);
                text.push_str("\ttokens\t");
                text.push_str(&tokens.len().to_string());
                if !tokens.is_empty() {
                    text.push('\t');
                    for token in tokens {
                        text.push_str(&token.kind.to_string());
                        text.push(':');
                        text.push_str(&hex(&token.spelling));
                        text.push(';');
                    }
                }
            }
            Answer::Suspended {
                key,
                need,
                owner,
                trait_name,
            } => {
                text.push_str(key);
                text.push_str("\tsuspended\t");
                text.push(if *need { '1' } else { '0' });
                text.push('\t');
                text.push_str(&hex(owner.as_bytes()));
                text.push('\t');
                text.push_str(&hex(trait_name.as_bytes()));
            }
        }
        text.push('\n');
    }
    fs::write(path, text)
        .with_context(|| format!("failed to write probe answers {}", path.display()))
}

/// The uppercase hex spelling of a token's bytes.
#[cfg(feature = "constfn-probe")]
fn hex(bytes: &[u8]) -> String {
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        text.push_str(&format!("{byte:02X}"));
    }
    text
}

/// Run every request of one manifest, each in its own emulator, and turn what
/// each returned into one answer.
///
/// The image holds every call of the round and the trampoline dispatches on the
/// request id the run is given, so the runs are independent: each one starts
/// from the same image bytes and therefore from the same world snapshot, arena
/// and globals. A scalar request is answered by a kind 0 RESULT frame whose id
/// names it, and the manifest's kind decides the decimal. A `tokens` request is
/// answered by the blob its own run commits, or by the suspension that stops it.
///
/// Two generators of one round can both pass `try_provide_impl` for the same
/// impl because they see the same world. Every successful offer is recorded and
/// only the earliest request (manifest order) per pair keeps its answer; the
/// later ones stay unanswered, so the next round re-asks them and they find the
/// impl. HOST_REQUEST calls are answered by [`answer_host_request`].
#[cfg(feature = "constfn-probe")]
pub fn run_answers(
    image: &[u8],
    requests: &[Request],
    options: ProbeOptions,
) -> Result<Vec<Answer>> {
    let outcomes: Vec<Result<(Answer, Vec<(String, String)>)>> = std::thread::scope(|scope| {
        let runs: Vec<_> = requests
            .iter()
            .map(|request| scope.spawn(|| run_request(image, request, options.clone())))
            .collect();
        runs.into_iter()
            .map(|run| run.join().expect("a probe run thread does not panic"))
            .collect()
    });
    let mut answers = Vec::with_capacity(requests.len());
    let mut claimed: Vec<(String, String)> = Vec::new();
    for outcome in outcomes {
        let (answer, provided) = outcome?;
        // An earlier request of this round already offers this impl, so this
        // one is left unanswered: the next round runs it against the impl.
        if provided.iter().any(|pair| claimed.contains(pair)) {
            continue;
        }
        claimed.extend(provided);
        answers.push(answer);
    }
    Ok(answers)
}

/// Run one request in its own emulator: its answer, plus the impl pairs its
/// `try_provide_impl` calls offered.
#[cfg(feature = "constfn-probe")]
fn run_request(
    image: &[u8],
    request: &Request,
    options: ProbeOptions,
) -> Result<(Answer, Vec<(String, String)>)> {
    // A failed run is only actionable with the budget it ran under and the
    // request it was answering, so the error names both.
    let budget = format!(
        "{} steps and {} ms",
        options.max_steps,
        options.timeout.as_millis()
    );
    let mut provided = Vec::new();
    let run = run_round(image, options, request.id, |host, emu| {
        answer_host_request(host, emu, &mut provided)
    })
    .with_context(|| {
        format!(
            "while evaluating const fn request `{}` with a budget of {budget}",
            request.key
        )
    })?;

    let answer = match request.kind {
        Kind::Scalar(scalar) => {
            if let Some(suspended) = &run.suspended {
                bail!(
                    "probe suspended non-token request {} (`{}`)",
                    suspended.request_id,
                    request.key
                );
            }
            if run.commit.is_some() {
                bail!(
                    "probe committed a token stream for request {} (`{}`), which asks for a scalar",
                    request.id,
                    request.key
                );
            }
            let frame = match run.results.as_slice() {
                [frame] if frame.id == request.id => *frame,
                [] => bail!(
                    "probe did not answer request {} (`{}`)",
                    request.id,
                    request.key
                ),
                _ => bail!(
                    "probe answered request {} (`{}`) with unexpected result frames",
                    request.id,
                    request.key
                ),
            };
            if frame.kind != RESULT_KIND_INT32 {
                bail!(
                    "probe result {} has kind {}, only int32 (kind 0) answers are wired up",
                    frame.id,
                    frame.kind
                );
            }
            Answer::Scalar {
                key: request.key.clone(),
                value: scalar.decimal(frame.low),
            }
        }
        Kind::Tokens => {
            if let Some(suspended) = &run.suspended {
                if suspended.request_id != request.id {
                    bail!(
                        "probe suspended request {} (`{}`) as request {}",
                        request.id,
                        request.key,
                        suspended.request_id
                    );
                }
                Answer::Suspended {
                    key: request.key.clone(),
                    need: suspended.need,
                    owner: suspended.owner.clone(),
                    trait_name: suspended.trait_name.clone(),
                }
            } else if let Some(bytes) = &run.commit {
                Answer::Tokens {
                    key: request.key.clone(),
                    tokens: parse_tokens(bytes)?,
                }
            } else {
                bail!(
                    "probe did not commit the tokens of request {} (`{}`)",
                    request.id,
                    request.key
                );
            }
        }
    };
    Ok((answer, provided))
}

/// Read a committed token stream: `u32 count`, then per token `u32 kind`,
/// `u32 len` and `len` spelling bytes, all big-endian.
///
/// The count and the per-token lengths describe the blob completely, so a count
/// the bytes cannot satisfy and bytes the count leaves behind are both errors:
/// an answer that disagrees with its own encoding is not one the compiler can
/// trust. The kind must be one of the seven `TokenKind` values the compiler and
/// the probe image share.
#[cfg(feature = "constfn-probe")]
fn parse_tokens(mut rest: &[u8]) -> Result<Vec<Token>> {
    let count = take_u32(&mut rest).context("the committed token stream has no token count")?;
    // Each token costs two words before its spelling, so a larger count cannot
    // fit the bytes that follow it.
    if count as usize > rest.len() / 8 {
        bail!(
            "the committed token stream counts {count} token(s), but only {} byte(s) follow the count",
            rest.len()
        );
    }
    let mut tokens = Vec::new();
    for index in 0..count {
        let kind = take_u32(&mut rest)
            .with_context(|| format!("the committed token stream ends at token {index}"))?;
        if kind > LAST_TOKEN_KIND {
            bail!(
                "committed token {index} has kind {kind}, but the known kinds are 0..={LAST_TOKEN_KIND}"
            );
        }
        let len = take_u32(&mut rest)
            .with_context(|| format!("the committed token stream ends at token {index}"))?;
        let len = len as usize;
        if len > rest.len() {
            bail!(
                "committed token {index} spells {len} byte(s), but only {} remain",
                rest.len()
            );
        }
        let (spelling, tail) = rest.split_at(len);
        rest = tail;
        tokens.push(Token {
            kind,
            spelling: spelling.to_vec(),
        });
    }
    if !rest.is_empty() {
        bail!(
            "the committed token stream has {} trailing byte(s) after {count} token(s)",
            rest.len()
        );
    }
    Ok(tokens)
}

/// Read one big-endian word off the front of `rest`.
#[cfg(feature = "constfn-probe")]
fn take_u32(rest: &mut &[u8]) -> Option<u32> {
    let word = rest.get(..4)?;
    *rest = &rest[4..];
    Some(u32::from_be_bytes(
        word.try_into().expect("a word is four bytes"),
    ))
}

/// What one finished probe image left for the answer stage.
#[cfg(feature = "constfn-probe")]
pub struct Suspension {
    pub request_id: u32,
    pub need: bool,
    pub owner: String,
    pub trait_name: String,
}

#[cfg(feature = "constfn-probe")]
pub struct Run {
    /// Scalar RESULT frames, in the order the guest emitted them.
    pub results: Vec<ResultFrame>,
    /// The blob the guest committed, when it stopped with COMMIT.
    pub commit: Option<Vec<u8>>,
    /// A generator request that stopped on SERVICE_SUSPEND.
    pub suspended: Option<Suspension>,
}

/// Run one request of a probe image, answering its HOST_REQUEST calls through
/// `host`.
///
/// This is `emu::probe::run` with the emulator kept alive: `run_probe` pauses on
/// every host request, `host` produces the reply -- with the live emulator, so
/// an op can read guest memory -- and `respond_probe` writes it back before the
/// run continues. `selector` is the request id the trampoline dispatches on;
/// it goes into the image's selector word before the run, so the same image
/// answers any one of its calls.
///
/// A clean finish is EXIT (`Stop::Done`) or COMMIT (`Stop::Commit`); the
/// latter's blob comes back in [`Run::commit`]. A trap, a foreign syscall, or
/// an exhausted budget means the answers are not the ones the compiler asked
/// for, so they are reported instead of being used. An error from `host`
/// aborts the round too.
#[cfg(feature = "constfn-probe")]
pub fn run_round<F>(image: &[u8], options: ProbeOptions, selector: u32, mut host: F) -> Result<Run>
where
    F: FnMut(&HostRequest, &mut Emu) -> Result<Vec<u8>>,
{
    let mut emu = Emu::new(options.debug, options.mem_size);
    emu.load_image(image)?;
    emu.debug_write_mem(shyc::probe::SELECTOR, &selector.to_be_bytes())?;
    emu.attach_probe(options)?;
    loop {
        match emu.run_probe() {
            Stop::Request(request) => {
                let reply = host(&request, &mut emu)?;
                emu.respond_probe(&reply)?;
            }
            stop => {
                return check_stop(ProbeOutcome {
                    stop,
                    exit: emu.probe_exit(),
                    stdout: emu.take_probe_output(),
                    results: emu.take_probe_results(),
                    steps: emu.probe_steps(),
                    output_truncated: emu.probe_output_truncated(),
                });
            }
        }
    }
}

/// One host request as the first ABI slice reads it: an op and four operands.
#[cfg(feature = "constfn-probe")]
struct HostCall {
    op: u32,
    args: [u32; 4],
}

/// Decode `[op][a0][a1][a2][a3]`, big-endian, from the request bytes.
#[cfg(feature = "constfn-probe")]
fn parse_host_call(request: &HostRequest) -> Result<HostCall> {
    let expected = HOST_REQUEST_WORDS * 4;
    if request.bytes.len() != expected {
        bail!(
            "host request service {} carries {} byte(s), expected {expected}",
            request.service,
            request.bytes.len()
        );
    }
    let mut words = [0u32; HOST_REQUEST_WORDS];
    for (word, bytes) in words.iter_mut().zip(request.bytes.chunks_exact(4)) {
        *word = u32::from_be_bytes(bytes.try_into().expect("a chunk is four bytes"));
    }
    Ok(HostCall {
        op: words[0],
        args: [words[1], words[2], words[3], words[4]],
    })
}

/// Stack a try_run child runs on: above the probe stack, and its own, so a
/// failed attempt cannot clobber the round's frames.
#[cfg(feature = "constfn-probe")]
const TRY_STACK: u32 = 0x0180_0000;
/// Bytes reserved for that child stack.
#[cfg(feature = "constfn-probe")]
const TRY_STACK_BYTES: u32 = 65536;
/// Instructions a try_run child may execute before it counts as failed.
#[cfg(feature = "constfn-probe")]
const TRY_STEPS: u64 = 4_000_000;

/// Incomplete compiler inputs and exhausted token storage are fatal compiler
/// errors, not ordinary failed attempts. Preserve that distinction through
/// child rollback and try_run.
#[cfg(feature = "constfn-probe")]
#[derive(Debug)]
struct FatalProbeError(String);

#[cfg(feature = "constfn-probe")]
impl std::fmt::Display for FatalProbeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(feature = "constfn-probe")]
impl std::error::Error for FatalProbeError {}

/// Answer one try_run request: run the thunk in a child emulator.
///
/// The child commits its memory writes only when it reaches the EXIT the thunk
/// ends with, so a failed attempt leaves the parent untouched and this answers
/// 0. A child may make read-only world queries, which use its view of the same
/// round world, and record the impls it offers like the parent run does.
/// Unsupported user requests fail the attempt and roll back; fatal compiler
/// errors instead abort the parent probe as well.
#[cfg(feature = "constfn-probe")]
fn answer_try(
    call: &HostCall,
    emu: &mut Emu,
    provided: &mut Vec<(String, String)>,
) -> Result<Vec<u8>> {
    let [entry, ..] = call.args;
    let options = emu::probe::ChildOptions {
        site: 0,
        entry,
        sp: TRY_STACK,
        stack_bytes: TRY_STACK_BYTES,
        parent_depth: 0,
        max_steps: TRY_STEPS,
        timeout: std::time::Duration::from_secs(5),
        output_cap: 1,
    };
    let outcome = emu.run_child_with_host(options, |request, child, _depth| {
        let query = parse_host_call(request)?;
        match query.op {
            HOST_OP_IMPLEMENTS | HOST_OP_DECLARED => answer_world_query(&query, child),
            HOST_OP_PROVIDE_IMPL => answer_provide_impl(&query, child, provided),
            HOST_OP_UNRESOLVED_BODY | HOST_OP_TOKEN_ALLOCATION_FAILURE => {
                fatal_host_request(&query, child)
            }
            op => anyhow::bail!(
                "host op {HOST_OP_TRY}: a try_run child cannot make host request {op}"
            ),
        }
    });
    let answered = match outcome {
        Err(error) if error.is::<FatalProbeError>() => return Err(error),
        Ok(child) => child.committed && matches!(child.stop, emu::probe::Stop::Done),
        Err(_) => false,
    };
    Ok(u32::from(answered).to_be_bytes().to_vec())
}

/// Compiler failures remain fatal even when reached through a try_run child.
#[cfg(feature = "constfn-probe")]
fn fatal_host_request(call: &HostCall, emu: &Emu) -> Result<Vec<u8>> {
    let message = match call.op {
        HOST_OP_UNRESOLVED_BODY => {
            let name = query_name(emu, call.args[0])?;
            format!(
                "function '{name}' still contains an unresolved const fn syntax expansion; no answer was committed"
            )
        }
        HOST_OP_TOKEN_ALLOCATION_FAILURE => {
            let [pool, count, available, ..] = call.args;
            let pool = match pool {
                0 => "integer slots",
                1 => "text pointer slots",
                2 => "bytes",
                _ => bail!("token allocation failure names unknown pool {pool}"),
            };
            format!(
                "const fn token arena exhausted: requested {} {pool}, {} available; no answer was committed",
                count as i32, available as i32
            )
        }
        op => bail!("unknown fatal probe request {op}"),
    };
    Err(FatalProbeError(message).into())
}

/// Answer one host request: the first ABI slice.
///
/// The request bytes are `[op][a0][a1][a2][a3]`, big-endian; the mailbox's
/// service id is informational because the op already names the operation.
/// op 1 echoes `a0` as one big-endian word. op 2 reads `a1` bytes at guest
/// address `a0` and answers their count. op 3 lexes the text at `a0`/`a1` into
/// the `struct Tokens` layout at `a2`/`a3` and answers one word: 0 when the
/// scratch holds the whole value, 1 when it is too small. op 9 records one
/// `try_provide_impl` offer in `provided`. Any other op is an error, never
/// zero.
#[cfg(feature = "constfn-probe")]
pub fn answer_host_request(
    request: &HostRequest,
    emu: &mut Emu,
    provided: &mut Vec<(String, String)>,
) -> Result<Vec<u8>> {
    let call = parse_host_call(request)?;
    match call.op {
        HOST_OP_ECHO_INT => Ok(call.args[0].to_be_bytes().to_vec()),
        HOST_OP_UTF8_LEN => {
            let [address, length, ..] = call.args;
            let text = emu.debug_read_mem(address, length as usize).with_context(|| {
                format!(
                    "host op {HOST_OP_UTF8_LEN}: cannot read {length} byte(s) at guest address 0x{address:08X}"
                )
            })?;
            // The bytes are not decoded yet; op 3 will lex them.
            Ok((text.len() as u32).to_be_bytes().to_vec())
        }
        HOST_OP_TOKENS_FROM_TEXT => answer_tokens_from_text(&call, emu),
        HOST_OP_TRY => answer_try(&call, emu, provided),
        HOST_OP_IMPLEMENTS | HOST_OP_DECLARED => answer_world_query(&call, emu),
        HOST_OP_PROVIDE_IMPL => answer_provide_impl(&call, emu, provided),
        HOST_OP_UNRESOLVED_BODY | HOST_OP_TOKEN_ALLOCATION_FAILURE => {
            fatal_host_request(&call, emu)
        }
        op => bail!("unsupported host request op {op}"),
    }
}

/// Record one `try_provide_impl` offer and answer 1.
///
/// The guest already saw that the round world lacks the impl, so 1 is the same
/// answer it would get with a single run. Recording the pair is what lets the
/// driver keep only the earliest offer of a round and leave the later requests
/// unanswered.
#[cfg(feature = "constfn-probe")]
fn answer_provide_impl(
    call: &HostCall,
    emu: &Emu,
    provided: &mut Vec<(String, String)>,
) -> Result<Vec<u8>> {
    let [owner_ptr, trait_ptr, ..] = call.args;
    let owner = query_name(emu, owner_ptr)?;
    let trait_name = query_name(emu, trait_ptr)?;
    provided.push((owner, trait_name));
    Ok(1u32.to_be_bytes().to_vec())
}

/// Read a bounded, NUL-terminated query name without reading past its end.
#[cfg(feature = "constfn-probe")]
fn query_name(emu: &Emu, address: u32) -> Result<String> {
    let mut bytes = Vec::new();
    for offset in 0..emu::probe::MAX_PROBE_STRING {
        let at = address
            .checked_add(offset as u32)
            .context("world query name address overflow")?;
        let byte = emu
            .debug_read_mem(at, 1)
            .with_context(|| format!("cannot read world query name at 0x{at:08X}"))?[0];
        if byte == 0 {
            return String::from_utf8(bytes).context("world query name is not UTF-8");
        }
        bytes.push(byte);
    }
    bail!(
        "world query name exceeds {} bytes",
        emu::probe::MAX_PROBE_STRING
    )
}

/// Decode the compiler-produced round world and use exactly the same matcher
/// that the declaration scheduler uses when waking a suspended generator.
#[cfg(feature = "constfn-probe")]
fn answer_world_query(call: &HostCall, emu: &mut Emu) -> Result<Vec<u8>> {
    let [owner_ptr, trait_ptr, world_ptr, world_len] = call.args;
    if world_len as usize > emu::probe::MAX_PROBE_REQUEST {
        bail!(
            "world query snapshot exceeds {} bytes",
            emu::probe::MAX_PROBE_REQUEST
        );
    }
    let data = emu
        .debug_read_mem(world_ptr, world_len as usize)
        .context("cannot read world query snapshot")?;
    let world = shyc::probe::decode_world(&data).map_err(anyhow::Error::msg)?;
    let owner = query_name(emu, owner_ptr)?;
    let answer = if call.op == HOST_OP_DECLARED {
        world.names.iter().any(|name| name == &owner)
    } else {
        let trait_name = query_name(emu, trait_ptr)?;
        shyc::probe::query_implements(&world, &owner, &trait_name)
    };
    Ok(u32::from(answer).to_be_bytes().to_vec())
}

/// Lex the text of one op 3 request and write its `struct Tokens` value into
/// the guest scratch buffer.
///
/// The scratch layout is the one `core/comptime.shyh` declares, all fields
/// big-endian `u32`:
///
/// ```text
/// +0  count
/// +4  kinds   -> count words
/// +8  texts   -> count words, each pointing at a NUL-terminated spelling
/// +12 lens    -> count words, the spelling byte count without the NUL
/// +16 spans   -> 2 * count words, start and end offset
/// ... kinds[count], texts[count], lens[count], spans[2 * count], spellings
/// ```
///
/// A token's spelling is `shyc::lexer::spelling`, so the bytes match the guest
/// image's own baking. Spans are zero for now: the compiler has not decided
/// which offsets a host-lexed text should carry.
///
/// The whole image is built before anything is written. When it does not fit
/// `capacity`, guest memory is left untouched and the reply is 1; the required
/// size goes to stderr because the reply is only one word.
#[cfg(feature = "constfn-probe")]
fn answer_tokens_from_text(call: &HostCall, emu: &mut Emu) -> Result<Vec<u8>> {
    let [text_address, text_length, scratch, capacity] = call.args;
    let bytes = emu
        .debug_read_mem(text_address, text_length as usize)
        .with_context(|| {
            format!(
                "host op {HOST_OP_TOKENS_FROM_TEXT}: cannot read {text_length} byte(s) at guest address 0x{text_address:08X}"
            )
        })?;
    let text = String::from_utf8(bytes).with_context(|| {
        format!(
            "host op {HOST_OP_TOKENS_FROM_TEXT}: the {text_length} byte(s) at guest address 0x{text_address:08X} are not valid UTF-8"
        )
    })?;

    let mut sources = SourceMap::default();
    let file = sources.add("<probe-tokens>", text);
    let mut tokens = lexer::lex(&sources, file).map_err(|diagnostic| {
        anyhow::anyhow!(
            "host op {HOST_OP_TOKENS_FROM_TEXT}: {}",
            sources.render(&diagnostic)
        )
    })?;
    // The lexer ends every stream with an Eof token, which is an artifact of
    // lexing a whole file. A token sequence a const fn builds is a fragment, so
    // the Eof goes: joining two sequences must not put one in the middle.
    if matches!(tokens.last().map(|token| &token.kind), Some(TokenKind::Eof)) {
        tokens.pop();
    }

    let spellings: Vec<Vec<u8>> = tokens
        .iter()
        .map(|token| lexer::spelling(token).into_bytes())
        .collect();
    let total = tokens_scratch_bytes(&spellings);
    // A probe cannot read this reply and recover: the generated guest stub
    // hands the scratch buffer back as the result as soon as the call returns.
    // Too small is therefore a compiler limit, not a value the generator could
    // act on, so it fails the round instead of answering with an empty table.
    if total > capacity as usize {
        bail!(
            "host op {HOST_OP_TOKENS_FROM_TEXT}: {} token(s) need {total} byte(s) of scratch at 0x{scratch:08X}, but the guest offered {capacity}",
            tokens.len()
        );
    }
    if scratch.checked_add(total as u32).is_none() {
        bail!(
            "host op {HOST_OP_TOKENS_FROM_TEXT}: scratch at 0x{scratch:08X} plus {total} byte(s) leaves the guest address space"
        );
    }
    let image = tokens_image(scratch, &tokens, &spellings);
    emu.debug_write_mem(scratch, &image).with_context(|| {
        format!(
            "host op {HOST_OP_TOKENS_FROM_TEXT}: cannot write {total} byte(s) at guest address 0x{scratch:08X}"
        )
    })?;
    Ok(0u32.to_be_bytes().to_vec())
}

/// Bytes needed for a `struct Tokens` over `spellings`: the header, five
/// words per token for the arrays (spans take two), and one NUL per spelling.
#[cfg(feature = "constfn-probe")]
fn tokens_scratch_bytes(spellings: &[Vec<u8>]) -> usize {
    TOKENS_HEADER_BYTES
        + spellings.len() * 20
        + spellings
            .iter()
            .map(|spelling| spelling.len() + 1)
            .sum::<usize>()
}

/// Build the whole `struct Tokens` image, spellings included.
///
/// `spellings` is parallel to `tokens`. The caller has already checked that
/// the image fits the offered scratch, so every offset stays inside it.
#[cfg(feature = "constfn-probe")]
fn tokens_image(base: u32, tokens: &[lexer::Token], spellings: &[Vec<u8>]) -> Vec<u8> {
    let count = tokens.len();
    let kinds_ptr = base + TOKENS_HEADER_BYTES as u32;
    let texts_ptr = kinds_ptr + 4 * count as u32;
    let lens_ptr = texts_ptr + 4 * count as u32;
    let spans_ptr = lens_ptr + 4 * count as u32;
    let data_ptr = spans_ptr + 8 * count as u32;

    let mut image = Vec::with_capacity(tokens_scratch_bytes(spellings));
    for word in [count as u32, kinds_ptr, texts_ptr, lens_ptr, spans_ptr] {
        image.extend_from_slice(&word.to_be_bytes());
    }
    for token in tokens {
        image.extend_from_slice(&token_kind_number(&token.kind).to_be_bytes());
    }
    let mut text_ptr = data_ptr;
    for spelling in spellings {
        image.extend_from_slice(&text_ptr.to_be_bytes());
        text_ptr += (spelling.len() + 1) as u32;
    }
    for spelling in spellings {
        image.extend_from_slice(&(spelling.len() as u32).to_be_bytes());
    }
    for _ in 0..2 * count {
        image.extend_from_slice(&0u32.to_be_bytes());
    }
    for spelling in spellings {
        image.extend_from_slice(spelling);
        image.push(0);
    }
    image
}

/// The number `enum TokenKind` in `core/comptime.shyh` assigns to a token.
#[cfg(feature = "constfn-probe")]
fn token_kind_number(kind: &TokenKind) -> u32 {
    match kind {
        TokenKind::Ident(_) => 0,
        TokenKind::Number(_) => 1,
        TokenKind::StringLiteral { .. } => 2,
        TokenKind::CharLiteral { .. } => 3,
        TokenKind::Punct(_) => 4,
        TokenKind::Newline => 5,
        TokenKind::Eof => 6,
    }
}

/// Accept EXIT or COMMIT as the end of a round and reject anything else.
#[cfg(feature = "constfn-probe")]
fn check_stop(outcome: ProbeOutcome) -> Result<Run> {
    let (commit, suspended) = match outcome.stop {
        Stop::Done => (None, None),
        Stop::Commit(bytes) => (Some(bytes), None),
        Stop::Suspended {
            request_id,
            need,
            owner,
            trait_name,
        } => (
            None,
            Some(Suspension {
                request_id,
                need,
                owner,
                trait_name,
            }),
        ),
        stop => bail!(
            "probe stopped before Done: {stop:?} (exit 0x{:08X}, {} steps, {} output bytes)",
            outcome.exit,
            outcome.steps,
            outcome.stdout.len()
        ),
    };
    if outcome.output_truncated {
        bail!("probe output passed the capture limit; the run is not complete");
    }
    Ok(Run {
        results: outcome.results,
        commit,
        suspended,
    })
}
