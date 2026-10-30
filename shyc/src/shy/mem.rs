//! Memory and operand access for the Shy backend.

use super::fallback_span;
use super::frame::is_64;
use super::function::FnWriter;
use super::inst::result;
use crate::ir::{Address, AddressBase, AtomicOrder, Inst, Operand, Type};
use crate::source::Diagnostic;

impl FnWriter<'_> {
    // Memory
    // -----------------------------------------------------------------------

    pub(crate) fn load(
        &mut self,
        instruction: &Inst,
        address: &Address,
        ty: Type,
        atomic: AtomicOrder,
    ) -> Result<(), Diagnostic> {
        if atomic.is_atomic() {
            return Err(Diagnostic::new(
                instruction.span.unwrap_or_else(fallback_span),
                "atomic memory operations are not implemented by the Shy backend",
            ));
        }
        let result = result(instruction)?;
        self.emit_address(address, "3x")?;
        let layout = self.function.layout;
        if ty.is_aggregate() {
            let bytes = ty.size(layout).unwrap_or(0);
            let object = self.frame.object(result);
            self.frame_addr(object, "1x");
            self.copy_bytes("1x", "3x", bytes)?;
            self.frame_addr(object, "1x");
            self.store_value_word(result, "1x");
            return Ok(());
        }
        match ty.size(layout).unwrap_or(0) {
            1 => {
                self.line("get8a 1x 3x");
                self.store_value_word(result, "1x");
            }
            2 => {
                self.line("get16a 1x 3x");
                self.store_value_word(result, "1x");
            }
            4 => {
                self.line("geta 1x 3x");
                self.store_value_word(result, "1x");
            }
            8 => {
                self.line("geta 2x 3x");
                self.line("addn 3x 4");
                self.line("geta 1x 3x");
                self.store_value_pair(result, "2x", "1x");
            }
            size => {
                return Err(Diagnostic::new(
                    instruction.span.unwrap_or_else(fallback_span),
                    format!("cannot load a scalar of size {size} (type {ty})"),
                ));
            }
        }
        Ok(())
    }

    pub(crate) fn store(
        &mut self,
        address: &Address,
        value: &Operand,
        ty: Type,
        atomic: AtomicOrder,
    ) -> Result<(), Diagnostic> {
        if atomic.is_atomic() {
            return Err(Diagnostic::new(
                fallback_span(),
                "atomic memory operations are not implemented by the Shy backend",
            ));
        }
        self.emit_address(address, "3x")?;
        if ty.is_aggregate() {
            let bytes = ty.size(self.function.layout).unwrap_or(0);
            self.load_operand(value, "1x", None)?;
            return self.copy_bytes("3x", "1x", bytes);
        }
        let size = ty.size(self.function.layout).unwrap_or(0);
        self.store_scalar_at(ty, value, size, "3x")
    }

    pub(crate) fn memset(&mut self, dst: &Address, byte: u8, bytes: u32) -> Result<(), Diagnostic> {
        self.emit_address(dst, "3x")?;
        if bytes == 0 {
            return Ok(());
        }
        let loop_label = self.fresh("memset");
        let done_label = self.fresh("memset_done");
        self.line(format!("setn cx {bytes}"));
        self.line(format!("setn dx {byte}"));
        self.line(format!("{loop_label}:"));
        self.line("equn cx 0");
        self.line(format!("jmpn {done_label}"));
        self.line("put8a 3x dx");
        self.line("addn 3x 1");
        self.line("subn cx 1");
        self.line(format!("ujmpn {loop_label}"));
        self.line(format!("{done_label}:"));
        Ok(())
    }

    pub(crate) fn emit_address(
        &mut self,
        address: &Address,
        register: &str,
    ) -> Result<(), Diagnostic> {
        match &address.base {
            AddressBase::Slot(slot) => {
                let offset = self.frame.slot(*slot);
                self.frame_addr(offset, register);
            }
            AddressBase::Symbol(symbol) => {
                self.line(format!("setn {register} {}", super::operand_symbol(symbol)))
            }
            AddressBase::Operand(operand) => {
                self.load_operand(operand, register, None)?;
            }
        }
        if address.offset != 0 {
            self.line(format!("addn {register} 0x{:x}", address.offset as u32));
        }
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Operand access
    // -----------------------------------------------------------------------

    pub(crate) fn operand_type(&self, operand: &Operand) -> Type {
        match operand {
            Operand::Value(value) => self.function.value_type(*value),
            Operand::Constant(constant) => constant.ty,
        }
    }

    pub(crate) fn load_operand(
        &mut self,
        operand: &Operand,
        low: &str,
        high: Option<&str>,
    ) -> Result<Type, Diagnostic> {
        let ty = self.operand_type(operand);
        match operand {
            Operand::Value(value) => {
                let offset = self.frame.value(*value);
                if is_64(ty) {
                    if let Some(high) = high {
                        self.load_pair(offset, high, low);
                    } else {
                        self.load_word(offset + 4, low);
                    }
                } else {
                    self.load_word(offset, low);
                }
            }
            Operand::Constant(constant) => {
                let bits = constant.as_u64();
                if is_64(ty) {
                    self.line(format!("setn {low} 0x{:x}", bits as u32));
                    if let Some(high) = high {
                        self.line(format!("setn {high} 0x{:x}", (bits >> 32) as u32));
                    }
                } else {
                    self.line(format!("setn {low} 0x{:x}", bits as u32));
                }
            }
        }
        Ok(ty)
    }

    pub(crate) fn load_operand_word(
        &mut self,
        operand: &Operand,
        source_offset: u32,
        register: &str,
    ) {
        match operand {
            Operand::Value(value) => {
                let offset = self.frame.value(*value) + source_offset;
                self.load_word(offset, register);
            }
            Operand::Constant(constant) => {
                let bits = constant.as_u64();
                let wide = is_64(self.operand_type(operand));
                let word = if wide && source_offset == 0 {
                    (bits >> 32) as u32
                } else {
                    bits as u32
                };
                self.line(format!("setn {register} 0x{word:x}"));
            }
        }
    }
}
