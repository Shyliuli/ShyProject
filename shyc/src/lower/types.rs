//! Resolve declaration specifiers and declarators into [`TypeStore`](crate::sema::types::TypeStore)
//! types, and answer the layout, conversion, and literal questions the rest of
//! lowering needs.

use super::{Lowerer, ResolvedSpecs};
use crate::ast;
use crate::sema::types::{
    Builtin, QualifiedType, Qualifiers, RecordField, RecordKind, TypeId, TypeKind,
};
use crate::source::{Diagnostic, Span};

/// The built-in keywords seen in one `DeclSpecs` list.
#[derive(Default)]
struct BuiltinKeywords {
    signed: Option<bool>,
    longs: u32,
    short: bool,
    char_: bool,
    int_: bool,
    float_: bool,
    double_: bool,
    void_: bool,
    bool_: bool,
}

/// A literal after decoding, with the C type C gives it.
pub(super) enum NumericLiteral {
    Integer { bits: u64, ty: TypeId },
    Float { bits: u64, ty: TypeId },
}

impl Lowerer {
    // -----------------------------------------------------------------------
    // Declaration specifiers
    // -----------------------------------------------------------------------

    /// Resolve a `DeclSpecs` list into one type plus its top-level qualifiers.
    ///
    /// C's keyword soup is handled here once: `unsigned long long`, implicit
    /// `int`, typedef names that Shy also accepts as bare tags, and `self`.
    pub(super) fn resolve_specs(
        &mut self,
        specs: &ast::DeclSpecs,
    ) -> Result<ResolvedSpecs, Diagnostic> {
        let qualifiers = self.resolve_qualifiers(&specs.qualifiers, specs.span)?;
        let mut base: Option<TypeId> = None;
        let mut keywords = BuiltinKeywords::default();
        let mut builtin_seen = false;

        for spec in &specs.type_specs {
            match spec {
                ast::TypeSpecifier::Builtin(builtin) => {
                    builtin_seen = true;
                    match builtin {
                        ast::BuiltinType::Void => keywords.void_ = true,
                        ast::BuiltinType::Bool => keywords.bool_ = true,
                        ast::BuiltinType::Char => keywords.char_ = true,
                        ast::BuiltinType::Short => keywords.short = true,
                        ast::BuiltinType::Int => keywords.int_ = true,
                        ast::BuiltinType::Long => keywords.longs += 1,
                        ast::BuiltinType::Float => keywords.float_ = true,
                        ast::BuiltinType::Double => keywords.double_ = true,
                        ast::BuiltinType::Signed => {
                            if keywords.signed == Some(false) {
                                return Err(Diagnostic::new(
                                    specs.span,
                                    "both `signed` and `unsigned` in one declaration",
                                ));
                            }
                            keywords.signed = Some(true);
                        }
                        ast::BuiltinType::Unsigned => {
                            if keywords.signed == Some(true) {
                                return Err(Diagnostic::new(
                                    specs.span,
                                    "both `signed` and `unsigned` in one declaration",
                                ));
                            }
                            keywords.signed = Some(false);
                        }
                    }
                }
                ast::TypeSpecifier::TypedefName(name) => {
                    if base.is_some() {
                        return Err(Diagnostic::new(
                            name.span,
                            "more than one type specifier in one declaration",
                        ));
                    }
                    base = Some(self.lookup_named_type(&name.text).ok_or_else(|| {
                        Diagnostic::new(name.span, format!("unknown type name '{}'", name.text))
                    })?);
                }
                ast::TypeSpecifier::Record(record) => {
                    if base.is_some() {
                        return Err(Diagnostic::new(
                            record.span,
                            "more than one type specifier in one declaration",
                        ));
                    }
                    base = Some(self.resolve_record(record)?);
                }
                ast::TypeSpecifier::Enum(enumeration) => {
                    if base.is_some() {
                        return Err(Diagnostic::new(
                            enumeration.span,
                            "more than one type specifier in one declaration",
                        ));
                    }
                    base = Some(self.resolve_enum(enumeration)?);
                }
                ast::TypeSpecifier::Atomic(type_name) => {
                    if base.is_some() {
                        return Err(Diagnostic::new(
                            type_name.span,
                            "more than one type specifier in one declaration",
                        ));
                    }
                    return Err(Diagnostic::new(
                        type_name.span,
                        "`_Atomic` objects are not supported by lowering",
                    ));
                }
                ast::TypeSpecifier::SelfType => {
                    if base.is_some() {
                        return Err(Diagnostic::new(
                            specs.span,
                            "more than one type specifier in one declaration",
                        ));
                    }
                    let Some((implemented, _)) = &self.impl_context else {
                        return Err(Diagnostic::new(
                            specs.span,
                            "`self` is only valid inside an impl block",
                        ));
                    };
                    base = Some(*implemented);
                }
            }
        }

        let ty = if let Some(base) = base {
            if builtin_seen {
                return Err(Diagnostic::new(
                    specs.span,
                    "cannot combine a named type with built-in type keywords",
                ));
            }
            base
        } else {
            self.combine_builtins(specs.span, keywords)?
        };
        Ok(ResolvedSpecs { ty, qualifiers })
    }

