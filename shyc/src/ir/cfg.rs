//! Control-flow graph concepts: phi nodes, switch cases, and terminators.
//!
//! The CFG is a list of [`Block`](super::Block)s, and every block ends in
//! exactly one [`Terminator`]. Edges are *per edge*, not per successor block:
//! a `switch` may list the same target for several cases (or for `default`
//! and a case), and a `branch` may use the same block on both edges. Code
//! that reasons about block identity must therefore deduplicate edges; see
//! [`Terminator::unique_successors`].

use super::types::*;
use crate::source::Span;

// ---------------------------------------------------------------------------
// Phi nodes
// ---------------------------------------------------------------------------

/// One incoming value of a phi node.
///
/// Incoming edges are keyed by predecessor *block*. A predecessor that reaches
/// the block through several edges, such as several switch cases pointing at
/// the same target, still contributes exactly one entry. [`Phi::incoming_for`]
/// returns the value for that block.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PhiIncoming {
    pub predecessor: BlockId,
    pub value: Operand,
}

impl PhiIncoming {
    pub fn new(predecessor: BlockId, value: Operand) -> PhiIncoming {
        PhiIncoming { predecessor, value }
    }
}

/// An explicit SSA merge at the start of a block.
///
/// `ty` repeats the result type from the value table so that the phi is
/// self-describing in dumps. Aggregates are not allowed as phi types; keep
/// aggregate locals in memory instead.
///
/// A well-formed phi has at most one incoming entry per predecessor block, and
/// exactly one for every live predecessor. Since the CFG counts edges, use
/// [`Function::predecessors`](super::Function::predecessors) (which
/// deduplicates) rather than [`Terminator::successors`] when building or
/// checking phi inputs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Phi {
    pub result: ValueId,
    pub ty: Type,
    pub incoming: Vec<PhiIncoming>,
    /// Source location for a phi created from a source-level join. Generated
    /// phis use `None`.
    pub span: Option<Span>,
}

impl Phi {
    pub fn new(result: ValueId, ty: Type, incoming: Vec<PhiIncoming>) -> Phi {
        Phi {
            result,
            ty,
            incoming,
            span: None,
        }
    }

    pub fn with_span(mut self, span: Span) -> Phi {
        self.span = Some(span);
        self
    }

    pub fn incoming_for(&self, predecessor: BlockId) -> Option<&Operand> {
        self.incoming
            .iter()
            .find(|incoming| incoming.predecessor == predecessor)
            .map(|incoming| &incoming.value)
    }

    pub fn for_each_use(&self, f: &mut impl FnMut(&Operand)) {
        for incoming in &self.incoming {
            f(&incoming.value);
        }
    }

    pub fn for_each_use_mut(&mut self, f: &mut impl FnMut(&mut Operand)) {
        for incoming in &mut self.incoming {
            f(&mut incoming.value);
        }
    }
}

// ---------------------------------------------------------------------------
// Terminators
// ---------------------------------------------------------------------------

/// One case of an integer switch. Case constants are compared for equality
/// after the frontend converts them to the switch value's type.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SwitchCase {
    pub value: Constant,
    pub target: BlockId,
}

impl SwitchCase {
    pub fn new(value: Constant, target: BlockId) -> SwitchCase {
        SwitchCase { value, target }
    }
}

/// The control transfer that ends a block.
///
/// Every block has exactly one terminator, even an unreachable one. The
/// successors of a terminator are *edges*: `switch` may name one target from
/// several cases (or from `default` and a case), and `branch` may name the
/// same block twice. Phis and predecessor queries work with deduplicated
/// successor blocks, because a block is one predecessor no matter how many
/// edges reach it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Terminator {
    Jump {
        target: BlockId,
        span: Option<Span>,
    },
    Branch {
        /// Must be an `i1` value in well-formed IR.
        condition: Operand,
        then_block: BlockId,
        else_block: BlockId,
        span: Option<Span>,
    },
    Switch {
        value: Operand,
        /// Type of the switched value, repeated for readable dumps.
        ty: Type,
        default: BlockId,
        cases: Vec<SwitchCase>,
        span: Option<Span>,
    },
    Return {
        /// `None` for a `void` return.
        value: Option<Operand>,
        span: Option<Span>,
    },
    Unreachable {
        span: Option<Span>,
    },
}

impl Terminator {
    pub fn jump(target: BlockId) -> Terminator {
        Terminator::Jump { target, span: None }
    }

