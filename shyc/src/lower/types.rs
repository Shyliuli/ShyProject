//! Resolve declaration specifiers and declarators into [`TypeStore`](crate::sema::types::TypeStore)
//! types, and answer the layout, conversion, and literal questions the rest of
//! lowering needs.

use super::{InstanceKey, Lowerer, ResolvedSpecs};
use crate::ast;
use crate::sema::types::{
    Builtin, EnumVariant, QualifiedType, Qualifiers, RecordField, RecordKind, TypeId, TypeKind,
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
                    base = Some(match self.lookup_named_type(&name.text) {
                        Some(ty) => ty,
                        None if self.generic_records.contains_key(&name.text)
                            || self.generic_enums.contains_key(&name.text) =>
                        {
                            return Err(Diagnostic::new(
                                name.span,
                                format!("generic type '{}' needs arguments", name.text),
                            ));
                        }
                        None if self.traits.contains_key(&name.text) => {
                            // A trait used as a type is its transparent record
                            // of function pointers.
                            self.trait_value_type(name)?
                        }
                        None => {
                            return Err(Diagnostic::new(
                                name.span,
                                format!("unknown type name '{}'", name.text),
                            ));
                        }
                    });
                }
                ast::TypeSpecifier::Generic(generic) => {
                    if base.is_some() {
                        return Err(Diagnostic::new(
                            generic.span,
                            "more than one type specifier in one declaration",
                        ));
                    }
                    base = Some(self.instantiate_generic(
                        &generic.name,
                        &generic.args,
                        generic.span,
                    )?);
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
                            "`self` is only valid inside an impl or trait declaration",
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
            // `long double` is `double`: the IR has one 64-bit float type, and
            // a program that only passes it through headers behaves the same.
            // Code that really needs extended precision must not use it here.
            let _ = longs;
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
        self.reject_generic_declarator(declarator)?;
        let (ty, qualifiers) = self.apply_derived(base, base_qualifiers, &declarator.derived)?;
        Ok(ResolvedSpecsWithName {
            ty,
            qualifiers,
            name: declarator.name.as_ref().map(|name| name.text.clone()),
        })
    }

    /// Rejects a generic function before its return type is resolved, so the
    /// diagnostic names generics instead of the unknown type parameter.
    pub(super) fn reject_generic_declarator(
        &self,
        declarator: &ast::Declarator,
    ) -> Result<(), Diagnostic> {
        for derived in &declarator.derived {
            if let ast::DerivedDeclarator::Function(function) = derived
                && let Some(parameter) = function.generics.first()
            {
                return Err(Diagnostic::new(
                    parameter.span,
                    "generic functions are not supported by lowering yet",
                ));
            }
        }
        Ok(())
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
                    if !function.generics.is_empty() {
                        return Err(Diagnostic::new(
                            function.generics[0].span,
                            "generic functions are not supported by lowering yet",
                        ));
                    }
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
            let ty = match &param.declarator {
                Some(declarator) => {
                    let resolved = self.resolve_declarator(ty, qualifiers, declarator)?;
                    resolved.ty
                }
                None => ty,
            };
            // Check the finished parameter type: `void *p` has a `void`
            // specifier but is a perfectly ordinary pointer parameter, so the
            // test has to happen after the declarator's operators are applied.
            if self.types.is_void(ty) {
                return Err(Diagnostic::new(
                    param.span,
                    "a parameter cannot have type `void`",
                ));
            }
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
        // File-scope generic definitions were registered by `collect`, so a
        // generic specifier that reaches here was reached another way.
        if let Some(parameter) = spec.generics.first() {
            return Err(Diagnostic::new(
                parameter.span,
                "generic record definitions are only supported at file scope",
            ));
        }
        let kind = match spec.kind {
            ast::RecordKind::Struct => RecordKind::Struct,
            ast::RecordKind::Union => RecordKind::Union,
        };
        let tag = spec.tag.as_ref().map(|name| name.text.clone());
        // A generic record has no plain tag: using its name without arguments
        // must not create a second, non-generic record.
        if let Some(tag) = &tag
            && self.generic_records.contains_key(tag)
        {
            return Err(Diagnostic::new(
                spec.span,
                format!("generic type '{tag}' needs arguments"),
            ));
        }
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

    /// Resolve the qualifier of `Name<args>::member` to its concrete type.
    ///
    /// Both generic records and generic enums instantiate through the same
    /// cache the type-specifier path uses, so a qualifier and a spelled type
    /// share one identity.
    pub(super) fn generic_qualifier_type(
        &mut self,
        qualifier: &ast::Name,
        args: &[ast::GenericArg],
    ) -> Result<TypeId, Diagnostic> {
        self.instantiate_generic(qualifier, args, qualifier.span)
    }

    /// Instantiate `Name<args>`: validate the arguments, then re-resolve the
    /// generic record or enum template with its parameters bound.
    fn instantiate_generic(
        &mut self,
        name: &ast::Name,
        args: &[ast::GenericArg],
        span: Span,
    ) -> Result<TypeId, Diagnostic> {
        /// How deep `Pair<Pair<...>>` may nest before instantiation gives up.
        const MAX_DEPTH: u32 = 64;

        if self.instantiation_depth >= MAX_DEPTH {
            return Err(Diagnostic::new(
                span,
                format!("generic type instantiation is nested more than {MAX_DEPTH} levels deep"),
            ));
        }
        if let Some(template) = self.generic_records.get(&name.text).cloned() {
            return self.instantiate_generic_record(name, args, span, &template);
        }
        if let Some(template) = self.generic_enums.get(&name.text).cloned() {
            return self.instantiate_generic_enum(name, args, &template);
        }
        if self.lookup_named_type(&name.text).is_some() {
            return Err(Diagnostic::new(
                name.span,
                format!("type '{}' is not generic", name.text),
            ));
        }
        Err(Diagnostic::new(
            name.span,
            format!("unknown generic type '{}'", name.text),
        ))
    }

    /// Instantiate a generic record template into a concrete record.
    ///
    /// The forward record is cached before its members resolve, so a member may
    /// mention its own instantiation (`Node<T> *next`) and every use of the
    /// same arguments gets the same record identity back.
    fn instantiate_generic_record(
        &mut self,
        name: &ast::Name,
        args: &[ast::GenericArg],
        span: Span,
        template: &super::GenericRecord,
    ) -> Result<TypeId, Diagnostic> {
        if args.len() != template.params.len() {
            return Err(Diagnostic::new(
                span,
                format!(
                    "generic type '{}' expects {} argument(s), but {} were given",
                    name.text,
                    template.params.len(),
                    args.len()
                ),
            ));
        }
        // Arguments resolve in the caller's scope, never the template's.
        let args = self.resolve_generic_args(&name.text, &template.params, args)?;
        let key = InstanceKey {
            name: name.text.clone(),
            args,
        };
        if let Some(&ty) = self.record_instances.get(&key) {
            return Ok(ty);
        }
        let kind = match template.spec.kind {
            ast::RecordKind::Struct => RecordKind::Struct,
            ast::RecordKind::Union => RecordKind::Union,
        };
        let Some(members) = &template.spec.members else {
            return Err(Diagnostic::new(
                template.spec.span,
                format!("generic record '{}' has no definition", name.text),
            ));
        };
        let ty = self.types.record_forward(kind);
        self.record_instances.insert(key.clone(), ty);
        self.instantiation_depth += 1;
        self.push_type_scope();
        let resolved = (|| {
            self.bind_generic_args(&template.params, &key.args);
            let fields = self.resolve_members(members)?;
            self.types.complete_record(ty, fields).map_err(|error| {
                Diagnostic::new(
                    template.spec.span,
                    format!("invalid record layout: {error}"),
                )
            })
        })();
        self.pop_type_scope();
        self.instantiation_depth -= 1;
        resolved.map(|_| ty)
    }

    /// Instantiate a generic enum template into a concrete payload enum.
    ///
    /// The instance is cached as soon as its type exists, before the payload
    /// type names resolve, so a payload may mention the enum's own
    /// instantiation behind a pointer and repeated uses share one identity.
    /// Enum constants stay scoped to `Name<args>::Variant`, so an instance
    /// registers none of them in the file-scope constant table.
    fn instantiate_generic_enum(
        &mut self,
        name: &ast::Name,
        args: &[ast::GenericArg],
        template: &super::GenericEnum,
    ) -> Result<TypeId, Diagnostic> {
        if args.len() != template.params.len() {
            return Err(Diagnostic::new(
                name.span,
                format!(
                    "generic enum '{}' expects {} argument(s), but {} were given",
                    name.text,
                    template.params.len(),
                    args.len()
                ),
            ));
        }
        // Arguments resolve in the caller's scope, never the template's.
        let args = self.resolve_generic_args(&name.text, &template.params, args)?;
        let key = InstanceKey {
            name: name.text.clone(),
            args,
        };
        if let Some(&ty) = self.enum_instances.get(&key) {
            return Ok(ty);
        }
        let Some(enumerators) = &template.spec.enumerators else {
            return Err(Diagnostic::new(
                template.spec.span,
                format!("generic enum '{}' has no definition", name.text),
            ));
        };
        self.instantiation_depth += 1;
        self.push_type_scope();
        let resolved = (|| {
            self.bind_generic_args(&template.params, &key.args);
            let underlying = match &template.spec.underlying {
                Some(type_name) => self.resolve_enum_underlying(type_name)?,
                None => self.types.builtin(Builtin::Int),
            };
            let has_payload = enumerators.iter().any(|e| e.payload.is_some());
            let ty = if has_payload {
                self.types.new_payload_enum(underlying)
            } else {
                self.types.new_enum(underlying)
            }
            .map_err(|error| {
                Diagnostic::new(template.spec.span, format!("invalid enum layout: {error}"))
            })?;
            self.enum_instances.insert(key.clone(), ty);
            self.define_enum(ty, underlying, enumerators, template.spec.span, false)?;
            Ok::<TypeId, Diagnostic>(ty)
        })();
        self.pop_type_scope();
        self.instantiation_depth -= 1;
        resolved
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
                let bit_width = match &declarator.bit_width {
                    Some(width) => {
                        let value = self.eval_const_int(width)?;
                        if !(0..=64).contains(&value) {
                            return Err(Diagnostic::new(
                                width.span(),
                                "a bit-field width must be between 0 and 64",
                            ));
                        }
                        Some(value as u32)
                    }
                    None => None,
                };
                let resolved = self.resolve_declarator(ty, qualifiers, &declarator.declarator)?;
                // An unnamed bit-field only pads the layout; it has no member
                // to look up, so it is dropped from the record's field list.
                let Some(name) = resolved.name else {
                    if bit_width.is_some() {
                        continue;
                    }
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
                let mut field =
                    RecordField::new(name, QualifiedType::new(resolved.ty, resolved.qualifiers));
                field.bit_width = bit_width;
                fields.push(field);
            }
        }
        Ok(fields)
    }

    fn resolve_enum(&mut self, spec: &ast::EnumSpec) -> Result<TypeId, Diagnostic> {
        // File-scope generic definitions were registered by `collect`, so a
        // generic specifier that reaches here was reached another way.
        if let Some(parameter) = spec.generics.first() {
            return Err(Diagnostic::new(
                parameter.span,
                "generic enum definitions are only supported at file scope",
            ));
        }
        let explicit = match &spec.underlying {
            Some(type_name) => Some(self.resolve_enum_underlying(type_name)?),
            None => None,
        };
        let Some(enumerators) = &spec.enumerators else {
            let Some(tag) = &spec.tag else {
                return Err(Diagnostic::new(
                    spec.span,
                    "enum specifier needs a tag or a definition",
                ));
            };
            if self.generic_enums.contains_key(&tag.text) {
                return Err(Diagnostic::new(
                    spec.span,
                    format!("generic enum '{}' needs arguments", tag.text),
                ));
            }
            let Some(existing) = self.lookup_tag(&tag.text) else {
                return Err(Diagnostic::new(
                    spec.span,
                    format!("unknown enum tag '{}'", tag.text),
                ));
            };
            let Some(stored) = self.types.enum_underlying(existing) else {
                return Err(Diagnostic::new(
                    spec.span,
                    format!("unknown enum tag '{}'", tag.text),
                ));
            };
            if let Some(explicit) = explicit
                && !self.types.is_compatible(explicit, stored)
            {
                return Err(Diagnostic::new(
                    spec.underlying.as_ref().map_or(spec.span, |ty| ty.span),
                    format!(
                        "enum '{}' was already declared with a different underlying type",
                        tag.text
                    ),
                ));
            }
            return Ok(existing);
        };

        // Without an explicit storage type the tag keeps a plain enum's `int`
        // default; choosing the narrowest tag that fits is future work.
        let underlying = explicit.unwrap_or_else(|| self.types.builtin(Builtin::Int));
        // A variant with a payload makes the whole enum an aggregate object;
        // a payload-free enum keeps the existing integer representation.
        let has_payload = enumerators.iter().any(|e| e.payload.is_some());
        let ty = if has_payload {
            self.types.new_payload_enum(underlying)
        } else {
            self.types.new_enum(underlying)
        }
        .map_err(|error| Diagnostic::new(spec.span, format!("invalid enum layout: {error}")))?;
        if let Some(tag) = &spec.tag {
            if let Some(existing) = self.tag_in_current_scope(&tag.text) {
                if let Some(explicit) = explicit
                    && let Some(stored) = self.types.enum_underlying(existing)
                    && !self.types.is_compatible(explicit, stored)
                {
                    return Err(Diagnostic::new(
                        spec.underlying.as_ref().map_or(spec.span, |ty| ty.span),
                        format!(
                            "enum '{}' was already declared with a different underlying type",
                            tag.text
                        ),
                    ));
                }
                return Err(Diagnostic::new(
                    spec.span,
                    format!("redefinition of enum '{}'", tag.text),
                ));
            }
            self.declare_tag(&tag.text, ty, spec.span)?;
        }
        self.define_enum(ty, underlying, enumerators, spec.span, true)
    }

    /// Defines an enum's variants and constants.
    ///
    /// `constants` controls whether payload-free variants also become
    /// file-scope constants under their bare name. A plain enum and a direct
    /// payload enum keep that behaviour; an instantiated generic enum does
    /// not, because two instances would otherwise collide on the same variant
    /// names. Every generic variant is reached as `Name<args>::Variant`.
    fn define_enum(
        &mut self,
        ty: TypeId,
        underlying: TypeId,
        enumerators: &[ast::Enumerator],
        span: Span,
        constants: bool,
    ) -> Result<TypeId, Diagnostic> {
        let (min, max, bits, unsigned) = self.enum_value_range(underlying);
        let payload_enum = self.types.is_payload_enum(ty);
        let mut variants = Vec::new();
        let mut next: i128 = 0;
        for enumerator in enumerators {
            let value = match &enumerator.value {
                Some(expr) => self.eval_enum_value(expr)?,
                None => next,
            };
            let span = match &enumerator.value {
                Some(expr) => expr.span(),
                None => enumerator.name.span,
            };
            if unsigned && value < 0 {
                return Err(Diagnostic::new(
                    span,
                    format!(
                        "enumerator '{}' cannot be negative for an unsigned underlying type",
                        enumerator.name.text
                    ),
                ));
            }
            if value < min || value > max {
                return Err(Diagnostic::new(
                    span,
                    format!(
                        "enumerator '{}' value {} does not fit in the {}-bit {} underlying type",
                        enumerator.name.text,
                        value,
                        bits,
                        if unsigned { "unsigned" } else { "signed" }
                    ),
                ));
            }
            next = value + 1;
            let payload = match &enumerator.payload {
                Some(type_name) => {
                    let payload = self.resolve_type_name(type_name)?;
                    if self.types.is_void(payload)
                        || self.types.is_function(payload)
                        || !self.types.is_complete(payload)
                    {
                        return Err(Diagnostic::new(
                            type_name.span,
                            "a variant payload must be a complete object type",
                        ));
                    }
                    Some(payload)
                }
                None => None,
            };
            if payload_enum {
                if variants
                    .iter()
                    .any(|variant: &EnumVariant| variant.name == enumerator.name.text)
                {
                    return Err(Diagnostic::new(
                        enumerator.name.span,
                        format!("duplicate variant '{}'", enumerator.name.text),
                    ));
                }
                variants.push(EnumVariant {
                    name: enumerator.name.text.clone(),
                    value: value as i64,
                    payload,
                });
            }
            // A payload variant has no integer value of its own; it is only
            // constructible as `Enum::Variant(value)`.
            if payload.is_some() || !constants {
                continue;
            }
            if self.enum_consts.contains_key(&enumerator.name.text) {
                return Err(Diagnostic::new(
                    enumerator.name.span,
                    format!(
                        "redefinition of enumerator constant '{}'",
                        enumerator.name.text
                    ),
                ));
            }
            let int = self.types.builtin(Builtin::Int);
            self.enum_consts
                .insert(enumerator.name.text.clone(), (value as i64, int));
        }
        if payload_enum {
            self.types
                .complete_payload_enum(ty, variants)
                .map_err(|error| {
                    Diagnostic::new(span, format!("invalid payload enum layout: {error}"))
                })?;
        }
        Ok(ty)
    }

    /// Validate and resolve the type in `enum(type)`.
    fn resolve_enum_underlying(&mut self, type_name: &ast::TypeName) -> Result<TypeId, Diagnostic> {
        let ty = self.resolve_type_name(type_name)?;
        let invalid = |message: &str| Diagnostic::new(type_name.span, message);
        match self.types.kind(ty) {
            TypeKind::Integer { .. } => {
                if self.types.size_of(ty).is_err() {
                    return Err(invalid("enum underlying type must be complete"));
                }
                Ok(ty)
            }
            TypeKind::Bool => Err(invalid("enum underlying type cannot be `_Bool`")),
            TypeKind::Enum(_) | TypeKind::PayloadEnum(_) => {
                Err(invalid("enum underlying type cannot be another enum"))
            }
            TypeKind::Float32 | TypeKind::Float64 => {
                Err(invalid("enum underlying type cannot be floating point"))
            }
            TypeKind::Pointer { .. } => Err(invalid("enum underlying type cannot be a pointer")),
            _ => Err(invalid(
                "enum underlying type must be a complete integer type",
            )),
        }
    }

    /// The signed or unsigned value range of an already validated enum
    /// underlying type, with its width in bits.
    fn enum_value_range(&self, underlying: TypeId) -> (i128, i128, u32, bool) {
        let (bits, unsigned) = match self.types.kind(underlying) {
            TypeKind::Integer { rank, unsigned } => (rank.size() * 8, *unsigned),
            _ => unreachable!("enum underlying type was validated"),
        };
        if unsigned {
            (0, (1i128 << bits) - 1, bits, true)
        } else {
            (
                -(1i128 << (bits - 1)),
                (1i128 << (bits - 1)) - 1,
                bits,
                false,
            )
        }
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
    /// The record an object denotes, looking through any pointer depth.
    ///
    /// ShyC dereferences automatically, so `P **r` denotes a `P`.
    pub(super) fn record_of(&self, mut ty: TypeId) -> Option<TypeId> {
        while !self.types.is_record(ty) {
            let TypeKind::Pointer { pointee } = self.types.kind(ty) else {
                return None;
            };
            ty = pointee.ty;
        }
        Some(ty)
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
            // A `long double` literal is a `double` literal here, for the same
            // reason the type is.
            'l' | 'L' => {
                text = &text[..text.len() - 1];
                Builtin::Double
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
