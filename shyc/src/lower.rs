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
//! Shy extensions are handled here too: inherent `impl` methods get the
//! `____Type__method` symbol, trait methods add the trait name and trait
//! arguments, and a materialized generic impl gets one internal symbol per
//! receiver, trait, and method. `self` names the implemented record type,
//! dot access auto-dereferences a pointer, and locals with a `drop` method are
//! dropped at scope exit, `return`, `break`, and `continue`.
//!
//! Deliberately not lowered: bit-fields, variable-length arrays, `_Atomic`
//! objects, thread-local storage, nested functions, and computed `goto`. Each
//! one produces a diagnostic that names the construct rather than generating
//! wrong code.

mod call;
mod const_eval;
mod decl;
mod deref;
mod export;
mod expr;
mod generics;
mod propagate;
mod shyc;
mod stmt;
mod type_of;
mod types;

use std::collections::{HashMap, HashSet};

use crate::ast;
use crate::export_instances::{CTypeName, ExportedInstance};
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
    /// The concrete instances named by the explicit instance table, in table
    /// order. Empty when the driver has no instance table.
    pub exports: Vec<ExportedInstance>,
    /// C spelling facts for every record and enum type an exported signature
    /// can mention. The exporter never guesses a name from a type id.
    pub c_types: HashMap<TypeId, CTypeName>,
    /// Every specialization the compiler materialized, in the shape the C
    /// backend prints: the template, its concrete arguments spelled as C, and
    /// the type name or symbol the compiler gave it.
    pub instances: Vec<InstancePlan>,
    /// The symbol every call expression resolved to, by span and receiver.
    pub call_symbols: HashMap<(Span, Option<String>), String>,
    /// The symbol every function and method definition emits, by span.
    pub declaration_symbols: HashMap<Span, String>,
    /// Externally visible file-scope functions by C name, in name order. The
    /// ordinary header drops a declaration whose type mentions a generic
    /// instance, and the instance section prints its prototype from here.
    pub c_functions: Vec<(String, TypeId)>,
}

/// A generic method the compiler materialized.
///
/// The receiver and the impl template are kept as the compiler resolved them,
/// so the C export prints this instance from the template's body instead of
/// guessing which method a name belongs to.
#[derive(Clone, Debug)]
pub struct MethodInstancePlan {
    pub name: String,
    pub record: TypeId,
    pub impl_name: String,
    pub trait_name: Option<String>,
    pub args: Vec<GenericArgKey>,
    pub symbol: String,
}

/// Every specialization the compiler materialized, ready for the C export.
///
/// A record instance is named by the type it became and a function or method
/// instance by its symbol; both are the compiler's answer, so the C output and
/// the generated header name one instance the same way.
fn instance_plans(
    lowerer: &Lowerer,
    c_types: &HashMap<TypeId, crate::export_instances::CTypeName>,
) -> Vec<InstancePlan> {
    // A plain record is declared under its source tag, so its C spelling is the
    // tag; a generic instance is spelled by the type printer.
    let spell = |ty: TypeId| match lowerer.type_names.get(&ty) {
        Some(tag) => tag.clone(),
        None => crate::export_instances::type_text(&lowerer.types, c_types, ty),
    };
    let text = |arg: &GenericArgKey| match arg {
        // The C backend declares a plain record under its source tag, so an
        // argument naming one has to use the same spelling.
        GenericArgKey::Type(ty) => spell(*ty),
        GenericArgKey::Int(value) => value.to_string(),
    };
    let mut plans = Vec::new();
    for (key, ty) in &lowerer.record_instances {
        plans.push(InstancePlan {
            name: key.name.clone(),
            args: key.args.iter().map(text).collect(),
            type_name: Some(spell(*ty)),
            layout: Some(crate::export_instances::layout_text(
                &lowerer.types,
                c_types,
                *ty,
            )),
            symbol: None,
            receiver: None,
            impl_name: None,
            trait_name: None,
        });
    }
    for (key, instance) in &lowerer.function_instances {
        plans.push(InstancePlan {
            name: key.name.clone(),
            args: key.args.iter().map(text).collect(),
            type_name: None,
            symbol: Some(instance.symbol.clone()),
            receiver: None,
            impl_name: None,
            trait_name: None,
            layout: None,
        });
    }
    for method in &lowerer.method_instances {
        plans.push(InstancePlan {
            name: method.name.clone(),
            args: method.args.iter().map(text).collect(),
            type_name: None,
            symbol: Some(method.symbol.clone()),
            receiver: Some(spell(method.record)),
            impl_name: Some(method.impl_name.clone()),
            trait_name: method.trait_name.clone(),
            layout: None,
        });
    }
    // The caches are hash maps, so the order they hand back is not the order
    // the C output should have; two runs of one input must print one file.
    plans.sort_by(|left, right| {
        (&left.name, &left.args, &left.receiver).cmp(&(&right.name, &right.args, &right.receiver))
    });
    plans.dedup_by(|left, right| {
        left.name == right.name && left.args == right.args && left.receiver == right.receiver
    });
    plans
}