    fn combine_builtins(
        &mut self,
        span: Span,
        keywords: BuiltinKeywords,
    ) -> Result<TypeId, Diagnostic> {
        let BuiltinKeywords {
            signed,
            longs,
            short,
            char_,
            int_,
            float_,
            double_,
            void_,
            bool_,
        } = keywords;
        let unsigned = signed == Some(false);
        let invalid = || Diagnostic::new(span, "invalid combination of type specifiers");
        if void_ {
            if signed.is_some() || longs > 0 || short || char_ || int_ || float_ || double_ || bool_
            {
                return Err(invalid());
            }
            return Ok(self.types.builtin(Builtin::Void));
        }
        if bool_ {
            if signed.is_some() || longs > 0 || short || char_ || int_ || float_ || double_ {
                return Err(invalid());
            }
            return Ok(self.types.builtin(Builtin::Bool));
        }
        if float_ || double_ {
            if float_ && double_ {
                return Err(invalid());
            }
            if longs > 0 {
                return Err(Diagnostic::new(
                    span,
                    "`long double` is not supported by the target type model",
                ));
            }
            if signed.is_some() || short || char_ || int_ || void_ || bool_ {
                return Err(invalid());
            }
            return Ok(self.types.builtin(if float_ {
                Builtin::Float
            } else {
                Builtin::Double
            }));
        }
        if char_ {
            if longs > 0 || short || int_ {
                return Err(invalid());
            }
            return Ok(self.types.builtin(if unsigned {
                Builtin::UnsignedChar
            } else {
                Builtin::Char
            }));
        }
        if short {
            if longs > 0 {
                return Err(invalid());
            }
            return Ok(self.types.builtin(if unsigned {
                Builtin::UnsignedShort
            } else {
                Builtin::Short
            }));
        }
        if longs > 2 {
            return Err(invalid());
        }
        if longs == 2 {
            return Ok(self.types.builtin(if unsigned {
                Builtin::UnsignedLongLong
            } else {
                Builtin::LongLong
            }));
        }
        if longs == 1 {
            return Ok(self.types.builtin(if unsigned {
                Builtin::UnsignedLong
            } else {
                Builtin::Long
            }));
        }
        Ok(self.types.builtin(if unsigned {
            Builtin::UnsignedInt
        } else {
            Builtin::Int
        }))
    }

    pub(super) fn resolve_qualifiers(
        &self,
        list: &[ast::Qualifier],
        span: Span,
    ) -> Result<Qualifiers, Diagnostic> {
        let mut qualifiers = Qualifiers::NONE;
        for qualifier in list {
            match qualifier {
                ast::Qualifier::Const => qualifiers.is_const = true,
                ast::Qualifier::Volatile => qualifiers.is_volatile = true,
                ast::Qualifier::Restrict => qualifiers.is_restrict = true,
                ast::Qualifier::Atomic => {
                    return Err(Diagnostic::new(
                        span,
                        "`_Atomic` objects are not supported by lowering",
                    ));
                }
            }
        }
        Ok(qualifiers)
    }

    // -----------------------------------------------------------------------
    // Declarators
    // -----------------------------------------------------------------------

