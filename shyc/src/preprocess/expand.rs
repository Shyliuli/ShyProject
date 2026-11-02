//! Macro expansion, argument collection, hidesets, and generated tokens.
//!
//! Expansion is iterative. Replacement tokens go onto a stack in reverse
//! order, in front of the remaining source tokens, which is the "rescan"
//! required by C. Function-like invocations may span physical lines because
//! [`FileCursor`] skips `Newline` tokens while looking ahead. A preprocessing
//! directive encountered while an argument list is open is an error rather
//! than silently becoming part of the argument.

use super::MAX_EXPANSION_DEPTH;
use super::Preprocessor;
use super::macros::{ExpToken, Hideset, Macro, is_punct, parameter_index, spelling};
use crate::lexer::{Token, TokenKind};
use crate::source::{Diagnostic, Span};

/// Remaining tokens of the file being expanded.
struct FileCursor<'a> {
    tokens: &'a [Token],
    pos: usize,
}

impl Preprocessor<'_> {
    /// Expands the tokens starting at `*pos` until a directive or EOF. The
    /// caller's index is advanced past every consumed token.
    pub(crate) fn expand_stream(
        &mut self,
        tokens: &[Token],
        pos: &mut usize,
        out: &mut Vec<ExpToken>,
    ) -> Result<(), Diagnostic> {
        let mut cursor = FileCursor { tokens, pos: *pos };
        let mut pending = Vec::new();
        while let Some(token) = pending.pop().or_else(|| next_source_token(&mut cursor)) {
            self.expand_step(token, &mut pending, Some(&mut cursor), out)?;
        }
        *pos = cursor.pos;
        Ok(())
    }

    /// Expands a self-contained token list (a macro argument) with no source
    /// lookahead.
    pub(crate) fn expand_all(&mut self, input: Vec<ExpToken>) -> Result<Vec<ExpToken>, Diagnostic> {
        if self.expansion_depth >= MAX_EXPANSION_DEPTH {
            let span = input.first().map(ExpToken::span).unwrap_or(Span {
                file: self.root,
                start: 0,
                end: 0,
            });
            return Err(Diagnostic::new(span, "macro expansion nesting is too deep"));
        }
        self.expansion_depth += 1;
        let result = expand_all_inner(self, input);
        self.expansion_depth -= 1;
        result
    }

    fn expand_step(
        &mut self,
        token: ExpToken,
        pending: &mut Vec<ExpToken>,
        cursor: Option<&mut FileCursor>,
        out: &mut Vec<ExpToken>,
    ) -> Result<(), Diagnostic> {
        let TokenKind::Ident(name) = &token.token.kind else {
            out.push(token);
            return Ok(());
        };
        let name = name.clone();
        if token.hideset.contains(&name) {
            out.push(token);
            return Ok(());
        }
        if let Some(definition) = self.macros.get(&name).cloned() {
            return self.expand_macro(token, &name, &definition, pending, cursor, out);
        }
        if !self.disabled_builtins.contains(&name)
            && matches!(name.as_str(), "__FILE__" | "__LINE__")
        {
            self.expand_builtin(token, &name, out);
            return Ok(());
        }
        out.push(token);
        Ok(())
    }

    fn expand_macro(
        &mut self,
        token: ExpToken,
        name: &str,
        definition: &Macro,
        pending: &mut Vec<ExpToken>,
        mut cursor: Option<&mut FileCursor>,
        out: &mut Vec<ExpToken>,
    ) -> Result<(), Diagnostic> {
        let invocation = token.span();

        if definition.params.is_none() {
            let hideset = token.hideset.with(name);
            let replacement = self.substitute(definition, &[], invocation, &hideset)?;
            push_replacement(pending, replacement);
            return Ok(());
        }

        let lookahead = pull(pending, cursor.as_deref_mut());
        let Some(lookahead) = lookahead else {
            out.push(token);
            return Ok(());
        };
        if !is_punct(&lookahead.token, "(") {
            pending.push(lookahead);
            out.push(token);
            return Ok(());
        }

        let (arguments, closing) =
            self.collect_arguments(name, invocation, pending, &mut cursor)?;
        let hideset = token.hideset.intersection(&closing.hideset).with(name);
        let arguments = self.prepare_arguments(name, definition, arguments, invocation)?;
        let replacement = self.substitute(definition, &arguments, invocation, &hideset)?;
        push_replacement(pending, replacement);
        Ok(())
    }

    /// Reads one macro invocation's argument list. The opening `(` has already
    /// been consumed by the caller. The closing `)` is returned because its
    /// hideset participates in the invocation's hideset intersection.
    fn collect_arguments(
        &mut self,
        name: &str,
        invocation: Span,
        pending: &mut Vec<ExpToken>,
        cursor: &mut Option<&mut FileCursor>,
    ) -> Result<(Vec<Vec<ExpToken>>, ExpToken), Diagnostic> {
        let mut arguments: Vec<Vec<ExpToken>> = Vec::new();
        let mut current: Vec<ExpToken> = Vec::new();
        let mut saw_content = false;
        let mut depth = 0usize;

        loop {
            let Some(token) = pull(pending, cursor.as_deref_mut()) else {
                return Err(Diagnostic::new(
                    invocation,
                    format!("unterminated invocation of macro `{name}`"),
                ));
            };
            match &token.token.kind {
                TokenKind::Punct(punct) if punct == ")" && depth == 0 => {
                    if saw_content || !arguments.is_empty() {
                        arguments.push(current);
                    } else {
                        // `f()` is one empty argument for a macro that takes
                        // one, and no arguments for a zero-parameter macro.
                        arguments.push(Vec::new());
                    }
                    return Ok((arguments, token));
                }
                TokenKind::Punct(punct) if punct == "," && depth == 0 => {
                    arguments.push(std::mem::take(&mut current));
                    saw_content = true;
                }
                TokenKind::Punct(punct) if punct == "(" => {
                    depth += 1;
                    current.push(token);
                    saw_content = true;
                }
                TokenKind::Punct(punct) if punct == ")" => {
                    depth -= 1;
                    current.push(token);
                    saw_content = true;
                }
                _ => {
                    current.push(token);
                    saw_content = true;
                }
            }
        }
    }

    fn prepare_arguments(
        &self,
        name: &str,
        definition: &Macro,
        mut arguments: Vec<Vec<ExpToken>>,
        invocation: Span,
    ) -> Result<Vec<Vec<ExpToken>>, Diagnostic> {
        let required = definition.params.as_ref().map_or(0, Vec::len);
        if definition.variadic {
            if arguments.len() < required {
                return Err(Diagnostic::new(
                    invocation,
                    format!(
                        "macro `{name}` expects at least {required} argument(s), but {} were given",
                        arguments.len()
                    ),
                ));
            }
            let extras = arguments.split_off(required);
            arguments.push(join_variadic(extras, invocation));
        } else {
            if required == 0 && arguments.len() == 1 && arguments[0].is_empty() {
                arguments.clear();
            }
            if arguments.len() != required {
                return Err(Diagnostic::new(
                    invocation,
                    format!(
                        "macro `{name}` expects {required} argument(s), but {} were given",
                        arguments.len()
                    ),
                ));
            }
        }
        Ok(arguments)
    }

    /// Replaces the parameters in the replacement list and performs `#`, `##`.
    fn substitute(
        &mut self,
        definition: &Macro,
        arguments: &[Vec<ExpToken>],
        invocation: Span,
        macro_hideset: &Hideset,
    ) -> Result<Vec<ExpToken>, Diagnostic> {
        let mut needs_expansion = vec![false; arguments.len()];
        if definition.params.is_some() {
            for (index, token) in definition.body.iter().enumerate() {
                let Some(parameter) = body_parameter(definition, token) else {
                    continue;
                };
                let previous_hash = index > 0 && is_punct(&definition.body[index - 1], "#");
                let previous_paste = index > 0 && is_punct(&definition.body[index - 1], "##");
                let next_paste = definition
                    .body
                    .get(index + 1)
                    .is_some_and(|token| is_punct(token, "##"));
                if !previous_hash && !previous_paste && !next_paste {
                    needs_expansion[parameter] = true;
                }
            }
        }

        let mut expanded_arguments: Vec<Option<Vec<ExpToken>>> = vec![None; arguments.len()];
        for (index, needed) in needs_expansion.iter().enumerate() {
            if *needed {
                expanded_arguments[index] = Some(self.expand_all(arguments[index].clone())?);
            }
        }

        let mut out: Vec<ExpToken> = Vec::new();
        // True when the immediately preceding replacement element produced no
        // tokens (an empty argument). Such an element is a placemarker for a
        // following `##`, not the last unrelated token in `out`.
        let mut previous_placemarker = false;
        let mut index = 0;
        while index < definition.body.len() {
            let body_token = &definition.body[index];

            if definition.params.is_some() && is_punct(body_token, "#") {
                let next = definition
                    .body
                    .get(index + 1)
                    .expect("validated stringify operand");
                let TokenKind::Ident(parameter) = &next.kind else {
                    unreachable!("validated stringify operand");
                };
                let parameter =
                    parameter_index(definition, parameter).expect("validated stringify parameter");
                let raw = stringify(arguments.get(parameter).map_or(&[], Vec::as_slice));
                let token = Token {
                    kind: TokenKind::StringLiteral {
                        prefix: String::new(),
                        raw,
                    },
                    span: invocation,
                    at_line_start: false,
                    leading_space: body_token.leading_space,
                };
                out.push(ExpToken {
                    token,
                    hideset: macro_hideset.clone(),
                    origin: Some(invocation),
                });
                previous_placemarker = false;
                index += 2;
                continue;
            }

            if is_punct(body_token, "##") {
                let right_token = definition
                    .body
                    .get(index + 1)
                    .expect("validated token-paste operand");
                let right = match body_parameter(definition, right_token) {
                    Some(parameter) => arguments.get(parameter).cloned().unwrap_or_default(),
                    None => vec![ExpToken {
                        token: right_token.clone(),
                        hideset: Hideset::empty(),
                        origin: Some(invocation),
                    }],
                };
                let left_placemarker = previous_placemarker;
                if right.is_empty() {
                    // A placemarker pasted with nothing stays a placemarker.
                    previous_placemarker = left_placemarker;
                    index += 2;
                    continue;
                }
                if left_placemarker {
                    for token in right {
                        out.push(apply_hideset(token, macro_hideset));
                    }
                    previous_placemarker = false;
                } else {
                    let left = out.pop().expect("non-placemarker left operand");
                    let text = format!("{}{}", spelling(&left.token), spelling(&right[0].token));
                    let mut pasted = self.paste(&text, invocation)?;
                    pasted.leading_space = left.token.leading_space;
                    pasted.at_line_start = false;
                    out.push(ExpToken {
                        token: pasted,
                        hideset: left.hideset.union(&right[0].hideset).union(macro_hideset),
                        origin: Some(invocation),
                    });
                    for token in right.into_iter().skip(1) {
                        out.push(apply_hideset(token, macro_hideset));
                    }
                    previous_placemarker = false;
                }
                index += 2;
                continue;
            }

            if let Some(parameter) = body_parameter(definition, body_token) {
                let argument = if needs_expansion.get(parameter).copied().unwrap_or(false) {
                    expanded_arguments[parameter]
                        .as_ref()
                        .expect("argument was expanded")
                } else {
                    &arguments[parameter]
                };
                previous_placemarker = argument.is_empty();
                for token in argument {
                    out.push(apply_hideset(token.clone(), macro_hideset));
                }
                index += 1;
                continue;
            }

            out.push(ExpToken {
                token: body_token.clone(),
                hideset: macro_hideset.clone(),
                origin: Some(invocation),
            });
            previous_placemarker = false;
            index += 1;
        }
        Ok(out)
    }

    /// Lexes a pasted spelling and requires exactly one token.
    fn paste(&mut self, text: &str, span: Span) -> Result<Token, Diagnostic> {
        let tokens = self
            .lex_synthetic("token paste", format!("{text}\n"))
            .map_err(|_| {
                Diagnostic::new(
                    span,
                    format!("pasting `{text}` does not form a valid preprocessing token"),
                )
            })?;
        let mut significant = tokens
            .into_iter()
            .filter(|token| !matches!(token.kind, TokenKind::Newline | TokenKind::Eof));
        let Some(first) = significant.next() else {
            return Err(Diagnostic::new(
                span,
                format!("pasting `{text}` does not form a valid preprocessing token"),
            ));
        };
        if significant.next().is_some() {
            return Err(Diagnostic::new(
                span,
                format!("pasting `{text}` forms more than one preprocessing token"),
            ));
        }
        Ok(first)
    }

    fn expand_builtin(&mut self, token: ExpToken, name: &str, out: &mut Vec<ExpToken>) {
        let span = token.span();
        let mut replacement = token.token.clone();
        replacement.span = span;
        replacement.at_line_start = false;
        if name == "__LINE__" {
            let (line, _) = self.sources.file(span.file).line_column(span.start);
            replacement.kind = TokenKind::Number(line.to_string());
        } else {
            let path = self.sources.file(span.file).path.display().to_string();
            replacement.kind = TokenKind::StringLiteral {
                prefix: String::new(),
                raw: escape_string(&path),
            };
        }
        out.push(ExpToken {
            token: replacement,
            hideset: token.hideset,
            origin: Some(span),
        });
    }
}

