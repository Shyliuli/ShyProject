//! Syntax-only abstract syntax tree for the ShyC frontend.
//!
//! Pipeline position: `source` -> `lexer` -> preprocessor -> **AST** -> sema.
//!
//! This tree records *what the user wrote*, not what it means:
//!
//! - names are kept as [`Name`] values and are not resolved;
//! - type spellings such as `unsigned long` stay as the list of keywords the
//!   user wrote; semantic analysis combines them into one type;
//! - `int` and `MyType` are both just declaration specifiers here;
//! - Shy's `Type::method`, `self`, `impl`, and `trait` are stored as written;
//!   automatic dot dereference and method dispatch belong to semantic analysis;
//! - number, string, and character literals keep the lexer's raw spelling.
//!
//! Every node that can appear in a diagnostic carries a [`Span`]. The tree is
//! deliberately made of plain structs and recursive enums (`Box`, `Vec`), with
//! no arenas, traits, or visitor framework, so a new reader can follow it.
//!
//! The parser is the next stage; see [`ty::Declarator`] for the exact
//! declarator order this tree expects.
//!
//! # Deliberately not represented
//!
//! The parser must report an explicit diagnostic for these instead of
//! silently dropping them:
//!
//! - `_Alignas`, `__attribute__((...))`, `#pragma pack`, and other alignment
//!   or packing controls;
//! - `_Static_assert` declarations;
//! - `_Thread_local` (the target has no TLS);
//! - `_Complex`, `typeof`, and `_Generic`;
//! - old-style K&R definitions whose parameter declarations follow the
//!   declarator;
//! - array parameter qualifiers and `static` (`int a[static 10]`,
//!   `int a[const 10]`);
//! - `case 1 ... 3:` ranges and `[0 ... 3]` designator ranges;
//! - asm qualifiers such as `asm volatile`, extended-asm operand/clobber
//!   lists, and file-scope `asm` declarations;
//! - labels as values (`&&label`) and nested function definitions;
//! - prototypes inside an `impl` block (methods always have a body here);
//! - runtime trait objects: a `trait` declares compile-time method signatures
//!   only, and using a trait name as a value type is rejected by semantic
//!   analysis;
//! - generic traits, bounds on generic records, enums, and traits, and trait
//!   bounds that write type arguments. A generic function may write static
//!   trait bounds (`fn render<T: Drawable>(T *value)`); they are kept on
//!   [`ty::GenericParam::bounds`] and checked at instantiation;
//! - scoped names with more than two segments (`A::B::m`).
//!
//! `_Atomic` is represented in both of its spellings: a qualifier
//! ([`ty::Qualifier::Atomic`]) and a type specifier
//! ([`ty::TypeSpecifier::Atomic`]). The type-operand builtins
//! `__builtin_reg_class` and `__builtin_types_compatible_p` are represented by
//! [`expr::Expr::BuiltinTypeCall`].

pub mod decl;
pub mod expr;
pub mod stmt;
pub mod ty;

pub use decl::*;
pub use expr::*;
pub use stmt::*;
pub use ty::*;

use crate::source::Span;

/// An unresolved identifier exactly as the lexer spelled it.
///
/// ShyC accepts Unicode identifiers, so `text` is a `String`, not bytes.
/// Whether the name is a variable, typedef name, struct tag, label, or method
/// is decided later by semantic analysis.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Name {
    /// Source spelling, with escapes and decorations untouched.
    pub text: String,
    /// Where the identifier appears in the source file.
    pub span: Span,
}

impl Name {
    pub fn new(text: impl Into<String>, span: Span) -> Self {
        Self {
            text: text.into(),
            span,
        }
    }

    pub fn as_str(&self) -> &str {
        &self.text
    }
}
