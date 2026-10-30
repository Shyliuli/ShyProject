//! Integer comparisons for the Shy backend.

use super::fallback_span;
use super::frame::is_64;
use super::function::FnWriter;
use super::inst::result;
use crate::ir::{FloatPredicate, Inst, IntPredicate, Operand, Type};
use crate::source::Diagnostic;

impl FnWriter<'_> {
    // Comparisons
    // -----------------------------------------------------------------------

    /// Floating-point comparison through the soft-float runtime.
    ///
    /// The helpers take raw bit patterns and return 0 or 1, so an `f32` needs
    /// one argument slot and an `f64` an ordinary 64-bit pair. `>` and `>=`
    /// are the `<` and `<=` helpers with the operands exchanged.
    pub(crate) fn fcmp(
        &mut self,
        instruction: &Inst,
        pred: FloatPredicate,
        lhs: &Operand,
        rhs: &Operand,
        ty: Type,
    ) -> Result<(), Diagnostic> {
        let prefix = if ty == Type::F32 { "f32" } else { "f64" };
        let (name, first, second) = match pred {
            FloatPredicate::Eq => ("eq", lhs, rhs),
            FloatPredicate::Ne => ("ne", lhs, rhs),
            FloatPredicate::Lt => ("lt", lhs, rhs),
            FloatPredicate::Le => ("le", lhs, rhs),
            FloatPredicate::Gt => ("lt", rhs, lhs),
            FloatPredicate::Ge => ("le", rhs, lhs),
        };
        let wide = is_64(ty);
        if wide {
            self.load_operand(first, "4x", Some("5x"))?;
            self.load_operand(second, "6x", Some("7x"))?;
        } else {
            self.load_operand(first, "4x", None)?;
            self.load_operand(second, "5x", None)?;
        }
        self.line(format!("calln __shy_{prefix}_{name}"));
        self.line("andn 1x 1");
        let result = result(instruction)?;
        self.store_value_word(result, "1x");
        Ok(())
    }

    pub(crate) fn icmp(
        &mut self,
        instruction: &Inst,
        pred: IntPredicate,
        lhs: &Operand,
        rhs: &Operand,
        ty: Type,
    ) -> Result<(), Diagnostic> {
        if ty.is_float() {
            return Err(Diagnostic::new(
                instruction.span.unwrap_or_else(fallback_span),
                "floating-point comparisons are not implemented by the Shy backend",
            ));
        }
        if ty.is_aggregate() || ty.is_void() {
            return Err(Diagnostic::new(
                instruction.span.unwrap_or_else(fallback_span),
                format!("comparison on unsupported type {ty}"),
            ));
        }
        let result = result(instruction)?;
        let wide = is_64(ty);
        self.load_operand(lhs, "1x", if wide { Some("2x") } else { None })?;
        self.load_operand(rhs, "3x", if wide { Some("cx") } else { None })?;
        if wide {
            self.icmp_wide(pred);
        } else {
            self.icmp_narrow(pred, ty);
        }
        self.store_value_word(result, "1x");
        Ok(())
    }

    fn icmp_narrow(&mut self, pred: IntPredicate, ty: Type) {
        if matches!(
            pred,
            IntPredicate::Slt | IntPredicate::Sle | IntPredicate::Sgt | IntPredicate::Sge
        ) {
            self.sign_extend("1x", ty);
            self.sign_extend("3x", ty);
        }
        match pred {
            IntPredicate::Eq => {
                self.line("equa 1x 3x");
                self.bool_from_rs(false);
            }
            IntPredicate::Ne => {
                self.line("equa 1x 3x");
                self.bool_from_rs(true);
            }
            IntPredicate::Ult => {
                self.line("smaa 1x 3x");
                self.bool_from_rs(false);
            }
            IntPredicate::Ule => {
                self.line("smaequa 1x 3x");
                self.bool_from_rs(false);
            }
            IntPredicate::Ugt => {
                self.line("biga 1x 3x");
                self.bool_from_rs(false);
            }
            IntPredicate::Uge => {
                self.line("bigequa 1x 3x");
                self.bool_from_rs(false);
            }
            IntPredicate::Slt => self.signed_less_32("1x", "3x", false),
            IntPredicate::Sgt => self.signed_less_32("3x", "1x", false),
            IntPredicate::Sle => self.signed_less_32("3x", "1x", true),
            IntPredicate::Sge => self.signed_less_32("1x", "3x", true),
        }
    }

    fn icmp_wide(&mut self, pred: IntPredicate) {
        match pred {
            IntPredicate::Eq => {
                let high_equal = self.fresh("eq");
                let true_label = self.fresh("eq_true");
                let end = self.fresh("eq_end");
                self.line("equa 2x cx");
                self.line(format!("jmpn {high_equal}"));
                self.line("setn 1x 0");
                self.line(format!("ujmpn {end}"));
                self.line(format!("{high_equal}:"));
                self.line("equa 1x 3x");
                self.line(format!("jmpn {true_label}"));
                self.line("setn 1x 0");
                self.line(format!("ujmpn {end}"));
                self.line(format!("{true_label}:"));
                self.line("setn 1x 1");
                self.line(format!("{end}:"));
            }
            IntPredicate::Ne => {
                self.icmp_wide(IntPredicate::Eq);
                self.line("xorn 1x 1");
            }
            IntPredicate::Ult => self.unsigned_less_64("2x", "1x", "cx", "3x", false),
            IntPredicate::Ugt => self.unsigned_less_64("cx", "3x", "2x", "1x", false),
            IntPredicate::Ule => self.unsigned_less_64("cx", "3x", "2x", "1x", true),
            IntPredicate::Uge => self.unsigned_less_64("2x", "1x", "cx", "3x", true),
            IntPredicate::Slt => self.signed_less_64("2x", "1x", "cx", "3x", false),
            IntPredicate::Sgt => self.signed_less_64("cx", "3x", "2x", "1x", false),
            IntPredicate::Sle => self.signed_less_64("cx", "3x", "2x", "1x", true),
            IntPredicate::Sge => self.signed_less_64("2x", "1x", "cx", "3x", true),
        }
    }

    fn bool_from_rs(&mut self, invert: bool) {
        let true_label = self.fresh("bool");
        let end = self.fresh("bool_end");
        self.line("setn 1x 0");
        if invert {
            self.line(format!("jmpn {end}"));
            self.line("setn 1x 1");
        } else {
            self.line(format!("jmpn {true_label}"));
            self.line(format!("ujmpn {end}"));
            self.line(format!("{true_label}:"));
            self.line("setn 1x 1");
        }
        self.line(format!("{end}:"));
    }

    /// `lhs < rhs` for 32-bit signed values, materialised in `1x`.
    fn signed_less_32(&mut self, lhs: &str, rhs: &str, invert: bool) {
        let same = self.fresh("scmp_same");
        let differ_true = self.fresh("scmp_true");
        let differ_false = self.fresh("scmp_false");
        let end = self.fresh("scmp_end");
        self.line(format!("seta dx {lhs}"));
        self.line("rsn dx 31");
        self.line(format!("seta ex {rhs}"));
        self.line("rsn ex 31");
        self.line("equa dx ex");
        self.line(format!("jmpn {same}"));
        self.line("equn dx 1");
        self.line(format!("jmpn {differ_true}"));
        self.line(format!("ujmpn {differ_false}"));
        self.line(format!("{differ_true}:"));
        self.line(format!("setn 1x {}", if invert { 0 } else { 1 }));
        self.line(format!("ujmpn {end}"));
        self.line(format!("{differ_false}:"));
        self.line(format!("setn 1x {}", if invert { 1 } else { 0 }));
        self.line(format!("ujmpn {end}"));
        self.line(format!("{same}:"));
        self.line(format!("smaa {lhs} {rhs}"));
        self.bool_from_rs(invert);
        self.line(format!("{end}:"));
    }

    fn unsigned_less_64(
        &mut self,
        lhs_high: &str,
        lhs_low: &str,
        rhs_high: &str,
        rhs_low: &str,
        invert: bool,
    ) {
        let true_label = self.fresh("ucmp_true");
        let false_label = self.fresh("ucmp_false");
        let end = self.fresh("ucmp_end");
        self.line(format!("smaa {lhs_high} {rhs_high}"));
        self.line(format!("jmpn {true_label}"));
        self.line(format!("biga {lhs_high} {rhs_high}"));
        self.line(format!("jmpn {false_label}"));
        self.line(format!("smaa {lhs_low} {rhs_low}"));
        self.line(format!("jmpn {true_label}"));
        self.line(format!("ujmpn {false_label}"));
        self.line(format!("{true_label}:"));
        self.line(format!("setn 1x {}", if invert { 0 } else { 1 }));
        self.line(format!("ujmpn {end}"));
        self.line(format!("{false_label}:"));
        self.line(format!("setn 1x {}", if invert { 1 } else { 0 }));
        self.line(format!("{end}:"));
    }

    fn signed_less_64(
        &mut self,
        lhs_high: &str,
        lhs_low: &str,
        rhs_high: &str,
        rhs_low: &str,
        invert: bool,
    ) {
        let same = self.fresh("scmp_same");
        let differ_true = self.fresh("scmp_true");
        let differ_false = self.fresh("scmp_false");
        let end = self.fresh("scmp_end");
        self.line(format!("seta dx {lhs_high}"));
        self.line("rsn dx 31");
        self.line(format!("seta ex {rhs_high}"));
        self.line("rsn ex 31");
        self.line("equa dx ex");
        self.line(format!("jmpn {same}"));
        self.line("equn dx 1");
        self.line(format!("jmpn {differ_true}"));
        self.line(format!("ujmpn {differ_false}"));
        self.line(format!("{differ_true}:"));
        self.line(format!("setn 1x {}", if invert { 0 } else { 1 }));
        self.line(format!("ujmpn {end}"));
        self.line(format!("{differ_false}:"));
        self.line(format!("setn 1x {}", if invert { 1 } else { 0 }));
        self.line(format!("ujmpn {end}"));
        self.line(format!("{same}:"));
        self.unsigned_less_64(lhs_high, lhs_low, rhs_high, rhs_low, invert);
        self.line(format!("{end}:"));
    }

    pub(crate) fn sign_extend(&mut self, register: &str, ty: Type) {
        if ty == Type::I1 {
            let zero = self.fresh("sext1");
            self.line(format!("andn {register} 1"));
            self.line(format!("equn {register} 0"));
            self.line(format!("jmpn {zero}"));
            self.line(format!("setn {register} 0xffffffff"));
            self.line(format!("{zero}:"));
            return;
        }
        let (limit, mask) = match ty {
            Type::I8 => (127, 0xffff_ff00u32),
            Type::I16 => (32767, 0xffff_0000u32),
            _ => return,
        };
        let sign = self.fresh("sext");
        let done = self.fresh("sext_done");
        self.line(format!("bign {register} {limit}"));
        self.line(format!("jmpn {sign}"));
        self.line(format!("ujmpn {done}"));
        self.line(format!("{sign}:"));
        self.line(format!("orn {register} 0x{mask:x}"));
        self.line(format!("{done}:"));
    }
}
