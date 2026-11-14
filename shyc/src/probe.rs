//! Probe-only translation unit and the direct-call trampoline.
//!
//! A probe-mode parse does not evaluate; it records a request for every call it
//! cannot answer (see [`crate::parser::probe`]). This module turns one round's
//! requests into code that calls those functions directly:
//!
//! - the parsed unit's other declarations: ordinary helpers, globals, types,
//!   `impl` and `trait` items, so a cloned body can call the program's own
//!   helpers and methods and read its globals the way it does at compile time.
//!   The user's `main` and `_start` stay out: the image has exactly one entry,
//!   and that entry is the trampoline;
//! - one ordinary function item per called `const fn`, cloned under a symbol
//!   this module owns ([`ProbeCall::symbol`]), plus one clone under its
//!   declared name for every const fn a requested body reaches, because a
//!   cloned body calls its dependencies by the name they were declared with;
//! - one [`ProbeCall`] per request: the request id, that symbol, the literal
//!   scalar arguments, and the result classification;
//! - for a request whose argument is a token sequence ([Arguments::Tokens]),
//!   static storage for that sequence and an ordinary wrapper that builds a
//!   Tokens value from it and calls the clone, so the aggregate argument is
//!   placed by the compiler's own CallPlan instead of by the trampoline;
//! - for a request whose result is a Tokens value ([ResultKind::Tokens]),
//!   static storage for that value and for the serialized bytes, plus an
//!   ordinary serializer that flattens the value into `[count][kind][len]
//!   [bytes...]` with big-endian `u32` fields;
//! - for a body that asks `declared` or `implements`, the parse's declaration
//!   world ([ParseOutput::world]) encoded in the image, plus host-request
//!   helpers that query it using the same matcher as the generator scheduler;
//! - for a body that builds token sequences (`tokens_from_int`,
//!   `tokens_slice`, `tokens_concat`, `tokens_text`), ordinary definitions
//!   over a static bump arena, so a `const fn` assembles a sequence at probe
//!   time without `malloc`;
//! - a raw trampoline (`emit_probe_asm`) that sets the stack, performs the
//!   probe handshake, dispatches on the request id the driver wrote to
//!   [`SELECTOR`], places that call's arguments exactly as
//!   `abi_plan::CallPlan` says, calls the function with `calln`, and then
//!   either turns the return registers into a `RESULT` frame (a scalar) or
//!   serializes the aggregate result and commits it as one blob (Tokens).
//!
//! The trampoline is not a C `main` and never calls the user's `main`; it is
//! the linker's entry section and the one place the probe protocol is spoken.
//! The call itself is a real `calln`/`ret`, so the callee's normal ABI
//! prologue and epilogue run.
//!
//! Lower [`ProbeUnit::unit`] with [`crate::lower::StartupMode::Manual`], emit
//! it, append [`emit_probe_asm`], assemble and link the result. Lowering stores
//! each function's `CallPlan` in the IR (`ir::Function.abi`), so the
//! trampoline reads the ABI the compiler already planned instead of re-deriving
//! it.
//!
//! The unit is deliberately narrow. A function is used only when every
//! parameter is an `int`, an `unsigned`, a `bool` (`_Bool`), or the single
//! Tokens value a token argument bakes, when the result is one of the three
//! scalars for a value position or a Tokens value for a file-scope generator,
//! when all scalar parameters fit the eight Shy argument registers, when the
//! request stands in an expression or on its own at file scope, when its
//! arguments are 32-bit integer constants, and when the assembled unit lowers.
//! A Tokens argument is supported only for a function whose single parameter
//! is that value: its wrapper passes the aggregate through the compiler's
//! CallPlan. Anything else returns [`Outcome::Unsupported`] instead of an
//! answer: a probe that guessed would fold a wrong value into the program.
//!
//! The trampoline holds one entry per call and a run makes exactly one, so the
//! driver can answer every request of a round in parallel: each request gets
//! its own emulator loaded from the same image and its own selector value.
//!
//! A global whose initializer still holds this round's placeholder -- the call
//! it could not answer -- keeps that placeholder in the probe image, so a body
//! that reads it answers from data the round does not have yet. A round is as
//! sound as the initializers it keeps.

use std::collections::HashMap;

use crate::abi::{Location, Passing, ReturnPlan, ValuePart};
use crate::ast;
use crate::ir::Layout;
use crate::lexer::{self, Token};
use crate::lower;
use crate::parser::{Arguments, ConstFn, ParseOutput, Place, ProbeRequest, TrySite, WorldSnapshot};
use crate::shy::abi::ARGUMENT_REGISTERS;
use crate::source::{SourceMap, Span};

mod world;
pub use world::decode_world;
use world::encode_world;

/// Probe syscall family the emulator intercepts.
const SYSCALL: u32 = 999;
/// Service 0: handshake, magic 0x50524642 ("PRFB") in `2x` and ABI 1 in `3x`.
const SERVICE_HELLO: u32 = 0;
/// Service 1: stop the run.
const SERVICE_EXIT: u32 = 1;
/// Service 2: append one `RESULT` frame.
const SERVICE_RESULT: u32 = 2;
/// Service 4: stop the run with one blob copied out of guest memory, whose
/// pointer is in `2x` and length in `3x`.
const SERVICE_COMMIT: u32 = 4;
/// Shared state the trampoline writes before each requested call.
const QUEUE_CONTEXT_GLOBALS: &str =
    "pub int __shy_probe_current;\npub int __shy_probe_can_suspend;\n";

/// Handshake magic, ASCII "PRFB" big-endian.
const MAGIC: u32 = 0x5052_4642;
/// Probe ABI version.
const ABI: u32 = 1;
/// `RESULT` tag 0: a raw 32-bit value in `low`; the decoder applies the
/// declared signedness of [`ResultKind`].
const RESULT_INT32: u32 = 0;
/// `RESULT` tag 1: a 64-bit value with `low` first; reserved for a later slice.
const RESULT_INT64: u32 = 1;
/// Prefix of the cloned function symbols.
const CLONE: &str = "__shy_probe_fn_";
/// Prefix of the generated wrapper symbols: one per baked token request.
const WRAPPER: &str = "__shy_probe_wrap_";
/// Prefix of the baked bytes a string argument points at.
const TEXT: &str = "__shy_probe_arg_";
/// Builtins this slice does not implement. A const fn body that calls one
/// cannot be answered by a probe image, so the whole round is unsupported.
const MISSING_BUILTINS: [&str; 0] = [];
/// Stack bottom the trampoline sets. The Shy stack grows upward and the image
/// is loaded at zero, so this leaves the image below and the default 32 MiB
/// probe memory above it.
pub const STACK: u32 = 0x0100_0000;
/// The word the driver writes a request id to before a run. It stands just
/// below the stack, where the image never reaches, and the trampoline reads it
/// to decide which call the run makes.
pub const SELECTOR: u32 = STACK - 4;
/// `sizeof(struct Tokens)` in `layout`: the count word, padded to the pointer
/// alignment, followed by the four array pointers.
fn tokens_bytes(layout: Layout) -> u32 {
    layout.pointer_bytes.max(4) + 4 * layout.pointer_bytes
}
/// Symbol of the static `struct Tokens` a Tokens result is written through.
/// The trampoline puts its address in the hidden return pointer's register.
const OUT: &str = "__shy_probe_out";
/// Symbol of the static bytes the serializer flattens a Tokens value into.
const BLOB: &str = "__shy_probe_blob";
/// Capacity of the blob. A wider result is reported to the emulator as a bad
/// mailbox instead of being truncated into a wrong answer.
const BLOB_BYTES: usize = 65536;
/// Symbol of the generated serializer. Every Tokens result has the same
/// `struct Tokens` shape, so one serializer serves the whole round.
const SERIALIZER: &str = "__shy_probe_serialize";

/// How a supported const fn's result travels back from a probe.
///
/// Only the declaration spelling decides this: `int`, `unsigned`, `_Bool`, or
/// `struct Tokens`. The shyc manifest and the shycc decoder must use this same
/// classification, so both agree on what the returned bytes mean.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResultKind {
    /// `int`: the decoder sign-extends the 32-bit word.
    I32Signed,
    /// `unsigned`: the decoder zero-extends the 32-bit word.
    I32Unsigned,
    /// `_Bool`: the word is 0 or 1.
    Bool,
    /// `struct Tokens`: the answer is the `COMMIT` blob, not a register.
    Tokens,
}

/// How one parameter of a probed `const fn` receives its argument.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ParameterKind {
    /// An `int`, `unsigned`, or `_Bool` literal the trampoline places.
    Scalar(ResultKind),
    /// The call's syntax, baked into a `Tokens` value by a wrapper.
    Tokens,
    /// A `const char *` parameter, which receives the address of a string
    /// literal the probe image carries.
    Str,
}

