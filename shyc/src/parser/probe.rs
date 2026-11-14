//! Probe-mode requests and answers.
//!
//! A normal parse evaluates a `const fn` call as it meets it (see
//! [`super::comptime`]). Probe mode does not: it looks the call up in the
//! answers an earlier round produced, and when there is no answer it records a
//! [`ProbeRequest`] and leaves a categorized placeholder in the tree. Nothing
//! here executes anything; the driver builds and runs the probe program between
//! rounds and hands the answers back to [`parse_for_probe`].

use std::collections::HashMap;

use crate::ast;
use crate::lexer::{Token, TokenKind};
use crate::source::{Diagnostic, FileId, Span};

use super::ComptimeSite;
use super::Parser;
use super::World;
use super::comptime;

/// Where a `const fn` call stands: it decides what the result means and what a
/// placeholder looks like.
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
pub enum Place {
    /// A call on its own at file scope, whose result is items.
    Item,
    /// A call inside an expression, whose result is a value.
    Expr,
    /// A call where statements are written.
    Statement,
    /// A call where a type is written.
    Type,
    /// A call where aggregate members are written.
    Member,
}

impl Place {
    fn word(self) -> &'static str {
        match self {
            Place::Item => "item",
            Place::Expr => "expr",
            Place::Statement => "statement",
            Place::Type => "type",
            Place::Member => "member",
        }
    }

    fn from_word(word: &str) -> Option<Place> {
        Some(match word {
            "item" => Place::Item,
            "expr" => Place::Expr,
            "statement" => Place::Statement,
            "type" => Place::Type,
            "member" => Place::Member,
            _ => return None,
        })
    }
}

/// The arguments of one call, as the call wrote them.
///
/// They stay in this process: the manifest a driver writes carries only a
/// [`RequestKey`], and the probe image is built from the tree.
#[derive(Clone)]
pub enum Arguments {
    /// A single `Tokens` parameter receives the call's syntax as written.
    Tokens(Vec<Token>),
    /// Ordinary parameters are expressions.
    Expressions(Vec<ast::Expr>),
}

/// Stable identity of one request.
///
/// Two rounds over the same source produce the same keys, so an answer can be
/// matched without keeping any syntax across a process boundary.
#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct RequestKey {
    /// The `const fn` name.
    pub function: String,
    /// Where the call stands.
    pub place: Place,
    /// The file the call was written in.
    ///
    /// Offsets alone are not unique: an included header and the including file
    /// both start at zero, and so do two headers of the same length.
    pub file: FileId,
    /// Offset of the call in that file.
    pub site: usize,
    /// Fingerprint of the argument spelling, so a call whose arguments changed
    /// is a different key.
    pub arguments: u64,
}

impl RequestKey {
    /// The text a manifest carries.
    ///
    /// The function name comes last because it is the only field that could
    /// contain the separator.
    pub fn text(&self) -> String {
        format!(
            "{}:{}:{}:{:016x}:{}",
            self.place.word(),
            self.file.0,
            self.site,
            self.arguments,
            self.function
        )
    }

    /// Reads back what [`RequestKey::text`] wrote.
    pub fn from_text(text: &str) -> Option<RequestKey> {
        let mut parts = text.splitn(5, ':');
        let place = Place::from_word(parts.next()?)?;
        let file = FileId(parts.next()?.parse().ok()?);
        let site = parts.next()?.parse().ok()?;
        let arguments = u64::from_str_radix(parts.next()?, 16).ok()?;
        let function = parts.next()?.to_string();
        Some(RequestKey {
            function,
            place,
            file,
            site,
            arguments,
        })
    }
}

/// One `const fn` call a probe must answer.
#[derive(Clone)]
pub struct ProbeRequest {
    pub key: RequestKey,
    /// Index into the const fn table.
    pub function: usize,
    pub name: String,
    pub place: Place,
    /// The call as written, from the name to the closing parenthesis.
    pub site: Span,
    /// The arguments, for building the probe image in this process.
    pub arguments: Arguments,
}

