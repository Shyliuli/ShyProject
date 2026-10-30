//! Typed integers for `#if` evaluation.
//!
//! C evaluates preprocessor expressions as if every signed integer type were
//! `intmax_t` and every unsigned integer type were `uintmax_t`. [`Value`]
//! stores the 64-bit representation plus the signedness tag.
//!
//! Binary operators apply the usual arithmetic conversions: if either operand
//! is unsigned, both operands are interpreted as `uintmax_t`, so comparison,
//! division, remainder, and right shift are unsigned. The result of a
//! comparison, `&&`, `||`, or `!` is signed.
//!
//! The `live` flag comes from short-circuit evaluation. Operands in a dead
//! branch are still parsed and typed because `?:` and the usual arithmetic
//! conversions need both branch types, but division by zero and invalid shift
//! counts are only diagnosed in a live branch.

use crate::source::{Diagnostic, Span};

#[derive(Clone, Copy, Debug)]
pub(crate) struct Value {
    pub(crate) bits: u64,
    pub(crate) unsigned: bool,
}

impl Value {
    pub(crate) fn signed(value: i64) -> Self {
        Self {
            bits: value as u64,
            unsigned: false,
        }
    }

    pub(crate) fn boolean(value: bool) -> Self {
        Self::signed(if value { 1 } else { 0 })
    }

    pub(crate) fn from_parts(bits: u64, unsigned: bool) -> Self {
        Self { bits, unsigned }
    }

    pub(crate) fn truthy(self) -> bool {
        self.bits != 0
    }
}

#[derive(Clone, Copy)]
pub(crate) enum UnaryOp {
    Plus,
    Minus,
    BitNot,
    Not,
}

pub(crate) fn unary(operator: UnaryOp, value: Value, live: bool) -> Value {
    match operator {
        UnaryOp::Plus => value,
        UnaryOp::Minus => Value::from_parts(
            if live {
                0u64.wrapping_sub(value.bits)
            } else {
                0
            },
            value.unsigned,
        ),
        UnaryOp::BitNot => Value::from_parts(if live { !value.bits } else { 0 }, value.unsigned),
        UnaryOp::Not => Value::boolean(live && !value.truthy()),
    }
}

#[derive(Clone, Copy)]
pub(crate) enum BinaryOp {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    BitAnd,
    BitOr,
    BitXor,
    Shl,
    Shr,
    Less,
    LessEqual,
    Greater,
    GreaterEqual,
    Equal,
    NotEqual,
}

pub(crate) fn binary(
    operator: BinaryOp,
    left: Value,
    right: Value,
    live: bool,
    span: Span,
) -> Result<Value, Diagnostic> {
    let unsigned = left.unsigned || right.unsigned;
    match operator {
        BinaryOp::Add => Ok(arithmetic(
            left.bits.wrapping_add(right.bits),
            unsigned,
            live,
        )),
        BinaryOp::Sub => Ok(arithmetic(
            left.bits.wrapping_sub(right.bits),
            unsigned,
            live,
        )),
        BinaryOp::Mul => Ok(arithmetic(
            left.bits.wrapping_mul(right.bits),
            unsigned,
            live,
        )),
        BinaryOp::Div => divide(left, right, unsigned, live, span, false),
        BinaryOp::Rem => divide(left, right, unsigned, live, span, true),
        BinaryOp::BitAnd => Ok(arithmetic(left.bits & right.bits, unsigned, live)),
        BinaryOp::BitOr => Ok(arithmetic(left.bits | right.bits, unsigned, live)),
        BinaryOp::BitXor => Ok(arithmetic(left.bits ^ right.bits, unsigned, live)),
        BinaryOp::Shl => shift(left, right, unsigned, live, span, true),
        BinaryOp::Shr => shift(left, right, unsigned, live, span, false),
        BinaryOp::Less | BinaryOp::LessEqual | BinaryOp::Greater | BinaryOp::GreaterEqual => {
            if !live {
                return Ok(Value::signed(0));
            }
            let truth = compare(operator, left, right, unsigned);
            Ok(Value::boolean(truth))
        }
        BinaryOp::Equal | BinaryOp::NotEqual => {
            if !live {
                return Ok(Value::signed(0));
            }
            let equal = left.bits == right.bits;
            Ok(Value::boolean(if matches!(operator, BinaryOp::Equal) {
                equal
            } else {
                !equal
            }))
        }
    }
}

fn arithmetic(bits: u64, unsigned: bool, live: bool) -> Value {
    Value::from_parts(if live { bits } else { 0 }, unsigned)
}

fn divide(
    left: Value,
    right: Value,
    unsigned: bool,
    live: bool,
    span: Span,
    remainder: bool,
) -> Result<Value, Diagnostic> {
    if !live {
        // The result type still matters to a surrounding `?:`.
        return Ok(Value::from_parts(0, unsigned));
    }
    let bits = if unsigned {
        if right.bits == 0 {
            return Err(zero_error(span, remainder));
        }
        if remainder {
            left.bits % right.bits
        } else {
            left.bits / right.bits
        }
    } else {
        let divisor = right.bits as i64;
        if divisor == 0 {
            return Err(zero_error(span, remainder));
        }
        let dividend = left.bits as i64;
        if remainder {
            dividend.wrapping_rem(divisor) as u64
        } else {
            dividend.wrapping_div(divisor) as u64
        }
    };
    Ok(Value::from_parts(bits, unsigned))
}

fn zero_error(span: Span, remainder: bool) -> Diagnostic {
    let message = if remainder {
        "remainder by zero in #if expression"
    } else {
        "division by zero in #if expression"
    };
    Diagnostic::new(span, message)
}

fn shift(
    left: Value,
    right: Value,
    unsigned: bool,
    live: bool,
    span: Span,
    left_shift: bool,
) -> Result<Value, Diagnostic> {
    if !live {
        return Ok(Value::from_parts(0, unsigned));
    }
    let count = if right.unsigned {
        if right.bits >= 64 {
            return Err(Diagnostic::new(
                span,
                "shift count out of range in #if expression",
            ));
        }
        right.bits as u32
    } else {
        let signed = right.bits as i64;
        if !(0..64).contains(&signed) {
            return Err(Diagnostic::new(
                span,
                "shift count out of range in #if expression",
            ));
        }
        signed as u32
    };
    let bits = if left_shift {
        left.bits.wrapping_shl(count)
    } else if unsigned {
        left.bits >> count
    } else {
        (left.bits as i64).wrapping_shr(count) as u64
    };
    Ok(Value::from_parts(bits, unsigned))
}

fn compare(operator: BinaryOp, left: Value, right: Value, unsigned: bool) -> bool {
    if unsigned {
        match operator {
            BinaryOp::Less => left.bits < right.bits,
            BinaryOp::LessEqual => left.bits <= right.bits,
            BinaryOp::Greater => left.bits > right.bits,
            _ => left.bits >= right.bits,
        }
    } else {
        let left = left.bits as i64;
        let right = right.bits as i64;
        match operator {
            BinaryOp::Less => left < right,
            BinaryOp::LessEqual => left <= right,
            BinaryOp::Greater => left > right,
            _ => left >= right,
        }
    }
}
