//! Statement syntax.
//!
//! A block is a flat `Vec<Stmt>`. Local declarations are statements
//! ([`Stmt::Decl`] and the inferred [`Stmt::Let`]) so that declarations and
//! statements keep their exact source order, including declarations after
//! statements.

use crate::ast::Name;
use crate::ast::decl::Declaration;
use crate::ast::expr::Expr;
use crate::ast::expr::StringLit;
use crate::source::Span;

/// A `{ ... }` block, used for function bodies, compound statements, and GNU
/// statement expressions.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Block {
    pub span: Span,
    pub stmts: Vec<Stmt>,
}

/// The first clause of a `for` loop: either a declaration or an expression.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ForInit {
    /// `for (int i = 0; ...)`
    Decl(Declaration),
    /// `for (i = 0; ...)`
    Expr(Expr),
}

/// Inline assembly, in either supported ShyC form.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AsmStmt {
    /// `asm("...")` with adjacent string literals kept as parts.
    Raw {
        span: Span,
        templates: Vec<StringLit>,
    },
    /// `asm!(a, b) { "adda {a} {b}" }`.
    ///
    /// `operands` are the plain names between the parentheses. The `&` in a
    /// placeholder such as `{&lock}` is part of the template text, not a
    /// separate AST flag.
    Binding {
        span: Span,
        operands: Vec<Name>,
        templates: Vec<StringLit>,
    },
}

impl AsmStmt {
    pub fn span(&self) -> Span {
        match self {
            AsmStmt::Raw { span, .. } | AsmStmt::Binding { span, .. } => *span,
        }
    }
}

/// A `let` local declaration: `let name = expression;` or
/// `let mut name = expression;`.
///
/// This is ShyC's only inferred declaration form. The type is deduced from the
/// required initializer, so no declaration specifier list is written; `mut`
/// only makes the object writable and adds no ownership or reference
/// semantics. `let` is a soft keyword: ordinary identifiers with that spelling
/// stay valid.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LetDecl {
    pub span: Span,
    /// `true` for `let mut`, `false` for a read-only `let`.
    pub is_mut: bool,
    pub name: Name,
    pub init: Expr,
}

/// One C statement or Shy statement extension.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Stmt {
    /// A lone `;`.
    Empty(Span),
    /// An expression statement: `x = 1;`.
    Expr { expr: Expr, span: Span },
    /// A declaration used as a statement: `int x = 1;`.
    Decl(Declaration),
    /// An inferred local declaration: `let x = 1;` or `let mut x = 1;`.
    Let(LetDecl),
    /// A compound statement: `{ ... }`.
    Block(Block),
    /// `if (cond) then_branch else else_branch`.
    If {
        condition: Expr,
        then_branch: Box<Stmt>,
        else_branch: Option<Box<Stmt>>,
        span: Span,
    },
    /// `while (cond) body`.
    While {
        condition: Expr,
        body: Box<Stmt>,
        span: Span,
    },
    /// `do body while (cond);`
    DoWhile {
        body: Box<Stmt>,
        condition: Expr,
        span: Span,
    },
    /// `for (init; condition; step) body`.
    ///
    /// Any of the three clauses may be omitted. `condition` and `step` are
    /// single expressions; the comma operator handles `i++, j++` inside one
    /// expression.
    For {
        init: Option<Box<ForInit>>,
        condition: Option<Box<Expr>>,
        step: Option<Box<Expr>>,
        body: Box<Stmt>,
        span: Span,
    },
    /// `switch (cond) body`.
    Switch {
        condition: Expr,
        body: Box<Stmt>,
        span: Span,
    },
    /// `case value: stmt`. Labels nest naturally when several cases share a
    /// statement.
    Case {
        value: Expr,
        stmt: Box<Stmt>,
        span: Span,
    },
    /// `default: stmt` or its `_:` spelling.
    Default { stmt: Box<Stmt>, span: Span },
    /// A named label: `name: stmt`.
    Label {
        name: Name,
        stmt: Box<Stmt>,
        span: Span,
    },
    /// `goto label;`
    Goto { label: Name, span: Span },
    /// `break;`
    Break(Span),
    /// `continue;`
    Continue(Span),
    /// `return;` or `return expr;`
    Return { value: Option<Expr>, span: Span },
    /// `asm("...");` or `asm!(...) { ... };`
    Asm(AsmStmt),
}

impl Stmt {
    /// Source range of the whole statement, including its `;` where present.
    pub fn span(&self) -> Span {
        match self {
            Stmt::Empty(span) | Stmt::Break(span) | Stmt::Continue(span) => *span,
            Stmt::Expr { span, .. }
            | Stmt::If { span, .. }
            | Stmt::While { span, .. }
            | Stmt::DoWhile { span, .. }
            | Stmt::For { span, .. }
            | Stmt::Switch { span, .. }
            | Stmt::Case { span, .. }
            | Stmt::Default { span, .. }
            | Stmt::Label { span, .. }
            | Stmt::Goto { span, .. }
            | Stmt::Return { span, .. } => *span,
            Stmt::Decl(decl) => decl.span,
            Stmt::Let(decl) => decl.span,
            Stmt::Block(block) => block.span,
            Stmt::Asm(asm) => asm.span(),
        }
    }
}
