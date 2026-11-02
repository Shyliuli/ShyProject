//! C source and C header export for the `shyc` driver.
//!
//! Pipeline position: `source` -> `lexer` -> `preprocess` -> `parser` ->
//! **`export`**. This module is the output end of the compiler, not a stage:
//! [`render_c_source`] respells an already preprocessed token stream, and
//! [`render_c_header`] prints a C interface for an already parsed translation
//! unit. Neither function analyses or lowers anything.
//!
//! # `--emit=c`
//!
//! [`render_c_source`] writes the preprocessed translation unit back out as
//! plain C, ready for a host compiler such as `clang -x c`. Spelling follows
//! the preprocessor's token stream: every token keeps the text the lexer gave
//! it and a space separates all of them, so adjacent macro expansions cannot
//! fuse into one token when the output is lexed again. Shy's metadata
//! attributes (`#![no_main]`, `#![mem]`, `#![stack]`,
//! `#![disable_nonexhaustive_switch]`) belong to the Shy front end, not to C,
//! and are never written here.
//!
//! The scope is ordinary C. ShyC extensions have no C spelling yet and are
//! copied verbatim, so a file that uses them does not compile as C:
//!
//! - `impl Type { ... }` blocks and `Type::method` names;
//! - `asm!` register bindings;
//! - automatic dot dereference (the `.` of `p.method()` stays a C `.`).
//!
//! A `let` declaration is Shy-only as well, but it is not copied: the driver
//! calls [`reject_let`] first and reports a diagnostic, because a silently
//! copied `let x = 1;` would be read as a C declaration with a made-up type
//! name. `pub` is rejected for the same reason: silently dropping it would
//! change the linkage the author asked for. `--emit=c-header` never prints function bodies, so a `let` local
//! cannot appear in a header.
//!
//! # `--emit=c-header`
//!
//! [`render_c_header`] walks the [`TranslationUnit`] and prints the interface a
//! C caller needs:
//!
//! - the header guard is derived from the output path, so two generated headers
//!   in different directories cannot collide on `SHYC_EXPORT_H`;
//! - `#include` lines and `#define` macros written in the main input file are
//!   carried over verbatim, in source order (includes first, then definitions).
//!   Declarations that came from an included header are not repeated: the
//!   carried include already provides them, and the header stays honest about
//!   what it owns. This is decided from the declaration's [`Span::file`], which
//!   is the preprocessor's root file for main-file declarations only;
//! - declarations appear inside `#ifdef __cplusplus` / `extern "C" { ... }`
//!   guards. The preprocessor itself has no `__cplusplus`, so preprocessing a
//!   generated header skips the block and never parses the C++ spelling;
//! - every non-`static` function definition becomes a prototype;
//! - `typedef`s, `struct`/`union`/`enum` definitions, and prototypes written
//!   at file scope are kept;
//! - objects become `extern` declarations without their initializers, so the
//!   header declares but never defines anything;
//! - `static` objects and functions stay private to their translation unit;
//! - generic records, generic functions, their instantiations, and any
//!   declaration whose type mentions one stay private too, because C cannot
//!   spell a template and every instance is internal to its unit;
//! - `impl` blocks are skipped: method export arrives with generic
//!   instantiation work, because a method name alone does not name a symbol;
//! - every trait gets a forward `typedef` and a `struct` of function pointers,
//!   printed before other declarations so a field can name the trait in any
//!   order. `self` is erased to `void`, which is what the compiler's trait value
//!   uses.
//!
//! The header is built from the AST, never from source spans. A preprocessed
//! span may point into an included header or a macro expansion, so slicing
//! source text would print the wrong declaration; the printers below decide
//! the text themselves and are therefore deterministic. Expressions are needed
//! only for array sizes, bit-field widths, and enumerator values, so the
//! expression printer covers the operators such a constant expression can
//! contain. A GNU statement expression is not a constant expression and is
//! printed as an identifier the C compiler rejects, rather than as a value
//! that silently differs from the source.

use std::collections::HashSet;
use std::path::{Component, Path};

use crate::ast::{
    ArrayDeclarator, ArrayLength, AssignOp, BinaryOp, BuiltinType, DeclSpecs, Declaration,
    Declarator, DerivedDeclarator, Designator, EnumSpec, Enumerator, Expr, FunctionDecl,
    FunctionDeclarator, GenericArg, GenericArgKind, GenericParam, GenericParamKind, GenericSpec,
    Initializer, InitializerKind, Item, MemberDecl, MemberOp, ParamDecl, PostfixOp, Qualifier,
    RecordKind, RecordSpec, StorageClass, TraitDecl, TraitMethod, TranslationUnit, TypeName,
    TypeSpecifier, UnaryOp,
};
use crate::lexer::{Token, TokenKind};
use crate::preprocess::Include;
use crate::source::{Diagnostic, FileId, Span};

/// Printed for a construct a C header cannot express; the host compiler then
/// reports the generated header instead of accepting a changed meaning.
const UNSUPPORTED: &str = "__shyc_unsupported_expression";