/// Canonical spelling for world-query keys. Only complete builtin words are
/// normalized; a user identifier containing "longint" is left intact.
pub(crate) fn canonical_type_name(text: &str) -> String {
    let text = text.trim();
    let (base, args) = split_type_name(text);
    if !args.is_empty() {
        let args: Vec<String> = args.iter().map(|arg| canonical_type_name(arg)).collect();
        return format!("{}<{}>", canonical_type_name(&base), args.join(","));
    }
    if let Some(pointee) = text.strip_suffix('*') {
        return format!("{}*", canonical_type_name(pointee));
    }
    if let Some(spelling) = crate::parser::canonical_builtin_name(text) {
        return spelling;
    }
    if let Ok(value) = crate::literal::integer(
        text,
        32,
        Span {
            file: crate::source::FileId(0),
            start: 0,
            end: 0,
        },
    ) {
        return value.bits.to_string();
    }
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Splits a complete generic spelling at top-level commas, keeping nested
/// arguments intact. Non-generic text is returned unchanged.
fn split_type_name(name: &str) -> (String, Vec<String>) {
    let Some(open) = name.find('<') else {
        return (name.to_string(), Vec::new());
    };
    if !name.ends_with('>') {
        return (name.to_string(), Vec::new());
    }
    let mut arguments = Vec::new();
    let mut depth = 0usize;
    let mut start = open + 1;
    for (offset, byte) in name[open..].bytes().enumerate() {
        let at = open + offset;
        match byte {
            b'<' => depth += 1,
            b'>' => {
                let Some(next_depth) = depth.checked_sub(1) else {
                    return (name.to_string(), Vec::new());
                };
                depth = next_depth;
                if depth == 0 {
                    if at != name.len() - 1 || at == start {
                        return (name.to_string(), Vec::new());
                    }
                    arguments.push(name[start..at].trim().to_string());
                    return (name[..open].trim().to_string(), arguments);
                }
            }
            b',' if depth == 1 => {
                arguments.push(name[start..at].trim().to_string());
                start = at + 1;
            }
            _ => {}
        }
    }
    (name.to_string(), Vec::new())
}

/// Whether `owner` implements `trait_name` in `world`.
///
/// A concrete pair answers directly. A generic impl answers when its base and
/// argument count match and every bound its parameter carries holds for the
/// corresponding argument, which is the same rule the type checker applies.
pub fn query_implements(world: &WorldSnapshot, owner: &str, trait_name: &str) -> bool {
    let owner = canonical_type_name(owner);
    let trait_name = canonical_type_name(trait_name);
    if world.impls.iter().any(|pair| {
        canonical_type_name(&pair.owner) == owner
            && canonical_type_name(&pair.trait_name) == trait_name
    }) {
        return true;
    }
    let (base, args) = split_type_name(&owner);
    world.generic_impls.iter().any(|generic| {
        canonical_type_name(&generic.base) == base
            && args.len() == generic.bounds.len()
            && canonical_type_name(&generic.trait_name) == trait_name
            && generic
                .bounds
                .iter()
                .zip(args.iter())
                .all(|(bounds, argument)| {
                    bounds
                        .iter()
                        .all(|bound| query_implements(world, argument, bound))
                })
    })
}

/// One argument the trampoline places for a call.
pub enum ProbeArgument {
    /// A constant whose 32-bit value is split across the parameter's parts.
    Immediate(i64),
    /// A string literal: its bytes live in the probe image under this symbol,
    /// and the parameter receives that address.
    Text(String),
}

/// What one round's probe unit came to.
pub enum Outcome {
    /// The tree and the calls to make for this round.
    Unit(ProbeUnit),
    /// The round has no request, so there is nothing to run and nothing to
    /// fold.
    Nothing,
    /// A request this builder cannot serve, and therefore a round whose
    /// answers must not be used.
    Unsupported {
        /// The `const fn` the request calls.
        function: String,
        /// Where the call stands, for the driver's diagnostic.
        span: Span,
        /// Why the request cannot be served.
        reason: String,
    },
}

/// The tree and the calls of one probe round.
pub struct ProbeUnit {
    /// The parsed unit without its entry point, plus the called const fns
    /// cloned under the symbols in [`ProbeCall::symbol`] and their const fn
    /// dependencies under their declared names.
    /// Lower this with [`crate::lower::StartupMode::Manual`].
    pub unit: ast::TranslationUnit,
    /// One call per request, in request order.
    pub calls: Vec<ProbeCall>,
}

/// One function call the trampoline makes.
pub struct ProbeCall {
    /// The answer id: the request's index in [`ParseOutput::requests`].
    pub id: usize,
    /// The `const fn`'s declared name, for diagnostics.
    pub function: String,
    /// The symbol of the cloned function in the probe image.
    pub symbol: String,
    /// How the decoder reads the answer: a scalar in registers, or the
    /// serialized bytes of a Tokens value.
    pub kind: ResultKind,
    /// The constant argument values, in parameter order. Each fits the
    /// parameter's 32-bit type. Empty for a Tokens request, whose argument is
    /// baked into the wrapper named by [`ProbeCall::symbol`].
    pub arguments: Vec<ProbeArgument>,
    /// The call is a file-scope generator, where a missing impl suspends it.
    pub queue_enabled: bool,
}

/// One request whose token argument is baked into the probe image.
struct BakedTokens {
    /// The request id, which names the wrapper and its static arrays.
    id: usize,
    /// Index of the const fn in the parse's table.
    function: usize,
    /// How the wrapper reports the result to the trampoline.
    kind: ResultKind,
    /// The call's tokens, exactly as captured.
    tokens: Vec<Token>,
}

/// One field of a reflected record.
#[derive(Clone)]
struct FieldReflection {
    name: String,
    ty: String,
    pointer: bool,
}

/// One named record the field builtins answer for.
///
/// Only structs are here: a union has no single active member, so a derived
/// Debug would print storage that may not be the value the program wrote.
/// A record whose fields cannot be spelled, or whose type is generic, is left
/// out entirely; a query for it reaches the probe's fatal path instead of
/// answering with an empty or partial field list.
struct RecordReflection {
    owner: String,
    fields: Vec<FieldReflection>,
}

/// Which compile-time builtins the probed bodies call, and therefore which
/// ordinary definitions the generated source supplies.
#[derive(Default)]
struct BuiltinNeeds {
    compile_error: bool,
    tokens_len: bool,
    token_text: bool,
    tokens_from_text: bool,
    tokens_from_int: bool,
    tokens_slice: bool,
    tokens_concat: bool,
    tokens_text: bool,
    declared: bool,
    implements: bool,
    require_impl: bool,
    try_provide_impl: bool,
    field_count: bool,
    field_name: bool,
    field_type: bool,
    field_is_pointer: bool,
}

impl BuiltinNeeds {
    fn any(&self) -> bool {
        self.compile_error
            || self.tokens_len
            || self.token_text
            || self.tokens_from_text
            || self.tokens_from_int
            || self.tokens_slice
            || self.tokens_concat
            || self.tokens_text
            || self.declared
            || self.implements
            || self.require_impl
            || self.try_provide_impl
            || self.reflects_fields()
    }

    /// Whether a body asks for record fields.
    fn reflects_fields(&self) -> bool {
        self.field_count || self.field_name || self.field_type || self.field_is_pointer
    }

    /// Whether a token builder the arena serves is called.
    fn builds_tokens(&self) -> bool {
        self.tokens_from_text
            || self.tokens_from_int
            || self.tokens_slice
            || self.tokens_concat
            || self.tokens_text
    }

    /// Whether a call takes storage from the arena pools.
    fn stores_tokens(&self) -> bool {
        self.tokens_from_text || self.tokens_from_int || self.tokens_concat || self.tokens_text
    }
}

/// The symbol of the generated wrapper for one request.
fn wrapper_symbol(id: usize) -> String {
    format!("{WRAPPER}{id}")
}

/// The ordinary definitions a round needs next to the cloned functions.
///
/// A token argument becomes static arrays for its spellings, kinds, lengths,
/// and spans, plus one wrapper that builds a `Tokens` value from them and
/// calls the clone. The wrapper is ordinary code, so the aggregate travels
/// through the compiler's own `CallPlan` and the trampoline only ever places
/// scalars. A Tokens result becomes the static storage the hidden return
/// pointer names and the serializer that flattens it. The accessors and the
/// declaration-world queries this slice implements are ordinary functions for
/// the same reason. Field reflection is the same: static strings plus four
/// ordinary lookups, so no part of the derived Debug is generated here.
fn generated_items(
    baked: &[BakedTokens],
    baked_text: &[(String, String)],
    needs: &BuiltinNeeds,
    tokens_result: bool,
    queue_context: bool,
    unresolved_bodies: bool,
    try_runs: &[&TrySite],
    world: &WorldSnapshot,
    records: &[RecordReflection],
    site: Span,
    layout: Layout,
) -> Result<Vec<ast::Item>, String> {
    if baked.is_empty()
        && baked_text.is_empty()
        && !needs.any()
        && !tokens_result
        && !queue_context
        && !unresolved_bodies
        && try_runs.is_empty()
    {
        return Ok(Vec::new());
    }
    let mut source = String::new();
    if needs.stores_tokens() || needs.reflects_fields() || unresolved_bodies {
        source.push_str(PROBE_FAILURE_HELPERS);
    }
    if unresolved_bodies {
        source.push_str("static void __shy_probe_unresolved_body(const char *name) { __shy_probe_fatal(8, (int)name, 0, 0); }\n");
    }
    if queue_context || needs.require_impl || needs.try_provide_impl {
        source.push_str(QUEUE_CONTEXT_GLOBALS);
    }
    for tokens in baked {
        bake_tokens(&mut source, tokens);
    }
    for (symbol, text) in baked_text {
        // `pub` keeps the linker-visible name: a C-style file-scope object is
        // file-local in ShyC, and the trampoline spells this symbol.
        let bytes = text.len() + 1;
        source.push_str(&format!(
            "pub char {symbol}[{bytes}] = {};\n",
            c_string(text)
        ));
    }
    if needs.builds_tokens() {
        // The arena pools come first: the builders below take their storage
        // from them, including the copy tokens_from_text materializes, and
        // every function here is declared before it is called.
        if needs.stores_tokens() {
            source.push_str(&token_arena());
        }
        source.push_str(TOKEN_HELPERS);
    }
    if needs.tokens_len {
        source.push_str("int tokens_len(struct Tokens tokens) { return tokens.count; }\n");
    }
    if needs.token_text {
        source.push_str(
            "const char *token_text(struct Tokens tokens, int index) {\n    if (index < 0 || index >= tokens.count) { return 0; }\n    return tokens.texts[index];\n}\n",
        );
    }
    if needs.tokens_from_text {
        source.push_str(&tokens_from_text(layout));
    }
    if needs.compile_error {
        source.push_str(
            r#"
void compile_error(const char *message) {
    int request[5] = {11, (int)message, 0, 0, 0};
    int reply[1];
    int *queue = request;
    int *answer = reply;
    asm!(queue, answer) {
        "seta 9x 4x\n"
        "seta ax 5x\n"
        "setn 0x 999\n"
        "setn 1x 3\n"
        "setn 2x 11\n"
        "seta 3x 9x\n"
        "setn 4x 20\n"
        "seta 5x ax\n"
        "setn 6x 4\n"
        "syscall\n"
        "seta 4x 9x\n"
        "seta 5x ax\n"
    };
}
"#,
        );
    }
    if needs.builds_tokens() {
        if needs.tokens_from_int {
            source.push_str(TOKENS_FROM_INT);
        }
        if needs.tokens_concat {
            source.push_str(TOKENS_CONCAT);
        }
        if needs.tokens_slice {
            source.push_str(TOKENS_SLICE);
        }
        if needs.tokens_text {
            source.push_str(TOKENS_TEXT);
        }
    }
    if needs.declared || needs.implements || needs.require_impl || needs.try_provide_impl {
        bake_world(&mut source, world, needs)?;
    }
    if needs.reflects_fields() {
        bake_field_reflection(&mut source, records);
    }
    if tokens_result {
        serialize_tokens(&mut source);
    }
    if !try_runs.is_empty() {
        try_run_helpers(&mut source, try_runs);
    }
    parse_generated(source, site)
}

/// Appends the helper one `try_run` call stands for.
///
/// The parser rewrites `try_run(f, a, b, &out)` into `__shy_try_<id>(a, b, &out)`.
/// That helper packs the arguments where the thunk reads them and asks the host
/// to run the thunk in a child emulator. The child commits its memory writes
/// only when it reaches the EXIT below, so a failed attempt leaves the parent
/// untouched and the helper answers 0.
fn try_run_helpers(source: &mut String, sites: &[&TrySite]) {
    for site in sites {
        let id = site.id;
        let function = &site.function;
        let parameters: Vec<String> = (0..site.arity).map(|at| format!("int a{at}")).collect();
        let arguments: Vec<String> = (0..site.arity)
            .map(|at| format!("__shy_try_args_{id}[{at}]"))
            .collect();
        let stores: Vec<String> = (0..site.arity)
            .map(|at| format!("    __shy_try_args_{id}[{at}] = a{at};\n"))
            .collect();
        let header = if parameters.is_empty() {
            "int *out".to_string()
        } else {
            format!("{}, int *out", parameters.join(", "))
        };
        source.push_str(&format!(
            "\
static int __shy_try_args_{id}[{arity}];\n\
static int __shy_try_value_{id};\n\
int __shy_try_{id}({header});\n\
int __shy_try_enter_{id}(void) {{\n    __shy_try_value_{id} = {function}({call_args});\n\
    asm!() {{\n        \"setn 0x 999\\n\"\
        \"setn 1x 1\\n\"\
        \"setn 2x 0\\n\"\
        \"syscall\\n\"\
    }};\n\
    return 0;\n\
}}\n\
int __shy_try_{id}({header}) {{\n    static int __shy_try_request_{id}[5];\n\
    static int __shy_try_reply_{id}[1];\n\
{stores}    __shy_try_request_{id}[0] = 4;\n\
    __shy_try_request_{id}[1] = (int)__shy_try_enter_{id};\n\
    __shy_try_request_{id}[2] = 0;\n\
    __shy_try_request_{id}[3] = 0;\n\
    __shy_try_request_{id}[4] = 0;\n\
    int *queue = __shy_try_request_{id};\n\
    int *reply = __shy_try_reply_{id};\n\
    asm!(queue, reply) {{\n        \"seta 9x 4x\\n\"\
        \"seta ax 5x\\n\"\
        \"setn 0x 999\\n\"\
        \"setn 1x 3\\n\"\
        \"setn 2x 4\\n\"\
        \"seta 3x 9x\\n\"\
        \"setn 4x 20\\n\"\
        \"seta 5x ax\\n\"\
        \"setn 6x 4\\n\"\
        \"syscall\\n\"\
        \"seta 4x 9x\\n\"\
        \"seta 5x ax\\n\"\
    }};\n\
    if (__shy_try_reply_{id}[0] == 0) {{ return 0; }}\n\
    *out = __shy_try_value_{id};\n\
    return 1;\n\
}}\n",
            arity = site.arity,
            call_args = arguments.join(", "),
            stores = stores.join(""),
        ));
    }
}

/// Capacities in elements: 65536 integers, 8192 text pointers, and 65536 bytes.
const POOL_INTS: usize = 65536;
const POOL_TEXTS: usize = 8192;
const POOL_CHARS: usize = 65536;

/// Fatal probe failures use the host mailbox, so the whole run is rejected
/// before a partial or placeholder-derived answer can be accepted.
const PROBE_FAILURE_HELPERS: &str = r#"
static int __shy_probe_fatal(int op, int first, int second, int third) {
    static int request[5];
    static int response[1];
    int *queue = request;
    int *reply = response;
    request[0] = op;
    request[1] = first;
    request[2] = second;
    request[3] = third;
    request[4] = 0;
    asm!(queue, reply) {
        "seta 9x 4x\n"
        "seta ax 5x\n"
        "setn 0x 999\n"
        "setn 1x 3\n"
        "setn 2x 5\n"
        "seta 3x 9x\n"
        "setn 4x 20\n"
        "seta 5x ax\n"
        "setn 6x 4\n"
        "syscall\n"
        "seta 4x 9x\n"
        "seta 5x ax\n"
    };
    return 0;
}
"#;

/// The static storage probe-time token building draws from, plus its three
/// bump allocators.
///
/// The generated source never runs through a preprocessor, so it has no
/// `malloc` prototype and no `size_t` it could trust; a fixed pool with a
/// cursor avoids both. A probe run answers one Tokens request and then ends,
/// so nothing is ever freed. Each pool counts elements, which keeps
/// every array naturally aligned (4-byte `int` and pointer) without any
/// alignment arithmetic.
///
/// Exhaustion reports a fatal host request. The driver rejects the whole
/// probe run, so allocation failure cannot be mistaken for an empty expansion.
fn token_arena() -> String {
    format!(
        r#"static int __shy_probe_int_pool[{POOL_INTS}];
static int __shy_probe_ints_used;
static char *__shy_probe_text_pool[{POOL_TEXTS}];
static int __shy_probe_texts_used;
static char __shy_probe_char_pool[{POOL_CHARS}];
static int __shy_probe_chars_used;

static int *__shy_probe_take_ints(int count) {{
    int *out;
    if (count < 0 || count > {POOL_INTS} - __shy_probe_ints_used) {{
        __shy_probe_fatal(5, 0, count, {POOL_INTS} - __shy_probe_ints_used);
        return 0;
    }}
    out = __shy_probe_int_pool + __shy_probe_ints_used;
    __shy_probe_ints_used = __shy_probe_ints_used + count;
    return out;
}}

static char **__shy_probe_take_texts(int count) {{
    char **out;
    if (count < 0 || count > {POOL_TEXTS} - __shy_probe_texts_used) {{
        __shy_probe_fatal(5, 1, count, {POOL_TEXTS} - __shy_probe_texts_used);
        return 0;
    }}
    out = __shy_probe_text_pool + __shy_probe_texts_used;
    __shy_probe_texts_used = __shy_probe_texts_used + count;
    return out;
}}

static char *__shy_probe_take_chars(int count) {{
    char *out;
    int start;
    if (count < 0) {{
        __shy_probe_fatal(5, 2, count, {POOL_CHARS} - __shy_probe_chars_used);
        return 0;
    }}
    // A byte block still starts on a word boundary: tokens_from_text lays a
    // struct Tokens over the block it asks for, and a 4-byte load from an odd
    // address traps on ShyISA.
    start = ((__shy_probe_chars_used + 3) / 4) * 4;
    if (count > {POOL_CHARS} - start) {{
        __shy_probe_fatal(5, 2, count, {POOL_CHARS} - start);
        return 0;
    }}
    out = __shy_probe_char_pool + start;
    __shy_probe_chars_used = start + count;
    return out;
}}
"#
    )
}

/// The one constructor and the one empty value every builder below uses.
const TOKEN_HELPERS: &str = r#"static struct Tokens __shy_probe_tokens(int count, int *kinds, char **texts, int *lens, int *spans) {
    struct Tokens out;
    out.count = count;
    out.kinds = kinds;
    out.texts = texts;
    out.lens = lens;
    out.spans = spans;
    return out;
}