/// One specialization the compiler materialized.
#[derive(Clone, Debug)]
pub struct InstancePlan {
    /// The template's name as written: a record tag, a function, or a method.
    pub name: String,
    /// The concrete arguments, spelled as the C type printer spells them.
    pub args: Vec<String>,
    /// The C name of the instantiated record or enum.
    pub type_name: Option<String>,
    /// The symbol of the instantiated function or method.
    pub symbol: Option<String>,
    /// For a method instance: the C name of its receiver type.
    pub receiver: Option<String>,
    /// For a method instance: the impl block it came from, as written.
    pub impl_name: Option<String>,
    /// For a method instance of a trait impl: the trait's name.
    pub trait_name: Option<String>,
    /// The complete C definition of a record or enum instance, printed by the
    /// same printer that writes the header.
    pub layout: Option<String>,
}

/// One instance the explicit table asks the C boundary to expose.
#[derive(Clone, Debug)]
pub struct ExportRequest {
    /// The generic template name.
    pub name: ast::Name,
    /// Concrete type and `const` arguments, resolved in file scope.
    pub args: Vec<ast::GenericArg>,
    /// The C identifier the instance is exported under.
    pub alias: String,
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
    /// `#![disable_nonexhaustive_switch]`: do not diagnose a `switch` on a
    /// plain C enum that has no `default` label. Payload enums are closed and
    /// keep their exhaustiveness check either way.
    pub disable_nonexhaustive_switch: bool,
    /// `#![disable_rustic_mut]`: keep ordinary C writable defaults, so a `let`
    /// local without `mut` may be assigned. This changes checking only.
    pub disable_rustic_mut: bool,
    /// `#![disable_default_static]`: keep the C linkage default, so only
    /// `static` gives a file-local symbol.
    pub disable_default_static: bool,
    /// Concrete generic instances the explicit instance table exports. Each
    /// one is instantiated even when the unit never uses it, and its functions
    /// get a public C symbol.
    pub exports: Vec<ExportRequest>,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            module_name: "module".to_string(),
            layout: ir::Layout::SHY,
            startup: StartupMode::Main,
            abi: abi_plan::AbiKind::ShyV1,
            disable_nonexhaustive_switch: false,
            disable_rustic_mut: false,
            disable_default_static: false,
            exports: Vec::new(),
        }
    }
}

