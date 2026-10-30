//! Promote scalar stack slots to SSA values (mem2reg).
//!
//! The pass has two halves. The first half *plans* and never mutates: it
//! decides whether a slot is promotable and, if so, where phis have to go. The
//! second half rewrites loads and stores once the plan is known to be sound, so
//! there is nothing to roll back.
//!
//! # Eligibility
//!
//! A slot is promoted only when every reference to it in the whole function is
//! a plain `load` or `store` at offset zero, with exactly the slot's type and
//! with neither `volatile` nor an atomic ordering. One reference of any other
//! kind rejects the slot:
//!
//! * `slot_addr` (the address escapes, so the slot could be written indirectly),
//! * a load/store at a nonzero offset, with a different type, or with volatile
//!   or atomic semantics (partial or observable access),
//! * `memcpy`, `memset`, or an inline-assembly binding that names the slot,
//! * an aggregate slot, which this pass never touches.
//!
//! # Definite assignment
//!
//! Removing a load is only valid when the value it would read has been written
//! on every path from the entry. The pass computes that with a forward "must be
//! assigned" dataflow over reachable blocks and checks every reachable load
//! against it. If any reachable load can see an uninitialized slot, the whole
//! slot stays in memory untouched. The pass never invents a zero or an `undef`
//! value for a read that might be uninitialized.
//!
//! # Rewrite
//!
//! Promotion is Cytron's SSA construction: place phis at the iterated dominance
//! frontier of the slot's definition blocks, pruned to blocks where the slot is
//! live on entry, then walk the dominator tree with a stack of reaching values,
//! replacing loads with the stack top and dropping stores after pushing their
//! value. Unreachable blocks are left alone; their loads and stores keep using
//! the slot, whose entry in the slots table is retained.
//!
//! Phis are fed once per predecessor *block*: a terminator that reaches the
//! same block through several edges is still a single phi input, as the IR
//! requires. Only reachable predecessors get inputs.

use crate::analysis::Dominators;
use crate::ir::{
    Address, AddressBase, BlockId, Function, InstKind, Operand, PhiIncoming, SlotId, Type, ValueId,
};

/// Promote every eligible scalar slot in `function` to SSA values.
///
/// Returns `true` when at least one slot was rewritten, which lets a caller
/// iterate passes to a fixed point. Aggregate slots and slots that cannot be
/// proven initialized are left in memory.
pub fn promote(function: &mut Function) -> bool {
    let dominators = Dominators::compute(function);
    let mut changed = false;
    for index in 0..function.slots.len() {
        let slot = SlotId(index as u32);
        let Some(plan) = plan_promotion(function, &dominators, slot) else {
            continue;
        };
        apply_promotion(function, &dominators, &plan);
        changed = true;
    }
    changed
}

/// Everything the rewrite needs to know about one slot.
struct Promotion {
    slot: SlotId,
    ty: Type,
    /// Blocks that receive a new phi, in ascending block ID order.
    phi_blocks: Vec<BlockId>,
}

/// Decide whether `slot` can be promoted and where its phis belong.
///
/// Returns `None` when the slot is ineligible, has a possibly uninitialized
/// read, or has no reachable access at all (nothing to do).
fn plan_promotion(function: &Function, dominators: &Dominators, slot: SlotId) -> Option<Promotion> {
    if !has_exact_accesses(function, slot) {
        return None;
    }
    if !definitely_assigned(function, dominators, slot) {
        return None;
    }

    let mut reachable_loads = false;
    let mut def_blocks = Vec::new();
    for &block in dominators.reverse_postorder() {
        let (upward_use, has_store) = block_access(function, slot, block);
        reachable_loads |= upward_use || block_reads(function, slot, block);
        if has_store {
            def_blocks.push(block);
        }
    }
    if !reachable_loads && def_blocks.is_empty() {
        return None;
    }

    let live_in = compute_liveness(function, dominators, slot);
    let phi_blocks = find_phi_blocks(dominators, &def_blocks, &live_in);
    Some(Promotion {
        slot,
        ty: function.slot(slot).ty,
        phi_blocks,
    })
}

