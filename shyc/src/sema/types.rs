//! Semantic types for the ShyC target, stored in a plain arena.
//!
//! The arena owns every type by [`TypeId`]. Types are immutable from the
//! outside: a record can start incomplete and be completed later, but its
//! identity never changes. This is the type layer used by declaration binding
//! and the expression checker; it knows nothing about AST nodes, symbols, or
//! constant evaluation.
//!
//! Target model, matching the existing ShyC frontend:
//!
//! | type | size | align |
//! | --- | ---: | ---: |
//! | `char`, `_Bool` | 1 | 1 |
//! | `short` | 2 | 2 |
//! | `int`, pointer | 4 | 4 |
//! | `long`, `long long` | 8 | 8 |
//! | `float` | 4 | 4 |
//! | `double` | 8 | 8 |
//!
//! `char` is signed. `long` and `long long` have the same width but different
//! ranks, so they remain distinct integer types. Record layout, `sizeof`,
//! `_Alignof`, type compatibility, integer promotions, and the usual arithmetic
//! conversions live in the sibling [`super::layout`] module; they are inherent
//! methods on [`TypeStore`].
//!
//! A record's natural alignment is *not* the 4-byte alignment the ABI uses for
//! stack argument transport. `long` and `double` members are 8-byte aligned in
//! the object layout even though argument blocks are only 4-byte aligned.

pub use super::layout::{Layout, RecordField, RecordInfo, TypeError};

use super::layout::align_up;

// ---------------------------------------------------------------------------
// Identities
// ---------------------------------------------------------------------------

/// Index of one type in a [`TypeStore`].
///
/// IDs belong to the store that created them; never mix IDs from two stores.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TypeId(u32);

impl TypeId {
    pub fn index(self) -> usize {
        self.0 as usize
    }
}

/// Identity of one struct or union, independent of its completion state.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct RecordId(u32);

impl RecordId {
    pub fn index(self) -> usize {
        self.0 as usize
    }
}

/// Identity of one enum type.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct EnumId(u32);

impl EnumId {
    pub fn index(self) -> usize {
        self.0 as usize
    }
}

// ---------------------------------------------------------------------------
// Qualifiers
// ---------------------------------------------------------------------------

/// The four C type qualifiers. They belong to a [`QualifiedType`], not to a
/// [`TypeKind`], so a pointer's target can be qualified independently.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Qualifiers {
    pub is_const: bool,
    pub is_volatile: bool,
    pub is_restrict: bool,
    pub is_atomic: bool,
}

impl Qualifiers {
    pub const NONE: Qualifiers = Qualifiers {
        is_const: false,
        is_volatile: false,
        is_restrict: false,
        is_atomic: false,
    };

    pub const CONST: Qualifiers = Qualifiers {
        is_const: true,
        is_volatile: false,
        is_restrict: false,
        is_atomic: false,
    };

    pub fn is_empty(self) -> bool {
        self == Qualifiers::NONE
    }
}

/// A type together with its top-level qualifiers.
///
/// Nested types use this type too: a pointer stores its target as a
/// `QualifiedType`, so `const int *` is a pointer whose target carries
/// `Qualifiers::CONST`. The same type may appear unqualified elsewhere.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QualifiedType {
    pub ty: TypeId,
    pub qualifiers: Qualifiers,
}

impl QualifiedType {
    pub fn new(ty: TypeId, qualifiers: Qualifiers) -> Self {
        Self { ty, qualifiers }
    }

    pub fn unqualified(ty: TypeId) -> Self {
        Self {
            ty,
            qualifiers: Qualifiers::NONE,
        }
    }

    pub fn is_qualified(self) -> bool {
        !self.qualifiers.is_empty()
    }
}

// ---------------------------------------------------------------------------
// Built-in types
// ---------------------------------------------------------------------------

/// Integer rank. Widths are fixed by the target; `Long` and `LongLong` share
/// the width 8 but keep distinct ranks for the usual arithmetic conversions.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum IntegerRank {
    Char,
    Short,
    Int,
    Long,
    LongLong,
}

impl IntegerRank {
    pub fn size(self) -> u32 {
        match self {
            IntegerRank::Char => 1,
            IntegerRank::Short => 2,
            IntegerRank::Int => 4,
            IntegerRank::Long | IntegerRank::LongLong => 8,
        }
    }