/// Lowers one parsed translation unit.
pub fn lower(unit: &ast::TranslationUnit, options: &Options) -> Result<Lowered, Diagnostic> {
    let mut lowerer = Lowerer::new(options);
    lowerer.collect(unit)?;
    // Exports are instantiated after every file-scope name exists and before
    // any body is lowered, so a call site already sees the exported symbol.
    let exports = lowerer.export_instances(&options.exports)?;
    lowerer.lower_functions()?;
    let c_types = lowerer.c_type_names(&exports);
    let c_functions = lowerer.c_functions();
    let instances = instance_plans(&lowerer, &c_types);
    Ok(Lowered {
        instances,
        call_symbols: std::mem::take(&mut lowerer.call_symbols),
        declaration_symbols: std::mem::take(&mut lowerer.declaration_symbols),
        module: lowerer.module,
        types: lowerer.types,
        exports,
        c_types,
        c_functions,
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
    /// The body of a materialized generic impl method, queued for lowering the
    /// first time method lookup needs it.
    pub deferred: Option<DeferredMethod>,
}

#[derive(Clone)]
pub(super) struct DerefStep {
    pub name: &'static str,
    pub method: MethodInfo,
    pub target: TypeId,
}

#[derive(Clone)]
pub(super) struct RecordPath {
    pub record: TypeId,
}

#[derive(Clone)]
pub(super) struct AutoMethod {
    pub method: MethodInfo,
    pub steps: Vec<DerefStep>,
}

/// A generic impl method whose signature exists but whose body has not been
/// queued yet.
///
/// Only the methods an instance actually uses are lowered, so a method that is
/// valid for one type argument does not make the impl invalid for another one
/// it never runs on.
#[derive(Clone)]
pub(super) struct DeferredMethod {
    pub decl: ast::FunctionDecl,
    pub env: GenericEnv,
    pub impl_context: (TypeId, String),
}

/// Identity of one trait implementation: the trait name plus its resolved
/// type or `const` arguments. A second implementation of the same trait with
/// different arguments therefore has a different key, and so does every
/// concrete receiver.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) struct TraitImplKey {
    pub name: String,
    pub args: Vec<GenericArgKey>,
}

/// The methods of one trait implementation.
///
/// A trait method is stored separately from an inherent method, so the same
/// method name may be implemented by an inherent block and by several traits,
/// each with its own symbol and signature. A receiver call looks through this
/// table only when no inherent method has the name.
#[derive(Default, Clone)]
pub(super) struct TraitMethodSet {
    pub methods: HashMap<String, MethodInfo>,
    /// Methods that declare their own generic parameters. A generic method has
    /// no single concrete signature, so it is instantiated like a generic
    /// function when a call names concrete arguments or when they are inferred
    /// from the value arguments.
    pub generic_methods: HashMap<String, GenericMethod>,
}

pub(super) struct ImplInfo {
    pub name: String,
    /// Methods of `impl Type`.
    pub methods: HashMap<String, MethodInfo>,
    /// Generic methods of `impl Type`.
    pub generic_methods: HashMap<String, GenericMethod>,
    /// Methods of `impl Trait<Args> for Type`, keyed by the trait identity.
    pub trait_methods: HashMap<TraitImplKey, TraitMethodSet>,
}

impl ImplInfo {
    pub(super) fn new(name: String) -> ImplInfo {
        ImplInfo {
            name,
            methods: HashMap::new(),
            generic_methods: HashMap::new(),
            trait_methods: HashMap::new(),
        }
    }
}

/// One generic method of an impl, kept as the AST template it is.
///
/// The receiver's bindings (`env`) come from the surrounding impl: an ordinary
/// `impl Circle` contributes none, and a materialized `impl<T> Box<T>`
/// contributes its concrete arguments. Instantiation adds the method's own
/// arguments on top and lowers one ordinary function through the generic
/// function machinery.
#[derive(Clone)]
pub(super) struct GenericMethod {
    pub params: Vec<ast::GenericParam>,
    pub decl: ast::FunctionDecl,
    pub env: GenericEnv,
    pub impl_context: (TypeId, String),
    /// The trait implementation this method belongs to, empty for an inherent
    /// impl. Two traits may both declare a generic method with the same name,
    /// so the trait identity is part of the instance cache key and symbol.
    pub trait_impl: Option<TraitImplKey>,
}

/// One `trait` declaration: the method signatures an implementation must
/// match. The AST is kept because `self` can only be resolved against the
/// implementing type, which is known when an `impl ... for ...` is checked.
#[derive(Clone)]
pub(super) struct TraitInfo {
    pub params: Vec<ast::GenericParam>,
    pub methods: Vec<ast::TraitMethod>,
}

/// One function definition waiting for the body-lowering pass.
pub(super) struct PendingFunction {
    pub decl: ast::FunctionDecl,
    pub symbol: String,
    pub ty: TypeId,
    pub impl_context: Option<(TypeId, String)>,
    /// Concrete bindings of the generic parameters, when this pending function
    /// is one instantiation of a template.
    pub env: Option<GenericEnv>,
    /// Internal linkage for a reason other than `static`: every generic
    /// instance is private to its translation unit.
    pub internal: bool,
}

#[derive(Default)]
pub(super) struct TypeScope {
    pub typedefs: HashMap<String, TypeId>,
    pub tags: HashMap<String, TypeId>,
    /// `const` generic parameter bindings, as the integer value plus the
    /// `int` type C gives every one of them.
    pub consts: HashMap<String, (i64, TypeId)>,
}

/// One generic struct or union definition, kept as the AST template it is.
///
/// Instantiating it re-resolves the same member declarations under a pushed
/// type scope that binds the parameters, so no source is copied and the type
/// arena needs no generic kind.
#[derive(Clone)]
pub(super) struct GenericRecord {
    pub params: Vec<ast::GenericParam>,
    pub spec: ast::RecordSpec,
}

/// One generic enum definition, kept as the AST template it is.
///
/// Instantiating it re-resolves the same enumerator payload type names under a
/// pushed type scope that binds the parameters, so a payload enum instance is
/// an ordinary concrete payload enum.
#[derive(Clone)]
pub(super) struct GenericEnum {
    pub params: Vec<ast::GenericParam>,
    pub spec: ast::EnumSpec,
}

/// One generic function definition or prototype, kept as the AST template it
/// is.
///
/// Instantiating it binds the parameters in a pushed type scope and re-resolves
/// the same declaration, so no source is copied and the type arena needs no
/// generic kind. A prototype registers without a declaration because there is
/// no body to instantiate yet.
#[derive(Clone)]
pub(super) struct GenericFunction {
    pub params: Vec<ast::GenericParam>,
    pub decl: Option<ast::FunctionDecl>,
}

/// One generic `impl` block, kept as the AST template it is.
///
/// `impl<T> Box<T>`, `impl<T> Pair<T, int>`, and
/// `impl<T, U> Trait for Pair<U, T>` are materialized when a concrete receiver
/// needs one of their methods. Registration keeps the source target, and
/// instantiation unifies it against the receiver's own arguments, so a
/// reordered, concrete, or nested target binds the same parameter names the
/// method signatures use. Each method becomes an ordinary function with a
/// translation-unit internal symbol.
#[derive(Clone)]
pub(super) struct GenericImpl {
    pub params: Vec<ast::GenericParam>,
    pub block: ast::ImplBlock,
}

/// The concrete bindings of one materialized generic impl, carried by each of
/// its method definitions so the body still resolves the parameter names.
#[derive(Clone)]
pub(super) struct ImplInstance {
    pub params: Vec<ast::GenericParam>,
    pub args: Vec<GenericArgKey>,
}

/// The impl block a method definition belongs to.
#[derive(Clone)]
pub(super) struct ImplTarget {
    pub ty: TypeId,
    /// The record name written after `impl`, which is also the base of a plain
    /// method symbol.
    pub name: String,
    /// Concrete bindings when this target is one materialized generic impl.
    pub instance: Option<ImplInstance>,
    /// The trait implementation this method belongs to. `None` is an inherent
    /// `impl Type`, which keeps its `____Type__method` symbol.
    pub trait_impl: Option<TraitImplKey>,
}

/// Identity of one materialized generic impl: the concrete receiver plus the
/// trait it implements. An inherent template has no trait part. Two trait
/// templates for the same receiver are separate when their trait arguments
/// differ, which is why the resolved trait arguments are part of the key.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) struct GenericImplInstanceKey {
    pub receiver: TypeId,
    pub trait_impl: Option<TraitImplKey>,
}

