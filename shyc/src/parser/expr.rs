//! Expression syntax: precedence climbing plus C and Shy postfix constructs.
//!
//! The tree records binding, not meaning: `a + b * c` is a sum of `a` and a
//! product. `Type::method` stays a [`crate::ast::Expr::ScopedName`], and
//! `p.field` on a pointer stays a dot member, because Shy's auto-dereference is
//! semantic work.

use crate::ast::{
    AssignOp, BinaryOp, Block, CharLit, Expr, GenericArg, LetDecl, MemberOp, Name, NumberLit,
    PostfixOp, Stmt, StringLit, StringLitGroup, UnaryOp,
};
use crate::lexer::{Token, TokenKind};

use super::probe::{self, Answer, Place, ProbeRequest};
use crate::source::{Diagnostic, Span};

use super::Parser;

impl Parser {
    /// Comma expression. Used by expression statements, `return`, `for`
    /// clauses, and parenthesised subexpressions.
    pub(crate) fn expression(&mut self) -> Result<Expr, Diagnostic> {
        let mut expr = self.assignment()?;
        while self.eat_punct(",") {
            let right = self.assignment()?;
            let span = expr.span().join(right.span());
            expr = Expr::Comma {
                left: Box::new(expr),
                right: Box::new(right),
                span,
            };
        }
        Ok(expr)
    }

    /// Assignment expression; right associative, so `a = b = c` nests to the
    /// right. This is also the entry point for a bare conditional expression.
    pub(crate) fn assignment(&mut self) -> Result<Expr, Diagnostic> {
        let left = self.conditional()?;
        let Some(op) = self.assign_op() else {
            return Ok(left);
        };
        self.bump();
        let right = self.assignment()?;
        let span = left.span().join(right.span());
        Ok(Expr::Assignment {
            op,
            left: Box::new(left),
            right: Box::new(right),
            span,
        })
    }

    fn assign_op(&self) -> Option<AssignOp> {
        let TokenKind::Punct(punct) = &self.peek().kind else {
            return None;
        };
        Some(match punct.as_str() {
            "=" => AssignOp::Assign,
            "+=" => AssignOp::Add,
            "-=" => AssignOp::Subtract,
            "*=" => AssignOp::Multiply,
            "/=" => AssignOp::Divide,
            "%=" => AssignOp::Remainder,
            "<<=" => AssignOp::ShiftLeft,
            ">>=" => AssignOp::ShiftRight,
            "&=" => AssignOp::BitAnd,
            "^=" => AssignOp::BitXor,
            "|=" => AssignOp::BitOr,
            _ => return None,
        })
    }

    /// Conditional expression: `condition ? then : else`.
    ///
    /// `?` is also the postfix propagation operator. The postfix loop leaves a
    /// ternary `?` alone when a middle expression and `:` follow it; if it did
    /// consume one anyway, an exact `Try` condition is re-associated back into
    /// an ordinary C conditional here.
    pub(crate) fn conditional(&mut self) -> Result<Expr, Diagnostic> {
        let condition = self.binary(1)?;
        if !self.eat_punct("?") {
            if let Expr::Try { value, span } = &condition
                && self.at_expression_start(0)
                && let Some(then_expr) = self.try_parse(|parser| {
                    let then_expr = parser.expression()?;
                    parser.expect_punct(":")?;
                    Ok(then_expr)
                })
            {
                let else_expr = self.conditional()?;
                let span = span.join(else_expr.span());
                return Ok(Expr::Conditional {
                    condition: value.clone(),
                    then_expr: Box::new(then_expr),
                    else_expr: Box::new(else_expr),
                    span,
                });
            }
            return Ok(condition);
        }
        // The middle operand is a full expression; the else operand is
        // conditional again, matching the grammar's right association.
        let then_expr = self.expression()?;
        self.expect_punct(":")?;
        let else_expr = self.conditional()?;
        let span = condition.span().join(else_expr.span());
        Ok(Expr::Conditional {
            condition: Box::new(condition),
            then_expr: Box::new(then_expr),
            else_expr: Box::new(else_expr),
            span,
        })
    }

    /// Precedence climbing over every C binary operator. Level 1 binds
    /// loosest (`||`), level 10 tightest (`*`). All are left associative.
    fn binary(&mut self, minimum: u8) -> Result<Expr, Diagnostic> {
        let mut left = self.cast()?;
        while let Some((precedence, op)) = self.binary_op() {
            if precedence < minimum {
                break;
            }
            self.bump();
            let right = self.binary(precedence + 1)?;
            let span = left.span().join(right.span());
            left = Expr::Binary {
                op,
                left: Box::new(left),
                right: Box::new(right),
                span,
            };
        }
        Ok(left)
    }

    fn binary_op(&self) -> Option<(u8, BinaryOp)> {
        let TokenKind::Punct(punct) = &self.peek().kind else {
            return None;
        };
        // Inside `<...>`, `>` closes the argument list instead of comparing.
        if self.angle_depth > 0 && matches!(punct.as_str(), ">" | ">=" | ">>") {
            return None;
        }
        Some(match punct.as_str() {
            "||" => (1, BinaryOp::LogicalOr),
            "&&" => (2, BinaryOp::LogicalAnd),
            "|" => (3, BinaryOp::BitOr),
            "^" => (4, BinaryOp::BitXor),
            "&" => (5, BinaryOp::BitAnd),
            "==" => (6, BinaryOp::Equal),
            "!=" => (6, BinaryOp::NotEqual),
            "<" => (7, BinaryOp::Less),
            "<=" => (7, BinaryOp::LessEqual),
            ">" => (7, BinaryOp::Greater),
            ">=" => (7, BinaryOp::GreaterEqual),
            "<<" => (8, BinaryOp::ShiftLeft),
            ">>" => (8, BinaryOp::ShiftRight),
            "+" => (9, BinaryOp::Add),
            "-" => (9, BinaryOp::Subtract),
            "*" => (10, BinaryOp::Multiply),
            "/" => (10, BinaryOp::Divide),
            "%" => (10, BinaryOp::Remainder),
            _ => return None,
        })
    }

