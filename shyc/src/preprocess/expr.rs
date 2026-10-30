//! Evaluation of `#if` and `#elif` controlling expressions.
//!
//! The expression is processed in the order C requires: `defined` operands
//! first, macro expansion second, and any identifier that survives expansion
//! becomes `0`. Evaluation uses C's `intmax_t`/`uintmax_t` model: every value
//! is a 64-bit pattern tagged signed or unsigned, and ordinary binary
//! operators apply the usual arithmetic conversions. A `U` suffix, or a value
//! above `INTMAX_MAX`, makes an integer constant unsigned.
//!
//! `&&`, `||`, and `?:` parse their unused branch but evaluate it with a
//! `live = false` flag. That preserves short-circuit semantics: `#if 0 &&
//! (1 / 0)` is valid, while the same division in a live branch is diagnosed.
//! Dead branches are still parsed and typed, because the conditional operator
//! converts both branch types even though only one value is selected.

use super::Preprocessor;
use super::macros::{ExpToken, is_punct};
use super::value::{self, BinaryOp, UnaryOp, Value};
use crate::lexer::{Token, TokenKind};
use crate::source::{Diagnostic, Span};

impl Preprocessor<'_> {
    pub(crate) fn eval_condition(
        &mut self,
        arguments: &[Token],
        span: Span,
    ) -> Result<bool, Diagnostic> {
        if arguments.is_empty() {
            return Err(Diagnostic::new(span, "expected an expression"));
        }
        let replaced = self.replace_defined(arguments)?;
        let expanded = self.expand_all(replaced.into_iter().map(ExpToken::plain).collect())?;
        // `__has_include(<x>)` and `__has_include_next(<x>)` answer 0: every
        // header that asks then takes its portable branch. The whole construct
        // has to be replaced, not just the identifier, because the operand is a
        // file name rather than an expression.
        let mut tokens: Vec<Token> = Vec::with_capacity(expanded.len());
        let mut index = 0;
        while index < expanded.len() {
            let expanded_token = expanded[index].clone();
            index += 1;
            let span = expanded_token.span();
            let mut token = expanded_token.token;
            token.span = span;
            // Compiler builtins such as `__has_include`, `__has_feature`, and
            // `__building_module` take a non-expression operand. Every one of
            // them answers 0 here, so headers choose their portable path.
            let has_include = matches!(
                &token.kind,
                TokenKind::Ident(name) if name.starts_with("__")
            ) && expanded.get(index).is_some_and(
                |next| matches!(&next.token.kind, TokenKind::Punct(text) if text == "("),
            );
            if has_include {
                let mut depth = 0usize;
                while let Some(next) = expanded.get(index) {
                    index += 1;
                    match &next.token.kind {
                        TokenKind::Punct(text) if text == "(" => depth += 1,
                        TokenKind::Punct(text) if text == ")" => {
                            depth -= 1;
                            if depth == 0 {
                                break;
                            }
                        }
                        _ => {}
                    }
                }
                token.kind = TokenKind::Number("0".to_string());
            } else if matches!(token.kind, TokenKind::Ident(_)) {
                token.kind = TokenKind::Number("0".to_string());
            }
            tokens.push(token);
        }
        ExprParser {
            tokens: &tokens,
            pos: 0,
            fallback: span,
        }
        .parse()
    }

    /// Replaces `defined name` and `defined(name)` with `1` or `0`.
    fn replace_defined(&self, arguments: &[Token]) -> Result<Vec<Token>, Diagnostic> {
        let mut out = Vec::with_capacity(arguments.len());
        let mut index = 0;
        while index < arguments.len() {
            let is_defined = matches!(
                &arguments[index].kind,
                TokenKind::Ident(name) if name == "defined"
            );
            if !is_defined {
                out.push(arguments[index].clone());
                index += 1;
                continue;
            }

            let operator_span = arguments[index].span;
            index += 1;
            let name = if arguments
                .get(index)
                .is_some_and(|token| is_punct(token, "("))
            {
                index += 1;
                let Some(Token {
                    kind: TokenKind::Ident(name),
                    ..
                }) = arguments.get(index)
                else {
                    return Err(Diagnostic::new(
                        operator_span,
                        "expected a macro name after `defined(`",
                    ));
                };
                let name = name.clone();
                index += 1;
                if !arguments
                    .get(index)
                    .is_some_and(|token| is_punct(token, ")"))
                {
                    return Err(Diagnostic::new(
                        operator_span,
                        "expected `)` after `defined(...)`",
                    ));
                }
                index += 1;
                name
            } else if let Some(Token {
                kind: TokenKind::Ident(name),
                ..
            }) = arguments.get(index)
            {
                let name = name.clone();
                index += 1;
                name
            } else {
                return Err(Diagnostic::new(
                    operator_span,
                    "expected a macro name after `defined`",
                ));
            };

            let value = if self.is_defined(&name) { "1" } else { "0" };
            out.push(Token {
                kind: TokenKind::Number(value.to_string()),
                span: operator_span,
                at_line_start: false,
                leading_space: false,
            });
        }
        Ok(out)
    }
}

