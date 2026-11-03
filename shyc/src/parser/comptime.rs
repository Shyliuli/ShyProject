//! Compile-time evaluation for `const fn` declarations.
//!
//! A `const fn` is an ordinary ShyC function whose call is replaced by its
//! result when the arguments are known at compile time. The evaluator below is
//! the executor: it walks the same AST the runtime compiler walks, keeps values
//! in a small environment, and reports a diagnostic instead of guessing when a
//! value is unknown.

use crate::ast::{self, BinaryOp, UnaryOp};
use crate::lexer::{Token, TokenKind};
use crate::source::{Diagnostic, Span};

/// How a parameter value is passed into a compile-time function.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ParamKind {
    /// An integer or boolean.
    Int,
    /// A string: `const char *`, `Str`, or `String`.
    Str,
    /// The call's argument tokens, captured as written.
    Tokens,
}

/// One `const fn` seen in the translation unit.
#[derive(Clone)]
pub(crate) struct ConstFn {
    pub(crate) name: String,
    /// Where the name is declared, for tools that jump to it.
    pub(crate) span: Span,
    pub(crate) params: Vec<(String, ParamKind)>,
    /// True when the declared return type is `Tokens`, so a call may stand in
    /// a type position and be expanded there.
    pub(crate) returns_tokens: bool,
    pub(crate) body: ast::Block,
}

/// Why a generator stopped: it needs an impl that is not there yet, or it is
/// willing to provide one if nobody else does.
#[derive(Clone, Debug)]
pub(crate) enum Suspend {
    /// Waiting in queue 1 for `owner: trait_name`.
    Needs { owner: String, trait_name: String },
    /// Waiting in queue 2, ready to provide `owner: trait_name`.
    Offers { owner: String, trait_name: String },
}

/// A compile-time value.
#[derive(Clone, Debug)]
pub(crate) enum Value {
    Int(i64),
    Str(String),
    /// Captured or generated token sequence.
    Tokens(Vec<Token>),
    Unit,
}

impl Value {
    fn as_int(&self, span: Span) -> Result<i64, Diagnostic> {
        match self {
            Value::Int(value) => Ok(*value),
            _ => Err(Diagnostic::new(span, "compile-time value is not an integer")),
        }
    }

    fn as_str(&self, span: Span) -> Result<&str, Diagnostic> {
        match self {
            Value::Str(value) => Ok(value),
            _ => Err(Diagnostic::new(span, "compile-time value is not a string")),
        }
    }

    fn as_tokens(&self, span: Span) -> Result<&[Token], Diagnostic> {
        match self {
            Value::Tokens(tokens) => Ok(tokens),
            _ => Err(Diagnostic::new(
                span,
                "compile-time value is not a token sequence",
            )),
        }
    }
}

/// Why a body stopped.
enum Flow {
    Normal,
    Return(Value),
    Break,
    Continue,
}

/// Maximum nested `const fn` calls, so a runaway recursion becomes a
/// diagnostic instead of a hang. Deep enough for a macro walking a long token
/// list or a recursive helper over a hundred steps.
const MAX_DEPTH: usize = 512;

pub(crate) struct Evaluator<'a> {
    functions: &'a [ConstFn],
    /// Declarations visible to `declared()` and `implements()`: the snapshot
    /// of the stage being evaluated. Absent when the evaluator has no world.
    world: Option<&'a crate::parser::World>,
    scopes: Vec<Vec<(String, Value)>>,
    depth: usize,
    /// The impl this run was granted permission to provide, if any.
    grant: Option<(String, String)>,
    /// True while a generator runs under the scheduler. Outside it, a function
    /// body only asks questions: it may not block and may not provide impls.
    scheduling: bool,
    /// Set when the run stopped waiting for, or offering, an impl.
    suspend: Option<Suspend>,
    /// Span of the const fn call being evaluated: tokens made by
    /// `tokens_from_text` carry it, so a diagnostic inside generated code
    /// points at the call site instead of a synthetic file.
    call_span: Span,
}

impl<'a> Evaluator<'a> {
    pub(crate) fn new(functions: &'a [ConstFn]) -> Self {
        Evaluator {
            functions,
            world: None,
            scopes: vec![Vec::new()],
            depth: 0,
            grant: None,
            scheduling: false,
            suspend: None,
            call_span: Span {
                file: crate::source::FileId(0),
                start: 0,
                end: 0,
            },
        }
    }