/// Everything the header printer needs that is not in the parsed AST.
#[derive(Debug)]
pub struct HeaderOptions {
    /// Include guard, normally [`header_guard`] of the output path.
    pub guard: String,
    /// The main input file. Only its declarations are exported.
    pub root: FileId,
    /// Main-file `#include` directives, in source order.
    pub includes: Vec<Include>,
    /// Main-file `#define` lines, in source order, as C text.
    pub defines: Vec<String>,
}

/// Include guard for the header written to `path`.
///
/// The last two path components name the guard, so the same header gets the
/// same guard whether the compiler is invoked from the repository root or with
/// an absolute path: `libshy/c/stdio.h` becomes `SHYC_GENERATED_C_STDIO_H`.
/// Bytes that cannot appear in a C identifier become `_`.
pub fn header_guard(path: &Path) -> String {
    let mut names = path
        .components()
        .filter_map(|component| match component {
            Component::Normal(name) => Some(guard_component(&name.to_string_lossy())),
            _ => None,
        })
        .collect::<Vec<_>>();
    if names.len() > 2 {
        names.drain(..names.len() - 2);
    }
    if names.is_empty() {
        names.push("HEADER".to_string());
    }
    format!("SHYC_GENERATED_{}", names.join("_"))
}

fn guard_component(name: &str) -> String {
    name.chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect()
}

/// Reject Shy-only `let` declarations before they are printed as C.
///
/// `let` has no C spelling. The parser accepts the shape `let name` or
/// `let mut name`; the same shape is what this check looks for in the
/// preprocessed stream, so ordinary C identifiers keep their meaning.
pub fn reject_let(tokens: &[Token]) -> Result<(), Diagnostic> {
    let tokens: Vec<&Token> = tokens
        .iter()
        .filter(|token| !matches!(token.kind, TokenKind::Newline))
        .collect();
    let name_at = |index: usize| {
        matches!(
            tokens.get(index).map(|token| &token.kind),
            Some(TokenKind::Ident(name)) if name != "mut"
        )
    };
    for (index, token) in tokens.iter().enumerate() {
        // `pub` 和 `let` 一样是 Shy 的修饰词：C 里没有它，写出来就是编译错误，
        // 悄悄删掉又会改变链接性，所以直接报出来。
        if matches!(&token.kind, TokenKind::Ident(word) if word == "pub") {
            return Err(Diagnostic::new(
                token.span,
                "`pub` is ShyC-only and cannot be exported as C; use `static` or the C default",
            ));
        }
        if !matches!(&token.kind, TokenKind::Ident(word) if word == "let") {
            continue;
        }
        let starts_let = name_at(index + 1)
            || (matches!(
                tokens.get(index + 1).map(|token| &token.kind),
                Some(TokenKind::Ident(word)) if word == "mut"
            ) && name_at(index + 2));
        if starts_let {
            return Err(Diagnostic::new(
                token.span,
                "`let` declarations are ShyC-only and cannot be exported as C",
            ));
        }
    }
    Ok(())
}

/// Respell a preprocessed token stream as plain C text.
///
/// The output ends with a newline. [`TokenKind::Eof`] and the preprocessor's
/// newline markers are not tokens of the C text, so they are skipped.
///
/// ShyC `use` imports are dropped: C has no import syntax, and an import only
/// checks a name that the includes already make visible. `use` is a soft
/// keyword, so only the import shape (`use name (:: name)* ;`) is removed and an
/// ordinary C declaration spelled with the identifier `use` is kept.
pub fn render_c_source(tokens: &[Token]) -> String {
    let mut output = String::new();
    let mut index = 0;
    while index < tokens.len() {
        let token = &tokens[index];
        index += 1;
        if let Some(end) = use_declaration_end(tokens, index - 1) {
            if token.at_line_start && !output.is_empty() && !output.ends_with('\n') {
                output.push('\n');
            }
            index = end + 1;
            continue;
        }
        let spelling = match &token.kind {
            TokenKind::Ident(text) | TokenKind::Number(text) | TokenKind::Punct(text) => {
                text.clone()
            }
            TokenKind::StringLiteral { prefix, raw } => format!("{prefix}\"{raw}\""),
            TokenKind::CharLiteral { prefix, raw } => format!("{prefix}'{raw}'"),
            TokenKind::Newline | TokenKind::Eof => continue,
        };
        if token.at_line_start && !output.is_empty() && !output.ends_with('\n') {
            output.push('\n');
        }
        output.push_str(&spelling);
        // A separator prevents adjacent expanded tokens from fusing on re-lexing.
        output.push(' ');
    }
    output.push('\n');
    output
}

