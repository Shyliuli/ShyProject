//! Statements, blocks, labels, and inline assembly.

use crate::ast::{
    AsmStmt, Block, Declarator, DerivedDeclarator, Expr, ForInit, LetDecl, Name, Stmt,
};
use crate::lexer::TokenKind;
use crate::source::{Diagnostic, Span};

use super::Parser;
use super::cursor::is_keyword;
use super::probe::{Answer, Place, ProbeRequest};

impl Parser {
    /// One statement, including a declaration used as a statement.
    ///
    /// A compile-time generator may stand here too, but this position holds
    /// one statement and cannot represent a longer expansion.
    pub(crate) fn statement(&mut self) -> Result<Stmt, Diagnostic> {
        let mut statements = self.statement_or_expansion()?;
        match statements.len() {
            0 => Ok(Stmt::Empty(self.here())),
            1 => Ok(statements.pop().expect("one statement")),
            count => Err(Diagnostic::new(
                self.here(),
                format!(
                    "a compile-time generator produced {count} statements where one statement is required; wrap it in a block"
                ),
            )),
        }
    }

    /// One statement, or the zero or more statements a compile-time generator
    /// produces. Blocks and generated statement lists share this path.
    fn statement_or_expansion(&mut self) -> Result<Vec<Stmt>, Diagnostic> {
        if let Some(expanded) = self.expand_statement_list()? {
            // The generator call's own semicolon closes the invocation; it is
            // not an extra empty statement.
            if self.at_punct(";") {
                self.bump();
            }
            return Ok(expanded);
        }
        Ok(vec![self.plain_statement()?])
    }

    /// Parses one written statement without expanding a generator.
    fn plain_statement(&mut self) -> Result<Stmt, Diagnostic> {
        if self.at_punct("#") {
            return self.attributed_statement();
        }
        if self.at_punct("{") {
            return Ok(Stmt::Block(self.block()?));
        }
        if self.at_punct("asm!") {
            return Ok(Stmt::Asm(self.asm_statement()?));
        }
        if self.at_punct(";") {
            let span = self.bump().span;
            return Ok(Stmt::Empty(span));
        }
        if self.at_defer_declaration() {
            return self.defer_statement();
        }
        if self.at_let_declaration() {
            return self.let_statement();
        }
        if self.at_ident("trait") {
            return Err(Diagnostic::new(
                self.here(),
                "trait declarations are only allowed at file scope",
            ));
        }

        if self.at_ident("if") {
            return self.if_statement();
        }
        if self.at_ident("while") {
            return self.while_statement();
        }
        if self.at_ident("do") {
            return self.do_while_statement();
        }
        if self.at_ident("for") {
            return self.for_statement();
        }
        if self.at_ident("switch") {
            return self.switch_statement();
        }
        if self.at_ident("case") {
            return self.case_statement();
        }
        if self.at_ident("default") {
            return self.default_statement();
        }
        // `_:` is the second spelling of `default:`. It has to be recognised
        // before the generic `label:` path below, which would otherwise claim
        // `_` as a label name.
        if self.at_ident("_") && self.at_punct_ahead(1, ":") {
            return self.default_statement();
        }
        if self.at_ident("break") {
            let start = self.bump().span;
            let semicolon = self.expect_punct(";")?;
            return Ok(Stmt::Break(start.join(semicolon)));
        }
        if self.at_ident("continue") {
            let start = self.bump().span;
            let semicolon = self.expect_punct(";")?;
            return Ok(Stmt::Continue(start.join(semicolon)));
        }
        if self.at_ident("return") {
            return self.return_statement();
        }
        if self.at_ident("goto") {
            return self.goto_statement();
        }
        if self.at_ident("asm") {
            return Ok(Stmt::Asm(self.asm_statement()?));
        }

        // `label:` uses its own namespace, so a label may have the same
        // spelling as a type name.
        if let Some(word) = self.ident_here_owned()
            && !is_keyword(&word)
            && self.at_punct_ahead(1, ":")
        {
            let name = self.expect_ident()?;
            let colon = self.expect_punct(":")?;
            let inner = self.statement()?;
            let span = name.span.join(colon).join(inner.span());
            return Ok(Stmt::Label {
                name,
                stmt: Box::new(inner),
                span,
            });
        }

        // `Type::method(...)` is a call whose callee happens to be a type
        // name, not a declaration: a declaration never has `::` after its
        // specifiers.
        if self.starts_type_name(0) && !self.at_punct_ahead(1, "::") {
            return Ok(Stmt::Decl(self.declaration()?));
        }

        let start = self.here();
        let expr = self.expression()?;
        let semicolon = self.expect_punct(";")?;
        Ok(Stmt::Expr {
            expr,
            span: start.join(semicolon),
        })
    }

