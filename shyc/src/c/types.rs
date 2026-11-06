//! The single place a C type is spelled.
//!
//! Both the header and the source backend print through these functions, so a
//! declaration cannot drift between the two outputs. Nothing here slices source
//! text: a preprocessed span may point into an included header or a macro
//! expansion, so every printer decides the text from the AST instead.
//!
//! A construct C cannot spell prints nothing at all (static storage, generic
//! records and their instantiations), because a header declares only what a C
//! caller can name, and the source backend keeps each unit self-contained.

use crate::ast::{
    ArrayDeclarator, ArrayLength, AssignOp, BinaryOp, BuiltinType, DeclSpecs, Declaration,
    Declarator, DerivedDeclarator, Designator, EnumSpec, Enumerator, Expr, FunctionDecl,
    FunctionDeclarator, GenericArg, GenericArgKind, GenericParam, GenericParamKind, GenericSpec,
    Initializer, InitializerKind, MemberDecl, MemberOp, ParamDecl, PostfixOp, Qualifier,
    RecordKind, RecordSpec, StorageClass, TraitDecl, TraitMethod, TypeName, TypeSpecifier, UnaryOp,
};
use std::collections::HashMap;

/// How one output spells a type.
///
/// Two things vary between the outputs and even between declarations: what
/// self denotes (a trait signature erases it to void, an impl method keeps its
/// receiver) and what a generic instance is called (the instance table may give
/// it a C name of its own). Everything else about a type is fixed, so both
/// travel together instead of as separate parameters.
#[derive(Clone, Copy)]
pub(crate) struct Spelling<'a> {
    /// What self denotes here.
    pub self_name: &'a str,
    /// C type name of each generic instance, keyed by its source spelling
    /// (Vec<int>). The compiler names the instances, so the C output and the
    /// header agree; empty when nothing was specialized.
    pub instances: Option<&'a HashMap<String, String>>,
    /// Type parameters of the template being specialized, bound to the type
    /// they stand for here (T to int).
    pub bindings: Option<&'a HashMap<String, String>>,
    /// Whether generic parameter lists are printed. An instance drops them,
    /// because C has no template.
    pub generics: bool,
}

impl<'a> Spelling<'a> {
    /// The spelling outside a trait signature, without an instance table.
    pub(crate) fn plain() -> Spelling<'a> {
        Spelling {
            self_name: "void",
            instances: None,
            bindings: None,
            generics: true,
        }
    }

    /// The C name of one generic instance, from the compiler's plan.
    ///
    /// An instance the plan does not name keeps the template's name, which is
    /// what makes the C compiler report the missing definition instead of
    /// quietly inventing one.
    pub(crate) fn instance(&self, name: &str, args: &[String]) -> String {
        let key = format!("{name}<{}>", args.join(", "));
        match self.instances.and_then(|instances| instances.get(&key)) {
            Some(instance) => instance.clone(),
            None => name.to_string(),
        }
    }
}

/// Printed for a construct a C header cannot express; the host compiler then
/// reports the generated header instead of accepting a changed meaning.
const UNSUPPORTED: &str = "__shyc_unsupported_expression";

// ---------------------------------------------------------------------------
// Trait value records
// ---------------------------------------------------------------------------

/// Whether a trait can be a runtime value, that is, a structure of function
/// pointers.
pub(crate) fn trait_value_possible(declaration: &TraitDecl) -> bool {
    trait_value_problem(declaration).is_none()
}

/// Why a trait cannot be a runtime value, or None when it can.
///
/// A trait with generic parameters, no methods, a generic method, or a method
/// that takes or returns self by value has no such structure: C cannot spell a
/// template, and void cannot stand in for an object passed by value.
fn trait_value_problem(declaration: &TraitDecl) -> Option<String> {
    if !declaration.generics.is_empty() {
        return Some("it has generic parameters".to_string());
    }
    if declaration.methods.is_empty() {
        return Some("it has no methods".to_string());
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
        return Some(format!("generic method '{name}' is static-only"));
    }
    if declaration
        .methods
        .iter()
        .any(|method| trait_method_field_text(method).is_none())
    {
        return Some("a method takes or returns self by value".to_string());
    }
    None
}