/// The index of the `;` ending the import that starts at `start`, if the tokens
/// there have the `use name :: ...` shape.
fn use_declaration_end(tokens: &[Token], start: usize) -> Option<usize> {
    if !matches!(&tokens[start].kind, TokenKind::Ident(word) if word == "use") {
        return None;
    }
    let name = next_significant(tokens, start + 1)?;
    if !matches!(tokens[name].kind, TokenKind::Ident(_)) {
        return None;
    }
    let separator = next_significant(tokens, name + 1)?;
    if !matches!(&tokens[separator].kind, TokenKind::Punct(punct) if punct == "::") {
        return None;
    }
    // The parser already validated the path, so the first `;` closes it.
    (separator..tokens.len())
        .find(|&index| matches!(&tokens[index].kind, TokenKind::Punct(punct) if punct == ";"))
}

/// The first index at or after `from` whose token is not a newline marker.
fn next_significant(tokens: &[Token], from: usize) -> Option<usize> {
    (from..tokens.len()).find(|&index| !matches!(tokens[index].kind, TokenKind::Newline))
}

/// Render the exported interface of `unit` as one C header.
///
/// The order is fixed: banner, guard, carried includes, carried definitions,
/// `extern "C"` block, declarations, guard end. Only declarations whose span
/// is the main input file are printed; a declaration from an included header is
/// already available through the carried `#include`.
pub fn render_c_header(unit: &TranslationUnit, options: &HeaderOptions) -> String {
    render_c_header_with(unit, options, "")
}

/// Render the exported interface, with an extra declaration block for explicit
/// generic instances.
///
/// `instances` is printed after the ordinary declarations and before the
/// closing `extern "C"` brace, so it may use every typedef the unit already
/// exports. An empty section is the ordinary `--emit=c-header` output.
pub fn render_c_header_with(
    unit: &TranslationUnit,
    options: &HeaderOptions,
    instances: &str,
) -> String {
    let mut output = String::new();
    output.push_str("/* Generated by shyc --emit=c-header. */\n");
    output.push_str(&format!(
        "#ifndef {guard}\n#define {guard}\n\n",
        guard = options.guard
    ));
    for include in &options.includes {
        let (open, close) = if include.quoted {
            ("\"", "\"")
        } else {
            ("<", ">")
        };
        output.push_str(&format!("#include {open}{}{close}\n", include.path));
    }
    if !options.includes.is_empty() {
        output.push('\n');
    }
    for define in &options.defines {
        output.push_str(define);
        output.push('\n');
    }
    if !options.defines.is_empty() {
        output.push('\n');
    }
    output.push_str("#ifdef __cplusplus\nextern \"C\" {\n#endif\n\n");

    let items = unit
        .items
        .iter()
        .filter(|item| item_span(item).file == options.root)
        .collect::<Vec<_>>();
    // Trait value records come first so a declaration anywhere in the unit can
    // name a trait type, whatever its source order. Every trait name is
    // forward-declared before any record body, so a method may mention the same
    // trait (or another one) through a pointer.
    let mut has_traits = false;
    for item in &items {
        if let Item::Trait(declaration) = item {
            has_traits = true;
            output.push_str(&format!(
                "typedef struct {name} {name};\n",
                name = declaration.name.text
            ));
        }
    }
    if has_traits {
        output.push('\n');
    }
    for item in &items {
        if let Item::Trait(declaration) = item {
            let text = trait_value_text(declaration);
            output.push_str(&text);
            output.push_str("\n\n");
        }
    }
    for item in &items {
        let text = match item {
            Item::Decl(declaration) => declaration_text(declaration),
            Item::Function(function) => prototype_text(function),
            // Methods need a symbol of their own before they can be declared;
            // a trait value record was printed above. An import is a
            // compile-time check with no C spelling.
            Item::Impl(_) | Item::Trait(_) | Item::Use(_) => String::new(),
        };
        if text.is_empty() {
            continue;
        }
        output.push_str(&text);
        output.push_str("\n\n");
    }
    if !instances.is_empty() {
        output.push_str(instances);
    }
    output.push_str("#ifdef __cplusplus\n}\n#endif\n");
    output.push_str(&format!("#endif /* {} */\n", options.guard));
    output
}

/// The source range a top-level item was parsed from.
fn item_span(item: &Item) -> Span {
    match item {
        Item::Decl(declaration) => declaration.span,
        Item::Function(function) => function.span,
        Item::Impl(block) => block.span,
        Item::Trait(declaration) => declaration.span,
        Item::Use(import) => import.span,
    }
}

// ---------------------------------------------------------------------------
// Trait value records
// ---------------------------------------------------------------------------