    /// An evaluator that answers `declared` and `implements` from `world`.
    pub(crate) fn with_world(
        functions: &'a [ConstFn],
        world: &'a crate::parser::World,
    ) -> Self {
        Evaluator {
            functions,
            world: Some(world),
            scopes: vec![Vec::new()],
            depth: 0,
            grant: None,
            scheduling: false,
            suspend: None,
            call_span: Span {
                file: crate::source::FileId(0),
                start: 0,
                end: 0,
            },
        }
    }

    /// Whether `name` is declared in the world this evaluation reads.
    pub(crate) fn declares(&self, name: &str) -> bool {
        self.world.is_some_and(|world| world.names.contains(name))
    }

    /// Whether `owner` has `trait` implemented in this snapshot.
    pub(crate) fn implements(&self, owner: &str, trait_name: &str) -> bool {
        self.world
            .is_some_and(|world| world.implements(owner, trait_name))
    }

    /// Grants this run permission to provide one impl.
    pub(crate) fn grant(&mut self, owner: String, trait_name: String) {
        self.grant = Some((owner, trait_name));
    }

    /// Marks this run as a scheduled generator, which may block.
    pub(crate) fn scheduling(&mut self) {
        self.scheduling = true;
    }

    /// Takes the suspension this run ended with, if any.
    pub(crate) fn take_suspend(&mut self) -> Option<Suspend> {
        self.suspend.take()
    }

    /// Evaluates one expression with no enclosing local scope.
    pub(crate) fn eval(&mut self, expr: &ast::Expr) -> Result<Value, Diagnostic> {
        self.expr(expr)
    }

    /// Calls `name` with already evaluated arguments.
    pub(crate) fn call(
        &mut self,
        index: usize,
        args: &[Value],
        span: Span,
    ) -> Result<Value, Diagnostic> {
        if self.depth >= MAX_DEPTH {
            return Err(Diagnostic::new(
                span,
                format!("compile-time evaluation is nested more than {MAX_DEPTH} calls deep"),
            ));
        }
        let function = &self.functions[index];
        if function.params.len() != args.len() {
            return Err(Diagnostic::new(
                span,
                format!(
                    "const fn '{}' expects {} argument(s), but {} were given",
                    function.name,
                    function.params.len(),
                    args.len()
                ),
            ));
        }
        self.depth += 1;
        let saved_span = self.call_span;
        self.call_span = span;
        self.scopes.push(Vec::new());
        for ((name, kind), value) in function.params.iter().zip(args) {
            let value = match kind {
                ParamKind::Int => Value::Int(value.as_int(span)?),
                ParamKind::Str => Value::Str(value.as_str(span)?.to_string()),
                ParamKind::Tokens => Value::Tokens(value.as_tokens(span)?.to_vec()),
            };
            self.bind(name, value);
        }
        let body = function.body.clone();
        let flow = self.block(&body);
        self.scopes.pop();
        self.call_span = saved_span;
        self.depth -= 1;
        match flow? {
            Flow::Return(value) => Ok(value),
            Flow::Normal => Ok(Value::Unit),
            Flow::Break | Flow::Continue => Err(Diagnostic::new(
                span,
                "break or continue outside a loop in a const fn",
            )),
        }
    }

    fn bind(&mut self, name: &str, value: Value) {
        if let Some(scope) = self.scopes.last_mut() {
            scope.push((name.to_string(), value));
        }
    }

    fn lookup(&self, name: &str) -> Option<&Value> {
        self.scopes
            .iter()
            .rev()
            .find_map(|scope| scope.iter().rev().find(|(key, _)| key == name))
            .map(|(_, value)| value)
    }

    fn assign(&mut self, name: &str, value: Value) -> bool {
        for scope in self.scopes.iter_mut().rev() {
            if let Some(slot) = scope.iter_mut().rev().find(|(key, _)| key == name) {
                slot.1 = value;
                return true;
            }
        }
        false
    }

    fn block(&mut self, block: &ast::Block) -> Result<Flow, Diagnostic> {
        self.scopes.push(Vec::new());
        let mut flow = Flow::Normal;
        for statement in &block.stmts {
            flow = self.stmt(statement)?;
            if !matches!(flow, Flow::Normal) {
                break;
            }
        }
        self.scopes.pop();
        Ok(flow)
    }

