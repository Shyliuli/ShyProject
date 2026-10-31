//! Object layout, size/alignment queries, type compatibility, and arithmetic
//! conversions.
//!
//! This module adds inherent methods to [`TypeStore`]. It is a sibling of
//! `types.rs`; the `sema` root is owned by the caller and must declare both:
//!
//! ```text
//! pub mod layout;
//! pub mod types;
//! ```
//!
//! Layout rules:
//!
//! - struct members use their natural alignment, and the struct size is rounded
//!   up to the struct alignment (tail padding);
//! - unions place every member at offset 0, use the largest member size and
//!   alignment, then round the size up to that alignment;
//! - a final incomplete array is a flexible array member: it contributes
//!   alignment and its offset, but zero bytes to the size. It is invalid in a
//!   union or anywhere except last;
//! - `long` and `double` are 8-byte aligned in records. This has nothing to do
//!   with the 4-byte alignment of the Shy ABI stack-argument transport area;
//! - bit-fields and explicit alignment are rejected with a [`TypeError`]. No
//!   fake offsets or bit numbering are produced. Big-endian bit numbering can
//!   be added later when code generation for bit-fields exists.

use std::fmt;

/// The four facts that compare two function types. Bundling them keeps
/// `function_compatible` readable instead of taking eight positional
/// arguments that are easy to swap by accident.
#[derive(Clone, Copy)]
struct FunctionSignature<'a> {
    result: TypeId,
    params: &'a [TypeId],
    variadic: bool,
    prototype: bool,
}

use super::types::{Builtin, IntegerRank, QualifiedType, RecordKind, TypeId, TypeKind, TypeStore};

/// One member supplied to record layout.
///
/// `offset` is filled in by `TypeStore::complete_record`; it is 0 before the
/// record is completed. `name` is `None` for an anonymous struct/union member
/// and for an unnamed bit-field.
/// Where one field sits: byte offset of its storage, and the bit offset inside
/// that storage for a bit-field.
pub type FieldSlot = (u32, u32);

/// Field slots, the record layout they produce, and whether the record ends
/// with a flexible array member.
pub type RecordLayout = (Vec<FieldSlot>, Layout, bool);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecordField {
    pub name: Option<String>,
    pub ty: QualifiedType,
    /// Bit-field width, when the member is a bit-field. Layout currently
    /// reports [`TypeError::UnsupportedBitField`] instead of inventing a
    /// bit numbering.
    pub bit_width: Option<u32>,
    /// Bit offset inside the storage unit, for a bit-field.
    pub bit_offset: u32,
    /// Explicit `_Alignas`/`aligned` alignment. The parser rejects those
    /// spellings today; if one ever reaches layout, it is reported as
    /// [`TypeError::UnsupportedExplicitAlignment`], never ignored.
    pub explicit_align: Option<u32>,
    pub offset: u32,
}

impl RecordField {
    pub fn new(name: impl Into<String>, ty: QualifiedType) -> Self {
        Self {
            name: Some(name.into()),
            ty,
            bit_width: None,
            bit_offset: 0,
            explicit_align: None,
            offset: 0,
        }
    }

    pub fn anonymous(ty: QualifiedType) -> Self {
        Self {
            name: None,
            ty,
            bit_width: None,
            bit_offset: 0,
            explicit_align: None,
            offset: 0,
        }
    }
}

/// The members of a forward-declared or completed record.
///
/// A forward record has an empty `fields` list and its type has
/// `size == None`. Completion replaces both. `is_flexible` is set when the
/// final member is an incomplete array.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecordInfo {
    pub fields: Vec<RecordField>,
    pub is_flexible: bool,
}

/// A computed object layout.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Layout {
    pub size: u32,
    pub align: u32,
}

/// Why a type cannot be sized, aligned, laid out, or converted.
///
/// These are plain type-layer errors. The caller adds source spans and field
/// names when it turns them into diagnostics.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TypeError {
    /// `void` and function types have no object size or alignment.
    NotObject,
    IncompleteRecord,
    IncompleteArray,
    IncompleteType,
    NotInteger,
    NotArithmetic,
    ArrayTooLarge,
    LayoutOverflow,
    UnsupportedBitField,
    UnsupportedExplicitAlignment,
    FlexibleArrayNotLast,
    FlexibleArrayInUnion,
    NotARecord,
    RecordAlreadyComplete,
}