/// Print the C structure a trait name denotes when it is used as a value.
///
/// One function-pointer field per method, in declaration order. `self` is
/// erased to `void`, so `self *value` becomes `void *value`: the implementation
/// keeps its concrete `Circle *` symbol, and the two pointers have the same C
/// ABI. A method that takes or returns `self` by value has no erased C spelling;
/// such a trait cannot be a value type, so the header says so instead of
/// printing a structure with the wrong layout.
fn trait_value_text(declaration: &TraitDecl) -> String {
    if !declaration.generics.is_empty() {
        return format!(
            "/* trait '{}' cannot be a value type: it has generic parameters */",
            declaration.name.text
        );
    }
    if declaration.methods.is_empty() {
        return format!(
            "/* trait '{}' cannot be a value type: it has no methods */",
            declaration.name.text
        );
    }
    if let Some(method) = declaration
        .methods
        .iter()
        .find(|method| !method.generics.is_empty())
    {
        let name = method
            .declarator
            .name
            .as_ref()
            .map_or("<unnamed>", |name| name.text.as_str());
        return format!(
            "/* trait '{}' cannot be a value type: generic method '{}' is static-only */",
            declaration.name.text, name
        );
    }
    let mut fields = Vec::new();
    for method in &declaration.methods {
        let Some(field) = trait_method_field_text(method) else {
            return format!(
                "/* trait '{}' cannot be a value type: a method takes or returns \
                 `self` by value */",
                declaration.name.text
            );
        };
        fields.push(format!("    {field}"));
    }
    format!(
        "struct {} {{\n{}\n}};",
        declaration.name.text,
        fields.join("\n")
    )
}

/// Print one method as a C function-pointer field, or `None` when `self`
/// appears somewhere that cannot be erased to `void`.
fn trait_method_field_text(method: &TraitMethod) -> Option<String> {
    let name = method.declarator.name.as_ref()?;
    let Some(DerivedDeclarator::Function(function)) = method.declarator.derived.first() else {
        return None;
    };
    if specs_have_self(&method.specs) && !return_self_is_pointer(&method.declarator) {
        return None;
    }
    if function
        .params
        .iter()
        .any(|param| specs_have_self(&param.specs) && !parameter_self_is_pointer(param))
    {
        return None;
    }
    let base = specs_text(&method.specs);
    let mut declarator = declarator_text(&method.declarator);
    // `declarator_text` already wrote the return pointers and the parameter
    // list; wrapping the name in `(*name)` turns the whole thing into the
    // declarator of a pointer to that function.
    let position = declarator.find(&name.text)?;
    let end = position + name.text.len();
    declarator.replace_range(position..end, &format!("(*{})", name.text));
    Some(format!("{base} {declarator};"))
}

fn specs_have_self(specs: &DeclSpecs) -> bool {
    specs
        .type_specs
        .iter()
        .any(|specifier| matches!(specifier, TypeSpecifier::SelfType))
}

fn return_self_is_pointer(declarator: &Declarator) -> bool {
    matches!(
        declarator.derived.as_slice(),
        [
            DerivedDeclarator::Function(_),
            DerivedDeclarator::Pointer(_),
            ..
        ]
    )
}

fn parameter_self_is_pointer(param: &ParamDecl) -> bool {
    param.declarator.as_ref().is_some_and(|declarator| {
        matches!(
            declarator.derived.first(),
            Some(DerivedDeclarator::Pointer(_))
        )
    })
}

// ---------------------------------------------------------------------------
// File-scope declarations
// ---------------------------------------------------------------------------

/// Print one file-scope declaration the way a header sees it.
///
/// `static` declarations are internal to their translation unit and print
/// nothing, and so does anything that mentions a generic record: C has no
/// spelling for a template or for one of its instantiations. Everything else
/// keeps its declarators on one line, so a record definition shared by
/// `typedef struct { ... } A, *AP;` is written once instead of once per
/// declarator. Objects and prototypes become `extern`, which declares without
/// defining: a header must never define an object, and `extern` on a prototype
/// is the same linkage a definition gets.
fn declaration_text(declaration: &Declaration) -> String {
    let specs = &declaration.specs;
    if specs.storage == Some(StorageClass::Static) || declaration_uses_generics(declaration) {
        return String::new();
    }
    let base = specs_text(specs);
    if declaration.declarators.is_empty() {
        // A bare tag definition: `struct Point { ... };`.
        return format!("{base};");
    }
    let declarators = declaration
        .declarators
        .iter()
        .map(|declarator| declarator_text(&declarator.declarator))
        .collect::<Vec<_>>()
        .join(", ");
    match specs.storage {
        Some(StorageClass::Typedef) => format!("typedef {base} {declarators};"),
        _ => format!("extern {base} {declarators};"),
    }
}

/// Print the prototype of one exported function definition.
fn prototype_text(function: &FunctionDecl) -> String {
    if function.specs.storage == Some(StorageClass::Static)
        || specs_use_generics(&function.specs)
        || declarator_uses_generics(&function.declarator)
    {
        return String::new();
    }
    format!(
        "{} {};",
        specs_text(&function.specs),
        declarator_text(&function.declarator)
    )
}