/// Print the C structure a trait name denotes when it is used as a value.
///
/// One function-pointer field per method, in declaration order. Self is erased
/// to void, so a self pointer prints as a void pointer: the implementation keeps
/// its concrete symbol, and the two pointers have the same C ABI. A trait that
/// cannot be a value type prints a comment saying why instead of a structure
/// with the wrong layout.
pub(crate) fn trait_value_text(declaration: &TraitDecl) -> String {
    if let Some(problem) = trait_value_problem(declaration) {
        return format!(
            "/* trait '{}' cannot be a value type: {} */",
            declaration.name.text, problem
        );
    }
    let mut fields = Vec::new();
    for method in &declaration.methods {
        // The predicate above rejected every method without a field.
        if let Some(field) = trait_method_field_text(method) {
            fields.push(format!("    {field}"));
        }
    }
    format!(
        "struct {} {{
{}
}};",
        declaration.name.text,
        fields.join(
            "
"
        )
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
pub(crate) fn declaration_text(declaration: &Declaration) -> String {
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
pub(crate) fn prototype_text(function: &FunctionDecl) -> String {
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
// ---------------------------------------------------------------------------
// Generic records and functions
// ---------------------------------------------------------------------------

/// Whether a declaration mentions a generic record or declares a `fn` template.
pub(crate) fn declaration_uses_generics(declaration: &Declaration) -> bool {
    specs_use_generics(&declaration.specs)
        || declaration
            .declarators
            .iter()
            .any(|declarator| declarator_uses_generics(&declarator.declarator))
}

pub(crate) fn specs_use_generics(specs: &DeclSpecs) -> bool {
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

pub(crate) fn declarator_uses_generics(declarator: &Declarator) -> bool {
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
pub(crate) fn specs_text(specs: &DeclSpecs) -> String {
    specs_text_in(specs, Spelling::plain())
}

/// Print the type part of declaration specifiers, with self spelled as
/// spelling.
///
/// A trait signature erases self to void, because a trait value stores function
/// pointers that take the implementing type through void *. An impl method keeps
/// the concrete type, so the two callers need different spellings of the same
/// keyword.
pub(crate) fn specs_text_in(specs: &DeclSpecs, spelling: Spelling) -> String {
    let mut words = Vec::new();
    for qualifier in &specs.qualifiers {
        words.push(qualifier_text(*qualifier).to_string());
    }
    if specs.type_specs.is_empty() {
        words.push("int".to_string());
    }
    for specifier in &specs.type_specs {
        words.push(specifier_text(specifier, spelling));
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

fn specifier_text(specifier: &TypeSpecifier, spelling: Spelling) -> String {
    match specifier {
        TypeSpecifier::Builtin(builtin) => builtin_text(*builtin).to_string(),
        TypeSpecifier::TypedefName(name) => match spelling.bindings {
            Some(bindings) => bindings
                .get(&name.text)
                .cloned()
                .unwrap_or_else(|| name.text.clone()),
            None => name.text.clone(),
        },
        TypeSpecifier::Generic(generic) => generic_spec_text(generic, spelling),
        TypeSpecifier::Record(record) => record_text(record, spelling),
        TypeSpecifier::Enum(enumeration) => enum_text(enumeration, spelling),
        TypeSpecifier::Atomic(type_name) => {
            format!("_Atomic({})", type_name_text_in(type_name, spelling))
        }
        // `self` appears only in a trait method signature here. The trait
        // value structure erases the implementing type, so `self *value`
        // prints as `void *value`. A by-value `self` is caught before this
        // printer is used, because `void value` is not a C object.
        TypeSpecifier::SelfType => spelling.self_name.to_string(),
    }
}

/// A generic use prints as the instance name C can spell: Vec<int> is
/// Vec_int. The instance itself is a separate declaration, so this is a name,
/// not a definition.
pub(crate) fn generic_spec_text(generic: &GenericSpec, spelling: Spelling) -> String {
    let args = generic
        .args
        .iter()
        .map(|arg| match &arg.kind {
            GenericArgKind::Type(type_name) => type_name_text_in(type_name, spelling),
            GenericArgKind::Const(value) => expr_text(value),
        })
        .collect::<Vec<_>>();
    spelling.instance(&generic.name.text, &args)
}

fn generic_params_text(params: &[GenericParam], spelling: Spelling) -> String {
    if params.is_empty() || !spelling.generics {
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
                    .map(|bound| {
                        format!(
                            "{}{}",
                            bound.name.text,
                            generic_args_text(&bound.args, spelling)
                        )
                    })
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

fn generic_args_text(args: &[GenericArg], spelling: Spelling) -> String {
    if args.is_empty() {
        return String::new();
    }
    let args = args
        .iter()
        .map(|arg| match &arg.kind {
            GenericArgKind::Type(type_name) => type_name_text_in(type_name, spelling),
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
fn record_text(record: &RecordSpec, spelling: Spelling) -> String {
    let keyword = match record.kind {
        RecordKind::Struct => "struct",
        RecordKind::Union => "union",
    };
    let tag = match &record.tag {
        Some(tag) => format!(" {}", tag.text),
        None => String::new(),
    };
    let generics = generic_params_text(&record.generics, spelling);
    let Some(members) = &record.members else {
        return format!("{keyword}{tag}{generics}");
    };
    let mut text = format!("{keyword}{tag}{generics} {{");
    for member in members {
        text.push('\n');
        text.push_str(&indented(&member_text(member, spelling)));
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
fn enum_text(enumeration: &EnumSpec, spelling: Spelling) -> String {
    let underlying = match &enumeration.underlying {
        Some(type_name) => format!("({})", type_name_text(type_name)),
        None => String::new(),
    };
    let tag = match &enumeration.tag {
        Some(tag) => format!(" {}", tag.text),
        None => String::new(),
    };
    let generics = generic_params_text(&enumeration.generics, spelling);
    let Some(enumerators) = &enumeration.enumerators else {
        return format!("enum{underlying}{tag}{generics}");
    };
    if enumerators
        .iter()
        .any(|enumerator| enumerator.payload.is_some())
    {
        return payload_enum_text(enumeration, enumerators, spelling);
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
fn payload_enum_text(
    enumeration: &EnumSpec,
    enumerators: &[Enumerator],
    spelling: Spelling,
) -> String {
    let tag_type = match &enumeration.underlying {
        Some(type_name) => type_name_text(type_name),
        None => "int".to_string(),
    };
    let tag = match &enumeration.tag {
        Some(tag) => format!(" {}", tag.text),
        None => String::new(),
    };
    let generics = generic_params_text(&enumeration.generics, spelling);
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
fn member_text(member: &MemberDecl, spelling: Spelling) -> String {
    let base = specs_text_in(&member.specs, spelling);
    if member.declarators.is_empty() {
        // A C11 anonymous struct or union member.
        return format!("{base};");
    }
    let mut declarators = Vec::new();
    for member in &member.declarators {
        let mut text = declarator_text_in(&member.declarator, spelling);
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
pub(crate) fn declarator_text(declarator: &Declarator) -> String {
    declarator_text_in(declarator, Spelling::plain())
}

/// Print one declarator, spelling self as spelling in its parameters.
pub(crate) fn declarator_text_in(declarator: &Declarator, spelling: Spelling) -> String {
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
                text = format!("{}{}", grouped(&text), function_text_in(function, spelling));
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

fn function_text_in(function: &FunctionDeclarator, spelling: Spelling) -> String {
    let mut params = Vec::new();
    for param in &function.params {
        params.push(parameter_text_in(param, spelling));
    }
    if function.variadic {
        params.push("...".to_string());
    }
    format!(
        "{}({})",
        generic_params_text(&function.generics, spelling),
        params.join(", ")
    )
}

fn parameter_text_in(param: &ParamDecl, spelling: Spelling) -> String {
    declare_text_in(&param.specs, param.declarator.as_ref(), spelling)
}

pub(crate) fn type_name_text(type_name: &TypeName) -> String {
    type_name_text_in(type_name, Spelling::plain())
}

/// The same, spelled for the body that is being printed.
pub(crate) fn type_name_text_with(type_name: &TypeName, spelling: Spelling) -> String {
    type_name_text_in(type_name, spelling)
}

fn type_name_text_in(type_name: &TypeName, spelling: Spelling) -> String {
    declare_text_in(&type_name.specs, type_name.declarator.as_ref(), spelling)
}

/// A specifier list followed by an optional declarator.
fn declare_text_in(
    specs: &DeclSpecs,
    declarator: Option<&Declarator>,
    spelling: Spelling,
) -> String {
    let base = specs_text_in(specs, spelling);
    match declarator.map(|declarator| declarator_text_in(declarator, spelling)) {
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
                generic_args_text(args, Spelling::plain()),
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
                generic_args_text(type_args, Spelling::plain())
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

/// The C text of one constant generic argument, such as the `16` of
/// `Array<int, 16>`.
pub(crate) fn const_argument_text(expression: &Expr) -> String {
    expr_text(expression)
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
