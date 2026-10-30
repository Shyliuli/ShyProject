//! Arithmetic, casts, and integer conversions for the Shy backend.

use super::fallback_span;
use super::frame::is_64;
use super::function::FnWriter;
use super::inst::result;
use crate::ir::{BinaryOp, CastOp, Inst, InstKind, Operand, Type, UnaryOp};
use crate::source::Diagnostic;

impl FnWriter<'_> {
    // Binary and unary operations
    // -----------------------------------------------------------------------

    pub(crate) fn binary(
        &mut self,
        instruction: &Inst,
        op: BinaryOp,
        lhs: &Operand,
        rhs: &Operand,
        ty: Type,
    ) -> Result<(), Diagnostic> {
        if op.is_float() || ty.is_float() {
            return self.float_binary(instruction, op, lhs, rhs, ty);
        }
        if ty.is_aggregate() || ty.is_void() {
            return Err(Diagnostic::new(
                instruction.span.unwrap_or_else(fallback_span),
                format!("binary operation on unsupported type {ty}"),
            ));
        }
        let result = result(instruction)?;
        let wide = is_64(ty);
        let rhs_wide = is_64(self.operand_type(rhs));
        self.load_operand(lhs, "1x", if wide { Some("2x") } else { None })?;
        self.load_operand(rhs, "3x", if rhs_wide { Some("cx") } else { None })?;
        if wide {
            self.binary_wide(op, rhs_wide)?;
            self.store_value_pair(result, "2x", "1x");
        } else {
            self.binary_narrow(op, ty)?;
            self.mask_to("1x", ty);
            self.store_value_word(result, "1x");
        }
        Ok(())
    }

    /// Floating-point arithmetic through the runtime helpers.
    ///
    /// `f32` operands are single words in `4x`/`5x`; `f64` operands are
    /// 64-bit pairs, so the second operand starts at `6x`.
    /// Inline assembly: bind operands to argument registers, splice the text,
    /// then write the results back.
    ///
    /// The assembly body is opaque, so this is the one place where the backend
    /// trusts the source. Bindings use the Shy argument registers in order,
    /// matching what `codegen_shy.c` does, and every value also lives in its
    /// frame slot, so clobbering temporaries inside the body is harmless.
    pub(crate) fn inline_asm(&mut self, instruction: &Inst) -> Result<(), Diagnostic> {
        let InstKind::InlineAsm(asm) = &instruction.kind else {
            return Err(Diagnostic::new(
                instruction.span.unwrap_or_else(fallback_span),
                "internal error: not inline assembly",
            ));
        };
        if asm.bindings.len() > super::abi::ARGUMENT_REGISTERS.len() {
            return Err(Diagnostic::new(
                instruction.span.unwrap_or_else(fallback_span),
                "inline assembly binds more values than there are argument registers",
            ));
        }
        for (index, binding) in asm.bindings.iter().enumerate() {
            let register = super::abi::ARGUMENT_REGISTERS[index];
            if binding.access == crate::ir::AsmAccess::AddressOnly {
                self.emit_address(&binding.address, register)?;
                continue;
            }
            if matches!(
                binding.access,
                crate::ir::AsmAccess::Read | crate::ir::AsmAccess::ReadWrite
            ) {
                self.emit_address(&binding.address, "3x")?;
                match binding.bytes {
                    1 => self.line(format!("get8a {register} 3x")),
                    2 => self.line(format!("get16a {register} 3x")),
                    4 => self.line(format!("geta {register} 3x")),
                    other => {
                        return Err(Diagnostic::new(
                            instruction.span.unwrap_or_else(fallback_span),
                            format!("inline assembly binding of {other} bytes"),
                        ));
                    }
                }
            }
        }

        let mut text = asm.text.clone();
        for (index, binding) in asm.bindings.iter().enumerate() {
            let register = super::abi::ARGUMENT_REGISTERS[index];
            text = text.replace(&format!("{{&{}}}", binding.name), register);
            text = text.replace(&format!("{{{}}}", binding.name), register);
        }
        self.out.push_str(&text);
        if !text.ends_with('\n') {
            self.out.push('\n');
        }

        for (index, binding) in asm.bindings.iter().enumerate() {
            if binding.access == crate::ir::AsmAccess::AddressOnly
                || binding.access == crate::ir::AsmAccess::Read
            {
                continue;
            }
            let register = super::abi::ARGUMENT_REGISTERS[index];
            self.emit_address(&binding.address, "3x")?;
            match binding.bytes {
                1 => self.line(format!("put8a 3x {register}")),
                2 => self.line(format!("put16a 3x {register}")),
                4 => self.line(format!("puta 3x {register}")),
                other => {
                    return Err(Diagnostic::new(
                        instruction.span.unwrap_or_else(fallback_span),
                        format!("inline assembly binding of {other} bytes"),
                    ));
                }
            }
        }
        Ok(())
    }

    fn float_binary(
        &mut self,
        instruction: &Inst,
        op: BinaryOp,
        lhs: &Operand,
        rhs: &Operand,
        ty: Type,
    ) -> Result<(), Diagnostic> {
        let suffix = match op {
            BinaryOp::FAdd => "add",
            BinaryOp::FSub => "sub",
            BinaryOp::FMul => "mul",
            BinaryOp::FDiv => "div",
            _ => {
                return Err(Diagnostic::new(
                    instruction.span.unwrap_or_else(fallback_span),
                    "unsupported floating-point operation",
                ));
            }
        };
        let prefix = if ty == Type::F32 { "f32" } else { "f64" };
        if is_64(ty) {
            self.load_operand(lhs, "4x", Some("5x"))?;
            self.load_operand(rhs, "6x", Some("7x"))?;
        } else {
            self.load_operand(lhs, "4x", None)?;
            self.load_operand(rhs, "5x", None)?;
        }
        self.line(format!("calln __shy_{prefix}_{suffix}"));
        let result = result(instruction)?;
        if is_64(ty) {
            self.store_value_pair(result, "2x", "1x");
        } else {
            self.store_value_word(result, "1x");
        }
        Ok(())
    }

    fn binary_narrow(&mut self, op: BinaryOp, ty: Type) -> Result<(), Diagnostic> {
        if matches!(op, BinaryOp::SDiv | BinaryOp::SRem | BinaryOp::AShr) {
            self.sign_extend("1x", ty);
            self.sign_extend("3x", ty);
        }
        match op {
            BinaryOp::Add => self.line("adda 1x 3x"),
            BinaryOp::Sub => self.line("suba 1x 3x"),
            BinaryOp::Mul => self.line("mula 1x 3x"),
            BinaryOp::UDiv => self.line("diva 1x 3x"),
            BinaryOp::SDiv => self.signed_divide(false),
            BinaryOp::URem => self.unsigned_remainder(),
            BinaryOp::SRem => self.signed_divide(true),
            BinaryOp::And => self.line("anda 1x 3x"),
            BinaryOp::Or => self.line("ora 1x 3x"),
            BinaryOp::Xor => self.line("xora 1x 3x"),
            BinaryOp::Shl => self.line("lsa 1x 3x"),
            BinaryOp::LShr => self.line("rsa 1x 3x"),
            BinaryOp::AShr => self.arithmetic_shift_right_32(),
            _ => {
                return Err(Diagnostic::new(
                    fallback_span(),
                    format!("unsupported Shy binary operation {op}"),
                ));
            }
        }
        Ok(())
    }

    fn binary_wide(&mut self, op: BinaryOp, rhs_wide: bool) -> Result<(), Diagnostic> {
        match op {
            BinaryOp::Add => self.add_wide(),
            BinaryOp::Sub => self.subtract_wide(),
            BinaryOp::Mul => self.call_helper("u64mul"),
            BinaryOp::And => {
                self.line("anda 1x 3x");
                self.line("anda 2x cx");
            }
            BinaryOp::Or => {
                self.line("ora 1x 3x");
                self.line("ora 2x cx");
            }
            BinaryOp::Xor => {
                self.line("xora 1x 3x");
                self.line("xora 2x cx");
            }
            BinaryOp::Shl => {
                if !rhs_wide {
                    self.line("setn cx 0");
                }
                self.call_helper("u64shl");
            }
            BinaryOp::LShr => {
                if !rhs_wide {
                    self.line("setn cx 0");
                }
                self.call_helper("u64lshr");
            }
            BinaryOp::AShr => {
                if !rhs_wide {
                    self.line("setn cx 0");
                }
                self.call_helper("u64ashr");
            }
            BinaryOp::UDiv => self.call_helper("u64div"),
            BinaryOp::URem => self.call_helper("u64mod"),
            BinaryOp::SDiv => self.call_helper("i64div"),
            BinaryOp::SRem => self.call_helper("i64mod"),
            _ => {
                return Err(Diagnostic::new(
                    fallback_span(),
                    format!("unsupported Shy binary operation {op}"),
                ));
            }
        }
        Ok(())
    }

    fn add_wide(&mut self) {
        let carry = self.fresh("carry");
        let done = self.fresh("carry_done");
        self.line("seta ex 1x");
        self.line("adda 1x 3x");
        self.line("smaa 1x ex");
        self.line("setn ex 0");
        self.line(format!("jmpn {carry}"));
        self.line(format!("ujmpn {done}"));
        self.line(format!("{carry}:"));
        self.line("setn ex 1");
        self.line(format!("{done}:"));
        self.line("adda 2x cx");
        self.line("adda 2x ex");
    }

    fn subtract_wide(&mut self) {
        let borrow = self.fresh("borrow");
        let done = self.fresh("borrow_done");
        self.line("seta ex 1x");
        self.line("suba 1x 3x");
        self.line("smaa ex 3x");
        self.line("setn ex 0");
        self.line(format!("jmpn {borrow}"));
        self.line(format!("ujmpn {done}"));
        self.line(format!("{borrow}:"));
        self.line("setn ex 1");
        self.line(format!("{done}:"));
        self.line("suba 2x cx");
        self.line("suba 2x ex");
    }

    fn unsigned_remainder(&mut self) {
        self.line("seta 4x 1x");
        self.line("diva 1x 3x");
        self.line("seta 5x 1x");
        self.line("seta 1x 4x");
        self.line("mula 5x 3x");
        self.line("suba 1x 5x");
    }

    /// Signed 32-bit division/remainder with truncation toward zero.
    ///
    /// The ISA only divides unsigned, so both operands are made positive,
    /// divided, and the quotient is negated when the signs differ. The
    /// remainder is recomputed as `a - (a / b) * b` so it keeps the sign of the
    /// dividend.
    fn signed_divide(&mut self, remainder: bool) {
        let abs_a = self.fresh("abs_a");
        let abs_a_done = self.fresh("abs_a_done");
        let abs_b = self.fresh("abs_b");
        let abs_b_done = self.fresh("abs_b_done");
        let quotient_done = self.fresh("quot_done");

        self.line("seta 4x 1x");
        self.line("seta 5x 3x");
        self.line("seta dx 1x");
        self.line("rsn dx 31");
        self.line("seta ex 3x");
        self.line("rsn ex 31");
        self.line("xora dx ex");
        self.line("seta ex 1x");
        self.line("rsn ex 31");
        self.line("equn ex 0");
        self.line(format!("jmpn {abs_a_done}"));
        self.line(format!("{abs_a}:"));
        self.line("nota 1x");
        self.line("addn 1x 1");
        self.line(format!("{abs_a_done}:"));
        self.line("seta ex 3x");
        self.line("rsn ex 31");
        self.line("equn ex 0");
        self.line(format!("jmpn {abs_b_done}"));
        self.line(format!("{abs_b}:"));
        self.line("nota 3x");
        self.line("addn 3x 1");
        self.line(format!("{abs_b_done}:"));
        self.line("diva 1x 3x");
        self.line("equn dx 0");
        self.line(format!("jmpn {quotient_done}"));
        self.line("nota 1x");
        self.line("addn 1x 1");
        self.line(format!("{quotient_done}:"));
        if remainder {
            self.line("seta 6x 1x");
            self.line("seta 1x 4x");
            self.line("mula 6x 5x");
            self.line("suba 1x 6x");
        }
    }

    fn arithmetic_shift_right_32(&mut self) {
        let loop_label = self.fresh("ashr");
        let done_label = self.fresh("ashr_done");
        self.line("equn 3x 0");
        self.line(format!("jmpn {done_label}"));
        self.line(format!("{loop_label}:"));
        self.line("seta ex 1x");
        self.line("rsn ex 31");
        self.line("lsn ex 31");
        self.line("rsn 1x 1");
        self.line("ora 1x ex");
        self.line("subn 3x 1");
        self.line("equn 3x 0");
        self.line(format!("jmpn {done_label}"));
        self.line(format!("ujmpn {loop_label}"));
        self.line(format!("{done_label}:"));
    }

    pub(crate) fn unary(
        &mut self,
        instruction: &Inst,
        op: UnaryOp,
        value: &Operand,
        ty: Type,
    ) -> Result<(), Diagnostic> {
        if ty.is_float() || matches!(op, UnaryOp::FNeg) {
            return Err(Diagnostic::new(
                instruction.span.unwrap_or_else(fallback_span),
                "floating-point arithmetic is not implemented by the Shy backend",
            ));
        }
        if ty.is_aggregate() || ty.is_void() {
            return Err(Diagnostic::new(
                instruction.span.unwrap_or_else(fallback_span),
                format!("unary operation on unsupported type {ty}"),
            ));
        }
        let result = result(instruction)?;
        let wide = is_64(ty);
        self.load_operand(value, "1x", if wide { Some("2x") } else { None })?;
        match op {
            UnaryOp::Neg if wide => {
                let carry = self.fresh("neg");
                let done = self.fresh("neg_done");
                self.line("nota 1x");
                self.line("nota 2x");
                self.line("addn 1x 1");
                self.line("equn 1x 0");
                self.line(format!("jmpn {carry}"));
                self.line(format!("ujmpn {done}"));
                self.line(format!("{carry}:"));
                self.line("addn 2x 1");
                self.line(format!("{done}:"));
            }
            UnaryOp::Neg => {
                self.line("nota 1x");
                self.line("addn 1x 1");
            }
            UnaryOp::Not if wide => {
                self.line("nota 1x");
                self.line("nota 2x");
            }
            UnaryOp::Not => self.line("nota 1x"),
            UnaryOp::FNeg => unreachable!("floating-point negation was rejected"),
        }
        if wide {
            self.store_value_pair(result, "2x", "1x");
        } else {
            self.mask_to("1x", ty);
            self.store_value_word(result, "1x");
        }
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Casts
    // -----------------------------------------------------------------------

    /// Floating-point conversions through the soft-float runtime.
    ///
    /// The helpers exchange raw bit patterns: an `f32` is one word, an `f64` a
    /// 64-bit pair, and the integer helpers always work on `i64`/`u64`, so
    /// narrower integers are widened first and results are truncated after.
    fn cast_float(
        &mut self,
        instruction: &Inst,
        op: CastOp,
        value: &Operand,
        from: Type,
        to: Type,
    ) -> Result<(), Diagnostic> {
        let result = result(instruction)?;
        let wide_to = is_64(to);
        match (op, from, to) {
            (CastOp::FPTrunc, Type::F64, Type::F32) => {
                self.load_operand(value, "4x", Some("5x"))?;
                self.line("calln __shy_f64_to_f32");
                self.store_value_word(result, "1x");
            }
            (CastOp::FPExt, Type::F32, Type::F64) => {
                self.load_operand(value, "4x", None)?;
                self.line("calln __shy_f32_to_f64");
                self.store_value_pair(result, "2x", "1x");
            }
            (CastOp::Bitcast, _, _) => {
                // Same width reinterpretation: copy the words unchanged.
                self.load_operand(value, "1x", if is_64(from) { Some("2x") } else { None })?;
                if wide_to {
                    self.store_value_pair(result, "2x", "1x");
                } else {
                    self.store_value_word(result, "1x");
                }
            }
            (CastOp::SIToFP | CastOp::UIToFP, _, Type::F32 | Type::F64) => {
                self.widen_for_float(value, from, op == CastOp::SIToFP)?;
                let unsigned = op == CastOp::UIToFP;
                let symbol = match (to, unsigned) {
                    (Type::F32, false) => "__shy_i64_to_f32",
                    (Type::F32, true) => "__shy_u64_to_f32",
                    (Type::F64, false) => "__shy_i64_to_f64",
                    (Type::F64, true) => "__shy_u64_to_f64",
                    _ => {
                        return Err(Diagnostic::new(
                            instruction.span.unwrap_or_else(fallback_span),
                            "unsupported integer-to-float conversion",
                        ));
                    }
                };
                self.line("seta 4x 1x");
                self.line("seta 5x 2x");
                self.line(format!("calln {symbol}"));
                if wide_to {
                    self.store_value_pair(result, "2x", "1x");
                } else {
                    self.store_value_word(result, "1x");
                }
            }
            (CastOp::FPToSI | CastOp::FPToUI, Type::F32 | Type::F64, _) => {
                if is_64(from) {
                    self.load_operand(value, "4x", Some("5x"))?;
                } else {
                    self.load_operand(value, "4x", None)?;
                }
                let unsigned = op == CastOp::FPToUI;
                let symbol = match (from, unsigned) {
                    (Type::F32, false) => "__shy_f32_to_i64",
                    (Type::F32, true) => "__shy_f32_to_u64",
                    (Type::F64, false) => "__shy_f64_to_i64",
                    (Type::F64, true) => "__shy_f64_to_u64",
                    _ => {
                        return Err(Diagnostic::new(
                            instruction.span.unwrap_or_else(fallback_span),
                            "unsupported float-to-integer conversion",
                        ));
                    }
                };
                self.line(format!("calln {symbol}"));
                if is_64(to) {
                    self.store_value_pair(result, "2x", "1x");
                } else {
                    if to.integer_bits().unwrap_or(32) < 32 {
                        self.line("andn 1x 0xffffffff");
                    }
                    self.store_value_word(result, "1x");
                }
            }
            _ => {
                return Err(Diagnostic::new(
                    instruction.span.unwrap_or_else(fallback_span),
                    "conversion between these types is not supported by the Shy backend",
                ));
            }
        }
        Ok(())
    }

    /// Get an integer operand into `1x`/`2x` as a sign- or zero-extended
    /// 64-bit value, which is what the runtime's integer helpers expect.
    fn widen_for_float(
        &mut self,
        value: &Operand,
        from: Type,
        signed: bool,
    ) -> Result<(), Diagnostic> {
        self.load_operand(value, "1x", if is_64(from) { Some("2x") } else { None })?;
        if is_64(from) {
            return Ok(());
        }
        self.line("setn 2x 0");
        if from != Type::I1 && signed {
            let done = self.fresh("float_sext");
            self.line("seta ex 1x");
            self.line("rsn ex 31");
            self.line("equn ex 0");
            self.line(format!("jmpn {done}"));
            self.line("setn 2x 0xffffffff");
            self.line(format!("{done}:"));
        }
        Ok(())
    }

    pub(crate) fn cast(
        &mut self,
        instruction: &Inst,
        op: CastOp,
        value: &Operand,
        to: Type,
    ) -> Result<(), Diagnostic> {
        let from = self.operand_type(value);
        if from.is_float() || to.is_float() {
            return self.cast_float(instruction, op, value, from, to);
        }
        if from.is_aggregate() || to.is_aggregate() || from.is_void() || to.is_void() {
            return Err(Diagnostic::new(
                instruction.span.unwrap_or_else(fallback_span),
                "cast between these types is not supported by the Shy backend",
            ));
        }
        let result = result(instruction)?;
        let from_wide = is_64(from);
        let to_wide = is_64(to);
        match op {
            CastOp::ZExt => {
                self.load_operand(value, "1x", if from_wide { Some("2x") } else { None })?;
                if to_wide && !from_wide {
                    self.line("setn 2x 0");
                }
            }
            CastOp::SExt => {
                self.load_operand(value, "1x", if from_wide { Some("2x") } else { None })?;
                if !from_wide {
                    self.sign_extend("1x", from);
                }
                if to_wide && !from_wide {
                    let done = self.fresh("sext_high");
                    self.line("setn 2x 0");
                    self.line("seta ex 1x");
                    self.line("rsn ex 31");
                    self.line("equn ex 0");
                    self.line(format!("jmpn {done}"));
                    self.line("setn 2x 0xffffffff");
                    self.line(format!("{done}:"));
                }
            }
            CastOp::Trunc => {
                self.load_operand(value, "1x", None)?;
            }
            CastOp::Bitcast => {
                if from_wide != to_wide {
                    return Err(Diagnostic::new(
                        instruction.span.unwrap_or_else(fallback_span),
                        "bitcast between different widths is not supported by the Shy backend",
                    ));
                }
                self.load_operand(value, "1x", if to_wide { Some("2x") } else { None })?;
            }
            CastOp::PtrToInt => {
                self.load_operand(value, "1x", None)?;
                if to_wide {
                    self.line("setn 2x 0");
                }
            }
            CastOp::IntToPtr => {
                self.load_operand(value, "1x", None)?;
            }
            _ => {
                return Err(Diagnostic::new(
                    instruction.span.unwrap_or_else(fallback_span),
                    format!("cast {op} requires the unsupported float path"),
                ));
            }
        }
        if to_wide {
            self.store_value_pair(result, "2x", "1x");
        } else {
            self.mask_to("1x", to);
            self.store_value_word(result, "1x");
        }
        Ok(())
    }
}