impl fmt::Display for TypeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            TypeError::NotObject => "void and function types have no object size or alignment",
            TypeError::IncompleteRecord => "record type is still incomplete",
            TypeError::IncompleteArray => "array type has no known bound",
            TypeError::IncompleteType => "type is incomplete",
            TypeError::NotInteger => "type is not an integer type",
            TypeError::NotArithmetic => "type is not an arithmetic type",
            TypeError::ArrayTooLarge => "array size exceeds 32-bit object sizes",
            TypeError::LayoutOverflow => "record layout exceeds 32-bit object sizes",
            TypeError::UnsupportedBitField => "bit-field layout is not implemented yet",
            TypeError::UnsupportedExplicitAlignment => {
                "explicit alignment in a record is not implemented yet"
            }
            TypeError::FlexibleArrayNotLast => "a flexible array member must be the last member",
            TypeError::FlexibleArrayInUnion => "a union cannot have a flexible array member",
            TypeError::NotARecord => "type is not a struct or union",
            TypeError::RecordAlreadyComplete => "record is already complete",
        };
        f.write_str(message)
    }
}

impl std::error::Error for TypeError {}

impl TypeStore {
    /// Size in bytes of a complete object type.
    ///
    /// `void`, function types, incomplete records, and incomplete arrays have
    /// no size. An incomplete array's alignment is still available through
    /// [`TypeStore::align_of`].
    pub fn size_of(&self, ty: TypeId) -> Result<u32, TypeError> {
        match self.get(ty).size {
            Some(size) => Ok(size),
            None => Err(self.incomplete_error(ty)),
        }
    }

    /// Alignment in bytes of a type.
    pub fn align_of(&self, ty: TypeId) -> Result<u32, TypeError> {
        match self.get(ty).align {
            Some(align) => Ok(align),
            None => Err(self.incomplete_error(ty)),
        }
    }

    fn incomplete_error(&self, ty: TypeId) -> TypeError {
        match self.kind(ty) {
            TypeKind::Void | TypeKind::Function { .. } => TypeError::NotObject,
            TypeKind::Array { .. } => TypeError::IncompleteArray,
            TypeKind::Record { .. } => TypeError::IncompleteRecord,
            _ => TypeError::IncompleteType,
        }
    }

    /// Defines a complete record in one step. The new record has a fresh
    /// identity, so two calls never produce compatible record types.
    pub fn define_record(
        &mut self,
        kind: RecordKind,
        fields: Vec<RecordField>,
    ) -> Result<TypeId, TypeError> {
        let ty = self.record_forward(kind);
        self.complete_record(ty, fields)?;
        Ok(ty)
    }

    /// Completes a forward-declared record and computes member offsets.
    ///
    /// On error the record stays incomplete and the arena is unchanged except
    /// for the forward entry itself. A successful call fills every
    /// [`RecordField::offset`] and returns the record's [`Layout`].
    pub fn complete_record(
        &mut self,
        ty: TypeId,
        fields: Vec<RecordField>,
    ) -> Result<Layout, TypeError> {
        let (record_id, kind) = match self.kind(ty) {
            TypeKind::Record { id, kind } => (*id, *kind),
            _ => return Err(TypeError::NotARecord),
        };
        if self.get(ty).size.is_some() {
            return Err(TypeError::RecordAlreadyComplete);
        }

        let (offsets, layout, is_flexible) = self.layout_record(kind, &fields)?;
        let mut fields = fields;
        for (field, (offset, bit_offset)) in fields.iter_mut().zip(offsets) {
            field.offset = offset;
            field.bit_offset = bit_offset;
        }

        self.records[record_id.index()] = RecordInfo {
            fields,
            is_flexible,
        };
        let record = &mut self.types[ty.index()];
        record.size = Some(layout.size);
        record.align = Some(layout.align);
        Ok(layout)
    }