    /// Whether the cursor starts a Shy `let` declaration.
    ///
    /// `let` and `mut` are soft keywords: a C typedef or object with either
    /// spelling keeps its ordinary meaning, and only the shape `let name` or
    /// `let mut name` starts the inferred form. A visible typedef named `let`
    /// wins, so existing C declarations keep parsing as declarations.
    pub(crate) fn at_let_declaration(&self) -> bool {
        self.at_let_declaration_at(0)
    }

    /// Whether the cursor `at` tokens ahead starts a `let` declaration.
    fn at_let_declaration_at(&self, at: usize) -> bool {
        if !self.at_ident_ahead(at, "let") {
            return false;
        }
        if at == 0 && self.is_type_name("let") {
            return false;
        }
        if self.at_ident_ahead(at + 1, "mut") {
            return matches!(&self.peek_ahead(at + 2).kind, TokenKind::Ident(name) if !is_keyword(name));
        }
        matches!(&self.peek_ahead(at + 1).kind, TokenKind::Ident(name) if !is_keyword(name))
    }

    /// Whether the cursor starts a `defer` declaration.
    ///
    /// `defer` is a soft keyword, like `let`: it only starts a declaration when
    /// a `let` or a type name follows, so an ordinary C identifier keeps its
    /// meaning.
    fn at_defer_declaration(&self) -> bool {
        self.at_ident("defer") && (self.at_let_declaration_at(1) || self.starts_type_name(1))
    }

    /// `defer T name = ...;` or `defer let name = ...;`.
    fn defer_statement(&mut self) -> Result<Stmt, Diagnostic> {
        let start = self.bump().span;
        if self.at_let_declaration() {
            let Stmt::Let(mut declaration) = self.let_statement()? else {
                unreachable!("let_statement returns Stmt::Let");
            };
            declaration.is_defer = true;
            declaration.span = start.join(declaration.span);
            return Ok(Stmt::Let(declaration));
        }
        let mut declaration = self.declaration()?;
        declaration.is_defer = true;
        declaration.span = start.join(declaration.span);
        Ok(Stmt::Decl(declaration))
    }

    /// `#[allow(defer_copy)] statement`.
    fn attributed_statement(&mut self) -> Result<Stmt, Diagnostic> {
        let start = self.bump().span;
        let (name, argument, span) = self.attribute()?;
        if name != "allow" || argument.as_deref() != Some("defer_copy") {
            return Err(Diagnostic::new(span, "expected #[allow(defer_copy)]"));
        }
        let inner = self.statement()?;
        Ok(Stmt::AllowDeferCopy {
            span: start.join(inner.span()),
            stmt: Box::new(inner),
        })
    }

    /// `let [mut] name = expr;`; the type is inferred from `expr` in lowering.
    fn let_statement(&mut self) -> Result<Stmt, Diagnostic> {
        let start = self.bump().span;
        let is_mut = self.eat_ident("mut");
        let name = self.expect_ident()?;
        if !self.at_punct("=") {
            return Err(Diagnostic::new(
                self.here(),
                "a `let` declaration needs an initializer",
            ));
        }
        self.bump();
        let init = self.expression()?;
        let semicolon = self.expect_punct(";")?;
        self.declare_value(&name.text);
        Ok(Stmt::Let(LetDecl {
            span: start.join(semicolon),
            is_mut,
            is_defer: false,
            name,
            init,
        }))
    }

    /// A `{ ... }` block, opening a fresh name scope.
    pub(crate) fn block(&mut self) -> Result<Block, Diagnostic> {
        self.block_with_params(None)
    }