/// The file-scope function names the ordinary header cannot spell.
///
/// A signature that mentions a generic instance has no C type name, so
/// [`prototype_text`] and [`declaration_text`] drop the declaration. The
/// instance section prints those prototypes from the resolved types instead,
/// where `Opt<int *>` is the compact `int *` its niche layout stores.
pub fn unspelled_function_names(unit: &TranslationUnit, root: FileId) -> HashSet<String> {
    let mut names = HashSet::new();
    for item in &unit.items {
        if item_span(item).file != root {
            continue;
        }
        match item {
            Item::Function(function) => {
                if function.specs.storage != Some(StorageClass::Static)
                    && (specs_use_generics(&function.specs)
                        || declarator_uses_generics(&function.declarator))
                    && let Some(name) = &function.declarator.name
                {
                    names.insert(name.text.clone());
                }
            }
            Item::Decl(declaration) => {
                if declaration.specs.storage != Some(StorageClass::Static)
                    && declaration_uses_generics(declaration)
                {
                    for declarator in &declaration.declarators {
                        if let Some(name) = &declarator.declarator.name {
                            names.insert(name.text.clone());
                        }
                    }
                }
            }
            _ => {}
        }
    }
    names
}

// ---------------------------------------------------------------------------
// Generic records and functions
// ---------------------------------------------------------------------------

/// Whether a declaration mentions a generic record or declares a `fn` template.
fn declaration_uses_generics(declaration: &Declaration) -> bool {
    specs_use_generics(&declaration.specs)
        || declaration
            .declarators
            .iter()
            .any(|declarator| declarator_uses_generics(&declarator.declarator))
}

fn specs_use_generics(specs: &DeclSpecs) -> bool {
    specs.type_specs.iter().any(|specifier| match specifier {
        TypeSpecifier::Generic(_) => true,
        TypeSpecifier::Record(record) => record_uses_generics(record),
        TypeSpecifier::Enum(enumeration) => enum_uses_generics(enumeration),
        _ => false,
    })
}

fn record_uses_generics(record: &RecordSpec) -> bool {
    !record.generics.is_empty()
        || record.members.as_ref().is_some_and(|members| {
            members
                .iter()
                .any(|member| specs_use_generics(&member.specs))
        })
}

fn enum_uses_generics(enumeration: &EnumSpec) -> bool {
    !enumeration.generics.is_empty()
        || enumeration.enumerators.as_ref().is_some_and(|enumerators| {
            enumerators.iter().any(|enumerator| {
                enumerator
                    .payload
                    .as_ref()
                    .is_some_and(type_name_uses_generics)
            })
        })
}

fn type_name_uses_generics(type_name: &TypeName) -> bool {
    specs_use_generics(&type_name.specs)
        || type_name
            .declarator
            .as_ref()
            .is_some_and(declarator_uses_generics)
}

fn declarator_uses_generics(declarator: &Declarator) -> bool {
    declarator.derived.iter().any(|derived| match derived {
        DerivedDeclarator::Function(function) => {
            // A `fn name<T>` template has no C spelling. Its instances are
            // lowered later, and each one has an internal symbol.
            !function.generics.is_empty()
                || function.params.iter().any(|param| {
                    specs_use_generics(&param.specs)
                        || param
                            .declarator
                            .as_ref()
                            .is_some_and(declarator_uses_generics)
                })
        }
        _ => false,
    })
}

// ---------------------------------------------------------------------------
// Specifiers and declarators
// ---------------------------------------------------------------------------

/// Print the type part of declaration specifiers.
///
/// Storage classes, `inline`, and `_Noreturn` are left to the caller: a header
/// chooses `extern` or `typedef` itself, and function specifiers describe the
/// definition, not the declaration. An empty specifier list means C's implicit
/// `int`.
fn specs_text(specs: &DeclSpecs) -> String {
    let mut words = Vec::new();
    for qualifier in &specs.qualifiers {
        words.push(qualifier_text(*qualifier).to_string());
    }
    if specs.type_specs.is_empty() {
        words.push("int".to_string());
    }
    for specifier in &specs.type_specs {
        words.push(specifier_text(specifier));
    }
    words.join(" ")
}

fn qualifier_text(qualifier: Qualifier) -> &'static str {
    match qualifier {
        Qualifier::Const => "const",
        Qualifier::Volatile => "volatile",
        Qualifier::Restrict => "restrict",
        Qualifier::Atomic => "_Atomic",
    }
}

fn specifier_text(specifier: &TypeSpecifier) -> String {
    match specifier {
        TypeSpecifier::Builtin(builtin) => builtin_text(*builtin).to_string(),
        TypeSpecifier::TypedefName(name) => name.text.clone(),
        TypeSpecifier::Generic(generic) => generic_spec_text(generic),
        TypeSpecifier::Record(record) => record_text(record),
        TypeSpecifier::Enum(enumeration) => enum_text(enumeration),
        TypeSpecifier::Atomic(type_name) => format!("_Atomic({})", type_name_text(type_name)),
        // `self` appears only in a trait method signature here. The trait
        // value structure erases the implementing type, so `self *value`
        // prints as `void *value`. A by-value `self` is caught before this
        // printer is used, because `void value` is not a C object.
        TypeSpecifier::SelfType => "void".to_string(),
    }
}

fn generic_spec_text(generic: &GenericSpec) -> String {
    format!("{}{}", generic.name.text, generic_args_text(&generic.args))
}