    fn layout_record(
        &self,
        kind: RecordKind,
        fields: &[RecordField],
    ) -> Result<RecordLayout, TypeError> {
        for field in fields {
            if field.explicit_align.is_some() {
                return Err(TypeError::UnsupportedExplicitAlignment);
            }
        }

        let mut flexible = None;
        for (index, field) in fields.iter().enumerate() {
            if matches!(self.kind(field.ty.ty), TypeKind::Array { len: None, .. }) {
                if index + 1 != fields.len() {
                    return Err(TypeError::FlexibleArrayNotLast);
                }
                flexible = Some(index);
            }
        }
        if kind == RecordKind::Union && flexible.is_some() {
            return Err(TypeError::FlexibleArrayInUnion);
        }

        match kind {
            RecordKind::Struct => self.layout_struct(fields, flexible),
            RecordKind::Union => self.layout_union(fields),
        }
    }

    fn layout_struct(
        &self,
        fields: &[RecordField],
        flexible: Option<usize>,
    ) -> Result<RecordLayout, TypeError> {
        let mut offsets = Vec::with_capacity(fields.len());
        let mut offset = 0u32;
        let mut align = 1u32;

        // Bit-fields pack into a storage unit of their declared type without
        // straddling it, as GNU C lays them out: a field that does not fit
        // starts a new unit.
        let mut unit_start = 0u32;
        let mut unit_bits = 0u32;
        let mut bits_used = 0u32;
        let mut in_unit = false;

        for (index, field) in fields.iter().enumerate() {
            let field_align = self.align_of(field.ty.ty)?;
            let field_size = if flexible == Some(index) {
                self.require_complete_element(field)?;
                0
            } else {
                self.size_of(field.ty.ty)?
            };

            if let Some(width) = field.bit_width {
                let bits_per_unit = field_size * 8;
                if width == 0 {
                    // A zero-width field only pads to the next unit.
                    if in_unit {
                        offset = unit_start + bits_used.div_ceil(8);
                        in_unit = false;
                    }
                    offset = align_up(offset, field_align).ok_or(TypeError::LayoutOverflow)?;
                    offsets.push((offset, 0));
                    continue;
                }
                if !in_unit {
                    offset = align_up(offset, field_align).ok_or(TypeError::LayoutOverflow)?;
                    unit_start = offset;
                    bits_used = 0;
                    unit_bits = bits_per_unit;
                    in_unit = true;
                } else if bits_used + width > unit_bits {
                    offset = unit_start + unit_bits.div_ceil(8);
                    offset = align_up(offset, field_align).ok_or(TypeError::LayoutOverflow)?;
                    unit_start = offset;
                    bits_used = 0;
                    unit_bits = bits_per_unit;
                }
                offsets.push((unit_start, bits_used));
                bits_used += width;
                align = align.max(field_align);
                if bits_used >= unit_bits {
                    offset = unit_start + unit_bits.div_ceil(8);
                    in_unit = false;
                }
                continue;
            }

            if in_unit {
                offset = offset.max(unit_start + bits_used.div_ceil(8));
                in_unit = false;
            }
            offset = align_up(offset, field_align).ok_or(TypeError::LayoutOverflow)?;
            offsets.push((offset, 0));
            offset = offset
                .checked_add(field_size)
                .ok_or(TypeError::LayoutOverflow)?;
            align = align.max(field_align);
        }
        if in_unit {
            offset = offset.max(unit_start + bits_used.div_ceil(8));
        }

        let size = align_up(offset, align).ok_or(TypeError::LayoutOverflow)?;
        Ok((offsets, Layout { size, align }, flexible.is_some()))
    }

    fn layout_union(&self, fields: &[RecordField]) -> Result<RecordLayout, TypeError> {
        let mut offsets = Vec::with_capacity(fields.len());
        let mut size = 0u32;
        let mut align = 1u32;

        for field in fields {
            let field_size = self.size_of(field.ty.ty)?;
            let field_align = self.align_of(field.ty.ty)?;
            offsets.push((0, 0));
            size = size.max(field_size);
            align = align.max(field_align);
        }

        let size = align_up(size, align).ok_or(TypeError::LayoutOverflow)?;
        Ok((offsets, Layout { size, align }, false))
    }

