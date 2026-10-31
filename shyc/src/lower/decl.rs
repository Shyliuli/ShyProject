//! File-scope declarations, global data, and local variable initialization.
//!
//! Global objects are built as target bytes plus relocations, because the IR
//! exposes raw initialized memory. Local objects get a slot and, when they
//! have an initializer, explicit stores or a `MemSet`/`MemCpy` sequence.

use super::const_eval::ConstKind;
use super::generics::generic_declarator;
use super::types::ResolvedSpecsWithName;
use super::{FnLower, GlobalRef, Local, Lowerer, ObjectInfo, PendingFunction, ResolvedSpecs};
use crate::ast;
use crate::ir;
use crate::sema::types::{QualifiedType, Qualifiers, TypeId, TypeKind};
use crate::source::{Diagnostic, Span};

impl Lowerer {
    pub(super) fn collect(&mut self, unit: &ast::TranslationUnit) -> Result<(), Diagnostic> {
        self.register_generic_records(unit)?;
        self.register_generic_functions(unit)?;
        for item in &unit.items {
            match item {
                ast::Item::Decl(declaration) => self.collect_declaration(declaration)?,
                ast::Item::Function(function) => self.collect_function(function, None)?,
                ast::Item::Impl(block) => self.collect_impl(block)?,
            }
        }
        Ok(())
    }

    /// Registers every generic record definition before anything is resolved.
    ///
    /// A use may appear before the definition it instantiates, so all templates
    /// are collected in one pass first. Generic records never enter the plain
    /// tag table: only an instantiation creates a record type.
    fn register_generic_records(&mut self, unit: &ast::TranslationUnit) -> Result<(), Diagnostic> {
        for item in &unit.items {
            let ast::Item::Decl(declaration) = item else {
                continue;
            };
            let Some(record) = generic_definition(&declaration.specs) else {
                continue;
            };
            let Some(tag) = &record.tag else {
                return Err(Diagnostic::new(
                    record.span,
                    "a generic record definition needs a tag",
                ));
            };
            if record.members.is_none() {
                return Err(Diagnostic::new(
                    record.span,
                    format!("generic record '{}' needs a definition", tag.text),
                ));
            }
            if self.generic_records.contains_key(&tag.text) {
                return Err(Diagnostic::new(
                    record.span,
                    format!("redefinition of generic record '{}'", tag.text),
                ));
            }
            self.generic_records.insert(
                tag.text.clone(),
                super::GenericRecord {
                    params: record.generics.clone(),
                    spec: record.clone(),
                },
            );
        }
        Ok(())
    }

    fn collect_declaration(&mut self, declaration: &ast::Declaration) -> Result<(), Diagnostic> {
        // The registration pass already holds the member declarations, so a
        // generic definition has nothing left to resolve. Declarators next to
        // one would use the generic record without its arguments.
        if generic_definition(&declaration.specs).is_some() {
            if let Some(init) = declaration.declarators.first() {
                return Err(Diagnostic::new(
                    init.span,
                    "a generic record needs type arguments before it can be used",
                ));
            }
            return Ok(());
        }
        // A `fn` template is not a concrete function yet; its prototype has no
        // symbol and no file-scope name until an instantiation needs one.
        if declaration
            .declarators
            .iter()
            .any(|init| generic_declarator(&init.declarator).is_some())
        {
            return Ok(());
        }
        for init in &declaration.declarators {
            self.reject_generic_declarator(&init.declarator)?;
        }
        let ResolvedSpecs { ty, qualifiers } = self.resolve_specs(&declaration.specs)?;
        let storage = declaration.specs.storage;
        if storage == Some(ast::StorageClass::Typedef) {
            for init in &declaration.declarators {
                if init.init.is_some() {
                    return Err(Diagnostic::new(
                        init.span,
                        "a typedef cannot have an initializer",
                    ));
                }
                let resolved = self.resolve_declarator(ty, qualifiers, &init.declarator)?;
                let Some(name) = resolved.name else {
                    return Err(Diagnostic::new(
                        init.span,
                        "typedef declaration needs a name",
                    ));
                };
                self.declare_typedef(&name, resolved.ty, init.span)?;
            }
            return Ok(());
        }
        if storage == Some(ast::StorageClass::Auto) || storage == Some(ast::StorageClass::Register)
        {
            return Err(Diagnostic::new(
                declaration.span,
                "`auto` and `register` are not allowed at file scope",
            ));
        }
        for init in &declaration.declarators {
            let resolved = self.resolve_object_type(
                ty,
                qualifiers,
                &init.declarator,
                init.init.as_ref(),
                init.span,
            )?;
            let Some(name) = resolved.name else {
                return Err(Diagnostic::new(init.span, "declaration needs a name"));
            };
            if self.types.is_function(resolved.ty) {
                if init.init.is_some() {
                    return Err(Diagnostic::new(
                        init.span,
                        "a function cannot have an initializer",
                    ));
                }
                let symbol = self.declared_function_symbol(&name, storage);
                self.declare_function_symbol(&name, &symbol, resolved.ty, false, init.span)?;
            } else {
                self.collect_object(
                    &name,
                    resolved.ty,
                    resolved.qualifiers,
                    storage,
                    init.init.as_ref(),
                    init.span,
                )?;
            }
        }
        Ok(())
    }