    /// Cast expression, compound literal, or a unary expression.
    fn cast(&mut self) -> Result<Expr, Diagnostic> {
        if self.at_punct("(") && self.starts_type_name(1) {
            let open = self.bump().span;
            let type_name = self.type_name()?;
            self.expect_punct(")")?;
            if self.at_punct("{") {
                // `(type){ ... }` is a compound literal: a postfix expression,
                // so suffixes like `[0]` or `.field` may follow it.
                let init = self.initializer()?;
                let span = open.join(init.span);
                let literal = Expr::CompoundLiteral {
                    type_name,
                    init: Box::new(init),
                    span,
                };
                return self.postfix_suffixes(literal);
            }
            let value = self.cast()?;
            let span = open.join(value.span());
            return Ok(Expr::Cast {
                type_name,
                value: Box::new(value),
                span,
            });
        }
        self.unary()
    }

    fn unary(&mut self) -> Result<Expr, Diagnostic> {
        // `&&` reaching a unary position can only be GNU's labels-as-values;
        // binary logical-and is consumed one level up.
        if self.at_punct("&&") {
            return Err(Diagnostic::new(
                self.here(),
                "labels as values ('&&label') are not supported",
            ));
        }

        if let Some(op) = self.unary_op() {
            let start = self.bump().span;
            let operand = match op {
                UnaryOp::PreIncrement | UnaryOp::PreDecrement => self.unary()?,
                _ => self.cast()?,
            };
            return Ok(Expr::Unary {
                op,
                operand: Box::new(operand),
                span: self.finish(start),
            });
        }

        if self.at_ident("sizeof") {
            return self.sizeof_expr();
        }
        if self.at_ident("_Alignof")
            || self.at_ident("alignof")
            || self.at_ident("__alignof__")
            || self.at_ident("__alignof")
        {
            return self.alignof_expr();
        }
        if self.at_ident("_Generic") {
            return Err(Diagnostic::new(
                self.here(),
                "_Generic selection is not supported",
            ));
        }

        self.postfix()
    }

    fn unary_op(&self) -> Option<UnaryOp> {
        let TokenKind::Punct(punct) = &self.peek().kind else {
            return None;
        };
        Some(match punct.as_str() {
            "+" => UnaryOp::Plus,
            "-" => UnaryOp::Minus,
            "!" => UnaryOp::LogicalNot,
            "~" => UnaryOp::BitNot,
            "*" => UnaryOp::Dereference,
            "&" => UnaryOp::AddressOf,
            "++" => UnaryOp::PreIncrement,
            "--" => UnaryOp::PreDecrement,
            _ => return None,
        })
    }

    fn sizeof_expr(&mut self) -> Result<Expr, Diagnostic> {
        let start = self.bump().span;
        if self.at_punct("(") && self.starts_type_name(1) {
            self.bump();
            let type_name = self.type_name()?;
            self.expect_punct(")")?;
            if self.at_punct("{") {
                // `sizeof (type){...}`: the operand is the compound literal.
                let init = self.initializer()?;
                let span = start.join(init.span);
                let literal = Expr::CompoundLiteral {
                    type_name,
                    init: Box::new(init),
                    span,
                };
                let value = self.postfix_suffixes(literal)?;
                let span = start.join(value.span());
                return Ok(Expr::SizeofExpr {
                    value: Box::new(value),
                    span,
                });
            }
            return Ok(Expr::SizeofType {
                type_name,
                span: self.finish(start),
            });
        }
        let value = self.unary()?;
        let span = start.join(value.span());
        Ok(Expr::SizeofExpr {
            value: Box::new(value),
            span,
        })
    }

    fn alignof_expr(&mut self) -> Result<Expr, Diagnostic> {
        let start = self.bump().span;
        if self.at_punct("(") && self.starts_type_name(1) {
            self.bump();
            let type_name = self.type_name()?;
            self.expect_punct(")")?;
            return Ok(Expr::AlignofType {
                type_name,
                span: self.finish(start),
            });
        }
        let value = self.unary()?;
        let span = start.join(value.span());
        Ok(Expr::AlignofExpr {
            value: Box::new(value),
            span,
        })
    }

    /// Expand `println!("{}", a, b)` into the wrapper that takes two values.
    ///
    /// The format string is parsed at run time by the library, so the compile
    /// time part is the arity: the call becomes `println2(fmt, a, b)` and the
    /// generic bounds of that wrapper type-check every argument. A later
    /// user-defined macro facility replaces this fixed table.