/// One `try_run(f, args..., &result)` call a const fn body contains.
///
/// The parser rewrites the call into `__shy_try_<id>(args..., &result)`, so
/// the probe image can supply that helper. This record keeps the function the
/// call names and the shape the helper must have.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrySite {
    /// Number of `try_run` sites before it, and the suffix of every symbol the
    /// probe image generates for this call.
    pub id: usize,
    /// The ordinary function the call tries to run.
    pub function: String,
    /// How many arguments the call passes to `function`.
    pub arity: usize,
    /// The whole call, for diagnostics and for finding the const fn body it
    /// stands in.
    pub site: Span,
}

/// One generator suspended on a runtime-computed impl query.
#[derive(Clone)]
pub struct SuspendedRequest {
    pub request: ProbeRequest,
    pub need: bool,
    pub owner: String,
    pub trait_name: String,
}

/// One answer a probe produced.
#[derive(Clone, Debug)]
pub enum Answer {
    /// A `const fn` that returns a value produced this value.
    Int(i64),
    /// A `const fn` that returns a string produced this text.
    Str(String),
    /// A `const fn` that returns tokens produced this sequence.
    Tokens(Vec<Token>),
    /// The call stopped in queue 1 (it needs an impl) or queue 2 (it offers
    /// one). The scheduler, not this parser, decides what happens next.
    Suspended {
        need: bool,
        owner: String,
        trait_name: String,
    },
    /// The call failed; the span is where the probe pointed, when it knew.
    Failed { message: String, span: Option<Span> },
}

/// The answers of the previous round, by key.
///
/// One map belongs to one translation unit: keys name the file a call was
/// written in, but the driver still starts a fresh map per unit, because the
/// same key in another unit can describe a different call.
#[derive(Clone, Default)]
pub struct Answers {
    by_key: HashMap<RequestKey, Answer>,
}

impl Answers {
    pub fn insert(&mut self, key: RequestKey, answer: Answer) {
        self.by_key.insert(key, answer);
    }

    pub fn get(&self, key: &RequestKey) -> Option<&Answer> {
        self.by_key.get(key)
    }

    pub fn is_empty(&self) -> bool {
        self.by_key.is_empty()
    }
}

/// What one probe-mode parse produced.
pub struct ParseOutput {
    /// Runtime dependencies whose generators are waiting, not runnable this round.
    pub suspended: Vec<SuspendedRequest>,
    /// The tree. A call without an answer left a categorized placeholder, so a
    /// caller that still has requests must not generate code from it.
    pub unit: ast::TranslationUnit,
    /// The declarations, for building the probe image in this process.
    pub const_fns: Vec<comptime::ConstFn>,
    /// Calls this parse could not answer, in source order.
    pub requests: Vec<ProbeRequest>,
    /// Declaration sites this round still leaves unresolved.
    ///
    /// A type or member call shapes the declaration it stands in, and a value
    /// call at file scope shapes the data of the declarator around it. Both
    /// end up in a probe image as layout or bytes, where no run-time guard can
    /// see them being read, so the driver answers these requests before any
    /// other and probe construction leaves every declaration that encloses one
    /// of the sites out of the image. The list comes from the whole request
    /// list, so trimming the requests afterwards cannot make a site disappear.
    pub unresolved_declarations: Vec<Span>,
    /// Ordinary functions whose bodies still contain declaration-stage
    /// placeholders. Probe construction must not evaluate a generator through
    /// one of these bodies.
    pub deferred_body_functions: Vec<Span>,
    /// Compile-time sites, for tools that need the syntax an expansion removed.
    pub sites: Vec<ComptimeSite>,
    /// `try_run` calls this parse rewrote, in source order. Each one stands in
    /// a const fn body and names an ordinary function of this unit.
    pub try_sites: Vec<TrySite>,
    /// Functions declared with `#[const_fn_token_builtin]`, in source order.
    ///
    /// The attribute names a declaration whose implementation the compiler
    /// generates: a probe image supplies it, either from baked data or with a
    /// host request. It is metadata, not a change to the declaration.
    pub token_builtins: Vec<(String, Span)>,
    /// The declarations this parse could see, for a probe image to bake.
    ///
    /// `declared` and `implements` ask about exactly this snapshot. The probe
    /// carries its encoding to the host for matching literal or computed names.
    pub world: WorldSnapshot,
}