fn expand_all_inner(
    preprocessor: &mut Preprocessor<'_>,
    input: Vec<ExpToken>,
) -> Result<Vec<ExpToken>, Diagnostic> {
    let mut pending: Vec<ExpToken> = input.into_iter().rev().collect();
    let mut out = Vec::new();
    while let Some(token) = pending.pop() {
        preprocessor.expand_step(token, &mut pending, None, &mut out)?;
    }
    Ok(out)
}

/// Pops the next token from pending replacement tokens, or from the file.
/// Returns `None` at EOF and at a line-start `#` so the driver can run the
/// directive before expansion continues.
fn pull(pending: &mut Vec<ExpToken>, cursor: Option<&mut FileCursor>) -> Option<ExpToken> {
    if let Some(token) = pending.pop() {
        return Some(token);
    }
    next_source_token(cursor?)
}

fn next_source_token(cursor: &mut FileCursor) -> Option<ExpToken> {
    let mut crossed_newline = false;
    while cursor.pos < cursor.tokens.len() {
        let token = &cursor.tokens[cursor.pos];
        match &token.kind {
            TokenKind::Newline => {
                crossed_newline = true;
                cursor.pos += 1;
            }
            TokenKind::Eof => return None,
            TokenKind::Punct(punct) if punct == "#" && token.at_line_start => return None,
            TokenKind::Ident(name)
                if name == "use"
                    && super::parse_use_include(
                        cursor.tokens,
                        cursor.pos,
                        super::use_statement_end(cursor.tokens, cursor.pos),
                    )
                    .is_some() =>
            {
                return None;
            }
            _ => {
                let mut token = token.clone();
                if crossed_newline {
                    token.leading_space = true;
                }
                cursor.pos += 1;
                return Some(ExpToken::plain(token));
            }
        }
    }
    None
}

