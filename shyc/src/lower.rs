//! Lower the typed-syntax AST to the typed virtual-register IR.
//!
//! The AST keeps spelling rather than resolved types, so this module does the
//! semantic work in one pass over declarations and a second pass over function
//! bodies:
//!
//! 1. [`Lowerer::collect`] resolves every file-scope type, typedef, tag, enum,
//!    global object, and function signature. Global initializers are written
//!    as target bytes plus symbol relocations.
//! 2. [`Lowerer::lower_functions`] lowers the stored function bodies. Locals
//!    stay in [`ir::Slot`]s; expressions become SSA values with explicit
//!    loads, stores, casts, and pointer arithmetic.
//!
//! Shy extensions are handled here too: `impl` methods get the
//! `____Type__method` symbol, `self` names the implemented record type, dot
//! access auto-dereferences a pointer, and locals with a `drop` method are
//! dropped at scope exit, `return`, `break`, and `continue`.
//!
//! Deliberately not lowered: bit-fields, variable-length arrays, `_Atomic`
//! objects, thread-local storage, nested functions, and computed `goto`. Each
//! one produces a diagnostic that names the construct rather than generating
//! wrong code.

mod call;
mod const_eval;
mod decl;
mod expr;
mod shyc;
mod stmt;
mod type_of;
mod types;

use std::collections::HashMap;

use crate::ast;
use crate::ir;
use crate::sema::types::{QualifiedType, Qualifiers, TypeId, TypeKind, TypeStore};
use crate::source::{Diagnostic, Span};

/// One lowered translation unit.
///
/// `types` is the arena used for every type and layout fact in `module`, so a
/// backend can ask it about aggregate fields and source types after lowering.
pub struct Lowered {
    pub module: ir::Module,
    pub types: TypeStore,
}

/// Where the generated entry point comes from.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum StartupMode {
    /// Emit `_start` that calls `main`, as a hosted ShyC program does.
    #[default]
    Main,
    /// `#![no_main]`: the program provides its own entry point.
    Manual,
}

/// Frontend choices for one translation unit.
#[derive(Clone, Debug)]
pub struct Options {
    pub module_name: String,
    pub layout: ir::Layout,
    pub startup: StartupMode,
    /// Target ABI the module is lowered for. The plan is computed here, while
    /// the field layout is still known, and stored in the IR.
    pub abi: abi_plan::AbiKind,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            module_name: "module".to_string(),
            layout: ir::Layout::SHY,
            startup: StartupMode::Main,
            abi: abi_plan::AbiKind::ShyV1,
        }
    }
}

/// Lowers one parsed translation unit.
pub fn lower(unit: &ast::TranslationUnit, options: &Options) -> Result<Lowered, Diagnostic> {
    let mut lowerer = Lowerer::new(options);
    lowerer.collect(unit)?;
    lowerer.lower_functions()?;
    Ok(Lowered {
        module: lowerer.module,
        types: lowerer.types,
    })
}

// ---------------------------------------------------------------------------
// Resolved file-scope symbols
// ---------------------------------------------------------------------------

/// A file-scope object declaration and, once defined, its data index.
pub(super) struct ObjectInfo {
    pub symbol: String,
    pub ty: TypeId,
    pub qualifiers: Qualifiers,
    pub has_init: bool,
    pub data: Option<usize>,
}

/// A function declaration, prototype, or definition.
pub(super) struct FunctionInfo {
    pub symbol: String,
    pub ty: TypeId,
    pub defined: bool,
}

#[derive(Clone, Copy)]
pub(super) enum GlobalRef {
    Object(usize),
    Function(usize),
}

#[derive(Clone)]
pub(super) struct MethodInfo {
    pub symbol: String,
    pub ty: TypeId,
}

pub(super) struct ImplInfo {
    pub name: String,
    pub methods: HashMap<String, MethodInfo>,
}

/// One function definition waiting for the body-lowering pass.
pub(super) struct PendingFunction {
    pub decl: ast::FunctionDecl,
    pub symbol: String,
    pub ty: TypeId,
    pub impl_context: Option<(TypeId, String)>,
}

#[derive(Default)]
pub(super) struct TypeScope {
    pub typedefs: HashMap<String, TypeId>,
    pub tags: HashMap<String, TypeId>,
}

/// One resolved declaration specifier list.
pub(super) struct ResolvedSpecs {
    pub ty: TypeId,
    pub qualifiers: Qualifiers,
}

// ---------------------------------------------------------------------------
// The lowering context
// ---------------------------------------------------------------------------

