//! Blocks, values, function bodies, and module-level data.

use super::cfg::*;
use super::ops::*;
use super::types::*;
use std::fmt;

// ---------------------------------------------------------------------------
// Blocks and functions
// ---------------------------------------------------------------------------

/// A basic block: phis, straight-line instructions, and one terminator.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Block {
    pub phis: Vec<Phi>,
    pub insts: Vec<Inst>,
    pub terminator: Terminator,
}

impl Block {
    pub fn new(terminator: Terminator) -> Block {
        Block {
            phis: Vec::new(),
            insts: Vec::new(),
            terminator,
        }
    }

    pub fn unreachable() -> Block {
        Block::new(Terminator::unreachable())
    }

    /// Successor edges in terminator order, which may repeat a block when
    /// several switch cases share a target.
    pub fn successors(&self) -> Vec<BlockId> {
        self.terminator.successors()
    }

    /// Distinct successor blocks in first-seen edge order.
    pub fn unique_successors(&self) -> Vec<BlockId> {
        self.terminator.unique_successors()
    }

    pub fn for_each_successor(&self, f: &mut impl FnMut(BlockId)) {
        self.terminator.for_each_successor(f);
    }

    pub fn replace_successor(&mut self, old: BlockId, new: BlockId) {
        self.terminator.replace_successor(old, new);
    }

    pub fn for_each_use(&self, f: &mut impl FnMut(&Operand)) {
        for phi in &self.phis {
            phi.for_each_use(&mut *f);
        }
        for inst in &self.insts {
            inst.for_each_use(&mut *f);
        }
        self.terminator.for_each_use(&mut *f);
    }

    pub fn for_each_use_mut(&mut self, f: &mut impl FnMut(&mut Operand)) {
        for phi in &mut self.phis {
            phi.for_each_use_mut(&mut *f);
        }
        for inst in &mut self.insts {
            inst.for_each_use_mut(&mut *f);
        }
        self.terminator.for_each_use_mut(&mut *f);
    }
}

/// The type of one entry in a function's value table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Value {
    pub ty: Type,
}

/// Where a value is defined. This is a query result, not stored state, so it
/// never becomes stale when instructions are removed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Definition {
    Parameter { index: u32 },
    Phi { block: BlockId },
    Instruction { block: BlockId },
}

/// One function: a typed value table, stack slots, and a CFG.
///
/// `values` is indexed by [`ValueId`], `blocks` by [`BlockId`], and `slots` by
/// [`SlotId`]. `blocks` may contain `None` after dead-block removal; values
/// and slots are never renumbered. The constructor allocates `bb0` as the
/// entry and one SSA parameter value per signature parameter.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Function {
    pub name: String,
    pub signature: Signature,
    /// Target facts this function was built for. Every size, alignment, and
    /// derived result type in the function is computed from it.
    pub layout: Layout,
    /// SSA values for the function parameters, in signature order.
    pub params: Vec<ValueId>,
    pub entry: BlockId,
    /// Type of every allocated SSA value. Entries remain after an instruction
    /// is removed; an unused type entry is harmless and keeps IDs stable.
    pub values: Vec<Value>,
    pub blocks: Vec<Option<Block>>,
    pub slots: Vec<Slot>,
    next_value: u32,
    next_block: u32,
    next_slot: u32,
}

impl Function {
    /// Create a function with an empty `bb0` entry block and SSA parameter
    /// values matching `signature.params`.
    pub fn new(name: impl Into<String>, signature: Signature, layout: Layout) -> Function {
        let param_types = signature.params.clone();
        let mut function = Function {
            name: name.into(),
            signature,
            layout,
            params: Vec::new(),
            entry: BlockId(0),
            values: Vec::new(),
            blocks: Vec::new(),
            slots: Vec::new(),
            next_value: 0,
            next_block: 0,
            next_slot: 0,
        };
        let entry = function.alloc_block();
        function.entry = entry;
        for ty in param_types {
            let param = function.alloc_value(ty);
            function.params.push(param);
        }
        function
    }

    pub fn alloc_value(&mut self, ty: Type) -> ValueId {
        let id = ValueId(self.next_value);
        self.next_value += 1;
        self.values.push(Value { ty });
        id
    }