    pub fn align(self) -> u32 {
        self.size()
    }
}

/// A built-in type spelling. `SignedChar` is the same type as `Char` because
/// plain `char` is signed on this target; both names select that one type.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Builtin {
    Void,
    Bool,
    Char,
    SignedChar,
    UnsignedChar,
    Short,
    UnsignedShort,
    Int,
    UnsignedInt,
    Long,
    UnsignedLong,
    LongLong,
    UnsignedLongLong,
    Float,
    Double,
}

/// What a type is. Qualifiers are intentionally absent; they live on
/// [`QualifiedType`] at each use site.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TypeKind {
    Void,
    Bool,
    Integer {
        rank: IntegerRank,
        unsigned: bool,
    },
    Float32,
    Float64,
    Pointer {
        pointee: QualifiedType,
    },
    /// `len == None` is an incomplete array (`int[]`). `Some(0)` is a GNU
    /// zero-length array and is distinct from an incomplete array.
    Array {
        element: QualifiedType,
        len: Option<u32>,
    },
    Function {
        /// Top-level qualifiers on a return value are irrelevant to the
        /// function type, so only the underlying type is kept.
        result: TypeId,
        /// Parameters are stored after array/function adjustment and with
        /// top-level qualifiers removed, as C requires for function types.
        params: Vec<TypeId>,
        variadic: bool,
        /// `true` for `(int)` or `(void)`; `false` for the old-style `()`.
        prototype: bool,
    },
    Record {
        id: RecordId,
        kind: RecordKind,
    },
    /// A C-compatible enum: an object whose representation is exactly its
    /// underlying integer type.
    Enum(EnumId),
    /// An enum with at least one payload variant. It is an aggregate object
    /// (`tag` plus a union of payloads), never an integer.
    PayloadEnum(EnumId),
}

/// A type and its layout. `size` is `None` while the type is incomplete.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Type {
    pub kind: TypeKind,
    pub size: Option<u32>,
    pub align: Option<u32>,
}

// ---------------------------------------------------------------------------
// Records and enums
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecordKind {
    Struct,
    Union,
}

/// An enum type and its integer representation. A plain `enum` uses `int`,
/// while an explicit `enum(type)` stores the chosen type. Enum identity is
/// still distinct from the underlying type.
///
/// A plain payload-free enum is an integer and its `variants` only record the
/// enumerator values. A payload enum stores its tag in the first
/// `size_of(underlying)` bytes and every payload at `payload_offset`,
/// overlapping in one union of size `payload_size`. That is the transparent
/// `tag + max-payload` layout: no hidden discriminant, no pointer indirection.
///
/// `niche` marks the one exception: an instantiated `Result<void, E>` whose
/// `Ok` is payload-free and whose `Err(E)` holds a payload-free enum with no
/// zero enumerator. It is represented as `E`'s integer, `0` meaning `Ok`, so
/// `underlying` is `E`'s underlying type rather than the enum's own. The tag
/// and payload offsets describe the same byte, at offset 0.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EnumInfo {
    pub underlying: TypeId,
    pub variants: Vec<EnumVariant>,
    /// Byte offset of the payload union, and the union's size and alignment.
    /// A plain enum leaves these at `0, 0, 1`.
    pub payload_offset: u32,
    pub payload_size: u32,
    pub payload_align: u32,
    /// Whether this payload enum uses the `Result<void, E>` zero-value niche.
    pub niche: bool,
}

/// One enum variant.
///
/// `value` is the enumerator's integer value; for a payload enum it is the tag
/// stored at offset 0. `payload` is the type stored at
/// [`EnumInfo::payload_offset`] when the variant is active, and is `None` for a
/// payload-free variant.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EnumVariant {
    pub name: String,
    pub value: i64,
    pub payload: Option<TypeId>,
}

// ---------------------------------------------------------------------------
// Type arena
// ---------------------------------------------------------------------------