struct ExprParser<'a> {
    tokens: &'a [Token],
    pos: usize,
    fallback: Span,
}

impl ExprParser<'_> {
    fn parse(&mut self) -> Result<bool, Diagnostic> {
        let value = self.conditional(true)?;
        if self.pos != self.tokens.len() {
            return Err(self.error_here("unexpected token in #if expression"));
        }
        Ok(value.truthy())
    }

    fn conditional(&mut self, live: bool) -> Result<Value, Diagnostic> {
        let condition = self.logical_or(live)?;
        if self.take("?").is_none() {
            return Ok(condition);
        }
        let when_true = self.conditional(live && condition.truthy())?;
        if self.take(":").is_none() {
            return Err(self.error_here("expected `:` in a conditional expression"));
        }
        let when_false = self.conditional(live && !condition.truthy())?;
        // Both branches participate in the usual arithmetic conversions even
        // though only the selected value is used.
        let unsigned = when_true.unsigned || when_false.unsigned;
        let bits = if condition.truthy() {
            when_true.bits
        } else {
            when_false.bits
        };
        Ok(Value::from_parts(bits, unsigned))
    }

    fn logical_or(&mut self, live: bool) -> Result<Value, Diagnostic> {
        let mut value = self.logical_and(live)?;
        while self.take("||").is_some() {
            let right = self.logical_and(live && !value.truthy())?;
            value = Value::boolean(live && (value.truthy() || right.truthy()));
        }
        Ok(value)
    }

    fn logical_and(&mut self, live: bool) -> Result<Value, Diagnostic> {
        let mut value = self.bitwise_or(live)?;
        while self.take("&&").is_some() {
            let right = self.bitwise_or(live && value.truthy())?;
            value = Value::boolean(live && value.truthy() && right.truthy());
        }
        Ok(value)
    }

    fn bitwise_or(&mut self, live: bool) -> Result<Value, Diagnostic> {
        let mut value = self.bitwise_xor(live)?;
        while let Some(span) = self.take("|") {
            let right = self.bitwise_xor(live)?;
            value = value::binary(BinaryOp::BitOr, value, right, live, span)?;
        }
        Ok(value)
    }

    fn bitwise_xor(&mut self, live: bool) -> Result<Value, Diagnostic> {
        let mut value = self.bitwise_and(live)?;
        while let Some(span) = self.take("^") {
            let right = self.bitwise_and(live)?;
            value = value::binary(BinaryOp::BitXor, value, right, live, span)?;
        }
        Ok(value)
    }

    fn bitwise_and(&mut self, live: bool) -> Result<Value, Diagnostic> {
        let mut value = self.equality(live)?;
        while let Some(span) = self.take("&") {
            let right = self.equality(live)?;
            value = value::binary(BinaryOp::BitAnd, value, right, live, span)?;
        }
        Ok(value)
    }

    fn equality(&mut self, live: bool) -> Result<Value, Diagnostic> {
        let mut value = self.relational(live)?;
        loop {
            if let Some(span) = self.take("==") {
                let right = self.relational(live)?;
                value = value::binary(BinaryOp::Equal, value, right, live, span)?;
            } else if let Some(span) = self.take("!=") {
                let right = self.relational(live)?;
                value = value::binary(BinaryOp::NotEqual, value, right, live, span)?;
            } else {
                return Ok(value);
            }
        }
    }

    fn relational(&mut self, live: bool) -> Result<Value, Diagnostic> {
        let mut value = self.shift(live)?;
        loop {
            let operator = if self.take("<").is_some() {
                BinaryOp::Less
            } else if self.take(">").is_some() {
                BinaryOp::Greater
            } else if self.take("<=").is_some() {
                BinaryOp::LessEqual
            } else if self.take(">=").is_some() {
                BinaryOp::GreaterEqual
            } else {
                return Ok(value);
            };
            let span = self.tokens[self.pos - 1].span;
            let right = self.shift(live)?;
            value = value::binary(operator, value, right, live, span)?;
        }
    }

    fn shift(&mut self, live: bool) -> Result<Value, Diagnostic> {
        let mut value = self.additive(live)?;
        loop {
            let operator = if self.take("<<").is_some() {
                BinaryOp::Shl
            } else if self.take(">>").is_some() {
                BinaryOp::Shr
            } else {
                return Ok(value);
            };
            let span = self.tokens[self.pos - 1].span;
            let right = self.additive(live)?;
            value = value::binary(operator, value, right, live, span)?;
        }
    }

    fn additive(&mut self, live: bool) -> Result<Value, Diagnostic> {
        let mut value = self.multiplicative(live)?;
        loop {
            let operator = if self.take("+").is_some() {
                BinaryOp::Add
            } else if self.take("-").is_some() {
                BinaryOp::Sub
            } else {
                return Ok(value);
            };
            let span = self.tokens[self.pos - 1].span;
            let right = self.multiplicative(live)?;
            value = value::binary(operator, value, right, live, span)?;
        }
    }

    fn multiplicative(&mut self, live: bool) -> Result<Value, Diagnostic> {
        let mut value = self.unary(live)?;
        loop {
            let operator = if self.take("*").is_some() {
                BinaryOp::Mul
            } else if self.take("/").is_some() {
                BinaryOp::Div
            } else if self.take("%").is_some() {
                BinaryOp::Rem
            } else {
                return Ok(value);
            };
            let span = self.tokens[self.pos - 1].span;
            let right = self.unary(live)?;
            value = value::binary(operator, value, right, live, span)?;
        }
    }

    fn unary(&mut self, live: bool) -> Result<Value, Diagnostic> {
        if self.take("+").is_some() {
            let value = self.unary(live)?;
            return Ok(value::unary(UnaryOp::Plus, value, live));
        }
        if self.take("-").is_some() {
            let value = self.unary(live)?;
            return Ok(value::unary(UnaryOp::Minus, value, live));
        }
        if self.take("~").is_some() {
            let value = self.unary(live)?;
            return Ok(value::unary(UnaryOp::BitNot, value, live));
        }
        if self.take("!").is_some() {
            let value = self.unary(live)?;
            return Ok(value::unary(UnaryOp::Not, value, live));
        }
        self.primary(live)
    }

    fn primary(&mut self, live: bool) -> Result<Value, Diagnostic> {
        if self.take("(").is_some() {
            let value = self.conditional(live)?;
            if self.take(")").is_none() {
                return Err(self.error_here("expected `)` in #if expression"));
            }
            return Ok(value);
        }

        let Some(token) = self.tokens.get(self.pos) else {
            return Err(self.error_here("expected an expression"));
        };
        match &token.kind {
            TokenKind::Number(text) => {
                let value = parse_integer(text).ok_or_else(|| {
                    Diagnostic::new(
                        token.span,
                        format!("invalid integer constant `{text}` in #if expression"),
                    )
                })?;
                self.pos += 1;
                Ok(value)
            }
            TokenKind::CharLiteral { raw, .. } => {
                let value = parse_character(raw, token.span)?;
                self.pos += 1;
                Ok(Value::signed(value))
            }
            TokenKind::StringLiteral { .. } => Err(Diagnostic::new(
                token.span,
                "string literals are not allowed in #if expressions",
            )),
            TokenKind::Ident(name) => Err(Diagnostic::new(
                token.span,
                format!("unexpected identifier `{name}` in #if expression"),
            )),
            TokenKind::Punct(punct) => Err(Diagnostic::new(
                token.span,
                format!("expected an expression, found `{punct}`"),
            )),
            TokenKind::Newline | TokenKind::Eof => Err(self.error_here("expected an expression")),
        }
    }

    fn take(&mut self, wanted: &str) -> Option<Span> {
        let token = self.tokens.get(self.pos)?;
        if !is_punct(token, wanted) {
            return None;
        }
        let span = token.span;
        self.pos += 1;
        Some(span)
    }

    fn error_here(&self, message: impl Into<String>) -> Diagnostic {
        let span = self
            .tokens
            .get(self.pos)
            .or_else(|| self.tokens.last())
            .map_or(self.fallback, |token| token.span);
        Diagnostic::new(span, message)
    }
}

