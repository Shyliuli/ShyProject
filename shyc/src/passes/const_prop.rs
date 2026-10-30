//! Constant folding, algebraic identities, and replacement fan-out.
//!
//! `fold_constants` evaluates an instruction when every operand it reads is
//! already a constant. `propagate_copies` replaces operations that are the
//! identity for modular integers. Both rewrite every operand use in the
//! function - instruction operands, phi inputs, terminators, address bases,
//! indirect-call targets, and inline-assembly addresses - and then delete the
//! defining instruction once its result is unused. The proof is about that
//! exact instruction, so a division/shift folded after its defined-behavior
//! check disappears even though the generic effect table conservatively says
//! "may trap". Observable instructions are never candidates; any unproven
//! trap-capable operation stays for DCE.
//!
//! Folding is visible within one pass through a value-to-constant map, so an
//! earlier fold in layout order can feed a later instruction. Longer chains
//! are picked up by the next round of `simplify`.
//!
//! # Integer folding
//!
//! Constants are bit patterns masked to their IR type, and integer
//! `add`/`sub`/`mul` wrap modulo the type width, so wrapping host arithmetic
//! plus a mask is exact for `i1` through `i64`; no host `i32`/`usize` width
//! leaks in. Trapping cases are never folded:
//!
//! - `udiv`/`urem` and `sdiv`/`srem` with a zero divisor;
//! - `sdiv`/`srem` with the type's signed minimum and `-1` (the overflow
//!   trap, including `i1`, whose only nonzero pattern is signed `-1`);
//! - shifts whose amount is not in `0 .. width`. The IR does not mask shift
//!   amounts, so an out-of-range constant stays a runtime trap. The amount is
//!   read with its own type, the left operand with the result type.
//!
//! `sdiv` truncates toward zero and `srem` keeps the dividend's sign, matching
//! `checked_div`/`checked_rem` on `i64` once the per-width overflow case above
//! is excluded.
//!
//! # Floats and pointers
//!
//! No host floating-point evaluation happens: float arithmetic, `fcmp`,
//! `fneg`, float-width casts, and `fptosi`/`fptoui` are untouched. The only
//! float-adjacent rewrite is `bitcast` between equal-size scalars
//! (`i32`/`f32`, `i64`/`f64`), which copies exact bits. `ptrtoint`/`inttoptr`
//! fold as width-defined truncation/zero extension of the 32-bit address;
//! `icmp` on `ptr` folds only `eq`/`ne`. Address arithmetic is not folded
//! beyond `ptr_offset(base, 0)`.
//!
//! # Identities
//!
//! Identities valid for modular integers without poison and without traps:
//! `x + 0`, `0 + x`, `x - 0`, `x - x`, `x * 1`, `1 * x`,
//! `x * 0`, `x & 0`, `x & -1`, `x & x`, `x | 0`, `x | -1`, `x | x`, `x ^ 0`,
//! `x ^ x`, shifts by a constant zero, `ptr_offset(base, 0)`, redundant
//! `bitcast`/`zext`/`sext` to the same type, and `icmp p x, x` for any
//! predicate over an integer or pointer. Division/remainder identities
//! (`x / 1`, `x % 1`) are used only for unsigned operations or signed ones
//! wider than one bit, because an `i1` divisor `1` is the signed value `-1`.
//!
//! `x - x` and `x ^ x` are integer-only; pointers use `ptr_offset`.

use std::collections::{HashMap, HashSet};

use super::dce::remove_unused_definitions;
use crate::ir::{
    BinaryOp, CastOp, Constant, Function, InstKind, IntPredicate, Layout, Operand, Type, UnaryOp,
    ValueId,
};

// Public passes

/// Replace instructions whose operands are all constants with those
/// constants, then fan the replacements out to every use.
///
/// A folded instruction is also deleted once its result is unused, because the
/// fold is a proof about that exact instruction: modular arithmetic cannot
/// trap, and a division or shift is only folded after its defined-behavior
/// conditions are checked. That includes an instruction whose result was
/// already unused, so `udiv 4, 2` does not linger. Unknown trap-capable
/// operations are never candidates; `eliminate_dead_instructions` keeps them.
pub fn fold_constants(function: &mut Function) -> bool {
    let mut replacements: HashMap<ValueId, Operand> = HashMap::new();
    let mut proven: HashSet<ValueId> = HashSet::new();
    for (_, block) in function.live_blocks() {
        for inst in &block.insts {
            let Some(result) = inst.result else { continue };
            // Observability wins over any value proof.
            if inst.effects().side_effects {
                continue;
            }
            let Some(constant) = fold_instruction(&inst.kind, &replacements, function.layout) else {
                continue;
            };
            replacements.insert(result, Operand::Constant(constant));
            proven.insert(result);
        }
    }
    finish_replacements(function, &replacements, &proven)
}

