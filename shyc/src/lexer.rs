//! Tokenizer for ShyC: turns one source file into a flat token stream.
//!
//! Pipeline position: `source` -> **`lexer`** -> preprocessor -> parser.
//!
//! The lexer runs before the preprocessor, so it never interprets `#define`,
//! `#include`, or `#![...]`: it only produces the tokens and the line structure
//! the preprocessor needs (`Newline`, `at_line_start`, `leading_space`).
//!
//! Two steps happen before scanning starts:
//!
//! 1. Backslash-newline pairs are deleted (C translation phase 2). An
//!    identifier, literal, or comment may therefore continue on the next
//!    physical line.
//! 2. Every remaining byte remembers its offset in the original file, so all
//!    spans still point at the text the user wrote instead of at the temporary
//!    buffer with the continuations removed.
//!
//! The lexer deliberately does not decode values. Numbers keep their original
//! spelling (`0xFF`, `1'000`, `1.5e-3`) and literals keep their escapes
//! (`"a\n"` stores `a\n`), because the parser and semantic analysis decide
//! what those bytes mean.
//!
//! Not implemented yet:
//!
//! - universal character names (`\uXXXX`) are rejected with a diagnostic;
//! - raw strings (`R"(...)"`) are not recognised, so they come out as an
//!   identifier followed by an ordinary string literal;
//! - `\r` is blank space, so files whose lines end in a bare `\r` are not
//!   supported.

use crate::source::{Diagnostic, FileId, SourceMap, Span};

/// What a token is. Payload strings keep the source spelling.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TokenKind {
    /// Identifier: Unicode letters, `_`, and digits after the first character.
    Ident(String),
    /// Preprocessing number. Raw spelling, no numeric type yet.
    Number(String),
    /// String literal. `raw` is the text between the quotes, escapes intact.
    StringLiteral { prefix: String, raw: String },
    /// Character literal. Same rules as `StringLiteral`.
    CharLiteral { prefix: String, raw: String },
    /// Operator or punctuation, e.g. `<<=`, `::`, `asm!`, `#`.
    Punct(String),
    /// End of a physical line. Kept so the preprocessor can see line structure.
    Newline,
    /// Always the last token of the stream.
    Eof,
}

/// One token plus the position facts the preprocessor needs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Token {
    pub kind: TokenKind,
    /// Byte range in the original file, never in the continuation-free copy.
    pub span: Span,
    /// No token has appeared yet on this logical line, so a `#` here starts a
    /// directive or an attribute.
    pub at_line_start: bool,
    /// Blanks or a comment separate this token from the previous one.
    pub leading_space: bool,
}

/// Lexes `file` into tokens, ending with exactly one [`TokenKind::Eof`].
pub fn lex(sources: &SourceMap, file: FileId) -> Result<Vec<Token>, Diagnostic> {
    let text = &sources.file(file).text;
    let cooked = remove_line_continuations(text);
    let mut lexer = Lexer {
        file,
        source_len: text.len(),
        bytes: &cooked.bytes,
        origin: &cooked.origin,
        pos: 0,
        at_line_start: true,
        leading_space: false,
        tokens: Vec::new(),
    };
    lexer.run()?;
    Ok(lexer.tokens)
}

/// C punctuators plus Shy's `::`, longest first.
///
/// The order is part of the algorithm: `Lexer::lex_punct` takes the first
/// entry that matches, so every multi-character punctuator must come before
/// the shorter punctuators it starts with (`<<=` before `<<` before `<`).
const PUNCTUATORS: &[&str] = &[
    "<<=", ">>=", "...", //
    "->", "++", "--", "<<", ">>", "<=", ">=", "==", "!=", "&&", "||", "*=", "/=", "%=", "+=", "-=",
    "&=", "^=", "|=", "##", "::", //
    "[", "]", "(", ")", "{", "}", ".", "&", "*", "+", "-", "~", "!", "/", "%", "<", ">", "^", "|",
    "?", ":", ";", "=", ",", "#", "@",
];

/// Source bytes with `\`-newline pairs deleted, plus their original offsets.
struct Cooked {
    bytes: Vec<u8>,
    /// `origin[i]` is the offset in the original text of `bytes[i]`.
    origin: Vec<usize>,
}