    /// A block that is a function body: the parameters are declared in the
    /// same scope as the outermost local declarations.
    pub(crate) fn block_with_params(
        &mut self,
        params: Option<&Declarator>,
    ) -> Result<Block, Diagnostic> {
        let open = self.expect_punct("{")?;
        // The body span is recorded so a second pass can re-parse just the
        // bodies that evaluated a const fn. A function body is the outermost
        // one, even when the call sits inside a nested block.
        self.body_stack.push(open);
        let is_function_body = params.is_some();
        if is_function_body {
            self.function_bodies.push(open);
        }
        self.push_scope();
        if let Some(declarator) = params
            && let Some(DerivedDeclarator::Function(function)) = declarator.derived.first()
        {
            for param in &function.params {
                if let Some(name) = param.declarator.as_ref().and_then(|d| d.name.as_ref()) {
                    self.declare_value(&name.text);
                }
            }
        }

        let mut stmts = Vec::new();
        while !self.at_punct("}") {
            if self.at_eof() {
                return Err(Diagnostic::new(self.here(), "unterminated block"));
            }
            stmts.extend(self.statement_or_expansion()?);
        }
        let close = self.expect_punct("}")?;
        self.pop_scope();
        self.body_stack.pop();
        if is_function_body {
            self.function_bodies.pop();
        }
        Ok(Block {
            span: open.join(close),
            stmts,
        })
    }

    fn if_statement(&mut self) -> Result<Stmt, Diagnostic> {
        let start = self.bump().span;
        self.expect_punct("(")?;
        let condition = self.expression()?;
        self.expect_punct(")")?;
        let then_branch = Box::new(self.statement()?);
        let else_branch = if self.eat_ident("else") {
            Some(Box::new(self.statement()?))
        } else {
            None
        };
        Ok(Stmt::If {
            condition,
            then_branch,
            else_branch,
            span: self.finish(start),
        })
    }

    fn while_statement(&mut self) -> Result<Stmt, Diagnostic> {
        let start = self.bump().span;
        self.expect_punct("(")?;
        let condition = self.expression()?;
        self.expect_punct(")")?;
        let body = Box::new(self.statement()?);
        Ok(Stmt::While {
            condition,
            body,
            span: self.finish(start),
        })
    }

    fn do_while_statement(&mut self) -> Result<Stmt, Diagnostic> {
        let start = self.bump().span;
        let body = Box::new(self.statement()?);
        self.expect_keyword("while")?;
        self.expect_punct("(")?;
        let condition = self.expression()?;
        self.expect_punct(")")?;
        let semicolon = self.expect_punct(";")?;
        Ok(Stmt::DoWhile {
            body,
            condition,
            span: start.join(semicolon),
        })
    }

    fn for_statement(&mut self) -> Result<Stmt, Diagnostic> {
        let start = self.bump().span;
        // A name declared in the first clause belongs to the loop, as in C.
        self.push_scope();
        let result = self.for_clauses_and_body(start);
        self.pop_scope();
        result
    }

    fn for_clauses_and_body(&mut self, start: Span) -> Result<Stmt, Diagnostic> {
        self.expect_punct("(")?;

        let init = if self.eat_punct(";") {
            None
        } else if self.at_let_declaration() {
            let Stmt::Let(decl) = self.let_statement()? else {
                unreachable!("let_statement returns Stmt::Let");
            };
            Some(Box::new(ForInit::Let(decl)))
        } else if self.starts_type_name(0) {
            Some(Box::new(ForInit::Decl(self.declaration()?)))
        } else {
            let expr = self.expression()?;
            self.expect_punct(";")?;
            Some(Box::new(ForInit::Expr(expr)))
        };

        let condition = if self.at_punct(";") {
            None
        } else {
            Some(Box::new(self.expression()?))
        };
        self.expect_punct(";")?;

        let step = if self.at_punct(")") {
            None
        } else {
            Some(Box::new(self.expression()?))
        };
        self.expect_punct(")")?;

        let body = Box::new(self.statement()?);
        Ok(Stmt::For {
            init,
            condition,
            step,
            body,
            span: self.finish(start),
        })
    }

    fn switch_statement(&mut self) -> Result<Stmt, Diagnostic> {
        let start = self.bump().span;
        self.expect_punct("(")?;
        let condition = self.expression()?;
        self.expect_punct(")")?;
        let body = Box::new(self.statement()?);
        Ok(Stmt::Switch {
            condition,
            body,
            span: self.finish(start),
        })
    }