fn parse_integer(text: &str) -> Option<Value> {
    let cleaned: String = text
        .chars()
        .filter(|character| *character != '\'')
        .collect();
    let bytes = cleaned.as_bytes();
    let mut end = bytes.len();
    let mut has_unsigned_suffix = false;
    while end > 0 && matches!(bytes[end - 1], b'u' | b'U' | b'l' | b'L') {
        if matches!(bytes[end - 1], b'u' | b'U') {
            has_unsigned_suffix = true;
        }
        end -= 1;
    }
    let body = &cleaned[..end];
    if body.is_empty() {
        return None;
    }
    let (digits, radix) =
        if let Some(rest) = body.strip_prefix("0x").or_else(|| body.strip_prefix("0X")) {
            (rest, 16)
        } else if let Some(rest) = body.strip_prefix("0b").or_else(|| body.strip_prefix("0B")) {
            (rest, 2)
        } else if body.len() > 1 && body.starts_with('0') {
            (&body[1..], 8)
        } else {
            (body, 10)
        };
    if digits.is_empty() {
        return None;
    }
    let bits = u64::from_str_radix(digits, radix).ok()?;
    // A non-decimal constant with the high bit set is uintmax_t in C; decimal
    // constants above INTMAX_MAX are given the same practical treatment.
    let unsigned = has_unsigned_suffix || bits > i64::MAX as u64;
    Some(Value::from_parts(bits, unsigned))
}

