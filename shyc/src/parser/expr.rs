//! Expression syntax: precedence climbing plus C and Shy postfix constructs.
//!
//! The tree records binding, not meaning: `a + b * c` is a sum of `a` and a
//! product. `Type::method` stays a [`crate::ast::Expr::ScopedName`], and
//! `p.field` on a pointer stays a dot member, because Shy's auto-dereference is
//! semantic work.

use crate::ast::{
    AssignOp, BinaryOp, CharLit, Expr, GenericArg, MemberOp, Name, NumberLit, PostfixOp, StringLit,
    StringLitGroup, UnaryOp,
};
use crate::lexer::{Token, TokenKind};

use super::comptime::{Evaluator, ParamKind, Value};
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

    /// Parses one const fn call site and replaces it with the call's result.
    ///
    /// A function with a single `Tokens` parameter captures everything inside
    /// the parentheses as written; every other signature parses ordinary
    /// expressions. A result that is a token sequence is parsed back into the
    /// expression the call stands for.
    fn const_call_site(&mut self, index: usize, start: Span) -> Result<Expr, Diagnostic> {
        self.record_const_call(index, start);
        let params = self.const_fns[index].params.clone();
        let tokens_parameter = params.len() == 1 && params[0].1 == ParamKind::Tokens;
        if tokens_parameter {
            let (tokens, close) = self.capture_tokens()?;
            let span = start.join(close);
            let value = self
                .call_const(index, vec![Value::Tokens(tokens)], span)?;
            return self.constant_expression(value, span, index);
        }
        let (args, close) = self.call_arguments()?;
        let span = start.join(close);
        let value = self.const_call(index, args, span)?;
        self.constant_expression(value, span, index)
    }

    /// Wraps an expression result, parsing a token result as an expression.
    ///
    /// A parse failure names the const fn that produced the tokens, since the
    /// error points into generated text the programmer never wrote.
    fn constant_expression(
        &self,
        value: Value,
        span: Span,
        index: usize,
    ) -> Result<Expr, Diagnostic> {
        match value {
            Value::Tokens(tokens) => {
                parse_fragment(tokens, span, self).map_err(|error| self.fragment_error(error, index))
            }
            other => constant(other, span),
        }
    }

    /// Runs one const fn with already prepared argument values.
    pub(crate) fn call_const(
        &self,
        index: usize,
        values: Vec<Value>,
        span: Span,
    ) -> Result<Value, Diagnostic> {
        let function = &self.const_fns[index];
        let mut evaluator = Evaluator::new(&self.const_fns);
        evaluator.call(index, &values, span).map_err(|error| {
            Diagnostic::new(
                span,
                format!("in const fn '{}': {}", function.name, error.message),
            )
        })
    }

    /// Captures the tokens between a pair of parentheses, brackets, or braces.
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

    /// Evaluates a call to `const fn` and turns the result into a value.
    fn const_call(&self, index: usize, args: Vec<Expr>, span: Span) -> Result<Value, Diagnostic> {
        let function = &self.const_fns[index];
        // A body call is recorded so only the bodies that depend on the settled
        // declarations are re-parsed.
        self.saw_body_call.set(true);
        if self.body_world.is_none()
            && let Some(span) = self.function_bodies.last().copied()
        {
            let mut calls = self.body_calls.borrow_mut();
            if !calls.iter().any(|recorded| recorded.start == span.start) {
                calls.push(span);
            }
        }
        let mut evaluator = match &self.body_world {
            Some(world) => Evaluator::with_world(&self.const_fns, world),
            None => Evaluator::new(&self.const_fns),
        };
        let mut values = Vec::new();
        for argument in &args {
            let value = evaluator.eval(argument).map_err(|error| {
                Diagnostic::new(
                    argument.span(),
                    format!(
                        "argument of const fn '{}' is not a compile-time constant: {}",
                        function.name, error.message
                    ),
                )
            })?;
            values.push(value);
        }
        evaluator.call(index, &values, span)
    }

    fn expand_format_macro(
        &mut self,
        name: Name,
        args: Vec<Expr>,
        span: Span,
    ) -> Result<Expr, Diagnostic> {
        let prefix = match name.text.as_str() {
            "println" => "println",
            "format" => "format",
            "scanln" => "scanln",
            other => {
                return Err(Diagnostic::new(
                    name.span,
                    format!("unknown macro '{other}!'"),
                ));
            }
        };
        if args.is_empty() || args.len() - 1 > 8 {
            return Err(Diagnostic::new(
                span,
                format!(
                    "macro '{}!' takes a format string and at most 8 arguments",
                    name.text
                ),
            ));
        }
        let mut format = args[0].clone();
        let mut values = args[1..].to_vec();
        if let Some(text) = literal_text(&format) {
            if name.text == "scanln" {
                // `scanln!` hands its string to `sscanf`, so it counts C
                // conversions instead of `{}` placeholders.
                let placeholders = scan_placeholders(&text);
                if placeholders != values.len() {
                    return Err(placeholder_mismatch(placeholders, values.len(), name.span));
                }
            } else {
                let (rewritten, specs) = parse_format(&text, name.span)?;
                if specs.len() > 8 {
                    return Err(Diagnostic::new(
                        span,
                        format!(
                            "macro '{}!' takes a format string and at most 8 arguments",
                            name.text
                        ),
                    ));
                }
                let mut given = values.into_iter();
                let mut bound = Vec::with_capacity(specs.len());
                for spec in &specs {
                    match &spec.name {
                        // `{name}` inserts the variable of that name, so no
                        // argument was written for it.
                        Some(variable) => {
                            bound.push(Expr::Name(Name::new(variable.clone(), name.span)))
                        }
                        None => match given.next() {
                            Some(argument) => bound.push(argument),
                            None => {
                                return Err(placeholder_mismatch(
                                    specs.len(),
                                    args.len() - 1,
                                    name.span,
                                ));
                            }
                        },
                    }
                }
                if given.next().is_some() {
                    return Err(placeholder_mismatch(specs.len(), args.len() - 1, name.span));
                }
                if specs.iter().any(|spec| spec.name.is_some()) {
                    // The runtime formatter only understands positional
                    // placeholders, so the named ones become `{}` or `{:?}`.
                    format = Expr::String(StringLitGroup {
                        parts: vec![StringLit {
                            prefix: String::new(),
                            raw: rewritten,
                            span: name.span,
                        }],
                        span: name.span,
                    });
                }
                values = bound;
            }
        }
        let callee = Name::new(format!("{prefix}{}", values.len()), name.span);
        let expansion = callee.text.clone();
        self.record_macro_site(&name.text, name.span, &expansion);
        let mut arguments = Vec::with_capacity(values.len() + 1);
        arguments.push(format);
        arguments.extend(values);
        Ok(Expr::Call {
            callee: Box::new(Expr::Name(callee)),
            type_args: Vec::new(),
            args: arguments,
            span,
        })
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
                // A call to a const fn is evaluated here, at compile time, and
                // the call site becomes the constant it produced.
                if let Expr::Name(name) = &expr
                    && let Some(index) = self.const_fns.iter().position(|f| f.name == name.text)
                {
                    let span = expr.span();
                    expr = self.const_call_site(index, span)?;
                    fresh_try = false;
                    continue;
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
    fn call_arguments(&mut self) -> Result<(Vec<Expr>, Span), Diagnostic> {
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
/// The text of a string literal expression, with escapes left as written.
///
/// Escapes never produce a brace, so scanning the raw text is enough for
/// counting placeholders.
fn literal_text(expr: &Expr) -> Option<String> {
    let Expr::String(group) = expr else {
        return None;
    };
    Some(group.parts.iter().map(|part| part.raw.as_str()).collect())
}

/// One placeholder of a literal format string.
struct FormatSpec {
    /// The variable a `{name}` placeholder inserts, if it names one.
    name: Option<String>,
}

/// Reads a literal format string, rewriting named placeholders.
///
/// Returns the string the runtime formatter should see (every placeholder
/// positional) and one entry per placeholder. `{{name}}` is not a placeholder:
/// escaped braces stay literal.
fn parse_format(text: &str, span: Span) -> Result<(String, Vec<FormatSpec>), Diagnostic> {
    let characters: Vec<char> = text.chars().collect();
    let mut rewritten = String::new();
    let mut specs = Vec::new();
    let mut index = 0;
    while index < characters.len() {
        let character = characters[index];
        if character == '{' && characters.get(index + 1) == Some(&'{') {
            rewritten.push_str("{{");
            index += 2;
            continue;
        }
        if character == '}' && characters.get(index + 1) == Some(&'}') {
            rewritten.push_str("}}");
            index += 2;
            continue;
        }
        if character != '{' {
            rewritten.push(character);
            index += 1;
            continue;
        }
        let Some(offset) = characters[index + 1..].iter().position(|c| *c == '}') else {
            return Err(Diagnostic::new(span, "format string has an unclosed '{'"));
        };
        let inner: String = characters[index + 1..index + 1 + offset].iter().collect();
        let (name, debug) = match inner.as_str() {
            "" => (None, false),
            ":?" => (None, true),
            other => {
                let (name, debug) = match other.strip_suffix(":?") {
                    Some(name) => (name, true),
                    None => (other, false),
                };
                let valid = !name.is_empty()
                    && name
                        .chars()
                        .all(|c| c.is_alphanumeric() || c == '_');
                if !valid {
                    return Err(Diagnostic::new(
                        span,
                        format!("unknown format specifier '{{{inner}}}'"),
                    ));
                }
                (Some(name.to_string()), debug)
            }
        };
        rewritten.push_str(if debug { "{:?}" } else { "{}" });
        specs.push(FormatSpec { name });
        index = index + 1 + offset + 1;
    }
    Ok((rewritten, specs))
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

/// Turns a compile-time value back into a literal expression.
fn constant(value: Value, span: Span) -> Result<Expr, Diagnostic> {
    match value {
        Value::Int(int) => Ok(Expr::Number(NumberLit {
            text: int.to_string(),
            span,
        })),
        Value::Str(text) => Ok(Expr::String(StringLitGroup {
            parts: vec![StringLit {
                prefix: String::new(),
                raw: escape_literal(&text),
                span,
            }],
            span,
        })),
        Value::Tokens(_) => Err(Diagnostic::new(
            span,
            "a token sequence can only be used where an expression is expected",
        )),
        Value::Unit => Err(Diagnostic::new(
            span,
            "a const fn used as a value must return a value",
        )),
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
fn parse_fragment(tokens: Vec<Token>, span: Span, parent: &Parser) -> Result<Expr, Diagnostic> {
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
    let expr = parser.expression()?;
    if !parser.at_eof() {
        return Err(Diagnostic::new(
            parser.here(),
            "tokens returned by a const fn do not form one expression",
        ));
    }
    Ok(expr)
}