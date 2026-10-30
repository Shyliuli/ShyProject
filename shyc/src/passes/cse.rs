//! Block-local common subexpression elimination.
//!
//! Two identical pure instructions in one block compute the same value, so the
//! second one becomes a use of the first result and is deleted. The pass is
//! deliberately block-local: values can only be reused where they were
//! computed, which keeps it independent of dominator analysis.
//!
//! Instructions with side effects or that may trap are never merged. That
//! includes every load, because a store in between could change what it reads.
use std::collections::{HashMap, HashSet};

use super::super::ir::{Function, Operand, ValueId};

/// Replace repeated pure computations by the value they already produced.
pub fn eliminate(function: &mut Function) -> bool {
    let mut replacements: Vec<(ValueId, Operand)> = Vec::new();
    let mut redundant: HashSet<ValueId> = HashSet::new();

    for (_, block) in function.live_blocks() {
        let mut seen: HashMap<String, ValueId> = HashMap::new();
        for inst in &block.insts {
            let Some(result) = inst.result else {
                continue;
            };
            if !inst.effects().is_pure() {
                continue;
            }
            // The debug form names operands and types, which is exactly the
            // identity of the computation.
            let key = format!("{:?}", inst.kind);
            match seen.get(&key) {
                Some(&first) => {
                    replacements.push((result, Operand::Value(first)));
                    redundant.insert(result);
                }
                None => {
                    seen.insert(key, result);
                }
            }
        }
    }

    if replacements.is_empty() {
        return false;
    }
    for (old, new) in replacements {
        function.replace_uses(old, new);
    }
    for (_, block) in function.live_blocks_mut() {
        block.insts.retain(|inst| {
            inst.result
                .is_none_or(|result| !redundant.contains(&result))
        });
    }
    true
}