/// Arena of target types with cached built-ins.
///
/// All constructors return stable [`TypeId`]s. There is no interning: asking
/// for `int *` twice creates two structurally equal pointer types, and
/// [`TypeStore::is_compatible`] is the operation that decides type equality.
/// Records and enums are the exception: their identity is their ID.
pub struct TypeStore {
    pub(super) types: Vec<Type>,
    pub(super) records: Vec<RecordInfo>,
    pub(super) enums: Vec<EnumInfo>,
    builtins: Builtins,
    /// Pointer width of the target. ShyISA uses 32-bit addresses; other
    /// backends use the platform's own width, so this is never hard-coded
    /// into a type.
    pointer_bytes: u32,
    pointer_align: u32,
}

#[derive(Clone, Copy)]
struct Builtins {
    void: TypeId,
    bool_: TypeId,
    char_: TypeId,
    unsigned_char: TypeId,
    short: TypeId,
    unsigned_short: TypeId,
    int: TypeId,
    unsigned_int: TypeId,
    long: TypeId,
    unsigned_long: TypeId,
    long_long: TypeId,
    unsigned_long_long: TypeId,
    float: TypeId,
    double_: TypeId,
}

impl Default for TypeStore {
    fn default() -> Self {
        Self::new()
    }
}

impl TypeStore {
    /// Creates a type arena for ShyISA, where pointers are 32-bit.
    pub fn new() -> Self {
        Self::with_pointer_width(4)
    }

    /// Creates a type arena whose pointers are `bytes` wide and aligned.
    ///
    /// A backend targeting an LP64 platform passes 8, which changes
    /// `sizeof(void *)`, pointer arithmetic, and every record that contains a
    /// pointer.
    pub fn with_pointer_width(bytes: u32) -> Self {
        let mut types = Vec::new();

        let void = push_type(&mut types, TypeKind::Void, None, None);
        let bool_ = push_type(&mut types, TypeKind::Bool, Some(1), Some(1));
        let char_ = push_integer(&mut types, IntegerRank::Char, false);
        let unsigned_char = push_integer(&mut types, IntegerRank::Char, true);
        let short = push_integer(&mut types, IntegerRank::Short, false);
        let unsigned_short = push_integer(&mut types, IntegerRank::Short, true);
        let int = push_integer(&mut types, IntegerRank::Int, false);
        let unsigned_int = push_integer(&mut types, IntegerRank::Int, true);
        let long = push_integer(&mut types, IntegerRank::Long, false);
        let unsigned_long = push_integer(&mut types, IntegerRank::Long, true);
        let long_long = push_integer(&mut types, IntegerRank::LongLong, false);
        let unsigned_long_long = push_integer(&mut types, IntegerRank::LongLong, true);
        let float = push_type(&mut types, TypeKind::Float32, Some(4), Some(4));
        let double_ = push_type(&mut types, TypeKind::Float64, Some(8), Some(8));

        Self {
            types,
            records: Vec::new(),
            enums: Vec::new(),
            builtins: Builtins {
                void,
                bool_,
                char_,
                unsigned_char,
                short,
                unsigned_short,
                int,
                unsigned_int,
                long,
                unsigned_long,
                long_long,
                unsigned_long_long,
                float,
                double_,
            },
            pointer_bytes: bytes,
            pointer_align: bytes,
        }
    }

    /// Pointer width of this target, in bytes.
    pub fn pointer_width(&self) -> u32 {
        self.pointer_bytes
    }

    /// Returns the canonical TypeId for a built-in spelling.
    pub fn builtin(&self, builtin: Builtin) -> TypeId {
        match builtin {
            Builtin::Void => self.builtins.void,
            Builtin::Bool => self.builtins.bool_,
            Builtin::Char | Builtin::SignedChar => self.builtins.char_,
            Builtin::UnsignedChar => self.builtins.unsigned_char,
            Builtin::Short => self.builtins.short,
            Builtin::UnsignedShort => self.builtins.unsigned_short,
            Builtin::Int => self.builtins.int,
            Builtin::UnsignedInt => self.builtins.unsigned_int,
            Builtin::Long => self.builtins.long,
            Builtin::UnsignedLong => self.builtins.unsigned_long,
            Builtin::LongLong => self.builtins.long_long,
            Builtin::UnsignedLongLong => self.builtins.unsigned_long_long,
            Builtin::Float => self.builtins.float,
            Builtin::Double => self.builtins.double_,
        }
    }