/// One `impl Trait for Type` pair of a snapshot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImplPair {
    /// The type that implements the trait, spelled the way the impl wrote it.
    pub owner: String,
    /// The trait with its arguments, spelled canonically as `Show<int>`.
    pub trait_name: String,
}

/// One generic `impl<T: Show> Trait for Vec<T>` of a snapshot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GenericImplSnapshot {
    /// The trait with its arguments, spelled canonically.
    pub trait_name: String,
    /// The base name of the implemented type: `Vec` for `Vec<T>`.
    pub base: String,
    /// One entry per parameter of the target: the trait bounds written on it.
    pub bounds: Vec<Vec<String>>,
}

/// The declaration world one parse saw, as plain data.
///
/// It is a snapshot, not a live view: a query reads what the parse knew when it
/// ended, and the outputs of generators that were still unanswered are not in
/// it. The sets are sorted, so two parses of one source produce the same
/// snapshot and a probe image does not depend on hash order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WorldSnapshot {
    /// Every declared name: functions, objects, types, tags, traits, imports.
    pub names: Vec<String>,
    /// Every concrete `impl Trait for Type`, sorted by owner then trait.
    pub impls: Vec<ImplPair>,
    /// Every generic impl, in source order.
    pub generic_impls: Vec<GenericImplSnapshot>,
}

impl WorldSnapshot {
    /// Copies the world one parse collected into plain, sorted data.
    fn new(world: &World) -> Self {
        let mut names: Vec<String> = world.names.iter().cloned().collect();
        names.sort();
        let mut impls: Vec<ImplPair> = world
            .impls
            .iter()
            .map(|(trait_name, owner)| ImplPair {
                owner: owner.clone(),
                trait_name: trait_name.clone(),
            })
            .collect();
        impls.sort_by(|left, right| {
            (&left.owner, &left.trait_name).cmp(&(&right.owner, &right.trait_name))
        });
        let generic_impls = world
            .generic_impls
            .iter()
            .map(|generic| GenericImplSnapshot {
                trait_name: generic.trait_name.clone(),
                base: generic.base.clone(),
                bounds: generic.bounds.clone(),
            })
            .collect();
        WorldSnapshot {
            names,
            impls,
            generic_impls,
        }
    }
}

impl ParseOutput {
    /// Where a `#[const_fn_token_builtin]` function is declared.
    pub fn token_builtin_span(&self, name: &str) -> Option<Span> {
        self.token_builtins
            .iter()
            .find(|(builtin, _)| builtin == name)
            .map(|(_, span)| *span)
    }
}

/// What one parse knows about probes: the answers it may use and the requests
/// it collected.
///
/// Sub-parsers share it, so a call inside a fragment is recorded once, in the
/// parent's list.
pub(crate) struct ProbeContext {
    answers: Answers,
    requests: Vec<ProbeRequest>,
    suspended: Vec<SuspendedRequest>,
}

impl ProbeContext {
    pub(crate) fn new(answers: Answers) -> Self {
        ProbeContext {
            answers,
            requests: Vec::new(),
            suspended: Vec::new(),
        }
    }

    fn record(&mut self, request: ProbeRequest) {
        if self.requests.iter().any(|known| known.key == request.key) {
            return;
        }
        self.requests.push(request);
    }

    pub(crate) fn take_requests(&mut self) -> Vec<ProbeRequest> {
        std::mem::take(&mut self.requests)
    }

    fn record_suspended(&mut self, suspended: SuspendedRequest) {
        if self
            .suspended
            .iter()
            .any(|known| known.request.key == suspended.request.key)
        {
            return;
        }
        self.suspended.push(suspended);
    }