    /// Apply a declarator to a base type.
    ///
    /// `derived` is ordered from the declared name outward, so it is applied in
    /// reverse: `int *a[3]` is `[Array(3), Pointer]` and becomes an array of
    /// pointers, while `int (*a)[3]` is `[Pointer, Array]` and becomes a
    /// pointer to an array.
    pub(super) fn resolve_declarator(
        &mut self,
        base: TypeId,
        base_qualifiers: Qualifiers,
        declarator: &ast::Declarator,
    ) -> Result<ResolvedSpecsWithName, Diagnostic> {
        let (ty, qualifiers) = self.apply_derived(base, base_qualifiers, &declarator.derived)?;
        Ok(ResolvedSpecsWithName {
            ty,
            qualifiers,
            name: declarator.name.as_ref().map(|name| name.text.clone()),
        })
    }

    pub(super) fn apply_derived(
        &mut self,
        base: TypeId,
        base_qualifiers: Qualifiers,
        derived: &[ast::DerivedDeclarator],
    ) -> Result<(TypeId, Qualifiers), Diagnostic> {
        let mut ty = base;
        let mut qualifiers = base_qualifiers;
        for operator in derived.iter().rev() {
            match operator {
                ast::DerivedDeclarator::Pointer(pointer) => {
                    let pointer_qualifiers =
                        self.resolve_qualifiers(&pointer.qualifiers, pointer.span)?;
                    ty = self.types.pointer_to(QualifiedType::new(ty, qualifiers));
                    qualifiers = pointer_qualifiers;
                }
                ast::DerivedDeclarator::Array(array) => {
                    let len = self.array_length(array)?;
                    let element = QualifiedType::new(ty, qualifiers);
                    ty = self.types.array_of(element, len).map_err(|error| {
                        Diagnostic::new(array.span, format!("invalid array type: {error}"))
                    })?;
                }
                ast::DerivedDeclarator::Function(function) => {
                    if self.types.is_array(ty) || self.types.is_function(ty) {
                        return Err(Diagnostic::new(
                            function.span,
                            "a function cannot return an array or another function",
                        ));
                    }
                    let (params, variadic, prototype) = self.resolve_params(function)?;
                    ty = self.types.function(ty, params, variadic, prototype);
                    qualifiers = Qualifiers::NONE;
                }
            }
        }
        Ok((ty, qualifiers))
    }

    pub(super) fn resolve_type_name(
        &mut self,
        type_name: &ast::TypeName,
    ) -> Result<TypeId, Diagnostic> {
        let ResolvedSpecs { ty, qualifiers } = self.resolve_specs(&type_name.specs)?;
        match &type_name.declarator {
            Some(declarator) => {
                let resolved = self.resolve_declarator(ty, qualifiers, declarator)?;
                Ok(resolved.ty)
            }
            None => Ok(ty),
        }
    }

    fn array_length(&mut self, array: &ast::ArrayDeclarator) -> Result<Option<u32>, Diagnostic> {
        match &array.length {
            ast::ArrayLength::Unspecified => Ok(None),
            ast::ArrayLength::Star => Err(Diagnostic::new(
                array.span,
                "variable length array (`[*]`) is not supported",
            )),
            ast::ArrayLength::Expr(expr) => {
                let value = self.eval_const_int(expr).map_err(|error| {
                    Diagnostic::new(
                        error.span,
                        format!(
                            "array length must be an integer constant expression: {}",
                            error.message
                        ),
                    )
                })?;
                if value < 0 {
                    return Err(Diagnostic::new(expr.span(), "array length is negative"));
                }
                u32::try_from(value).map(Some).map_err(|_| {
                    Diagnostic::new(expr.span(), "array length exceeds 32-bit object sizes")
                })
            }
        }
    }

    fn resolve_params(
        &mut self,
        function: &ast::FunctionDeclarator,
    ) -> Result<(Vec<TypeId>, bool, bool), Diagnostic> {
        let mut params = Vec::new();
        for (index, param) in function.params.iter().enumerate() {
            let ResolvedSpecs { ty, qualifiers } = self.resolve_specs(&param.specs)?;
            if index == 0
                && function.params.len() == 1
                && param.declarator.is_none()
                && self.types.is_void(ty)
            {
                continue;
            }
            if self.types.is_void(ty) {
                return Err(Diagnostic::new(
                    param.span,
                    "a parameter cannot have type `void`",
                ));
            }
            let ty = match &param.declarator {
                Some(declarator) => {
                    let resolved = self.resolve_declarator(ty, qualifiers, declarator)?;
                    resolved.ty
                }
                None => ty,
            };
            let adjusted = self.adjust_parameter(ty);
            params.push(adjusted);
        }
        Ok((params, function.variadic, !function.params.is_empty()))
    }