    fn stmt(&mut self, statement: &ast::Stmt) -> Result<Flow, Diagnostic> {
        match statement {
            ast::Stmt::Empty(_) => Ok(Flow::Normal),
            ast::Stmt::Expr { expr, .. } => {
                self.expr(expr)?;
                Ok(Flow::Normal)
            }
            ast::Stmt::Decl(declaration) => {
                for declarator in &declaration.declarators {
                    let Some(name) = &declarator.declarator.name else {
                        continue;
                    };
                    let value = match &declarator.init {
                        Some(initializer) => self.initializer(initializer)?,
                        None => Value::Int(0),
                    };
                    self.bind(&name.text, value);
                }
                Ok(Flow::Normal)
            }
            ast::Stmt::Let(declaration) => {
                let value = self.expr(&declaration.init)?;
                self.bind(&declaration.name.text, value);
                Ok(Flow::Normal)
            }
            ast::Stmt::Block(block) => self.block(block),
            ast::Stmt::If {
                condition,
                then_branch,
                else_branch,
                ..
            } => {
                if self.expr(condition)?.as_int(condition.span())? != 0 {
                    self.stmt(then_branch)
                } else if let Some(else_branch) = else_branch {
                    self.stmt(else_branch)
                } else {
                    Ok(Flow::Normal)
                }
            }
            ast::Stmt::While {
                condition, body, ..
            } => {
                while self.expr(condition)?.as_int(condition.span())? != 0 {
                    match self.stmt(body)? {
                        Flow::Break => break,
                        Flow::Continue | Flow::Normal => {}
                        flow @ Flow::Return(_) => return Ok(flow),
                    }
                }
                Ok(Flow::Normal)
            }
            ast::Stmt::DoWhile {
                body, condition, ..
            } => {
                loop {
                    match self.stmt(body)? {
                        Flow::Break => break,
                        Flow::Continue | Flow::Normal => {}
                        flow @ Flow::Return(_) => return Ok(flow),
                    }
                    if self.expr(condition)?.as_int(condition.span())? == 0 {
                        break;
                    }
                }
                Ok(Flow::Normal)
            }
            ast::Stmt::For {
                init,
                condition,
                step,
                body,
                ..
            } => {
                self.scopes.push(Vec::new());
                if let Some(init) = init {
                    match init.as_ref() {
                        ast::ForInit::Decl(declaration) => {
                            self.stmt(&ast::Stmt::Decl(declaration.clone()))?;
                        }
                        ast::ForInit::Expr(expr) => {
                            self.expr(expr)?;
                        }
                        ast::ForInit::Let(declaration) => {
                            let value = self.expr(&declaration.init)?;
                            self.bind(&declaration.name.text, value);
                        }
                    }
                }
                loop {
                    if let Some(condition) = condition
                        && self.expr(condition)?.as_int(condition.span())? == 0
                    {
                        break;
                    }
                    match self.stmt(body)? {
                        Flow::Break => break,
                        Flow::Continue | Flow::Normal => {}
                        flow @ Flow::Return(_) => {
                            self.scopes.pop();
                            return Ok(flow);
                        }
                    }
                    if let Some(step) = step {
                        self.expr(step)?;
                    }
                }
                self.scopes.pop();
                Ok(Flow::Normal)
            }
            ast::Stmt::Break(_) => Ok(Flow::Break),
            ast::Stmt::Continue(_) => Ok(Flow::Continue),
            ast::Stmt::Return { value, .. } => {
                let value = match value {
                    Some(value) => self.expr(value)?,
                    None => Value::Unit,
                };
                Ok(Flow::Return(value))
            }
            ast::Stmt::Switch {
                condition,
                body,
                span,
            } => {
                let value = self.expr(condition)?.as_int(*span)?;
                // Overlapping labels would make the first match win silently.
                let mut labels = Vec::new();
                self.switch_labels(body, &mut labels)?;
                for (index, (low, high)) in labels.iter().enumerate() {
                    if labels[..index]
                        .iter()
                        .any(|(seen_low, seen_high)| low <= seen_high && seen_low <= high)
                    {
                        return Err(Diagnostic::new(*span, "case labels overlap in this switch"));
                    }
                }
                // Which entry runs: the matching case, the default, or none.
                let start = if self.switch_matches(body, value)? {
                    SwitchStart::Case(value)
                } else if has_default(body) {
                    SwitchStart::Default
                } else {
                    SwitchStart::Never
                };
                let mut running = false;
                let flow = self.run_switch(body, &start, &mut running)?;
                // `break` leaves the switch, not the enclosing function.
                Ok(match flow {
                    Flow::Break => Flow::Normal,
                    other => other,
                })
            }
            ast::Stmt::Case { span, .. } | ast::Stmt::Default { span, .. } => Err(Diagnostic::new(
                *span,
                "'case' and 'default' may only appear inside a switch",
            )),
            ast::Stmt::Goto { span, .. } | ast::Stmt::Label { span, .. } => Err(Diagnostic::new(
                *span,
                "labels and goto are not available in a const fn",
            )),
            other => Err(Diagnostic::new(
                span_of_stmt(other),
                "this statement is not supported in a const fn",
            )),
        }
    }