    pub(crate) fn take_suspended(&mut self) -> Vec<SuspendedRequest> {
        std::mem::take(&mut self.suspended)
    }
}

/// Parses one unit in probe mode, in two stages.
///
/// The declaration stage settles the file-scope world, so a `const fn` call
/// inside a function body is left unasked there. Once that stage asks for
/// nothing — every generator above it already has an answer — the world is
/// final and the unit is parsed again with bodies resolved. That is what stops
/// a body from seeing a world a later generator would have extended.
///
/// Every `const fn` call is answered from `answers`. A call without an answer
/// becomes a [`ProbeRequest`] and leaves a placeholder, so the parse finishes
/// and the driver can run the next round; a call the trampoline cannot carry
/// stays ordinary code. Nothing is evaluated here.
pub fn parse_for_probe(
    tokens: &super::TokenStream,
    answers: Answers,
) -> Result<ParseOutput, Diagnostic> {
    let (declarations, bodies_deferred) = parse_one_stage(tokens, answers.clone(), true)?;
    let mut output = if !declarations.requests.is_empty() || !bodies_deferred {
        declarations
    } else {
        parse_one_stage(tokens, answers, false).map(|(output, _)| output)?
    };
    // A wait says something is missing only once the whole unit has nothing
    // left to ask: a body call can still become a request above, and only
    // another request's answer can make the impl appear.
    if output.requests.is_empty()
        && let Some(suspended) = output.suspended.first()
    {
        return Err(Diagnostic::new(
            suspended.request.site,
            format!(
                "const fn '{}' waits for type '{}' to implement trait '{}', but no runnable generator provides it",
                suspended.request.name, suspended.owner, suspended.trait_name
            ),
        ));
    }
    // A unit with nothing left to ask is the program: a const fn its bodies
    // still name as a value or a call the trampoline cannot carry is emitted
    // as the ordinary function it is.
    if output.requests.is_empty() {
        crate::probe::insert_native_functions(&mut output);
    }
    Ok(output)
}

/// One pass over the unit.
///
/// With `defer_body_calls`, a `const fn` call inside a function body is left as
/// a placeholder and reported through the second half of the result, so the
/// caller knows a second pass is needed.
fn parse_one_stage(
    tokens: &super::TokenStream,
    answers: Answers,
    defer_body_calls: bool,
) -> Result<(ParseOutput, bool), Diagnostic> {
    let global = super::decl::prescan(tokens);
    let mut parser = super::Parser::with_tokens(std::rc::Rc::clone(tokens), false);
    parser.scopes[0].typedefs = global.typedefs;
    parser.scopes[0].tags = global.tags;
    parser.scopes[0].traits = global.traits;
    parser.defer_body_calls = defer_body_calls;
    parser.probe = Some(std::rc::Rc::new(std::cell::RefCell::new(
        ProbeContext::new(answers),
    )));
    let mut items = Vec::new();
    // Probe mode never queues a live generator: every file-scope call was
    // answered from the cache or became a request.
    parser.parse_unit_items(&mut items)?;
    let mut requests = parser.take_probe_requests();
    let suspended = parser
        .probe
        .as_ref()
        .map(|context| context.borrow_mut().take_suspended())
        .unwrap_or_default();
    let try_sites = std::mem::take(&mut parser.try_sites);
    validate_try_sites(&items, &parser.const_fns, &try_sites)?;
    let world = snapshot(&parser, &items);
    // Only a `require_impl` call that actually ran can hold a generator back.
    // A static reading of the body cannot stand in for that: it also sees calls
    // in branches nothing runs, and it misses the ones a helper makes.
    let mut parked: Vec<SuspendedRequest> = Vec::new();
    for suspended in suspended {
        // The probe image suspends instead of answering 0, so the record names
        // the pair the generator actually asked about. The same predicate the
        // type checker uses decides whether the world answers it yet.
        if crate::probe::query_implements(&world, &suspended.owner, &suspended.trait_name) {
            requests.push(suspended.request);
        } else {
            parked.push(suspended);
        }
    }
    // A parked wait is not a request: asking it again this round would report
    // the same suspension. It stays parked while anything else is runnable,
    // because only another request's answer can make the impl appear.
    // `HashMap` has no order, and a manifest or a stub list must not depend on
    // hash order: sort by where the declarations stand.
    let mut token_builtins: Vec<(String, Span)> = parser.token_builtins.into_iter().collect();
    token_builtins.sort_by_key(|(_, span)| (span.file.0, span.start));
    let bodies_deferred = parser.bodies_deferred;
    // Read from the requests this round collected, before the driver drops any
    // of them: a dropped request must still keep its declaration out of a probe
    // image, because a dropped request is not an answered one.
    let unresolved_declarations = unresolved_declarations(&items, &requests);
    Ok((
        ParseOutput {
            suspended: parked,
            unit: ast::TranslationUnit { items },
            const_fns: parser.const_fns,
            requests,
            unresolved_declarations,
            deferred_body_functions: parser.deferred_body_functions,
            sites: parser.comptime_sites,
            try_sites,
            token_builtins,
            world,
        },
        bodies_deferred,
    ))
}

