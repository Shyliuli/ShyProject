//! CFG simplification: branch folding, unreachable blocks, trivial phis, and
//! empty forwarding blocks.
//!
//! Every rewrite keeps value and block IDs stable, and the entry block is
//! never removed. A block that is reachable from the entry is never deleted,
//! so loops and intentional nontermination survive all of these passes.
//!
//! # Phi inputs
//!
//! A phi input is keyed by predecessor *block*, not by edge. When a terminator
//! change removes an edge, the target's phi input for that predecessor is
//! pruned. A missing input is never invented: the IR has no `undef`, so an
//! incomplete phi stays incomplete and the rewrites that depend on complete
//! coverage simply do not fire.

use std::collections::HashMap;

use super::const_prop::apply_replacements;
use crate::ir::{BlockId, Constant, Function, Operand, PhiIncoming, Terminator, ValueId};

// ---------------------------------------------------------------------------
// Constant branches and switches
// ---------------------------------------------------------------------------

/// Replace branches with equal destinations, constant branches, switches
/// whose destination does not depend on the value, and switches on a constant
/// with direct jumps. The original span is preserved.
///
/// A constant switch compares a case constant with the switched value after
/// masking both to the switch type, which is the IR's documented comparison.
/// If a malformed switch repeats a case value, the first matching case wins.
///
/// After the rewrite, phi inputs that named a now-removed predecessor edge are
/// pruned. A target that became unreachable is left for
/// `remove_unreachable_blocks`.
pub fn simplify_branches(function: &mut Function) -> bool {
    let mut changed = false;
    for (_, block) in function.live_blocks_mut() {
        let replacement = match &block.terminator {
            Terminator::Branch {
                condition: Operand::Constant(condition),
                then_block,
                else_block,
                span,
            } => {
                let target = if condition.as_u64() != 0 {
                    *then_block
                } else {
                    *else_block
                };
                Some((target, *span))
            }
            Terminator::Branch {
                then_block,
                else_block,
                span,
                ..
            } if then_block == else_block => Some((*then_block, *span)),
            Terminator::Switch {
                value: Operand::Constant(value),
                ty,
                default,
                cases,
                span,
            } => {
                let wanted = Constant::raw(*ty, value.bits);
                let target = cases
                    .iter()
                    .find(|case| Constant::raw(*ty, case.value.bits) == wanted)
                    .map(|case| case.target)
                    .unwrap_or(*default);
                Some((target, *span))
            }
            Terminator::Switch {
                default,
                cases,
                span,
                ..
            } if cases.is_empty() || cases.iter().all(|case| case.target == *default) => {
                Some((*default, *span))
            }
            _ => None,
        };
        if let Some((target, span)) = replacement {
            let mut jump = Terminator::jump(target);
            jump.set_span(span);
            block.terminator = jump;
            changed = true;
        }
    }
    if changed {
        changed |= prune_stale_phi_inputs(function);
    }
    changed
}

/// Drop phi inputs whose predecessor block no longer has an edge to the phi's
/// block, and drop duplicate inputs for one predecessor. Returns whether any
/// phi changed.
///
/// This is the repair step for terminator rewrites. It is also safe to call on
/// its own; it only removes information that the CFG no longer supports.
pub fn prune_stale_phi_inputs(function: &mut Function) -> bool {
    let predecessors = predecessor_map(function);
    let mut changed = false;
    for (id, block) in function.live_blocks_mut() {
        let expected = &predecessors[id.0 as usize];
        for phi in &mut block.phis {
            let before = phi.incoming.len();
            let mut seen: Vec<BlockId> = Vec::new();
            phi.incoming.retain(|incoming| {
                if !expected.contains(&incoming.predecessor) || seen.contains(&incoming.predecessor)
                {
                    return false;
                }
                seen.push(incoming.predecessor);
                true
            });
            if phi.incoming.len() != before {
                changed = true;
            }
        }
    }
    changed
}

// ---------------------------------------------------------------------------
// Unreachable blocks
// ---------------------------------------------------------------------------

/// Remove every live block that the entry cannot reach.
///
/// Reachability follows distinct successor blocks; repeated switch edges do
/// not change the answer. `Function::remove_block` tombstones the block and
/// drops phi inputs that name it, so surviving phis stay consistent. The entry
/// block and every reachable loop are kept, which preserves nontermination.
pub fn remove_unreachable_blocks(function: &mut Function) -> bool {
    let mut reachable = vec![false; function.blocks.len()];
    reachable[function.entry.0 as usize] = true;
    let mut stack = vec![function.entry];
    while let Some(id) = stack.pop() {
        let Some(block) = function.blocks[id.0 as usize].as_ref() else {
            continue;
        };
        for successor in block.unique_successors() {
            let index = successor.0 as usize;
            if index < reachable.len() && !reachable[index] {
                reachable[index] = true;
                stack.push(successor);
            }
        }
    }

    let mut dead: Vec<BlockId> = Vec::new();
    for (id, _) in function.live_blocks() {
        if !reachable[id.0 as usize] {
            dead.push(id);
        }
    }
    for id in &dead {
        function.remove_block(*id);
    }
    !dead.is_empty()
}

// ---------------------------------------------------------------------------
// Trivial phis
// ---------------------------------------------------------------------------