    /// Answers one const fn call in an expression from the probe cache.
    ///
    /// Unanswered calls become requests when the trampoline can carry the
    /// signature. A body call it cannot carry stays an ordinary call, and
    /// other unresolved calls keep a guarded placeholder. Returned tokens are
    /// parsed back into the expression they replace.
    fn probe_expr_call(&mut self, index: usize, start: Span) -> Result<Expr, Diagnostic> {
        self.record_const_call(index, start);
        let (arguments, close) = self.const_arguments(index)?;
        let span = start.join(close);
        // The declaration pass leaves a body call for the second pass, which
        // runs against the settled world: what a body sees must not depend on
        // how far the generators had expanded when the call was asked.
        if self.defer_body_calls && !self.body_stack.is_empty() {
            self.bodies_deferred = true;
            if let Some(call) = self.native_probe_value_call(index, &arguments, start, span) {
                return Ok(call);
            }
            // Syntax-producing calls still need a parsed expansion. Keep them
            // marked as unresolved so probe safety checks can reject them, and
            // keep the call itself so the placeholder has the result's type.
            self.defer_body_call();
            return Ok(self.placeholder_call(index, &arguments, span));
        }
        let name = self.const_fns[index].name.clone();
        let key = self.probe_key(index, Place::Expr, span, probe::fingerprint(&arguments));
        match self.probe_answer(&key) {
            Some(Answer::Int(value)) => Ok(number(value, span)),
            Some(Answer::Str(text)) => Ok(string_literal(&text, span)),
            Some(Answer::Tokens(tokens)) => parse_fragment(tokens, span, self)
                .map_err(|error| self.fragment_error(error, index)),
            Some(Answer::Suspended { .. }) => Err(Diagnostic::new(
                span,
                format!(
                    "probe answered a suspension for '{name}', which stands in an expression and cannot block"
                ),
            )),
            Some(Answer::Failed { message, span: at }) => {
                Err(Diagnostic::new(at.unwrap_or(span), message))
            }
            None => {
                let native_call = if self.body_stack.is_empty() {
                    None
                } else {
                    self.native_probe_value_call(index, &arguments, start, span)
                };
                let placeholder = self.placeholder_call(index, &arguments, span);
                // A call the trampoline cannot carry is not asked: the name
                // stays in the tree and the final unit emits the function.
                let asked = native_call.is_none()
                    || crate::probe::result_kind(&self.const_fns[index]).is_ok();
                if asked {
                    self.probe_record(ProbeRequest {
                        key,
                        function: index,
                        name,
                        place: Place::Expr,
                        site: span,
                        arguments,
                    });
                }
                if let Some(call) = native_call {
                    return Ok(call);
                }
                if !self.body_stack.is_empty() {
                    self.defer_body_call();
                }
                // Categorized placeholder: the call the answer will replace,
                // so the rest of the expression still parses and type-checks.
                // A round that has requests produces no code.
                Ok(placeholder)
            }
        }
    }

    /// A const fn that is a plain function can be a real function: inside a
    /// probe image, and in the final unit when the trampoline cannot carry the
    /// call. This representation is used only while a body call is unresolved;
    /// a call the probe answers is still replaced by that answer.
    fn native_probe_value_call(
        &self,
        index: usize,
        arguments: &probe::Arguments,
        start: Span,
        span: Span,
    ) -> Option<Expr> {
        let probe::Arguments::Expressions(args) = arguments else {
            return None;
        };
        if !crate::probe::native_callable(&self.const_fns[index]) {
            return None;
        }
        Some(Expr::Call {
            callee: Box::new(Expr::Name(Name::new(
                self.const_fns[index].name.clone(),
                start,
            ))),
            type_args: Vec::new(),
            args: args.clone(),
            span,
        })
    }

    /// The expression an unanswered call leaves in the tree.
    ///
    /// A call whose answer is syntax keeps its own expression: the const fn is
    /// declared with its real signature and the probe image carries a clone
    /// under its declared name, so the placeholder has the type the slot
    /// expects. A body that would run it is trapped by the probe image's
    /// unresolved-body guard.
    fn placeholder_call(&self, index: usize, arguments: &probe::Arguments, span: Span) -> Expr {
        let probe::Arguments::Expressions(args) = arguments else {
            return self.placeholder_value(index, span);
        };
        Expr::Call {
            callee: Box::new(Expr::Name(Name::new(
                self.const_fns[index].name.clone(),
                span,
            ))),
            type_args: Vec::new(),
            args: args.clone(),
            span,
        }
    }

    /// A zero value for a call whose syntax argument leaves no expression to
    /// keep.
    ///
    /// A Tokens result is a struct, so it takes a zero compound literal of its
    /// declared type; every other result type is a scalar or a pointer, where
    /// `0` is the zero value.
    fn placeholder_value(&self, index: usize, span: Span) -> Expr {
        let function = &self.const_fns[index];
        if !function.returns_tokens {
            return Expr::Number(NumberLit {
                text: "0".to_string(),
                span,
            });
        }
        Expr::CompoundLiteral {
            type_name: crate::ast::TypeName {
                span,
                specs: function.specs.clone(),
                declarator: None,
            },
            init: Box::new(crate::ast::Initializer {
                span,
                kind: crate::ast::InitializerKind::List(Vec::new()),
            }),
            span,
        }
    }