/// The declaration sites the round's requests leave unresolved.
///
/// A type or member call shapes the declaration it stands in; a value call at
/// file scope shapes the data of the declarator around it. Neither is code a
/// probe could trap at run time: a probe image compiles the layout or the bytes
/// and the answer reads them without a call to blame. The site recorded is the
/// call's, so probe construction can leave out the declaration whose own span
/// encloses it.
fn unresolved_declarations(items: &[ast::Item], requests: &[ProbeRequest]) -> Vec<Span> {
    let mut sites: Vec<Span> = Vec::new();
    for request in requests {
        let shapes = matches!(request.place, Place::Type | Place::Member);
        let data = matches!(request.place, Place::Expr)
            && items.iter().any(|item| match item {
                ast::Item::Decl(declaration) => encloses(declaration.span, request.site),
                ast::Item::Function(function) => {
                    encloses(function.declarator.span, request.site)
                        || encloses(function.specs.span, request.site)
                }
                _ => false,
            });
        if (shapes || data) && !sites.contains(&request.site) {
            sites.push(request.site);
        }
    }
    sites
}

/// Whether `outer` stands in the same file as `inner` and covers its bytes.
fn encloses(outer: Span, inner: Span) -> bool {
    outer.file == inner.file && outer.start <= inner.start && inner.end <= outer.end
}

/// Snapshots the declarations this parse can see.
///
/// It is built the way the evaluator's world is built, from the file-scope type
/// names the prescan collected and the items the parse kept. A probe parse does
/// not run the generators a round is about, so their output is not here yet
/// unless an earlier round answered them; a query about a declaration such a
/// generator would add answers 0.
fn snapshot(parser: &Parser, items: &[ast::Item]) -> WorldSnapshot {
    let mut world = World::default();
    world
        .names
        .extend(parser.scopes[0].typedefs.iter().cloned());
    world.names.extend(parser.scopes[0].tags.iter().cloned());
    world.names.extend(parser.scopes[0].traits.iter().cloned());
    world.collect(items);
    WorldSnapshot::new(&world)
}

impl Parser {
    /// Whether this parse collects probe requests instead of evaluating.
    pub(crate) fn probe_active(&self) -> bool {
        self.probe.is_some()
    }

    /// Records that this function body has a placeholder and names the body
    /// that owns it, so a generator cannot be probed through that placeholder.
    pub(crate) fn defer_body_call(&mut self) {
        self.bodies_deferred = true;
        if let Some(span) = self.function_bodies.last()
            && !self.deferred_body_functions.contains(span)
        {
            self.deferred_body_functions.push(*span);
        }
    }

    /// Records one `try_run` call and returns the id its helper symbols use.
    pub(crate) fn record_try_site(&mut self, function: String, arity: usize, site: Span) -> usize {
        let id = self.try_sites.len();
        self.try_sites.push(TrySite {
            id,
            function,
            arity,
            site,
        });
        id
    }