    fn initializer(&mut self, initializer: &ast::Initializer) -> Result<Value, Diagnostic> {
        match &initializer.kind {
            ast::InitializerKind::Expr(expr) => self.expr(expr),
            ast::InitializerKind::List(_) => Err(Diagnostic::new(
                initializer.span,
                "an aggregate initializer is not supported in a const fn",
            )),
        }
    }

    fn expr(&mut self, expr: &ast::Expr) -> Result<Value, Diagnostic> {
        match expr {
            ast::Expr::Number(literal) => Ok(Value::Int(number(&literal.text, literal.span)?)),
            ast::Expr::Char(literal) => {
                let value = crate::literal::character(&literal.prefix, &literal.raw, literal.span)?;
                Ok(Value::Int(i64::from(value)))
            }
            ast::Expr::String(group) => {
                // Escapes are decoded here, so a compile-time string is the
                // same text the program would see at run time.
                let mut text = String::new();
                for part in &group.parts {
                    let value = crate::literal::string(&part.prefix, &part.raw, part.span)?;
                    text.push_str(&String::from_utf8_lossy(&value.bytes));
                }
                Ok(Value::Str(text))
            }
            ast::Expr::Name(name) => self
                .lookup(&name.text)
                .cloned()
                .ok_or_else(|| Diagnostic::new(name.span, format!("unknown compile-time value '{}'", name.text))),
            ast::Expr::Unary { op, operand, span } => {
                let value = self.expr(operand)?;
                let int = value.as_int(*span)?;
                match op {
                    UnaryOp::Minus => Ok(Value::Int(-int)),
                    UnaryOp::Plus => Ok(Value::Int(int)),
                    UnaryOp::LogicalNot => Ok(Value::Int(i64::from(int == 0))),
                    UnaryOp::BitNot => Ok(Value::Int(!int)),
                    _ => Err(Diagnostic::new(*span, "this operator is not supported in a const fn")),
                }
            }
            ast::Expr::Binary {
                op,
                left,
                right,
                span,
            } => {
                if matches!(op, BinaryOp::LogicalAnd | BinaryOp::LogicalOr) {
                    let left = self.expr(left)?.as_int(*span)? != 0;
                    if *op == BinaryOp::LogicalAnd && !left {
                        return Ok(Value::Int(0));
                    }
                    if *op == BinaryOp::LogicalOr && left {
                        return Ok(Value::Int(1));
                    }
                    let right = self.expr(right)?.as_int(*span)? != 0;
                    return Ok(Value::Int(i64::from(right)));
                }
                let left = self.expr(left)?;
                let right = self.expr(right)?;
                binary(*op, &left, &right, *span)
            }
            ast::Expr::Conditional {
                condition,
                then_expr,
                else_expr,
                ..
            } => {
                if self.expr(condition)?.as_int(condition.span())? != 0 {
                    self.expr(then_expr)
                } else {
                    self.expr(else_expr)
                }
            }
            ast::Expr::Comma { left, right, .. } => {
                self.expr(left)?;
                self.expr(right)
            }
            ast::Expr::Call {
                callee,
                type_args,
                args,
                span,
            } => {
                if !type_args.is_empty() {
                    return Err(Diagnostic::new(
                        *span,
                        "a const fn call cannot take generic arguments yet",
                    ));
                }
                let ast::Expr::Name(name) = callee.as_ref() else {
                    return Err(Diagnostic::new(
                        *span,
                        "a const fn can only call a plain function name",
                    ));
                };
                // Two builtins inspect a compile-time string. They are what a
                // const fn needs to walk text the way a macro would.
                if name.text == "tokens_from_text" {
                    let [value] = args.as_slice() else {
                        return Err(Diagnostic::new(
                            *span,
                            "tokens_from_text takes one string",
                        ));
                    };
                    let value = self.expr(value)?;
                    let text = value.as_str(*span)?.to_string();
                    let mut sources = crate::source::SourceMap::default();
                    let file = sources.add("<comptime>", text);
                    let tokens = crate::lexer::lex(&sources, file)
                        .map_err(|error| Diagnostic::new(*span, error.message))?;
                    return Ok(Value::Tokens(
                        tokens
                            .into_iter()
                            .filter(|token| !matches!(token.kind, TokenKind::Eof | TokenKind::Newline))
                            .map(|mut token| {
                                token.span = self.call_span;
                                token
                            })
                            .collect(),
                    ));
                }
                if name.text == "require_impl" || name.text == "try_provide_impl" {
                    let [owner, trait_name] = args.as_slice() else {
                        return Err(Diagnostic::new(
                            *span,
                            format!("{} takes a type and a trait name", name.text),
                        ));
                    };
                    let owner = self.expr(owner)?;
                    let owner = owner.as_str(*span)?.to_string();
                    let trait_name = self.expr(trait_name)?;
                    let trait_name = trait_name.as_str(*span)?.to_string();
                    let present = self.implements(&owner, &trait_name);
                    if name.text == "require_impl" {
                        if present {
                            return Ok(Value::Int(1));
                        }
                        // A scheduled generator waits in queue 1; a function
                        // body only asks, and the answer is no.
                        if !self.scheduling {
                            return Ok(Value::Int(0));
                        }
                        self.suspend = Some(Suspend::Needs { owner, trait_name });
                        return Err(Diagnostic::new(*span, "waiting for an impl"));
                    }
                    // Queue 2: provide it only if nobody else does.
                    if present {
                        return Ok(Value::Int(0));
                    }
                    let granted = self
                        .grant
                        .as_ref()
                        .is_some_and(|(granted_owner, granted_trait)| {
                            granted_owner == &owner && granted_trait == &trait_name
                        });
                    if granted {
                        return Ok(Value::Int(1));
                    }
                    // A function body cannot inject a global impl.
                    if !self.scheduling {
                        return Ok(Value::Int(0));
                    }
                    self.suspend = Some(Suspend::Offers { owner, trait_name });
                    return Err(Diagnostic::new(*span, "offering an impl"));
                }
                if name.text == "declared" {
                    let [value] = args.as_slice() else {
                        return Err(Diagnostic::new(*span, "declared takes one name"));
                    };
                    let value = self.expr(value)?;
                    let name = value.as_str(*span)?.to_string();
                    return Ok(Value::Int(i64::from(self.declares(&name))));
                }
                if name.text == "implements" {
                    let [owner, trait_name] = args.as_slice() else {
                        return Err(Diagnostic::new(
                            *span,
                            "implements takes a type and a trait name",
                        ));
                    };
                    let owner = self.expr(owner)?;
                    let owner = owner.as_str(*span)?.to_string();
                    let trait_name = self.expr(trait_name)?;
                    let trait_name = trait_name.as_str(*span)?.to_string();
                    return Ok(Value::Int(i64::from(self.implements(&owner, &trait_name))));
                }
                if let Some(builtin) = token_builtin(name.text.as_str())
                    .or_else(|| builtin(name.text.as_str()))
                {
                    let mut values = Vec::with_capacity(args.len());
                    for argument in args {
                        values.push(self.expr(argument)?);
                    }
                    return builtin(&values, *span);
                }
                let Some(index) = self.functions.iter().position(|f| f.name == name.text) else {
                    return Err(Diagnostic::new(
                        name.span,
                        format!("'{}' is not a const fn", name.text),
                    ));
                };
                let mut values = Vec::with_capacity(args.len());
                for argument in args {
                    values.push(self.expr(argument)?);
                }
                self.call(index, &values, *span)
            }
            ast::Expr::Cast { value, .. } => self.expr(value),
            ast::Expr::Index { array, index, span } => {
                let array = self.expr(array)?;
                let index = self.expr(index)?.as_int(*span)?;
                match array {
                    Value::Str(text) => {
                        let bytes = text.as_bytes();
                        if index < 0 || index as usize >= bytes.len() {
                            return Err(Diagnostic::new(*span, "compile-time index is out of range"));
                        }
                        Ok(Value::Int(i64::from(bytes[index as usize])))
                    }
                    _ => Err(Diagnostic::new(*span, "this value cannot be indexed at compile time")),
                }
            }
            ast::Expr::Assignment {
                left,
                op,
                right,
                span,
            } => {
                let ast::Expr::Name(name) = left.as_ref() else {
                    return Err(Diagnostic::new(
                        left.span(),
                        "a const fn can only assign to a local name",
                    ));
                };
                let value = match op {
                    ast::AssignOp::Assign => self.expr(right)?,
                    _ => {
                        let current = self.lookup(&name.text).cloned().ok_or_else(|| {
                            Diagnostic::new(
                                name.span,
                                format!("unknown compile-time value '{}'", name.text),
                            )
                        })?;
                        let right = self.expr(right)?;
                        binary(assign_operator(op), &current, &right, *span)?
                    }
                };
                if !self.assign(&name.text, value.clone()) {
                    return Err(Diagnostic::new(
                        name.span,
                        format!("unknown compile-time value '{}'", name.text),
                    ));
                }
                Ok(value)
            }
            ast::Expr::SizeofType { span, .. }
            | ast::Expr::SizeofExpr { span, .. }
            | ast::Expr::AlignofType { span, .. } => Err(Diagnostic::new(
                *span,
                "sizeof is not available in a const fn; pass the size as a parameter",
            )),
            other => Err(Diagnostic::new(
                other.span(),
                "this expression is not supported in a const fn",
            )),
        }
    }
}


