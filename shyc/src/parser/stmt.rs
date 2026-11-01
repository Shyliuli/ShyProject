//! Statements, blocks, labels, and inline assembly.

use crate::ast::{AsmStmt, Block, Declarator, DerivedDeclarator, ForInit, LetDecl, Stmt};
use crate::lexer::TokenKind;
use crate::source::{Diagnostic, Span};

use super::Parser;
use super::cursor::is_keyword;

impl Parser {
    /// One statement, including a declaration used as a statement.
    pub(crate) fn statement(&mut self) -> Result<Stmt, Diagnostic> {
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

        if self.starts_type_name(0) {
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
        if !self.at_ident("let") || self.is_type_name("let") {
            return false;
        }
        if self.at_ident_ahead(1, "mut") {
            return matches!(&self.peek_ahead(2).kind, TokenKind::Ident(name) if !is_keyword(name));
        }
        matches!(&self.peek_ahead(1).kind, TokenKind::Ident(name) if !is_keyword(name))
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
            stmts.push(self.statement()?);
        }
        let close = self.expect_punct("}")?;
        self.pop_scope();
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
        if self.at_punct("...") {
            return Err(Diagnostic::new(
                self.here(),
                "case ranges are not supported",
            ));
        }
        self.expect_punct(":")?;
        let inner = self.statement()?;
        Ok(Stmt::Case {
            value,
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