// ---------------------------------------------------------------------------
// Eligibility
// ---------------------------------------------------------------------------

/// True when every reference to `slot` is a normal scalar load or store of
/// exactly the slot's own type at offset zero.
fn has_exact_accesses(function: &Function, slot: SlotId) -> bool {
    let ty = function.slot(slot).ty;
    if !ty.is_scalar() {
        return false;
    }
    for block in function.blocks.iter().flatten() {
        for inst in &block.insts {
            let usable = match &inst.kind {
                InstKind::Load {
                    address,
                    ty: access_ty,
                    volatile,
                    atomic,
                } => {
                    if !address_mentions_slot(address, slot) {
                        true
                    } else {
                        inst.result.is_some()
                            && is_exact_slot_address(address, slot)
                            && *access_ty == ty
                            && !*volatile
                            && !atomic.is_atomic()
                    }
                }
                InstKind::Store {
                    address,
                    value,
                    ty: access_ty,
                    volatile,
                    atomic,
                } => {
                    if !address_mentions_slot(address, slot) {
                        true
                    } else {
                        is_exact_slot_address(address, slot)
                            && *access_ty == ty
                            && function.operand_type(*value) == ty
                            && !*volatile
                            && !atomic.is_atomic()
                    }
                }
                // Taking the address lets arbitrary code read or write the slot.
                InstKind::SlotAddr { slot: escaped } => *escaped != slot,
                InstKind::MemCpy { dst, src, .. } => {
                    !address_mentions_slot(dst, slot) && !address_mentions_slot(src, slot)
                }
                InstKind::MemSet { dst, .. } => !address_mentions_slot(dst, slot),
                InstKind::InlineAsm(asm) => asm
                    .bindings
                    .iter()
                    .all(|binding| !address_mentions_slot(&binding.address, slot)),
                _ => true,
            };
            if !usable {
                return false;
            }
        }
    }
    true
}

/// True for `[slot]` with no displacement, the only address form `mem2reg`
/// understands.
fn is_exact_slot_address(address: &Address, slot: SlotId) -> bool {
    address.offset == 0 && matches!(&address.base, AddressBase::Slot(base) if *base == slot)
}

/// True when the address names `slot` in any form, including `[slot + 4]`.
fn address_mentions_slot(address: &Address, slot: SlotId) -> bool {
    matches!(&address.base, AddressBase::Slot(base) if *base == slot)
}

// ---------------------------------------------------------------------------
// Definite assignment
// ---------------------------------------------------------------------------