/// Where execution starts inside a switch body.
enum SwitchStart {
    /// The case whose value matched.
    Case(i64),
    /// No case matched, so the default label runs.
    Default,
    /// Nothing matched and there is no default.
    Never,
}

impl<'a> Evaluator<'a> {
    /// Collects every label of a switch body as `(low, high)`.
    fn switch_labels(
        &mut self,
        statement: &ast::Stmt,
        out: &mut Vec<(i64, i64)>,
    ) -> Result<(), Diagnostic> {
        match statement {
            ast::Stmt::Block(block) => {
                for inner in &block.stmts {
                    self.switch_labels(inner, out)?;
                }
            }
            ast::Stmt::Case {
                value, end, stmt, ..
            } => {
                out.push(self.case_range(value, end.as_ref())?);
                self.switch_labels(stmt, out)?;
            }
            ast::Stmt::Default { stmt, .. } => self.switch_labels(stmt, out)?,
            _ => {}
        }
        Ok(())
    }

    /// The low and high value of a case label; a plain label is its own range.
    fn case_range(
        &mut self,
        case: &ast::Expr,
        end: Option<&ast::Expr>,
    ) -> Result<(i64, i64), Diagnostic> {
        let low = self.expr(case)?.as_int(case.span())?;
        let high = match end {
            Some(end) => self.expr(end)?.as_int(end.span())?,
            None => low,
        };
        Ok((low.min(high), low.max(high)))
    }