    /// C parameter adjustment: arrays and functions become pointers.
    pub(super) fn adjust_parameter(&mut self, ty: TypeId) -> TypeId {
        if let TypeKind::Array { element, .. } = self.types.kind(ty) {
            return self.types.pointer_to(*element);
        }
        if self.types.is_function(ty) {
            return self.types.pointer_to(QualifiedType::unqualified(ty));
        }
        ty
    }

    // -----------------------------------------------------------------------
    // Records and enums
    // -----------------------------------------------------------------------

    fn resolve_record(&mut self, spec: &ast::RecordSpec) -> Result<TypeId, Diagnostic> {
        let kind = match spec.kind {
            ast::RecordKind::Struct => RecordKind::Struct,
            ast::RecordKind::Union => RecordKind::Union,
        };
        let tag = spec.tag.as_ref().map(|name| name.text.clone());
        let Some(members) = &spec.members else {
            let Some(tag) = tag else {
                return Err(Diagnostic::new(
                    spec.span,
                    "record specifier needs a tag or a definition",
                ));
            };
            if let Some(existing) = self.lookup_tag(&tag) {
                match self.types.kind(existing) {
                    TypeKind::Record {
                        kind: existing_kind,
                        ..
                    } if *existing_kind == kind => return Ok(existing),
                    _ => {
                        return Err(Diagnostic::new(
                            spec.span,
                            format!("tag '{tag}' names a different kind of type"),
                        ));
                    }
                }
            }
            let ty = self.types.record_forward(kind);
            self.declare_tag(&tag, ty, spec.span)?;
            return Ok(ty);
        };

        let existing = tag.as_ref().and_then(|tag| self.tag_in_current_scope(tag));
        let ty = match existing {
            Some(existing) => {
                if self.types.get(existing).size.is_some() {
                    return Err(Diagnostic::new(
                        spec.span,
                        format!("redefinition of record '{}'", tag.as_deref().unwrap_or("")),
                    ));
                }
                match self.types.kind(existing) {
                    TypeKind::Record {
                        kind: existing_kind,
                        ..
                    } if *existing_kind == kind => existing,
                    _ => {
                        return Err(Diagnostic::new(
                            spec.span,
                            "tag names a different kind of type",
                        ));
                    }
                }
            }
            None => {
                let ty = self.types.record_forward(kind);
                if let Some(tag) = &tag {
                    self.declare_tag(tag, ty, spec.span)?;
                }
                ty
            }
        };
        let fields = self.resolve_members(members)?;
        self.types.complete_record(ty, fields).map_err(|error| {
            Diagnostic::new(spec.span, format!("invalid record layout: {error}"))
        })?;
        Ok(ty)
    }

    fn resolve_members(
        &mut self,
        members: &[ast::MemberDecl],
    ) -> Result<Vec<RecordField>, Diagnostic> {
        let mut fields = Vec::new();
        let mut names: Vec<String> = Vec::new();
        for member in members {
            let ResolvedSpecs { ty, qualifiers } = self.resolve_specs(&member.specs)?;
            if member.declarators.is_empty() {
                return Err(Diagnostic::new(
                    member.span,
                    "anonymous struct/union members are not supported",
                ));
            }
            for declarator in &member.declarators {
                if let Some(width) = &declarator.bit_width {
                    return Err(Diagnostic::new(
                        width.span(),
                        "bit-fields are not supported",
                    ));
                }
                let resolved = self.resolve_declarator(ty, qualifiers, &declarator.declarator)?;
                let Some(name) = resolved.name else {
                    return Err(Diagnostic::new(
                        declarator.span,
                        "a record member needs a name",
                    ));
                };
                if names.contains(&name) {
                    return Err(Diagnostic::new(
                        declarator.span,
                        format!("duplicate member '{name}'"),
                    ));
                }
                names.push(name.clone());
                fields.push(RecordField::new(
                    name,
                    QualifiedType::new(resolved.ty, resolved.qualifiers),
                ));
            }
        }
        Ok(fields)
    }

