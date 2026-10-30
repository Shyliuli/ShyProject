//! Top-level declarations, initializers, and Shy `impl` blocks.
//!
//! One [`TranslationUnit`] is one preprocessed source file. The parser fills
//! [`TranslationUnit::items`] in source order; semantic analysis later decides
//! which declarators are functions, objects, or typedef names.

use crate::ast::Name;
use crate::ast::expr::Expr;
use crate::ast::stmt::Block;
use crate::ast::ty::{DeclSpecs, Declarator, TypeName};
use crate::source::Span;

/// A whole source file after preprocessing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TranslationUnit {
    pub items: Vec<Item>,
}

/// One top-level construct.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Item {
    /// A declaration without a function body: a global variable, a typedef,
    /// a `struct`/`union`/`enum` declaration, or a function prototype.
    Decl(Declaration),
    /// A function definition. The body is always present, so prototypes never
    /// appear here; they are ordinary [`Item::Decl`] declarations.
    Function(FunctionDecl),
    /// Shy's `impl Type { ... }` block containing method definitions.
    Impl(ImplBlock),
}

/// A declaration with its specifiers and declarators.
///
/// `int a = 1, *b;` is one `Declaration` with two [`InitDeclarator`]s. The
/// parser keeps a typedef exactly as a declaration with
/// [`crate::ast::ty::StorageClass::Typedef`]; semantic analysis records the
/// new type name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Declaration {
    pub span: Span,
    pub specs: DeclSpecs,
    pub declarators: Vec<InitDeclarator>,
}

/// One declarator plus its optional initializer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InitDeclarator {
    pub span: Span,
    pub declarator: Declarator,
    pub init: Option<Initializer>,
}

/// A function definition, used both at file scope and inside an
/// [`ImplBlock`].
///
/// The declarator's `derived` list must start with a function operator,
/// because declarators are ordered from the name outward: `int *f(void)` is
/// `[Function, Pointer]`. Checking that shape is semantic analysis' job. The
/// body is a normal [`Block`], so local declarations and statements keep
/// their order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FunctionDecl {
    pub span: Span,
    pub specs: DeclSpecs,
    pub declarator: Declarator,
    pub body: Block,
}

/// Shy's `impl Type { methods }` block.
///
/// `self_type` is a type name, normally the single typedef-style name written
/// after `impl`. Each method is a [`FunctionDecl`] because its syntax is an
/// ordinary function definition; the parser does not resolve the implemented
/// type or the method names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImplBlock {
    pub span: Span,
    pub self_type: TypeName,
    pub methods: Vec<FunctionDecl>,
}

// ---------------------------------------------------------------------------
// Initializers
// ---------------------------------------------------------------------------

/// An initializer: either one expression or a brace-enclosed list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Initializer {
    pub span: Span,
    pub kind: InitializerKind,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InitializerKind {
    /// `= expr`
    Expr(Box<Expr>),
    /// `{ ... }`. An empty list is valid syntax; semantic analysis rejects it
    /// only if the target type cannot accept it.
    List(Vec<InitializerEntry>),
}

/// One element of a brace-enclosed initializer.
///
/// `designators` is empty for a positional element. For
/// `{ .x = 1, [2] = 3 }`, the first entry has one field designator and the
/// second has one index designator. Nested braces are another
/// [`Initializer`] in `value`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InitializerEntry {
    pub span: Span,
    pub designators: Vec<Designator>,
    pub value: Initializer,
}

/// One designator in front of an initializer element.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Designator {
    /// `.field`
    Field(Name),
    /// `[index]`
    Index(Expr),
}