/// C translation phase 2: a backslash immediately before a newline joins the
/// two physical lines into one logical line.
fn remove_line_continuations(text: &str) -> Cooked {
    let source = text.as_bytes();
    let mut cooked = Cooked {
        bytes: Vec::with_capacity(source.len()),
        origin: Vec::with_capacity(source.len()),
    };
    let mut i = 0;
    while i < source.len() {
        if source[i] == b'\\' {
            let mut after = i + 1;
            if source.get(after) == Some(&b'\r') {
                after += 1;
            }
            if source.get(after) == Some(&b'\n') {
                i = after + 1;
                continue;
            }
        }
        cooked.bytes.push(source[i]);
        cooked.origin.push(i);
        i += 1;
    }
    cooked
}

struct Lexer<'a> {
    file: FileId,
    /// Length of the original file, used for the span of `Eof`.
    source_len: usize,
    /// The continuation-free bytes being scanned.
    bytes: &'a [u8],
    /// Original offset of each byte in `bytes`.
    origin: &'a [usize],
    /// Next byte of `bytes` to scan.
    pos: usize,
    at_line_start: bool,
    leading_space: bool,
    tokens: Vec<Token>,
}

impl Lexer<'_> {
    fn run(&mut self) -> Result<(), Diagnostic> {
        loop {
            self.skip_trivia()?;
            if self.pos == self.bytes.len() {
                break;
            }
            if self.bytes[self.pos] == b'\n' {
                // Trailing blanks do not "lead" the newline, so drop the flag.
                self.leading_space = false;
                let start = self.pos;
                self.pos += 1;
                let end = self.pos;
                self.push(TokenKind::Newline, start, end);
                self.at_line_start = true;
                continue;
            }
            self.lex_token()?;
        }

        let eof = Token {
            kind: TokenKind::Eof,
            span: Span {
                file: self.file,
                start: self.source_len,
                end: self.source_len,
            },
            at_line_start: self.at_line_start,
            leading_space: false,
        };
        self.tokens.push(eof);
        Ok(())
    }

    /// Records a token for `start..end` of `bytes`, then clears the flags that
    /// describe the gap in front of a token.
    fn push(&mut self, kind: TokenKind, start: usize, end: usize) {
        let token = Token {
            kind,
            span: self.span(start, end),
            at_line_start: self.at_line_start,
            leading_space: self.leading_space,
        };
        self.tokens.push(token);
        self.at_line_start = false;
        self.leading_space = false;
    }

    /// Skips blanks and comments, remembering that the next token is separated.
    ///
    /// A block comment becomes one space, including its internal newlines.
    /// It neither ends a directive nor changes whether this logical line
    /// already contains a token.
    fn skip_trivia(&mut self) -> Result<(), Diagnostic> {
        loop {
            match self.peek() {
                Some(b' ' | b'\t' | b'\r' | 0x0b | 0x0c) => {
                    self.pos += 1;
                    self.leading_space = true;
                }
                Some(b'/') if self.peek_at(1) == Some(b'/') => {
                    // `// #![...]` 写在注释里的 Shy 属性：C 兼容的源码保持纯
                    // C 语法，注释对 C 工具无害，这里把 `#` 之前的注释前缀
                    // 让出去，让属性按普通 token 继续走预处理器。
                    if let Some(offset) = self.comment_attribute_offset() {
                        self.pos += offset;
                        self.leading_space = true;
                        continue;
                    }
                    self.pos += 2;
                    while self.peek().is_some_and(|byte| byte != b'\n') {
                        self.pos += 1;
                    }
                    self.leading_space = true;
                }
                Some(b'/') if self.peek_at(1) == Some(b'*') => {
                    let start = self.pos;
                    self.pos += 2;
                    let mut terminated = false;
                    while self.pos < self.bytes.len() {
                        if self.bytes[self.pos] == b'*' && self.peek_at(1) == Some(b'/') {
                            self.pos += 2;
                            terminated = true;
                            break;
                        }
                        self.pos += 1;
                    }
                    if !terminated {
                        return Err(Diagnostic::new(
                            self.span_to_eof(start),
                            "unterminated block comment",
                        ));
                    }
                    self.leading_space = true;
                }
                _ => return Ok(()),
            }
        }
    }

    /// `Some(offset)` when the `//` at `pos` is a commented Shy attribute on a
    /// line that has no token yet: `// #![no_main]` and friends.
    ///
    /// `offset` is where the `#` starts, so the caller can rewind to it and let
    /// the ordinary tokenizer read the attribute.
    fn comment_attribute_offset(&self) -> Option<usize> {
        if !self.at_line_start {
            return None;
        }
        let mut cursor = self.pos + 2;
        while matches!(self.bytes.get(cursor), Some(b' ' | b'\t' | b'\r')) {
            cursor += 1;
        }
        if self.bytes.get(cursor) != Some(&b'#') {
            return None;
        }
        // 两种形状都认：文件级的 `#![...]`，以及声明级的 `#[...]`。
        let bracket = if self.bytes.get(cursor + 1) == Some(&b'!') {
            cursor + 2
        } else {
            cursor + 1
        };
        if self.bytes.get(bracket) != Some(&b'[') {
            return None;
        }
        Some(cursor - self.pos)
    }

    /// Lexes one token that is not a newline. `pos` is on a non-trivia byte.
    fn lex_token(&mut self) -> Result<(), Diagnostic> {
        let start = self.pos;
        let byte = self.bytes[start];

        if byte == b'"' {
            return self.lex_string(String::new(), start);
        }
        if byte == b'\'' {
            return self.lex_char(String::new(), start);
        }
        // A pp-number starts with a digit, or with `.` followed by a digit.
        let starts_number =
            byte.is_ascii_digit() || (byte == b'.' && matches!(self.peek_at(1), Some(b'0'..=b'9')));
        if starts_number {
            self.lex_number();
            return Ok(());
        }
        if byte == b'\\' {
            return Err(self.backslash_error());
        }
        if let Some(width) = self.ident_start(start) {
            self.lex_ident(width)?;
            return Ok(());
        }
        self.lex_punct()
    }

    fn lex_ident(&mut self, first_width: usize) -> Result<(), Diagnostic> {
        let start = self.pos;
        self.pos += first_width;
        while let Some((c, width)) = self.char_at(self.pos) {
            if c == '_' || c.is_alphanumeric() {
                self.pos += width;
            } else {
                break;
            }
        }
        let name = self.text(start, self.pos);

        // Shy's register-binding inline assembly is spelled `asm!`; the `!` is
        // part of the token, so the parser does not have to glue them back.
        if name == "asm" && self.peek() == Some(b'!') {
            self.pos += 1;
            let end = self.pos;
            self.push(TokenKind::Punct("asm!".to_string()), start, end);
            return Ok(());
        }

        // `L"..."`, `u8"..."`, `U'...'`: the encoding prefix is part of the
        // literal token, and only when the quote follows with no blanks.
        if is_literal_prefix(&name) {
            match self.peek() {
                Some(b'"') => return self.lex_string(name, start),
                Some(b'\'') => return self.lex_char(name, start),
                _ => {}
            }
        }

        let end = self.pos;
        self.push(TokenKind::Ident(name), start, end);
        Ok(())
    }

    /// C pp-number: digits, letters, `_`, `.`, `'` between digits, and an
    /// exponent sign after `e`/`E`/`p`/`P`. No other signs are absorbed, so
    /// `-1` stays `-` followed by `1`.
    fn lex_number(&mut self) {
        let start = self.pos;
        self.pos += 1; // the leading digit, or the `.` of `.5`
        while let Some((c, width)) = self.char_at(self.pos) {
            match c {
                '.' => self.pos += 1,
                'e' | 'E' | 'p' | 'P' => {
                    self.pos += 1;
                    if matches!(self.peek(), Some(b'+' | b'-')) {
                        self.pos += 1;
                    }
                }
                '\'' if matches!(self.peek_at(1), Some(b'0'..=b'9')) => self.pos += 1,
                c if c == '_' || c.is_alphanumeric() => self.pos += width,
                _ => break,
            }
        }
        let end = self.pos;
        let spelling = self.text(start, end);
        self.push(TokenKind::Number(spelling), start, end);
    }

    fn lex_string(&mut self, prefix: String, start: usize) -> Result<(), Diagnostic> {
        let (raw, end) = self.scan_quoted(b'"', start)?;
        self.push(TokenKind::StringLiteral { prefix, raw }, start, end);
        Ok(())
    }

    fn lex_char(&mut self, prefix: String, start: usize) -> Result<(), Diagnostic> {
        let (raw, end) = self.scan_quoted(b'\'', start)?;
        self.push(TokenKind::CharLiteral { prefix, raw }, start, end);
        Ok(())
    }

    /// Scans from the opening quote at `pos` to the closing quote and returns
    /// the contents between them, escapes kept verbatim.
    fn scan_quoted(&mut self, quote: u8, start: usize) -> Result<(String, usize), Diagnostic> {
        let what = if quote == b'"' { "string" } else { "character" };
        self.pos += 1; // opening quote
        let content_start = self.pos;
        loop {
            let Some((c, width)) = self.char_at(self.pos) else {
                return Err(Diagnostic::new(
                    self.span_to_eof(start),
                    format!("unterminated {what} literal"),
                ));
            };
            if c == quote as char {
                let raw = self.text(content_start, self.pos);
                self.pos += 1;
                return Ok((raw, self.pos));
            }
            if c == '\n' {
                // A bare newline is not allowed inside a literal; a continued
                // line was already joined, so this really is the end.
                return Err(Diagnostic::new(
                    self.span(start, self.pos),
                    format!("unterminated {what} literal"),
                ));
            }
            if c == '\\' {
                // Skip the escaped character so `\"` cannot close the literal.
                self.pos += 1;
                if let Some((escaped, escaped_width)) = self.char_at(self.pos)
                    && escaped != '\n'
                {
                    self.pos += escaped_width;
                }
                continue;
            }
            self.pos += width;
        }
    }

    /// Longest-match punctuator lookup; anything left over is an illegal byte.
    fn lex_punct(&mut self) -> Result<(), Diagnostic> {
        let start = self.pos;
        for punct in PUNCTUATORS {
            if self.bytes[start..].starts_with(punct.as_bytes()) {
                self.pos += punct.len();
                let end = self.pos;
                self.push(TokenKind::Punct((*punct).to_string()), start, end);
                return Ok(());
            }
        }
        let Some((c, width)) = self.char_at(start) else {
            return Err(Diagnostic::new(
                self.span_to_eof(start),
                "invalid UTF-8 in source",
            ));
        };
        Err(Diagnostic::new(
            self.span(start, start + width),
            format!("unexpected character {c:?} in source"),
        ))
    }

    /// `\u`/`\U` would be a C universal character name; a lone `\` is usually a
    /// broken line continuation. Both are reported rather than skipped.
    fn backslash_error(&self) -> Diagnostic {
        let start = self.pos;
        let message = match self.peek_at(1) {
            Some(b'u' | b'U') => "universal character names (\\uXXXX) are not supported yet",
            _ => {
                "stray '\\' in source (a line continuation must be the last character of its line)"
            }
        };
        Diagnostic::new(self.span(start, start + 1), message)
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn peek_at(&self, offset: usize) -> Option<u8> {
        self.bytes.get(self.pos + offset).copied()
    }

    /// Width of the identifier-start character at `offset`, if there is one.
    fn ident_start(&self, offset: usize) -> Option<usize> {
        let (c, width) = self.char_at(offset)?;
        if c == '_' || c.is_alphabetic() {
            Some(width)
        } else {
            None
        }
    }

    /// Decodes the character at `offset`. The bytes are copied from a valid
    /// UTF-8 `String`, so `None` means "past the end" in practice.
    fn char_at(&self, offset: usize) -> Option<(char, usize)> {
        let byte = *self.bytes.get(offset)?;
        let width = match byte {
            0x00..=0x7f => 1,
            0xc0..=0xdf => 2,
            0xe0..=0xef => 3,
            _ => 4,
        };
        let text = std::str::from_utf8(self.bytes.get(offset..offset + width)?).ok()?;
        text.chars().next().map(|c| (c, width))
    }

    fn text(&self, start: usize, end: usize) -> String {
        // The cooked buffer is a byte-for-byte copy of the valid UTF-8 source.
        std::str::from_utf8(&self.bytes[start..end])
            .expect("source is valid UTF-8")
            .to_string()
    }

    /// Span for the cooked range `start..end`, mapped back to the real file.
    fn span(&self, start: usize, end: usize) -> Span {
        let origin_start = self.origin_offset(start);
        let origin_end = if end > start {
            self.origin_offset(end - 1) + 1
        } else {
            origin_start
        };
        Span {
            file: self.file,
            start: origin_start,
            end: origin_end,
        }
    }

    /// Span reaching the end of the file, for unterminated constructs.
    fn span_to_eof(&self, start: usize) -> Span {
        Span {
            file: self.file,
            start: self.origin_offset(start),
            end: self.source_len,
        }
    }

    fn origin_offset(&self, pos: usize) -> usize {
        self.origin.get(pos).copied().unwrap_or(self.source_len)
    }
}

/// Encoding prefixes ShyC understands on string and character literals.
fn is_literal_prefix(text: &str) -> bool {
    matches!(text, "L" | "u" | "U" | "u8")
}
