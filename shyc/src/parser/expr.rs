//! Expression syntax: precedence climbing plus C and Shy postfix constructs.
//!
//! The tree records binding, not meaning: `a + b * c` is a sum of `a` and a
//! product. `Type::method` stays a [`crate::ast::Expr::ScopedName`], and
//! `p.field` on a pointer stays a dot member, because Shy's auto-dereference is
//! semantic work.

use crate::ast::{
    AssignOp, BinaryOp, CharLit, Expr, MemberOp, Name, NumberLit, PostfixOp, StringLit,
    StringLitGroup, UnaryOp,
};
use crate::lexer::TokenKind;
use crate::source::{Diagnostic, Span};

use super::Parser;

impl Parser<'_> {
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
    pub(crate) fn conditional(&mut self) -> Result<Expr, Diagnostic> {
        let condition = self.binary(1)?;
        if !self.eat_punct("?") {
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

    fn postfix(&mut self) -> Result<Expr, Diagnostic> {
        let expr = self.primary()?;
        self.postfix_suffixes(expr)
    }

    fn postfix_suffixes(&mut self, mut expr: Expr) -> Result<Expr, Diagnostic> {
        loop {
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
            } else if self.at_punct("(") {
                self.bump();
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
                let span = expr.span().join(close);
                expr = Expr::Call {
                    callee: Box::new(expr),
                    args,
                    span,
                };
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
            } else {
                break;
            }
        }
        Ok(expr)
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