/// Concrete bindings for one generic function instantiation: the type
/// arguments and the integer values of its `const` arguments.
#[derive(Clone)]
pub(super) struct GenericEnv {
    pub types: HashMap<String, TypeId>,
    pub consts: HashMap<String, (i64, TypeId)>,
}

/// The emitted symbol and concrete type of one function instantiation.
#[derive(Clone)]
pub(super) struct FunctionInstance {
    pub symbol: String,
    pub ty: TypeId,
}

/// One concrete generic argument. Types and `const` integers share one key
/// type so records and functions can keep a single cache shape.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum GenericArgKey {
    Type(TypeId),
    /// A `const` argument, always implicit `int`.
    Int(i64),
}

/// Identity of one instantiation: the generic's name plus its concrete
/// arguments. Records and functions keep separate caches, so one key type
/// serves both.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) struct InstanceKey {
    pub name: String,
    pub args: Vec<GenericArgKey>,
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
    /// Every generic record definition, registered before anything resolves.
    pub generic_records: HashMap<String, GenericRecord>,
    /// Instantiated generic records, keyed by tag and concrete arguments.
    pub record_instances: HashMap<InstanceKey, TypeId>,
    /// Every generic enum definition, registered before anything resolves.
    pub generic_enums: HashMap<String, GenericEnum>,
    /// Instantiated generic enums, keyed by tag and concrete arguments.
    pub enum_instances: HashMap<InstanceKey, TypeId>,
    /// Every generic function definition or prototype, registered before
    /// anything resolves.
    pub generic_functions: HashMap<String, GenericFunction>,
    /// Instantiated generic functions, keyed by name and concrete arguments.
    pub function_instances: HashMap<InstanceKey, FunctionInstance>,
    /// Every generic impl template, by the record name it targets. One record
    /// may have an inherent template plus one template per implemented trait.
    pub generic_impls: HashMap<String, Vec<GenericImpl>>,
    /// Materialized generic impls, keyed by concrete receiver and trait (with
    /// its resolved arguments), and mapped to the block that produced them so
    /// a repeated lookup skips its own work while a second template with the
    /// same identity is still diagnosed.
    pub generic_impl_instances: HashMap<GenericImplInstanceKey, Span>,
    /// Nesting level of the generic instantiation currently being resolved.
    pub instantiation_depth: u32,
    /// The source tag behind each record and enum type, so a C declaration can
    /// name a type the way the source does.
    pub type_names: HashMap<TypeId, String>,
    /// Every generic method the compiler materialized: which template it came
    /// from and what it bound, so the C export can print the body.
    pub method_instances: Vec<MethodInstancePlan>,
    /// The symbol every call expression resolved to, by span and the receiver
    /// the body belongs to. One trait default or template method body is
    /// lowered once per receiver, so the span alone does not identify a call.
    pub call_symbols: HashMap<(Span, Option<String>), String>,
    /// The symbol every function and method definition emits, by span.
    pub declaration_symbols: HashMap<Span, String>,
    /// Every trait declaration, by name. Registered before any `impl` is
    /// checked.
    pub traits: HashMap<String, TraitInfo>,
    /// `(trait, trait arguments, concrete receiver)` triples already
    /// implemented, to reject a second implementation of the same pair. The
    /// receiver is a resolved type, so `Box<int>` and `Box<long>` never collide
    /// just because both instances came from `Box`.
    pub trait_impls: HashSet<(String, Vec<GenericArgKey>, TypeId)>,
    /// Concrete `impl From<Source> for Target` conversions, used by `?` when
    /// the propagated error types are not identical.
    pub from_conversions: HashMap<(TypeId, TypeId), MethodInfo>,
    /// Synthetic record that represents each trait used as a value. It has one
    /// function-pointer field per trait method, named after that method.
    pub trait_values: HashMap<String, TypeId>,
    /// Placeholder record used as `self` when a trait signature is checked
    /// without an implementation. It is never emitted.
    trait_self_type: Option<TypeId>,
    pub impls: HashMap<TypeId, ImplInfo>,
    pub impl_context: Option<(TypeId, String)>,
    pub current_function_name: Option<String>,
    pub disable_nonexhaustive_switch: bool,
    pub disable_rustic_mut: bool,
    pub disable_default_static: bool,
    pub pending: Vec<PendingFunction>,
    string_counter: u32,
    static_counter: u32,
    /// Sanitised module name used to scope internal-linkage symbols.
    module_prefix: String,
}