/// Replace a scalar phi by an operand when every non-self input is that same
/// operand.
///
/// Self references are ignored: a back-edge input that names the phi itself
/// adds no information, so `a = phi(x, a)` is `x`. A phi whose inputs are all
/// self references (or empty) has no value to substitute and is left alone;
/// the IR has no `undef` to invent.
///
/// The phi must have exactly one input for each live predecessor, otherwise a
/// missing edge could hide a path on which the candidate is not defined. A
/// candidate that is another phi in the same block is skipped: phis in one
/// block are evaluated in parallel, and replacing a mutual reference would
/// make the substitution circular.
pub fn simplify_trivial_phis(function: &mut Function) -> bool {
    let predecessors = predecessor_map(function);
    let mut replacements: HashMap<ValueId, Operand> = HashMap::new();
    for (id, block) in function.live_blocks() {
        let expected = &predecessors[id.0 as usize];
        for phi in &block.phis {
            if phi.incoming.len() != expected.len()
                || !expected
                    .iter()
                    .all(|predecessor| phi.incoming_for(*predecessor).is_some())
            {
                continue;
            }
            let mut candidate: Option<Operand> = None;
            let mut conflict = false;
            for incoming in &phi.incoming {
                if incoming.value == Operand::Value(phi.result) {
                    continue;
                }
                match candidate {
                    None => candidate = Some(incoming.value),
                    Some(previous) if previous != incoming.value => {
                        conflict = true;
                        break;
                    }
                    Some(_) => {}
                }
            }
            if conflict {
                continue;
            }
            let Some(candidate) = candidate else { continue };
            if let Operand::Value(target) = candidate
                && block.phis.iter().any(|other| other.result == target)
            {
                continue;
            }
            replacements.insert(phi.result, candidate);
        }
    }
    apply_replacements(function, &replacements)
}

// ---------------------------------------------------------------------------
// Empty forwarding blocks
// ---------------------------------------------------------------------------

/// Bypass a block that has no phis, no instructions, and a single jump.
///
/// Every predecessor edge is redirected to the jump target and the block is
/// removed. This is only done when the target's phi inputs can be updated
/// soundly:
///
/// - the target has no phis, so there is nothing to update, or
/// - the target has a phi input for the bypassed block, and every predecessor
///   either has no input for that phi yet, or already has the same operand.
///   If a predecessor would need two different values for its one input slot,
///   the block is left in place.
///
/// The entry block and a block that jumps to itself are never removed, so the
/// entry stays valid and a self-loop is not rewritten by hand.
pub fn remove_forwarding_blocks(function: &mut Function) -> bool {
    let mut changed = false;
    loop {
        let mut removed = false;
        let candidates: Vec<BlockId> = function.live_blocks().map(|(id, _)| id).collect();
        for id in candidates {
            if id == function.entry {
                continue;
            }
            let Some(target) = forwarding_target(function, id) else {
                continue;
            };
            let predecessors = function.predecessors(id);
            if predecessors.is_empty() {
                continue;
            }
            let Some(values) = forwarding_values(function, id, target, &predecessors) else {
                continue;
            };
            forward_block(function, id, target, &predecessors, &values);
            removed = true;
            break;
        }
        if !removed {
            return changed;
        }
        changed = true;
    }
}

/// The jump target of a block that is empty except for its terminator.
fn forwarding_target(function: &Function, id: BlockId) -> Option<BlockId> {
    let block = function.block(id);
    if !block.phis.is_empty() || !block.insts.is_empty() {
        return None;
    }
    let target = match &block.terminator {
        Terminator::Jump { target, .. } => *target,
        _ => return None,
    };
    if target == id {
        return None;
    }
    Some(target)
}

/// For each phi of the target, the operand that must replace the bypassed
/// block's input. `None` means the redirection would be unsound.
fn forwarding_values(
    function: &Function,
    from: BlockId,
    to: BlockId,
    predecessors: &[BlockId],
) -> Option<Vec<Operand>> {
    let target = function.block(to);
    if target.phis.is_empty() {
        return Some(Vec::new());
    }
    let mut values = Vec::with_capacity(target.phis.len());
    for phi in &target.phis {
        let value = *phi.incoming_for(from)?;
        for predecessor in predecessors {
            if *predecessor == from {
                return None;
            }
            if let Some(existing) = phi.incoming_for(*predecessor)
                && *existing != value
            {
                return None;
            }
        }
        values.push(value);
    }
    Some(values)
}

/// Redirect every predecessor edge, translate the target's phi inputs, and
/// remove the bypassed block.
fn forward_block(
    function: &mut Function,
    from: BlockId,
    to: BlockId,
    predecessors: &[BlockId],
    values: &[Operand],
) {
    {
        let target = function.block_mut(to);
        for (index, value) in values.iter().enumerate() {
            for predecessor in predecessors {
                let phi = &mut target.phis[index];
                let present = phi
                    .incoming
                    .iter()
                    .any(|incoming| incoming.predecessor == *predecessor);
                if !present {
                    phi.incoming.push(PhiIncoming::new(*predecessor, *value));
                }
            }
        }
    }
    for predecessor in predecessors {
        function.block_mut(*predecessor).replace_successor(from, to);
    }
    function.remove_block(from);
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Distinct predecessor blocks for every block ID, keyed by block index.
///
/// Successor edges are deduplicated per terminator, matching
/// `Function::predecessors`: one predecessor block supplies exactly one phi
/// input even when several switch cases reach it.
fn predecessor_map(function: &Function) -> Vec<Vec<BlockId>> {
    let mut predecessors: Vec<Vec<BlockId>> = vec![Vec::new(); function.blocks.len()];
    for (id, block) in function.live_blocks() {
        for successor in block.unique_successors() {
            predecessors[successor.0 as usize].push(id);
        }
    }
    predecessors
}
