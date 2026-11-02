//! Top-level declarations, initializers, and Shy `impl`, `trait`, and `use` items.
//!
//! One [`TranslationUnit`] is one preprocessed source file. The parser fills
//! [`TranslationUnit::items`] in source order; semantic analysis later decides
//! which declarators are functions, objects, or typedef names.

use crate::ast::Name;
use crate::ast::expr::Expr;
use crate::ast::stmt::Block;
use crate::ast::ty::{DeclSpecs, Declarator, GenericArg, GenericParam, TypeName};
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
    /// Shy's `impl Type { ... }` or `impl Trait for Type { ... }` block
    /// containing method definitions.
    Impl(ImplBlock),
    /// Shy's `trait Name { ... }` declaration containing method signatures.
    Trait(TraitDecl),
    /// Shy's `use path::Name;` import of a file-scope name.
    Use(UseDecl),
}

/// A file-scope `use path::Name;` import.
///
/// The path names the module and, for a named import, its final symbol.
/// Preprocessing loads the module header before parsing; the retained node
/// lets semantic analysis check the name and lets the LSP index the import.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UseDecl {
    pub span: Span,
    /// The path segments before an optional imported name.
    pub path: Vec<Name>,
    /// Import every declaration from the included module (`::*`).
    pub glob: bool,
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

/// Shy's `impl Type { methods }` or `impl Trait for Type { methods }` block.
///
/// `self_type` is a type name, normally the single typedef-style name written
/// after `impl`. Each method is a [`FunctionDecl`] because its syntax is an
/// ordinary function definition; the parser does not resolve the implemented
/// type, the trait, or the method names. `trait_name` is `Some` only for the
/// two-name form.
///
/// `generics` is non-empty for a template impl such as `impl<T> Box<T>` or the
/// short `impl Box<T>` form: the parameters are bound when one concrete
/// receiver instantiates the block.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImplBlock {
    pub span: Span,
    /// The parameters written after `impl`, or inferred from the target's
    /// argument list, empty for an ordinary impl.
    pub generics: Vec<GenericParam>,
    /// The trait written before `for`, if this is a trait implementation.
    pub trait_name: Option<Name>,
    /// The concrete arguments written after the trait name, empty for an
    /// inherent impl or a trait without parameters.
    pub trait_args: Vec<GenericArg>,
    pub self_type: TypeName,
    pub methods: Vec<FunctionDecl>,
    /// Methods declared without a body, `fn len(self *s) -> int;`. Only an
    /// inherent impl of a concrete type has them: a header declares the
    /// methods and one translation unit defines them, as with C prototypes.
    pub prototypes: Vec<MethodPrototype>,
}

/// One bodiless method declaration inside an `impl` block.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MethodPrototype {
    pub span: Span,
    pub specs: DeclSpecs,
    pub declarator: Declarator,
}

/// Shy's `trait Name { method signatures }` declaration.
///
/// A trait is checked entirely at compile time. Its methods are signatures
/// only: no body, no storage class, and no runtime object. An `impl Trait for
/// Type` must define every one of them with a matching signature.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TraitDecl {
    pub span: Span,
    pub name: Name,
    /// Generic parameters written after the trait name, empty if none.
    pub generics: Vec<GenericParam>,
    pub methods: Vec<TraitMethod>,
}

/// One method signature in a [`TraitDecl`].
///
/// The declarator is kept exactly as the matching implementation will write
/// it, so `self` can be resolved against the implementing type when the
/// implementation is checked. There is no [`Block`] because a trait method is
/// never defined here.
///
/// `generics` are the parameters written between the method name and `(`, as
/// in `fn convert<T>(self *value, T value) -> T`. They are method-local: an
/// implementation binds its own parameters by position when the signatures are
/// compared. The same list is still present on the function declarator,
/// because that is where the syntax stores it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TraitMethod {
    pub span: Span,
    pub specs: DeclSpecs,
    pub declarator: Declarator,
    /// Method-local generic parameters, empty for an ordinary method.
    pub generics: Vec<GenericParam>,
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