/// Replace operations that are provably an identity by one of their operands,
/// then fan the replacements out to every use.
///
/// This is the IR's form of copy propagation: the IR has no copy instruction,
/// so value-to-value replacements come from identities such as `x + 0`,
/// `x * 1`, `x & -1`, a shift by zero, or `ptr_offset(base, 0)`. Trivial phis
/// are handled by `cfg_simp::simplify_trivial_phis` because they are part of
/// the CFG.
///
/// The identity instruction is deleted once its result is unused. The identity
/// is only used when it cannot trap (`x / 1`, `x % 1`, a shift by zero), so
/// deleting it is sound; a division/shift that does not match an identity is
/// left for `eliminate_dead_instructions`, which keeps it.
pub fn propagate_copies(function: &mut Function) -> bool {
    let mut replacements: HashMap<ValueId, Operand> = HashMap::new();
    let mut proven: HashSet<ValueId> = HashSet::new();
    let source: &Function = function;
    for (_, block) in source.live_blocks() {
        for inst in &block.insts {
            let Some(result) = inst.result else { continue };
            if inst.effects().side_effects {
                continue;
            }
            let Some(operand) = identity_operand(&inst.kind, source) else {
                continue;
            };
            if operand != Operand::Value(result) {
                replacements.insert(result, operand);
                proven.insert(result);
            }
        }
    }
    finish_replacements(function, &replacements, &proven)
}

// Replacement application

/// Resolve replacement chains and rewrite every operand use in the function.
///
/// Used by the CFG rewrite for trivial phis, which leaves the dead phi for
/// `eliminate_dead_instructions` instead of deleting a definition here.
pub(crate) fn apply_replacements(
    function: &mut Function,
    raw: &HashMap<ValueId, Operand>,
) -> bool {
    let replacements = resolve_replacements(raw);
    rewrite_uses(function, &replacements)
}

/// Apply a fold/identity proof: rewrite uses, then delete the proven
/// definitions whose results became unused.
///
/// A chain that resolution dropped (self or cyclic) did not get its uses
/// rewritten, so that definition is not removable. Instructions with
/// observable effects were never candidates, and an unknown trap-capable
/// instruction is still left for `eliminate_dead_instructions`.
fn finish_replacements(
    function: &mut Function,
    raw: &HashMap<ValueId, Operand>,
    proven: &HashSet<ValueId>,
) -> bool {
    let replacements = resolve_replacements(raw);
    let mut changed = rewrite_uses(function, &replacements);
    let removable: HashSet<ValueId> = proven
        .iter()
        .copied()
        .filter(|value| replacements.contains_key(value))
        .collect();
    changed |= remove_unused_definitions(function, &removable);
    changed
}

/// Rewrite every operand use through an already resolved map.
fn rewrite_uses(function: &mut Function, replacements: &HashMap<ValueId, Operand>) -> bool {
    if replacements.is_empty() {
        return false;
    }
    let mut changed = false;
    for (_, block) in function.live_blocks_mut() {
        block.for_each_use_mut(&mut |operand| {
            let Operand::Value(value) = *operand else {
                return;
            };
            let Some(replacement) = replacements.get(&value) else {
                return;
            };
            if *operand != *replacement {
                *operand = *replacement;
                changed = true;
            }
        });
    }
    changed
}

/// Follow `a -> b -> c` to its final operand, dropping entries whose chain
/// loops. The output map never points at one of its own keys, so applying it
/// is order-independent.
fn resolve_replacements(raw: &HashMap<ValueId, Operand>) -> HashMap<ValueId, Operand> {
    let mut resolved = HashMap::with_capacity(raw.len());
    for start in raw.keys().copied() {
        let Some(first) = raw.get(&start).copied() else {
            continue;
        };
        let mut current = first;
        let mut seen: HashSet<ValueId> = HashSet::new();
        let mut cyclic = false;
        while let Operand::Value(target) = current {
            if !seen.insert(target) {
                cyclic = true;
                break;
            }
            match raw.get(&target).copied() {
                Some(next) => current = next,
                None => break,
            }
        }
        if cyclic || current == Operand::Value(start) {
            continue;
        }
        resolved.insert(start, current);
    }
    resolved
}