static struct Tokens __shy_probe_no_tokens(void) {
    return __shy_probe_tokens(0, 0, 0, 0, 0);
}
"#;

/// tokens_from_text: the host lexes the text and the guest materializes it.
///
/// The host has nowhere to write but one fixed scratch buffer, so a second
/// call would overwrite the first result's arrays. The guest therefore copies
/// the image into the arena before returning: every result owns its storage
/// and outlives the call that made it. The caller still has to keep the text
/// alive only for the call itself, because the host reads it during the
/// request.
///
/// The image is a Tokens value followed by its arrays and spellings, so
/// copying the used prefix and shifting every pointer by the same delta makes
/// a self-contained copy. If the image does not fit, the host rejects the
/// probe run rather than accepting an empty expansion.
///
/// The image is the one the driver writes, so its sizes follow `layout`:
/// `tokens_bytes` is the value itself and `entry_bytes` the four array words
/// one token takes.
fn tokens_from_text(layout: Layout) -> String {
    TOKENS_FROM_TEXT
        .replace("{tokens_bytes}", &tokens_bytes(layout).to_string())
        .replace("{entry_bytes}", &(16 + layout.pointer_bytes).to_string())
}

/// The generated source; the two sizes are filled in for the layout.
const TOKENS_FROM_TEXT: &str = r#"static int __shy_probe_text_len(const char *text) {
    int length = 0;
    while (text[length] != 0) { length = length + 1; }
    return length;
}

struct Tokens tokens_from_text(const char *text) {
    static char __shy_probe_scratch[65536];
    static int __shy_probe_request[5];
    static int __shy_probe_reply[1];
    struct Tokens staging;
    struct Tokens out;
    int *queue;
    int *reply;
    int size;
    int index;
    int delta;
    char *copy;
    __shy_probe_request[0] = 3;
    __shy_probe_request[1] = (int)text;
    __shy_probe_request[2] = __shy_probe_text_len(text);
    __shy_probe_request[3] = (int)__shy_probe_scratch;
    __shy_probe_request[4] = 65536;
    queue = __shy_probe_request;
    reply = __shy_probe_reply;
    asm!(queue, reply) {
        "seta 9x 4x\n"
        "seta ax 5x\n"
        "setn 0x 999\n"
        "setn 1x 3\n"
        "setn 2x 3\n"
        "seta 3x 9x\n"
        "setn 4x 20\n"
        "seta 5x ax\n"
        "setn 6x 4\n"
        "syscall\n"
        "seta 4x 9x\n"
        "seta 5x ax\n"
    };
    staging = *(struct Tokens *)__shy_probe_scratch;
    size = {tokens_bytes} + staging.count * {entry_bytes};
    index = 0;
    while (index < staging.count) {
        size = size + staging.lens[index] + 1;
        index = index + 1;
    }
    copy = __shy_probe_take_chars(size);
    if (copy == 0) { return __shy_probe_no_tokens(); }
    index = 0;
    while (index < size) {
        copy[index] = __shy_probe_scratch[index];
        index = index + 1;
    }
    delta = (int)copy - (int)__shy_probe_scratch;
    out = *(struct Tokens *)copy;
    out.kinds = (int *)((int)out.kinds + delta);
    out.texts = (char **)((int)out.texts + delta);
    out.lens = (int *)((int)out.lens + delta);
    out.spans = (int *)((int)out.spans + delta);
    index = 0;
    while (index < out.count) {
        out.texts[index] = (char *)((int)out.texts[index] + delta);
        index = index + 1;
    }
    return out;
}
"#;

/// tokens_from_int: one number token spelled in decimal.
///
/// The magnitude is taken in unsigned int so the most negative int still
/// spells every digit; the spelling, the four arrays, and the two span words
/// come from the arena, so two calls in one expression never share storage.
/// The kind is TOKEN_NUMBER and the spans are 0, 0, the same as a
/// materialized tokens_from_text result.
const TOKENS_FROM_INT: &str = r#"struct Tokens tokens_from_int(int value) {
    unsigned int magnitude;
    unsigned int rest;
    int negative;
    int length;
    int at;
    char *spelling;
    int *kinds;
    char **texts;
    int *lens;
    int *spans;
    negative = value < 0;
    magnitude = negative ? 0u - (unsigned int)value : (unsigned int)value;
    rest = magnitude;
    length = 1;
    while (rest >= 10u) {
        rest = rest / 10u;
        length = length + 1;
    }
    if (negative) { length = length + 1; }
    spelling = __shy_probe_take_chars(length + 1);
    kinds = __shy_probe_take_ints(1);
    texts = __shy_probe_take_texts(1);
    lens = __shy_probe_take_ints(1);
    spans = __shy_probe_take_ints(2);
    if (spelling == 0 || kinds == 0 || texts == 0 || lens == 0 || spans == 0) {
        return __shy_probe_no_tokens();
    }
    at = length - 1;
    while (at >= (negative ? 1 : 0)) {
        spelling[at] = (char)('0' + (int)(magnitude % 10u));
        magnitude = magnitude / 10u;
        at = at - 1;
    }
    if (negative) { spelling[0] = '-'; }
    spelling[length] = 0;
    kinds[0] = 1;
    texts[0] = spelling;
    lens[0] = length;
    spans[0] = 0;
    spans[1] = 0;
    return __shy_probe_tokens(1, kinds, texts, lens, spans);
}
"#;

/// tokens_concat: one new sequence holding both inputs' tokens.
///
/// The arrays are copies from the arena, so the result outlives either input
/// and a token built by tokens_from_int keeps its spelling. Spans stay 0, 0
/// per token.
const TOKENS_CONCAT: &str = r#"struct Tokens tokens_concat(struct Tokens left, struct Tokens right) {
    int count;
    int index;
    int from;
    int *kinds;
    char **texts;
    int *lens;
    int *spans;
    count = left.count + right.count;
    kinds = __shy_probe_take_ints(count);
    texts = __shy_probe_take_texts(count);
    lens = __shy_probe_take_ints(count);
    spans = __shy_probe_take_ints(count * 2);
    if (kinds == 0 || texts == 0 || lens == 0 || spans == 0) {
        return __shy_probe_no_tokens();
    }
    index = 0;
    while (index < left.count) {
        kinds[index] = left.kinds[index];
        texts[index] = left.texts[index];
        lens[index] = left.lens[index];
        spans[index * 2] = left.spans[index * 2];
        spans[index * 2 + 1] = left.spans[index * 2 + 1];
        index = index + 1;
    }
    while (index < count) {
        from = index - left.count;
        kinds[index] = right.kinds[from];
        texts[index] = right.texts[from];
        lens[index] = right.lens[from];
        spans[index * 2] = right.spans[from * 2];
        spans[index * 2 + 1] = right.spans[from * 2 + 1];
        index = index + 1;
    }
    return __shy_probe_tokens(count, kinds, texts, lens, spans);
}
"#;

/// tokens_slice: a view of len tokens from start.
///
/// Both bounds are clamped to the sequence, so no view reaches outside it. The
/// pointers shift by element size through ordinary pointer arithmetic and
/// nothing is copied: the view reads the same spellings as its input.
const TOKENS_SLICE: &str = r#"struct Tokens tokens_slice(struct Tokens tokens, int start, int len) {
    if (start < 0) { start = 0; }
    if (start > tokens.count) { start = tokens.count; }
    if (len < 0) { len = 0; }
    if (len > tokens.count - start) { len = tokens.count - start; }
    return __shy_probe_tokens(len, tokens.kinds + start, tokens.texts + start, tokens.lens + start, tokens.spans + start * 2);
}
"#;

/// tokens_text: every spelling in order, NUL-terminated.
///
/// The bytes are copied into one arena block, so the answer stays valid after
/// the input arrays change. An empty sequence returns a pointer to an empty
/// string; a block that does not fit returns null.
const TOKENS_TEXT: &str = r#"const char *tokens_text(struct Tokens tokens) {
    int size;
    int index;
    int offset;
    int at;
    char *text;
    size = 1;
    index = 0;
    while (index < tokens.count) {
        size = size + tokens.lens[index];
        index = index + 1;
    }
    text = __shy_probe_take_chars(size);
    if (text == 0) { return 0; }
    at = 0;
    index = 0;
    while (index < tokens.count) {
        offset = 0;
        while (offset < tokens.lens[index]) {
            text[at] = tokens.texts[index][offset];
            at = at + 1;
            offset = offset + 1;
        }
        index = index + 1;
    }
    text[at] = 0;
    return text;
}
"#;

/// Carries the round's world to the host matcher. All query arguments, whether
/// literal or computed in guest code, are interpreted by the same Rust code.
fn bake_world(
    source: &mut String,
    world: &WorldSnapshot,
    needs: &BuiltinNeeds,
) -> Result<(), String> {
    let bytes = encode_world(world)?;
    let data = bytes
        .iter()
        .map(u8::to_string)
        .collect::<Vec<_>>()
        .join(",");
    source.push_str(&format!(
        "static unsigned char __shy_probe_world[{}] = {{ {data} }};\nstatic int __shy_probe_world_bytes = {};\n",
        bytes.len(), bytes.len(),
    ));
    source.push_str(WORLD_QUERY_BODY);
    if needs.declared {
        source.push_str(
            "int declared(const char *name) { return __shy_probe_world_query(7, name, 0); }\n",
        );
    }
    if needs.implements || needs.require_impl || needs.try_provide_impl {
        source.push_str("static int __shy_probe_has_impl(const char *owner, const char *trait_name) { return __shy_probe_world_query(6, owner, trait_name); }\n");
    }
    if needs.implements {
        source.push_str("int implements(const char *owner, const char *trait_name) { return __shy_probe_has_impl(owner, trait_name); }\n");
    }
    if needs.require_impl {
        source.push_str(REQUIRE_IMPL_BODY);
    }
    if needs.try_provide_impl {
        source.push_str(TRY_PROVIDE_IMPL_BODY);
    }
    Ok(())
}

/// Collects the struct definitions field reflection can answer for.
///
/// A record with a field shape that cannot be spelled at all is left out of
/// the table rather than failing the round: a program may define such a record
/// without deriving anything. Querying it later reaches the probe's fatal
/// path, so the absence never turns into an empty derived Debug.
fn reflect_records(items: &[ast::Item]) -> Vec<RecordReflection> {
    let mut aliases: HashMap<String, (ast::DeclSpecs, ast::Declarator)> = HashMap::new();
    for item in items {
        let ast::Item::Decl(declaration) = item else {
            continue;
        };
        if declaration.specs.storage != Some(ast::StorageClass::Typedef) {
            continue;
        }
        for declarator in &declaration.declarators {
            if let Some(name) = &declarator.declarator.name {
                aliases.insert(
                    name.text.clone(),
                    (declaration.specs.clone(), declarator.declarator.clone()),
                );
            }
        }
    }
    let mut records = Vec::new();
    for item in items {
        let ast::Item::Decl(declaration) = item else {
            continue;
        };
        let mut aliases_here = Vec::new();
        if declaration.specs.storage == Some(ast::StorageClass::Typedef) {
            for declarator in &declaration.declarators {
                let plain = declarator.declarator.derived.is_empty();
                if plain && let Some(name) = &declarator.declarator.name {
                    aliases_here.push(name.text.clone());
                }
            }
        }
        for specifier in &declaration.specs.type_specs {
            if let ast::TypeSpecifier::Record(record) = specifier {
                reflect_record(record, &aliases_here, &aliases, &mut records);
            }
        }
    }
    records
}

/// Adds one record definition, its nested definitions, and every name it is
/// visible under.
fn reflect_record(
    record: &ast::RecordSpec,
    typedef_names: &[String],
    aliases: &HashMap<String, (ast::DeclSpecs, ast::Declarator)>,
    records: &mut Vec<RecordReflection>,
) {
    let Some(members) = &record.members else {
        return;
    };
    for member in members {
        for specifier in &member.specs.type_specs {
            if let ast::TypeSpecifier::Record(nested) = specifier {
                reflect_record(nested, &[], aliases, records);
            }
        }
    }
    if record.kind != ast::RecordKind::Struct || !record.generics.is_empty() {
        return;
    }
    let mut owners = Vec::new();
    if let Some(tag) = &record.tag {
        owners.push(tag.text.clone());
    }
    for name in typedef_names {
        if !owners.contains(name) {
            owners.push(name.clone());
        }
    }
    if owners.is_empty() {
        return;
    }
    let Some(fields) = reflect_fields(members, aliases) else {
        return;
    };
    for owner in owners {
        if records.iter().any(|known| known.owner == owner) {
            continue;
        }
        records.push(RecordReflection {
            owner,
            fields: fields.clone(),
        });
    }
}