    /// Whether `name` is a local variable or parameter visible at the cursor.
    pub(crate) fn local_visible(&self, name: &str) -> bool {
        self.scopes.iter().any(|scope| scope.values.contains(name))
    }

    pub(crate) fn probe_key(
        &self,
        index: usize,
        place: Place,
        site: Span,
        arguments: u64,
    ) -> RequestKey {
        RequestKey {
            function: self.const_fns[index].name.clone(),
            place,
            file: site.file,
            site: site.start,
            arguments,
        }
    }

    pub(crate) fn probe_answer(&self, key: &RequestKey) -> Option<Answer> {
        self.probe.as_ref()?.borrow().answers.get(key).cloned()
    }

    /// Records one call this parse could not answer.
    ///
    /// A type, member, or statement call leaves a shape or a statement the
    /// round does not have yet, so the ordinary body around it cannot be
    /// trusted by a probe image either: name that body, and probe construction
    /// can leave it out. An expression call is left to the run-time guard,
    /// which also covers the helpers no static walk reaches.
    pub(crate) fn probe_record(&mut self, request: ProbeRequest) {
        if matches!(
            request.place,
            Place::Type | Place::Member | Place::Statement
        ) {
            self.defer_body_call();
        }
        if let Some(context) = &self.probe {
            context.borrow_mut().record(request);
        }
    }

    /// Takes the requests this parse collected.
    pub(crate) fn take_probe_requests(&self) -> Vec<ProbeRequest> {
        self.probe
            .as_ref()
            .map(|context| context.borrow_mut().take_requests())
            .unwrap_or_default()
    }

    /// Answers, or records, one file-scope call: a generator.
    pub(crate) fn probe_item(
        &mut self,
        index: usize,
        span: Span,
        arguments: Arguments,
        items: &mut Vec<ast::Item>,
    ) -> Result<(), Diagnostic> {
        let key = self.probe_key(index, Place::Item, span, fingerprint(&arguments));
        let name = self.const_fns[index].name.clone();
        match self.probe_answer(&key) {
            Some(Answer::Tokens(tokens)) => {
                if tokens.is_empty() {
                    return Ok(());
                }
                let (stage, _) = self
                    .parse_item_fragment(tokens, span, None)
                    .map_err(|error| self.fragment_error(error, index))?;
                // `name = value;` parses at file scope as a C89 implicit-int
                // declaration, but a generator that only writes statements
                // belongs in a function body.
                if stage.len() == 1
                    && let ast::Item::Decl(declaration) = &stage[0]
                    && declaration.specs.type_specs.is_empty()
                    && declaration
                        .declarators
                        .iter()
                        .any(|declarator| declarator.init.is_some())
                {
                    return Err(Diagnostic::new(
                        span,
                        format!(
                            "const fn '{name}' produced statements, which cannot stand at file scope; call it inside a function"
                        ),
                    ));
                }
                items.extend(stage);
                Ok(())
            }
            // A suspension is a dependency, not a completed empty answer: the
            // record below keeps the key unanswered, so the scheduler asks it
            // again once the world answers the pair instead of caching nothing.
            Some(Answer::Suspended {
                need,
                owner,
                trait_name,
            }) => {
                if let Some(context) = &self.probe {
                    context.borrow_mut().record_suspended(SuspendedRequest {
                        request: ProbeRequest {
                            key,
                            function: index,
                            name,
                            place: Place::Item,
                            site: span,
                            arguments,
                        },
                        need,
                        owner,
                        trait_name,
                    });
                }
                Ok(())
            }
            Some(Answer::Int(_)) | Some(Answer::Str(_)) => Err(Diagnostic::new(
                span,
                "an item const fn must produce tokens",
            )),
            Some(Answer::Failed { message, span: at }) => {
                Err(Diagnostic::new(at.unwrap_or(span), message))
            }
            // Categorized placeholder: an unanswered generator contributes no
            // items, so the rest of the unit still parses.
            None => {
                self.probe_record(ProbeRequest {
                    key,
                    function: index,
                    name,
                    place: Place::Item,
                    site: span,
                    arguments,
                });
                Ok(())
            }
        }
    }
}

