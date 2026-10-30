//! Typed virtual-register IR for ShyC.
//!
//! # Shape of the IR
//!
//! A [`Function`] is a control-flow graph of [`Block`]s. Scalar values are
//! SSA-style [`ValueId`]s, so a normal instruction reads operands and produces
//! at most one result. Locals that have an address live in stack [`Slot`]s;
//! reading and writing them is always an explicit [`InstKind::Load`] or
//! [`InstKind::Store`]. There is no operand stack and no implicit evaluation
//! order: this is a three-address, virtual-register IR.
//!
//! Blocks end in a separate [`Terminator`]. Merges of scalar values use
//! explicit [`Phi`] nodes whose incoming edges name both a predecessor block
//! and an operand. A later `mem2reg` pass can promote slots to SSA values and
//! insert/replace phis; before that pass, locals simply stay in memory.
//!
//! File layout mirrors those concepts: `types` holds identifiers, types,
//! operands, addresses, slots, and signatures; `ops` holds operations and
//! instructions; `cfg` holds phis and terminators; `function` holds blocks,
//! the function body, and module-level data.
//!
//! # Identifiers
//!
//! [`ValueId`], [`BlockId`], and [`SlotId`] are allocated by a [`Function`]
//! and are never renumbered or reused. Removing an instruction therefore does
//! not invalidate any ID. Removing a dead block tombstones it: its vector slot
//! becomes `None` and IDs of surviving blocks are unchanged.
//!
//! # Types and operations
//!
//! [`Type`] has fixed target widths (ShyC is an ILP32-style target with 32-bit
//! pointers and 64-bit `long`); host `usize`/ABI widths never leak into the
//! IR. Signedness is not part of a type. It is explicit in the operation:
//! [`BinaryOp::SDiv`] versus [`BinaryOp::UDiv`], signed versus unsigned
//! [`IntPredicate`]s, and [`CastOp::SExt`] versus [`CastOp::ZExt`]. Integer
//! `add`/`sub`/`mul` wrap modulo the width of their type. The IR has no
//! poison, `undef`, `nsw`, or fast-math flags.
//!
//! # Aggregates (deliberate simplification)
//!
//! [`Type::Aggregate`] is an opaque `(size, align)` memory blob. The frontend
//! owns the C field layout; the IR never reconstructs fields. Aggregate
//! values are usable as function parameters, call arguments/results, and
//! whole-blob `load`/`store`/`return` values. They are **not** usable by
//! arithmetic, comparisons, casts, or `phi`. Field access must go through a
//! slot plus [`InstKind::PtrOffset`] and scalar loads/stores. This is enough
//! for by-value aggregate ABI lowering (native bytes on the stack) but it is
//! not a claim of full field-wise aggregate SSA support.
//!
//! # ABI independence
//!
//! The IR does not encode argument-slot limits, does not split 64-bit scalars,
//! and does not forbid by-value aggregates. Signatures and calls carry source
//! types only. Register allocation, stack argument slots, the return-slot
//! convention, and `sret` are decisions for the ABI/codegen stage.
//!
//! # Effects
//!
//! [`Effects`] is deliberately conservative. Calls, inline assembly, and
//! atomics have observable side effects. Ordinary loads may trap. Division,
//! remainder, and shifts may trap. Default optimizations must not hoist or
//! speculate trap-capable operations, and must not delete them merely because
//! their result is unused. Inline assembly is a full barrier.
//!
//! The IR has no interpreter, code generator, or verifier. Constructing a
//! well-typed function is the frontend's responsibility; the helper methods
//! here only make the common construction paths hard to get wrong.

mod cfg;
mod display;
mod function;
mod ops;
mod types;

pub use cfg::*;
pub use function::*;
pub use ops::*;
pub use types::*;