fn generic_params_text(params: &[GenericParam]) -> String {
    if params.is_empty() {
        return String::new();
    }
    let params = params
        .iter()
        .map(|param| {
            let mut text = match param.kind {
                GenericParamKind::Type => param.name.text.clone(),
                GenericParamKind::Const => format!("const {}", param.name.text),
            };
            if !param.bounds.is_empty() {
                let bounds = param
                    .bounds
                    .iter()
                    .map(|bound| format!("{}{}", bound.name.text, generic_args_text(&bound.args)))
                    .collect::<Vec<_>>()
                    .join(" + ");
                text.push_str(": ");
                text.push_str(&bounds);
            }
            text
        })
        .collect::<Vec<_>>()
        .join(", ");
    format!("<{params}>")
}

fn generic_args_text(args: &[GenericArg]) -> String {
    if args.is_empty() {
        return String::new();
    }
    let args = args
        .iter()
        .map(|arg| match &arg.kind {
            GenericArgKind::Type(type_name) => type_name_text(type_name),
            GenericArgKind::Const(value) => expr_text(value),
        })
        .collect::<Vec<_>>()
        .join(", ");
    format!("<{args}>")
}

fn builtin_text(builtin: BuiltinType) -> &'static str {
    match builtin {
        BuiltinType::Void => "void",
        BuiltinType::Bool => "_Bool",
        BuiltinType::Char => "char",
        BuiltinType::Short => "short",
        BuiltinType::Int => "int",
        BuiltinType::Long => "long",
        BuiltinType::Float => "float",
        BuiltinType::Double => "double",
        BuiltinType::Signed => "signed",
        BuiltinType::Unsigned => "unsigned",
    }
}

/// Print a `struct` or `union` specifier, with its definition if it has one.
///
/// Members are indented one level deeper than the record, so a nested
/// definition stays readable.
fn record_text(record: &RecordSpec) -> String {
    let keyword = match record.kind {
        RecordKind::Struct => "struct",
        RecordKind::Union => "union",
    };
    let tag = match &record.tag {
        Some(tag) => format!(" {}", tag.text),
        None => String::new(),
    };
    let generics = generic_params_text(&record.generics);
    let Some(members) = &record.members else {
        return format!("{keyword}{tag}{generics}");
    };
    let mut text = format!("{keyword}{tag}{generics} {{");
    for member in members {
        text.push('\n');
        text.push_str(&indented(&member_text(member)));
    }
    text.push_str("\n}");
    text
}

/// Print an `enum` specifier, with its underlying type and enumerators when
/// it has them.
///
/// C has no payload enum, so a definition with payload variants is printed as
/// the layout it really has: a tag followed by one union of the payloads. A
/// plain enum keeps its ordinary `enum` spelling.
fn enum_text(enumeration: &EnumSpec) -> String {
    let underlying = match &enumeration.underlying {
        Some(type_name) => format!("({})", type_name_text(type_name)),
        None => String::new(),
    };
    let tag = match &enumeration.tag {
        Some(tag) => format!(" {}", tag.text),
        None => String::new(),
    };
    let generics = generic_params_text(&enumeration.generics);
    let Some(enumerators) = &enumeration.enumerators else {
        return format!("enum{underlying}{tag}{generics}");
    };
    if enumerators
        .iter()
        .any(|enumerator| enumerator.payload.is_some())
    {
        return payload_enum_text(enumeration, enumerators);
    }
    let mut items = Vec::new();
    for enumerator in enumerators {
        items.push(match &enumerator.value {
            Some(value) => format!("{} = {}", enumerator.name.text, expr_text(value)),
            None => enumerator.name.text.clone(),
        });
    }
    format!("enum{underlying}{tag}{generics} {{ {} }}", items.join(", "))
}

/// The visible C form of a payload enum: `struct Tag { tag; union {...} payload; }`.
///
/// A payload enum without an explicit storage type uses `int` for its tag, so
/// the exported header says `int` too. Every payloadless variant still only
/// contributes its tag value and no union member.
fn payload_enum_text(enumeration: &EnumSpec, enumerators: &[Enumerator]) -> String {
    let tag_type = match &enumeration.underlying {
        Some(type_name) => type_name_text(type_name),
        None => "int".to_string(),
    };
    let tag = match &enumeration.tag {
        Some(tag) => format!(" {}", tag.text),
        None => String::new(),
    };
    let generics = generic_params_text(&enumeration.generics);
    let mut members = Vec::new();
    for enumerator in enumerators {
        if let Some(payload) = &enumerator.payload {
            members.push(format!(
                "{} {};",
                type_name_text(payload),
                enumerator.name.text
            ));
        }
    }
    format!(
        "struct{tag}{generics} {{ {tag_type} tag; union {{ {} }} payload; }}",
        members.join(" ")
    )
}