    pub(super) fn collect_function(
        &mut self,
        declaration: &ast::FunctionDecl,
        impl_context: Option<(TypeId, String)>,
    ) -> Result<(), Diagnostic> {
        if generic_declarator(&declaration.declarator).is_some() {
            if impl_context.is_some() {
                return Err(Diagnostic::new(
                    declaration.declarator.span,
                    "generic methods are not supported by lowering yet",
                ));
            }
            // A file-scope template was registered by the collection pre-pass;
            // only an instantiation creates a concrete function.
            return Ok(());
        }
        self.reject_generic_declarator(&declaration.declarator)?;
        let ResolvedSpecs { ty, qualifiers } = self.resolve_specs(&declaration.specs)?;
        let resolved = self.resolve_declarator(ty, qualifiers, &declaration.declarator)?;
        let Some(name) = resolved.name else {
            return Err(Diagnostic::new(
                declaration.span,
                "function definition needs a name",
            ));
        };
        let TypeKind::Function { .. } = self.types.kind(resolved.ty) else {
            return Err(Diagnostic::new(
                declaration.span,
                "declarator is not a function",
            ));
        };
        // An `impl` method lives in its own namespace, so the mangled symbol is
        // also its name-table key. A file-scope function is keyed by its C name
        // so that later declarations resolve to it; only the emitted symbol
        // differs for `static`.
        let (key, symbol) = match &impl_context {
            Some((_, type_name)) => {
                let symbol = format!(
                    "____{}__{}",
                    Self::sanitize_ident(type_name),
                    Self::sanitize_ident(&name)
                );
                (symbol.clone(), symbol)
            }
            None => {
                let symbol = self.declared_function_symbol(&name, declaration.specs.storage);
                (name.clone(), symbol)
            }
        };
        self.declare_function_symbol(&key, &symbol, resolved.ty, true, declaration.span)?;
        if let Some((implemented, _)) = &impl_context {
            let info = self
                .impls
                .entry(*implemented)
                .or_insert_with(|| super::ImplInfo {
                    name: String::new(),
                    methods: Default::default(),
                });
            if let Some((_, type_name)) = &impl_context {
                info.name = type_name.clone();
            }
            if info.methods.contains_key(&name) {
                return Err(Diagnostic::new(
                    declaration.span,
                    format!("redefinition of method '{name}'"),
                ));
            }
            info.methods.insert(
                name,
                super::MethodInfo {
                    symbol: symbol.clone(),
                    ty: resolved.ty,
                },
            );
        }
        self.pending.push(PendingFunction {
            decl: declaration.clone(),
            symbol,
            ty: resolved.ty,
            impl_context,
            env: None,
            internal: false,
        });
        Ok(())
    }

    /// The emitted symbol a declaration of `name` would use.
    ///
    /// An existing function keeps the symbol its first declaration chose, so a
    /// prototype and a later definition always agree.
    pub(super) fn declared_function_symbol(
        &self,
        name: &str,
        storage: Option<ast::StorageClass>,
    ) -> String {
        if let Some(GlobalRef::Function(index)) = self.global_ref(name) {
            return self.function(index).symbol.clone();
        }
        match storage {
            Some(ast::StorageClass::Static) => self.internal_symbol(name),
            _ => name.to_string(),
        }
    }

    /// Register a function, checking redeclaration compatibility.
    ///
    /// `name` is the C identifier and the key of the file-scope name table;
    /// `symbol` is the name the backend emits, which differs for a `static`
    /// function. The first declaration fixes the emitted symbol.
    pub(super) fn declare_function_symbol(
        &mut self,
        name: &str,
        symbol: &str,
        ty: TypeId,
        defined: bool,
        span: Span,
    ) -> Result<usize, Diagnostic> {
        match self.global_ref(name) {
            Some(GlobalRef::Function(index)) => {
                let (previous_ty, previous_defined) = {
                    let function = self.function(index);
                    (function.ty, function.defined)
                };
                if !self.types.is_compatible(previous_ty, ty) {
                    return Err(Diagnostic::new(
                        span,
                        format!("conflicting declaration of function '{name}'"),
                    ));
                }
                if defined && previous_defined {
                    return Err(Diagnostic::new(
                        span,
                        format!("redefinition of function '{name}'"),
                    ));
                }
                self.functions[index].defined = previous_defined || defined;
                Ok(index)
            }
            Some(GlobalRef::Object(_)) => Err(Diagnostic::new(
                span,
                format!("'{name}' is redeclared as a function"),
            )),
            None => {
                let index = self.functions.len();
                self.functions.push(super::FunctionInfo {
                    symbol: symbol.to_string(),
                    ty,
                    defined,
                });
                self.globals
                    .insert(name.to_string(), GlobalRef::Function(index));
                Ok(index)
            }
        }
    }