// Constant folding

/// Fold one instruction from constants that are already known this pass.
fn fold_instruction(
    kind: &InstKind,
    pending: &HashMap<ValueId, Operand>,
    layout: Layout,
) -> Option<Constant> {
    let constant = |operand: Operand| -> Option<Constant> {
        match operand {
            Operand::Constant(constant) => Some(constant),
            Operand::Value(value) => match pending.get(&value) {
                Some(Operand::Constant(constant)) => Some(*constant),
                _ => None,
            },
        }
    };
    match kind {
        InstKind::Binary { op, lhs, rhs, ty } => {
            fold_binary(*op, constant(*lhs)?, constant(*rhs)?, *ty)
        }
        InstKind::Unary { op, value, ty } => fold_unary(*op, constant(*value)?, *ty),
        InstKind::ICmp { pred, lhs, rhs, ty } => {
            fold_icmp(*pred, constant(*lhs)?, constant(*rhs)?, *ty)
        }
        InstKind::Cast { op, value, to } => fold_cast(*op, constant(*value)?, *to, layout),
        _ => None,
    }
}

fn fold_binary(op: BinaryOp, lhs: Constant, rhs: Constant, ty: Type) -> Option<Constant> {
    if op.is_float() || !ty.is_integer() {
        return None;
    }
    let width = ty.integer_bits()?;
    let a = bits_in(ty, lhs);
    let b = bits_in(ty, rhs);
    match op {
        BinaryOp::Add => Some(Constant::raw(ty, a.wrapping_add(b))),
        BinaryOp::Sub => Some(Constant::raw(ty, a.wrapping_sub(b))),
        BinaryOp::Mul => Some(Constant::raw(ty, a.wrapping_mul(b))),
        BinaryOp::UDiv => (b != 0).then(|| Constant::raw(ty, a / b)),
        BinaryOp::URem => (b != 0).then(|| Constant::raw(ty, a % b)),
        BinaryOp::SDiv | BinaryOp::SRem => {
            let signed_lhs = signed_in(ty, lhs);
            let signed_rhs = signed_in(ty, rhs);
            if signed_rhs == 0 || (signed_lhs == signed_min(ty) && signed_rhs == -1) {
                return None;
            }
            let folded = if op == BinaryOp::SDiv {
                signed_lhs.checked_div(signed_rhs)?
            } else {
                signed_lhs.checked_rem(signed_rhs)?
            };
            Some(Constant::int(ty, folded))
        }
        BinaryOp::And => Some(Constant::raw(ty, a & b)),
        BinaryOp::Or => Some(Constant::raw(ty, a | b)),
        BinaryOp::Xor => Some(Constant::raw(ty, a ^ b)),
        BinaryOp::Shl | BinaryOp::LShr | BinaryOp::AShr => {
            // The amount uses the amount operand's own type, not `ty`.
            let amount = rhs.as_u64();
            if amount >= u64::from(width) {
                return None;
            }
            let amount = amount as u32;
            match op {
                BinaryOp::Shl => Some(Constant::raw(ty, a << amount)),
                BinaryOp::LShr => Some(Constant::raw(ty, a >> amount)),
                BinaryOp::AShr => Some(Constant::int(ty, signed_in(ty, lhs) >> amount)),
                _ => None,
            }
        }
        _ => None,
    }
}

fn fold_unary(op: UnaryOp, value: Constant, ty: Type) -> Option<Constant> {
    if !ty.is_integer() {
        return None;
    }
    let bits = bits_in(ty, value);
    match op {
        UnaryOp::Neg => Some(Constant::raw(ty, bits.wrapping_neg())),
        UnaryOp::Not => Some(Constant::raw(ty, !bits)),
        UnaryOp::FNeg => None,
    }
}