fn push_replacement(pending: &mut Vec<ExpToken>, replacement: Vec<ExpToken>) {
    pending.extend(replacement.into_iter().rev());
}

fn apply_hideset(mut token: ExpToken, hideset: &Hideset) -> ExpToken {
    token.hideset = token.hideset.union(hideset);
    token
}

fn body_parameter(definition: &Macro, token: &Token) -> Option<usize> {
    let TokenKind::Ident(name) = &token.kind else {
        return None;
    };
    parameter_index(definition, name)
}

fn join_variadic(extras: Vec<Vec<ExpToken>>, invocation: Span) -> Vec<ExpToken> {
    let mut out = Vec::new();
    for (index, argument) in extras.into_iter().enumerate() {
        if index > 0 {
            out.push(ExpToken::plain(Token {
                kind: TokenKind::Punct(",".to_string()),
                span: invocation,
                at_line_start: false,
                leading_space: false,
            }));
        }
        out.extend(argument);
    }
    out
}

/// `#` stringification: token spellings, single spaces between tokens that
/// were separated, and `\`/`"` escaped so the result is one string literal.
fn stringify(tokens: &[ExpToken]) -> String {
    let mut out = String::new();
    for (index, token) in tokens.iter().enumerate() {
        if index > 0 && token.token.leading_space {
            out.push(' ');
        }
        for character in spelling(&token.token).chars() {
            if character == '\\' || character == '"' {
                out.push('\\');
            }
            out.push(character);
        }
    }
    out
}

fn escape_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for character in text.chars() {
        if character == '\\' || character == '"' {
            out.push('\\');
        }
        out.push(character);
    }
    out
}