    fn collect_object(
        &mut self,
        name: &str,
        ty: TypeId,
        qualifiers: Qualifiers,
        storage: Option<ast::StorageClass>,
        init: Option<&ast::Initializer>,
        span: Span,
    ) -> Result<(), Diagnostic> {
        let is_extern = storage == Some(ast::StorageClass::Extern);
        let symbol = if storage == Some(ast::StorageClass::Static) {
            self.internal_symbol(name)
        } else {
            name.to_string()
        };
        let defined = init.is_some() || !is_extern;
        let index = match self.global_ref(name) {
            Some(GlobalRef::Function(_)) => {
                return Err(Diagnostic::new(
                    span,
                    format!("'{name}' is redeclared as an object"),
                ));
            }
            Some(GlobalRef::Object(index)) => {
                let (previous_ty, previous_init) = {
                    let object = self.object(index);
                    (object.ty, object.has_init)
                };
                if previous_init && init.is_some() {
                    return Err(Diagnostic::new(
                        span,
                        format!("redefinition of global object '{name}'"),
                    ));
                }
                if !self.types.is_compatible(previous_ty, ty) {
                    let incomplete_array = matches!(
                        self.types.kind(previous_ty),
                        TypeKind::Array { len: None, .. }
                    ) && self.types.is_array(ty);
                    if !incomplete_array {
                        return Err(Diagnostic::new(
                            span,
                            format!("conflicting declaration of global object '{name}'"),
                        ));
                    }
                    if init.is_none() {
                        return Err(Diagnostic::new(
                            span,
                            "array declaration without a bound after a bounded declaration",
                        ));
                    }
                }
                if init.is_some() || self.object(index).data.is_none() {
                    self.objects[index].ty = ty;
                }
                if init.is_some() {
                    self.objects[index].has_init = true;
                }
                index
            }
            None => {
                let index = self.objects.len();
                self.objects.push(ObjectInfo {
                    symbol: symbol.clone(),
                    ty,
                    qualifiers,
                    has_init: init.is_some(),
                    data: None,
                });
                self.globals
                    .insert(name.to_string(), GlobalRef::Object(index));
                index
            }
        };
        if !defined {
            return Ok(());
        }
        if init.is_none()
            && matches!(
                self.types.kind(self.objects[index].ty),
                TypeKind::Array { len: None, .. }
            )
        {
            return Err(Diagnostic::new(
                span,
                format!("tentative definition of incomplete array '{name}'"),
            ));
        }
        if init.is_some() || self.objects[index].data.is_none() {
            let object_ty = self.objects[index].ty;
            let object_qualifiers = self.objects[index].qualifiers;
            let global =
                self.build_global_data(&symbol, object_ty, object_qualifiers, init, span)?;
            let data = self.module.globals.len();
            self.module.add_global(global);
            self.objects[index].data = Some(data);
        }
        Ok(())
    }

    fn build_global_data(
        &mut self,
        symbol: &str,
        ty: TypeId,
        qualifiers: Qualifiers,
        init: Option<&ast::Initializer>,
        span: Span,
    ) -> Result<ir::Global, Diagnostic> {
        let size = self.size_of(ty, span)?;
        let align = self.align_of(ty, span)?.max(1);
        let mut global = ir::Global::with_bytes(symbol, align, vec![0; size as usize]);
        global.readonly = qualifiers.is_const;
        if let Some(init) = init {
            self.write_global_init(&mut global, ty, 0, init)?;
        }
        Ok(global)
    }

    // -----------------------------------------------------------------------
    // Global initializer bytes
    // -----------------------------------------------------------------------

    fn write_global_init(
        &mut self,
        global: &mut ir::Global,
        ty: TypeId,
        offset: u32,
        init: &ast::Initializer,
    ) -> Result<(), Diagnostic> {
        match &init.kind {
            ast::InitializerKind::Expr(expr) => {
                if self.types.is_array(ty) {
                    if let ast::Expr::String(group) = &**expr {
                        return self.write_string_bytes(global, ty, offset, group);
                    }
                    return Err(Diagnostic::new(
                        init.span,
                        "an array initializer must be a string literal or a brace-enclosed list",
                    ));
                }
                if !self.types.is_scalar(ty) {
                    return Err(Diagnostic::new(
                        init.span,
                        "this aggregate initializer needs a brace-enclosed list",
                    ));
                }
                let value = self.eval_const(expr)?;
                self.write_global_scalar(global, ty, offset, value, init.span)
            }
            ast::InitializerKind::List(entries) => {
                if self.types.is_scalar(ty) {
                    let Some(entry) = entries.first() else {
                        return Ok(());
                    };
                    if entries.len() > 1 {
                        return Err(Diagnostic::new(
                            init.span,
                            "too many initializers for a scalar",
                        ));
                    }
                    if !entry.designators.is_empty() {
                        return Err(Diagnostic::new(
                            entry.span,
                            "a designator is not valid for a scalar initializer",
                        ));
                    }
                    return self.write_global_init(global, ty, offset, &entry.value);
                }
                self.write_global_list(global, ty, offset, entries, init.span)
            }
        }
    }