/// The fields of one struct, or `None` when a field has a shape reflection
/// cannot spell.
fn reflect_fields(
    members: &[ast::MemberDecl],
    aliases: &HashMap<String, (ast::DeclSpecs, ast::Declarator)>,
) -> Option<Vec<FieldReflection>> {
    let mut fields = Vec::new();
    for member in members {
        // An anonymous struct/union member has no field name of its own.
        if member.declarators.is_empty() {
            return None;
        }
        if member
            .specs
            .type_specs
            .iter()
            .any(|specifier| matches!(specifier, ast::TypeSpecifier::Record(record) if record.members.is_some()))
        {
            return None;
        }
        for declarator in &member.declarators {
            if declarator.bit_width.is_some() {
                return None;
            }
            if declarator
                .declarator
                .derived
                .iter()
                .any(|derived| !matches!(derived, ast::DerivedDeclarator::Pointer(_)))
            {
                return None;
            }
            let name = declarator.declarator.name.as_ref()?.text.clone();
            let pointer = pointer_field(&member.specs, &declarator.declarator, aliases)?;
            fields.push(FieldReflection {
                name,
                ty: field_type_text(&member.specs, &declarator.declarator, pointer),
                pointer,
            });
        }
    }
    Some(fields)
}

/// Whether a field is a pointer, resolving typedef chains.
///
/// `None` means the declared type is an array or function through an alias, so
/// the field cannot be reflected as a value.
fn pointer_field(
    specs: &ast::DeclSpecs,
    declarator: &ast::Declarator,
    aliases: &HashMap<String, (ast::DeclSpecs, ast::Declarator)>,
) -> Option<bool> {
    let mut seen = Vec::new();
    pointer_shape(specs, declarator, aliases, &mut seen)
}

fn pointer_shape(
    specs: &ast::DeclSpecs,
    declarator: &ast::Declarator,
    aliases: &HashMap<String, (ast::DeclSpecs, ast::Declarator)>,
    seen: &mut Vec<String>,
) -> Option<bool> {
    for derived in &declarator.derived {
        match derived {
            ast::DerivedDeclarator::Pointer(_) => return Some(true),
            ast::DerivedDeclarator::Array(_) | ast::DerivedDeclarator::Function(_) => return None,
        }
    }
    for specifier in &specs.type_specs {
        let ast::TypeSpecifier::TypedefName(name) = specifier else {
            continue;
        };
        if seen.contains(&name.text) {
            return Some(false);
        }
        let Some((alias_specs, alias_declarator)) = aliases.get(&name.text) else {
            return Some(false);
        };
        seen.push(name.text.clone());
        return pointer_shape(alias_specs, alias_declarator, aliases, seen);
    }
    Some(false)
}

/// The field's type as the world matcher spells it.
///
/// A non-pointer field drops its value qualifiers: `const int` and `int` have
/// one Debug impl between them, and the generated call takes the field's
/// address as `const int *` anyway. A pointer field keeps its pointee
/// qualifiers, because `const char *` and `char *` are different Debug impls,
/// and drops only the qualifier on the outermost pointer itself, which does not
/// change the pointed-to type. A tag used with its `struct`, `union`, or `enum`
/// keyword is written as the bare name, which is the spelling the impl world
/// stores. The result is then canonicalized, so `char *` becomes `char*`.
fn field_type_text(specs: &ast::DeclSpecs, declarator: &ast::Declarator, pointer: bool) -> String {
    let mut specs = specs.clone();
    if !pointer {
        specs.qualifiers.clear();
    }
    for specifier in &mut specs.type_specs {
        match specifier {
            ast::TypeSpecifier::Record(record) => {
                if let Some(tag) = &record.tag {
                    *specifier = ast::TypeSpecifier::TypedefName(tag.clone());
                }
            }
            ast::TypeSpecifier::Enum(enumeration) => {
                if let Some(tag) = &enumeration.tag {
                    *specifier = ast::TypeSpecifier::TypedefName(tag.clone());
                }
            }
            _ => {}
        }
    }
    let mut declarator = declarator.clone();
    declarator.name = None;
    if pointer
        && let Some(ast::DerivedDeclarator::Pointer(outermost)) = declarator.derived.first_mut()
    {
        outermost.qualifiers.clear();
    }
    let type_name = ast::TypeName {
        span: specs.span,
        specs,
        declarator: Some(declarator),
    };
    canonical_type_name(&crate::c::types::type_name_text(&type_name))
}

/// Bakes the owner and field tables the four reflection builtins read.
///
/// Every table entry is a static string or int, so a const fn's call needs no
/// host round. A missing owner is fatal rather than an empty answer: a derived
/// Debug that formatted nothing would look correct at run time.
fn bake_field_reflection(source: &mut String, records: &[RecordReflection]) {
    let owner_count = records.len();
    let field_count: usize = records.iter().map(|record| record.fields.len()).sum();
    let mut starts = Vec::with_capacity(owner_count + 1);
    let mut owners = Vec::new();
    let mut names = Vec::new();
    let mut types = Vec::new();
    let mut pointers = Vec::new();
    starts.push(0usize);
    for record in records {
        owners.push(record.owner.clone());
        for field in &record.fields {
            names.push(field.name.clone());
            types.push(field.ty.clone());
            pointers.push(if field.pointer { "1" } else { "0" });
        }
        starts.push(names.len());
    }
    let owners = list_or_zero(&owners.iter().map(|name| c_string(name)).collect::<Vec<_>>());
    let starts = list_or_zero(&starts.iter().map(usize::to_string).collect::<Vec<_>>());
    let names = list_or_zero(&names.iter().map(|name| c_string(name)).collect::<Vec<_>>());
    let types = list_or_zero(&types.iter().map(|ty| c_string(ty)).collect::<Vec<_>>());
    let pointers = list_or_zero(
        &pointers
            .iter()
            .map(|value| value.to_string())
            .collect::<Vec<_>>(),
    );
    source.push_str(&format!(
        "\
static const char *__shy_probe_field_owners[{}] = {{ {owners} }};
static int __shy_probe_field_starts[{}] = {{ {starts} }};
static const char *__shy_probe_field_names[{}] = {{ {names} }};
static const char *__shy_probe_field_types[{}] = {{ {types} }};
static int __shy_probe_field_pointers[{}] = {{ {pointers} }};

static int __shy_probe_streq(const char *left, const char *right) {{
    int i = 0;
    while (left[i] != 0 || right[i] != 0) {{
        if (left[i] != right[i]) {{ return 0; }}
        i = i + 1;
    }}
    return 1;
}}

static int __shy_probe_field_owner(const char *owner) {{
    int i = 0;
    while (i < {owner_count}) {{
        if (__shy_probe_streq(owner, __shy_probe_field_owners[i])) {{ return i; }}
        i = i + 1;
    }}
    __shy_probe_fatal(10, (int)owner, 0, 0);
    return -1;
}}

int field_count(const char *owner) {{
    int at = __shy_probe_field_owner(owner);
    if (at < 0) {{ return -1; }}
    return __shy_probe_field_starts[at + 1] - __shy_probe_field_starts[at];
}}

const char *field_name(const char *owner, int index) {{
    int at = __shy_probe_field_owner(owner);
    if (at < 0) {{ return 0; }}
    if (index < 0 || index >= __shy_probe_field_starts[at + 1] - __shy_probe_field_starts[at]) {{ return 0; }}
    return __shy_probe_field_names[__shy_probe_field_starts[at] + index];
}}

const char *field_type(const char *owner, int index) {{
    int at = __shy_probe_field_owner(owner);
    if (at < 0) {{ return 0; }}
    if (index < 0 || index >= __shy_probe_field_starts[at + 1] - __shy_probe_field_starts[at]) {{ return 0; }}
    return __shy_probe_field_types[__shy_probe_field_starts[at] + index];
}}

int field_is_pointer(const char *owner, int index) {{
    int at = __shy_probe_field_owner(owner);
    if (at < 0) {{ return -1; }}
    if (index < 0 || index >= __shy_probe_field_starts[at + 1] - __shy_probe_field_starts[at]) {{ return -1; }}
    return __shy_probe_field_pointers[__shy_probe_field_starts[at] + index];
}}
",
        owner_count.max(1),
        owner_count + 1,
        field_count.max(1),
        field_count.max(1),
        field_count.max(1),
    ));
}

/// The two host queries use the existing five-word mailbox. Ops 6 and 7 carry
/// owner/trait pointers and the serialized round world, followed by its length.
const WORLD_QUERY_BODY: &str = r#"
static int __shy_probe_world_query(int op, const char *owner, const char *trait_name) {
    static int request[5];
    static int response[1];
    int *queue = request;
    int *reply = response;
    request[0] = op;
    request[1] = (int)owner;
    request[2] = (int)trait_name;
    request[3] = (int)__shy_probe_world;
    request[4] = __shy_probe_world_bytes;
    asm!(queue, reply) {
        "seta 9x 4x\n"
        "seta ax 5x\n"
        "setn 0x 999\n"
        "setn 1x 3\n"
        "setn 2x 6\n"
        "seta 3x 9x\n"
        "setn 4x 20\n"
        "seta 5x ax\n"
        "setn 6x 4\n"
        "syscall\n"
        "seta 4x 9x\n"
        "seta 5x ax\n"
    };
    return response[0];
}
"#;

/// The body of `require_impl`: generators suspend instead of caching an
/// empty answer when a computed query is not satisfied yet.
const REQUIRE_IMPL_BODY: &str = r#"
pub int __shy_probe_suspend_id;
pub int __shy_probe_suspend_kind;
pub const char *__shy_probe_suspend_owner;
pub const char *__shy_probe_suspend_trait;

static void __shy_probe_suspend_now(void) {
    asm!() {
        "getn 2x __shy_probe_suspend_id\n"
        "getn 3x __shy_probe_suspend_kind\n"
        "getn 4x __shy_probe_suspend_owner\n"
        "getn 5x __shy_probe_suspend_trait\n"
        "setn 0x 999\n"
        "setn 1x 5\n"
        "syscall\n"
    };
}

int require_impl(const char *owner, const char *trait_name) {
    if (__shy_probe_has_impl(owner, trait_name)) { return 1; }
    if (__shy_probe_can_suspend) {
        __shy_probe_suspend_id = __shy_probe_current;
        __shy_probe_suspend_kind = 1;
        __shy_probe_suspend_owner = owner;
        __shy_probe_suspend_trait = trait_name;
        __shy_probe_suspend_now();
    }
    return 0;
}
"#;

/// The body of `try_provide_impl`: 1 only while nobody provides the impl.
///
/// An offer that reaches 1 is reported to the host, which records the pair.
/// Generators of one round run against the same world, so two of them can both
/// offer the same impl; the driver keeps the earliest offer and leaves the
/// later ones unanswered, and they find the impl on the next round.
const TRY_PROVIDE_IMPL_BODY: &str = r#"
static int __shy_probe_provide(const char *owner, const char *trait_name) {
    static int request[5];
    static int response[1];
    int *queue = request;
    int *reply = response;
    request[0] = 9;
    request[1] = (int)owner;
    request[2] = (int)trait_name;
    request[3] = 0;
    request[4] = 0;
    asm!(queue, reply) {
        "seta 9x 4x\n"
        "seta ax 5x\n"
        "setn 0x 999\n"
        "setn 1x 3\n"
        "setn 2x 9\n"
        "seta 3x 9x\n"
        "setn 4x 20\n"
        "seta 5x ax\n"
        "setn 6x 4\n"
        "syscall\n"
        "seta 4x 9x\n"
        "seta 5x ax\n"
    };
    return response[0];
}

int try_provide_impl(const char *owner, const char *trait_name) {
    if (!__shy_probe_can_suspend) { return 0; }
    if (__shy_probe_has_impl(owner, trait_name)) { return 0; }
    return __shy_probe_provide(owner, trait_name);
}
"#;

/// Appends the static storage and the serializer a Tokens result needs.
///
/// The two objects are `pub` so they keep exactly the names the trampoline
/// spells: in ShyC a C-style file-scope object is file-local by default.
///
/// The blob is `[count]` followed by `[kind][len][bytes...]` per token, every
/// number a big-endian `u32`, which is what the driver decodes. The serializer
/// is ordinary code, so the format lives in one readable place instead of in
/// hand-written assembly. A result wider than the blob returns one byte more
/// than the blob holds, which the emulator rejects as a bad mailbox: an
/// oversized answer must fail, never be truncated into a wrong one.
fn serialize_tokens(source: &mut String) {
    source.push_str(&format!(
        "\
pub struct Tokens {OUT};
pub unsigned char {BLOB}[{BLOB_BYTES}];

void __shy_probe_put32(int at, unsigned int value) {{
    {BLOB}[at] = (unsigned char)(value >> 24);
    {BLOB}[at + 1] = (unsigned char)(value >> 16);
    {BLOB}[at + 2] = (unsigned char)(value >> 8);
    {BLOB}[at + 3] = (unsigned char)value;
}}

int {SERIALIZER}(void) {{
    struct Tokens tokens = {OUT};
    int size = 4;
    int at = 0;
    int i = 0;
    int j = 0;
    while (i < tokens.count) {{
        size = size + 8 + tokens.lens[i];
        i = i + 1;
    }}
    if (size > {BLOB_BYTES}) {{
        return {overflow};
    }}
    __shy_probe_put32(0, (unsigned int)tokens.count);
    at = 4;
    i = 0;
    while (i < tokens.count) {{
        __shy_probe_put32(at, (unsigned int)tokens.kinds[i]);
        at = at + 4;
        __shy_probe_put32(at, (unsigned int)tokens.lens[i]);
        at = at + 4;
        j = 0;
        while (j < tokens.lens[i]) {{
            {BLOB}[at] = (unsigned char)tokens.texts[i][j];
            at = at + 1;
            j = j + 1;
        }}
        i = i + 1;
    }}
    return size;
}}
",
        overflow = BLOB_BYTES + 1
    ));
}