    /// The canonical integer type for a rank and signedness.
    pub(super) fn integer_type(&self, rank: IntegerRank, unsigned: bool) -> TypeId {
        let builtin = match (rank, unsigned) {
            (IntegerRank::Char, false) => Builtin::Char,
            (IntegerRank::Char, true) => Builtin::UnsignedChar,
            (IntegerRank::Short, false) => Builtin::Short,
            (IntegerRank::Short, true) => Builtin::UnsignedShort,
            (IntegerRank::Int, false) => Builtin::Int,
            (IntegerRank::Int, true) => Builtin::UnsignedInt,
            (IntegerRank::Long, false) => Builtin::Long,
            (IntegerRank::Long, true) => Builtin::UnsignedLong,
            (IntegerRank::LongLong, false) => Builtin::LongLong,
            (IntegerRank::LongLong, true) => Builtin::UnsignedLongLong,
        };
        self.builtin(builtin)
    }

    pub fn get(&self, ty: TypeId) -> &Type {
        &self.types[ty.index()]
    }

    pub fn kind(&self, ty: TypeId) -> &TypeKind {
        &self.get(ty).kind
    }

    /// Creates a `pointee`-pointing pointer type using this target's width.
    pub fn pointer_to(&mut self, pointee: QualifiedType) -> TypeId {
        push_type(
            &mut self.types,
            TypeKind::Pointer { pointee },
            Some(self.pointer_bytes),
            Some(self.pointer_align),
        )
    }

    /// Creates an array type.
    ///
    /// `len == None` creates an incomplete array; it still has the element's
    /// alignment. The element must be a complete object type. A known length
    /// whose byte size overflows 32 bits is rejected.
    pub fn array_of(
        &mut self,
        element: QualifiedType,
        len: Option<u32>,
    ) -> Result<TypeId, TypeError> {
        let element_size = self.size_of(element.ty)?;
        let element_align = self.align_of(element.ty)?;
        let size = match len {
            Some(len) => Some(
                element_size
                    .checked_mul(len)
                    .ok_or(TypeError::ArrayTooLarge)?,
            ),
            None => None,
        };
        Ok(push_type(
            &mut self.types,
            TypeKind::Array { element, len },
            size,
            Some(element_align),
        ))
    }

    /// Creates a function type. Parameter adjustment and top-level qualifier
    /// removal must already have been done by the caller.
    pub fn function(
        &mut self,
        result: TypeId,
        params: Vec<TypeId>,
        variadic: bool,
        prototype: bool,
    ) -> TypeId {
        push_type(
            &mut self.types,
            TypeKind::Function {
                result,
                params,
                variadic,
                prototype,
            },
            None,
            None,
        )
    }

    /// Creates a new incomplete struct or union with a fresh identity.
    pub fn record_forward(&mut self, kind: RecordKind) -> TypeId {
        let id = RecordId(self.records.len() as u32);
        self.records.push(RecordInfo {
            fields: Vec::new(),
            is_flexible: false,
        });
        push_type(&mut self.types, TypeKind::Record { id, kind }, None, None)
    }

    /// Creates a fresh enum type whose object size and alignment follow
    /// `underlying`. Callers must supply a complete integer type.
    pub fn new_enum(&mut self, underlying: TypeId) -> Result<TypeId, TypeError> {
        let size = self.size_of(underlying)?;
        let align = self.align_of(underlying)?;
        let id = EnumId(self.enums.len() as u32);
        self.enums.push(EnumInfo {
            underlying,
            variants: Vec::new(),
            payload_offset: 0,
            payload_size: 0,
            payload_align: 1,
            niche: false,
        });
        Ok(push_type(
            &mut self.types,
            TypeKind::Enum(id),
            Some(size),
            Some(align),
        ))
    }

    /// Creates a fresh payload enum. Its layout is unknown until
    /// [`TypeStore::complete_payload_enum`] supplies the variants, so the type
    /// is incomplete and cannot be a payload or a member before then.
    pub fn new_payload_enum(&mut self, underlying: TypeId) -> Result<TypeId, TypeError> {
        self.size_of(underlying)?;
        self.align_of(underlying)?;
        let id = EnumId(self.enums.len() as u32);
        self.enums.push(EnumInfo {
            underlying,
            variants: Vec::new(),
            payload_offset: 0,
            payload_size: 0,
            payload_align: 1,
            niche: false,
        });
        Ok(push_type(
            &mut self.types,
            TypeKind::PayloadEnum(id),
            None,
            None,
        ))
    }