    /// Rewrites `try_run(f, args..., &result)` into a call to the helper the
    /// probe image generates for this site.
    ///
    /// The function name is removed from the argument list: the helper calls it
    /// from the probe image, where the arguments travel through static storage.
    /// Everything else (the call's arguments and the out parameter) stays, so
    /// the compiler type-checks the same expressions it would have passed to
    /// `f`.
    fn try_run_call(&mut self, name: Name) -> Result<Expr, Diagnostic> {
        let (mut arguments, close) = self.call_arguments()?;
        let span = name.span.join(close);
        if arguments.len() < 2 {
            return Err(Diagnostic::new(
                span,
                "try_run needs a function name and the address of the local that                  receives the result, as in try_run(f, arg, &result)",
            ));
        }
        let function = match &arguments[0] {
            Expr::Name(function) => function.clone(),
            other => {
                return Err(Diagnostic::new(
                    other.span(),
                    "the first argument of try_run must name a function declared in                      this translation unit",
                ));
            }
        };
        if self.local_visible(&function.text) {
            return Err(Diagnostic::new(
                function.span,
                format!(
                    "try_run names '{}', which is a local variable here, not a function",
                    function.text
                ),
            ));
        }
        let out = arguments.last().expect("checked two arguments");
        let Expr::Unary {
            op: UnaryOp::AddressOf,
            operand,
            ..
        } = out
        else {
            return Err(Diagnostic::new(
                out.span(),
                "the last argument of try_run must be the address of a local variable,                  as in &result",
            ));
        };
        let Expr::Name(local) = operand.as_ref() else {
            return Err(Diagnostic::new(
                operand.span(),
                "the last argument of try_run must be the address of a local variable,                  as in &result",
            ));
        };
        if !self.local_visible(&local.text) {
            return Err(Diagnostic::new(
                local.span,
                format!(
                    "'{}' is not a local variable of this function, so try_run cannot                      write the result through &{}",
                    local.text, local.text
                ),
            ));
        }
        let arity = arguments.len() - 2;
        let id = self.record_try_site(function.text.clone(), arity, span);
        arguments.remove(0);
        let callee = Name::new(format!("__shy_try_{id}"), name.span);
        Ok(Expr::Call {
            callee: Box::new(Expr::Name(callee)),
            type_args: Vec::new(),
            args: arguments,
            span,
        })
    }

    /// Captures the tokens between a pair of parentheses, brackets, or braces.
    /// Captures a token argument, dropping the `;` that closes it.
    ///
    /// A `;` written last closes the argument the way it closes a statement,
    /// so it is not one of the tokens the const fn receives.
    pub(crate) fn capture_token_argument(&mut self) -> Result<(Vec<Token>, Span), Diagnostic> {
        let (mut tokens, close) = self.capture_tokens()?;
        if matches!(
            tokens.last().map(|token| &token.kind),
            Some(TokenKind::Punct(punct)) if punct == ";"
        ) {
            tokens.pop();
        }
        Ok((tokens, close))
    }

    pub(crate) fn capture_tokens(&mut self) -> Result<(Vec<Token>, Span), Diagnostic> {
        let open = self.expect_punct("(")?;
        let mut depth = 1usize;
        let mut tokens = Vec::new();
        loop {
            if self.at_eof() {
                return Err(Diagnostic::new(open, "unterminated argument list"));
            }
            let token = self.bump();
            match &token.kind {
                TokenKind::Punct(punct) if punct == "(" || punct == "[" || punct == "{" => {
                    depth += 1;
                    tokens.push(token);
                }
                TokenKind::Punct(punct) if punct == ")" || punct == "]" || punct == "}" => {
                    depth -= 1;
                    if depth == 0 {
                        return Ok((tokens, token.span));
                    }
                    tokens.push(token);
                }
                _ => tokens.push(token),
            }
        }
    }

    /// keeps its wrapper call, because \`sscanf\` reads the C conversions itself.
    fn expand_format_macro(
        &mut self,
        name: Name,
        args: Vec<Expr>,
        span: Span,
    ) -> Result<Expr, Diagnostic> {
        let Some(format) = args.first() else {
            return Err(Diagnostic::new(
                span,
                format!("macro '{}!' takes a format string", name.text),
            ));
        };
        let literal = match format {
            Expr::String(group) => Some(string_value(group)?),
            _ => None,
        };
        match name.text.as_str() {
            "println" | "format" => {
                let Some(text) = literal else {
                    return Err(Diagnostic::new(
                        name.span,
                        format!("the format string of '{}!' must be a literal", name.text),
                    ));
                };
                let parts = split_format(&text, args.len() - 1, name.span)?;
                let actions = format_actions(parts, args[1..].to_vec(), name.span)?;
                // The macro keeps a definition to jump to: the library call its
                // expansion starts with.
                let entry = if name.text == "println" {
                    "fmt_line"
                } else {
                    "fmt_begin"
                };
                self.record_macro_site(&name.text.clone(), name.span, entry);
                Ok(self.format_statement(&name, actions, span))
            }
            "scanln" => {
                if args.len() - 1 > 8 {
                    return Err(Diagnostic::new(
                        span,
                        "macro 'scanln!' takes a format string and at most 8 arguments",
                    ));
                }
                if let Some(text) = &literal {
                    // \`scanln!\` hands its string to \`sscanf\`, so it counts C
                    // conversions instead of \`{}\` placeholders.
                    let placeholders = scan_placeholders(text);
                    if placeholders != args.len() - 1 {
                        return Err(placeholder_mismatch(
                            placeholders,
                            args.len() - 1,
                            name.span,
                        ));
                    }
                }
                let callee = Name::new(format!("scanln{}", args.len() - 1), name.span);
                self.record_macro_site(&name.text.clone(), name.span, &callee.text.clone());
                Ok(Expr::Call {
                    callee: Box::new(Expr::Name(callee)),
                    type_args: Vec::new(),
                    args,
                    span,
                })
            }
            other => Err(Diagnostic::new(
                name.span,
                format!("unknown macro '{other}!'"),
            )),
        }
    }