/// Appends the static arrays and wrapper for one baked token sequence.
fn bake_tokens(source: &mut String, baked: &BakedTokens) {
    let id = baked.id;
    let count = baked.tokens.len();
    // C has no zero-length array; one unused slot keeps the generated source
    // valid for a call whose parentheses were empty.
    let slots = count.max(1);

    let kinds: Vec<String> = baked
        .tokens
        .iter()
        .map(|token| crate::parser::token_kind_number(&token.kind).to_string())
        .collect();
    source.push_str(&format!(
        "static int __shy_probe_kinds_{id}[{slots}] = {{ {} }};\n",
        list_or_zero(&kinds)
    ));

    for (index, token) in baked.tokens.iter().enumerate() {
        let text = crate::parser::token_text(token);
        source.push_str(&format!(
            "static char __shy_probe_text_{id}_{index}[] = {};\n",
            c_string(&text)
        ));
    }
    let texts: Vec<String> = (0..count)
        .map(|index| format!("__shy_probe_text_{id}_{index}"))
        .collect();
    source.push_str(&format!(
        "static char *__shy_probe_texts_{id}[{slots}] = {{ {} }};\n",
        list_or_zero(&texts)
    ));

    let lens: Vec<String> = baked
        .tokens
        .iter()
        .map(|token| crate::parser::token_text(token).len().to_string())
        .collect();
    source.push_str(&format!(
        "static int __shy_probe_lens_{id}[{slots}] = {{ {} }};\n",
        list_or_zero(&lens)
    ));

    let spans: Vec<String> = baked
        .tokens
        .iter()
        .flat_map(|token| [token.span.start, token.span.end])
        .map(|offset| offset.to_string())
        .collect();
    source.push_str(&format!(
        "static int __shy_probe_spans_{id}[{}] = {{ {} }};\n",
        spans.len().max(1),
        list_or_zero(&spans)
    ));

    // A static Tokens value, initialized by name so the field order in the
    // header can change without touching this emitter.
    source.push_str(&format!(
        "static struct Tokens __shy_probe_tokens_{id} = {{\n    .count = {count},\n    .kinds = __shy_probe_kinds_{id},\n    .texts = __shy_probe_texts_{id},\n    .lens = __shy_probe_lens_{id},\n    .spans = __shy_probe_spans_{id},\n}};\n"
    ));
    source.push_str(&format!(
        "{} {}(void) {{\n    return {}(__shy_probe_tokens_{id});\n}}\n",
        return_spelling(baked.kind),
        wrapper_symbol(id),
        clone_symbol(baked.function)
    ));
}

/// A comma-separated initializer list, or C's zero idiom for an empty one.
fn list_or_zero(items: &[String]) -> String {
    if items.is_empty() {
        "0".to_string()
    } else {
        items.join(", ")
    }
}

/// One byte string as a C string literal.
///
/// Printable ASCII stays readable; every other byte, including the newline a
/// Newline token spells, becomes an octal escape. Three digits are the most an
/// octal escape can take, so an escape never swallows the next character.
fn c_string(text: &str) -> String {
    let mut literal = String::from("\"");
    for byte in text.bytes() {
        match byte {
            b'\\' => literal.push_str("\\\\"),
            b'"' => literal.push_str("\\\""),
            0x20..=0x7e => literal.push(byte as char),
            _ => literal.push_str(&format!("\\{byte:03o}")),
        }
    }
    literal.push('"');
    literal
}

/// The C spelling of a result type the wrapper returns.
fn return_spelling(kind: ResultKind) -> &'static str {
    match kind {
        ResultKind::I32Signed => "int",
        ResultKind::I32Unsigned => "unsigned",
        ResultKind::Bool => "_Bool",
        ResultKind::Tokens => "struct Tokens",
    }
}

/// Parses generated definitions into items of the probe unit.
///
/// The text is only the data and wrappers this module writes, so it needs no
/// declarations from the user's file; `struct Tokens` is a reference the unit's
/// own definition completes. Every span is the request site, because the
/// generated text has no file of its own in the caller's source map and a
/// diagnostic should point at the call that asked for this round.
fn parse_generated(source: String, site: Span) -> Result<Vec<ast::Item>, String> {
    let mut sources = SourceMap::default();
    let file = sources.add("<probe>", source);
    let tokens = lexer::lex(&sources, file).map_err(|error| error.message)?;
    let tokens: Vec<Token> = tokens
        .into_iter()
        .map(|mut token| {
            token.span = site;
            token
        })
        .collect();
    crate::parser::parse(&tokens)
        .map(|unit| unit.items)
        .map_err(|error| error.message)
}

/// Records which token accessors `names` calls, or why one of them cannot be
/// served by the probe image.
fn builtin_names(caller: &str, names: &[String], needs: &mut BuiltinNeeds) -> Option<String> {
    for name in names {
        if MISSING_BUILTINS.contains(&name.as_str()) {
            return Some(format!(
                "'{caller}' calls '{name}', which the probe image does not implement"
            ));
        }
        match name.as_str() {
            "compile_error" => needs.compile_error = true,
            "tokens_len" => needs.tokens_len = true,
            "token_text" => needs.token_text = true,
            "tokens_from_text" => needs.tokens_from_text = true,
            "tokens_from_int" => needs.tokens_from_int = true,
            "tokens_slice" => needs.tokens_slice = true,
            "tokens_concat" => needs.tokens_concat = true,
            "tokens_text" => needs.tokens_text = true,
            "declared" => needs.declared = true,
            "implements" => needs.implements = true,
            "require_impl" => needs.require_impl = true,
            "try_provide_impl" => needs.try_provide_impl = true,
            "field_count" => needs.field_count = true,
            "field_name" => needs.field_name = true,
            "field_type" => needs.field_type = true,
            "field_is_pointer" => needs.field_is_pointer = true,
            _ => {}
        }
    }
    None
}

/// Collects the items the probe unit keeps, each with the names it mentions.
///
/// A missed name costs a link error, never a wrong answer, and the cost of a
/// spurious one is a round reported as unsupported.
fn unit_calls(items: &[ast::Item], calls: &mut Vec<(String, Vec<String>)>) {
    for item in items {
        match item {
            ast::Item::Function(function) => {
                method_names(&function.declarator, &function.body, calls, true);
            }
            ast::Item::Impl(block) => {
                for method in &block.methods {
                    method_names(&method.declarator, &method.body, calls, false);
                }
            }
            ast::Item::Trait(declaration) => {
                for method in &declaration.methods {
                    if let Some(body) = &method.body {
                        method_names(&method.declarator, body, calls, false);
                    }
                }
            }
            ast::Item::Decl(declaration) => {
                let mut names = Vec::new();
                declaration_names(declaration, &mut names);
                calls.push(("<declaration>".to_string(), names));
            }
            ast::Item::Use(_) => {}
        }
    }
}

fn method_names(
    declarator: &ast::Declarator,
    body: &ast::Block,
    calls: &mut Vec<(String, Vec<String>)>,
    plain_function: bool,
) {
    let mut caller = declarator
        .name
        .as_ref()
        .map(|name| name.text.clone())
        .unwrap_or_else(|| "<function>".to_string());
    if !plain_function {
        caller = format!("{caller}@{}:{}", body.span.file.0, body.span.start);
    }
    let mut names = Vec::new();
    called_names(body, &mut names);
    calls.push((caller, names));
}

/// Builds the probe-only unit for one parsed round.
///
/// The requests are the ones `parsed` collected: this reads
/// [`ParseOutput::requests`] and never another round's list. A request that
/// would run a body still holding a declaration-stage placeholder is left out
/// of the round, so the requests it does not reach are still answered.
pub fn build(parsed: &mut ParseOutput, layout: Layout) -> Outcome {
    if parsed.requests.is_empty() {
        return Outcome::Nothing;
    }
    if let Some(generator) = parsed
        .requests
        .iter()
        .find(|request| matches!(request.place, Place::Item))
        && parsed
            .requests
            .iter()
            .any(|request| !matches!(request.place, Place::Item))
    {
        return unsupported(
            generator,
            "an item generator cannot share a probe round with unresolved non-item calls",
        );
    }
    // Every const fn a requested body reaches is cloned under its declared
    // name, because a call inside a cloned body names that declaration. The
    // unit's own helpers are what such a call can name, so they are read
    // before the requests are checked against them.
    let mut items: Vec<ast::Item> = parsed
        .unit
        .items
        .iter()
        .filter(|item| {
            !is_entry(item) && !declaration_is_unresolved(item, &parsed.unresolved_declarations)
        })
        .cloned()
        .collect();
    let mut helper_calls = Vec::new();
    unit_calls(&items, &mut helper_calls);
    // A body that still holds a declaration-stage placeholder cannot run in a
    // probe image: the syntax expansion it needs is not in the tree yet. Only
    // the requests that reach such a body are left unanswered, so unrelated
    // calls in the same round are still answered, and the dropped requests are
    // asked again once the bodies are resolved.
    let mut kept: Vec<ProbeRequest> = Vec::new();
    let mut blocked: Vec<(ProbeRequest, String)> = Vec::new();
    for request in std::mem::take(&mut parsed.requests) {
        match unresolved_reached(parsed, &request, &helper_calls) {
            Some(unresolved) => blocked.push((request, unresolved)),
            None => kept.push(request),
        }
    }
    if kept.is_empty() {
        let (request, unresolved) = &blocked[0];
        return unsupported(
            request,
            format!(
                "the body of '{unresolved}' still holds an unresolved const fn syntax expansion"
            ),
        );
    }
    parsed.requests = kept;
    // One clone per const fn, however many requests call it.
    let mut functions: Vec<usize> = Vec::new();
    let mut calls = Vec::new();
    let mut baked: Vec<BakedTokens> = Vec::new();
    let mut tokens_result = false;
    let mut baked_text: Vec<(String, String)> = Vec::new();
    for (id, request) in parsed.requests.iter().enumerate() {
        let Some(function) = parsed.const_fns.get(request.function) else {
            return unsupported(
                request,
                "the request names a const fn this parse does not have",
            );
        };
        let (parameters, kind) = match signature(function) {
            Ok(signature) => signature,
            Err(reason) => return unsupported(request, reason),
        };
        // Tokens travel through COMMIT in every syntax position. The parser
        // validates the returned fragment in the position that requested it.
        match (request.place, kind) {
            (_, ResultKind::Tokens) => tokens_result = true,
            (Place::Expr, _) => {}
            (Place::Item, _) => {
                return unsupported(request, "a file-scope generator must return Tokens");
            }
            (place, _) => {
                return unsupported(
                    request,
                    format!(
                        "a {} request produces syntax, not a value",
                        place_word(place)
                    ),
                );
            }
        }
        let (symbol, arguments) = match &request.arguments {
            Arguments::Expressions(expressions) => {
                if parameters
                    .iter()
                    .any(|parameter| *parameter == ParameterKind::Tokens)
                {
                    return unsupported(request, "the call passes values to a Tokens parameter");
                }
                let long_bits = layout.long_bytes * 8;
                let mut arguments = match constants(expressions, &parameters, long_bits) {
                    Ok(values) => values,
                    Err(reason) => return unsupported(request, reason),
                };
                // A string literal becomes bytes in the image; the call gets
                // the address the linker gives them.
                for (index, argument) in arguments.iter_mut().enumerate() {
                    if let ProbeArgument::Text(text) = argument {
                        let symbol = text_symbol(id, index);
                        baked_text.push((symbol.clone(), text.clone()));
                        *argument = ProbeArgument::Text(symbol);
                    }
                }
                (clone_symbol(request.function), arguments)
            }
            Arguments::Tokens(tokens) => {
                if parameters.as_slice() != [ParameterKind::Tokens] {
                    return unsupported(
                        request,
                        "this slice probes only a function whose single parameter is Tokens",
                    );
                }
                baked.push(BakedTokens {
                    id,
                    function: request.function,
                    kind,
                    tokens: tokens.clone(),
                });
                (wrapper_symbol(id), Vec::new())
            }
        };
        calls.push(ProbeCall {
            id,
            function: function.name.clone(),
            symbol,
            kind,
            arguments,
            queue_enabled: matches!(request.place, Place::Item),
        });
        if !functions.contains(&request.function) {
            functions.push(request.function);
        }
    }
    let dependencies = const_dependencies(parsed, &functions, &items);
    // A body that calls an accessor this slice does not implement would leave
    // the probe image with an unresolved symbol, so the round stops here. Both
    // the cloned const fns and every helper the unit keeps are checked: the
    // image links the unit as a whole, not only the call the trampoline makes.
    let mut needs = BuiltinNeeds::default();
    for &index in &dependencies {
        let function = &parsed.const_fns[index];
        let mut names = Vec::new();
        called_names(&function.body, &mut names);
        if let Some(reason) = builtin_names(&function.name, &names, &mut needs) {
            return unsupported(&parsed.requests[0], reason);
        }
    }
    for (caller, names) in &helper_calls {
        if let Some(reason) = builtin_names(caller, names, &mut needs) {
            return unsupported(&parsed.requests[0], reason);
        }
    }
    let mut unresolved_bodies = false;
    for &index in &functions {
        let function = &parsed.const_fns[index];
        let mut item = clone_function(function, Some(&clone_symbol(index)));
        if let ast::Item::Function(clone) = &mut item {
            // The trampoline calls a renamed clone. Check it against the
            // original declaration name and retain that name in diagnostics.
            unresolved_bodies |= guard_unresolved_body(
                &clone.specs,
                &clone.declarator,
                &mut clone.body,
                &parsed.deferred_body_functions,
            );
        }
        items.push(item);
    }
    for &index in &dependencies {
        let function = &parsed.const_fns[index];
        let position = first_initializer(&items, &function.name);
        items.insert(position, clone_function(function, None));
    }
    // Guard the body itself, not an approximate graph of its callers. This
    // covers methods, indirect calls and try_run without rejecting unrelated
    // requests merely because an unused helper is present.
    unresolved_bodies |= guard_unresolved_bodies(&mut items, &parsed.deferred_body_functions);
    let try_runs: Vec<&TrySite> = parsed.try_sites.iter().collect();
    let records = if needs.reflects_fields() {
        reflect_records(&items)
    } else {
        Vec::new()
    };
    match generated_items(
        &baked,
        &baked_text,
        &needs,
        tokens_result,
        calls.iter().any(|call| call.queue_enabled),
        unresolved_bodies,
        &try_runs,
        &parsed.world,
        &records,
        parsed.requests[0].site,
        layout,
    ) {
        Ok(generated) => items.extend(generated),
        Err(reason) => return unsupported(&parsed.requests[0], reason),
    }
    let unit = ast::TranslationUnit { items };
    // A unit that does not lower has no answer to give, and a narrower unit
    // would answer from a body whose helpers are missing.
    if let Err(reason) = lowers(&unit, layout) {
        return unsupported(&parsed.requests[0], reason);
    }
    Outcome::Unit(ProbeUnit { unit, calls })
}