fn fold_icmp(pred: IntPredicate, lhs: Constant, rhs: Constant, ty: Type) -> Option<Constant> {
    if !(ty.is_integer() || ty.is_pointer()) {
        return None;
    }
    if ty.is_pointer() && !matches!(pred, IntPredicate::Eq | IntPredicate::Ne) {
        return None;
    }
    let unsigned_lhs = bits_in(ty, lhs);
    let unsigned_rhs = bits_in(ty, rhs);
    let signed_lhs = signed_in(ty, lhs);
    let signed_rhs = signed_in(ty, rhs);
    let result = match pred {
        IntPredicate::Eq => unsigned_lhs == unsigned_rhs,
        IntPredicate::Ne => unsigned_lhs != unsigned_rhs,
        IntPredicate::Ult => unsigned_lhs < unsigned_rhs,
        IntPredicate::Ule => unsigned_lhs <= unsigned_rhs,
        IntPredicate::Ugt => unsigned_lhs > unsigned_rhs,
        IntPredicate::Uge => unsigned_lhs >= unsigned_rhs,
        IntPredicate::Slt => signed_lhs < signed_rhs,
        IntPredicate::Sle => signed_lhs <= signed_rhs,
        IntPredicate::Sgt => signed_lhs > signed_rhs,
        IntPredicate::Sge => signed_lhs >= signed_rhs,
    };
    Some(Constant::raw(Type::I1, result as u64))
}

fn fold_cast(op: CastOp, value: Constant, to: Type, layout: Layout) -> Option<Constant> {
    let from = value.ty;
    match op {
        CastOp::ZExt => {
            let from_width = from.integer_bits()?;
            let to_width = to.integer_bits()?;
            (to_width >= from_width).then(|| Constant::raw(to, value.as_u64()))
        }
        CastOp::SExt => {
            let from_width = from.integer_bits()?;
            let to_width = to.integer_bits()?;
            (to_width >= from_width).then(|| Constant::int(to, signed_in(from, value)))
        }
        CastOp::Trunc => {
            let from_width = from.integer_bits()?;
            let to_width = to.integer_bits()?;
            (to_width <= from_width).then(|| Constant::raw(to, value.bits))
        }
        CastOp::Bitcast => {
            if !bitcast_compatible(from, to, layout) {
                return None;
            }
            Some(Constant::raw(to, value.bits))
        }
        CastOp::PtrToInt => {
            to.integer_bits()?;
            if !from.is_pointer() {
                return None;
            }
            Some(Constant::raw(to, value.bits))
        }
        CastOp::IntToPtr => {
            if !from.is_integer() || !to.is_pointer() {
                return None;
            }
            Some(Constant::raw(to, value.bits))
        }
        CastOp::FPToSI
        | CastOp::FPToUI
        | CastOp::SIToFP
        | CastOp::UIToFP
        | CastOp::FPTrunc
        | CastOp::FPExt => None,
    }
}

// Identity recognition

/// Return the operand that provably has the same value as the instruction's
/// result, if there is one.
fn identity_operand(kind: &InstKind, function: &Function) -> Option<Operand> {
    match kind {
        InstKind::Binary { op, lhs, rhs, ty } if ty.is_integer() => {
            binary_identity(*op, *lhs, *rhs, *ty)
        }
        InstKind::PtrOffset { base, offset } => match offset {
            Operand::Constant(constant) if constant.as_u64() == 0 => Some(*base),
            _ => None,
        },
        InstKind::Cast { op, value, to } => {
            cast_identity(*op, *value, *to, function.operand_type(*value))
        }
        InstKind::ICmp { pred, lhs, rhs, ty }
            if (ty.is_integer() || ty.is_pointer()) && lhs == rhs =>
        {
            Some(Operand::Constant(Constant::raw(
                Type::I1,
                predicate_is_true(*pred) as u64,
            )))
        }
        _ => None,
    }
}