    /// The statement expression a \`println!\` or \`format!\` call stands for.
    fn format_statement(&mut self, name: &Name, actions: Vec<FormatAction>, span: Span) -> Expr {
        self.macro_index += 1;
        let out = Name::new(format!("__fmt_out_{}", self.macro_index), span);
        let mut stmts = Vec::with_capacity(actions.len() + 2);
        stmts.push(Stmt::Let(LetDecl {
            span,
            is_mut: true,
            name: out.clone(),
            init: plain_call("fmt_begin", Vec::new(), span),
        }));
        for action in actions {
            match action {
                FormatAction::Text(text) => {
                    if text.is_empty() {
                        continue;
                    }
                    stmts.push(Stmt::Expr {
                        expr: method_call(
                            &out,
                            "push_cstr",
                            vec![string_literal(&text, span)],
                            span,
                        ),
                        span,
                    });
                }
                FormatAction::Argument { debug, value } => {
                    let slot = value.span();
                    let render = if debug { "fmt_debug" } else { "fmt_display" };
                    stmts.push(Stmt::Expr {
                        expr: plain_call(render, vec![value, address_of(&out, span)], slot),
                        span: slot,
                    });
                }
            }
        }
        let tail = if name.text == "println" {
            plain_call("fmt_line", vec![address_of(&out, span)], span)
        } else {
            Expr::Name(out)
        };
        stmts.push(Stmt::Expr { expr: tail, span });
        Expr::StatementExpr {
            block: Block { span, stmts },
            span,
        }
    }

    fn postfix(&mut self) -> Result<Expr, Diagnostic> {
        let expr = self.primary()?;
        self.postfix_suffixes(expr)
    }

    fn postfix_suffixes(&mut self, mut expr: Expr) -> Result<Expr, Diagnostic> {
        // A `?` binds to the expression it follows. Do not let the suffixes
        // that would re-read the result as a call, an increment, or another
        // ternary be stolen immediately after it: `(f()?)(x)` opts into a
        // call, while `f()?(x)` stays a parse error.
        let mut fresh_try = false;
        loop {
            if fresh_try
                && (self.at_punct("(")
                    || self.at_punct("<")
                    || self.at_punct("++")
                    || self.at_punct("--")
                    || self.at_punct("?"))
            {
                break;
            }
            if self.at_punct("[") {
                self.bump();
                let index = self.expression()?;
                let close = self.expect_punct("]")?;
                let span = expr.span().join(close);
                expr = Expr::Index {
                    array: Box::new(expr),
                    index: Box::new(index),
                    span,
                };
                fresh_try = false;
            } else if self.at_punct("<") {
                // `f<int>(x)` is a generic call, but `a < b > (c)` is an
                // ordinary comparison. Parse speculatively and fall back to
                // the expression grammar when the shape does not fit.
                let Some(type_args) = self.try_call_type_args() else {
                    break;
                };
                let (args, close) = self.call_arguments()?;
                let span = expr.span().join(close);
                expr = Expr::Call {
                    callee: Box::new(expr),
                    type_args,
                    args,
                    span,
                };
                fresh_try = false;
            } else if self.at_punct("!") && self.at_punct_ahead(1, "(") {
                let Expr::Name(name) = &expr else {
                    return Err(Diagnostic::new(
                        expr.span(),
                        "a macro call needs a plain name before '!'",
                    ));
                };
                let name = name.clone();
                self.bump();
                let (args, close) = self.call_arguments()?;
                let span = expr.span().join(close);
                expr = self.expand_format_macro(name, args, span)?;
                fresh_try = false;
            } else if self.at_punct("(") {
                // `try_run(f, args..., &result)` asks the probe to run an
                // ordinary function. Rewrite it into the helper the probe image
                // generates for this site; the helper keeps the arguments and
                // the address of the local that receives the result.
                if let Expr::Name(name) = &expr
                    && name.text == "try_run"
                {
                    if !self.in_const_body {
                        return Err(Diagnostic::new(
                            name.span,
                            "try_run is only available inside a const fn body",
                        ));
                    }
                    let name = name.clone();
                    expr = self.try_run_call(name)?;
                    fresh_try = false;
                    continue;
                }
                // A call to a const fn is evaluated here, at compile time, and
                // the call site becomes the constant it produced. Inside a
                // const fn body the call stays: its arguments are that body's
                // locals, so only the evaluator can run it.
                if let Expr::Name(name) = &expr
                    && let Some(index) = self.const_fns.iter().position(|f| f.name == name.text)
                {
                    let span = expr.span();
                    if self.in_const_body {
                        self.record_const_call(index, span);
                    } else if self.probe_active() {
                        expr = self.probe_expr_call(index, span)?;
                        fresh_try = false;
                        continue;
                    } else {
                        expr = self.probe_expr_call(index, span)?;
                        fresh_try = false;
                        continue;
                    }
                }
                let (args, close) = self.call_arguments()?;
                let span = expr.span().join(close);
                expr = Expr::Call {
                    callee: Box::new(expr),
                    type_args: Vec::new(),
                    args,
                    span,
                };
                fresh_try = false;
            } else if self.at_punct(".") || self.at_punct("->") {
                let op = if self.at_punct(".") {
                    self.bump();
                    MemberOp::Dot
                } else {
                    self.bump();
                    MemberOp::Arrow
                };
                let name = self.expect_ident()?;
                let span = expr.span().join(name.span);
                expr = Expr::Member {
                    object: Box::new(expr),
                    name,
                    op,
                    span,
                };
                fresh_try = false;
            } else if self.at_punct("++") || self.at_punct("--") {
                let op = if self.at_punct("++") {
                    PostfixOp::Increment
                } else {
                    PostfixOp::Decrement
                };
                self.bump();
                let span = expr.span().join(self.last_span());
                expr = Expr::Postfix {
                    op,
                    operand: Box::new(expr),
                    span,
                };
                fresh_try = false;
            } else if self.at_punct("?") {
                // `a ? b : c` keeps its C meaning: the postfix operator only
                // applies when no middle expression and `:` follow the `?`.
                if self.question_starts_conditional() {
                    break;
                }
                self.bump();
                let span = expr.span().join(self.last_span());
                expr = Expr::Try {
                    value: Box::new(expr),
                    span,
                };
                fresh_try = true;
            } else {
                break;
            }
        }
        Ok(expr)
    }