pub(super) struct Lowerer {
    pub types: TypeStore,
    pub layout: ir::Layout,
    pub abi: abi_plan::AbiKind,
    pub module: ir::Module,
    pub objects: Vec<ObjectInfo>,
    pub functions: Vec<FunctionInfo>,
    pub globals: HashMap<String, GlobalRef>,
    pub type_scopes: Vec<TypeScope>,
    pub enum_consts: HashMap<String, (i64, TypeId)>,
    pub impls: HashMap<TypeId, ImplInfo>,
    pub impl_context: Option<(TypeId, String)>,
    pub current_function_name: Option<String>,
    pub pending: Vec<PendingFunction>,
    string_counter: u32,
    static_counter: u32,
}

impl Lowerer {
    fn new(options: &Options) -> Lowerer {
        let types = TypeStore::with_pointer_width(options.layout.pointer_bytes);
        let mut module = ir::Module::new(&options.module_name);
        module.startup = match options.startup {
            StartupMode::Main => ir::Startup::Auto {
                entry: "main".to_string(),
                stack_pointer: 0x00ff_f000,
            },
            StartupMode::Manual => ir::Startup::Manual,
        };
        Lowerer {
            types,
            layout: options.layout,
            abi: options.abi,
            module,
            objects: Vec::new(),
            functions: Vec::new(),
            globals: HashMap::new(),
            type_scopes: vec![TypeScope::default()],
            enum_consts: HashMap::new(),
            impls: HashMap::new(),
            impl_context: None,
            current_function_name: None,
            pending: Vec::new(),
            string_counter: 0,
            static_counter: 0,
        }
    }