    /// Whether a `case` in the switch body matches `value`.
    fn switch_matches(&mut self, statement: &ast::Stmt, value: i64) -> Result<bool, Diagnostic> {
        match statement {
            ast::Stmt::Block(block) => {
                for inner in &block.stmts {
                    if self.switch_matches(inner, value)? {
                        return Ok(true);
                    }
                }
                Ok(false)
            }
            ast::Stmt::Case {
                value: case,
                end,
                stmt,
                ..
            } => {
                let (low, high) = self.case_range(case, end.as_ref())?;
                if (low..=high).contains(&value) {
                    return Ok(true);
                }
                self.switch_matches(stmt, value)
            }
            ast::Stmt::Default { stmt, .. } => self.switch_matches(stmt, value),
            _ => Ok(false),
        }
    }

    /// Runs the switch body from the chosen label, falling through the rest.
    fn run_switch(
        &mut self,
        statement: &ast::Stmt,
        start: &SwitchStart,
        running: &mut bool,
    ) -> Result<Flow, Diagnostic> {
        match statement {
            ast::Stmt::Block(block) => {
                for inner in &block.stmts {
                    let flow = self.run_switch(inner, start, running)?;
                    if !matches!(flow, Flow::Normal) {
                        return Ok(flow);
                    }
                }
                Ok(Flow::Normal)
            }
            ast::Stmt::Case {
                value: case,
                end,
                stmt,
                ..
            } => {
                if !*running
                    && let SwitchStart::Case(target) = start
                {
                    let (low, high) = self.case_range(case, end.as_ref())?;
                    if (low..=high).contains(target) {
                        *running = true;
                    }
                }
                // A label may sit inside another label's statement, as in
                // `case 1: case 2: body`, so look inside even before running.
                self.run_switch(stmt, start, running)
            }
            ast::Stmt::Default { stmt, .. } => {
                if !*running && matches!(start, SwitchStart::Default) {
                    *running = true;
                }
                self.run_switch(stmt, start, running)
            }
            // A `break` only ends the switch once its entry is running.
            ast::Stmt::Break(_) => Ok(if *running { Flow::Break } else { Flow::Normal }),
            other => {
                if *running {
                    self.stmt(other)
                } else {
                    Ok(Flow::Normal)
                }
            }
        }
    }
}

/// Whether a switch body has a `default` label.
fn has_default(statement: &ast::Stmt) -> bool {
    match statement {
        ast::Stmt::Block(block) => block.stmts.iter().any(has_default),
        ast::Stmt::Default { .. } => true,
        ast::Stmt::Case { stmt, .. } => has_default(stmt),
        _ => false,
    }
}
fn span_of_stmt(statement: &ast::Stmt) -> Span {
    match statement {
        ast::Stmt::Switch { span, .. } => *span,
        ast::Stmt::Case { span, .. } => *span,
        ast::Stmt::Default { span, .. } => *span,
        ast::Stmt::Label { span, .. } => *span,
        ast::Stmt::Goto { span, .. } => *span,
        ast::Stmt::Asm(ast::AsmStmt::Raw { span, .. }) => *span,
        ast::Stmt::Asm(ast::AsmStmt::Binding { span, .. }) => *span,
        ast::Stmt::Empty(span) | ast::Stmt::Break(span) | ast::Stmt::Continue(span) => *span,
        _ => Span {
            file: crate::source::FileId(0),
            start: 0,
            end: 0,
        },
    }
}