    /// Whether the `?` at the cursor is the C conditional operator.
    ///
    /// The middle operand is a full expression, so a speculative parse that
    /// reaches a matching `:` settles the question. The cursor is restored
    /// either way; the conditional grammar then consumes the `?` itself.
    fn question_starts_conditional(&mut self) -> bool {
        if !self.at_expression_start(1) {
            return false;
        }
        self.probe(|parser| {
            // `probe` restores the cursor, so consuming the `?` here only lets
            // the middle expression be parsed from the token after it.
            parser.bump();
            let _ = parser.expression()?;
            parser.expect_punct(":")?;
            Ok(())
        })
        .is_some()
    }

    /// Speculatively parses `<...>` only when a call's `(` follows it.
    fn try_call_type_args(&mut self) -> Option<Vec<GenericArg>> {
        self.try_parse(|parser| {
            let type_args = parser.generic_args()?;
            if !parser.at_punct("(") {
                return Err(Diagnostic::new(parser.here(), "not a generic call"));
            }
            Ok(type_args)
        })
    }

    /// Parses `( expression, ... )` after a callee and returns its arguments
    /// plus the closing parenthesis span.
    pub(crate) fn call_arguments(&mut self) -> Result<(Vec<Expr>, Span), Diagnostic> {
        self.expect_punct("(")?;
        let mut args = Vec::new();
        if !self.at_punct(")") {
            loop {
                args.push(self.assignment()?);
                if !self.eat_punct(",") {
                    break;
                }
            }
        }
        let close = self.expect_punct(")")?;
        Ok((args, close))
    }

    fn primary(&mut self) -> Result<Expr, Diagnostic> {
        let token = self.peek().clone();
        match token.kind {
            TokenKind::Number(text) => {
                self.bump();
                Ok(Expr::Number(NumberLit {
                    text,
                    span: token.span,
                }))
            }
            TokenKind::CharLiteral { prefix, raw } => {
                self.bump();
                Ok(Expr::Char(CharLit {
                    prefix,
                    raw,
                    span: token.span,
                }))
            }
            TokenKind::StringLiteral { .. } => Ok(Expr::String(self.string_group()?)),
            TokenKind::Ident(text) => self.name_expr(text, token.span),
            TokenKind::Punct(ref punct) if punct == "(" => {
                if self.at_punct_ahead(1, "{") {
                    let open = self.bump().span;
                    let block = self.block()?;
                    let close = self.expect_punct(")")?;
                    return Ok(Expr::StatementExpr {
                        block,
                        span: open.join(close),
                    });
                }
                self.bump();
                let expr = self.expression()?;
                self.expect_punct(")")?;
                Ok(expr)
            }
            _ => Err(self.expected("an expression")),
        }
    }

    fn name_expr(&mut self, text: String, span: Span) -> Result<Expr, Diagnostic> {
        // The two type-operand builtins keep their arguments as type names.
        if matches!(
            text.as_str(),
            "__builtin_reg_class" | "__builtin_types_compatible_p"
        ) && self.at_punct_ahead(1, "(")
        {
            self.bump();
            self.expect_punct("(")?;
            if self.at_punct(")") {
                return Err(Diagnostic::new(
                    self.here(),
                    format!("'{text}' needs a type argument"),
                ));
            }
            let mut type_args = Vec::new();
            loop {
                type_args.push(self.type_name()?);
                if !self.eat_punct(",") {
                    break;
                }
            }
            let close = self.expect_punct(")")?;
            return Ok(Expr::BuiltinTypeCall {
                name: Name::new(text, span),
                type_args,
                span: span.join(close),
            });
        }

        self.bump();
        let name = Name::new(text, span);
        // `Option<int>::Some` is a scoped name whose qualifier takes generic
        // arguments. Parse the arguments speculatively and keep them only when
        // `::` really follows, so `a < b > c` stays an ordinary comparison.
        if self.at_punct("<")
            && let Some(args) = self.try_parse(|parser| {
                let args = parser.generic_args()?;
                if !parser.at_punct("::") {
                    return Err(Diagnostic::new(parser.here(), "not a generic scoped name"));
                }
                Ok(args)
            })
        {
            self.expect_punct("::")?;
            let member = self.expect_ident()?;
            if self.at_punct("::") {
                return Err(Diagnostic::new(
                    self.here(),
                    "scoped names with more than two segments are not supported",
                ));
            }
            let span = name.span.join(member.span);
            return Ok(Expr::GenericScopedName {
                qualifier: name,
                args,
                name: member,
                span,
            });
        }
        if self.eat_punct("::") {
            let method = self.expect_ident()?;
            if self.at_punct("::") {
                return Err(Diagnostic::new(
                    self.here(),
                    "scoped names with more than two segments are not supported",
                ));
            }
            let span = name.span.join(method.span);
            return Ok(Expr::ScopedName {
                qualifier: name,
                name: method,
                span,
            });
        }
        Ok(Expr::Name(name))
    }