    fn write_global_list(
        &mut self,
        global: &mut ir::Global,
        ty: TypeId,
        offset: u32,
        entries: &[ast::InitializerEntry],
        span: Span,
    ) -> Result<(), Diagnostic> {
        if self.types.is_array(ty) {
            let Some(element) = self.array_element(ty) else {
                return Ok(());
            };
            let len = self.array_len(ty);
            let element_size = self.size_of(element.ty, span)?;
            let mut cursor = 0u32;
            for entry in entries {
                let (sub_ty, sub_offset, top_index) = if entry.designators.is_empty() {
                    let index = cursor;
                    (element.ty, index * element_size, index)
                } else {
                    self.designator_target(ty, &entry.designators, entry.span)?
                };
                if top_index >= len {
                    return Err(Diagnostic::new(
                        entry.span,
                        "array initializer index is out of bounds",
                    ));
                }
                self.write_global_init(global, sub_ty, offset + sub_offset, &entry.value)?;
                cursor = top_index + 1;
            }
            return Ok(());
        }
        if self.types.is_record(ty) {
            let fields = self.record_fields(ty);
            if fields.is_empty() {
                return Err(Diagnostic::new(span, "initializer for an empty record"));
            }
            let is_union = matches!(
                self.types.kind(ty),
                TypeKind::Record {
                    kind: crate::sema::types::RecordKind::Union,
                    ..
                }
            );
            let mut cursor = 0u32;
            let mut initialized = 0usize;
            for entry in entries {
                let (sub_ty, sub_offset) = if entry.designators.is_empty() {
                    let field = fields.get(cursor as usize).ok_or_else(|| {
                        Diagnostic::new(entry.span, "too many initializers for this record")
                    })?;
                    cursor += 1;
                    (field.1, field.2)
                } else {
                    let (sub_ty, sub_offset, index) =
                        self.designator_target(ty, &entry.designators, entry.span)?;
                    cursor = index + 1;
                    (sub_ty, sub_offset)
                };
                self.write_global_init(global, sub_ty, offset + sub_offset, &entry.value)?;
                initialized += 1;
                if is_union && initialized > 1 {
                    return Err(Diagnostic::new(
                        entry.span,
                        "a union initializer may initialize only one member",
                    ));
                }
            }
            return Ok(());
        }
        Err(Diagnostic::new(span, "invalid aggregate initializer"))
    }

    fn write_string_bytes(
        &mut self,
        global: &mut ir::Global,
        ty: TypeId,
        offset: u32,
        group: &ast::StringLitGroup,
    ) -> Result<(), Diagnostic> {
        let (bytes, unit) = self.decode_string(group)?;
        let element = self.array_element(ty).map(|element| element.ty);
        let Some(element) = element else {
            return Err(Diagnostic::new(
                group.span,
                "string initializer needs an array",
            ));
        };
        if self.size_of(element, group.span)? as usize != unit {
            return Err(Diagnostic::new(
                group.span,
                "string literal element width does not match the array element",
            ));
        }
        let size = self.types.size_of(ty).unwrap_or(0);
        let start = offset as usize;
        if start >= global.bytes.len() || start + size as usize > global.bytes.len() {
            return Err(Diagnostic::new(
                group.span,
                "string initializer is out of bounds",
            ));
        }
        let copy = bytes.len().min(size as usize);
        global.bytes[start..start + copy].copy_from_slice(&bytes[..copy]);
        Ok(())
    }

    fn decode_string(&self, group: &ast::StringLitGroup) -> Result<(Vec<u8>, usize), Diagnostic> {
        let (mut bytes, unit) = self.decoded_string(group)?;
        bytes.extend(std::iter::repeat_n(0u8, unit));
        Ok((bytes, unit))
    }

    fn write_global_scalar(
        &mut self,
        global: &mut ir::Global,
        ty: TypeId,
        offset: u32,
        value: super::const_eval::ConstValue,
        span: Span,
    ) -> Result<(), Diagnostic> {
        let size = self.size_of(ty, span)?;
        if self.types.is_pointer(ty) {
            let value = self.const_cast(value, ty, span)?;
            let ConstKind::Address { symbol, addend } = &value.kind else {
                return Err(Diagnostic::new(
                    span,
                    "pointer initializer is not an address constant",
                ));
            };
            match symbol {
                Some(symbol) => {
                    if size != self.layout.pointer_bytes {
                        return Err(Diagnostic::new(
                            span,
                            format!(
                                "a pointer initializer needs {} bytes on this target, but the \
                                 pointer is {size} bytes",
                                self.layout.pointer_bytes
                            ),
                        ));
                    }
                    if offset + size > global.bytes.len() as u32 {
                        return Err(Diagnostic::new(
                            span,
                            "relocation is out of bounds for this object",
                        ));
                    }
                    global.add_relocation(offset, symbol.clone(), *addend);
                }
                None => {
                    let width = size as usize;
                    let bytes = target_bytes(*addend as u64, width, self.layout.endian);
                    put_bytes(global, offset, &bytes, span)?;
                }
            }
            return Ok(());
        }
        let value = self.const_cast(value, ty, span)?;
        match value.kind {
            ConstKind::Int(bits) => {
                let bytes = target_bytes(bits, size as usize, self.layout.endian);
                put_bytes(global, offset, &bytes, span)
            }
            ConstKind::Float(bits) => {
                let bytes = target_bytes(bits, size as usize, self.layout.endian);
                put_bytes(global, offset, &bytes, span)
            }
            ConstKind::Address { .. } => Err(Diagnostic::new(
                span,
                "an address constant cannot initialize a non-pointer object",
            )),
        }
    }

    // -----------------------------------------------------------------------
    // Type and field helpers
    // -----------------------------------------------------------------------