    fn resolve_enum(&mut self, spec: &ast::EnumSpec) -> Result<TypeId, Diagnostic> {
        let Some(enumerators) = &spec.enumerators else {
            let Some(tag) = &spec.tag else {
                return Err(Diagnostic::new(
                    spec.span,
                    "enum specifier needs a tag or a definition",
                ));
            };
            if let Some(existing) = self.lookup_tag(&tag.text)
                && matches!(self.types.kind(existing), TypeKind::Enum(_))
            {
                return Ok(existing);
            }
            return Err(Diagnostic::new(
                spec.span,
                format!("unknown enum tag '{}'", tag.text),
            ));
        };
        let ty = self.types.new_enum();
        if let Some(tag) = &spec.tag {
            if self.tag_in_current_scope(&tag.text).is_some() {
                return Err(Diagnostic::new(
                    spec.span,
                    format!("redefinition of enum '{}'", tag.text),
                ));
            }
            self.declare_tag(&tag.text, ty, spec.span)?;
        }
        let mut next = 0i64;
        for enumerator in enumerators {
            let value = match &enumerator.value {
                Some(expr) => self.eval_const_int(expr)?,
                None => next,
            };
            next = value.wrapping_add(1);
            if self.enum_consts.contains_key(&enumerator.name.text) {
                return Err(Diagnostic::new(
                    enumerator.name.span,
                    format!(
                        "redefinition of enumerator constant '{}'",
                        enumerator.name.text
                    ),
                ));
            }
            self.enum_consts
                .insert(enumerator.name.text.clone(), (value, ty));
        }
        Ok(ty)
    }

    fn tag_in_current_scope(&self, name: &str) -> Option<TypeId> {
        self.type_scopes
            .last()
            .and_then(|scope| scope.tags.get(name).copied())
    }

    // -----------------------------------------------------------------------
    // Type queries used by expression lowering
    // -----------------------------------------------------------------------

    pub(super) fn lookup_named_type(&self, name: &str) -> Option<TypeId> {
        self.lookup_typedef(name).or_else(|| self.lookup_tag(name))
    }

    pub(super) fn integer_promotion(&self, ty: TypeId) -> TypeId {
        self.types
            .integer_promotion(ty)
            .unwrap_or_else(|_| self.types.builtin(Builtin::Int))
    }

    pub(super) fn usual_arithmetic(
        &self,
        left: TypeId,
        right: TypeId,
        span: Span,
    ) -> Result<TypeId, Diagnostic> {
        self.types
            .usual_arithmetic_conversions(left, right)
            .map_err(|error| Diagnostic::new(span, format!("invalid arithmetic operands: {error}")))
    }

    pub(super) fn is_unsigned(&self, ty: TypeId) -> bool {
        match self.types.kind(ty) {
            TypeKind::Bool => true,
            TypeKind::Integer { unsigned, .. } => *unsigned,
            TypeKind::Enum(_) => self
                .types
                .enum_underlying(ty)
                .is_some_and(|underlying| self.is_unsigned(underlying)),
            _ => false,
        }
    }

    pub(super) fn pointer_to(&mut self, ty: TypeId) -> TypeId {
        self.types.pointer_to(QualifiedType::unqualified(ty))
    }

    /// Array-to-pointer and function-to-pointer decay for value contexts.
    pub(super) fn decay_type(&mut self, ty: TypeId) -> TypeId {
        if let TypeKind::Array { element, .. } = self.types.kind(ty) {
            return self.types.pointer_to(*element);
        }
        if self.types.is_function(ty) {
            return self.types.pointer_to(QualifiedType::unqualified(ty));
        }
        ty
    }

    pub(super) fn array_element(&self, ty: TypeId) -> Option<QualifiedType> {
        match self.types.kind(ty) {
            TypeKind::Array { element, .. } => Some(*element),
            _ => None,
        }
    }

    pub(super) fn pointee(&self, ty: TypeId) -> Option<QualifiedType> {
        match self.types.kind(ty) {
            TypeKind::Pointer { pointee } => Some(*pointee),
            _ => None,
        }
    }

    /// Strip one pointer level for dot auto-dereference or array decay.
    pub(super) fn record_of(&self, ty: TypeId) -> Option<TypeId> {
        if self.types.is_record(ty) {
            return Some(ty);
        }
        match self.types.kind(ty) {
            TypeKind::Pointer { pointee } if self.types.is_record(pointee.ty) => Some(pointee.ty),
            _ => None,
        }
    }

    // -----------------------------------------------------------------------
    // Literals
    // -----------------------------------------------------------------------