    pub(crate) fn string_group(&mut self) -> Result<StringLitGroup, Diagnostic> {
        let start = self.here();
        let parts = self.string_parts()?;
        Ok(StringLitGroup {
            parts,
            span: self.finish(start),
        })
    }

    /// One or more adjacent string literals, kept as separate parts.
    pub(crate) fn string_parts(&mut self) -> Result<Vec<StringLit>, Diagnostic> {
        let mut parts = Vec::new();
        loop {
            let token = self.peek();
            let (prefix, raw) = match &token.kind {
                TokenKind::StringLiteral { prefix, raw } => (prefix.clone(), raw.clone()),
                _ => break,
            };
            let span = token.span;
            self.bump();
            parts.push(StringLit { prefix, raw, span });
        }
        if parts.is_empty() {
            return Err(self.expected("a string literal"));
        }
        Ok(parts)
    }
}
/// One piece of a compiled format string.
enum FormatPart {
    /// Literal text, escapes decoded and braces resolved.
    Text(String),
    /// A placeholder, rendered with Display or Debug.
    Slot {
        debug: bool,
        /// The variable a \`{name}\` placeholder inserts, if it names one.
        name: Option<String>,
    },
}

/// One step of the statement expression a format macro compiles to.
enum FormatAction {
    Text(String),
    Argument { debug: bool, value: Expr },
}

/// The text a string literal stands for, escapes decoded.
fn string_value(group: &StringLitGroup) -> Result<String, Diagnostic> {
    let mut text = String::new();
    for part in &group.parts {
        let value = crate::literal::string(&part.prefix, &part.raw, part.span)?;
        text.push_str(&String::from_utf8_lossy(&value.bytes));
    }
    Ok(text)
}

/// Splits a literal format string into text and placeholders.
///
/// \`{{\`/\`}}\` are resolved here, so nothing downstream looks for a brace, and
/// the placeholder count is checked against the arguments the call supplied.
fn split_format(text: &str, arguments: usize, span: Span) -> Result<Vec<FormatPart>, Diagnostic> {
    let mut parts: Vec<FormatPart> = Vec::new();
    let mut pending = String::new();
    let mut placeholders = 0;
    let mut characters = text.chars().peekable();
    while let Some(character) = characters.next() {
        match character {
            '{' if characters.peek() == Some(&'{') => {
                characters.next();
                pending.push('{');
            }
            '}' if characters.peek() == Some(&'}') => {
                characters.next();
                pending.push('}');
            }
            '{' => {
                let mut inner = String::new();
                loop {
                    match characters.next() {
                        Some('}') => break,
                        Some(character) => inner.push(character),
                        None => {
                            return Err(Diagnostic::new(span, "format string has an unclosed '{'"));
                        }
                    }
                }
                let (name, debug) = format_spec(&inner, span)?;
                if !pending.is_empty() {
                    parts.push(FormatPart::Text(std::mem::take(&mut pending)));
                }
                // A `{name}` placeholder inserts a variable that is already in
                // scope, so only the positional ones take an argument.
                if name.is_none() {
                    placeholders += 1;
                }
                parts.push(FormatPart::Slot { debug, name });
            }
            character => pending.push(character),
        }
    }
    if !pending.is_empty() {
        parts.push(FormatPart::Text(pending));
    }
    if placeholders != arguments {
        return Err(placeholder_mismatch(placeholders, arguments, span));
    }
    Ok(parts)
}

/// The variable and Debug flag of one \`{...}\` placeholder.
fn format_spec(inner: &str, span: Span) -> Result<(Option<String>, bool), Diagnostic> {
    match inner {
        "" => return Ok((None, false)),
        ":?" => return Ok((None, true)),
        _ => {}
    }
    let (name, debug) = match inner.strip_suffix(":?") {
        Some(name) => (name, true),
        None => (inner, false),
    };
    let valid = !name.is_empty() && name.chars().all(|c| c.is_alphanumeric() || c == '_');
    if !valid {
        return Err(Diagnostic::new(
            span,
            format!("unknown format specifier '{{{inner}}}'"),
        ));
    }
    Ok((Some(name.to_string()), debug))
}