    /// Resolve an object declarator, inferring an omitted array bound from the
    /// initializer (`char s[] = "..."` and `int a[] = { ... }`).
    pub(super) fn resolve_object_type(
        &mut self,
        base: TypeId,
        qualifiers: Qualifiers,
        declarator: &ast::Declarator,
        init: Option<&ast::Initializer>,
        span: Span,
    ) -> Result<ResolvedSpecsWithName, Diagnostic> {
        let name = declarator.name.as_ref().map(|name| name.text.clone());
        let outer_unspecified = matches!(
            declarator.derived.first(),
            Some(ast::DerivedDeclarator::Array(ast::ArrayDeclarator {
                length: ast::ArrayLength::Unspecified,
                ..
            }))
        );
        if outer_unspecified && let Some(init) = init {
            let (element_ty, element_qualifiers) =
                self.apply_derived(base, qualifiers, &declarator.derived[1..])?;
            let count = self.infer_array_count(init, element_ty, span)?;
            let element = QualifiedType::new(element_ty, element_qualifiers);
            let ty = self
                .types
                .array_of(element, Some(count))
                .map_err(|error| Diagnostic::new(span, format!("invalid array type: {error}")))?;
            return Ok(ResolvedSpecsWithName {
                ty,
                qualifiers: element_qualifiers,
                name,
            });
        }
        self.resolve_declarator(base, qualifiers, declarator)
    }

    fn infer_array_count(
        &mut self,
        init: &ast::Initializer,
        element: TypeId,
        span: Span,
    ) -> Result<u32, Diagnostic> {
        match &init.kind {
            ast::InitializerKind::Expr(expr) => {
                let ast::Expr::String(group) = &**expr else {
                    return Err(Diagnostic::new(
                        span,
                        "cannot infer an array bound from this initializer",
                    ));
                };
                let (bytes, unit) = self.decode_string(group)?;
                if self.size_of(element, span)? as usize != unit {
                    return Err(Diagnostic::new(
                        group.span,
                        "string literal element width does not match the array element",
                    ));
                }
                u32::try_from(bytes.len() / unit)
                    .map_err(|_| Diagnostic::new(span, "array bound is too large"))
            }
            ast::InitializerKind::List(entries) => {
                let mut count = 0u32;
                for entry in entries {
                    match entry.designators.first() {
                        Some(ast::Designator::Index(index)) => {
                            let value = self.eval_const_int(index)?;
                            if value < 0 {
                                return Err(Diagnostic::new(
                                    index.span(),
                                    "negative array initializer index",
                                ));
                            }
                            count = count.max(value as u32 + 1);
                        }
                        Some(ast::Designator::Field(name)) => {
                            return Err(Diagnostic::new(
                                name.span,
                                "a field designator is not valid for an array",
                            ));
                        }
                        None => count = count.saturating_add(1),
                    }
                }
                Ok(count)
            }
        }
    }

    pub(super) fn array_len(&self, ty: TypeId) -> u32 {
        match self.types.kind(ty) {
            TypeKind::Array { len: Some(len), .. } => *len,
            _ => 0,
        }
    }

    /// Fields of a record as `(name, type, offset)`, in declaration order.
    pub(super) fn record_fields(&self, ty: TypeId) -> Vec<(String, TypeId, u32)> {
        let Some(info) = self.types.record_info(ty) else {
            return Vec::new();
        };
        info.fields
            .iter()
            .filter_map(|field| {
                field
                    .name
                    .as_ref()
                    .map(|name| (name.clone(), field.ty.ty, field.offset))
            })
            .collect()
    }

    pub(super) fn field_qualifiers(&self, record: TypeId, name: &str) -> Qualifiers {
        let Some(info) = self.types.record_info(record) else {
            return Qualifiers::NONE;
        };
        info.fields
            .iter()
            .find(|field| field.name.as_deref() == Some(name))
            .map(|field| field.ty.qualifiers)
            .unwrap_or(Qualifiers::NONE)
    }

    pub(super) fn field_of(
        &self,
        record: TypeId,
        name: &str,
        span: Span,
    ) -> Result<(TypeId, u32), Diagnostic> {
        self.field_index(record, name, span)
            .map(|(_, ty, offset)| (ty, offset))
    }

    /// Type, storage offset, and bit placement of a bit-field member.
    pub(super) fn bit_field(&self, record: TypeId, name: &str) -> Option<(TypeId, u32, u32, u32)> {
        let info = self.types.record_info(record)?;
        let field = info
            .fields
            .iter()
            .find(|field| field.name.as_deref() == Some(name))?;
        field
            .bit_width
            .map(|width| (field.ty.ty, field.offset, width, field.bit_offset))
    }

    pub(super) fn field_index(
        &self,
        record: TypeId,
        name: &str,
        span: Span,
    ) -> Result<(usize, TypeId, u32), Diagnostic> {
        let Some(info) = self.types.record_info(record) else {
            return Err(Diagnostic::new(span, "type is not a struct or union"));
        };
        for (index, field) in info.fields.iter().enumerate() {
            if field.name.as_deref() == Some(name) {
                // A bit-field shares its storage unit with its neighbours, so
                // reading it as an ordinary member would return the wrong
                // bits. Reject the access instead of miscompiling it.
                if field.bit_width.is_some() {
                    return Err(Diagnostic::new(
                        span,
                        format!("reading or writing the bit-field '{name}' is not supported yet"),
                    ));
                }
                return Ok((index, field.ty.ty, field.offset));
            }
        }
        Err(Diagnostic::new(span, format!("no member named '{name}'")))
    }

