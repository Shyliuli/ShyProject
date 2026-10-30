//! Dominance analysis on the reachable control-flow graph.
//!
//! [`Dominators`] is the textbook construction: walk the CFG from the entry to
//! get a reverse postorder, then compute immediate dominators with the simple
//! iterative algorithm from Cooper, Harvey, and Kennedy. The dominance
//! frontier follows directly from the immediate dominators.
//!
//! Only blocks reachable from the entry take part. An unreachable block has no
//! reverse postorder position, no immediate dominator, and no dominance
//! frontier, and [`Dominators::dominates`] is `false` for it. This is what
//! passes want: values and phis are only defined along paths that execute.
//!
//! Edges are deduplicated. A `switch` with several cases pointing at one block,
//! or a `branch` whose arms are the same block, is one predecessor for phi
//! construction purposes, matching [`Phi`](crate::ir::Phi).
//!
//! The entry has no virtual predecessor. If the CFG has a backedge into it,
//! the entry is still a merge point, so it is recorded in its own frontier and
//! in the frontier of the blocks that dominate its predecessors.
//!
//! Well-formed IR is assumed: every edge names a live block. An edge into a
//! removed block is not a case this analysis repairs.

use crate::ir::{BlockId, Function};

/// Immediate dominators, the dominator tree, and the dominance frontier of a
/// function's reachable blocks.
///
/// Build it with [`Dominators::compute`]. The data is kept in vectors indexed
/// by [`BlockId`], so every query except [`Dominators::dominates`] is a direct
/// lookup. IDs are stable and are never renumbered by this analysis.
#[derive(Clone, Debug)]
pub struct Dominators {
    entry: BlockId,
    /// Reachable blocks in reverse postorder; the entry comes first.
    reverse_postorder: Vec<BlockId>,
    /// Reverse postorder position of a reachable block, `u32::MAX` otherwise.
    order: Vec<u32>,
    /// Deduplicated predecessors of reachable blocks, in ascending block ID
    /// order. Unreachable predecessors are not listed.
    predecessors: Vec<Vec<BlockId>>,
    /// Immediate dominator. The entry is its own dominator in this table;
    /// unreachable blocks have `None`.
    idom: Vec<Option<BlockId>>,
    /// Dominator-tree children, in ascending block ID order.
    children: Vec<Vec<BlockId>>,
    /// Dominance frontier blocks, in ascending block ID order.
    frontier: Vec<Vec<BlockId>>,
}