    fn case_statement(&mut self) -> Result<Stmt, Diagnostic> {
        let start = self.bump().span;
        let value = self.conditional()?;
        let end = if self.eat_punct("...") {
            Some(self.conditional()?)
        } else {
            None
        };
        self.expect_punct(":")?;
        let inner = self.statement()?;
        Ok(Stmt::Case {
            value,
            end,
            span: start.join(inner.span()),
            stmt: Box::new(inner),
        })
    }

    /// `default:` or its `_:` spelling; the caller has checked which one is
    /// present.
    fn default_statement(&mut self) -> Result<Stmt, Diagnostic> {
        let start = self.bump().span;
        self.expect_punct(":")?;
        let inner = self.statement()?;
        Ok(Stmt::Default {
            span: start.join(inner.span()),
            stmt: Box::new(inner),
        })
    }

    fn return_statement(&mut self) -> Result<Stmt, Diagnostic> {
        let start = self.bump().span;
        let value = if self.at_punct(";") {
            None
        } else {
            Some(self.expression()?)
        };
        let semicolon = self.expect_punct(";")?;
        Ok(Stmt::Return {
            value,
            span: start.join(semicolon),
        })
    }

    fn goto_statement(&mut self) -> Result<Stmt, Diagnostic> {
        let start = self.bump().span;
        if self.at_punct("*") {
            return Err(Diagnostic::new(
                self.here(),
                "computed goto is not supported",
            ));
        }
        let label = self.expect_ident()?;
        let semicolon = self.expect_punct(";")?;
        Ok(Stmt::Goto {
            label,
            span: start.join(semicolon),
        })
    }

    /// Both ShyC assembly forms:
    ///
    /// ```text
    /// asm("template");
    /// asm!(a, b) { "template" };
    /// ```
    fn asm_statement(&mut self) -> Result<AsmStmt, Diagnostic> {
        let start = self.here();
        let mut binding = self.at_punct("asm!");
        self.bump();
        // The lexer glues `asm!` into one token, but tolerate `asm !` too.
        if !binding && self.eat_punct("!") {
            binding = true;
        }

        if !binding {
            // File-scope asm is rejected elsewhere; qualifiers are not in the
            // AST at all, so name them instead of failing on 'volatile'.
            if matches!(self.ident_here(), Some("volatile" | "inline" | "goto")) {
                return Err(Diagnostic::new(
                    self.here(),
                    "asm qualifiers are not supported",
                ));
            }
            self.expect_punct("(")?;
            let templates = self.string_parts()?;
            if self.at_punct(":") {
                return Err(Diagnostic::new(
                    self.here(),
                    "extended asm operand lists are not supported",
                ));
            }
            self.expect_punct(")")?;
            self.expect_punct(";")?;
            return Ok(AsmStmt::Raw {
                span: start.join(self.last_span()),
                templates,
            });
        }

        self.expect_punct("(")?;
        let mut operands = Vec::new();
        while !self.at_punct(")") {
            operands.push(self.expect_ident()?);
            if !self.eat_punct(",") {
                break;
            }
        }
        self.expect_punct(")")?;
        self.expect_punct("{")?;
        let templates = self.string_parts()?;
        self.expect_punct("}")?;
        self.expect_punct(";")?;
        Ok(AsmStmt::Binding {
            span: start.join(self.last_span()),
            operands,
            templates,
        })
    }
}
impl Parser {
    /// Expands a `const fn` call that stands where statements are written.
    ///
    /// The returned tokens must form complete statements; a trailing expression
    /// without its semicolon is accepted by supplying the one the call site
    /// left behind. A `const fn` body expands such a call the same way: the
    /// call is syntax, not a value the body could compute.
    pub(crate) fn expand_statement_list(&mut self) -> Result<Option<Vec<Stmt>>, Diagnostic> {
        let Some(index) = self.const_fn_generator_ahead() else {
            return Ok(None);
        };
        let start = self.here();
        self.record_const_call(index, start);
        self.bump();
        let name = self.const_fns[index].name.clone();
        let (arguments, close) = self.const_arguments(index)?;
        let span = start.join(close);
        if self.defer_body_calls && !self.body_stack.is_empty() {
            self.defer_body_call();
            return Ok(Some(vec![
                self.placeholder_statement(index, &arguments, span),
            ]));
        }
        let key = self.probe_key(
            index,
            Place::Statement,
            span,
            super::probe::fingerprint(&arguments),
        );
        let returned = match self.probe_answer(&key) {
            Some(Answer::Tokens(tokens)) => tokens,
            Some(Answer::Failed { message, span: at }) => {
                return Err(Diagnostic::new(at.unwrap_or(span), message));
            }
            Some(Answer::Int(_)) | Some(Answer::Str(_)) | Some(Answer::Suspended { .. }) => {
                return Err(Diagnostic::new(
                    span,
                    "a statement const fn must produce tokens",
                ));
            }
            // Categorized placeholder: the call the answer will replace, so
            // the block still parses and type-checks. A round that has
            // requests produces no code.
            None => {
                let placeholder = self.placeholder_statement(index, &arguments, span);
                self.probe_record(ProbeRequest {
                    key,
                    function: index,
                    name,
                    place: Place::Statement,
                    site: span,
                    arguments,
                });
                return Ok(Some(vec![placeholder]));
            }
        };
        if returned.is_empty() {
            return Ok(Some(Vec::new()));
        }
        let name = self.const_fns[index].name.clone();
        return self
            .parse_statement_fragment(returned, span)
            .map(Some)
            .map_err(|error| {
                // A declaration cannot stand inside a block, and the fragment
                // parser rejects a function definition there. Name the const fn
                // and the place it belongs in instead of that parse detail.
                if error.message.starts_with("nested function definitions") {
                    Diagnostic::new(
                        error.span,
                        format!(
                            "const fn '{name}' produced declarations, which cannot stand inside a function; call it at file scope"
                        ),
                    )
                } else {
                    self.fragment_error(error, index)
                }
            });
    }