fn binary_identity(op: BinaryOp, lhs: Operand, rhs: Operand, ty: Type) -> Option<Operand> {
    let width = ty.integer_bits()?;
    let zero = Constant::zero(ty);
    let one = Constant::int(ty, 1);
    let all_ones = Constant::raw(ty, u64::MAX);
    // Normalize the constant to `ty` before comparing; operands of a
    // well-formed non-shift operation have that type already.
    let is = |operand: Operand, wanted: Constant| match operand {
        Operand::Constant(constant) => Constant::raw(ty, constant.bits) == wanted,
        Operand::Value(_) => false,
    };
    // Shift amounts have their own type, so read zero without truncating to
    // `ty`; an i64 amount of 2^32 must not become an i32 zero.
    let shift_amount_is_zero = matches!(rhs, Operand::Constant(constant) if constant.as_u64() == 0);
    let lhs_zero = is(lhs, zero);
    let rhs_zero = is(rhs, zero);
    let lhs_one = is(lhs, one);
    let rhs_one = is(rhs, one);
    let lhs_all_ones = is(lhs, all_ones);
    let rhs_all_ones = is(rhs, all_ones);
    match op {
        BinaryOp::Add => {
            if lhs_zero {
                Some(rhs)
            } else if rhs_zero {
                Some(lhs)
            } else {
                None
            }
        }
        BinaryOp::Sub => {
            if rhs_zero {
                Some(lhs)
            } else if lhs == rhs {
                Some(Operand::Constant(zero))
            } else {
                None
            }
        }
        BinaryOp::Mul => {
            if lhs_one {
                Some(rhs)
            } else if rhs_one {
                Some(lhs)
            } else if lhs_zero || rhs_zero {
                Some(Operand::Constant(zero))
            } else {
                None
            }
        }
        BinaryOp::UDiv => {
            if rhs_one {
                Some(lhs)
            } else {
                None
            }
        }
        // An i1 divisor constant `1` is the signed value -1, so signed
        // division/remainder identities need more than one bit.
        BinaryOp::SDiv => {
            if width > 1 && rhs_one {
                Some(lhs)
            } else {
                None
            }
        }
        BinaryOp::URem => {
            if rhs_one {
                Some(Operand::Constant(zero))
            } else {
                None
            }
        }
        BinaryOp::SRem => {
            if width > 1 && rhs_one {
                Some(Operand::Constant(zero))
            } else {
                None
            }
        }
        BinaryOp::And => {
            if lhs_zero || rhs_zero {
                Some(Operand::Constant(zero))
            } else if lhs_all_ones {
                Some(rhs)
            } else if rhs_all_ones || lhs == rhs {
                Some(lhs)
            } else {
                None
            }
        }
        BinaryOp::Or => {
            if lhs_all_ones || rhs_all_ones {
                Some(Operand::Constant(all_ones))
            } else if lhs_zero {
                Some(rhs)
            } else if rhs_zero || lhs == rhs {
                Some(lhs)
            } else {
                None
            }
        }
        BinaryOp::Xor => {
            if lhs_zero {
                Some(rhs)
            } else if rhs_zero {
                Some(lhs)
            } else if lhs == rhs {
                Some(Operand::Constant(zero))
            } else {
                None
            }
        }
        BinaryOp::Shl | BinaryOp::LShr | BinaryOp::AShr => {
            if shift_amount_is_zero {
                Some(lhs)
            } else {
                None
            }
        }
        _ => None,
    }
}

fn cast_identity(op: CastOp, value: Operand, to: Type, from: Type) -> Option<Operand> {
    let identity = from == to
        && from.is_scalar()
        && matches!(op, CastOp::Bitcast | CastOp::ZExt | CastOp::SExt);
    if identity {
        Some(value)
    } else {
        None
    }
}

fn predicate_is_true(pred: IntPredicate) -> bool {
    match pred {
        IntPredicate::Eq
        | IntPredicate::Ule
        | IntPredicate::Uge
        | IntPredicate::Sle
        | IntPredicate::Sge => true,
        IntPredicate::Ne
        | IntPredicate::Ult
        | IntPredicate::Ugt
        | IntPredicate::Slt
        | IntPredicate::Sgt => false,
    }
}

// Small helpers

/// Re-interpret a constant's bits as `ty` (masked) and read them unsigned.
fn bits_in(ty: Type, constant: Constant) -> u64 {
    Constant::raw(ty, constant.bits).as_u64()
}

/// Re-interpret a constant's bits as `ty` and read them signed.
fn signed_in(ty: Type, constant: Constant) -> i64 {
    Constant::raw(ty, constant.bits).as_i64()
}

/// Smallest signed value representable in `ty`.
fn signed_min(ty: Type) -> i64 {
    match ty.integer_bits() {
        Some(64) => i64::MIN,
        Some(width) => -(1i64 << (width - 1)),
        None => 0,
    }
}

/// A `bitcast` keeps the exact bits, so both sides must be scalars of the
/// same storage size. Integer-to-integer bitcasts additionally need the same
/// bit width: `i1` and `i8` both take one byte in memory, but a bitcast
/// between them would change the meaningful bits.
fn bitcast_compatible(from: Type, to: Type, layout: Layout) -> bool {
    if !from.is_scalar() || !to.is_scalar() {
        return false;
    }
    if from.size(layout) != to.size(layout) {
        return false;
    }
    if from.is_integer() && to.is_integer() && from.integer_bits() != to.integer_bits() {
        return false;
    }
    true
}