    /// Defines the variants of a payload enum and computes the transparent
    /// `tag + max-payload` layout.
    ///
    /// The tag occupies the underlying type's bytes at offset 0. Every payload
    /// starts at the same offset, aligned for the widest one, and the object is
    /// rounded up to the larger of the tag and payload alignments. A
    /// payload-free variant contributes no bytes to the union.
    ///
    /// With `allow_niche`, an instantiated `Result<void, E>` whose `Ok` is
    /// payload-free and whose error is a zero-free payload-free enum is laid
    /// out as `E`'s integer instead: `0` is `Ok` and any other value is the
    /// error. Hand-written enums never take this layout, so the C header and
    /// the ordinary tagged representation keep their existing shape.
    pub fn complete_payload_enum(
        &mut self,
        ty: TypeId,
        variants: Vec<EnumVariant>,
        allow_niche: bool,
    ) -> Result<Layout, TypeError> {
        let TypeKind::PayloadEnum(id) = *self.kind(ty) else {
            return Err(TypeError::NotAnEnum);
        };
        if self.get(ty).size.is_some() {
            return Err(TypeError::EnumAlreadyComplete);
        }
        let underlying = self.enums[id.index()].underlying;

        if allow_niche && let Some(error) = self.niche_result_error(&variants) {
            let size = self.size_of(error)?;
            let align = self.align_of(error)?;
            let underlying = self.enum_underlying(error).ok_or(TypeError::NotAnEnum)?;
            self.enums[id.index()] = EnumInfo {
                underlying,
                variants,
                payload_offset: 0,
                payload_size: size,
                payload_align: align,
                niche: true,
            };
            let enum_type = &mut self.types[ty.index()];
            enum_type.size = Some(size);
            enum_type.align = Some(align);
            return Ok(Layout { size, align });
        }

        let tag_size = self.size_of(underlying)?;
        let tag_align = self.align_of(underlying)?;

        let mut payload_size = 0u32;
        let mut payload_align = 1u32;
        for variant in &variants {
            let Some(payload) = variant.payload else {
                continue;
            };
            payload_size = payload_size.max(self.size_of(payload)?);
            payload_align = payload_align.max(self.align_of(payload)?);
        }
        let payload_offset = align_up(tag_size, payload_align).ok_or(TypeError::LayoutOverflow)?;
        let align = tag_align.max(payload_align);
        let size =
            align_up(payload_offset + payload_size, align).ok_or(TypeError::LayoutOverflow)?;

        self.enums[id.index()] = EnumInfo {
            underlying,
            variants,
            payload_offset,
            payload_size,
            payload_align,
            niche: false,
        };
        let enum_type = &mut self.types[ty.index()];
        enum_type.size = Some(size);
        enum_type.align = Some(align);
        Ok(Layout { size, align })
    }

    /// The error enum of a `Result<void, E>` that can use the zero-value
    /// niche: `Ok` is payload-free with tag 0, `Err` carries a plain enum `E`,
    /// and no enumerator of `E` is 0.
    fn niche_result_error(&self, variants: &[EnumVariant]) -> Option<TypeId> {
        if variants.len() != 2 {
            return None;
        }
        let ok = variants.iter().find(|variant| variant.name == "Ok")?;
        if ok.payload.is_some() || ok.value != 0 {
            return None;
        }
        let error = variants.iter().find(|variant| variant.name == "Err")?;
        let error_ty = error.payload?;
        let TypeKind::Enum(id) = *self.kind(error_ty) else {
            return None;
        };
        if self.enums[id.index()]
            .variants
            .iter()
            .any(|variant| variant.value == 0)
        {
            return None;
        }
        Some(error_ty)
    }

    /// Records the enumerators of a plain enum. The representation stays the
    /// underlying integer; the variants are kept so the `Result<void, E>`
    /// niche can reject an error enum that contains 0.
    pub fn set_enum_variants(
        &mut self,
        ty: TypeId,
        variants: Vec<EnumVariant>,
    ) -> Result<(), TypeError> {
        let TypeKind::Enum(id) = *self.kind(ty) else {
            return Err(TypeError::NotAnEnum);
        };
        self.enums[id.index()].variants = variants;
        Ok(())
    }

