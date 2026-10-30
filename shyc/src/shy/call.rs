//! Call lowering: ABI staging, indirect calls, and call results.

use super::abi;
use super::abi::{part_mask, return_mask};
use super::fallback_span;
use super::function::FnWriter;
use crate::abi::{Location, Passing};
use crate::ir::{Call, CallTarget, Inst, Operand, Type};
use crate::source::{Diagnostic, Span};

impl FnWriter<'_> {
    // Calls
    // -----------------------------------------------------------------------

    pub(crate) fn emit_call(
        &mut self,
        instruction: &Inst,
        call: &Call,
        span: Option<Span>,
    ) -> Result<(), Diagnostic> {
        let span = span.unwrap_or_else(fallback_span);
        let plan = abi::plan_signature(&call.signature, self.function.layout, span)?;
        if call.args.len() != call.signature.params.len() {
            return Err(Diagnostic::new(
                span,
                "call argument count does not match its signature",
            ));
        }
        let total_stack = plan.stack_bytes();
        if total_stack > 0 {
            self.line(format!("addn sp {total_stack}"));
        }

        for (index, argument) in plan.args.iter().enumerate() {
            let operand = &call.args[index];
            let operand_type = self.operand_type(operand);
            let Passing::Direct { parts } = &argument.passing else {
                return Err(Diagnostic::new(
                    span,
                    format!("unsupported Shy ABI v1 classification for argument {index}"),
                ));
            };
            for part in parts {
                let Location::Stack { offset } = part.location else {
                    continue;
                };
                self.line("seta 3x sp");
                self.line(format!("addn 3x 0x{:x}", offset as u32));
                if operand_type.is_aggregate() {
                    let bytes = operand_type.size(self.function.layout).unwrap_or(0);
                    self.load_operand(operand, "1x", None)?;
                    self.copy_bytes("3x", "1x", bytes)?;
                } else {
                    self.store_scalar_at(operand_type, operand, part.size, "3x")?;
                }
            }
        }

        for (index, argument) in plan.args.iter().enumerate() {
            let operand = &call.args[index];
            let Passing::Direct { parts } = &argument.passing else {
                continue;
            };
            for part in parts {
                let Location::Register(register) = part.location else {
                    continue;
                };
                let name = abi::argument_register(register.index).ok_or_else(|| {
                    Diagnostic::new(
                        span,
                        format!(
                            "Shy ABI v1 used argument register slot {} for argument {index}",
                            register.index
                        ),
                    )
                })?;
                self.load_operand_word(operand, part.source_offset, name);
                if let Some(mask) = part_mask(self.operand_type(operand), part.size) {
                    self.line(format!("andn {name} 0x{mask:x}"));
                }
            }
        }

        if call.signature.ret.is_aggregate() {
            let result = instruction
                .result
                .ok_or_else(|| Diagnostic::new(span, "aggregate call has no result value"))?;
            let object = self.frame.object(result);
            self.frame_addr(object, "4x");
        }

        match &call.target {
            CallTarget::Direct(symbol) => {
                self.line(format!("calln {}", super::operand_symbol(symbol)))
            }
            CallTarget::Indirect(operand) => {
                self.load_operand(operand, "ex", None)?;
                self.line("calla ex");
            }
        }

        if total_stack > 0 {
            self.line(format!("subn sp {total_stack}"));
        }

        let ret = call.signature.ret;
        if ret.is_void() {
            if instruction.result.is_some() {
                return Err(Diagnostic::new(span, "void call produces a value"));
            }
            return Ok(());
        }
        if ret.is_aggregate() {
            let result = instruction
                .result
                .ok_or_else(|| Diagnostic::new(span, "aggregate call has no result value"))?;
            let object = self.frame.object(result);
            self.frame_addr(object, "1x");
            self.store_value_word(result, "1x");
            return Ok(());
        }

        let result = instruction
            .result
            .ok_or_else(|| Diagnostic::new(span, "call result value is missing"))?;
        let registers = abi::direct_return_parts(&plan)
            .map_err(|message| Diagnostic::new(span, format!("Shy ABI v1: {message}")))?;
        if let Some(mask) = return_mask(ret) {
            for (register, _) in &registers {
                self.line(format!("andn {register} 0x{mask:x}"));
            }
        }
        for (register, source_offset) in registers {
            let spill = self.frame.value(result) + source_offset;
            self.store_word(spill, register);
        }
        Ok(())
    }

    /// Store a scalar operand at the address already held in `address`.
    ///
    /// `size` is the ABI/IR object size, which may be narrower than the
    /// register representation of the operand.
    pub(crate) fn store_scalar_at(
        &mut self,
        ty: Type,
        operand: &Operand,
        size: u32,
        address: &str,
    ) -> Result<(), Diagnostic> {
        if size == 8 {
            self.load_operand(operand, "1x", Some("2x"))?;
            self.line(format!("puta {address} 2x"));
            self.line(format!("addn {address} 4"));
            self.line(format!("puta {address} 1x"));
            return Ok(());
        }
        self.load_operand(operand, "1x", None)?;
        match size {
            1 => self.line(format!("put8a {address} 1x")),
            2 => self.line(format!("put16a {address} 1x")),
            4 => self.line(format!("puta {address} 1x")),
            other => {
                return Err(Diagnostic::new(
                    fallback_span(),
                    format!("cannot store a scalar of size {other} (type {ty})"),
                ));
            }
        }
        Ok(())
    }
}