fn parse_character(raw: &str, span: Span) -> Result<i64, Diagnostic> {
    let characters: Vec<char> = raw.chars().collect();
    if characters.is_empty() {
        return Err(Diagnostic::new(span, "empty character constant"));
    }
    let mut index = 1;
    let value = if characters[0] == '\\' {
        decode_escape(&characters, &mut index, span)?
    } else {
        characters[0] as u32
    };
    if index != characters.len() {
        return Err(Diagnostic::new(
            span,
            "multi-character character constants are not supported in #if expressions",
        ));
    }
    Ok(i64::from(value))
}

fn decode_escape(characters: &[char], index: &mut usize, span: Span) -> Result<u32, Diagnostic> {
    let Some(&escape) = characters.get(*index) else {
        return Err(Diagnostic::new(span, "incomplete escape sequence"));
    };
    *index += 1;
    let value = match escape {
        'n' => 10,
        't' => 9,
        'r' => 13,
        'a' => 7,
        'b' => 8,
        'f' => 12,
        'v' => 11,
        '\\' => 92,
        '\'' => 39,
        '"' => 34,
        '?' => 63,
        '0'..='7' => {
            let mut value = u32::from(escape as u8 - b'0');
            let mut count = 1;
            while count < 3 {
                let Some(&next) = characters.get(*index) else {
                    break;
                };
                let Some(digit) = next.to_digit(8) else {
                    break;
                };
                value = value * 8 + digit;
                *index += 1;
                count += 1;
            }
            value
        }
        'x' => {
            let mut value = 0u32;
            let mut digits = 0;
            while let Some(&next) = characters.get(*index) {
                let Some(digit) = next.to_digit(16) else {
                    break;
                };
                value = value.saturating_mul(16).saturating_add(digit);
                *index += 1;
                digits += 1;
            }
            if digits == 0 {
                return Err(Diagnostic::new(
                    span,
                    "`\\x` must be followed by a hexadecimal digit",
                ));
            }
            value
        }
        other => {
            return Err(Diagnostic::new(
                span,
                format!("unsupported escape sequence `\\{other}` in a character constant"),
            ));
        }
    };
    Ok(value)
}
