//! Expression syntax.
//!
//! Operators keep their C precedence by their shape: the tree has already
//! decided what binds to what (`a + b * c` is a sum whose right side is a
//! product). Names, literal values, and Shy extensions are still unresolved.
//!
//! Shy notes:
//!
//! - `Type::method` is [`Expr::ScopedName`], not a resolved function.
//! - `c.add(4)` and `p.add(5)` are both [`Expr::Member`] with
//!   [`MemberOp::Dot`]. Automatic dereference of `p` happens in semantic
//!   analysis; the AST does not insert a `*`.

use crate::ast::Name;
use crate::ast::decl::Initializer;
use crate::ast::stmt::Block;
use crate::ast::ty::{GenericArg, TypeName};
use crate::source::Span;

// ---------------------------------------------------------------------------
// Literals
// ---------------------------------------------------------------------------

/// A preprocessing number, exactly as the lexer spelled it.
///
/// `text` may be `0xFF`, `1'000`, `1.5e-3`, or `42UL`. The suffix and the
/// value are decoded by semantic analysis.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NumberLit {
    pub text: String,
    pub span: Span,
}

/// A character literal with its encoding prefix and escapes untouched.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CharLit {
    /// `""`, `"L"`, `"u"`, `"U"`, or `"u8"`.
    pub prefix: String,
    /// Text between the single quotes, escapes intact.
    pub raw: String,
    pub span: Span,
}

/// One string literal with its encoding prefix and escapes untouched.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StringLit {
    /// `""`, `"L"`, `"u"`, `"U"`, or `"u8"`.
    pub prefix: String,
    /// Text between the double quotes, escapes intact.
    pub raw: String,
    pub span: Span,
}

/// Adjacent string literals, kept as separate parts.
///
/// `"ab" "cd"` produces two parts. Semantic analysis checks that the encoding
/// prefixes agree and concatenates the decoded bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StringLitGroup {
    pub parts: Vec<StringLit>,
    pub span: Span,
}

// ---------------------------------------------------------------------------
// Operators
// ---------------------------------------------------------------------------

/// `.` or `->` on a member expression.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemberOp {
    Dot,
    Arrow,
}

/// Prefix unary operators. `sizeof` and `_Alignof` are separate variants
/// because they accept either a type name or an expression.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnaryOp {
    Plus,
    Minus,
    LogicalNot,
    BitNot,
    Dereference,
    AddressOf,
    PreIncrement,
    PreDecrement,
}

/// Postfix `++` and `--`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PostfixOp {
    Increment,
    Decrement,
}

/// Binary operators, one variant per C spelling.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BinaryOp {
    Multiply,
    Divide,
    Remainder,
    Add,
    Subtract,
    ShiftLeft,
    ShiftRight,
    Less,
    LessEqual,
    Greater,
    GreaterEqual,
    Equal,
    NotEqual,
    BitAnd,
    BitXor,
    BitOr,
    LogicalAnd,
    LogicalOr,
}

/// Assignment operators, including compound assignments.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AssignOp {
    Assign,
    Add,
    Subtract,
    Multiply,
    Divide,
    Remainder,
    ShiftLeft,
    ShiftRight,
    BitAnd,
    BitXor,
    BitOr,
}