/// The token kind numbers `enum TokenKind` declares in `core/comptime.shyh`.
///
/// A probe image bakes them next to each token's spelling, and the answer
/// decoder reads them back, so both directions live here.
pub fn token_kind_number(kind: &TokenKind) -> u32 {
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

/// One token rebuilt from the `[kind][len][bytes]` record a probe serialized.
///
/// The bytes are the spelling [`comptime::token_text`] writes, so a literal's
/// prefix and its quoted body split at the quote.
pub fn token_from_probe(kind: u32, spelling: &str) -> Option<TokenKind> {
    Some(match kind {
        0 => TokenKind::Ident(spelling.to_string()),
        1 => TokenKind::Number(spelling.to_string()),
        2 => {
            let (prefix, raw) = literal_parts(spelling, '"')?;
            TokenKind::StringLiteral { prefix, raw }
        }
        3 => {
            let (prefix, raw) = literal_parts(spelling, '\'')?;
            TokenKind::CharLiteral { prefix, raw }
        }
        4 => TokenKind::Punct(spelling.to_string()),
        5 => TokenKind::Newline,
        6 => TokenKind::Eof,
        _ => return None,
    })
}

/// The prefix and body of a literal spelling such as `u8"a"`.
fn literal_parts(spelling: &str, quote: char) -> Option<(String, String)> {
    let (prefix, body) = spelling.split_once(quote)?;
    let raw = body.strip_suffix(quote)?;
    Some((prefix.to_string(), raw.to_string()))
}

/// A fingerprint of how the call spelled its arguments.
///
/// Spans are stable for one source, and token text is what a `Tokens`
/// parameter actually receives, so the same call keys the same in every round.
pub(crate) fn fingerprint(arguments: &Arguments) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    match arguments {
        Arguments::Tokens(tokens) => {
            for token in tokens {
                eat(&mut hash, comptime::token_text(token).as_bytes());
                eat(&mut hash, &[0]);
            }
        }
        Arguments::Expressions(expressions) => {
            for expression in expressions {
                let span = expression.span();
                eat(&mut hash, &(span.start as u64).to_be_bytes());
                eat(&mut hash, &(span.end as u64).to_be_bytes());
                // Generated tokens all inherit the generator call site's
                // span, so spans alone cannot tell two generated expressions
                // apart. The tree can, and a key only has to be stable within
                // one compiler build, which `Debug` is.
                eat(&mut hash, format!("{expression:?}").as_bytes());
            }
        }
    }
    hash
}