/// The arithmetic a compound assignment performs.
fn assign_operator(op: &ast::AssignOp) -> BinaryOp {
    match op {
        ast::AssignOp::Assign => BinaryOp::Add,
        ast::AssignOp::Add => BinaryOp::Add,
        ast::AssignOp::Subtract => BinaryOp::Subtract,
        ast::AssignOp::Multiply => BinaryOp::Multiply,
        ast::AssignOp::Divide => BinaryOp::Divide,
        ast::AssignOp::Remainder => BinaryOp::Remainder,
        ast::AssignOp::ShiftLeft => BinaryOp::ShiftLeft,
        ast::AssignOp::ShiftRight => BinaryOp::ShiftRight,
        ast::AssignOp::BitAnd => BinaryOp::BitAnd,
        ast::AssignOp::BitXor => BinaryOp::BitXor,
        ast::AssignOp::BitOr => BinaryOp::BitOr,
    }
}
fn binary(op: BinaryOp, left: &Value, right: &Value, span: Span) -> Result<Value, Diagnostic> {
    let (a, b) = (left.as_int(span)?, right.as_int(span)?);
    let value = match op {
        BinaryOp::Add => a.checked_add(b),
        BinaryOp::Subtract => a.checked_sub(b),
        BinaryOp::Multiply => a.checked_mul(b),
        BinaryOp::Divide => {
            if b == 0 {
                return Err(Diagnostic::new(span, "division by zero in a const fn"));
            }
            a.checked_div(b)
        }
        BinaryOp::Remainder => {
            if b == 0 {
                return Err(Diagnostic::new(span, "remainder by zero in a const fn"));
            }
            a.checked_rem(b)
        }
        BinaryOp::ShiftLeft => a.checked_shl(b as u32),
        BinaryOp::ShiftRight => a.checked_shr(b as u32),
        BinaryOp::BitAnd => Some(a & b),
        BinaryOp::BitOr => Some(a | b),
        BinaryOp::BitXor => Some(a ^ b),
        BinaryOp::Equal => return Ok(Value::Int(i64::from(a == b))),
        BinaryOp::NotEqual => return Ok(Value::Int(i64::from(a != b))),
        BinaryOp::Less => return Ok(Value::Int(i64::from(a < b))),
        BinaryOp::LessEqual => return Ok(Value::Int(i64::from(a <= b))),
        BinaryOp::Greater => return Ok(Value::Int(i64::from(a > b))),
        BinaryOp::GreaterEqual => return Ok(Value::Int(i64::from(a >= b))),
        BinaryOp::LogicalAnd | BinaryOp::LogicalOr => unreachable!("handled by short circuit"),
    };
    value
        .map(Value::Int)
        .ok_or_else(|| Diagnostic::new(span, "constant arithmetic overflowed in a const fn"))
}

/// Parses an integer literal the way the runtime compiler will see it.
fn number(text: &str, span: Span) -> Result<i64, Diagnostic> {
    let trimmed = text.trim_end_matches(|ch: char| ch.is_ascii_alphabetic());
    let parsed = if let Some(hex) = trimmed.strip_prefix("0x").or_else(|| trimmed.strip_prefix("0X")) {
        i64::from_str_radix(hex, 16)
    } else if let Some(binary) = trimmed.strip_prefix("0b").or_else(|| trimmed.strip_prefix("0B")) {
        i64::from_str_radix(binary, 2)
    } else {
        trimmed.parse::<i64>()
    };
    parsed.map_err(|_| {
        let message = if text.contains('.') || text.contains('e') || text.contains('E') {
            format!("floating literal '{text}' is not available in a const fn")
        } else {
            format!("unsupported integer literal '{text}' in a const fn")
        };
        Diagnostic::new(span, message)
    })
}
/// The compile-time builtins a const fn may call.
///
/// A builtin is a name the evaluator answers directly: it describes compiler
/// state, so it cannot be written as a ShyC function.
fn builtin(name: &str) -> Option<fn(&[Value], Span) -> Result<Value, Diagnostic>> {
    match name {
        "str_len" => Some(|args, span| {
            let [text] = args else {
                return Err(Diagnostic::new(span, "str_len takes one string"));
            };
            Ok(Value::Int(text.as_str(span)?.chars().count() as i64))
        }),
        "str_eq" => Some(|args, span| {
            let [left, right] = args else {
                return Err(Diagnostic::new(span, "str_eq takes two strings"));
            };
            Ok(Value::Int(i64::from(
                left.as_str(span)? == right.as_str(span)?,
            )))
        }),
        "str_byte" => Some(|args, span| {
            let [text, index] = args else {
                return Err(Diagnostic::new(span, "str_byte takes a string and an index"));
            };
            let bytes = text.as_str(span)?.as_bytes();
            let index = index.as_int(span)?;
            if index < 0 || index as usize >= bytes.len() {
                return Err(Diagnostic::new(span, "compile-time index is out of range"));
            }
            Ok(Value::Int(i64::from(bytes[index as usize])))
        }),
        _ => None,
    }
}
/// The source spelling of one token, for a compile-time string result.
pub(crate) fn token_text(token: &Token) -> String {
    match &token.kind {
        TokenKind::Ident(text) | TokenKind::Number(text) | TokenKind::Punct(text) => text.clone(),
        TokenKind::StringLiteral { prefix, raw } => format!("{prefix}\"{raw}\""),
        TokenKind::CharLiteral { prefix, raw } => format!("{prefix}'{raw}'"),
        TokenKind::Newline => "\n".to_string(),
        TokenKind::Eof => String::new(),
    }
}