    pub fn alloc_block(&mut self) -> BlockId {
        self.add_block(Block::unreachable())
    }

    /// Append a fully built block. Its ID is stable for the function's life.
    pub fn add_block(&mut self, block: Block) -> BlockId {
        let id = BlockId(self.next_block);
        self.next_block += 1;
        self.blocks.push(Some(block));
        id
    }

    pub fn alloc_slot(&mut self, name: impl Into<String>, ty: Type) -> SlotId {
        let align = ty.align(self.layout).unwrap_or(1);
        self.alloc_slot_aligned(name, ty, align)
    }

    pub fn alloc_slot_aligned(&mut self, name: impl Into<String>, ty: Type, align: u32) -> SlotId {
        let id = SlotId(self.next_slot);
        self.next_slot += 1;
        self.slots.push(Slot::with_align(name, ty, align));
        id
    }

    pub fn value(&self, id: ValueId) -> &Value {
        &self.values[id.0 as usize]
    }

    pub fn value_type(&self, id: ValueId) -> Type {
        self.value(id).ty
    }

    /// Static type of an operand: constants carry their type, values are
    /// looked up in the function's value table.
    pub fn operand_type(&self, operand: Operand) -> Type {
        match operand {
            Operand::Value(value) => self.value_type(value),
            Operand::Constant(constant) => constant.ty,
        }
    }

    /// Access a live block. Panics if the block was removed.
    pub fn block(&self, id: BlockId) -> &Block {
        self.blocks[id.0 as usize]
            .as_ref()
            .expect("block has been removed")
    }

    /// Mutable access to a live block. Panics if the block was removed.
    pub fn block_mut(&mut self, id: BlockId) -> &mut Block {
        self.blocks[id.0 as usize]
            .as_mut()
            .expect("block has been removed")
    }

    pub fn slot(&self, id: SlotId) -> &Slot {
        &self.slots[id.0 as usize]
    }

    pub fn slot_mut(&mut self, id: SlotId) -> &mut Slot {
        &mut self.slots[id.0 as usize]
    }