    pub fn record_info(&self, ty: TypeId) -> Option<&RecordInfo> {
        match self.kind(ty) {
            TypeKind::Record { id, .. } => self.records.get(id.index()),
            _ => None,
        }
    }

    pub fn enum_info(&self, ty: TypeId) -> Option<&EnumInfo> {
        match self.kind(ty) {
            TypeKind::Enum(id) | TypeKind::PayloadEnum(id) => self.enums.get(id.index()),
            _ => None,
        }
    }

    pub fn enum_underlying(&self, ty: TypeId) -> Option<TypeId> {
        self.enum_info(ty).map(|info| info.underlying)
    }

    /// The payload enum variant called `name`, if `ty` is a payload enum.
    pub fn enum_variant(&self, ty: TypeId, name: &str) -> Option<&EnumVariant> {
        match self.kind(ty) {
            TypeKind::PayloadEnum(id) => self.enums[id.index()]
                .variants
                .iter()
                .find(|variant| variant.name == name),
            _ => None,
        }
    }

    // -----------------------------------------------------------------------
    // Queries
    // -----------------------------------------------------------------------

    pub fn is_void(&self, ty: TypeId) -> bool {
        matches!(self.kind(ty), TypeKind::Void)
    }

    pub fn is_bool(&self, ty: TypeId) -> bool {
        matches!(self.kind(ty), TypeKind::Bool)
    }

    pub fn is_integer(&self, ty: TypeId) -> bool {
        match self.kind(ty) {
            TypeKind::Bool | TypeKind::Integer { .. } | TypeKind::Enum(_) => true,
            // A `Result<void, E>` niche is the error enum's integer.
            TypeKind::PayloadEnum(id) => self.enums[id.index()].niche,
            _ => false,
        }
    }

    pub fn is_floating(&self, ty: TypeId) -> bool {
        matches!(self.kind(ty), TypeKind::Float32 | TypeKind::Float64)
    }

    pub fn is_arithmetic(&self, ty: TypeId) -> bool {
        self.is_integer(ty) || self.is_floating(ty)
    }

    pub fn is_pointer(&self, ty: TypeId) -> bool {
        matches!(self.kind(ty), TypeKind::Pointer { .. })
    }

    pub fn is_array(&self, ty: TypeId) -> bool {
        matches!(self.kind(ty), TypeKind::Array { .. })
    }

    pub fn is_function(&self, ty: TypeId) -> bool {
        matches!(self.kind(ty), TypeKind::Function { .. })
    }

    pub fn is_record(&self, ty: TypeId) -> bool {
        matches!(self.kind(ty), TypeKind::Record { .. })
    }

    pub fn is_payload_enum(&self, ty: TypeId) -> bool {
        matches!(self.kind(ty), TypeKind::PayloadEnum(_))
    }

    /// Whether this payload enum is the integer `Result<void, E>` niche.
    pub fn is_niche_enum(&self, ty: TypeId) -> bool {
        match self.kind(ty) {
            TypeKind::PayloadEnum(id) => self.enums[id.index()].niche,
            _ => false,
        }
    }

    pub fn is_scalar(&self, ty: TypeId) -> bool {
        self.is_arithmetic(ty) || self.is_pointer(ty)
    }

    pub fn is_aggregate(&self, ty: TypeId) -> bool {
        self.is_array(ty)
            || self.is_record(ty)
            || (self.is_payload_enum(ty) && !self.is_niche_enum(ty))
    }

    /// A type is complete when its size is known. `void`, function types, and
    /// incomplete arrays/records are not complete.
    pub fn is_complete(&self, ty: TypeId) -> bool {
        self.get(ty).size.is_some()
    }
}

fn push_type(
    types: &mut Vec<Type>,
    kind: TypeKind,
    size: Option<u32>,
    align: Option<u32>,
) -> TypeId {
    let id = TypeId(types.len() as u32);
    types.push(Type { kind, size, align });
    id
}

fn push_integer(types: &mut Vec<Type>, rank: IntegerRank, unsigned: bool) -> TypeId {
    push_type(
        types,
        TypeKind::Integer { rank, unsigned },
        Some(rank.size()),
        Some(rank.align()),
    )
}