/// FNV-1a over one chunk of the fingerprint.
fn eat(hash: &mut u64, bytes: &[u8]) {
    for byte in bytes {
        *hash ^= u64::from(*byte);
        *hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
}

/// Rejects a `try_run` call whose function or signature the probe image cannot
/// serve.
///
/// The shape was checked while the call was parsed; the declarations are only
/// complete at the end of the unit, so the function it names is resolved here.
fn validate_try_sites(
    items: &[ast::Item],
    const_fns: &[comptime::ConstFn],
    sites: &[TrySite],
) -> Result<(), Diagnostic> {
    for try_site in sites {
        let name = &try_site.function;
        if const_fns.iter().any(|function| function.name == *name) {
            return Err(Diagnostic::new(
                try_site.site,
                format!("try_run cannot run '{name}': a const fn already runs at compile time"),
            ));
        }
        let Some(function) = function_definition(items, name) else {
            let message = if function_prototype(items, name) {
                format!(
                    "try_run names '{name}', which this translation unit only declares; try_run needs its definition"
                )
            } else {
                format!("try_run names '{name}', which this translation unit does not declare")
            };
            return Err(Diagnostic::new(try_site.site, message));
        };
        check_try_function(function, name, try_site)?;
    }
    Ok(())
}

/// Checks that one ordinary function has the shape a generated try helper
/// calls: a non-variadic int-to-int function whose parameters match the call.
fn check_try_function(
    function: &ast::FunctionDecl,
    name: &str,
    try_site: &TrySite,
) -> Result<(), Diagnostic> {
    let only_int = "try_run currently supports only int parameters and an int return type";
    let Some(ast::DerivedDeclarator::Function(declarator)) = function.declarator.derived.first()
    else {
        return Err(Diagnostic::new(
            try_site.site,
            format!("try_run needs '{name}' to be a function definition"),
        ));
    };
    if function.declarator.derived.len() != 1 || !int_specs(&function.specs) {
        return Err(Diagnostic::new(
            try_site.site,
            format!("try_run needs '{name}' to return int; {only_int}"),
        ));
    }
    if declarator.variadic {
        return Err(Diagnostic::new(
            try_site.site,
            format!("try_run cannot call variadic function '{name}'; {only_int}"),
        ));
    }
    let parameters = parameters_of(declarator);
    if parameters.len() != try_site.arity {
        return Err(Diagnostic::new(
            try_site.site,
            format!(
                "try_run passes {} argument(s) to '{name}', which takes {}",
                try_site.arity,
                parameters.len()
            ),
        ));
    }
    for (index, parameter) in parameters.iter().enumerate() {
        if !int_parameter(parameter) {
            return Err(Diagnostic::new(
                try_site.site,
                format!(
                    "try_run needs int parameters: parameter {} of '{name}' is not an int; {only_int}",
                    index + 1
                ),
            ));
        }
    }
    Ok(())
}

/// The parameters a declarator really takes: `(void)` spells none.
fn parameters_of(declarator: &ast::FunctionDeclarator) -> &[ast::ParamDecl] {
    if declarator.params.len() == 1 && bare_void(&declarator.params[0]) {
        &[]
    } else {
        &declarator.params
    }
}

/// Whether a parameter is the lone `void` that stands for an empty list.
fn bare_void(parameter: &ast::ParamDecl) -> bool {
    parameter.declarator.is_none()
        && parameter.specs.type_specs.len() == 1
        && matches!(
            parameter.specs.type_specs[0],
            ast::TypeSpecifier::Builtin(ast::BuiltinType::Void)
        )
}

/// Whether a parameter is a plain `int`, with no pointer, array, or function
/// declarator around it.
fn int_parameter(parameter: &ast::ParamDecl) -> bool {
    let plain = parameter
        .declarator
        .as_ref()
        .is_none_or(|declarator| declarator.derived.is_empty());
    plain && int_specs(&parameter.specs)
}

/// Whether a specifier list spells `int` (including `signed` and `signed int`).
fn int_specs(specs: &ast::DeclSpecs) -> bool {
    let mut saw_int = false;
    let mut saw_signed = false;
    for specifier in &specs.type_specs {
        match specifier {
            ast::TypeSpecifier::Builtin(ast::BuiltinType::Int) => saw_int = true,
            ast::TypeSpecifier::Builtin(ast::BuiltinType::Signed) => saw_signed = true,
            _ => return false,
        }
    }
    saw_int || saw_signed
}

/// The function definition named `name`, if this unit has one.
fn function_definition<'a>(items: &'a [ast::Item], name: &str) -> Option<&'a ast::FunctionDecl> {
    items.iter().find_map(|item| match item {
        ast::Item::Function(function)
            if function
                .declarator
                .name
                .as_ref()
                .is_some_and(|declared| declared.text == name) =>
        {
            Some(function)
        }
        _ => None,
    })
}

/// Whether this unit declares `name` with a function prototype only.
fn function_prototype(items: &[ast::Item], name: &str) -> bool {
    items.iter().any(|item| {
        let ast::Item::Decl(declaration) = item else {
            return false;
        };
        declaration.declarators.iter().any(|declarator| {
            declarator
                .declarator
                .name
                .as_ref()
                .is_some_and(|declared| declared.text == name)
                && matches!(
                    declarator.declarator.derived.first(),
                    Some(ast::DerivedDeclarator::Function(_))
                )
        })
    })
}