    /// Resolve a designator path to a sub-object type, its byte offset, and the
    /// top-level cursor position used for continuation.
    fn designator_target(
        &mut self,
        ty: TypeId,
        designators: &[ast::Designator],
        span: Span,
    ) -> Result<(TypeId, u32, u32), Diagnostic> {
        let mut current = ty;
        let mut offset = 0u32;
        let mut cursor = 0u32;
        for (index, designator) in designators.iter().enumerate() {
            match designator {
                ast::Designator::Field(name) => {
                    if !self.types.is_record(current) {
                        return Err(Diagnostic::new(
                            name.span,
                            "field designator on a non-record type",
                        ));
                    }
                    let (field_index, field_ty, field_offset) =
                        self.field_index(current, &name.text, name.span)?;
                    if index == 0 {
                        cursor = field_index as u32;
                    }
                    offset = offset
                        .checked_add(field_offset)
                        .ok_or_else(|| Diagnostic::new(name.span, "initializer offset overflow"))?;
                    current = field_ty;
                }
                ast::Designator::Index(expr) => {
                    let Some(element) = self.array_element(current) else {
                        return Err(Diagnostic::new(
                            expr.span(),
                            "array designator on a non-array type",
                        ));
                    };
                    let len = self.array_len(current);
                    let value = self.eval_const_int(expr)?;
                    if value < 0 || value >= i64::from(len) {
                        return Err(Diagnostic::new(
                            expr.span(),
                            "array designator is out of bounds",
                        ));
                    }
                    let size = self.size_of(element.ty, expr.span())?;
                    if index == 0 {
                        cursor = value as u32;
                    }
                    offset = offset.checked_add(value as u32 * size).ok_or_else(|| {
                        Diagnostic::new(expr.span(), "initializer offset overflow")
                    })?;
                    current = element.ty;
                }
            }
        }
        let _ = span;
        Ok((current, offset, cursor))
    }
}

// ---------------------------------------------------------------------------
// Local declarations and initializers
// ---------------------------------------------------------------------------