    pub fn branch(condition: Operand, then_block: BlockId, else_block: BlockId) -> Terminator {
        Terminator::Branch {
            condition,
            then_block,
            else_block,
            span: None,
        }
    }

    pub fn switch(
        value: Operand,
        ty: Type,
        default: BlockId,
        cases: Vec<SwitchCase>,
    ) -> Terminator {
        Terminator::Switch {
            value,
            ty,
            default,
            cases,
            span: None,
        }
    }

    pub fn ret(value: Option<Operand>) -> Terminator {
        Terminator::Return { value, span: None }
    }

    pub fn unreachable() -> Terminator {
        Terminator::Unreachable { span: None }
    }

    pub fn span(&self) -> Option<Span> {
        match self {
            Terminator::Jump { span, .. }
            | Terminator::Branch { span, .. }
            | Terminator::Switch { span, .. }
            | Terminator::Return { span, .. }
            | Terminator::Unreachable { span } => *span,
        }
    }

    pub fn set_span(&mut self, new_span: Option<Span>) {
        match self {
            Terminator::Jump { span, .. }
            | Terminator::Branch { span, .. }
            | Terminator::Switch { span, .. }
            | Terminator::Return { span, .. }
            | Terminator::Unreachable { span } => *span = new_span,
        }
    }

    pub fn with_span(mut self, span: Span) -> Terminator {
        self.set_span(Some(span));
        self
    }

    /// Successor edges in a stable textual order: branch then/else, switch
    /// default first and then cases in vector order.
    ///
    /// The result may contain the same block more than once when several
    /// cases (or `default` and a case) share a target. Use
    /// [`Terminator::unique_successors`] when the caller needs distinct blocks.
    pub fn successors(&self) -> Vec<BlockId> {
        let mut successors = Vec::new();
        self.for_each_successor(&mut |block| successors.push(block));
        successors
    }

    /// Distinct successor blocks in first-seen edge order.
    pub fn unique_successors(&self) -> Vec<BlockId> {
        let mut successors: Vec<BlockId> = Vec::new();
        self.for_each_successor(&mut |block| {
            if !successors.contains(&block) {
                successors.push(block);
            }
        });
        successors
    }

    /// Visit each successor edge, including repeated edges to one block.
    pub fn for_each_successor(&self, f: &mut impl FnMut(BlockId)) {
        match self {
            Terminator::Jump { target, .. } => f(*target),
            Terminator::Branch {
                then_block,
                else_block,
                ..
            } => {
                f(*then_block);
                f(*else_block);
            }
            Terminator::Switch { default, cases, .. } => {
                f(*default);
                for case in cases {
                    f(case.target);
                }
            }
            Terminator::Return { .. } | Terminator::Unreachable { .. } => {}
        }
    }

    /// Rewrite every successor edge from `old` to `new`. This does not touch
    /// phi inputs; a CFG-changing pass must update those separately.
    pub fn replace_successor(&mut self, old: BlockId, new: BlockId) {
        match self {
            Terminator::Jump { target, .. } => {
                if *target == old {
                    *target = new;
                }
            }
            Terminator::Branch {
                then_block,
                else_block,
                ..
            } => {
                if *then_block == old {
                    *then_block = new;
                }
                if *else_block == old {
                    *else_block = new;
                }
            }
            Terminator::Switch { default, cases, .. } => {
                if *default == old {
                    *default = new;
                }
                for case in cases {
                    if case.target == old {
                        case.target = new;
                    }
                }
            }
            Terminator::Return { .. } | Terminator::Unreachable { .. } => {}
        }
    }

    pub fn for_each_use(&self, f: &mut impl FnMut(&Operand)) {
        match self {
            Terminator::Branch { condition, .. } => f(condition),
            Terminator::Switch { value, .. } => f(value),
            Terminator::Return {
                value: Some(value), ..
            } => f(value),
            Terminator::Jump { .. }
            | Terminator::Return { value: None, .. }
            | Terminator::Unreachable { .. } => {}
        }
    }

    pub fn for_each_use_mut(&mut self, f: &mut impl FnMut(&mut Operand)) {
        match self {
            Terminator::Branch { condition, .. } => f(condition),
            Terminator::Switch { value, .. } => f(value),
            Terminator::Return {
                value: Some(value), ..
            } => f(value),
            Terminator::Jump { .. }
            | Terminator::Return { value: None, .. }
            | Terminator::Unreachable { .. } => {}
        }
    }
}
