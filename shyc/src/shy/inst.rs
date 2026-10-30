//! Scalar instruction lowering for the Shy backend.
//!
//! Operations use the scratch-register convention documented in
//! `function.rs`. Narrow integer values are kept zero-extended in registers;
//! signed operations explicitly sign-extend their operands first. Results are
//! masked back to the IR type width before they are stored into a spill slot.

use super::fallback_span;
use super::function::FnWriter;
use crate::ir::{Inst, InstKind};
use crate::source::Diagnostic;

impl FnWriter<'_> {
    pub(crate) fn instruction(&mut self, instruction: &Inst) -> Result<(), Diagnostic> {
        self.comment(instruction.span);
        match &instruction.kind {
            InstKind::Load {
                address,
                ty,
                atomic,
                ..
            } => self.load(instruction, address, *ty, *atomic),
            InstKind::Store {
                address,
                value,
                ty,
                atomic,
                ..
            } => self.store(address, value, *ty, *atomic),
            InstKind::PtrOffset { base, offset } => {
                let result = result(instruction)?;
                self.load_operand(base, "1x", None)?;
                self.load_operand(offset, "3x", None)?;
                self.line("adda 1x 3x");
                self.store_value_word(result, "1x");
                Ok(())
            }
            InstKind::SymbolAddr { symbol } => {
                let result = result(instruction)?;
                self.line(format!("setn 1x {symbol}"));
                self.store_value_word(result, "1x");
                Ok(())
            }
            InstKind::SlotAddr { slot } => {
                let result = result(instruction)?;
                let offset = self.frame.slot(*slot);
                self.frame_addr(offset, "1x");
                self.store_value_word(result, "1x");
                Ok(())
            }
            InstKind::Binary { op, lhs, rhs, ty } => self.binary(instruction, *op, lhs, rhs, *ty),
            InstKind::Unary { op, value, ty } => self.unary(instruction, *op, value, *ty),
            InstKind::ICmp { pred, lhs, rhs, ty } => self.icmp(instruction, *pred, lhs, rhs, *ty),
            InstKind::FCmp { .. } => Err(Diagnostic::new(
                instruction.span.unwrap_or_else(fallback_span),
                "floating-point comparisons are not implemented by the Shy backend",
            )),
            InstKind::Cast { op, value, to } => self.cast(instruction, *op, value, *to),
            InstKind::Call(call) => self.emit_call(instruction, call, instruction.span),
            InstKind::MemCpy { dst, src, bytes } => {
                self.emit_address(dst, "3x")?;
                self.emit_address(src, "1x")?;
                self.copy_bytes("3x", "1x", *bytes)
            }
            InstKind::MemSet { dst, byte, bytes } => self.memset(dst, *byte, *bytes),
            InstKind::InlineAsm(_) => Err(Diagnostic::new(
                instruction.span.unwrap_or_else(fallback_span),
                "inline assembly is not implemented by the Shy backend; the instruction is rejected, not dropped",
            )),
        }
    }
}

pub(crate) fn result(instruction: &Inst) -> Result<crate::ir::ValueId, Diagnostic> {
    instruction.result.ok_or_else(|| {
        Diagnostic::new(
            instruction.span.unwrap_or_else(fallback_span),
            "value-producing IR instruction has no result",
        )
    })
}