/// Print one member declaration of a `struct` or `union`.
fn member_text(member: &MemberDecl) -> String {
    let base = specs_text(&member.specs);
    if member.declarators.is_empty() {
        // A C11 anonymous struct or union member.
        return format!("{base};");
    }
    let mut declarators = Vec::new();
    for member in &member.declarators {
        let mut text = declarator_text(&member.declarator);
        if let Some(width) = &member.bit_width {
            if !text.is_empty() {
                text.push(' ');
            }
            text.push_str(&format!(": {}", expr_text(width)));
        }
        declarators.push(text);
    }
    format!("{base} {};", declarators.join(", "))
}

/// Prefix every line of `text` with one indentation level.
fn indented(text: &str) -> String {
    text.lines()
        .map(|line| format!("    {line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Print one declarator without its declaration specifiers.
///
/// [`Declarator::derived`] is ordered from the declared name outward, so the
/// operators are applied in that order: a pointer prefixes the text it
/// already has, an array or function suffix appends to it. A suffix applied to
/// a pointer needs parentheses, which is what separates `*a[3]` from `(*a)[3]`.
fn declarator_text(declarator: &Declarator) -> String {
    let mut text = match &declarator.name {
        Some(name) => name.text.clone(),
        None => String::new(),
    };
    for derived in &declarator.derived {
        match derived {
            DerivedDeclarator::Pointer(pointer) => {
                let mut prefix = "*".to_string();
                for qualifier in &pointer.qualifiers {
                    prefix.push(' ');
                    prefix.push_str(qualifier_text(*qualifier));
                }
                text = format!("{prefix}{text}");
            }
            DerivedDeclarator::Array(array) => {
                text = format!("{}{}", grouped(&text), array_text(array));
            }
            DerivedDeclarator::Function(function) => {
                text = format!("{}{}", grouped(&text), function_text(function));
            }
        }
    }
    text
}

/// Wrap `text` in parentheses before a suffix is attached to it.
fn grouped(text: &str) -> String {
    if text.starts_with('*') {
        format!("({text})")
    } else {
        text.to_string()
    }
}

fn array_text(array: &ArrayDeclarator) -> String {
    match &array.length {
        ArrayLength::Unspecified => "[]".to_string(),
        ArrayLength::Star => "[*]".to_string(),
        ArrayLength::Expr(length) => format!("[{}]", expr_text(length)),
    }
}

fn function_text(function: &FunctionDeclarator) -> String {
    let mut params = Vec::new();
    for param in &function.params {
        params.push(parameter_text(param));
    }
    if function.variadic {
        params.push("...".to_string());
    }
    format!(
        "{}({})",
        generic_params_text(&function.generics),
        params.join(", ")
    )
}

fn parameter_text(param: &ParamDecl) -> String {
    declare_text(&param.specs, param.declarator.as_ref())
}

fn type_name_text(type_name: &TypeName) -> String {
    declare_text(&type_name.specs, type_name.declarator.as_ref())
}

/// A specifier list followed by an optional declarator.
fn declare_text(specs: &DeclSpecs, declarator: Option<&Declarator>) -> String {
    let base = specs_text(specs);
    match declarator.map(declarator_text) {
        Some(text) if !text.is_empty() => format!("{base} {text}"),
        _ => base,
    }
}

// ---------------------------------------------------------------------------
// Expressions
// ---------------------------------------------------------------------------

/// Print an expression, parenthesizing every non-atomic node.
///
/// A header needs expressions only in integer constant expressions, so no
/// precedence table is needed: wrapping each operator's operands keeps the
/// meaning of the source without having to decide what binds tighter.
fn expr_text(expression: &Expr) -> String {
    match expression {
        Expr::Number(literal) => literal.text.clone(),
        Expr::Char(literal) => format!("{}'{}'", literal.prefix, literal.raw),
        Expr::String(literals) => literals
            .parts
            .iter()
            .map(|part| format!("{}\"{}\"", part.prefix, part.raw))
            .collect::<Vec<_>>()
            .join(" "),
        Expr::Name(name) => name.text.clone(),
        Expr::ScopedName {
            qualifier, name, ..
        } => {
            format!("{}::{}", qualifier.text, name.text)
        }
        Expr::GenericScopedName {
            qualifier,
            args,
            name,
            ..
        } => {
            format!(
                "{}{}::{}",
                qualifier.text,
                generic_args_text(args),
                name.text
            )
        }
        Expr::BuiltinTypeCall {
            name, type_args, ..
        } => {
            let args = type_args
                .iter()
                .map(type_name_text)
                .collect::<Vec<_>>()
                .join(", ");
            format!("{}({args})", name.text)
        }
        Expr::Unary { op, operand, .. } => {
            format!("({}{})", unary_text(*op), expr_text(operand))
        }
        Expr::Postfix { op, operand, .. } => {
            format!("({}{})", expr_text(operand), postfix_text(*op))
        }
        // A C header has no spelling for propagation, and it never appears in
        // the constant expressions a header needs.
        Expr::Try { .. } => UNSUPPORTED.to_string(),
        Expr::Binary {
            op, left, right, ..
        } => format!(
            "({} {} {})",
            expr_text(left),
            binary_text(*op),
            expr_text(right)
        ),
        Expr::Assignment {
            op, left, right, ..
        } => format!(
            "({} {} {})",
            expr_text(left),
            assign_text(*op),
            expr_text(right)
        ),
        Expr::Conditional {
            condition,
            then_expr,
            else_expr,
            ..
        } => format!(
            "({} ? {} : {})",
            expr_text(condition),
            expr_text(then_expr),
            expr_text(else_expr)
        ),
        Expr::Comma { left, right, .. } => {
            format!("({}, {})", expr_text(left), expr_text(right))
        }
        Expr::Call {
            callee,
            type_args,
            args,
            ..
        } => {
            let args = args.iter().map(expr_text).collect::<Vec<_>>().join(", ");
            format!(
                "{}{}({args})",
                expr_text(callee),
                generic_args_text(type_args)
            )
        }
        Expr::Index { array, index, .. } => {
            format!("{}[{}]", expr_text(array), expr_text(index))
        }
        Expr::Member {
            object, name, op, ..
        } => {
            let op = match op {
                MemberOp::Dot => ".",
                MemberOp::Arrow => "->",
            };
            format!("{}{op}{}", expr_text(object), name.text)
        }
        Expr::Cast {
            type_name, value, ..
        } => format!("(({}){})", type_name_text(type_name), expr_text(value)),
        Expr::SizeofType { type_name, .. } => {
            format!("sizeof({})", type_name_text(type_name))
        }
        Expr::SizeofExpr { value, .. } => format!("sizeof({})", expr_text(value)),
        Expr::AlignofType { type_name, .. } => {
            format!("_Alignof({})", type_name_text(type_name))
        }
        Expr::AlignofExpr { value, .. } => format!("_Alignof({})", expr_text(value)),
        Expr::CompoundLiteral {
            type_name, init, ..
        } => format!(
            "(({}){})",
            type_name_text(type_name),
            initializer_text(init)
        ),
        // A statement expression is not a constant expression, so a valid
        // header never reaches this case.
        Expr::StatementExpr { .. } => UNSUPPORTED.to_string(),
    }
}

fn unary_text(op: UnaryOp) -> &'static str {
    match op {
        UnaryOp::Plus => "+",
        UnaryOp::Minus => "-",
        UnaryOp::LogicalNot => "!",
        UnaryOp::BitNot => "~",
        UnaryOp::Dereference => "*",
        UnaryOp::AddressOf => "&",
        UnaryOp::PreIncrement => "++",
        UnaryOp::PreDecrement => "--",
    }
}

fn postfix_text(op: PostfixOp) -> &'static str {
    match op {
        PostfixOp::Increment => "++",
        PostfixOp::Decrement => "--",
    }
}

fn binary_text(op: BinaryOp) -> &'static str {
    match op {
        BinaryOp::Multiply => "*",
        BinaryOp::Divide => "/",
        BinaryOp::Remainder => "%",
        BinaryOp::Add => "+",
        BinaryOp::Subtract => "-",
        BinaryOp::ShiftLeft => "<<",
        BinaryOp::ShiftRight => ">>",
        BinaryOp::Less => "<",
        BinaryOp::LessEqual => "<=",
        BinaryOp::Greater => ">",
        BinaryOp::GreaterEqual => ">=",
        BinaryOp::Equal => "==",
        BinaryOp::NotEqual => "!=",
        BinaryOp::BitAnd => "&",
        BinaryOp::BitXor => "^",
        BinaryOp::BitOr => "|",
        BinaryOp::LogicalAnd => "&&",
        BinaryOp::LogicalOr => "||",
    }
}

