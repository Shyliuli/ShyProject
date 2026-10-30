//! Dead pure value elimination by mark and sweep.
//!
//! This is the textbook formulation, with no use counts and no
//! strongly-connected-component tricks:
//!
//! - Roots are the operands of every terminator and of every instruction that
//!   stays: an instruction whose `effects.is_pure()` is false, or a statement
//!   with no result. A kept store, call, `memcpy`/`memset`, inline assembly,
//!   volatile/atomic access, division/shift, or ordinary trapping load makes
//!   its operands live.
//! - A live value makes the operands of its definition live too, whether the
//!   definition is a phi or an instruction. The sweep then deletes pure
//!   instructions and phis whose results are not live.
//!
//! Liveness flows only from observable roots, so a group of phis that
//! reference each other but are never read disappears as a group. Deleting
//! those phis is safe because a phi has no effects. DCE never edits
//! terminators or blocks, so a reachable loop or intentional nontermination is
//! untouched.
//!
//! Only `effects.is_pure()` definitions are swept: an unproven division,
//! shift, load, call, or asm stays even when its result is unused.
//! `remove_unused_definitions` is the separate helper the fold/identity passes
//! call after proving one specific instruction cannot trap.

use std::collections::{HashMap, HashSet};

use crate::ir::{Function, Operand, ValueId};

/// Delete pure instructions and phis whose results are not reachable from an
/// observable root. Returns whether anything was removed.
pub fn eliminate_dead_instructions(function: &mut Function) -> bool {
    let mut live: HashSet<ValueId> = HashSet::new();
    let mut worklist: Vec<ValueId> = Vec::new();
    let mut definitions: HashMap<ValueId, Vec<Operand>> = HashMap::new();

    // Collect definitions and mark the roots in one pass over the function.
    for (_, block) in function.live_blocks() {
        for phi in &block.phis {
            let mut operands = Vec::new();
            phi.for_each_use(&mut |operand| operands.push(*operand));
            definitions.insert(phi.result, operands);
        }
        for inst in &block.insts {
            let mut operands = Vec::new();
            inst.for_each_use(&mut |operand| operands.push(*operand));
            let stays = !inst.effects().is_pure() || inst.result.is_none();
            if stays {
                for operand in &operands {
                    mark(*operand, &mut live, &mut worklist);
                }
            }
            if let Some(result) = inst.result {
                definitions.insert(result, operands);
            }
        }
        block.terminator.for_each_use(&mut |operand| {
            mark(*operand, &mut live, &mut worklist);
        });
    }

    // Mark the operands of every definition reached from a root, including the
    // ones reachable only through other marked values.
    while let Some(value) = worklist.pop() {
        let Some(operands) = definitions.get(&value) else {
            continue;
        };
        for operand in operands {
            mark(*operand, &mut live, &mut worklist);
        }
    }

    let mut removed = false;
    for (_, block) in function.live_blocks_mut() {
        block.insts.retain(|inst| {
            let Some(result) = inst.result else {
                return true;
            };
            if inst.effects().is_pure() && !live.contains(&result) {
                removed = true;
                false
            } else {
                true
            }
        });
        block.phis.retain(|phi| {
            if live.contains(&phi.result) {
                true
            } else {
                removed = true;
                false
            }
        });
    }
    removed
}

/// Remove the given instruction definitions once their results are unread.
///
/// The caller has already rewritten every use through its replacement map, and
/// only passes results whose instruction is side-effect free and was proven
/// not to trap (a modular fold, or an identity such as `x / 1` or a shift by
/// zero). An unproven trap-capable instruction must never be passed here; it
/// remains the job of `eliminate_dead_instructions` to keep it.
///
/// Removal repeats because deleting one candidate can leave another candidate
/// without readers, for example a chain `a = b + 0; b = c + 0`.
pub(crate) fn remove_unused_definitions(
    function: &mut Function,
    candidates: &HashSet<ValueId>,
) -> bool {
    let mut changed = false;
    loop {
        let mut used: HashSet<ValueId> = HashSet::new();
        function.for_each_use(&mut |value| {
            used.insert(value);
        });
        let mut removed = false;
        for (_, block) in function.live_blocks_mut() {
            block.insts.retain(|inst| {
                let Some(result) = inst.result else {
                    return true;
                };
                if candidates.contains(&result) && !used.contains(&result) {
                    removed = true;
                    false
                } else {
                    true
                }
            });
        }
        if !removed {
            return changed;
        }
        changed = true;
    }
}

/// Mark a value operand as live and queue it for definition expansion.
fn mark(operand: Operand, live: &mut HashSet<ValueId>, worklist: &mut Vec<ValueId>) {
    let Operand::Value(value) = operand else {
        return;
    };
    if live.insert(value) {
        worklist.push(value);
    }
}
