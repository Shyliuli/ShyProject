//! Type syntax: declaration specifiers, structs, unions, enums, and
//! declarators.
//!
//! Nothing here decides a final type. `unsigned long` is a list of two
//! keywords, `struct Counter` is a tag with an optional definition, and a
//! declarator is a name plus an ordered list of operators. Semantic analysis
//! turns those spellings into actual types.

use crate::ast::Name;
use crate::ast::expr::Expr;
use crate::source::Span;

// ---------------------------------------------------------------------------
// Declaration specifiers
// ---------------------------------------------------------------------------

/// Storage class keyword. At most one appears in a C declaration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StorageClass {
    Typedef,
    Extern,
    Static,
    Auto,
    Register,
}

/// Type qualifier attached to a declaration specifier or a pointer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Qualifier {
    Const,
    Volatile,
    Restrict,
    /// `_Atomic` as a qualifier, for example `_Atomic int` or
    /// `int * _Atomic`. The parenthesised form `_Atomic(int)` is a type
    /// specifier instead; see [`TypeSpecifier::Atomic`].
    Atomic,
}

/// One built-in type keyword.
///
/// Keywords are kept separately and in source order, so `unsigned long long`
/// is `[Unsigned, Long, Long]`. Joining them is semantic analysis' job.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BuiltinType {
    Void,
    /// `_Bool`
    Bool,
    Char,
    Short,
    /// `int`. C's implicit `int` is represented by an empty specifier list,
    /// not by inventing this keyword.
    Int,
    Long,
    Float,
    Double,
    Signed,
    Unsigned,
}

/// One entry in a declaration specifier list.
///
/// A declaration such as `static const unsigned long *p` has storage
/// `Static`, qualifier `Const`, and type specifiers `[Builtin(Unsigned),
/// Builtin(Long)]`. The pointer qualifiers live on the declarator instead.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TypeSpecifier {
    /// One built-in keyword.
    Builtin(BuiltinType),
    /// An identifier used as a type name. This may name a typedef, or in ShyC
    /// also a struct, union, or enum tag (`Counter c;`). Resolution is later.
    TypedefName(Name),
    /// A generic type use such as `Vec<int>`, with its arguments spelled out.
    Generic(GenericSpec),
    /// `struct ...` or `union ...`, with an optional definition.
    Record(RecordSpec),
    /// `enum ...`, with an optional definition.
    Enum(EnumSpec),
    /// The parenthesised atomic type form `_Atomic(type-name)`.
    ///
    /// Bare `_Atomic` is a keyword list member, so it appears in
    /// [`DeclSpecs::qualifiers`] as [`Qualifier::Atomic`] instead.
    Atomic(TypeName),
    /// Shy's `self` type, valid only inside an `impl` block.
    SelfType,
}

/// What kind of generic parameter a name introduces.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GenericParamKind {
    /// A type parameter, written `T`.
    Type,
    /// A compile-time integer parameter, written `const N`.
    Const,
}

/// One generic parameter in a declaration, for example `T`, `T: Drawable`, or
/// `const N`.
///
/// `bounds` lists the traits written after `:`, separated by `+`. Only a type
/// parameter may have bounds; a `const` parameter keeps `bounds` empty.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GenericParam {
    pub span: Span,
    pub name: Name,
    pub kind: GenericParamKind,
    pub bounds: Vec<Name>,
}

/// One generic argument at a use site or call site.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GenericArg {
    pub span: Span,
    pub kind: GenericArgKind,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GenericArgKind {
    /// A type argument, for example `int` or `Vec<T>`.
    Type(TypeName),
    /// A compile-time value argument, for example `4` or `N + 1`.
    Const(Expr),
}

/// A generic type use such as `Vec<int>`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GenericSpec {
    pub span: Span,
    pub name: Name,
    pub args: Vec<GenericArg>,
}

/// The specifiers in front of one declaration.
///
/// `type_specs` may be empty: old-style C allows an omitted `int`. The parser
/// preserves that as written and lets semantic analysis reject it where
/// needed.
///
/// `is_inline` and `is_noreturn` are C function specifiers (`inline`,
/// `_Noreturn`). They are flags rather than storage classes because they may
/// appear together with `static` or `extern`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeclSpecs {
    pub span: Span,
    pub storage: Option<StorageClass>,
    pub is_inline: bool,
    pub is_noreturn: bool,
    pub qualifiers: Vec<Qualifier>,
    pub type_specs: Vec<TypeSpecifier>,
}

// ---------------------------------------------------------------------------
// Structs, unions, enums
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecordKind {
    Struct,
    Union,
}

/// `struct Tag { ... }`, `union Tag { ... }`, or a reference to a tag.
///
/// `tag` is `None` for an anonymous record definition.
/// `members` is `None` for a reference (`struct Tag`), and `Some` for a
/// definition. `Some(vec![])` is an explicitly empty definition (`{}`), which
/// is distinct from a reference.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecordSpec {
    pub span: Span,
    pub kind: RecordKind,
    pub tag: Option<Name>,
    /// Generic parameters of a record definition (`Vec<T>`), empty if none.
    pub generics: Vec<GenericParam>,
    pub members: Option<Vec<MemberDecl>>,
}

/// One member declaration, for example `int x, y : 3;`.
///
/// A C11 anonymous struct/union member has no declarator at all; the parser
/// represents it with an empty `declarators` list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemberDecl {
    pub span: Span,
    pub specs: DeclSpecs,
    pub declarators: Vec<MemberDeclarator>,
}