fn assign_text(op: AssignOp) -> &'static str {
    match op {
        AssignOp::Assign => "=",
        AssignOp::Add => "+=",
        AssignOp::Subtract => "-=",
        AssignOp::Multiply => "*=",
        AssignOp::Divide => "/=",
        AssignOp::Remainder => "%=",
        AssignOp::ShiftLeft => "<<=",
        AssignOp::ShiftRight => ">>=",
        AssignOp::BitAnd => "&=",
        AssignOp::BitXor => "^=",
        AssignOp::BitOr => "|=",
    }
}

/// Print an initializer, which only a compound literal needs here.
fn initializer_text(initializer: &Initializer) -> String {
    match &initializer.kind {
        InitializerKind::Expr(expression) => expr_text(expression),
        InitializerKind::List(entries) => {
            let entries = entries
                .iter()
                .map(|entry| {
                    let mut text = String::new();
                    for designator in &entry.designators {
                        match designator {
                            Designator::Field(name) => text.push_str(&format!(".{}", name.text)),
                            Designator::Index(index) => {
                                text.push_str(&format!("[{}]", expr_text(index)));
                            }
                        }
                    }
                    if !entry.designators.is_empty() {
                        text.push_str(" = ");
                    }
                    text.push_str(&initializer_text(&entry.value));
                    text
                })
                .collect::<Vec<_>>();
            format!("{{ {} }}", entries.join(", "))
        }
    }
}
