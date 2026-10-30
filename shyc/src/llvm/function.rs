//! Function definitions: signature, blocks, slots, phis, and operand spelling.
//!
//! One IR value becomes one named LLVM value (`%v<id>`); one IR block becomes
//! one label (`bb<id>`); stack slots become `alloca` instructions in the entry
//! block. Address temporaries are the only extra values emitted, and they are
//! always defined directly before the instruction that needs them.

use super::Options;
use super::types::*;
use crate::ir::*;
use crate::source::Diagnostic;

mod inst;

pub(super) fn emit(
    w: &mut Writer,
    function: &Function,
    options: &Options,
) -> Result<(), Diagnostic> {
    Body {
        w,
        function,
        options,
        abi: super::abi::kind(&options.triple),
        next_temp: 0,
        split_entry: false,
    }
    .emit()
}

struct Body<'a> {
    w: &'a mut Writer,
    function: &'a Function,
    options: &'a Options,
    abi: super::abi::Kind,
    next_temp: u32,
    /// The IR entry block has predecessors, so a fresh LLVM entry block is
    /// emitted in front of it to hold the slots.
    split_entry: bool,
}

impl Body<'_> {
    fn emit(mut self) -> Result<(), Diagnostic> {
        let function = self.function;
        if function.live_blocks().next().is_none() {
            return Err(error(
                None,
                format!("function `{}` has no live basic blocks", function.name),
            ));
        }
        let entry = function
            .blocks
            .get(function.entry.0 as usize)
            .and_then(|block| block.as_ref())
            .ok_or_else(|| {
                error(
                    None,
                    format!(
                        "function `{}`: the entry block has been removed",
                        function.name
                    ),
                )
            })?;

        // LLVM requires the entry block to have no predecessors. Split off a
        // fresh prologue when the IR entry is also a branch target, as long as
        // it has no phis (a phi there would need an incoming edge that the IR
        // does not describe).
        self.split_entry = !function.predecessors(function.entry).is_empty();
        if self.split_entry && !entry.phis.is_empty() {
            return Err(error(
                None,
                format!(
                    "function `{}`: the entry block has predecessors and phi nodes; \
                     LLVM requires an entry block without predecessors",
                    function.name
                ),
            ));
        }

        let layout = self.options.layout;
        let ret = function.signature.ret;
        let sret = self.abi.by_reference(ret, layout);
        let ret_chunks = self.abi.chunks(ret, layout);
        let ret_arm = self.abi.arm_small(ret, layout);
        let ret_text = match (&ret_chunks, sret) {
            (_, _) if ret_arm.is_some() => ret_arm
                .map(|arm| arm.llvm_type(ret.size(layout).unwrap_or(0)))
                .unwrap_or_default(),
            (_, true) => "void".to_string(),
            (Some(chunks), false) if chunks.len() > 1 => format!(
                "{{ {} }}",
                chunks
                    .iter()
                    .map(|chunk| chunk.llvm_type())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            (Some(chunks), false) => chunks[0].llvm_type(),
            _ => ty(ret),
        };
        let mut signature = format!("define {ret_text} {}(", symbol(&function.name));
        let mut first = true;
        if sret {
            signature.push_str(&format!(
                "ptr sret({}) {} %sret",
                super::abi::array(ret, layout),
                super::abi::alignment(ret, layout)
            ));
            first = false;
        }
        for (index, value) in function.params.iter().enumerate() {
            if !first {
                signature.push_str(", ");
            }
            first = false;
            let param_ty = function
                .signature
                .params
                .get(index)
                .copied()
                .unwrap_or_else(|| function.value_type(*value));
            if let Some(arm) = self.abi.arm_small(param_ty, layout) {
                signature.push_str(&format!(
                    "{} {}.0",
                    arm.llvm_type(param_ty.size(layout).unwrap_or(0)),
                    self.value(*value)
                ));
                continue;
            }
            if let Some(chunks) = self.abi.chunks(param_ty, layout) {
                for (chunk_index, chunk) in chunks.iter().enumerate() {
                    if chunk_index > 0 {
                        signature.push_str(", ");
                    }
                    signature.push_str(&format!(
                        "{} {}.{}",
                        chunk.llvm_type(),
                        self.value(*value),
                        chunk_index
                    ));
                }
                continue;
            }
            if self.abi.by_reference(param_ty, layout) {
                if self.abi.copies() {
                    signature.push_str(&format!(
                        "ptr byval({}) {} %arg{}",
                        super::abi::array(param_ty, layout),
                        super::abi::alignment(param_ty, layout),
                        value.0
                    ));
                } else {
                    signature.push_str(&format!("ptr %arg{}", value.0));
                }
            } else {
                signature.push_str(&format!("{} {}", ty(param_ty), self.value(*value)));
            }
        }
        if function.signature.variadic {
            if !function.params.is_empty() {
                signature.push_str(", ");
            }
            signature.push_str("...");
        }
        signature.push_str(") {");
        self.w.line(signature);
        self.w.indent();

        if self.split_entry {
            self.w.line("shyc.prologue:");
            self.w.indent();
            self.emit_slots()?;
            self.emit_memory_params();
            self.w
                .line(format!("br label {}", self.label(function.entry)));
            self.w.dedent();
        }

        // The IR entry block is what starts execution, so it must be the first
        // block of the definition (LLVM reads the first block as the entry).
        let blocks: Vec<(BlockId, &Block)> = function.live_blocks().collect();
        let entry_position = blocks
            .iter()
            .position(|(id, _)| *id == function.entry)
            .expect("live entry block");
        let (entry_id, entry_block) = blocks[entry_position];
        self.emit_block(entry_id, entry_block)?;
        for (index, (id, block)) in blocks.iter().enumerate() {
            if index != entry_position {
                self.emit_block(*id, block)?;
            }
        }

        self.w.dedent();
        self.w.line("}");
        Ok(())
    }

    fn emit_block(&mut self, id: BlockId, block: &Block) -> Result<(), Diagnostic> {
        self.w.line(format!("{}:", self.label_name(id)));
        self.w.indent();
        for phi in &block.phis {
            self.emit_phi(phi)?;
        }
        if id == self.function.entry && !self.split_entry {
            self.emit_slots()?;
            self.emit_memory_params();
        }
        for inst in &block.insts {
            self.emit_inst(inst)?;
        }
        self.emit_terminator(&block.terminator)?;
        self.w.dedent();
        Ok(())
    }

    /// Load a by-reference parameter into the aggregate value the IR expects.
    fn emit_memory_params(&mut self) {
        let layout = self.options.layout;
        let params: Vec<(ValueId, Type)> = self
            .function
            .params
            .iter()
            .enumerate()
            .map(|(index, value)| {
                let param_ty = self
                    .function
                    .signature
                    .params
                    .get(index)
                    .copied()
                    .unwrap_or_else(|| self.function.value_type(*value));
                (*value, param_ty)
            })
            .collect();
        for (value, param_ty) in params {
            if let Some(arm) = self.abi.arm_small(param_ty, layout) {
                let slot = self.temp();
                self.w.line(format!(
                    "{slot} = alloca [{} x i8], align {}",
                    param_ty.size(layout).unwrap_or(0).div_ceil(8) * 8,
                    param_ty.align(layout).unwrap_or(1).max(1)
                ));
                let piece = format!("{}.0", self.value(value));
                self.w.line(format!(
                    "store {} {piece}, ptr {slot}",
                    arm.llvm_type(param_ty.size(layout).unwrap_or(0))
                ));
                self.w.line(format!(
                    "{} = load {}, ptr {slot}",
                    self.value(value),
                    super::abi::array(param_ty, layout)
                ));
                continue;
            }
            if let Some(chunks) = self.abi.chunks(param_ty, layout) {
                // Rebuild the object from one register per eightbyte.
                let slot = self.temp();
                self.w.line(format!(
                    "{slot} = alloca [{} x i8], align {}",
                    param_ty.size(layout).unwrap_or(0).div_ceil(8) * 8,
                    param_ty.align(layout).unwrap_or(1).max(1)
                ));
                for (index, chunk) in chunks.iter().enumerate() {
                    let piece = format!("{}.{}", self.value(value), index);
                    let address = self.chunk_address(&slot, chunk.offset);
                    self.w.line(format!(
                        "store {} {piece}, ptr {address}",
                        chunk.llvm_type()
                    ));
                }
                self.w.line(format!(
                    "{} = load {}, ptr {slot}",
                    self.value(value),
                    super::abi::array(param_ty, layout)
                ));
                continue;
            }
            if self.abi.by_reference(param_ty, layout) {
                self.w.line(format!(
                    "{} = load {}, ptr %arg{}",
                    self.value(value),
                    super::abi::array(param_ty, layout),
                    value.0
                ));
            }
        }
    }

    fn emit_slots(&mut self) -> Result<(), Diagnostic> {
        for (index, slot) in self.function.slots.iter().enumerate() {
            if slot.ty.is_void() {
                return Err(error(
                    None,
                    format!(
                        "function `{}`: slot `{}` has type void, which has no storage",
                        self.function.name, slot.name
                    ),
                ));
            }
            self.w.line(format!(
                "{} = alloca {}, align {}",
                self.slot_name(SlotId(index as u32)),
                ty(slot.ty),
                slot.align.max(1)
            ));
        }
        Ok(())
    }

    fn emit_phi(&mut self, phi: &Phi) -> Result<(), Diagnostic> {
        let name = self.value(phi.result);
        if phi.incoming.is_empty() {
            return Err(error(
                phi.span,
                format!(
                    "function `{}`: phi {} has no incoming values and cannot be emitted",
                    self.function.name, name
                ),
            ));
        }
        let mut text = format!("{name} = phi {} ", ty(phi.ty));
        for (index, incoming) in phi.incoming.iter().enumerate() {
            if index > 0 {
                text.push_str(", ");
            }
            let value = match incoming.value {
                Operand::Value(value) => self.value(value),
                Operand::Constant(constant) => literal(constant, self.options.layout),
            };
            text.push_str(&format!(
                "[ {value}, {} ]",
                self.label(incoming.predecessor)
            ));
        }
        self.w.line(text);
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Addresses and operands
    // -----------------------------------------------------------------------

    /// An address as a typed pointer operand (`ptr %v`, `ptr @g`, ...).
    ///
    /// GEPs deliberately carry no `inbounds`: IR address arithmetic wraps and
    /// makes no object-bounds promise.
    fn address(&mut self, address: &Address) -> Result<String, Diagnostic> {
        let offset = address.offset;
        match &address.base {
            AddressBase::Slot(slot) => {
                let slot_ty = self.function.slot(*slot).ty;
                if slot_ty.is_void() {
                    return Err(error(
                        None,
                        format!(
                            "function `{}`: slot `{}` has type void, which has no address",
                            self.function.name,
                            self.function.slot(*slot).name
                        ),
                    ));
                }
                let base = self.slot_name(*slot);
                if offset == 0 {
                    return Ok(format!("ptr {base}"));
                }
                let temp = self.temp();
                self.w.line(format!(
                    "{temp} = getelementptr i8, ptr {base}, i64 {offset}"
                ));
                Ok(format!("ptr {temp}"))
            }
            AddressBase::Symbol(name) => {
                let base = symbol(name);
                if offset == 0 {
                    Ok(format!("ptr {base}"))
                } else {
                    Ok(format!("ptr getelementptr (i8, ptr {base}, i64 {offset})"))
                }
            }
            AddressBase::Operand(Operand::Constant(constant)) => {
                let bits = match constant.ty {
                    Type::Ptr => constant.as_u64(),
                    Type::I1 | Type::I8 | Type::I16 | Type::I32 | Type::I64 => constant.as_u64(),
                    other => {
                        return Err(error(
                            None,
                            format!(
                                "function `{}`: address base constant has type `{}`",
                                self.function.name,
                                ty(other)
                            ),
                        ));
                    }
                };
                let bits = bits.wrapping_add(offset as u64) & ptr_mask(self.options.layout);
                Ok(format!(
                    "ptr {}",
                    pointer_constant_bits(bits, self.options.layout)
                ))
            }
            AddressBase::Operand(operand) => {
                let base = self.pointer_operand(*operand)?;
                if offset == 0 {
                    return Ok(base);
                }
                let temp = self.temp();
                self.w
                    .line(format!("{temp} = getelementptr i8, {base}, i64 {offset}"));
                Ok(format!("ptr {temp}"))
            }
        }
    }

    /// A pointer operand, converting an integer-form address with `inttoptr`.
    fn pointer_operand(&mut self, operand: Operand) -> Result<String, Diagnostic> {
        let operand_ty = self.function.operand_type(operand);
        if operand_ty.is_pointer() {
            return Ok(format!("ptr {}", self.text(operand)));
        }
        if !operand_ty.is_integer() {
            return Err(error(
                None,
                format!(
                    "function `{}`: address base has type `{}`, which is not a pointer",
                    self.function.name,
                    ty(operand_ty)
                ),
            ));
        }
        match operand {
            Operand::Constant(constant) => {
                let bits = constant.as_u64() & ptr_mask(self.options.layout);
                Ok(format!(
                    "ptr inttoptr ({} {bits} to ptr)",
                    ptr_int_name(self.options.layout)
                ))
            }
            Operand::Value(value) => {
                let temp = self.temp();
                self.w.line(format!(
                    "{temp} = inttoptr {} {} to ptr",
                    ty(operand_ty),
                    self.value(value)
                ));
                Ok(format!("ptr {temp}"))
            }
        }
    }

    /// A GEP index, always a typed integer operand.
    fn index_operand(&mut self, operand: Operand) -> Result<String, Diagnostic> {
        match operand {
            Operand::Constant(constant) => {
                if !constant.ty.is_integer() {
                    return Err(error(
                        None,
                        format!(
                            "function `{}`: GEP index constant has type `{}`",
                            self.function.name,
                            ty(constant.ty)
                        ),
                    ));
                }
                Ok(format!("i64 {}", constant.as_i64()))
            }
            Operand::Value(value) => {
                let value_ty = self.function.value_type(value);
                if !value_ty.is_integer() {
                    return Err(error(
                        None,
                        format!(
                            "function `{}`: GEP index {} has type `{}`, which is not an integer",
                            self.function.name,
                            self.value(value),
                            ty(value_ty)
                        ),
                    ));
                }
                Ok(format!("{} {}", ty(value_ty), self.value(value)))
            }
        }
    }

    // -----------------------------------------------------------------------
    // Operand spelling and conversions
    // -----------------------------------------------------------------------

    /// Untyped operand for positions that already state the type once
    /// (`add i32 %a, %b`, `ret i32 %v`, `store i32 %v, ptr %p`).
    fn text(&self, operand: Operand) -> String {
        match operand {
            Operand::Value(value) => self.value(value),
            Operand::Constant(constant) => literal(constant, self.options.layout),
        }
    }

    /// Operand with its type, for positions that need it per argument.
    fn typed(&self, operand: Operand) -> String {
        match operand {
            Operand::Value(value) => format!(
                "{} {}",
                ty(self.function.value_type(value)),
                self.value(value)
            ),
            Operand::Constant(value) => constant(value, self.options.layout),
        }
    }

    /// Untyped operand guaranteed to have type `wanted`; inserts a converting
    /// instruction for mismatched SSA values.
    fn as_untyped(&mut self, operand: Operand, wanted: Type) -> Result<String, Diagnostic> {
        let from = self.function.operand_type(operand);
        if from == wanted {
            return Ok(self.text(operand));
        }
        match operand {
            Operand::Constant(constant) => {
                if !wanted.is_integer() && !wanted.is_pointer() {
                    return Err(self.conversion_error(from, wanted));
                }
                Ok(literal(retyped(constant, wanted), self.options.layout))
            }
            Operand::Value(value) => {
                let source = self.value(value);
                let converted = self.conversion(from, wanted, &source)?;
                let temp = self.temp();
                self.w.line(format!("{temp} = {converted}"));
                Ok(temp)
            }
        }
    }

    /// Typed operand guaranteed to have type `wanted`.
    fn as_typed(&mut self, operand: Operand, wanted: Type) -> Result<String, Diagnostic> {
        let from = self.function.operand_type(operand);
        if from == wanted {
            return Ok(self.typed(operand));
        }
        let text = self.as_untyped(operand, wanted)?;
        Ok(format!("{} {text}", ty(wanted)))
    }

    fn conversion(&self, from: Type, to: Type, source: &str) -> Result<String, Diagnostic> {
        if from.is_integer() && to.is_integer() {
            let op = if from.integer_bits() > to.integer_bits() {
                "trunc"
            } else {
                "zext"
            };
            return Ok(format!("{op} {} {source} to {}", ty(from), ty(to)));
        }
        if from.is_pointer() && to.is_integer() {
            return Ok(format!("ptrtoint {source} to {}", ty(to)));
        }
        if from.is_integer() && to.is_pointer() {
            return Ok(format!("inttoptr {} {source} to ptr", ty(from)));
        }
        Err(self.conversion_error(from, to))
    }

    fn conversion_error(&self, from: Type, to: Type) -> Diagnostic {
        error(
            None,
            format!(
                "function `{}`: cannot convert `{}` to `{}` in LLVM IR",
                self.function.name,
                ty(from),
                ty(to)
            ),
        )
    }

    fn cast_error(&self, inst: &Inst, op: CastOp, from: Type, to: Type) -> Diagnostic {
        error(
            inst.span,
            format!(
                "function `{}`: cannot emit `{op}` from `{}` to `{}` in LLVM IR",
                self.function.name,
                ty(from),
                ty(to)
            ),
        )
    }

    fn length_constant(&self, bytes: u32) -> String {
        constant(
            Constant::uint(ptr_int_ty(self.options.layout), bytes as u64),
            self.options.layout,
        )
    }

    // -----------------------------------------------------------------------
    // Small helpers
    // -----------------------------------------------------------------------

    fn value(&self, value: ValueId) -> String {
        format!("%v{}", value.0)
    }

    fn label_name(&self, block: BlockId) -> String {
        format!("bb{}", block.0)
    }

    fn label(&self, block: BlockId) -> String {
        format!("%{}", self.label_name(block))
    }

    fn slot_name(&self, slot: SlotId) -> String {
        format!("%s{}", slot.0)
    }

    fn temp(&mut self) -> String {
        let name = format!("%t{}", self.next_temp);
        self.next_temp += 1;
        name
    }

    fn result_prefix(&self, inst: &Inst, what: &str) -> Result<String, Diagnostic> {
        match inst.result {
            Some(value) => Ok(format!("{} = ", self.value(value))),
            None => Err(error(
                inst.span,
                format!(
                    "function `{}`: `{what}` produces a value but has no result",
                    self.function.name
                ),
            )),
        }
    }

    fn void_error(&self, inst: &Inst, what: &str) -> Diagnostic {
        error(
            inst.span,
            format!(
                "function `{}`: `{what}` with type void has no value to access",
                self.function.name
            ),
        )
    }

    /// Address of one eightbyte inside a scratch slot.
    fn chunk_address(&mut self, slot: &str, offset: u32) -> String {
        if offset == 0 {
            return slot.to_string();
        }
        let address = self.temp();
        self.w.line(format!(
            "{address} = getelementptr i8, ptr {slot}, i64 {offset}"
        ));
        address
    }

    fn natural_align(&self, ty: Type) -> u32 {
        align_of(ty, self.options.layout)
    }
}