    /// The statement an unanswered generator call leaves in the tree.
    ///
    /// A call with expression arguments keeps its own expression, so the
    /// placeholder has the type the expansion will fill. A body that would run
    /// it is trapped by the probe image's unresolved-body guard. A `Tokens`
    /// argument has no expression to keep, so one empty statement stands in
    /// there.
    fn placeholder_statement(
        &self,
        index: usize,
        arguments: &super::probe::Arguments,
        span: Span,
    ) -> Stmt {
        let super::probe::Arguments::Expressions(args) = arguments else {
            return Stmt::Empty(span);
        };
        let expr = Expr::Call {
            callee: Box::new(Expr::Name(Name::new(
                self.const_fns[index].name.clone(),
                span,
            ))),
            type_args: Vec::new(),
            args: args.clone(),
            span,
        };
        Stmt::Expr { expr, span }
    }

    /// Parses a token sequence as statements.
    pub(crate) fn parse_statement_fragment(
        &mut self,
        mut tokens: Vec<crate::lexer::Token>,
        span: crate::source::Span,
    ) -> Result<Vec<Stmt>, Diagnostic> {
        let needs_semicolon = !matches!(
            tokens.last().map(|token| &token.kind),
            Some(crate::lexer::TokenKind::Punct(punct)) if punct == ";" || punct == "}"
        );
        if needs_semicolon {
            tokens.push(crate::lexer::Token {
                kind: crate::lexer::TokenKind::Punct(";".to_string()),
                span,
                at_line_start: false,
                leading_space: false,
            });
        }
        tokens.push(crate::lexer::Token {
            kind: crate::lexer::TokenKind::Eof,
            span,
            at_line_start: false,
            leading_space: false,
        });
        let mut parser = Parser::new(tokens, false);
        parser.scopes = self.scopes.clone();
        parser.const_fns = self.const_fns.clone();
        parser.probe = self.probe.clone();
        parser.body_world = self.body_world.clone();
        parser.defer_body_calls = self.defer_body_calls;
        parser.body_stack = self.body_stack.clone();
        parser.function_bodies = self.function_bodies.clone();
        let mut statements = Vec::new();
        while !parser.at_eof() {
            statements.extend(parser.statement_or_expansion()?);
        }
        self.bodies_deferred |= parser.bodies_deferred;
        self.deferred_body_functions
            .extend(parser.deferred_body_functions.iter().cloned());
        Ok(statements)
    }
}