    /// A flexible array member must have a complete element type. The array
    /// itself is incomplete, so `size_of` on the array cannot be used here.
    fn require_complete_element(&self, field: &RecordField) -> Result<(), TypeError> {
        let TypeKind::Array { element, .. } = self.kind(field.ty.ty) else {
            return Err(TypeError::IncompleteArray);
        };
        self.size_of(element.ty)?;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Integer promotions and usual arithmetic conversions
    // -----------------------------------------------------------------------

    /// C integer promotion. `_Bool`, `char`, and `short` become `int`, as
    /// does an enum whose underlying type has a rank below `int`. Wider
    /// types, including wider enums, are unchanged.
    pub fn integer_promotion(&self, ty: TypeId) -> Result<TypeId, TypeError> {
        match self.kind(ty) {
            TypeKind::Bool => Ok(self.builtin(Builtin::Int)),
            TypeKind::Enum(id) => {
                let underlying = self.enums[id.index()].underlying;
                match self.kind(underlying) {
                    TypeKind::Integer { rank, .. } if *rank < IntegerRank::Int => {
                        Ok(self.builtin(Builtin::Int))
                    }
                    _ => Ok(underlying),
                }
            }
            TypeKind::Integer { rank, .. } if *rank < IntegerRank::Int => {
                Ok(self.builtin(Builtin::Int))
            }
            TypeKind::Integer { .. } => Ok(ty),
            _ => Err(TypeError::NotInteger),
        }
    }

    /// The common type of two arithmetic operands.
    ///
    /// `float`/`double` win over integers; otherwise both operands are
    /// promoted, and the C signedness/rank rules choose the result. Because
    /// `long` and `long long` are both 64-bit but keep distinct ranks,
    /// `unsigned long` and `long long` produce `unsigned long long`.
    pub fn usual_arithmetic_conversions(&self, a: TypeId, b: TypeId) -> Result<TypeId, TypeError> {
        if !self.is_arithmetic(a) || !self.is_arithmetic(b) {
            return Err(TypeError::NotArithmetic);
        }

        if self.is_floating(a) || self.is_floating(b) {
            let wide = matches!(self.kind(a), TypeKind::Float64)
                || matches!(self.kind(b), TypeKind::Float64);
            return Ok(self.builtin(if wide {
                Builtin::Double
            } else {
                Builtin::Float
            }));
        }

        let a = self.integer_promotion(a)?;
        let b = self.integer_promotion(b)?;
        let (a_rank, a_unsigned) = self.integer_parts(a).ok_or(TypeError::NotInteger)?;
        let (b_rank, b_unsigned) = self.integer_parts(b).ok_or(TypeError::NotInteger)?;

        if a_rank == b_rank && a_unsigned == b_unsigned {
            return Ok(a);
        }
        if a_unsigned == b_unsigned {
            let rank = if a_rank >= b_rank { a_rank } else { b_rank };
            return Ok(self.integer_type(rank, a_unsigned));
        }

        let (unsigned_rank, signed_rank) = if a_unsigned {
            (a_rank, b_rank)
        } else {
            (b_rank, a_rank)
        };

        if unsigned_rank >= signed_rank {
            return Ok(self.integer_type(unsigned_rank, true));
        }
        if signed_rank.size() > unsigned_rank.size() {
            Ok(self.integer_type(signed_rank, false))
        } else {
            Ok(self.integer_type(signed_rank, true))
        }
    }

    fn integer_parts(&self, ty: TypeId) -> Option<(IntegerRank, bool)> {
        match self.kind(ty) {
            TypeKind::Integer { rank, unsigned } => Some((*rank, *unsigned)),
            TypeKind::Bool => Some((IntegerRank::Int, false)),
            TypeKind::Enum(id) => self.integer_parts(self.enums[id.index()].underlying),
            _ => None,
        }
    }
}

fn align_up(value: u32, align: u32) -> Option<u32> {
    let remainder = value % align;
    if remainder == 0 {
        Some(value)
    } else {
        value.checked_add(align - remainder)
    }
}

// ---------------------------------------------------------------------------
// Type compatibility
// ---------------------------------------------------------------------------

impl TypeStore {
    /// C type compatibility, used for redeclarations and
    /// `__builtin_types_compatible_p`.
    ///
    /// Top-level qualifiers are not represented by a `TypeId`; callers that
    /// care about them (for example a qualified pointer target) still get them
    /// compared inside pointer and array types. Records and enums are
    /// compatible only with the same identity, so two structurally identical
    /// definitions are different types.
    ///
    /// A function with a prototype is compatible with an old-style `()`
    /// function when the prototype has no ellipsis and every parameter already
    /// has its default-promoted type. `int f()` and `int f(int)` are
    /// compatible; `int f(char)` is not.
    pub fn is_compatible(&self, a: TypeId, b: TypeId) -> bool {
        if a == b {
            return true;
        }
        match (self.kind(a), self.kind(b)) {
            (TypeKind::Void, TypeKind::Void) => true,
            (TypeKind::Bool, TypeKind::Bool) => true,
            (
                TypeKind::Integer {
                    rank: a_rank,
                    unsigned: a_unsigned,
                },
                TypeKind::Integer {
                    rank: b_rank,
                    unsigned: b_unsigned,
                },
            ) => a_rank == b_rank && a_unsigned == b_unsigned,
            (TypeKind::Float32, TypeKind::Float32) => true,
            (TypeKind::Float64, TypeKind::Float64) => true,
            (TypeKind::Pointer { pointee: a }, TypeKind::Pointer { pointee: b }) => {
                self.qualified_compatible(*a, *b)
            }
            (
                TypeKind::Array {
                    element: a_element,
                    len: a_len,
                },
                TypeKind::Array {
                    element: b_element,
                    len: b_len,
                },
            ) => {
                self.qualified_compatible(*a_element, *b_element)
                    && match (a_len, b_len) {
                        (Some(a_len), Some(b_len)) => a_len == b_len,
                        _ => true,
                    }
            }
            (
                TypeKind::Function {
                    result: a_result,
                    params: a_params,
                    variadic: a_variadic,
                    prototype: a_prototype,
                },
                TypeKind::Function {
                    result: b_result,
                    params: b_params,
                    variadic: b_variadic,
                    prototype: b_prototype,
                },
            ) => self.function_compatible(
                FunctionSignature {
                    result: *a_result,
                    params: a_params,
                    variadic: *a_variadic,
                    prototype: *a_prototype,
                },
                FunctionSignature {
                    result: *b_result,
                    params: b_params,
                    variadic: *b_variadic,
                    prototype: *b_prototype,
                },
            ),
            (TypeKind::Record { id: a, .. }, TypeKind::Record { id: b, .. }) => a == b,
            (TypeKind::Enum(a), TypeKind::Enum(b)) => a == b,
            _ => false,
        }
    }