/// The first body a request would run that still holds a declaration-stage
/// placeholder, if any.
///
/// A cloned body calls its const fn dependencies and the unit's ordinary
/// helpers by their declared names, so the same names walk the graph. Only
/// what this request reaches counts: an unused helper's unresolved body must
/// not hold the round back. An indirect call is invisible here, so the probe
/// image's run-time guard stays the backstop for those.
fn unresolved_reached(
    parsed: &ParseOutput,
    request: &ProbeRequest,
    helpers: &[(String, Vec<String>)],
) -> Option<String> {
    let function = parsed.const_fns.get(request.function)?;
    let mut pending = vec![function.declarator.name.as_ref()?.text.clone()];
    let mut seen: Vec<String> = Vec::new();
    while let Some(name) = pending.pop() {
        if seen.contains(&name) {
            continue;
        }
        seen.push(name.clone());
        if let Some(index) = parsed.const_fns.iter().position(|function| {
            function
                .declarator
                .name
                .as_ref()
                .is_some_and(|n| n.text == name)
        }) {
            if body_is_unresolved(
                parsed.const_fns[index].body.span,
                &parsed.deferred_body_functions,
            ) {
                return Some(name);
            }
            called_names(&parsed.const_fns[index].body, &mut pending);
        } else if let Some((_, names)) = helpers.iter().find(|(helper, _)| *helper == name) {
            if parsed.unit.items.iter().any(|item| {
                matches!(item, ast::Item::Function(function)
                    if function.declarator.name.as_ref().is_some_and(|n| n.text == name)
                    && body_is_unresolved(function.body.span, &parsed.deferred_body_functions))
            }) {
                return Some(name);
            }
            pending.extend(names.iter().cloned());
        }
    }
    None
}

/// Replaces the body of every ordinary function that still holds a syntax
/// placeholder.
///
/// The probe image never runs such a body: a request that reaches one is left
/// out of the round, and an indirect call must fail instead of answering from a
/// missing expansion. Keeping only the fail-fast entry call and a zero return
/// also keeps the placeholder's type out of the image's type check.
fn body_is_unresolved(body: Span, openings: &[Span]) -> bool {
    openings
        .iter()
        .any(|span| span.file == body.file && span.start == body.start)
}

fn guard_unresolved_bodies(items: &mut [ast::Item], names: &[Span]) -> bool {
    let mut guarded = false;
    for item in items {
        match item {
            ast::Item::Function(function) => {
                guarded |= guard_unresolved_body(
                    &function.specs,
                    &function.declarator,
                    &mut function.body,
                    names,
                );
            }
            ast::Item::Impl(block) => {
                for method in &mut block.methods {
                    guarded |= guard_unresolved_body(
                        &method.specs,
                        &method.declarator,
                        &mut method.body,
                        names,
                    );
                }
            }
            ast::Item::Trait(declaration) => {
                for method in &mut declaration.methods {
                    if let Some(body) = &mut method.body {
                        guarded |=
                            guard_unresolved_body(&method.specs, &method.declarator, body, names);
                    }
                }
            }
            ast::Item::Decl(_) | ast::Item::Use(_) => {}
        }
    }
    guarded
}

/// Replaces one body with the fail-fast entry call and a zero return.
fn guard_unresolved_body(
    specs: &ast::DeclSpecs,
    declarator: &ast::Declarator,
    body: &mut ast::Block,
    names: &[Span],
) -> bool {
    let Some(name) = &declarator.name else {
        return false;
    };
    if !body_is_unresolved(body.span, names) {
        return false;
    }
    let span = body.span;
    let expr = ast::Expr::Call {
        callee: Box::new(ast::Expr::Name(ast::Name::new(
            "__shy_probe_unresolved_body",
            span,
        ))),
        type_args: Vec::new(),
        args: vec![ast::Expr::String(ast::StringLitGroup {
            parts: vec![ast::StringLit {
                prefix: String::new(),
                raw: name.text.clone(),
                span,
            }],
            span,
        })],
        span,
    };
    let mut stmts = vec![ast::Stmt::Expr { expr, span }];
    if let Some(statement) = zero_return(specs, declarator, span) {
        stmts.push(statement);
    }
    body.stmts = stmts;
    true
}

/// `return` with a zero value of the declared result type, or none for `void`.
///
/// A result the declarator wraps in a pointer takes `0`; every other result
/// type is spelled by the declaration's own specifiers, so a struct, union, or
/// typedef result gets a zero compound literal of exactly that type.
fn zero_return(
    specs: &ast::DeclSpecs,
    declarator: &ast::Declarator,
    span: Span,
) -> Option<ast::Stmt> {
    let plain = declarator.derived.len() == 1;
    if plain
        && specs.qualifiers.is_empty()
        && matches!(
            specs.type_specs.as_slice(),
            [ast::TypeSpecifier::Builtin(ast::BuiltinType::Void)]
        )
    {
        return None;
    }
    let value = if plain {
        ast::Expr::CompoundLiteral {
            type_name: ast::TypeName {
                span,
                specs: specs.clone(),
                declarator: None,
            },
            init: Box::new(ast::Initializer {
                span,
                kind: ast::InitializerKind::List(Vec::new()),
            }),
            span,
        }
    } else {
        ast::Expr::Number(ast::NumberLit {
            text: "0".to_string(),
            span,
        })
    };
    Some(ast::Stmt::Return {
        value: Some(value),
        span,
    })
}

/// A type, member list, or file-scope initializer with no answer must not
/// contribute an approximate layout or value to a probe image.
fn declaration_is_unresolved(item: &ast::Item, unresolved: &[Span]) -> bool {
    let span = match item {
        ast::Item::Decl(declaration) => declaration.span,
        ast::Item::Function(function) => function.span,
        ast::Item::Impl(block) => block.span,
        ast::Item::Trait(declaration) => declaration.span,
        ast::Item::Use(declaration) => declaration.span,
    };
    unresolved
        .iter()
        .any(|site| site.file == span.file && site.start >= span.start && site.end <= span.end)
}

/// Whether an item defines the user's entry point.
///
/// The probe image brings its own `_start` trampoline, so neither spelling may
/// come from the parsed unit: a second `_start` would give the image two entry
/// sections, and `main` is the program's entry, not something a probe calls.
fn is_entry(item: &ast::Item) -> bool {
    let ast::Item::Function(function) = item else {
        return false;
    };
    matches!(
        function
            .declarator
            .name
            .as_ref()
            .map(|name| name.text.as_str()),
        Some("main" | "_start")
    )
}

/// The const fns requested by the trampoline or called by retained ordinary
/// helpers, plus their transitive const fn calls, in declaration order. Every
/// kept body is lowered, so each named const fn needs its declared-name clone
/// even when the trampoline does not call that helper.
fn const_dependencies(
    parsed: &ParseOutput,
    requested: &[usize],
    items: &[ast::Item],
) -> Vec<usize> {
    let mut reached: Vec<usize> = Vec::new();
    let mut pending: Vec<usize> = requested.to_vec();
    // Ordinary helpers in the probe-only tree can call const fns natively.
    // Keep their declared symbols as well as the trampoline's requested clones.
    let mut helper_calls = Vec::new();
    unit_calls(items, &mut helper_calls);
    for (_, names) in helper_calls {
        for name in names {
            if let Some(index) = parsed.const_fns.iter().position(|function| {
                function
                    .declarator
                    .name
                    .as_ref()
                    .is_some_and(|n| n.text == name)
            }) {
                pending.push(index);
            }
        }
    }
    while let Some(index) = pending.pop() {
        if reached.contains(&index) {
            continue;
        }
        reached.push(index);
        let mut names = Vec::new();
        called_names(&parsed.const_fns[index].body, &mut names);
        for name in names {
            let dependency = parsed.const_fns.iter().position(|function| {
                function
                    .declarator
                    .name
                    .as_ref()
                    .is_some_and(|n| n.text == name)
            });
            if let Some(dependency) = dependency
                && !reached.contains(&dependency)
            {
                pending.push(dependency);
            }
        }
    }
    // Emission order has to be one order, whatever order the bodies named.
    reached.sort_unstable();
    reached
}

/// Emits every const fn the program still names as an ordinary function.
///
/// A request is answered by replacing its call with the value the probe
/// produced; a call or address the trampoline cannot answer stays in the tree,
/// so the unit that is the program needs the function itself.
pub fn insert_native_functions(parsed: &mut ParseOutput) {
    for index in const_dependencies(parsed, &[], &parsed.unit.items) {
        let function = &parsed.const_fns[index];
        let item = clone_function(function, None);
        let position = first_initializer(&parsed.unit.items, &function.name);
        parsed.unit.items.insert(position, item);
    }
}

/// Where an ordinary function rebuilt from a const fn belongs: before the first
/// file-scope initializer that names it, or at the end when none does.
///
/// Types are declared where they are written, before the function, and a body
/// reaches a function whatever the order, so only an initializer decides.
fn first_initializer(items: &[ast::Item], name: &str) -> usize {
    items
        .iter()
        .position(|item| {
            let ast::Item::Decl(declaration) = item else {
                return false;
            };
            let mut names = Vec::new();
            declaration_names(declaration, &mut names);
            names.iter().any(|mentioned| mentioned == name)
        })
        .unwrap_or(items.len())
}

/// Collects the plain names a block mentions: a call `f(...)` or `f<int>(...)`
/// and a bare `f`, which is how a body takes a function's address.
///
/// A method call is a member expression, not a name, and is left alone. So is
/// every expression a type name could hide. A missed name costs a dependency
/// clone, which lowering then reports as [`Outcome::Unsupported`]; it never
/// turns into a guessed answer.
fn called_names(block: &ast::Block, names: &mut Vec<String>) {
    for statement in &block.stmts {
        statement_names(statement, names);
    }
}

fn statement_names(statement: &ast::Stmt, names: &mut Vec<String>) {
    match statement {
        ast::Stmt::Empty(_)
        | ast::Stmt::Break(_)
        | ast::Stmt::Continue(_)
        | ast::Stmt::Goto { .. }
        | ast::Stmt::Asm(_) => {}
        ast::Stmt::Expr { expr, .. } => expression_names(expr, names),
        ast::Stmt::Decl(declaration) => declaration_names(declaration, names),
        ast::Stmt::Let(declaration) => expression_names(&declaration.init, names),
        ast::Stmt::Block(block) => called_names(block, names),
        ast::Stmt::If {
            condition,
            then_branch,
            else_branch,
            ..
        } => {
            expression_names(condition, names);
            statement_names(then_branch, names);
            if let Some(else_branch) = else_branch {
                statement_names(else_branch, names);
            }
        }
        ast::Stmt::While {
            condition, body, ..
        } => {
            expression_names(condition, names);
            statement_names(body, names);
        }
        ast::Stmt::DoWhile {
            body, condition, ..
        } => {
            statement_names(body, names);
            expression_names(condition, names);
        }
        ast::Stmt::For {
            init,
            condition,
            step,
            body,
            ..
        } => {
            match init.as_deref() {
                Some(ast::ForInit::Decl(declaration)) => declaration_names(declaration, names),
                Some(ast::ForInit::Expr(expr)) => expression_names(expr, names),
                Some(ast::ForInit::Let(declaration)) => expression_names(&declaration.init, names),
                None => {}
            }
            if let Some(condition) = condition {
                expression_names(condition, names);
            }
            if let Some(step) = step {
                expression_names(step, names);
            }
            statement_names(body, names);
        }
        ast::Stmt::Switch {
            condition, body, ..
        } => {
            expression_names(condition, names);
            statement_names(body, names);
        }
        ast::Stmt::Case {
            value, end, stmt, ..
        } => {
            expression_names(value, names);
            if let Some(end) = end {
                expression_names(end, names);
            }
            statement_names(stmt, names);
        }
        ast::Stmt::Default { stmt, .. }
        | ast::Stmt::Label { stmt, .. }
        | ast::Stmt::AllowDeferCopy { stmt, .. } => statement_names(stmt, names),
        ast::Stmt::Return { value, .. } => {
            if let Some(value) = value {
                expression_names(value, names);
            }
        }
    }
}

fn declaration_names(declaration: &ast::Declaration, names: &mut Vec<String>) {
    for declarator in &declaration.declarators {
        if let Some(init) = &declarator.init {
            initializer_names(init, names);
        }
    }
}

fn initializer_names(initializer: &ast::Initializer, names: &mut Vec<String>) {
    match &initializer.kind {
        ast::InitializerKind::Expr(expr) => expression_names(expr, names),
        ast::InitializerKind::List(entries) => {
            for entry in entries {
                initializer_names(&entry.value, names);
            }
        }
    }
}