/// One member declarator, with its optional bit-field width.
///
/// `int : 0;` is an unnamed bit-field: `declarator.name` is `None` and
/// `bit_width` is `Some(0)`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemberDeclarator {
    pub span: Span,
    pub declarator: Declarator,
    pub bit_width: Option<Expr>,
}

/// `enum Tag { A, B = 2 }`, `enum(u8) Tag { ... }`, or a reference to
/// `enum Tag`.
///
/// `underlying` is the optional storage type written in parentheses directly
/// after `enum`. `generics` holds the parameters of a definition
/// (`enum Option<T> { ... }`) and is empty otherwise. `enumerators` is `None`
/// for a reference and `Some` for a definition, following the same rule as
/// [`RecordSpec::members`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EnumSpec {
    pub span: Span,
    pub underlying: Option<TypeName>,
    pub tag: Option<Name>,
    /// Generic parameters of an enum definition, empty if none.
    pub generics: Vec<GenericParam>,
    pub enumerators: Option<Vec<Enumerator>>,
}

/// One enumerator. A missing `value` means "previous value plus one".
///
/// `payload` is the type in `Variant(Type)`; such an enum is a payload enum
/// and the variant cannot also carry an explicit value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Enumerator {
    pub span: Span,
    pub name: Name,
    pub value: Option<Expr>,
    pub payload: Option<TypeName>,
}

// ---------------------------------------------------------------------------
// Declarators
// ---------------------------------------------------------------------------

/// A declarator, including abstract declarators used in type names.
///
/// # Binding order
///
/// `derived` is stored **from the declared name outward**. Build the type by
/// starting with the base type from the declaration specifiers and applying
/// `derived` in reverse order. The classic examples:
///
/// ```text
/// int *a[3];      a: array 3 of pointer to int      derived = [Array(3), Pointer]
/// int (*a)[3];    a: pointer to array 3 of int      derived = [Pointer, Array(3)]
/// int *f(void);   f: function returning pointer     derived = [Function, Pointer]
/// int (*f)(void); f: pointer to function            derived = [Pointer, Function]
/// ```
///
/// This order removes the parentheses-precedence puzzle from every later
/// consumer. The parser produces it with this recipe:
///
/// 1. Parse the leading `*` list. Remember the pointers in source order.
/// 2. Parse the direct declarator:
///    - plain name: start with its trailing suffixes (`[]`, `()`), and if the
///      declarator is abstract, start with those suffixes alone;
///    - parenthesised declarator `( inner )`: recursively parse `inner`, then
///      append the suffixes written after the closing `)`;
///    - abstract declarator: an omitted name, plus the same suffixes.
/// 3. Append the leading pointers in **reverse** source order.
///
/// Step 2's grouping rule is what distinguishes `*a[3]` from `(*a)[3]`; step 3
/// is what makes the rightmost `*` the pointer nearest the name.
///
/// `name` is `None` for an abstract declarator.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Declarator {
    pub span: Span,
    pub name: Option<Name>,
    pub derived: Vec<DerivedDeclarator>,
}

/// One type-building operator in a declarator, ordered from the name outward.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DerivedDeclarator {
    /// `*` with its optional qualifiers, for example `* const`.
    Pointer(PointerDeclarator),
    /// `[N]`, `[]`, or `[*]`.
    Array(ArrayDeclarator),
    /// `(params)` or `(void)`.
    Function(FunctionDeclarator),
}

/// A pointer operator.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PointerDeclarator {
    pub span: Span,
    pub qualifiers: Vec<Qualifier>,
}

/// An array suffix.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArrayDeclarator {
    pub span: Span,
    pub length: ArrayLength,
}

/// The text between `[` and `]`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ArrayLength {
    /// `[]`
    Unspecified,
    /// `[expr]`. A constant size and a VLA both look like this here; semantic
    /// analysis decides which one is allowed.
    Expr(Box<Expr>),
    /// `[*]`, the VLA size placeholder allowed in prototypes.
    Star,
}

/// A function suffix and its parameter list.
///
/// `()` is an empty `params` list with `variadic == false`. `(void)` is one
/// parameter whose specifiers are `[Builtin(Void)]`; the parser must keep that
/// distinction for semantic analysis. `(int a, ...)` sets `variadic`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FunctionDeclarator {
    pub span: Span,
    /// Generic parameters written between the name and `(`, as in `fn f<T>(...)`.
    pub generics: Vec<GenericParam>,
    pub params: Vec<ParamDecl>,
    pub variadic: bool,
}

/// One parameter declaration.
///
/// `declarator` is `None` when the parameter has no declarator text, as in
/// `f(int)` or `f(void)`. An abstract declarator such as `int *` or
/// `int (*)(void)` is `Some` with [`Declarator::name`] equal to `None`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParamDecl {
    pub span: Span,
    pub specs: DeclSpecs,
    pub declarator: Option<Declarator>,
}

/// A type name as it appears in a cast, `sizeof`, `_Alignof`, a compound
/// literal, or an `impl` header.
///
/// `declarator` is `None` for a bare type (`int`), and `Some` for an abstract
/// declarator (`int *`, `int (*)[3]`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TypeName {
    pub span: Span,
    pub specs: DeclSpecs,
    pub declarator: Option<Declarator>,
}