/// The builtins a const fn may call over token sequences.
pub(crate) fn token_builtin(name: &str) -> Option<fn(&[Value], Span) -> Result<Value, Diagnostic>> {
    match name {
        "tokens_len" => Some(|args, span| {
            let [tokens] = args else {
                return Err(Diagnostic::new(span, "tokens_len takes one token sequence"));
            };
            Ok(Value::Int(tokens.as_tokens(span)?.len() as i64))
        }),
        "token_text" => Some(|args, span| {
            let [tokens, index] = args else {
                return Err(Diagnostic::new(
                    span,
                    "token_text takes a token sequence and an index",
                ));
            };
            let tokens = tokens.as_tokens(span)?;
            let index = index.as_int(span)?;
            if index < 0 || index as usize >= tokens.len() {
                return Err(Diagnostic::new(span, "token index is out of range"));
            }
            Ok(Value::Str(token_text(&tokens[index as usize])))
        }),
        "tokens_text" => Some(|args, span| {
            let [tokens] = args else {
                return Err(Diagnostic::new(span, "tokens_text takes one token sequence"));
            };
            Ok(Value::Str(
                tokens
                    .as_tokens(span)?
                    .iter()
                    .map(token_text)
                    .collect::<Vec<_>>()
                    .join(" "),
            ))
        }),
        "tokens_slice" => Some(|args, span| {
            let [tokens, start, len] = args else {
                return Err(Diagnostic::new(
                    span,
                    "tokens_slice takes a token sequence, a start, and a length",
                ));
            };
            let tokens = tokens.as_tokens(span)?;
            let start = start.as_int(span)?;
            let len = len.as_int(span)?;
            if start < 0 || len < 0 || start as usize + len as usize > tokens.len() {
                return Err(Diagnostic::new(span, "token slice is out of range"));
            }
            Ok(Value::Tokens(
                tokens[start as usize..start as usize + len as usize].to_vec(),
            ))
        }),
        "tokens_from_int" => Some(|args, span| {
            let [value] = args else {
                return Err(Diagnostic::new(span, "tokens_from_int takes one integer"));
            };
            let value = value.as_int(span)?;
            Ok(Value::Tokens(vec![Token {
                kind: TokenKind::Number(value.to_string()),
                span,
                at_line_start: false,
                leading_space: false,
            }]))
        }),
        "gensym" => Some(|args, span| {
            let [prefix] = args else {
                return Err(Diagnostic::new(span, "gensym takes one prefix"));
            };
            let prefix = prefix.as_str(span)?.to_string();
            Ok(Value::Str(gensym(&prefix)))
        }),
        "tokens_concat" => Some(|args, span| {
            let [left, right] = args else {
                return Err(Diagnostic::new(span, "tokens_concat takes two token sequences"));
            };
            let mut tokens = left.as_tokens(span)?.to_vec();
            tokens.extend_from_slice(right.as_tokens(span)?);
            Ok(Value::Tokens(tokens))
        }),
        _ => None,
    }
}
/// A unique identifier text for a generated name.
///
/// Generated locals must not collide with names the caller wrote, so each call
/// gets a number of its own for the whole compilation.
fn gensym(prefix: &str) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let index = NEXT.fetch_add(1, Ordering::Relaxed);
    format!("__{prefix}_{index}")
}