    /// Visit live blocks in ID/layout order. Removed blocks are skipped.
    pub fn live_blocks(&self) -> impl Iterator<Item = (BlockId, &Block)> + '_ {
        self.blocks
            .iter()
            .enumerate()
            .filter_map(|(index, block)| block.as_ref().map(|block| (BlockId(index as u32), block)))
    }

    pub fn live_blocks_mut(&mut self) -> impl Iterator<Item = (BlockId, &mut Block)> + '_ {
        self.blocks
            .iter_mut()
            .enumerate()
            .filter_map(|(index, block)| block.as_mut().map(|block| (BlockId(index as u32), block)))
    }

    /// Allocate a value whose type is derived from `kind`, append the
    /// instruction to `block`, and return the result value.
    ///
    /// Panics if `kind` is a statement (for example a `void` call or a store).
    pub fn push_value(&mut self, block: BlockId, kind: InstKind) -> ValueId {
        let ty = kind
            .result_type()
            .expect("instruction does not produce a value");
        let result = self.alloc_value(ty);
        self.block_mut(block).insts.push(Inst::value(result, kind));
        result
    }

    /// Append a statement instruction to `block`.
    ///
    /// Panics if `kind` produces a value; use [`Function::push_value`] for
    /// those so the type table stays consistent.
    pub fn push_statement(&mut self, block: BlockId, kind: InstKind) {
        assert!(
            kind.result_type().is_none(),
            "instruction produces a value; use push_value"
        );
        self.block_mut(block).insts.push(Inst::statement(kind));
    }

    /// Allocate a phi result and append the phi to `block`.
    pub fn push_phi(&mut self, block: BlockId, ty: Type, incoming: Vec<PhiIncoming>) -> ValueId {
        let result = self.alloc_value(ty);
        self.block_mut(block)
            .phis
            .push(Phi::new(result, ty, incoming));
        result
    }

    /// Distinct predecessor blocks in deterministic ID order. A self-loop
    /// counts as a predecessor.
    ///
    /// Successors are edges, so one terminator can reach `block` several times
    /// (switch cases sharing a target). That still yields one predecessor here,
    /// which is the key phi inputs use.
    pub fn predecessors(&self, block: BlockId) -> Vec<BlockId> {
        let mut predecessors = Vec::new();
        for (id, candidate) in self.live_blocks() {
            let mut reaches = false;
            candidate.terminator.for_each_successor(&mut |successor| {
                if successor == block {
                    reaches = true;
                }
            });
            if reaches && !predecessors.contains(&id) {
                predecessors.push(id);
            }
        }
        predecessors
    }

    /// Find where `value` is defined, scanning parameters and then live
    /// blocks in ID order. Returns `None` for an allocated but undefined
    /// value. Multiple definitions are invalid IR; the first is returned.
    pub fn def_of(&self, value: ValueId) -> Option<Definition> {
        if let Some(index) = self.params.iter().position(|&param| param == value) {
            return Some(Definition::Parameter {
                index: index as u32,
            });
        }
        for (index, block) in self.blocks.iter().enumerate() {
            let Some(block) = block else { continue };
            if block.phis.iter().any(|phi| phi.result == value) {
                return Some(Definition::Phi {
                    block: BlockId(index as u32),
                });
            }
            if block.insts.iter().any(|inst| inst.result == Some(value)) {
                return Some(Definition::Instruction {
                    block: BlockId(index as u32),
                });
            }
        }
        None
    }

    /// Visit every operand read in the function. Results, phi predecessors,
    /// and terminator successors are not operands and are not visited.
    pub fn for_each_use(&self, f: &mut impl FnMut(ValueId)) {
        for block in self.blocks.iter().flatten() {
            block.for_each_use(&mut |operand| {
                if let Operand::Value(value) = *operand {
                    f(value);
                }
            });
        }
    }

    /// Replace every use of `old` with `new` throughout the function,
    /// including operands that appear inside memory addresses and indirect
    /// call targets. The definition of `old` is not changed.
    pub fn replace_uses(&mut self, old: ValueId, new: Operand) {
        for block in self.blocks.iter_mut().flatten() {
            block.for_each_use_mut(&mut |operand| operand.replace_value(old, new));
        }
    }

    /// Tombstone a dead block and drop phi inputs that name it as a
    /// predecessor.
    ///
    /// The caller must first remove or redirect every surviving CFG edge to
    /// `block`; this method does not choose a replacement successor. IDs of
    /// surviving blocks are unchanged and the removed ID is never reused.
    pub fn remove_block(&mut self, block: BlockId) {
        for entry in self.blocks.iter_mut().flatten() {
            for phi in &mut entry.phis {
                phi.incoming
                    .retain(|incoming| incoming.predecessor != block);
            }
        }
        self.blocks[block.0 as usize] = None;
    }

    pub(super) fn fmt_indented(&self, f: &mut fmt::Formatter<'_>, indent: &str) -> fmt::Result {
        write!(f, "{indent}function @{}(", self.name)?;
        for (index, param) in self.params.iter().enumerate() {
            if index > 0 {
                f.write_str(", ")?;
            }
            write!(f, "{}: {}", param, self.value_type(*param))?;
        }
        if self.signature.variadic {
            if !self.params.is_empty() {
                f.write_str(", ")?;
            }
            f.write_str("...")?;
        }
        writeln!(f, ") -> {} {{", self.signature.ret)?;
        writeln!(f, "{indent}  entry: {}", self.entry)?;

        if !self.slots.is_empty() {
            writeln!(f, "{indent}  slots:")?;
            for (index, slot) in self.slots.iter().enumerate() {
                writeln!(
                    f,
                    "{indent}    slot {} {:?}: {} align {}",
                    SlotId(index as u32),
                    slot.name,
                    slot.ty,
                    slot.align
                )?;
            }
        }

        for (index, block) in self.blocks.iter().enumerate() {
            let Some(block) = block else { continue };
            writeln!(f, "{indent}  {}:", BlockId(index as u32))?;
            for phi in &block.phis {
                writeln!(f, "{indent}    {}", phi)?;
            }
            for inst in &block.insts {
                writeln!(f, "{indent}    {}", inst)?;
            }
            writeln!(f, "{indent}    {}", block.terminator)?;
        }

        writeln!(f, "{indent}}}")
    }
}

// ---------------------------------------------------------------------------
// Module-level data
// ---------------------------------------------------------------------------