/// True when every reachable load of `slot` is preceded by a store on every
/// path from the entry.
///
/// `in` says whether the slot is definitely written on entry to a block. The
/// entry starts as `false`; every other reachable block starts at the top of
/// the lattice (`true`) and is lowered by intersecting its predecessors.
fn definitely_assigned(function: &Function, dominators: &Dominators, slot: SlotId) -> bool {
    let count = function.blocks.len();
    let entry = dominators.entry();
    let mut assigned_on_entry = vec![true; count];
    let mut assigned_on_exit = vec![true; count];
    assigned_on_entry[entry.0 as usize] = false;
    assigned_on_exit[entry.0 as usize] = block_exit(function, slot, entry, false);

    loop {
        let mut changed = false;
        for &block in dominators.reverse_postorder() {
            if block == entry {
                continue;
            }
            let new_entry = dominators
                .predecessors(block)
                .iter()
                .all(|predecessor| assigned_on_exit[predecessor.0 as usize]);
            if new_entry != assigned_on_entry[block.0 as usize] {
                assigned_on_entry[block.0 as usize] = new_entry;
                changed = true;
            }
            let new_exit = block_exit(function, slot, block, new_entry);
            if new_exit != assigned_on_exit[block.0 as usize] {
                assigned_on_exit[block.0 as usize] = new_exit;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }

    for &block in dominators.reverse_postorder() {
        let mut assigned = assigned_on_entry[block.0 as usize];
        for inst in &function.block(block).insts {
            match &inst.kind {
                InstKind::Load { address, .. }
                    if !assigned && address_mentions_slot(address, slot) =>
                {
                    return false;
                }
                InstKind::Store { address, .. } if address_mentions_slot(address, slot) => {
                    assigned = true;
                }
                _ => {}
            }
        }
    }
    true
}

/// Whether the slot is definitely written after executing `block`, starting
/// from `assigned`.
fn block_exit(function: &Function, slot: SlotId, block: BlockId, mut assigned: bool) -> bool {
    for inst in &function.block(block).insts {
        if let InstKind::Store { address, .. } = &inst.kind
            && address_mentions_slot(address, slot)
        {
            assigned = true;
        }
    }
    assigned
}

// ---------------------------------------------------------------------------
// Liveness and phi placement
// ---------------------------------------------------------------------------

/// True when a load of the slot can execute in `block` before any store, and
/// whether the block stores to the slot at all.
///
/// The first flag is the "upward exposed use" of classic liveness: only a read
/// before the first write depends on the incoming value.
fn block_access(function: &Function, slot: SlotId, block: BlockId) -> (bool, bool) {
    let mut upward_use = false;
    let mut has_store = false;
    let mut before_first_store = true;
    for inst in &function.block(block).insts {
        match &inst.kind {
            InstKind::Load { address, .. }
                if before_first_store && address_mentions_slot(address, slot) =>
            {
                upward_use = true;
            }
            InstKind::Store { address, .. } if address_mentions_slot(address, slot) => {
                has_store = true;
                before_first_store = false;
            }
            _ => {}
        }
    }
    (upward_use, has_store)
}

/// True when `block` reads the slot at all. Used only to tell whether a slot
/// has any reachable access worth rewriting.
fn block_reads(function: &Function, slot: SlotId, block: BlockId) -> bool {
    function.block(block).insts.iter().any(|inst| {
        matches!(&inst.kind, InstKind::Load { address, .. } if address_mentions_slot(address, slot))
    })
}

/// Backward "is the incoming value still needed" dataflow over reachable
/// blocks. A block is live on entry when it reads before writing, or when it
/// does not write and a successor is live on entry.
fn compute_liveness(function: &Function, dominators: &Dominators, slot: SlotId) -> Vec<bool> {
    let count = function.blocks.len();
    let mut live_in = vec![false; count];
    loop {
        let mut changed = false;
        for &block in dominators.reverse_postorder() {
            let (upward_use, has_store) = block_access(function, slot, block);
            let live_out = function
                .block(block)
                .unique_successors()
                .iter()
                .any(|successor| live_in[successor.0 as usize]);
            let new_live_in = upward_use || (!has_store && live_out);
            if new_live_in != live_in[block.0 as usize] {
                live_in[block.0 as usize] = new_live_in;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    live_in
}

/// Iterated dominance frontier of the definition blocks, pruned to blocks
/// where the slot is live on entry. Returns the phi blocks in ascending ID
/// order.
fn find_phi_blocks(
    dominators: &Dominators,
    def_blocks: &[BlockId],
    live_in: &[bool],
) -> Vec<BlockId> {
    let count = live_in.len();
    let mut picked = vec![false; count];
    let mut worklist = Vec::new();
    for &definition in def_blocks {
        for &block in dominators.frontier(definition) {
            record_phi_candidate(block, live_in, &mut picked, &mut worklist);
        }
    }
    while let Some(block) = worklist.pop() {
        for &frontier_block in dominators.frontier(block) {
            record_phi_candidate(frontier_block, live_in, &mut picked, &mut worklist);
        }
    }
    (0..count)
        .map(|index| BlockId(index as u32))
        .filter(|block| picked[block.0 as usize])
        .collect()
}

/// Record one phi block, ignoring duplicates and blocks where the slot is dead
/// on entry.
fn record_phi_candidate(
    block: BlockId,
    live_in: &[bool],
    picked: &mut [bool],
    worklist: &mut Vec<BlockId>,
) {
    if live_in[block.0 as usize] && !picked[block.0 as usize] {
        picked[block.0 as usize] = true;
        worklist.push(block);
    }
}

// ---------------------------------------------------------------------------
// Rewrite
// ---------------------------------------------------------------------------

/// What the rename walk does with one instruction.
enum Action {
    /// Read the slot: replace uses with the current value, then remove.
    Load(ValueId),
    /// Write the slot: remember the stored value, then remove.
    Store(Operand),
    /// Anything else stays.
    Keep,
}

/// Insert the planned phis, then rename loads and stores across the dominator
/// tree. The CFG is not changed, so the dominators stay valid.
fn apply_promotion(function: &mut Function, dominators: &Dominators, plan: &Promotion) {
    let count = function.blocks.len();
    let mut phi_value: Vec<Option<ValueId>> = vec![None; count];
    for &block in &plan.phi_blocks {
        let result = function.push_phi(block, plan.ty, Vec::new());
        phi_value[block.0 as usize] = Some(result);
    }

    let mut stack: Vec<Operand> = Vec::new();
    rename_block(
        function,
        dominators,
        plan,
        dominators.entry(),
        &phi_value,
        &mut stack,
    );

    // Inputs were appended in dominator-tree order; sort by predecessor so the
    // textual IR is deterministic.
    for &block in &plan.phi_blocks {
        let result = phi_value[block.0 as usize].expect("phi was created above");
        let phi = function
            .block_mut(block)
            .phis
            .iter_mut()
            .find(|phi| phi.result == result)
            .expect("phi was created above");
        phi.incoming.sort_by_key(|incoming| incoming.predecessor);
    }
}

/// Cytron's rename step for one slot: push the reaching values, consume the
/// block's loads and stores, fill the phis of successor blocks, recurse into
/// the dominator tree, then drop this block's values.
fn rename_block(
    function: &mut Function,
    dominators: &Dominators,
    plan: &Promotion,
    block: BlockId,
    phi_value: &[Option<ValueId>],
    stack: &mut Vec<Operand>,
) {
    let stack_base = stack.len();
    if let Some(value) = phi_value[block.0 as usize] {
        stack.push(Operand::Value(value));
    }

    let mut index = 0;
    while index < function.block(block).insts.len() {
        let action = {
            let inst = &function.block(block).insts[index];
            match &inst.kind {
                InstKind::Load { address, .. } if is_exact_slot_address(address, plan.slot) => {
                    Action::Load(
                        inst.result
                            .expect("eligible load of a promoted slot has a result"),
                    )
                }
                InstKind::Store { address, value, .. }
                    if is_exact_slot_address(address, plan.slot) =>
                {
                    Action::Store(*value)
                }
                _ => Action::Keep,
            }
        };
        match action {
            Action::Load(result) => {
                let reaching = *stack
                    .last()
                    .expect("definite assignment guarantees a reaching value");
                function.replace_uses(result, reaching);
                function.block_mut(block).insts.remove(index);
            }
            Action::Store(value) => {
                stack.push(value);
                function.block_mut(block).insts.remove(index);
            }
            Action::Keep => index += 1,
        }
    }

    // One input per predecessor block, even with several edges to the same
    // successor.
    for successor in function.block(block).unique_successors() {
        let Some(result) = phi_value[successor.0 as usize] else {
            continue;
        };
        let reaching = *stack
            .last()
            .expect("phi inputs come from blocks with a reaching value");
        let phi = function
            .block_mut(successor)
            .phis
            .iter_mut()
            .find(|phi| phi.result == result)
            .expect("phi was created by this promotion");
        phi.incoming.push(PhiIncoming::new(block, reaching));
    }

    for &child in dominators.children(block) {
        rename_block(function, dominators, plan, child, phi_value, stack);
    }
    stack.truncate(stack_base);
}