impl FnLower<'_> {
    pub(super) fn lower_declaration(
        &mut self,
        declaration: &ast::Declaration,
    ) -> Result<(), Diagnostic> {
        let ResolvedSpecs { ty, qualifiers } = self.l.resolve_specs(&declaration.specs)?;
        let storage = declaration.specs.storage;
        if storage == Some(ast::StorageClass::Typedef) {
            for init in &declaration.declarators {
                if init.init.is_some() {
                    return Err(Diagnostic::new(
                        init.span,
                        "a typedef cannot have an initializer",
                    ));
                }
                let resolved = self
                    .l
                    .resolve_declarator(ty, qualifiers, &init.declarator)?;
                let Some(name) = resolved.name else {
                    return Err(Diagnostic::new(
                        init.span,
                        "typedef declaration needs a name",
                    ));
                };
                self.l.declare_typedef(&name, resolved.ty, init.span)?;
            }
            return Ok(());
        }
        for init in &declaration.declarators {
            let resolved = self.l.resolve_object_type(
                ty,
                qualifiers,
                &init.declarator,
                init.init.as_ref(),
                init.span,
            )?;
            let Some(name) = resolved.name else {
                return Err(Diagnostic::new(init.span, "declaration needs a name"));
            };
            if self.l.types.is_function(resolved.ty) {
                if init.init.is_some() {
                    return Err(Diagnostic::new(
                        init.span,
                        "a function cannot have an initializer",
                    ));
                }
                let symbol = self.l.declared_function_symbol(&name, storage);
                self.l
                    .declare_function_symbol(&name, &symbol, resolved.ty, false, init.span)?;
                self.bind(
                    name.clone(),
                    super::LocalBinding::Function {
                        symbol,
                        ty: resolved.ty,
                    },
                );
                continue;
            }
            match storage {
                Some(ast::StorageClass::Extern) => {
                    self.l.collect_object(
                        &name,
                        resolved.ty,
                        resolved.qualifiers,
                        Some(ast::StorageClass::Extern),
                        None,
                        init.span,
                    )?;
                    self.bind(
                        name.clone(),
                        super::LocalBinding::Object {
                            symbol: name,
                            ty: resolved.ty,
                            qualifiers: resolved.qualifiers,
                        },
                    );
                    continue;
                }
                Some(ast::StorageClass::Static) => {
                    let function = self.f.name.clone();
                    let symbol = self.l.fresh_static_symbol(&function, &name);
                    let global = self.l.build_local_static(
                        &symbol,
                        resolved.ty,
                        resolved.qualifiers,
                        init.init.as_ref(),
                        init.span,
                    )?;
                    self.l.module.add_global(global);
                    self.bind(
                        name,
                        super::LocalBinding::Object {
                            symbol,
                            ty: resolved.ty,
                            qualifiers: resolved.qualifiers,
                        },
                    );
                    continue;
                }
                _ => {}
            }
            if matches!(
                self.l.types.kind(resolved.ty),
                TypeKind::Array { len: None, .. }
            ) {
                return Err(Diagnostic::new(
                    init.span,
                    format!("local array '{name}' has no known bound"),
                ));
            }
            let ir_ty = self.l.ir_type(resolved.ty);
            let align = self.l.types.align_of(resolved.ty).unwrap_or(1);
            let slot = self.f.alloc_slot_aligned(&name, ir_ty, align);
            let drop_symbol = self.l.drop_method(resolved.ty).map(|method| method.symbol);
            let active = drop_symbol.is_some();
            let local = self.locals.len();
            self.locals.push(Local {
                slot,
                ty: resolved.ty,
                qualifiers: resolved.qualifiers,
                drop_symbol,
                drop_active: active,
            });
            self.bind(name.clone(), super::LocalBinding::Local(local));
            if active {
                self.current_scope_mut().drops.push(local);
            }
            if let Some(init) = &init.init {
                self.init_local_at(ir::Address::slot(slot), resolved.ty, init)?;
                if self.locals[local].drop_symbol.is_some() {
                    self.deactivate_if_local_expr(init);
                }
            }
        }
        Ok(())
    }

    pub(super) fn init_local_at(
        &mut self,
        address: ir::Address,
        ty: TypeId,
        init: &ast::Initializer,
    ) -> Result<(), Diagnostic> {
        let span = init.span;
        match &init.kind {
            ast::InitializerKind::Expr(expr) => {
                if let ast::Expr::String(group) = &**expr
                    && self.l.types.is_array(ty)
                {
                    return self.copy_string_to(address, ty, group);
                }
                if self.l.types.is_aggregate(ty) && !self.l.types.is_array(ty) {
                    if !self.l.types.is_record(ty) {
                        return Err(Diagnostic::new(span, "invalid aggregate initializer"));
                    }
                    let value = self.lower_expr(expr)?;
                    let source_ty = value.ty;
                    if !self.l.types.is_compatible(source_ty, ty) {
                        return Err(Diagnostic::new(
                            expr.span(),
                            "initializer type does not match the object type",
                        ));
                    }
                    return self.copy_aggregate(address, value, ty, expr.span());
                }
                if self.l.types.is_array(ty) {
                    return Err(Diagnostic::new(
                        expr.span(),
                        "an array initializer must be a string literal or a brace-enclosed list",
                    ));
                }
                let (operand, operand_ty) = self.rvalue(expr)?;
                let operand = self.convert(operand, operand_ty, ty, expr.span(), false)?;
                self.store_value(address, operand, ty, Qualifiers::NONE);
                Ok(())
            }
            ast::InitializerKind::List(entries) => {
                if self.l.types.is_scalar(ty) {
                    let Some(entry) = entries.first() else {
                        return Ok(());
                    };
                    if entries.len() > 1 {
                        return Err(Diagnostic::new(span, "too many initializers for a scalar"));
                    }
                    if !entry.designators.is_empty() {
                        return Err(Diagnostic::new(
                            entry.span,
                            "a designator is not valid for a scalar initializer",
                        ));
                    }
                    return self.init_local_at(address, ty, &entry.value);
                }
                let size = self.l.size_of(ty, span)?;
                if size > 0 {
                    self.emit_statement(ir::InstKind::MemSet {
                        dst: address.clone(),
                        byte: 0,
                        bytes: size,
                    });
                }
                self.init_local_list(address, ty, entries, span)
            }
        }
    }

    fn init_local_list(
        &mut self,
        address: ir::Address,
        ty: TypeId,
        entries: &[ast::InitializerEntry],
        span: Span,
    ) -> Result<(), Diagnostic> {
        if self.l.types.is_array(ty) {
            let Some(element) = self.l.array_element(ty) else {
                return Ok(());
            };
            let len = self.l.array_len(ty);
            let element_size = self.l.size_of(element.ty, span)?;
            let mut cursor = 0u32;
            for entry in entries {
                let (sub_ty, sub_offset, top_index) = if entry.designators.is_empty() {
                    (element.ty, cursor * element_size, cursor)
                } else {
                    self.l
                        .designator_target(ty, &entry.designators, entry.span)?
                };
                if top_index >= len {
                    return Err(Diagnostic::new(
                        entry.span,
                        "array initializer index is out of bounds",
                    ));
                }
                self.init_local_at(
                    address.clone().with_offset(i64::from(sub_offset)),
                    sub_ty,
                    &entry.value,
                )?;
                cursor = top_index + 1;
            }
            return Ok(());
        }
        if self.l.types.is_record(ty) {
            let fields = self.l.record_fields(ty);
            if fields.is_empty() {
                return Err(Diagnostic::new(span, "initializer for an empty record"));
            }
            let is_union = matches!(
                self.l.types.kind(ty),
                TypeKind::Record {
                    kind: crate::sema::types::RecordKind::Union,
                    ..
                }
            );
            let mut cursor = 0u32;
            let mut initialized = 0usize;
            for entry in entries {
                let (sub_ty, sub_offset) = if entry.designators.is_empty() {
                    let field = fields.get(cursor as usize).ok_or_else(|| {
                        Diagnostic::new(entry.span, "too many initializers for this record")
                    })?;
                    cursor += 1;
                    (field.1, field.2)
                } else {
                    let (sub_ty, sub_offset, index) =
                        self.l
                            .designator_target(ty, &entry.designators, entry.span)?;
                    cursor = index + 1;
                    (sub_ty, sub_offset)
                };
                self.init_local_at(
                    address.clone().with_offset(i64::from(sub_offset)),
                    sub_ty,
                    &entry.value,
                )?;
                initialized += 1;
                if is_union && initialized > 1 {
                    return Err(Diagnostic::new(
                        entry.span,
                        "a union initializer may initialize only one member",
                    ));
                }
            }
            return Ok(());
        }
        Err(Diagnostic::new(span, "invalid aggregate initializer"))
    }

    fn copy_string_to(
        &mut self,
        address: ir::Address,
        ty: TypeId,
        group: &ast::StringLitGroup,
    ) -> Result<(), Diagnostic> {
        let (symbol, _) = self.l.string_global(group)?;
        let (bytes, _) = self.l.decode_string(group)?;
        let size = self.l.size_of(ty, group.span)?;
        let copy = (bytes.len() as u32).min(size);
        if size > 0 {
            self.emit_statement(ir::InstKind::MemSet {
                dst: address.clone(),
                byte: 0,
                bytes: size,
            });
        }
        if copy > 0 {
            self.emit_statement(ir::InstKind::MemCpy {
                dst: address,
                src: ir::Address::symbol(symbol),
                bytes: copy,
            });
        }
        Ok(())
    }

    fn copy_aggregate(
        &mut self,
        address: ir::Address,
        value: super::ExprVal,
        ty: TypeId,
        span: Span,
    ) -> Result<(), Diagnostic> {
        let size = self.l.size_of(ty, span)?;
        match value.kind {
            super::ExprKind::Place(source) => {
                self.emit_statement(ir::InstKind::MemCpy {
                    dst: address,
                    src: source,
                    bytes: size,
                });
                Ok(())
            }
            super::ExprKind::Value(operand) => {
                self.store_value(address, operand, ty, Qualifiers::NONE);
                Ok(())
            }
            super::ExprKind::Void => Err(Diagnostic::new(
                span,
                "void value used as an aggregate initializer",
            )),
        }
    }

    // -----------------------------------------------------------------------
    // Bindings
    // -----------------------------------------------------------------------

    pub(super) fn bind(&mut self, name: String, binding: super::LocalBinding) {
        self.current_scope_mut().bindings.insert(name, binding);
    }

    pub(super) fn current_scope_mut(&mut self) -> &mut super::FnScope {
        self.scopes.last_mut().expect("scope stack")
    }

    pub(super) fn bind_parameter(
        &mut self,
        name: &str,
        ty: TypeId,
        param: ir::ValueId,
    ) -> Result<(), Diagnostic> {
        if name.is_empty() {
            return Ok(());
        }
        let ir_ty = self.l.ir_type(ty);
        let align = self.l.types.align_of(ty).unwrap_or(1);
        let slot = self.f.alloc_slot_aligned(name, ir_ty, align);
        let local = self.locals.len();
        self.locals.push(Local {
            slot,
            ty,
            qualifiers: Qualifiers::NONE,
            drop_symbol: None,
            drop_active: false,
        });
        self.bind(name.to_string(), super::LocalBinding::Local(local));
        self.store_value(
            ir::Address::slot(slot),
            ir::Operand::Value(param),
            ty,
            Qualifiers::NONE,
        );
        Ok(())
    }

    pub(super) fn declare_va_area(&mut self) -> Result<(), Diagnostic> {
        let char_ty = self.l.types.builtin(crate::sema::types::Builtin::Char);
        let array = self
            .l
            .types
            .array_of(QualifiedType::unqualified(char_ty), Some(136))
            .map_err(|error| {
                Diagnostic::new(
                    Span {
                        file: crate::source::FileId(0),
                        start: 0,
                        end: 0,
                    },
                    format!("cannot build the variadic area: {error}"),
                )
            })?;
        let slot = self
            .f
            .alloc_slot_aligned("__va_area__", ir::Type::aggregate(136, 1), 1);
        let local = self.locals.len();
        self.locals.push(Local {
            slot,
            ty: array,
            qualifiers: Qualifiers::NONE,
            drop_symbol: None,
            drop_active: false,
        });
        self.bind("__va_area__".to_string(), super::LocalBinding::Local(local));
        Ok(())
    }
}