impl Dominators {
    /// Compute dominance information for every block reachable from the
    /// function's entry.
    pub fn compute(function: &Function) -> Dominators {
        let count = function.blocks.len();
        let entry = function.entry;

        // Control-flow edges of live blocks, one per distinct target block.
        let mut successors: Vec<Vec<BlockId>> = vec![Vec::new(); count];
        for (index, block) in function.blocks.iter().enumerate() {
            let Some(block) = block else { continue };
            successors[index] = block.unique_successors();
        }

        // Iterative depth-first search; the postorder reversed is the RPO.
        let mut visited = vec![false; count];
        let mut postorder = Vec::new();
        let mut stack = vec![(entry, 0usize)];
        visited[entry.0 as usize] = true;
        while let Some(&(block, next_edge)) = stack.last() {
            let edges = &successors[block.0 as usize];
            if next_edge < edges.len() {
                let successor = edges[next_edge];
                stack.last_mut().expect("stack is not empty").1 = next_edge + 1;
                if !visited[successor.0 as usize] {
                    visited[successor.0 as usize] = true;
                    stack.push((successor, 0));
                }
            } else {
                postorder.push(block);
                stack.pop();
            }
        }
        postorder.reverse();
        let reverse_postorder = postorder;

        let mut order = vec![u32::MAX; count];
        for (position, block) in reverse_postorder.iter().enumerate() {
            order[block.0 as usize] = position as u32;
        }

        // Ascending ID order keeps the predecessor lists deterministic.
        let mut predecessors = vec![Vec::new(); count];
        for index in 0..count {
            if order[index] == u32::MAX {
                continue;
            }
            for &successor in &successors[index] {
                predecessors[successor.0 as usize].push(BlockId(index as u32));
            }
        }

        // Cooper-Harvey-Kennedy: start with only the entry processed and
        // repeatedly intersect the already-processed predecessors.
        let mut idom: Vec<Option<BlockId>> = vec![None; count];
        idom[entry.0 as usize] = Some(entry);
        loop {
            let mut changed = false;
            for &block in reverse_postorder.iter().skip(1) {
                let mut new_idom: Option<BlockId> = None;
                for &predecessor in &predecessors[block.0 as usize] {
                    if idom[predecessor.0 as usize].is_none() {
                        continue;
                    }
                    new_idom = Some(match new_idom {
                        None => predecessor,
                        Some(current) => intersect(&idom, &order, current, predecessor),
                    });
                }
                if let Some(new_idom) = new_idom
                    && idom[block.0 as usize] != Some(new_idom)
                {
                    idom[block.0 as usize] = Some(new_idom);
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }

        let mut children = vec![Vec::new(); count];
        for (index, parent) in idom.iter().enumerate() {
            let Some(parent) = *parent else { continue };
            let block = BlockId(index as u32);
            if parent != block {
                children[parent.0 as usize].push(block);
            }
        }

        // A block belongs to the dominance frontier of every predecessor whose
        // walk up the dominator tree stops below the block's immediate
        // dominator. Self-loops and shared switch targets are handled because
        // the walk starts at the predecessor block itself.
        let mut frontier = vec![Vec::new(); count];
        for &block in &reverse_postorder {
            let block_predecessors = &predecessors[block.0 as usize];
            if block_predecessors.len() < 2 {
                continue;
            }
            let stop = idom[block.0 as usize].expect("reachable block has an immediate dominator");
            for &predecessor in block_predecessors {
                let mut runner = predecessor;
                while runner != stop {
                    let runner_frontier = &mut frontier[runner.0 as usize];
                    if !runner_frontier.contains(&block) {
                        runner_frontier.push(block);
                    }
                    runner = idom[runner.0 as usize]
                        .expect("reachable block has an immediate dominator");
                }
            }
        }
        // A backedge into the entry is the one merge the walk above cannot
        // reach: the entry is its own immediate dominator, so the walk has no
        // lower stop there. Nothing dominates the entry either, so by the
        // frontier definition the entry belongs to the frontier of itself and
        // of every block that dominates one of its predecessors. Lowered code
        // normally has no predecessor of the entry; when it does, this keeps
        // the frontier exact.
        if !predecessors[entry.0 as usize].is_empty() {
            for &predecessor in &predecessors[entry.0 as usize] {
                let mut runner = predecessor;
                loop {
                    let blocks = &mut frontier[runner.0 as usize];
                    if !blocks.contains(&entry) {
                        blocks.push(entry);
                    }
                    if runner == entry {
                        break;
                    }
                    runner = idom[runner.0 as usize]
                        .expect("reachable block has an immediate dominator");
                }
            }
        }
        for blocks in &mut frontier {
            blocks.sort();
        }

        Dominators {
            entry,
            reverse_postorder,
            order,
            predecessors,
            idom,
            children,
            frontier,
        }
    }

    /// The function's entry block.
    pub fn entry(&self) -> BlockId {
        self.entry
    }

    /// Reachable blocks in reverse postorder, starting with the entry.
    pub fn reverse_postorder(&self) -> &[BlockId] {
        &self.reverse_postorder
    }

    /// True when `block` can be reached from the entry. Tombstoned blocks are
    /// not reachable.
    pub fn is_reachable(&self, block: BlockId) -> bool {
        self.order[block.0 as usize] != u32::MAX
    }

    /// Deduplicated predecessors of a reachable block in ascending ID order.
    ///
    /// This differs from [`Function::predecessors`], which also counts
    /// unreachable blocks and visits every live block. Phi inputs should use
    /// this list: only reachable predecessors execute.
    pub fn predecessors(&self, block: BlockId) -> &[BlockId] {
        &self.predecessors[block.0 as usize]
    }

    /// The immediate dominator of a reachable block, or `None` for the entry
    /// and for unreachable blocks.
    pub fn idom(&self, block: BlockId) -> Option<BlockId> {
        if block == self.entry {
            return None;
        }
        self.idom[block.0 as usize]
    }

    /// Dominator-tree children of a reachable block, in ascending ID order.
    pub fn children(&self, block: BlockId) -> &[BlockId] {
        &self.children[block.0 as usize]
    }

    /// True when every path from the entry to `block` passes through
    /// `dominator`. A reachable block dominates itself. Unreachable blocks are
    /// not dominated by anything.
    pub fn dominates(&self, dominator: BlockId, block: BlockId) -> bool {
        if !self.is_reachable(dominator) || !self.is_reachable(block) {
            return false;
        }
        let mut current = block;
        loop {
            if current == dominator {
                return true;
            }
            match self.idom(current) {
                Some(parent) => current = parent,
                None => return false,
            }
        }
    }

    /// Dominance frontier of a reachable block, in ascending ID order.
    ///
    /// These are the blocks where a value defined in `block` reaches a merge
    /// that is not dominated by it, so a phi may be needed there. Blocks with
    /// fewer than two predecessors are never in a frontier, except that an
    /// entry with a backedge is listed in its own frontier and in those of the
    /// blocks dominating its predecessors.
    pub fn frontier(&self, block: BlockId) -> &[BlockId] {
        &self.frontier[block.0 as usize]
    }
}

/// Walk two blocks up the dominator tree until they meet, using reverse
/// postorder positions as the depth measure (a dominator always has a smaller
/// position than the block it dominates).
fn intersect(
    idom: &[Option<BlockId>],
    order: &[u32],
    mut left: BlockId,
    mut right: BlockId,
) -> BlockId {
    while left != right {
        while order[left.0 as usize] > order[right.0 as usize] {
            left = idom[left.0 as usize].expect("processed block has an immediate dominator");
        }
        while order[right.0 as usize] > order[left.0 as usize] {
            right = idom[right.0 as usize].expect("processed block has an immediate dominator");
        }
    }
    left
}