/// Pairs each placeholder with its argument, in order.
fn format_actions(
    parts: Vec<FormatPart>,
    values: Vec<Expr>,
    span: Span,
) -> Result<Vec<FormatAction>, Diagnostic> {
    let mut values = values.into_iter();
    let mut actions = Vec::with_capacity(parts.len());
    for part in parts {
        match part {
            FormatPart::Text(text) => actions.push(FormatAction::Text(text)),
            FormatPart::Slot { debug, name } => {
                let value = match name {
                    // \`{name}\` inserts the variable of that name, so no
                    // argument was written for it.
                    Some(variable) => Expr::Name(Name::new(variable, span)),
                    None => match values.next() {
                        Some(value) => value,
                        None => {
                            return Err(Diagnostic::new(
                                span,
                                "format string has more placeholders than arguments",
                            ));
                        }
                    },
                };
                actions.push(FormatAction::Argument { debug, value });
            }
        }
    }
    Ok(actions)
}

/// A call to a plain function name.
fn plain_call(name: &str, args: Vec<Expr>, span: Span) -> Expr {
    Expr::Call {
        callee: Box::new(Expr::Name(Name::new(name.to_string(), span))),
        type_args: Vec::new(),
        args,
        span,
    }
}

/// \`object.method(args)\` on a local name.
fn method_call(object: &Name, method: &str, args: Vec<Expr>, span: Span) -> Expr {
    Expr::Call {
        callee: Box::new(Expr::Member {
            object: Box::new(Expr::Name(object.clone())),
            name: Name::new(method.to_string(), span),
            op: MemberOp::Dot,
            span,
        }),
        type_args: Vec::new(),
        args,
        span,
    }
}

/// \`&name\`.
fn address_of(name: &Name, span: Span) -> Expr {
    Expr::Unary {
        op: UnaryOp::AddressOf,
        operand: Box::new(Expr::Name(name.clone())),
        span,
    }
}

/// One string literal expression, escapes re-applied.
fn string_literal(text: &str, span: Span) -> Expr {
    Expr::String(StringLitGroup {
        parts: vec![StringLit {
            prefix: String::new(),
            raw: escape_literal(text),
            span,
        }],
        span,
    })
}

/// Counts the C conversions a `scanf` format performs.
///
/// `%%` is a literal percent sign, and everything after a conversion's width
/// and length modifiers still counts as one conversion.
fn scan_placeholders(text: &str) -> usize {
    let bytes = text.as_bytes();
    let mut count = 0;
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'%' {
            index += 1;
            continue;
        }
        if bytes.get(index + 1) == Some(&b'%') {
            index += 2;
            continue;
        }
        count += 1;
        index += 1;
        // Skip the conversion's own characters: flags, width, length, specifier.
        while index < bytes.len() && !bytes[index].is_ascii_alphabetic() {
            index += 1;
        }
        index += 1;
    }
    count
}

/// The diagnostic for a format string whose placeholders and arguments differ.
fn placeholder_mismatch(placeholders: usize, arguments: usize, span: Span) -> Diagnostic {
    Diagnostic::new(
        span,
        format!(
            "format string has {placeholders} placeholder(s) but {arguments} argument(s) were given"
        ),
    )
}

/// A literal expression for `value`.
///
/// The lexer only spells unsigned literals, so a negative value is a minus
/// applied to its magnitude: `NumberLit { text: "-1" }` has no digits for the
/// literal parser and would be rejected.
fn number(value: i64, span: Span) -> Expr {
    let literal = Expr::Number(NumberLit {
        text: value.unsigned_abs().to_string(),
        span,
    });
    if value < 0 {
        Expr::Unary {
            op: UnaryOp::Minus,
            operand: Box::new(literal),
            span,
        }
    } else {
        literal
    }
}

/// Escapes a computed string so it can be written back as a literal's raw text.
fn escape_literal(text: &str) -> String {
    let mut escaped = String::new();
    for byte in text.bytes() {
        match byte {
            b'"' => escaped.push_str("\\\""),
            b'\\' => escaped.push_str("\\\\"),
            b'\n' => escaped.push_str("\\n"),
            b'\t' => escaped.push_str("\\t"),
            b'\r' => escaped.push_str("\\r"),
            0x20..=0x7e => escaped.push(byte as char),
            other => escaped.push_str(&format!("\\x{other:02x}")),
        }
    }
    escaped
}

/// Parses a token sequence returned by a const fn as one expression.
///
/// The result must be a complete expression: a fragment that leaves trailing
/// tokens behind would silently swallow the surrounding operator, so it is
/// rejected instead.
fn parse_fragment(tokens: Vec<Token>, span: Span, parent: &mut Parser) -> Result<Expr, Diagnostic> {
    if tokens.is_empty() {
        return Err(Diagnostic::new(span, "a const fn returned no tokens"));
    }
    let mut stream = tokens;
    stream.push(Token {
        kind: TokenKind::Eof,
        span,
        at_line_start: false,
        leading_space: false,
    });
    let mut parser = Parser::new(stream, false);
    parser.scopes[0].typedefs = parent.scopes[0].typedefs.clone();
    parser.scopes[0].tags = parent.scopes[0].tags.clone();
    parser.scopes[0].traits = parent.scopes[0].traits.clone();
    parser.probe = parent.probe.clone();
    parser.defer_body_calls = parent.defer_body_calls;
    parser.body_stack = parent.body_stack.clone();
    parser.function_body_names = parent.function_body_names.clone();
    let expr = parser.expression()?;
    if !parser.at_eof() {
        return Err(Diagnostic::new(
            parser.here(),
            "tokens returned by a const fn do not form one expression",
        ));
    }
    parent.bodies_deferred |= parser.bodies_deferred;
    parent
        .deferred_body_functions
        .extend(parser.deferred_body_functions.iter().cloned());
    Ok(expr)
}