fn expression_names(expression: &ast::Expr, names: &mut Vec<String>) {
    match expression {
        ast::Expr::Number(_)
        | ast::Expr::Char(_)
        | ast::Expr::String(_)
        | ast::Expr::ScopedName { .. }
        | ast::Expr::GenericScopedName { .. }
        | ast::Expr::BuiltinTypeCall { .. }
        | ast::Expr::SizeofType { .. }
        | ast::Expr::AlignofType { .. } => {}
        ast::Expr::Name(name) => names.push(name.text.clone()),
        ast::Expr::Unary { operand, .. }
        | ast::Expr::Postfix { operand, .. }
        | ast::Expr::Cast { value: operand, .. }
        | ast::Expr::SizeofExpr { value: operand, .. }
        | ast::Expr::AlignofExpr { value: operand, .. } => expression_names(operand, names),
        ast::Expr::Try { value, .. } => expression_names(value, names),
        ast::Expr::Binary { left, right, .. }
        | ast::Expr::Assignment { left, right, .. }
        | ast::Expr::Comma { left, right, .. } => {
            expression_names(left, names);
            expression_names(right, names);
        }
        ast::Expr::Conditional {
            condition,
            then_expr,
            else_expr,
            ..
        } => {
            expression_names(condition, names);
            expression_names(then_expr, names);
            expression_names(else_expr, names);
        }
        ast::Expr::Call { callee, args, .. } => {
            if let ast::Expr::Name(name) = callee.as_ref() {
                names.push(name.text.clone());
            } else {
                expression_names(callee, names);
            }
            for argument in args {
                expression_names(argument, names);
            }
        }
        ast::Expr::VariantInit { fields, .. } => {
            for field in fields {
                expression_names(&field.value, names);
            }
        }
        ast::Expr::Index { array, index, .. } => {
            expression_names(array, names);
            expression_names(index, names);
        }
        ast::Expr::Member { object, .. } => expression_names(object, names),
        ast::Expr::CompoundLiteral { init, .. } => initializer_names(init, names),
        ast::Expr::StatementExpr { block, .. } => called_names(block, names),
    }
}

/// The cloned function for one const fn.
///
/// With a `symbol`, the name is generated and the declaration gets C linkage,
/// so the linked symbol is exactly [`ProbeCall::symbol`] and the trampoline
/// can name it. Without one, the declaration keeps its own name, which is what
/// a call inside another cloned body already spells.
fn clone_function(function: &ConstFn, symbol: Option<&str>) -> ast::Item {
    let mut item = function.function_item();
    if let (ast::Item::Function(declaration), Some(symbol)) = (&mut item, symbol) {
        declaration.declarator.name = Some(ast::Name::new(symbol, declaration.span));
        declaration.specs.is_shy_fn = false;
        declaration.specs.is_pub = false;
        declaration.specs.storage = None;
    }
    item
}

/// The symbol of the clone for the const fn at `index`.
fn clone_symbol(index: usize) -> String {
    format!("{CLONE}{index}")
}

/// The symbol of the bytes one string argument of one call is baked into.
fn text_symbol(id: usize, index: usize) -> String {
    format!("{TEXT}{id}_{index}")
}

/// The literal text of an expression that is one string literal.
fn string_literal(expression: &ast::Expr) -> Option<String> {
    let ast::Expr::String(group) = expression else {
        return None;
    };
    let [part] = group.parts.as_slice() else {
        return None;
    };
    Some(part.raw.clone())
}

/// The probe trampoline: `text._start` for one lowered probe unit.
///
/// It sets the stack bottom, sends HELLO, reads the request id the driver
/// wrote to [`SELECTOR`], and jumps to that one call's entry. An entry places
/// the arguments from the function's [`abi_plan::CallPlan`] and calls the
/// cloned symbol with `calln`: a scalar result travels back as one `RESULT`
/// frame, and a Tokens result is written through the hidden return pointer into
/// static storage, flattened by [`SERIALIZER`], and committed as one blob.
/// EXIT ends an entry that no `COMMIT` already ended. The image holds every
/// call of the round, but a run makes exactly one, so the driver can give each
/// request its own emulator. It never calls the user's `main`.
///
/// Append the returned text to [`crate::shy::emit`]'s output before
/// assembling. Only scalar register arguments, a scalar register return, and a
/// Tokens result are accepted; the builder already rejected everything else.
pub fn emit_probe_asm(
    lowered: &lower::Lowered,
    calls: &[ProbeCall],
    layout: Layout,
) -> Result<String, String> {
    if calls.is_empty() {
        return Err("a probe trampoline needs at least one call".to_string());
    }
    let queue_context = calls.iter().any(|call| call.queue_enabled);
    let mut out = String::new();
    out.push_str("\n.section text._start\n.symbol _start\n");
    out.push_str(&format!("setn sp 0x{STACK:08x}\n"));
    out.push_str(&format!(
        "setn 0x {SYSCALL}\nsetn 1x {SERVICE_HELLO}\nsetn 2x 0x{MAGIC:08x}\nsetn 3x {ABI}\nsyscall\n"
    ));
    // The driver writes one request id to SELECTOR before every run and gives
    // each run its own emulator, so the image can hold all of a round's calls
    // while a run makes exactly one. `fx` is the selector's scratch register;
    // no call has happened yet, so nothing else is using it.
    out.push_str(&format!("getn fx 0x{SELECTOR:08x}\n"));
    for call in calls {
        out.push_str(&format!(
            "equn fx {}\njmpn .L.shyc.probe.call_{}\n",
            call.id, call.id
        ));
    }
    // The driver only writes an id this image holds. A miss is a broken
    // protocol, so the run stops instead of answering a different call.
    out.push_str(&format!("setn 0x {SYSCALL}\nsetn 1x 0xFFFFFFFF\nsyscall\n"));
    for call in calls {
        out.push_str(&format!(".L.shyc.probe.call_{}:\n", call.id));
        let function = lowered
            .module
            .functions
            .iter()
            .find(|function| function.name == call.symbol)
            .ok_or_else(|| {
                format!(
                    "the probe image has no function '{}' for call {}",
                    call.symbol, call.id
                )
            })?;
        let plan = &function.abi;
        if plan.args.len() != call.arguments.len() {
            return Err(format!(
                "call {} passes {} argument(s), '{}' takes {}",
                call.id,
                call.arguments.len(),
                call.symbol,
                plan.args.len()
            ));
        }
        for (index, argument) in plan.args.iter().enumerate() {
            let Passing::Direct { parts } = &argument.passing else {
                return Err(format!(
                    "call {}: aggregate argument {index} is not supported",
                    call.id
                ));
            };
            let total = argument.ty.size();
            for part in parts {
                let Location::Register(register) = part.location else {
                    return Err(format!(
                        "call {}: stack argument {index} is not supported in this slice",
                        call.id
                    ));
                };
                let Some(name) = ARGUMENT_REGISTERS.get(register.index as usize) else {
                    return Err(format!(
                        "call {}: argument {index} uses register slot {}",
                        call.id, register.index
                    ));
                };
                match &call.arguments[index] {
                    ProbeArgument::Immediate(value) => {
                        let word = word(*value, total, part)?;
                        out.push_str(&format!("setn {name} 0x{word:08x}\n"));
                    }
                    ProbeArgument::Text(symbol) => {
                        // A pointer is one register word: the address the
                        // linker gives the baked bytes. `setn` is how the
                        // trampoline names every other image object.
                        out.push_str(&format!("setn {name} {symbol}\n"));
                    }
                }
            }
        }
        if queue_context {
            let enabled = u32::from(call.queue_enabled);
            out.push_str(&format!(
                "setn 0x {}\nsetn 1x __shy_probe_current\nputa 1x 0x\nsetn 0x {enabled}\nsetn 1x __shy_probe_can_suspend\nputa 1x 0x\n",
                call.id
            ));
        }
        if call.kind == ResultKind::Tokens {
            // The aggregate result travels through the hidden pointer the ABI
            // reserves, so the callee copies it into the trampoline's own
            // storage and the serializer turns it into the COMMIT bytes.
            let sret = sret_register(plan, call.id, layout)?;
            out.push_str(&format!("setn {sret} {OUT}\n"));
            out.push_str(&format!("calln {}\n", call.symbol));
            // The serializer returns the blob length, which the COMMIT call
            // reads from 3x once the blob's address is in 2x.
            out.push_str(&format!("calln {SERIALIZER}\n"));
            out.push_str(&format!("seta 3x 1x\nsetn 2x {BLOB}\n"));
            out.push_str(&format!(
                "setn 0x {SYSCALL}\nsetn 1x {SERVICE_COMMIT}\nsyscall\n"
            ));
        } else {
            let (tag, low, high, mask) = return_layout(plan, call.id)?;
            out.push_str(&format!("calln {}\n", call.symbol));
            // The return is in the plan's registers now; move it to the RESULT
            // frame's low/high words before the handshake registers reuse 1x/2x.
            out.push_str(&format!("seta 4x {low}\n"));
            if let Some(high) = high {
                out.push_str(&format!("seta 5x {high}\n"));
            } else {
                out.push_str("setn 5x 0\n");
            }
            if let Some(mask) = mask {
                out.push_str(&format!("andn 4x 0x{mask:x}\n"));
            }
            out.push_str(&format!(
                "setn 0x {SYSCALL}\nsetn 1x {SERVICE_RESULT}\nsetn 2x {}\nsetn 3x {tag}\nsyscall\n",
                call.id
            ));
        }
        // A COMMIT already ended the run; this EXIT covers a scalar entry.
        out.push_str(&format!(
            "setn 0x {SYSCALL}\nsetn 1x {SERVICE_EXIT}\nsetn 2x 0\nsyscall\n"
        ));
    }
    out.push_str(".L.shyc.probe.park:\nujmpn .L.shyc.probe.park\n");
    Ok(out)
}

/// The register a Tokens result comes back through: the ABI's hidden pointer.
///
/// The plan owns the register slot, so this reads it instead of assuming the
/// trampoline's `4x`. The pointer must name the static `struct Tokens` the
/// whole round shares, so the size has to be that struct's.
fn sret_register(
    plan: &crate::abi::CallPlan,
    id: usize,
    layout: Layout,
) -> Result<&'static str, String> {
    let ReturnPlan::Indirect {
        pointer: Location::Register(register),
        size,
        ..
    } = &plan.return_value
    else {
        return Err(format!(
            "call {id}: a Tokens result is not returned through a hidden pointer"
        ));
    };
    let tokens_bytes = tokens_bytes(layout);
    if *size != tokens_bytes {
        return Err(format!(
            "call {id}: the Tokens result is {size} bytes, not {tokens_bytes}"
        ));
    }
    match ARGUMENT_REGISTERS.get(register.index as usize) {
        Some(name) => Ok(name),
        None => Err(format!(
            "call {id}: the hidden pointer uses register slot {}",
            register.index
        )),
    }
}

/// The RESULT frame tag, the low/high return registers, and a narrow mask.
///
/// The first slice returns one scalar no wider than 32 bits, so there is no
/// high word and `_Bool` needs its bit masked. A 64-bit result is split the
/// same way the ABI does: the part at object offset 4 is the low word.
fn return_layout(
    plan: &crate::abi::CallPlan,
    id: usize,
) -> Result<(u32, &'static str, Option<&'static str>, Option<u32>), String> {
    let ReturnPlan::Direct { parts } = &plan.return_value else {
        return Err(format!("call {id}: the result is not a scalar"));
    };
    let total = plan.signature.ret.size();
    let mut low = None;
    let mut high = None;
    for part in parts {
        let Location::Register(register) = part.location else {
            return Err(format!("call {id}: the result is not in a register"));
        };
        let Some(name) = crate::shy::abi::return_register(register.index) else {
            return Err(format!(
                "call {id}: the result uses register slot {}",
                register.index
            ));
        };
        match (total - part.source_offset - part.size) * 8 {
            0 => low = Some(name),
            32 => high = Some(name),
            _ => return Err(format!("call {id}: the result is not 32 or 64 bits")),
        }
    }
    let Some(low) = low else {
        return Err(format!("call {id}: the result has no low word"));
    };
    let tag = if total <= 4 {
        RESULT_INT32
    } else if total <= 8 {
        RESULT_INT64
    } else {
        return Err(format!("call {id}: the result is wider than 64 bits"));
    };
    let mask = match total {
        1 => Some(0x1),
        2 => Some(0xffff),
        _ => None,
    };
    Ok((tag, low, high, mask))
}

/// One big-endian part of a scalar value, as the ABI splits it.
fn word(value: i64, total: u32, part: &ValuePart) -> Result<u32, String> {
    let end = part
        .source_offset
        .checked_add(part.size)
        .ok_or_else(|| "a value part overflows".to_string())?;
    if end > total || part.size == 0 || part.size > 8 {
        return Err(format!(
            "value part offset {} size {} does not fit a {total}-byte value",
            part.source_offset, part.size
        ));
    }
    let shift = (total - end) * 8;
    let mask = if part.size >= 8 {
        u64::MAX
    } else {
        (1u64 << (part.size * 8)) - 1
    };
    Ok((((value as u64) >> shift) & mask) as u32)
}