/// A symbolic relocation inside a global's raw bytes.
///
/// `offset` is a byte offset from the start of the global. The linker patches
/// a 32-bit big-endian field at that offset with `symbol + addend`, modulo
/// 2^32. The frontend leaves zero bytes at the relocated field.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Relocation {
    pub offset: u32,
    pub symbol: String,
    pub addend: i64,
}

impl Relocation {
    pub fn new(offset: u32, symbol: impl Into<String>, addend: i64) -> Relocation {
        Relocation {
            offset,
            symbol: symbol.into(),
            addend,
        }
    }
}

/// A global memory object.
///
/// `bytes` are already in target byte order (big-endian for ShyISA). The IR
/// stores raw initialized bytes, not a typed initializer tree, so the frontend
/// owns C initializer layout and padding. Relocations describe only the
/// address fields that need link-time patching.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Global {
    pub symbol: String,
    pub align: u32,
    /// `true` for string literals and notably other immutable data.
    pub readonly: bool,
    pub bytes: Vec<u8>,
    pub relocations: Vec<Relocation>,
}

impl Global {
    pub fn new(symbol: impl Into<String>, align: u32) -> Global {
        Global {
            symbol: symbol.into(),
            align,
            readonly: false,
            bytes: Vec::new(),
            relocations: Vec::new(),
        }
    }

    pub fn with_bytes(symbol: impl Into<String>, align: u32, bytes: Vec<u8>) -> Global {
        Global {
            symbol: symbol.into(),
            align,
            readonly: false,
            bytes,
            relocations: Vec::new(),
        }
    }

    pub fn size(&self) -> u32 {
        self.bytes.len() as u32
    }

    pub fn add_relocation(&mut self, offset: u32, symbol: impl Into<String>, addend: i64) {
        self.relocations
            .push(Relocation::new(offset, symbol, addend));
    }

    pub(super) fn fmt_indented(&self, f: &mut fmt::Formatter<'_>, indent: &str) -> fmt::Result {
        write!(f, "{indent}global @{} align {}", self.symbol, self.align)?;
        if self.readonly {
            f.write_str(" readonly")?;
        }
        writeln!(f, " {{")?;
        write!(f, "{indent}  bytes:")?;
        for byte in &self.bytes {
            write!(f, " {:02x}", byte)?;
        }
        writeln!(f)?;
        for relocation in &self.relocations {
            write!(
                f,
                "{indent}  reloc {}: @{}",
                relocation.offset, relocation.symbol
            )?;
            if relocation.addend != 0 {
                write!(f, " {:+}", relocation.addend)?;
            }
            writeln!(f)?;
        }
        writeln!(f, "{indent}}}")
    }
}

/// Startup behavior requested by the translation unit.
///
/// `Auto` corresponds to ShyC's generated `_start`: initialize `sp` to
/// `stack_pointer`, call `entry`, then exit with the return value. `Manual`
/// corresponds to `#![no_main]`; the program must provide `_start`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Startup {
    Auto { entry: String, stack_pointer: u32 },
    Manual,
}

impl Default for Startup {
    fn default() -> Startup {
        Startup::Auto {
            entry: "main".to_string(),
            stack_pointer: 0x00ff_f000,
        }
    }
}

/// Resource hints from `#![mem(...)]` and `#![stack(...)]`.
///
/// `None` means the linker default applies. Values are byte sizes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Resources {
    pub memory_bytes: Option<u64>,
    pub stack_bytes: Option<u64>,
}

/// A translation unit: globals, function bodies, and startup/resource
/// metadata. Function and global order is preserved for deterministic output.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Module {
    pub name: String,
    pub globals: Vec<Global>,
    pub functions: Vec<Function>,
    pub startup: Startup,
    pub resources: Resources,
}

impl Module {
    pub fn new(name: impl Into<String>) -> Module {
        Module {
            name: name.into(),
            globals: Vec::new(),
            functions: Vec::new(),
            startup: Startup::default(),
            resources: Resources::default(),
        }
    }

    pub fn add_global(&mut self, global: Global) {
        self.globals.push(global);
    }

    pub fn add_function(&mut self, function: Function) {
        self.functions.push(function);
    }
}