    pub(super) fn number_literal(
        &self,
        literal: &ast::NumberLit,
    ) -> Result<NumericLiteral, Diagnostic> {
        let text = literal.text.as_str();
        let lower = text.to_ascii_lowercase();
        let is_float = if lower.starts_with("0x") {
            lower.contains('.') || lower.contains('p')
        } else {
            lower.contains('.') || lower.contains('e')
        };
        if is_float {
            return self.float_literal(literal);
        }
        let integer = crate::literal::integer(text, literal.span)?;
        let ty = match (integer.rank, integer.unsigned) {
            (crate::literal::IntegerRank::Int, false) => Builtin::Int,
            (crate::literal::IntegerRank::Int, true) => Builtin::UnsignedInt,
            (crate::literal::IntegerRank::Long, false) => Builtin::Long,
            (crate::literal::IntegerRank::Long, true) => Builtin::UnsignedLong,
            (crate::literal::IntegerRank::LongLong, false) => Builtin::LongLong,
            (crate::literal::IntegerRank::LongLong, true) => Builtin::UnsignedLongLong,
        };
        let ty = self.types.builtin(ty);
        let width = self.types.size_of(ty).unwrap_or(4) * 8;
        let bits = mask_width(integer.bits, width);
        Ok(NumericLiteral::Integer { bits, ty })
    }

    fn float_literal(&self, literal: &ast::NumberLit) -> Result<NumericLiteral, Diagnostic> {
        let mut text = literal.text.as_str();
        let suffix = text.chars().last().unwrap_or('0');
        let ty = match suffix {
            'f' | 'F' => {
                text = &text[..text.len() - 1];
                Builtin::Float
            }
            'l' | 'L' => {
                return Err(Diagnostic::new(
                    literal.span,
                    "`long double` literals are not supported by the target type model",
                ));
            }
            _ => Builtin::Double,
        };
        let value = if text.starts_with("0x") || text.starts_with("0X") {
            parse_hex_float(text).ok_or_else(|| {
                Diagnostic::new(literal.span, "invalid hexadecimal floating literal")
            })?
        } else {
            text.parse::<f64>()
                .map_err(|_| Diagnostic::new(literal.span, "invalid floating literal"))?
        };
        let ty = self.types.builtin(ty);
        let bits = if self.types.size_of(ty) == Ok(4) {
            (value as f32).to_bits() as u64
        } else {
            value.to_bits()
        };
        Ok(NumericLiteral::Float { bits, ty })
    }

    pub(super) fn char_literal(&self, literal: &ast::CharLit) -> Result<(u64, TypeId), Diagnostic> {
        let value = crate::literal::character(&literal.prefix, &literal.raw, literal.span)?;
        let builtin = match literal.prefix.as_str() {
            "" => Builtin::Int,
            "u8" => Builtin::UnsignedChar,
            "u" => Builtin::UnsignedShort,
            _ => Builtin::UnsignedInt,
        };
        let ty = self.types.builtin(builtin);
        let width = self.types.size_of(ty).unwrap_or(4) * 8;
        Ok((mask_width(value as u64, width), ty))
    }
}

/// A declarator resolved to a type, its top-level qualifiers, and its name.
pub(super) struct ResolvedSpecsWithName {
    pub ty: TypeId,
    pub qualifiers: Qualifiers,
    pub name: Option<String>,
}

fn mask_width(bits: u64, width: u32) -> u64 {
    if width >= 64 {
        bits
    } else {
        bits & ((1u64 << width) - 1)
    }
}

/// Parse the `0x...p...` spelling C99 allows for floating literals.
fn parse_hex_float(text: &str) -> Option<f64> {
    let text = text
        .strip_prefix("0x")
        .or_else(|| text.strip_prefix("0X"))?;
    let (mantissa, exponent) = match text.find(['p', 'P']) {
        Some(index) => (&text[..index], text[index + 1..].parse::<i32>().ok()?),
        None => (text, 0),
    };
    let (whole, fraction) = match mantissa.split_once('.') {
        Some((whole, fraction)) => (whole, fraction),
        None => (mantissa, ""),
    };
    let mut value = 0f64;
    for digit in whole.chars().chain(fraction.chars()) {
        value = value * 16.0 + digit.to_digit(16)? as f64;
    }
    value *= 2f64.powi(-4 * fraction.len() as i32);
    Some(value * 2f64.powi(exponent))
}
