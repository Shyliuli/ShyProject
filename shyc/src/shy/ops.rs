//! Arithmetic, casts, and integer conversions for the Shy backend.

use super::fallback_span;
use super::frame::is_64;
use super::function::FnWriter;
use super::inst::result;
use crate::ir::{BinaryOp, CastOp, Inst, Operand, Type, UnaryOp};
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
            return Err(Diagnostic::new(
                instruction.span.unwrap_or_else(fallback_span),
                "floating-point arithmetic is not implemented by the Shy backend",
            ));
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

    pub(crate) fn cast(
        &mut self,
        instruction: &Inst,
        op: CastOp,
        value: &Operand,
        to: Type,
    ) -> Result<(), Diagnostic> {
        let from = self.operand_type(value);
        if from.is_float() || to.is_float() {
            return Err(Diagnostic::new(
                instruction.span.unwrap_or_else(fallback_span),
                "floating-point conversions are not implemented by the Shy backend",
            ));
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