impl Lowerer {
    fn build_local_static(
        &mut self,
        symbol: &str,
        ty: TypeId,
        qualifiers: Qualifiers,
        init: Option<&ast::Initializer>,
        span: Span,
    ) -> Result<ir::Global, Diagnostic> {
        self.build_global_data(symbol, ty, qualifiers, init, span)
    }
}

fn put_bytes(
    global: &mut ir::Global,
    offset: u32,
    bytes: &[u8],
    span: Span,
) -> Result<(), Diagnostic> {
    let start = offset as usize;
    let end = start
        .checked_add(bytes.len())
        .filter(|&end| end <= global.bytes.len());
    let Some(end) = end else {
        return Err(Diagnostic::new(
            span,
            "initializer writes past the end of the object",
        ));
    };
    global.bytes[start..end].copy_from_slice(bytes);
    Ok(())
}

/// Encode the low `width` bytes of `value` in the target's byte order.
fn target_bytes(value: u64, width: usize, endian: crate::ir::Endian) -> Vec<u8> {
    let all = value.to_be_bytes();
    let tail = &all[8 - width..];
    match endian {
        crate::ir::Endian::Big => tail.to_vec(),
        crate::ir::Endian::Little => tail.iter().rev().copied().collect(),
    }
}

/// The generic record specifier in a declaration, if it declares or uses one.
fn generic_definition(specs: &ast::DeclSpecs) -> Option<&ast::RecordSpec> {
    specs.type_specs.iter().find_map(|spec| match spec {
        ast::TypeSpecifier::Record(record) if !record.generics.is_empty() => Some(record),
        _ => None,
    })
}