impl Lowerer {
    fn new(options: &Options) -> Lowerer {
        let types =
            TypeStore::with_data_model(options.layout.pointer_bytes, options.layout.long_bytes);
        let mut module = ir::Module::new(&options.module_name);
        module.startup = match options.startup {
            StartupMode::Main => ir::Startup::Auto {
                entry: "main".to_string(),
                stack_pointer: 0x00ff_f000,
            },
            StartupMode::Manual => ir::Startup::Manual,
        };
        Lowerer {
            module_prefix: Self::sanitize_ident(&options.module_name),
            types,
            layout: options.layout,
            abi: options.abi,
            module,
            objects: Vec::new(),
            functions: Vec::new(),
            globals: HashMap::new(),
            type_scopes: vec![TypeScope::default()],
            enum_consts: HashMap::new(),
            generic_records: HashMap::new(),
            type_names: HashMap::new(),
            method_instances: Vec::new(),
            call_symbols: HashMap::new(),
            declaration_symbols: HashMap::new(),
            record_instances: HashMap::new(),
            generic_enums: HashMap::new(),
            enum_instances: HashMap::new(),
            generic_functions: HashMap::new(),
            function_instances: HashMap::new(),
            generic_impls: HashMap::new(),
            generic_impl_instances: HashMap::new(),
            instantiation_depth: 0,
            traits: HashMap::new(),
            trait_impls: HashSet::new(),
            from_conversions: HashMap::new(),
            trait_values: HashMap::new(),
            trait_self_type: None,
            impls: HashMap::new(),
            impl_context: None,
            current_function_name: None,
            disable_nonexhaustive_switch: options.disable_nonexhaustive_switch,
            disable_rustic_mut: options.disable_rustic_mut,
            disable_default_static: options.disable_default_static,
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
            TypeKind::Integer { .. } => match self.types.size_of(ty).unwrap_or(4) {
                1 => ir::Type::I8,
                2 => ir::Type::I16,
                4 => ir::Type::I32,
                _ => ir::Type::I64,
            },
            // An enum value has its underlying integer's width; a plain enum
            // keeps the `int` representation and the 32-bit IR type.
            TypeKind::Enum(_) => {
                let underlying = self
                    .types
                    .enum_underlying(ty)
                    .unwrap_or_else(|| self.types.builtin(crate::sema::types::Builtin::Int));
                self.ir_type(underlying)
            }
            // The `Result<void, E>` niche is `E`'s integer, so it is one too.
            TypeKind::PayloadEnum(_) if self.types.is_niche_enum(ty) => {
                let underlying = self
                    .types
                    .enum_underlying(ty)
                    .unwrap_or_else(|| self.types.builtin(crate::sema::types::Builtin::Int));
                self.ir_type(underlying)
            }
            TypeKind::Float32 => ir::Type::F32,
            TypeKind::Float64 => ir::Type::F64,
            TypeKind::Pointer { .. } | TypeKind::Function { .. } => ir::Type::Ptr,
            // A payload enum is an aggregate object in the IR; its tag and
            // payload offsets stay in the source arena, the same way a record
            // keeps its field layout there.
            TypeKind::Array { .. } | TypeKind::Record { .. } | TypeKind::PayloadEnum(_) => {
                ir::Type::aggregate(
                    self.types.size_of(ty).unwrap_or(0),
                    self.types.align_of(ty).unwrap_or(1),
                )
            }
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

    /// The method `name` of a record, materializing its generic impl first when
    /// the record is an instance that has not needed one yet.
    ///
    /// An inherent method wins over a trait method. When only trait methods
    /// match, they must be unique: two traits may implement the same method
    /// name for one type, and then a receiver call has no unambiguous target.
    /// The trait value table still reaches each implementation through its own
    /// trait identity.
    pub(super) fn method_of(
        &mut self,
        record: TypeId,
        name: &ast::Name,
    ) -> Result<Option<MethodInfo>, Diagnostic> {
        self.instantiate_generic_impl(record)?;
        if let Some(method) = self
            .impls
            .get(&record)
            .and_then(|info| info.methods.get(&name.text))
            .cloned()
        {
            return Ok(Some(
                self.queue_deferred_method(record, None, &name.text, method),
            ));
        }
        let mut matches = self.trait_methods_named(record, &name.text);
        match matches.len() {
            0 => Ok(None),
            1 => {
                let key = matches.remove(0);
                let method = self
                    .impls
                    .get(&record)
                    .and_then(|info| info.trait_methods.get(&key))
                    .and_then(|set| set.methods.get(&name.text))
                    .cloned()
                    .expect("the method was just found");
                Ok(Some(self.queue_deferred_method(
                    record,
                    Some(&key),
                    &name.text,
                    method,
                )))
            }
            _ => {
                let mut traits: Vec<String> = matches.into_iter().map(|key| key.name).collect();
                traits.sort();
                traits.dedup();
                Err(Diagnostic::new(
                    name.span,
                    format!(
                        "method '{}' is ambiguous: it is implemented by traits {}; \
                         an inherent method or a trait value is needed",
                        name.text,
                        traits
                            .iter()
                            .map(|trait_name| format!("'{trait_name}'"))
                            .collect::<Vec<_>>()
                            .join(" and ")
                    ),
                ))
            }
        }
    }

    /// The `drop` method for a type, if an `impl` declared one.
    pub(super) fn drop_method(&mut self, ty: TypeId) -> Result<Option<MethodInfo>, Diagnostic> {
        if !self.types.is_record(ty) {
            return Ok(None);
        }
        self.instantiate_generic_impl(ty)?;
        if let Some(method) = self
            .impls
            .get(&ty)
            .and_then(|info| info.methods.get("drop"))
            .cloned()
        {
            return Ok(Some(self.queue_deferred_method(ty, None, "drop", method)));
        }
        // Several traits may own a `drop` method; without an inherent one the
        // receiver call would be ambiguous, so no implicit drop is installed.
        let matches = self.trait_methods_named(ty, "drop");
        let [key] = matches.as_slice() else {
            return Ok(None);
        };
        let key = key.clone();
        let Some(method) = self
            .impls
            .get(&ty)
            .and_then(|info| info.trait_methods.get(&key))
            .and_then(|set| set.methods.get("drop"))
            .cloned()
        else {
            return Ok(None);
        };
        Ok(Some(self.queue_deferred_method(
            ty,
            Some(&key),
            "drop",
            method,
        )))
    }

    /// Queue a materialized generic method's body on its first lookup.
    fn queue_deferred_method(
        &mut self,
        record: TypeId,
        trait_impl: Option<&TraitImplKey>,
        name: &str,
        method: MethodInfo,
    ) -> MethodInfo {
        let Some(deferred) = &method.deferred else {
            return method;
        };
        self.pending.push(PendingFunction {
            decl: deferred.decl.clone(),
            symbol: method.symbol.clone(),
            ty: method.ty,
            impl_context: Some(deferred.impl_context.clone()),
            env: Some(deferred.env.clone()),
            internal: true,
        });
        let Some(info) = self.impls.get_mut(&record) else {
            return method;
        };
        let stored = match trait_impl {
            Some(trait_impl) => info
                .trait_methods
                .get_mut(trait_impl)
                .and_then(|set| set.methods.get_mut(name)),
            None => info.methods.get_mut(name),
        };
        if let Some(stored) = stored {
            stored.deferred = None;
        }
        method
    }

    /// The ordinary method `name` of one concrete trait implementation,
    /// materializing a generic impl first and queueing a deferred body.
    pub(super) fn trait_method_of(
        &mut self,
        record: TypeId,
        trait_impl: &TraitImplKey,
        name: &ast::Name,
    ) -> Result<Option<MethodInfo>, Diagnostic> {
        self.instantiate_generic_impl(record)?;
        Ok(self.trait_method_named_now(record, trait_impl, &name.text))
    }

    /// The ordinary method `name` of one concrete trait implementation,
    /// materializing a generic impl first and queueing a deferred body.
    pub(super) fn trait_method_named(
        &mut self,
        record: TypeId,
        trait_impl: &TraitImplKey,
        name: &str,
    ) -> Result<Option<MethodInfo>, Diagnostic> {
        self.instantiate_generic_impl(record)?;
        Ok(self.trait_method_named_now(record, trait_impl, name))
    }

    fn trait_method_named_now(
        &mut self,
        record: TypeId,
        trait_impl: &TraitImplKey,
        name: &str,
    ) -> Option<MethodInfo> {
        let method = self
            .impls
            .get(&record)
            .and_then(|info| info.trait_methods.get(trait_impl))
            .and_then(|set| set.methods.get(name))
            .cloned()?;
        Some(self.queue_deferred_method(record, Some(trait_impl), name, method))
    }

    /// The trait implementations of `record` that declare an ordinary method
    /// called `name`.
    fn trait_methods_named(&self, record: TypeId, name: &str) -> Vec<TraitImplKey> {
        self.impls
            .get(&record)
            .into_iter()
            .flat_map(|info| info.trait_methods.iter())
            .filter(|(_, set)| set.methods.contains_key(name))
            .map(|(key, _)| key.clone())
            .collect()
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

    /// The emitted symbol of one method of `impl Trait<Args> for Type`.
    ///
    /// The trait name and the resolved trait arguments are part of the symbol,
    /// so two traits that declare the same method, and two parameterizations
    /// of one trait, never collide. An inherent `impl Type` keeps the old
    /// `____Type__method` spelling.
    pub(super) fn trait_impl_method_symbol(
        &self,
        trait_impl: &TraitImplKey,
        receiver: &str,
        method: &str,
    ) -> String {
        let mut symbol = format!(
            "____{}__{}__{}",
            Self::sanitize_ident(&trait_impl.name),
            Self::sanitize_ident(receiver),
            Self::sanitize_ident(method)
        );
        push_symbol_args(&mut symbol, &trait_impl.args);
        symbol
    }

    /// The internal symbol of one method of a materialized `impl<T> Trait for
    /// Type<T>`.
    ///
    /// The concrete receiver is part of the generic impl cache already; the
    /// trait identity and its resolved arguments keep several traits and
    /// parameterizations of one receiver apart, while an inherent generic impl
    /// keeps `generic.impl.Type.method`.
    pub(super) fn generic_trait_impl_method_symbol(
        &self,
        trait_impl: &TraitImplKey,
        receiver: &str,
        method: &str,
        record: TypeId,
    ) -> String {
        let mut mangled = format!(
            "generic.trait.{}.{}.{}.{}",
            Self::sanitize_ident(&trait_impl.name),
            Self::sanitize_ident(receiver),
            Self::sanitize_ident(method),
            record.index()
        );
        push_symbol_args(&mut mangled, &trait_impl.args);
        self.internal_symbol(&mangled)
    }

    /// The emitted name of a file-scope entity with internal linkage.
    ///
    /// Both backends emit one section (Shy) or one symbol (LLVM) per global and
    /// per function, and the Shy linker requires section names to be unique
    /// across a whole link. A `static` object or function is therefore scoped by
    /// its translation unit, which is exactly what internal linkage means; the
    /// scope key is the module name the driver derives from the input file.
    pub(super) fn internal_symbol(&self, name: &str) -> String {
        format!(".L.{}.{}", self.module_prefix, name)
    }

    /// Unique symbol for an anonymous object such as a string literal.
    pub(super) fn fresh_symbol(&mut self, prefix: &str) -> String {
        let index = self.string_counter;
        self.string_counter += 1;
        format!(".L.{}.{prefix}.{index}", self.module_prefix)
    }

    pub(super) fn fresh_static_symbol(&mut self, function: &str, name: &str) -> String {
        let index = self.static_counter;
        self.static_counter += 1;
        format!(
            ".L.{}.static.{}.{}.{}",
            self.module_prefix,
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

    /// The innermost `const` generic binding for a name, if any.
    pub(super) fn lookup_const(&self, name: &str) -> Option<(i64, TypeId)> {
        self.type_scopes
            .iter()
            .rev()
            .find_map(|scope| scope.consts.get(name).copied())
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

/// Append one `__t<index>` or `__n<hex>` part per resolved trait argument.
///
/// A type is spelled by its arena identity and a `const` value by its
/// fixed-width bits, so the suffix is unique and cannot be confused with the
/// trait, receiver, or method parts before it.
pub(super) fn push_symbol_args(symbol: &mut String, args: &[GenericArgKey]) {
    for arg in args {
        symbol.push_str("__");
        match arg {
            GenericArgKey::Type(ty) => symbol.push_str(&format!("t{}", ty.index())),
            GenericArgKey::Int(value) => symbol.push_str(&format!("n{:016x}", *value as u64)),
        }
    }
}

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
    pub name: String,
    pub slot: ir::SlotId,
    pub ty: TypeId,
    pub qualifiers: Qualifiers,
    /// `let` without `mut`: assigning to this object is rejected. This is a
    /// checking flag, not a C `const` qualifier, so `&local` keeps the plain
    /// pointer type and aliases keep ordinary C semantics.
    pub readonly: bool,
    pub drop_symbol: Option<String>,
    /// The `drop` method's function type, kept with the symbol so emitting the
    /// call does not look the method up again.
    pub drop_ty: Option<TypeId>,
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
    /// The scrutinee's type before integer promotion. Exhaustiveness needs the
    /// original type to tell a closed payload enum from an open C enum.
    pub scrutinee: TypeId,
    pub terminator_block: ir::BlockId,
    pub default: Option<ir::BlockId>,
    pub cases: Vec<ir::SwitchCase>,
    /// `case low ... high` labels, dispatched by a comparison chain because a
    /// switch instruction compares one value.
    pub ranges: Vec<(ir::Constant, ir::Constant, ir::BlockId)>,
    /// Every label's numeric span, so overlapping labels are reported.
    pub labels: Vec<(i64, i64)>,
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
        let env = pending.env;
        let mut f = ir::Function::new(&symbol, signature, l.layout, abi);
        f.internal = pending.internal
            || matches!(pending.decl.specs.storage, Some(ast::StorageClass::Static));
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
        // An instantiated generic body still names its parameters (`T x;`), so
        // the concrete bindings join the function's own type scope. A direct
        // insert, unlike `declare_typedef`, lets a parameter shadow an outer
        // name.
        if let Some(env) = env {
            let scope = lowering.l.type_scopes.last_mut().expect("type scope stack");
            for (name, ty) in env.types {
                scope.typedefs.insert(name, ty);
            }
            for (name, binding) in env.consts {
                scope.consts.insert(name, binding);
            }
        }
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
