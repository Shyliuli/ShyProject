//! 64-bit helper routines emitted once per function that needs them.
//!
//! ShyISA has no 64-bit ALU. The backend lowers 64-bit addition and
//! subtraction directly, and calls these local routines for multiplication,
//! shifts, division, and remainder. The routines use the same internal
//! convention as an instruction body: the first operand is `1x`/`2x`, the
//! second is `3x`/`cx`, and the result replaces `1x`/`2x`.
//!
//! The routines are local labels in the function's own `text.<symbol>`
//! section. They add no exported symbol and cannot collide with another object
//! file, and they are emitted after the function's `ret`, so they are only
//! reachable through `calln`.

use super::function::FnWriter;

impl FnWriter<'_> {
    pub(crate) fn runtime_helpers(&mut self) {
        let need_divmod = self.helpers.u64_div
            || self.helpers.u64_mod
            || self.helpers.i64_div
            || self.helpers.i64_mod;
        if need_divmod {
            self.emit_u64_divmod();
        }
        if self.helpers.u64_div || self.helpers.i64_div {
            self.emit_u64_div();
        }
        if self.helpers.u64_mod || self.helpers.i64_mod {
            self.emit_u64_mod();
        }
        if self.helpers.u64_mul {
            self.emit_u64_mul();
        }
        if self.helpers.u64_shl {
            self.emit_u64_shl();
        }
        if self.helpers.u64_lshr {
            self.emit_u64_shr(true);
        }
        if self.helpers.u64_ashr {
            self.emit_u64_shr(false);
        }
        if self.helpers.i64_div {
            self.emit_i64_div();
        }
        if self.helpers.i64_mod {
            self.emit_i64_mod();
        }
    }

    fn entry(&mut self, name: &str) {
        let label = self.helper_label(name);
        self.line(format!("{label}:"));
    }

    /// Quotient in `1x`/`2x`, remainder in `6x`/`7x`.
    ///
    /// This is a restoring long division over 64 bits. Division by zero is not
    /// a trap on ShyISA; like the ISA's 32-bit `diva`, it produces all ones.
    fn emit_u64_divmod(&mut self) {
        self.entry("u64divmod");
        let loop_label = self.fresh("u64divmod_loop");
        let subtract = self.fresh("u64divmod_sub");
        let skip = self.fresh("u64divmod_skip");
        let borrow = self.fresh("u64divmod_borrow");
        let borrow_done = self.fresh("u64divmod_borrow_done");
        let divide_zero = self.fresh("u64divmod_zero");

        self.line("seta dx 3x");
        self.line("ora dx cx");
        self.line("equn dx 0");
        self.line(format!("jmpn {divide_zero}"));
        self.line("setn 4x 0");
        self.line("setn 5x 0");
        self.line("setn 6x 0");
        self.line("setn 7x 0");
        self.line("setn 8x 64");
        self.line(format!("{loop_label}:"));
        self.line("seta 9x 4x");
        self.line("rsn 9x 31");
        self.line("lsn 4x 1");
        self.line("lsn 5x 1");
        self.line("adda 5x 9x");
        self.line("seta 9x 6x");
        self.line("rsn 9x 31");
        self.line("lsn 6x 1");
        self.line("lsn 7x 1");
        self.line("adda 7x 9x");
        self.line("seta 9x 2x");
        self.line("rsn 9x 31");
        self.line("adda 6x 9x");
        self.line("seta 9x 1x");
        self.line("rsn 9x 31");
        self.line("lsn 1x 1");
        self.line("lsn 2x 1");
        self.line("adda 2x 9x");
        self.line("biga 7x cx");
        self.line(format!("jmpn {subtract}"));
        self.line("smaa 7x cx");
        self.line(format!("jmpn {skip}"));
        self.line("bigequa 6x 3x");
        self.line(format!("jmpn {subtract}"));
        self.line(format!("ujmpn {skip}"));
        self.line(format!("{subtract}:"));
        self.line("seta 9x 6x");
        self.line("suba 6x 3x");
        self.line("smaa 9x 3x");
        self.line("setn ax 0");
        self.line(format!("jmpn {borrow}"));
        self.line(format!("ujmpn {borrow_done}"));
        self.line(format!("{borrow}:"));
        self.line("setn ax 1");
        self.line(format!("{borrow_done}:"));
        self.line("suba 7x cx");
        self.line("suba 7x ax");
        self.line("addn 4x 1");
        self.line(format!("{skip}:"));
        self.line("subn 8x 1");
        self.line("bign 8x 0");
        self.line(format!("jmpn {loop_label}"));
        self.line("seta 1x 4x");
        self.line("seta 2x 5x");
        self.line("ret");
        self.line(format!("{divide_zero}:"));
        self.line("setn 1x 0xffffffff");
        self.line("setn 2x 0xffffffff");
        self.line("setn 6x 0xffffffff");
        self.line("setn 7x 0xffffffff");
        self.line("ret");
    }

    fn emit_u64_div(&mut self) {
        self.entry("u64div");
        let target = self.helper_label("u64divmod");
        self.line(format!("calln {target}"));
        self.line("ret");
    }

    fn emit_u64_mod(&mut self) {
        self.entry("u64mod");
        let target = self.helper_label("u64divmod");
        self.line(format!("calln {target}"));
        self.line("seta 1x 6x");
        self.line("seta 2x 7x");
        self.line("ret");
    }

    fn emit_u64_mul(&mut self) {
        self.entry("u64mul");
        let loop_label = self.fresh("u64mul_loop");
        let done = self.fresh("u64mul_done");
        let skip = self.fresh("u64mul_skip");
        let carry = self.fresh("u64mul_carry");
        let carry_done = self.fresh("u64mul_carry_done");
        self.line("setn 4x 0");
        self.line("setn 5x 0");
        self.line(format!("{loop_label}:"));
        self.line("seta 6x 3x");
        self.line("ora 6x cx");
        self.line("equn 6x 0");
        self.line(format!("jmpn {done}"));
        self.line("seta 6x 3x");
        self.line("andn 6x 1");
        self.line("equn 6x 0");
        self.line(format!("jmpn {skip}"));
        self.line("seta 6x 4x");
        self.line("adda 4x 1x");
        self.line("smaa 4x 6x");
        self.line("setn 7x 0");
        self.line(format!("jmpn {carry}"));
        self.line(format!("ujmpn {carry_done}"));
        self.line(format!("{carry}:"));
        self.line("setn 7x 1");
        self.line(format!("{carry_done}:"));
        self.line("adda 5x 2x");
        self.line("adda 5x 7x");
        self.line(format!("{skip}:"));
        self.line("seta 6x 1x");
        self.line("rsn 6x 31");
        self.line("lsn 1x 1");
        self.line("lsn 2x 1");
        self.line("adda 2x 6x");
        self.line("seta 6x cx");
        self.line("andn 6x 1");
        self.line("lsn 6x 31");
        self.line("rsn cx 1");
        self.line("rsn 3x 1");
        self.line("adda 3x 6x");
        self.line(format!("ujmpn {loop_label}"));
        self.line(format!("{done}:"));
        self.line("seta 1x 4x");
        self.line("seta 2x 5x");
        self.line("ret");
    }

    fn emit_u64_shl(&mut self) {
        self.entry("u64shl");
        let check = self.fresh("u64shl_check");
        let loop_label = self.fresh("u64shl_loop");
        let done = self.fresh("u64shl_done");
        self.line("equn cx 0");
        self.line(format!("jmpn {check}"));
        self.line("setn 1x 0");
        self.line("setn 2x 0");
        self.line(format!("ujmpn {done}"));
        self.line(format!("{check}:"));
        self.line("smaequn 3x 63");
        self.line(format!("jmpn {loop_label}"));
        self.line("setn 1x 0");
        self.line("setn 2x 0");
        self.line(format!("ujmpn {done}"));
        self.line(format!("{loop_label}:"));
        self.line("equn 3x 0");
        self.line(format!("jmpn {done}"));
        self.line("seta dx 1x");
        self.line("rsn dx 31");
        self.line("lsn 1x 1");
        self.line("lsn 2x 1");
        self.line("ora 2x dx");
        self.line("subn 3x 1");
        self.line(format!("ujmpn {loop_label}"));
        self.line(format!("{done}:"));
        self.line("ret");
    }

    fn emit_u64_shr(&mut self, unsigned: bool) {
        self.entry(if unsigned { "u64lshr" } else { "u64ashr" });
        let check = self.fresh("u64shr_check");
        let loop_label = self.fresh("u64shr_loop");
        let done = self.fresh("u64shr_done");
        let large_pos = self.fresh("u64shr_large_pos");
        let big_pos = self.fresh("u64shr_big_pos");
        let sign_done = self.fresh("u64shr_sign_done");

        let fill_sign = |writer: &mut Self, positive: &str| {
            writer.line("seta dx 2x");
            writer.line("rsn dx 31");
            writer.line("equn dx 0");
            writer.line(format!("jmpn {positive}"));
            writer.line("setn 1x 0xffffffff");
            writer.line("setn 2x 0xffffffff");
            writer.line(format!("ujmpn {done}"));
            writer.line(format!("{positive}:"));
            writer.line("setn 1x 0");
            writer.line("setn 2x 0");
        };

        self.line("equn cx 0");
        self.line(format!("jmpn {check}"));
        if unsigned {
            self.line("setn 1x 0");
            self.line("setn 2x 0");
        } else {
            fill_sign(self, &big_pos);
        }
        self.line(format!("ujmpn {done}"));
        self.line(format!("{check}:"));
        self.line("smaequn 3x 63");
        self.line(format!("jmpn {loop_label}"));
        if unsigned {
            self.line("setn 1x 0");
            self.line("setn 2x 0");
        } else {
            fill_sign(self, &large_pos);
        }
        self.line(format!("ujmpn {done}"));
        self.line(format!("{loop_label}:"));
        self.line("equn 3x 0");
        self.line(format!("jmpn {done}"));
        self.line("seta dx 2x");
        self.line("andn dx 1");
        self.line("lsn dx 31");
        if !unsigned {
            self.line("seta 4x 2x");
            self.line("andn 4x 0x80000000");
        }
        self.line("rsn 2x 1");
        if !unsigned {
            self.line("equn 4x 0");
            self.line(format!("jmpn {sign_done}"));
            self.line("setn 4x 0x80000000");
            self.line("ora 2x 4x");
            self.line(format!("{sign_done}:"));
        }
        self.line("rsn 1x 1");
        self.line("ora 1x dx");
        self.line("subn 3x 1");
        self.line(format!("ujmpn {loop_label}"));
        self.line(format!("{done}:"));
        self.line("ret");
    }

    /// Signed division wrapper: take absolute values, divide unsigned, then
    /// negate the quotient when the operand signs differ.
    fn emit_i64_div(&mut self) {
        self.entry("i64div");
        let lhs_abs = self.fresh("i64div_lhs_abs");
        let lhs_carry = self.fresh("i64div_lhs_carry");
        let rhs_abs = self.fresh("i64div_rhs_abs");
        let rhs_carry = self.fresh("i64div_rhs_carry");
        let result_carry = self.fresh("i64div_result_carry");
        let done = self.fresh("i64div_done");
        let unsigned_div = self.helper_label("u64div");

        self.line("seta 8x 2x");
        self.line("rsn 8x 31");
        self.line("seta 9x cx");
        self.line("rsn 9x 31");
        self.line("xora 8x 9x");
        self.line("pusha 8x");
        self.line("seta 9x 2x");
        self.line("rsn 9x 31");
        self.line("equn 9x 0");
        self.line(format!("jmpn {lhs_abs}"));
        self.line("nota 1x");
        self.line("nota 2x");
        self.line("addn 1x 1");
        self.line("equn 1x 0");
        self.line(format!("jmpn {lhs_carry}"));
        self.line(format!("ujmpn {lhs_abs}"));
        self.line(format!("{lhs_carry}:"));
        self.line("addn 2x 1");
        self.line(format!("{lhs_abs}:"));
        self.line("seta 9x cx");
        self.line("rsn 9x 31");
        self.line("equn 9x 0");
        self.line(format!("jmpn {rhs_abs}"));
        self.line("nota 3x");
        self.line("nota cx");
        self.line("addn 3x 1");
        self.line("equn 3x 0");
        self.line(format!("jmpn {rhs_carry}"));
        self.line(format!("ujmpn {rhs_abs}"));
        self.line(format!("{rhs_carry}:"));
        self.line("addn cx 1");
        self.line(format!("{rhs_abs}:"));
        self.line(format!("calln {unsigned_div}"));
        self.line("popa 8x");
        self.line("equn 8x 0");
        self.line(format!("jmpn {done}"));
        self.line("nota 1x");
        self.line("nota 2x");
        self.line("addn 1x 1");
        self.line("equn 1x 0");
        self.line(format!("jmpn {result_carry}"));
        self.line(format!("ujmpn {done}"));
        self.line(format!("{result_carry}:"));
        self.line("addn 2x 1");
        self.line(format!("{done}:"));
        self.line("ret");
    }

    /// Signed remainder wrapper: the remainder keeps the dividend's sign.
    fn emit_i64_mod(&mut self) {
        self.entry("i64mod");
        let lhs_abs = self.fresh("i64mod_lhs_abs");
        let lhs_carry = self.fresh("i64mod_lhs_carry");
        let rhs_abs = self.fresh("i64mod_rhs_abs");
        let rhs_carry = self.fresh("i64mod_rhs_carry");
        let result_carry = self.fresh("i64mod_result_carry");
        let done = self.fresh("i64mod_done");
        let unsigned_mod = self.helper_label("u64mod");

        self.line("seta 8x 2x");
        self.line("rsn 8x 31");
        self.line("pusha 8x");
        self.line("equn 8x 0");
        self.line(format!("jmpn {lhs_abs}"));
        self.line("nota 1x");
        self.line("nota 2x");
        self.line("addn 1x 1");
        self.line("equn 1x 0");
        self.line(format!("jmpn {lhs_carry}"));
        self.line(format!("ujmpn {lhs_abs}"));
        self.line(format!("{lhs_carry}:"));
        self.line("addn 2x 1");
        self.line(format!("{lhs_abs}:"));
        self.line("seta 9x cx");
        self.line("rsn 9x 31");
        self.line("equn 9x 0");
        self.line(format!("jmpn {rhs_abs}"));
        self.line("nota 3x");
        self.line("nota cx");
        self.line("addn 3x 1");
        self.line("equn 3x 0");
        self.line(format!("jmpn {rhs_carry}"));
        self.line(format!("ujmpn {rhs_abs}"));
        self.line(format!("{rhs_carry}:"));
        self.line("addn cx 1");
        self.line(format!("{rhs_abs}:"));
        self.line(format!("calln {unsigned_mod}"));
        self.line("popa 8x");
        self.line("equn 8x 0");
        self.line(format!("jmpn {done}"));
        self.line("nota 1x");
        self.line("nota 2x");
        self.line("addn 1x 1");
        self.line("equn 1x 0");
        self.line(format!("jmpn {result_carry}"));
        self.line(format!("ujmpn {done}"));
        self.line(format!("{result_carry}:"));
        self.line("addn 2x 1");
        self.line(format!("{done}:"));
        self.line("ret");
    }
}