// ---------------------------------------------------------------------------
// Expressions
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Expr {
    /// A numeric literal, spelling preserved.
    Number(NumberLit),
    /// A character literal, spelling preserved.
    Char(CharLit),
    /// One or more adjacent string literals.
    String(StringLitGroup),
    /// A plain identifier used as a value: `x`.
    Name(Name),
    /// Shy's `Type::method`, kept as two unresolved names.
    ScopedName {
        qualifier: Name,
        name: Name,
        span: Span,
    },
    /// `Type<args>::member`, a scoped name whose qualifier is a generic type
    /// use such as `Option<int>::Some`.
    GenericScopedName {
        qualifier: Name,
        args: Vec<GenericArg>,
        name: Name,
        span: Span,
    },
    /// A builtin call whose arguments are type names, not expressions.
    ///
    /// `__builtin_reg_class(ty)` has one type argument and
    /// `__builtin_types_compatible_p(T1, T2)` has two. `name` keeps the exact
    /// spelling so semantic analysis can check the arity. Other builtins such
    /// as `__builtin_compare_and_swap` take ordinary expressions and are
    /// parsed as [`Expr::Call`] with an [`Expr::Name`] callee.
    BuiltinTypeCall {
        name: Name,
        type_args: Vec<TypeName>,
        span: Span,
    },
    /// Prefix unary operation: `-x`, `*p`, `&x`, `!ok`, `++i`.
    Unary {
        op: UnaryOp,
        operand: Box<Expr>,
        span: Span,
    },
    /// Postfix increment or decrement: `i++`.
    Postfix {
        op: PostfixOp,
        operand: Box<Expr>,
        span: Span,
    },
    /// Binary operation: `a + b`, `a && b`.
    Binary {
        op: BinaryOp,
        left: Box<Expr>,
        right: Box<Expr>,
        span: Span,
    },
    /// Assignment: `a = b`, `a += b`.
    Assignment {
        op: AssignOp,
        left: Box<Expr>,
        right: Box<Expr>,
        span: Span,
    },
    /// `condition ? then_expr : else_expr`.
    Conditional {
        condition: Box<Expr>,
        then_expr: Box<Expr>,
        else_expr: Box<Expr>,
        span: Span,
    },
    /// Comma operator. Kept binary, matching the grammar's left association.
    Comma {
        left: Box<Expr>,
        right: Box<Expr>,
        span: Span,
    },
    /// Function or method call: `f(a, b)` or a generic call `f<int>(a)`.
    Call {
        callee: Box<Expr>,
        /// Type and `const` arguments written before the call parentheses.
        type_args: Vec<GenericArg>,
        args: Vec<Expr>,
        span: Span,
    },
    /// Array subscript: `a[i]`. `a` may be a pointer or an array.
    Index {
        array: Box<Expr>,
        index: Box<Expr>,
        span: Span,
    },
    /// Member access: `s.x` or `p->x`. Dot does not encode auto-deref.
    Member {
        object: Box<Expr>,
        name: Name,
        op: MemberOp,
        span: Span,
    },
    /// Explicit cast: `(int)x`.
    Cast {
        type_name: TypeName,
        value: Box<Expr>,
        span: Span,
    },
    /// `sizeof (type)`.
    SizeofType { type_name: TypeName, span: Span },
    /// `sizeof expr`.
    SizeofExpr { value: Box<Expr>, span: Span },
    /// `_Alignof(type)` or `alignof(type)`.
    AlignofType { type_name: TypeName, span: Span },
    /// `_Alignof expr` or `alignof expr`.
    AlignofExpr { value: Box<Expr>, span: Span },
    /// C compound literal: `(struct Point){ .x = 1, .y = 2 }`.
    ///
    /// The initializer is boxed to keep the recursive type finite.
    CompoundLiteral {
        type_name: TypeName,
        init: Box<Initializer>,
        span: Span,
    },
    /// GNU statement expression: `({ ... })`.
    ///
    /// The block is a normal [`Block`]; its last expression statement is the
    /// value. Semantic analysis checks that such a value exists.
    StatementExpr { block: Block, span: Span },
}

impl Expr {
    /// Source range of the whole expression.
    pub fn span(&self) -> Span {
        match self {
            Expr::Number(lit) => lit.span,
            Expr::Char(lit) => lit.span,
            Expr::String(lit) => lit.span,
            Expr::Name(name) => name.span,
            Expr::ScopedName { span, .. }
            | Expr::GenericScopedName { span, .. }
            | Expr::BuiltinTypeCall { span, .. }
            | Expr::Unary { span, .. }
            | Expr::Postfix { span, .. }
            | Expr::Binary { span, .. }
            | Expr::Assignment { span, .. }
            | Expr::Conditional { span, .. }
            | Expr::Comma { span, .. }
            | Expr::Call { span, .. }
            | Expr::Index { span, .. }
            | Expr::Member { span, .. }
            | Expr::Cast { span, .. }
            | Expr::SizeofType { span, .. }
            | Expr::SizeofExpr { span, .. }
            | Expr::AlignofType { span, .. }
            | Expr::AlignofExpr { span, .. }
            | Expr::CompoundLiteral { span, .. }
            | Expr::StatementExpr { span, .. } => *span,
        }
    }
}