/// The parameter classifications and the result classification, or why the
/// signature is outside the supported set.
///
/// A function qualifies when it is a plain function (no generic parameters, no
/// variadic parameter, no pointer or array after the call), every parameter is
/// `int`, `unsigned`, `_Bool`, or the single Tokens value a token argument
/// bakes (a lone `void` parameter is a prototype for none), the result is one
/// of the three scalars or a Tokens value, and the parameters fit the Shy
/// argument registers. The types are read from the declaration the parser
/// kept, not from the evaluator's summary: `int` and `char` are both an
/// integer there, but only the first one is a value this probe may send.
fn signature(function: &ConstFn) -> Result<(Vec<ParameterKind>, ResultKind), String> {
    let callable = callable(function)?;
    if function.declarator.derived.len() != 1 {
        return Err("the function does not return a plain value".to_string());
    }
    if callable.params.len() > ARGUMENT_REGISTERS.len() {
        return Err(format!(
            "the function has more parameters than the {} Shy argument registers",
            ARGUMENT_REGISTERS.len()
        ));
    }
    let mut parameters = Vec::with_capacity(callable.params.len());
    for (index, parameter) in callable.params.iter().enumerate() {
        // C's `f(void)` is a prototype for no parameters: one unnamed `void`
        // spelling in the list, nothing to pass.
        if index == 0 && callable.params.len() == 1 && bare_void(parameter) {
            continue;
        }
        let Some(declarator) = &parameter.declarator else {
            return Err("a parameter has no name".to_string());
        };
        let Some(name) = &declarator.name else {
            return Err("a parameter has no name".to_string());
        };
        if tokens_parameter(parameter, declarator) {
            parameters.push(ParameterKind::Tokens);
            continue;
        }
        if string_parameter(parameter, declarator) {
            parameters.push(ParameterKind::Str);
            continue;
        }
        match scalar(&parameter.specs) {
            Some(kind) if declarator.derived.is_empty() => {
                parameters.push(ParameterKind::Scalar(kind));
            }
            _ => {
                return Err(format!(
                    "parameter '{}' is not an int, unsigned, bool, const char *, or Tokens",
                    name.text
                ));
            }
        }
    }
    let result = match scalar(&function.specs) {
        Some(kind) => kind,
        None if tokens_specs(&function.specs) => ResultKind::Tokens,
        None => {
            return Err("the result is not an int, unsigned, bool, or Tokens".to_string());
        }
    };
    Ok((parameters, result))
}

/// Whether one parameter receives the call's tokens rather than a value.
///
/// The type is the spelling the parser keeps for a Tokens parameter: a bare
/// Tokens type name or `struct Tokens`. A pointer or array declarator is a
/// different type.
fn tokens_parameter(parameter: &ast::ParamDecl, declarator: &ast::Declarator) -> bool {
    declarator.derived.is_empty() && tokens_specs(&parameter.specs)
}

/// Whether this parameter takes a `const char *`.
///
/// A pointer is one register word, and the trampoline can place the address of
/// a string literal it bakes into the image, so this is the one pointer shape a
/// request accepts.
fn string_parameter(parameter: &ast::ParamDecl, declarator: &ast::Declarator) -> bool {
    let chars = parameter.specs.type_specs.len() == 1
        && matches!(
            parameter.specs.type_specs[0],
            ast::TypeSpecifier::Builtin(ast::BuiltinType::Char)
        );
    chars
        && matches!(
            declarator.derived.as_slice(),
            [ast::DerivedDeclarator::Pointer(_)]
        )
}

/// Whether a specifier list spells the `Tokens` type: a bare type name or
/// `struct Tokens`, never a record definition written in place.
fn tokens_specs(specs: &ast::DeclSpecs) -> bool {
    specs.type_specs.iter().any(|specifier| match specifier {
        ast::TypeSpecifier::TypedefName(name) => name.text == "Tokens",
        ast::TypeSpecifier::Record(record) => {
            record.members.is_none() && record.tag.as_ref().is_some_and(|tag| tag.text == "Tokens")
        }
        _ => false,
    })
}

/// The result classification of a const fn a probe can carry, or why its
/// signature is outside the supported set.
pub fn result_kind(function: &ConstFn) -> Result<ResultKind, String> {
    signature(function).map(|(_, result)| result)
}

/// Why a `const fn` declaration's result is an address, as a phrase that
/// follows "const fn 'name'", or None.
///
/// A probe cannot fold an address into the program: a call whose result is a
/// pointer, or a record holding one, stays an ordinary function call, and an
/// address the probe image computes only exists in that image. Reported at the
/// declaration, which is where the result type is decided.
pub fn declared_address(function: &ConstFn, unit: &ast::TranslationUnit) -> Option<&'static str> {
    // The declarator builds the result type outward from the function: a
    // pointer after it is a pointer result.
    let returns_pointer = function.declarator.derived.get(1..).is_some_and(|result| {
        result
            .iter()
            .any(|builder| matches!(builder, ast::DerivedDeclarator::Pointer(_)))
    });
    if returns_pointer {
        return Some(
            "returns a pointer, which a probe cannot fold into the program: the value \
             is not a compile-time constant, and an address the probe image computes \
             means nothing in this program",
        );
    }
    if specs_hold_pointers(&function.specs, unit) {
        return Some(
            "returns a value that holds pointers, which a probe cannot fold into the \
             program: the value is not a compile-time constant, and an address the \
             probe image computes means nothing in this program",
        );
    }
    None
}

/// Whether a specifier list is a record that holds pointers.
///
/// A record written in place is read directly; a named one is a reference, so
/// its definition is looked up among the unit's declarations.
fn specs_hold_pointers(specs: &ast::DeclSpecs, unit: &ast::TranslationUnit) -> bool {
    specs.type_specs.iter().any(|specifier| {
        let ast::TypeSpecifier::Record(record) = specifier else {
            return false;
        };
        match &record.members {
            Some(members) => members_hold_pointers(members, unit),
            None => record
                .tag
                .as_ref()
                .is_some_and(|tag| record_definition_holds_pointers(&tag.text, unit)),
        }
    })
}

fn members_hold_pointers(members: &[ast::MemberDecl], unit: &ast::TranslationUnit) -> bool {
    members.iter().any(|member| {
        // A pointer declarator ends the search: a record that points at
        // itself must not be followed.
        member.declarators.iter().any(|declarator| {
            declarator
                .declarator
                .derived
                .iter()
                .any(|builder| matches!(builder, ast::DerivedDeclarator::Pointer(_)))
        }) || specs_hold_pointers(&member.specs, unit)
    })
}

fn record_definition_holds_pointers(tag: &str, unit: &ast::TranslationUnit) -> bool {
    unit.items.iter().any(|item| {
        let ast::Item::Decl(declaration) = item else {
            return false;
        };
        declaration.specs.type_specs.iter().any(|specifier| {
            let ast::TypeSpecifier::Record(record) = specifier else {
                return false;
            };
            record.tag.as_ref().is_some_and(|name| name.text == tag)
                && record
                    .members
                    .as_ref()
                    .is_some_and(|members| members_hold_pointers(members, unit))
        })
    })
}

/// Whether the cloned body of a const fn converts an address to an integer.
///
/// The body runs in the probe image, so the integer it computes is an address
/// there, and the answer is folded into the program as a plain number.
pub fn converts_address(lowered: &lower::Lowered, index: usize) -> bool {
    let symbol = clone_symbol(index);
    let Some(function) = lowered
        .module
        .functions
        .iter()
        .find(|function| function.name == symbol)
    else {
        return false;
    };
    function.blocks.iter().flatten().any(|block| {
        block.insts.iter().any(|instruction| {
            matches!(
                &instruction.kind,
                crate::ir::InstKind::Cast {
                    op: crate::ir::CastOp::PtrToInt,
                    ..
                }
            )
        })
    })
}

/// Whether a const fn is a plain function a call can reach at run time.
///
/// The trampoline carries fewer shapes than this: [`result_kind`] decides what
/// a probe image can call, while this decides whether a call the trampoline
/// cannot carry is still ordinary code. Parameters are not restricted, and a
/// pointer result is an ordinary C result like any other.
pub fn native_callable(function: &ConstFn) -> bool {
    callable(function).is_ok() && !function.returns_tokens
}

/// The function declarator of a const fn, or why it is not a callable
/// function.
fn callable(function: &ConstFn) -> Result<&ast::FunctionDeclarator, String> {
    let Some(ast::DerivedDeclarator::Function(callable)) = function.declarator.derived.first()
    else {
        return Err("the declaration is not a function".to_string());
    };
    if !callable.generics.is_empty() {
        return Err("the function is a template".to_string());
    }
    if callable.variadic {
        return Err("the function is variadic".to_string());
    }
    Ok(callable)
}

/// Whether one parameter is C's `void`: no declarator and nothing but the
/// `void` keyword, as in `f(void)`.
fn bare_void(parameter: &ast::ParamDecl) -> bool {
    parameter.declarator.is_none()
        && parameter.specs.qualifiers.is_empty()
        && matches!(
            parameter.specs.type_specs.as_slice(),
            [ast::TypeSpecifier::Builtin(ast::BuiltinType::Void)]
        )
}

/// The classification a specifier list spells, if a probe can carry it.
fn scalar(specs: &ast::DeclSpecs) -> Option<ResultKind> {
    // `_Atomic` is a different object; no other qualifier changes the value.
    if specs
        .qualifiers
        .iter()
        .any(|qualifier| matches!(qualifier, ast::Qualifier::Atomic))
    {
        return None;
    }
    let (mut int, mut signed, mut unsigned, mut boolean) = (0, 0, 0, 0);
    for specifier in &specs.type_specs {
        match specifier {
            ast::TypeSpecifier::Builtin(ast::BuiltinType::Int) => int += 1,
            ast::TypeSpecifier::Builtin(ast::BuiltinType::Signed) => signed += 1,
            ast::TypeSpecifier::Builtin(ast::BuiltinType::Unsigned) => unsigned += 1,
            ast::TypeSpecifier::Builtin(ast::BuiltinType::Bool) => boolean += 1,
            // A typedef, record, enum, or generic name is not builtin, and
            // `char`, `short`, `long`, and floating types have other widths.
            _ => return None,
        }
    }
    match (int, signed, unsigned, boolean) {
        (0, 0, 0, 1) => Some(ResultKind::Bool),
        (0, 1, 0, 0) | (1, 0, 0, 0) | (1, 1, 0, 0) => Some(ResultKind::I32Signed),
        (0, 0, 1, 0) | (1, 0, 1, 0) => Some(ResultKind::I32Unsigned),
        _ => None,
    }
}

/// The argument values the trampoline places, or why one cannot be used.
///
/// The probe computes nothing: a value that is not an integer constant cannot
/// be put in a register now, and it must not be guessed, so the round stops.
fn constants(
    expressions: &[ast::Expr],
    parameters: &[ParameterKind],
    long_bits: u32,
) -> Result<Vec<ProbeArgument>, String> {
    if expressions.len() != parameters.len() {
        return Err(format!(
            "the call passes {} argument(s) but the function takes {}",
            expressions.len(),
            parameters.len()
        ));
    }
    let mut values = Vec::with_capacity(expressions.len());
    for (index, (expression, parameter)) in expressions.iter().zip(parameters).enumerate() {
        match parameter {
            ParameterKind::Str => {
                values.push(ProbeArgument::Text(string_literal(expression).ok_or_else(
                    || {
                        format!(
                            "argument {} is not a string literal a const char * can receive",
                            index + 1
                        )
                    },
                )?));
                continue;
            }
            ParameterKind::Scalar(kind) => {
                let Some(value) = constant(expression, long_bits) else {
                    return Err(format!(
                        "argument {} is not a compile-time integer",
                        index + 1
                    ));
                };
                if !fits(*kind, value) {
                    return Err(format!(
                        "argument {} does not fit its 32-bit parameter",
                        index + 1
                    ));
                }
                values.push(ProbeArgument::Immediate(value));
            }
            ParameterKind::Tokens => {
                return Err(format!("argument {} is a Tokens value", index + 1));
            }
        }
    }
    Ok(values)
}

/// The integer one argument expression spells, when it is a constant.
///
/// The spelling is decoded by [`crate::literal::integer`], the parser the
/// evaluator and lowering use, so a suffix or a hex digit at the end reads the
/// way the rest of the compiler reads it.
fn constant(expression: &ast::Expr, long_bits: u32) -> Option<i64> {
    match expression {
        ast::Expr::Number(number) => {
            let integer = crate::literal::integer(&number.text, long_bits, number.span).ok()?;
            i64::try_from(integer.bits).ok()
        }
        ast::Expr::Unary {
            op: ast::UnaryOp::Plus,
            operand,
            ..
        } => constant(operand, long_bits),
        ast::Expr::Unary {
            op: ast::UnaryOp::Minus,
            operand,
            ..
        } => constant(operand, long_bits)?.checked_neg(),
        _ => None,
    }
}

/// Whether a value fits the 32-bit parameter it is passed to.
fn fits(kind: ResultKind, value: i64) -> bool {
    match kind {
        ResultKind::I32Signed => i32::try_from(value).is_ok(),
        ResultKind::I32Unsigned => u32::try_from(value).is_ok(),
        ResultKind::Bool => matches!(value, 0 | 1),
        // A Tokens parameter takes no integer; the wrapper bakes it.
        ResultKind::Tokens => false,
    }
}

/// Whether the assembled probe unit lowers.
///
/// A const fn body may reach for a construct only the evaluator understands,
/// and a kept declaration may too. A failure here is a reason to skip the
/// round, never a reason to guess.
fn lowers(unit: &ast::TranslationUnit, layout: Layout) -> Result<(), String> {
    let options = lower::Options {
        module_name: "probe".to_string(),
        layout,
        startup: lower::StartupMode::Manual,
        ..lower::Options::default()
    };
    lower::lower(unit, &options)
        .map(|_| ())
        .map_err(|error| format!("the probe unit does not lower: {}", error.message))
}

/// Wraps a request's failure into the round's outcome.
fn unsupported(request: &ProbeRequest, reason: impl Into<String>) -> Outcome {
    Outcome::Unsupported {
        function: request.name.clone(),
        span: request.site,
        reason: reason.into(),
    }
}

/// A readable name for a request position, for a diagnostic.
fn place_word(place: Place) -> &'static str {
    match place {
        Place::Item => "file-scope",
        Place::Expr => "expression",
        Place::Statement => "statement",
        Place::Type => "type",
        Place::Member => "member",
    }
}
