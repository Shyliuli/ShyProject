//! Standalone scalar optimization passes for the typed IR.
//!
//! This module is a bag of ordinary functions, not a pass manager. `simplify`
//! is the only scheduling logic; every other public function is one pass and
//! can be called, inspected, or taught on its own. All of them return `true`
//! when they changed the function.
//!
//! # One round
//!
//! 1. `fold_constants` - evaluate instructions whose operands are constants,
//!    then delete the proven definition once its result is unused.
//! 2. `propagate_copies` - replace identity operations by one of their
//!    operands, with the same deletion rule.
//! 3. `simplify_branches` - turn constant or duplicated branch/switch edges
//!    into jumps, then prune phi inputs whose predecessor edge disappeared.
//! 4. `remove_unreachable_blocks` - delete blocks the entry cannot reach;
//!    `Function::remove_block` also drops their phi inputs.
//! 5. `simplify_trivial_phis` - a phi whose non-self inputs are all equal is
//!    that value.
//! 6. `eliminate_dead_instructions` - mark values reachable from terminator
//!    and staying-instruction operands, then sweep unmarked pure instructions
//!    and phis.
//! 7. `remove_forwarding_blocks` - bypass an empty block that only jumps,
//!    rewriting predecessor edges and phi inputs when that is sound.
//!
//! Rounds repeat until nothing changes or `MAX_ROUNDS` is exhausted. The
//! bound is a teaching/safety device, not a convergence claim: a long
//! fold/replace chain simply keeps some opportunity for a later scheduling
//! level.
//!
//! # Non-goals
//!
//! No pass manager, no analyses cached between passes, no instruction
//! insertion, no value or block renumbering, no memory optimization
//! (load/store forwarding, mem2reg), no float algebra, no interprocedural
//! work. Slot promotion is the separate `mem2reg` pass in this module; it is
//! not scheduled by `simplify`, which only cleans up existing scalar SSA code.
//! Side effects, atomics, volatile accesses, calls, and inline assembly are
//! never removed or reordered. An instruction is deleted only when it is pure
//! (`effects.is_pure()`), or when a fold/identity proves that specific
//! side-effect-free instruction cannot trap. No instruction is hoisted or
//! duplicated, and DCE keeps every unproven trap-capable operation.
//!
//! # Assumptions
//!
//! The passes expect well-formed IR: operands dominate their uses, a phi has
//! at most one input per predecessor, and phi inputs match the result type.
//! Malformed IR is not repaired. Where an update would be unsound (for
//! example, two predecessor edges that would need different values for one
//! phi), the rewrite is skipped instead.

/// Promote scalar stack slots to SSA values. A separate pass with its own
/// analysis, exposed here but deliberately not scheduled by `simplify`: the
/// scalar cleanup below never changes the slot memory model, and a later O1
/// schedule can run `mem2reg::promote` before `simplify`.
pub mod mem2reg;

mod cfg_simp;
mod const_prop;
mod dce;

pub use cfg_simp::{
    prune_stale_phi_inputs, remove_forwarding_blocks, remove_unreachable_blocks, simplify_branches,
    simplify_trivial_phis,
};
pub use const_prop::{fold_constants, propagate_copies};
pub use dce::eliminate_dead_instructions;

use crate::ir::Function;

/// Upper bound on full simplification rounds run by `simplify`.
///
/// Eight rounds cover the short constant/copy chains the frontend emits while
/// keeping the driver obviously bounded. It is deliberately not a claim that
/// the function is at a global fixed point.
pub const MAX_ROUNDS: usize = 8;

/// Run the scalar `-O1` building blocks on one function until a bounded fixed
/// point is reached.
///
/// Each round runs constant/copy propagation, CFG simplification, dead pure
/// instruction elimination, and empty-block forwarding. Returns `true` if any
/// round changed the function. Value and block IDs stay stable, and the entry
/// block plus every reachable loop are preserved.
pub fn simplify(function: &mut Function) -> bool {
    let mut changed = false;
    for _ in 0..MAX_ROUNDS {
        let mut round = false;
        round |= fold_constants(function);
        round |= propagate_copies(function);
        round |= simplify_branches(function);
        round |= remove_unreachable_blocks(function);
        round |= simplify_trivial_phis(function);
        round |= eliminate_dead_instructions(function);
        round |= remove_forwarding_blocks(function);
        changed |= round;
        if !round {
            break;
        }
    }
    changed
}
