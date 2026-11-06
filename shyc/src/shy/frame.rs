//! Frame layout for the Shy backend.
//!
//! The backend keeps every SSA value in memory: a value has a fixed spill
//! offset, and instructions load their operands into scratch registers, compute
//! there, and store the result back. That is deliberately simple and makes
//! control flow, calls, and phi copies easy to reason about.
//!
//! All offsets are positive displacements from the frame pointer `fx`.
//! `fx` points just above the saved caller frame pointer, so the first frame
//! byte is `[fx]`. The saved `fx` lives at `[fx - 4]` and the return address at
//! `[fx - 8]`.

use crate::ir::{Function, Layout, SlotId, Type, ValueId};

/// A 64-bit object occupies two 4-byte words in big-endian order: the high
/// word is at the lower address. Spill slots use the same representation as
/// ordinary memory so a value can be copied between the two without shuffling.
pub(crate) fn is_64(ty: Type) -> bool {
    matches!(ty, Type::I64 | Type::F64)
}

/// Bytes a scalar value moves through memory.
///
/// A pointer object is as wide as the data model says, but the address it
/// holds is one 32-bit ShyISA word at the object's start, so a load or a
/// store moves only that word.
pub(crate) fn scalar_bytes(ty: Type, layout: Layout) -> u32 {
    if ty.is_pointer() {
        4
    } else {
        ty.size(layout).unwrap_or(0)
    }
}

fn align_up(value: u32, align: u32) -> u32 {
    if align <= 1 {
        value
    } else {
        value.div_ceil(align) * align
    }
}

pub(crate) struct Frame {
    /// Spill offset of every SSA value. Aggregate values keep a pointer here.
    value_offsets: Vec<u32>,
    /// Object offset for values of aggregate type (the value itself is the
    /// address of this object).
    object_offsets: Vec<u32>,
    /// Temporary storage used while copying phi inputs on an edge.
    phi_offsets: Vec<Option<u32>>,
    slot_offsets: Vec<u32>,
    /// Hidden `sret` pointer for a function that returns an aggregate.
    sret: Option<u32>,
    size: u32,
}

impl Frame {
    pub(crate) fn build(function: &Function, layout: Layout) -> Frame {
        let mut frame = Frame {
            value_offsets: vec![0; function.values.len()],
            object_offsets: vec![0; function.values.len()],
            phi_offsets: vec![None; function.values.len()],
            slot_offsets: vec![0; function.slots.len()],
            sret: None,
            size: 0,
        };
        let mut cursor = 0u32;

        for (index, slot) in function.slots.iter().enumerate() {
            let align = slot.align.max(1);
            cursor = align_up(cursor, align);
            frame.slot_offsets[index] = cursor;
            cursor = cursor.saturating_add(slot.size(layout).max(1));
        }

        for (index, value) in function.values.iter().enumerate() {
            let ty = value.ty;
            if ty.is_aggregate() {
                let size = ty.size(layout).unwrap_or(0);
                let align = ty.align(layout).unwrap_or(1).max(1);
                cursor = align_up(cursor, align);
                frame.object_offsets[index] = cursor;
                cursor = cursor.saturating_add(size.max(1));
                cursor = align_up(cursor, 4);
                frame.value_offsets[index] = cursor;
                cursor = cursor.saturating_add(4);
            } else if is_64(ty) {
                cursor = align_up(cursor, 4);
                frame.value_offsets[index] = cursor;
                cursor = cursor.saturating_add(8);
            } else {
                cursor = align_up(cursor, 4);
                frame.value_offsets[index] = cursor;
                cursor = cursor.saturating_add(4);
            }
        }

        for (_, block) in function.live_blocks() {
            for phi in &block.phis {
                cursor = align_up(cursor, 4);
                frame.phi_offsets[phi.result.0 as usize] = Some(cursor);
                cursor = cursor.saturating_add(8);
            }
        }

        if function.signature.ret.is_aggregate() {
            cursor = align_up(cursor, 4);
            frame.sret = Some(cursor);
            cursor = cursor.saturating_add(4);
        }

        frame.size = align_up(cursor, 4);
        frame
    }

    pub(crate) fn size(&self) -> u32 {
        self.size
    }

    pub(crate) fn value(&self, value: ValueId) -> u32 {
        self.value_offsets[value.0 as usize]
    }

    pub(crate) fn object(&self, value: ValueId) -> u32 {
        self.object_offsets[value.0 as usize]
    }

    pub(crate) fn phi(&self, value: ValueId) -> Option<u32> {
        self.phi_offsets[value.0 as usize]
    }

    pub(crate) fn slot(&self, slot: SlotId) -> u32 {
        self.slot_offsets[slot.0 as usize]
    }

    pub(crate) fn sret(&self) -> Option<u32> {
        self.sret
    }
}