    fn qualified_compatible(&self, a: QualifiedType, b: QualifiedType) -> bool {
        a.qualifiers == b.qualifiers && self.is_compatible(a.ty, b.ty)
    }

    fn function_compatible(&self, a: FunctionSignature<'_>, b: FunctionSignature<'_>) -> bool {
        if !self.is_compatible(a.result, b.result) || a.variadic != b.variadic {
            return false;
        }
        match (a.prototype, b.prototype) {
            (true, true) => {
                a.params.len() == b.params.len()
                    && a.params
                        .iter()
                        .zip(b.params)
                        .all(|(&a, &b)| self.is_compatible(a, b))
            }
            (false, false) => true,
            (true, false) => self.old_style_compatible(a.params, a.variadic),
            (false, true) => self.old_style_compatible(b.params, b.variadic),
        }
    }

    fn old_style_compatible(&self, params: &[TypeId], variadic: bool) -> bool {
        if variadic {
            return false;
        }
        params
            .iter()
            .all(|&param| match self.default_argument_promotion(param) {
                Some(promoted) => self.is_compatible(param, promoted),
                None => false,
            })
    }

    fn default_argument_promotion(&self, ty: TypeId) -> Option<TypeId> {
        match self.kind(ty) {
            TypeKind::Bool | TypeKind::Integer { .. } | TypeKind::Enum(_) => {
                self.integer_promotion(ty).ok()
            }
            TypeKind::Float32 => Some(self.builtin(Builtin::Double)),
            TypeKind::Float64 | TypeKind::Pointer { .. } | TypeKind::Record { .. } => Some(ty),
            _ => None,
        }
    }
}