    fn lower_functions(&mut self) -> Result<(), Diagnostic> {
        while !self.pending.is_empty() {
            let pending = self.pending.remove(0);
            let saved_context = self.impl_context.clone();
            self.impl_context = pending.impl_context.clone();
            let lowering = FnLower::new(self, pending)?;
            let function = lowering.run()?;
            self.impl_context = saved_context;
            self.module.add_function(function);
        }
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Type mapping
    // -----------------------------------------------------------------------

    /// Map a source type to the IR's type vocabulary.
    ///
    /// Aggregates become opaque `(size, align)` blobs; the source arena keeps
    /// the field layout. Function types never appear as IR values, so they map
    /// to `ptr` for the rare path that asks before decaying.
    pub(super) fn ir_type(&self, ty: TypeId) -> ir::Type {
        match self.types.kind(ty) {
            TypeKind::Void => ir::Type::Void,
            TypeKind::Bool => ir::Type::I1,
            TypeKind::Integer { rank, .. } => match rank.size() {
                1 => ir::Type::I8,
                2 => ir::Type::I16,
                4 => ir::Type::I32,
                _ => ir::Type::I64,
            },
            TypeKind::Enum(_) => ir::Type::I32,
            TypeKind::Float32 => ir::Type::F32,
            TypeKind::Float64 => ir::Type::F64,
            TypeKind::Pointer { .. } | TypeKind::Function { .. } => ir::Type::Ptr,
            TypeKind::Array { .. } | TypeKind::Record { .. } => ir::Type::aggregate(
                self.types.size_of(ty).unwrap_or(0),
                self.types.align_of(ty).unwrap_or(1),
            ),
        }
    }

    pub(super) fn size_of(&self, ty: TypeId, span: Span) -> Result<u32, Diagnostic> {
        self.types
            .size_of(ty)
            .map_err(|error| Diagnostic::new(span, format!("cannot compute object size: {error}")))
    }

    pub(super) fn align_of(&self, ty: TypeId, span: Span) -> Result<u32, Diagnostic> {
        self.types.align_of(ty).map_err(|error| {
            Diagnostic::new(span, format!("cannot compute object alignment: {error}"))
        })
    }

    pub(super) fn ir_signature(&self, function_ty: TypeId) -> ir::Signature {
        let TypeKind::Function {
            result,
            params,
            variadic,
            ..
        } = self.types.kind(function_ty)
        else {
            return ir::Signature::new(ir::Type::Void, Vec::new());
        };
        ir::Signature {
            ret: self.ir_type(*result),
            params: params.iter().map(|&param| self.ir_type(param)).collect(),
            variadic: *variadic,
        }
    }

    // -----------------------------------------------------------------------
    // Symbol helpers
    // -----------------------------------------------------------------------

    pub(super) fn object(&self, index: usize) -> &ObjectInfo {
        &self.objects[index]
    }

    pub(super) fn function(&self, index: usize) -> &FunctionInfo {
        &self.functions[index]
    }

    pub(super) fn global_ref(&self, name: &str) -> Option<GlobalRef> {
        self.globals.get(name).copied()
    }

    pub(super) fn method_of(&self, record: TypeId, name: &str) -> Option<MethodInfo> {
        self.impls
            .get(&record)
            .and_then(|info| info.methods.get(name))
            .cloned()
    }

    /// The `drop` method for a type, if an `impl` declared one.
    pub(super) fn drop_method(&self, ty: TypeId) -> Option<MethodInfo> {
        if !self.types.is_record(ty) {
            return None;
        }
        self.method_of(ty, "drop")
    }

    pub(super) fn sanitize_ident(name: &str) -> String {
        name.chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .collect()
    }

    /// Unique symbol for an anonymous object such as a string literal.
    pub(super) fn fresh_symbol(&mut self, prefix: &str) -> String {
        let index = self.string_counter;
        self.string_counter += 1;
        format!(".L.{prefix}.{index}")
    }

    pub(super) fn fresh_static_symbol(&mut self, function: &str, name: &str) -> String {
        let index = self.static_counter;
        self.static_counter += 1;
        format!(
            ".L.static.{}.{}.{}",
            Self::sanitize_ident(function),
            Self::sanitize_ident(name),
            index
        )
    }

    /// Decode adjacent string parts into their bytes and code-unit width,
    /// without the terminating zero.
    pub(super) fn decoded_string(
        &self,
        group: &ast::StringLitGroup,
    ) -> Result<(Vec<u8>, usize), Diagnostic> {
        let mut encoding = None;
        let mut bytes = Vec::new();
        for part in &group.parts {
            let value = crate::literal::string(&part.prefix, &part.raw, part.span)?;
            match encoding {
                Some(previous) if previous != value.encoding => {
                    return Err(Diagnostic::new(
                        part.span,
                        "adjacent string literals use different encodings",
                    ));
                }
                _ => encoding = Some(value.encoding),
            }
            bytes.extend_from_slice(&value.bytes);
        }
        let encoding = encoding.unwrap_or(crate::literal::Encoding::Utf8);
        Ok((bytes, encoding.unit_bytes()))
    }

    fn string_element(&mut self, unit: usize) -> TypeId {
        match unit {
            1 => self.types.builtin(crate::sema::types::Builtin::Char),
            2 => self
                .types
                .builtin(crate::sema::types::Builtin::UnsignedShort),
            _ => self.types.builtin(crate::sema::types::Builtin::UnsignedInt),
        }
    }

    /// The C array type `char[N]` (or the wide equivalent) of a string literal.
    pub(super) fn string_type(
        &mut self,
        group: &ast::StringLitGroup,
    ) -> Result<TypeId, Diagnostic> {
        let (bytes, unit) = self.decoded_string(group)?;
        let len = u32::try_from(bytes.len() / unit + 1)
            .map_err(|_| Diagnostic::new(group.span, "string literal is too long"))?;
        let element = self.string_element(unit);
        self.types
            .array_of(QualifiedType::unqualified(element), Some(len))
            .map_err(|error| {
                Diagnostic::new(group.span, format!("invalid string literal type: {error}"))
            })
    }

    /// Intern one string literal group as read-only global data.
    ///
    /// Returns the symbol and the C type of the array object, including the
    /// terminating zero code unit. Wide literals store big-endian code units,
    /// matching the target's data order.
    pub(super) fn string_global(
        &mut self,
        group: &ast::StringLitGroup,
    ) -> Result<(String, TypeId), Diagnostic> {
        let (mut bytes, unit) = self.decoded_string(group)?;
        bytes.extend(std::iter::repeat_n(0u8, unit));
        let len = u32::try_from(bytes.len() / unit)
            .map_err(|_| Diagnostic::new(group.span, "string literal is too long"))?;
        let element = self.string_element(unit);
        let array = self
            .types
            .array_of(QualifiedType::unqualified(element), Some(len))
            .map_err(|error| {
                Diagnostic::new(group.span, format!("invalid string literal type: {error}"))
            })?;
        let symbol = self.fresh_symbol("str");
        let mut global = ir::Global::with_bytes(&symbol, unit as u32, bytes);
        global.readonly = true;
        self.module.add_global(global);
        Ok((symbol, array))
    }

    /// Build the `char[N]` symbol for `__func__` / `__FUNCTION__`.
    pub(super) fn function_name_global(&mut self, function: &str) -> (String, TypeId) {
        let mut bytes = function.as_bytes().to_vec();
        bytes.push(0);
        let len = bytes.len() as u32;
        let element = self.types.builtin(crate::sema::types::Builtin::Char);
        let array = self
            .types
            .array_of(QualifiedType::unqualified(element), Some(len))
            .unwrap_or(element);
        let symbol = self.fresh_symbol("func");
        let mut global = ir::Global::with_bytes(&symbol, 1, bytes);
        global.readonly = true;
        self.module.add_global(global);
        (symbol, array)
    }

    // -----------------------------------------------------------------------
    // Lexical type scopes
    // -----------------------------------------------------------------------

    pub(super) fn push_type_scope(&mut self) {
        self.type_scopes.push(TypeScope::default());
    }

    pub(super) fn pop_type_scope(&mut self) {
        self.type_scopes.pop();
    }

    pub(super) fn push_type_scope_for_function(&mut self) {
        self.push_type_scope();
    }

    pub(super) fn declare_typedef(
        &mut self,
        name: &str,
        ty: TypeId,
        span: Span,
    ) -> Result<(), Diagnostic> {
        // C allows a typedef to be repeated with a compatible type, and the
        // standard headers rely on it when two of them define the same width.
        if let Some(previous) = self
            .type_scopes
            .iter()
            .rev()
            .find_map(|scope| scope.typedefs.get(name).copied())
        {
            if previous == ty || self.types.is_compatible(previous, ty) {
                return Ok(());
            }
            return Err(Diagnostic::new(
                span,
                format!("redefinition of typedef name '{name}'"),
            ));
        }
        let innermost = self.type_scopes.last_mut().expect("type scope stack");
        innermost.typedefs.insert(name.to_string(), ty);
        Ok(())
    }

    pub(super) fn lookup_typedef(&self, name: &str) -> Option<TypeId> {
        self.type_scopes
            .iter()
            .rev()
            .find_map(|scope| scope.typedefs.get(name).copied())
    }

    pub(super) fn lookup_tag(&self, name: &str) -> Option<TypeId> {
        self.type_scopes
            .iter()
            .rev()
            .find_map(|scope| scope.tags.get(name).copied())
    }

    pub(super) fn declare_tag(
        &mut self,
        name: &str,
        ty: TypeId,
        span: Span,
    ) -> Result<(), Diagnostic> {
        if self
            .type_scopes
            .last()
            .expect("type scope stack")
            .tags
            .contains_key(name)
        {
            return Err(Diagnostic::new(
                span,
                format!("redefinition of struct/union/enum tag '{name}'"),
            ));
        }
        self.type_scopes
            .last_mut()
            .expect("type scope stack")
            .tags
            .insert(name.to_string(), ty);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Function body lowering
// ---------------------------------------------------------------------------

/// A name visible inside a function body.
pub(super) enum LocalBinding {
    Local(usize),
    Object {
        symbol: String,
        ty: TypeId,
        qualifiers: Qualifiers,
    },
    Function {
        symbol: String,
        ty: TypeId,
    },
}

pub(super) struct FnScope {
    pub id: u32,
    pub drops: Vec<usize>,
    pub bindings: HashMap<String, LocalBinding>,
}

pub(super) struct Local {
    pub slot: ir::SlotId,
    pub ty: TypeId,
    pub qualifiers: Qualifiers,
    pub drop_symbol: Option<String>,
    pub drop_active: bool,
}

pub(super) struct LoopContext {
    pub continue_target: ir::BlockId,
    pub continue_depth: usize,
}

pub(super) struct BreakContext {
    pub target: ir::BlockId,
    pub depth: usize,
}

pub(super) struct SwitchContext {
    pub value: ir::Operand,
    pub value_ty: TypeId,
    pub ir_ty: ir::Type,
    pub terminator_block: ir::BlockId,
    pub default: Option<ir::BlockId>,
    pub cases: Vec<ir::SwitchCase>,
    pub end: ir::BlockId,
    pub span: Span,
}

#[derive(Clone)]
pub(super) struct LabelInfo {
    pub block: ir::BlockId,
    pub path: Vec<(u32, usize)>,
}

pub(super) struct PendingGoto {
    pub label: String,
    pub source: ir::BlockId,
    pub trampoline: ir::BlockId,
    pub span: Span,
    pub path: Vec<(u32, usize)>,
}

/// The value of an expression: already in the IR, or still an addressable
/// object that needs a load before use.
/// Wrap an SSA operand as a byte address.
pub(super) fn address_operand(operand: ir::Operand) -> ir::Address {
    ir::Address::new(ir::AddressBase::Operand(operand), 0)
}

/// The result of looking a plain identifier up inside a function body.
pub(super) enum Resolved {
    Local(usize),
    Object {
        symbol: String,
        ty: TypeId,
        qualifiers: Qualifiers,
    },
    Function {
        symbol: String,
        ty: TypeId,
    },
    EnumConstant {
        value: i64,
        ty: TypeId,
    },
    FuncName {
        symbol: String,
        ty: TypeId,
    },
}

pub(super) enum ExprKind {
    Value(ir::Operand),
    Place(ir::Address),
    Void,
}

pub(super) struct ExprVal {
    pub ty: TypeId,
    pub qualifiers: Qualifiers,
    pub kind: ExprKind,
}

pub(super) struct FnLower<'a> {
    pub l: &'a mut Lowerer,
    pub f: ir::Function,
    pub block: ir::BlockId,
    pub terminated: bool,
    pub scopes: Vec<FnScope>,
    pub locals: Vec<Local>,
    pub labels: HashMap<String, LabelInfo>,
    pub pending_gotos: Vec<PendingGoto>,
    pub loops: Vec<LoopContext>,
    pub breaks: Vec<BreakContext>,
    pub switches: Vec<SwitchContext>,
    pub return_ty: TypeId,
    pub is_main: bool,
    pub func_name_global: Option<(String, TypeId)>,
    pub scope_counter: u32,
    pub body: ast::Block,
}

impl<'a> FnLower<'a> {
    fn new(l: &'a mut Lowerer, pending: PendingFunction) -> Result<FnLower<'a>, Diagnostic> {
        let function_type = pending.ty;
        let (result, params) = match l.types.kind(function_type) {
            TypeKind::Function { result, params, .. } => (*result, params.clone()),
            _ => {
                return Err(Diagnostic::new(
                    pending.decl.declarator.span,
                    "declarator does not declare a function",
                ));
            }
        };
        let signature = l.ir_signature(function_type);
        let abi = crate::abi_adapter::plan_target_function(&l.types, function_type, l.abi)
            .map_err(|error| {
                Diagnostic::new(
                    pending.decl.declarator.span,
                    format!("cannot plan the target ABI: {error}"),
                )
            })?;
        let symbol = pending.symbol;
        let f = ir::Function::new(&symbol, signature, l.layout, abi);
        let function_declarator = match pending.decl.declarator.derived.first() {
            Some(ast::DerivedDeclarator::Function(function)) => Some(function),
            _ => None,
        };
        let names: Vec<String> = params
            .iter()
            .enumerate()
            .map(|(index, _)| {
                function_declarator
                    .and_then(|function| function.params.get(index))
                    .and_then(|param| param.declarator.as_ref())
                    .and_then(|declarator| declarator.name.as_ref())
                    .map(|name| name.text.clone())
                    .unwrap_or_default()
            })
            .collect();
        let block = f.entry;
        let mut lowering = FnLower {
            l,
            f,
            block,
            terminated: false,
            scopes: vec![FnScope {
                id: 0,
                drops: Vec::new(),
                bindings: HashMap::new(),
            }],
            locals: Vec::new(),
            labels: HashMap::new(),
            pending_gotos: Vec::new(),
            loops: Vec::new(),
            breaks: Vec::new(),
            switches: Vec::new(),
            return_ty: result,
            is_main: symbol == "main",
            func_name_global: None,
            scope_counter: 1,
            body: pending.decl.body,
        };
        lowering.l.push_type_scope_for_function();
        lowering.l.current_function_name = Some(symbol.clone());
        for (index, ty) in params.iter().enumerate() {
            let param = lowering.f.params[index];
            lowering.bind_parameter(&names[index], *ty, param)?;
        }
        // Inside an `impl`, the receiver is also reachable as `self`, the way
        // `int get(self) { return self.x; }` reads.
        if lowering.l.impl_context.is_some()
            && let Some((&ty, &param)) = params.first().zip(lowering.f.params.first())
            && names.first().is_some_and(|name| name != "self")
        {
            lowering.bind_parameter("self", ty, param)?;
        }
        if lowering.f.signature.variadic {
            lowering.declare_va_area()?;
        }
        Ok(lowering)
    }

    fn run(mut self) -> Result<ir::Function, Diagnostic> {
        let body = self.body.clone();
        self.lower_block(&body)?;
        self.patch_gotos()?;
        if !self.terminated {
            self.emit_all_drops();
            self.finish_fallthrough();
        }
        self.l.pop_type_scope();
        Ok(self.f)
    }
}
