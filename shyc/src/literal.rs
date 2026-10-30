//! Decode literal spellings after preprocessing, using ShyC's target widths.
//! Integer ranks are kept even when `long` and `long long` have the same width.
use crate::source::{Diagnostic, Span};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IntegerRank {
    Int,
    Long,
    LongLong,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Integer {
    pub bits: u64,
    pub rank: IntegerRank,
    pub unsigned: bool,
}

impl Integer {
    pub fn width(self) -> u32 {
        match self.rank {
            IntegerRank::Int => 32,
            IntegerRank::Long | IntegerRank::LongLong => 64,
        }
    }
}

pub fn integer(text: &str, span: Span) -> Result<Integer, Diagnostic> {
    let chars: Vec<char> = text.chars().collect();
    for (index, &c) in chars.iter().enumerate() {
        if c == '\''
            && (index == 0
                || index + 1 == chars.len()
                || !chars[index - 1].is_ascii_hexdigit()
                || !chars[index + 1].is_ascii_hexdigit())
        {
            return Err(Diagnostic::new(
                span,
                "digit separator must be between digits",
            ));
        }
    }
    let spelling = text.replace('\'', "");
    let (base, first) = if spelling.starts_with("0x") || spelling.starts_with("0X") {
        (16, 2)
    } else if spelling.starts_with("0b") || spelling.starts_with("0B") {
        (2, 2)
    } else if spelling.starts_with('0') && spelling.len() > 1 {
        (8, 0)
    } else {
        (10, 0)
    };
    let end = spelling[first..]
        .char_indices()
        .take_while(|(_, c)| c.is_digit(base))
        .last()
        .map(|(offset, c)| first + offset + c.len_utf8())
        .unwrap_or(first);
    if end == first {
        return Err(Diagnostic::new(span, "integer literal has no digits"));
    }
    let value = u64::from_str_radix(&spelling[first..end], base)
        .map_err(|_| Diagnostic::new(span, "integer literal exceeds 64 bits"))?;
    let suffix = &spelling[end..];
    let lower = suffix.to_ascii_lowercase();
    let (unsigned, minimum) = match lower.as_str() {
        "" => (false, IntegerRank::Int),
        "u" => (true, IntegerRank::Int),
        "l" => (false, IntegerRank::Long),
        "ul" | "lu" => (true, IntegerRank::Long),
        "ll" => (false, IntegerRank::LongLong),
        "ull" | "llu" => (true, IntegerRank::LongLong),
        _ => {
            return Err(Diagnostic::new(
                span,
                format!("invalid integer suffix: {suffix}"),
            ));
        }
    };
    if suffix.contains("lL") || suffix.contains("Ll") {
        return Err(Diagnostic::new(span, "long-long suffix must be ll or LL"));
    }
    let ranks = [IntegerRank::Int, IntegerRank::Long, IntegerRank::LongLong];
    let first_rank = ranks.iter().position(|rank| *rank == minimum).unwrap();
    for &rank in &ranks[first_rank..] {
        let signed_max = if rank == IntegerRank::Int {
            i32::MAX as u64
        } else {
            i64::MAX as u64
        };
        let unsigned_max = if rank == IntegerRank::Int {
            u32::MAX as u64
        } else {
            u64::MAX
        };
        if !unsigned && value <= signed_max {
            return Ok(Integer {
                bits: value,
                rank,
                unsigned: false,
            });
        }
        // Decimal unsuffixed literals consider only signed candidates.
        if (unsigned || base != 10) && value <= unsigned_max {
            return Ok(Integer {
                bits: value,
                rank,
                unsigned: true,
            });
        }
    }
    Err(Diagnostic::new(
        span,
        "integer literal does not fit its candidate types",
    ))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Encoding {
    Utf8,
    Utf16,
    Utf32,
}

impl Encoding {
    pub fn unit_bytes(self) -> usize {
        match self {
            Encoding::Utf8 => 1,
            Encoding::Utf16 => 2,
            Encoding::Utf32 => 4,
        }
    }
}

/// Decoded contents without a terminator. Concatenation happens before the
/// caller adds the final zero code unit. Wide code units are already big-endian.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StringValue {
    pub encoding: Encoding,
    pub bytes: Vec<u8>,
}

pub fn string(prefix: &str, raw: &str, span: Span) -> Result<StringValue, Diagnostic> {
    let encoding = match prefix {
        "" | "u8" => Encoding::Utf8,
        "u" => Encoding::Utf16,
        "U" | "L" => Encoding::Utf32,
        _ => return Err(Diagnostic::new(span, "unknown literal encoding prefix")),
    };
    let mut bytes = Vec::new();
    let mut chars = raw.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\\' {
            append_character(&mut bytes, encoding, c);
            continue;
        }
        let escaped = chars
            .next()
            .ok_or_else(|| Diagnostic::new(span, "incomplete escape sequence"))?;
        let value = match escaped {
            '\'' => '\'' as u32,
            '"' => '"' as u32,
            '?' => '?' as u32,
            '\\' => '\\' as u32,
            'a' => 7,
            'b' => 8,
            't' => 9,
            'n' => 10,
            'v' => 11,
            'f' => 12,
            'r' => 13,
            'e' => 27, // GNU escape, also accepted by the existing frontend.
            '0'..='7' => {
                let mut value = escaped.to_digit(8).unwrap();
                for _ in 0..2 {
                    match chars.peek().and_then(|c| c.to_digit(8)) {
                        Some(digit) => {
                            chars.next();
                            value = value * 8 + digit;
                        }
                        None => break,
                    }
                }
                value
            }
            'x' => {
                let mut value = 0u32;
                let mut digits = 0;
                while let Some(digit) = chars.peek().and_then(|c| c.to_digit(16)) {
                    chars.next();
                    value = value
                        .checked_mul(16)
                        .and_then(|v| v.checked_add(digit))
                        .ok_or_else(|| Diagnostic::new(span, "hex escape exceeds 32 bits"))?;
                    digits += 1;
                }
                if digits == 0 {
                    return Err(Diagnostic::new(span, "hex escape needs at least one digit"));
                }
                value
            }
            'u' | 'U' => {
                let mut value = 0u32;
                for _ in 0..if escaped == 'u' { 4 } else { 8 } {
                    let digit = chars.next().and_then(|c| c.to_digit(16)).ok_or_else(|| {
                        Diagnostic::new(span, "incomplete universal character escape")
                    })?;
                    value = value * 16 + digit;
                }
                let c = char::from_u32(value)
                    .ok_or_else(|| Diagnostic::new(span, "invalid Unicode scalar value"))?;
                append_character(&mut bytes, encoding, c);
                continue;
            }
            _ => {
                return Err(Diagnostic::new(
                    span,
                    format!("unknown escape sequence: \\{escaped}"),
                ));
            }
        };
        match encoding {
            Encoding::Utf8 if value <= u8::MAX as u32 => bytes.push(value as u8),
            Encoding::Utf16 if value <= u16::MAX as u32 => {
                bytes.extend_from_slice(&(value as u16).to_be_bytes())
            }
            Encoding::Utf32 => bytes.extend_from_slice(&value.to_be_bytes()),
            _ => {
                return Err(Diagnostic::new(
                    span,
                    "escape value does not fit the literal code unit",
                ));
            }
        }
    }
    Ok(StringValue { encoding, bytes })
}

fn append_character(bytes: &mut Vec<u8>, encoding: Encoding, c: char) {
    match encoding {
        Encoding::Utf8 => bytes.extend_from_slice(c.encode_utf8(&mut [0; 4]).as_bytes()),
        Encoding::Utf16 => {
            for unit in c.encode_utf16(&mut [0; 2]) {
                bytes.extend_from_slice(&unit.to_be_bytes());
            }
        }
        Encoding::Utf32 => bytes.extend_from_slice(&(c as u32).to_be_bytes()),
    }
}

/// Ordinary multi-character literals use the target's big-endian packing.
/// Prefixed character literals must contain exactly one code unit.
pub fn character(prefix: &str, raw: &str, span: Span) -> Result<u32, Diagnostic> {
    let value = string(prefix, raw, span)?;
    let expected = value.encoding.unit_bytes();
    if value.bytes.is_empty()
        || (prefix.is_empty() && value.bytes.len() > 4)
        || (!prefix.is_empty() && value.bytes.len() != expected)
    {
        return Err(Diagnostic::new(
            span,
            "invalid number of code units in character literal",
        ));
    }
    Ok(value
        .bytes
        .iter()
        .fold(0, |bits, byte| (bits << 8) | *byte as u32))
}
