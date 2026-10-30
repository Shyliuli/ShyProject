//! Macro table, macro definitions, and the token-level facts expansion needs.
//!
//! The table is keyed by name. A definition stores its replacement list as
//! ordinary [`Token`]s from the `#define` line, so the spellings the lexer
//! produced are preserved during expansion. Generated tokens (stringification,
//! token pasting, `-D` values) are lexed from synthetic [`SourceMap`] entries
//! by the expansion module.
//!
//! [`Hideset`] is an immutable set of macro names attached to every token that
//! came out of an expansion. A name in a token's hideset is not expanded again
//! for that token. Keeping the set on the token rather than on the whole
//! expansion is what stops `#define a a` and mutually recursive macros from
//! looping while still allowing an unrelated invocation of the same macro
//! later in the replacement list to expand normally.

use std::collections::HashMap;
use std::rc::Rc;

use crate::lexer::{Token, TokenKind};
use crate::source::{Diagnostic, Span};

use super::Preprocessor;

/// All currently defined macros.
#[derive(Default)]
pub(crate) struct MacroTable {
    entries: HashMap<String, Macro>,
}

impl MacroTable {
    pub(crate) fn get(&self, name: &str) -> Option<&Macro> {
        self.entries.get(name)
    }

    pub(crate) fn contains(&self, name: &str) -> bool {
        self.entries.contains_key(name)
    }

    pub(crate) fn insert(&mut self, name: String, definition: Macro) {
        self.entries.insert(name, definition);
    }

    pub(crate) fn remove(&mut self, name: &str) {
        self.entries.remove(name);
    }
}

/// A macro definition.
#[derive(Clone, Debug)]
pub(crate) struct Macro {
    /// `None` for an object-like macro, `Some(params)` for a function-like
    /// macro. The parameter list may be empty.
    pub(crate) params: Option<Vec<String>>,
    /// True when the last parameter is `...`.
    pub(crate) variadic: bool,
    /// Replacement list, exactly as written after the parameter list.
    pub(crate) body: Vec<Token>,
    /// Location of the macro name in its `#define`.
    pub(crate) span: Span,
}

impl Macro {
    pub(crate) fn object(body: Vec<Token>, span: Span) -> Self {
        Self {
            params: None,
            variadic: false,
            body,
            span,
        }
    }

    /// Structural equality, ignoring spans and line bookkeeping. Used for the
    /// standard "identical redefinition is allowed" rule.
    fn same_as(&self, other: &Macro) -> bool {
        self.variadic == other.variadic
            && self.params == other.params
            && self.body.len() == other.body.len()
            && self
                .body
                .iter()
                .zip(&other.body)
                .all(|(left, right)| left.kind == right.kind)
    }
}

/// An immutable linked set of macro names.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Hideset(Option<Rc<HidesetNode>>);

#[derive(Debug, PartialEq, Eq)]
struct HidesetNode {
    name: String,
    parent: Hideset,
}

impl Hideset {
    pub(crate) fn empty() -> Self {
        Self(None)
    }

    pub(crate) fn contains(&self, name: &str) -> bool {
        let mut current = self;
        while let Some(node) = &current.0 {
            if node.name == name {
                return true;
            }
            current = &node.parent;
        }
        false
    }

    /// Returns a set that also contains `name`.
    pub(crate) fn with(&self, name: &str) -> Self {
        if self.contains(name) {
            self.clone()
        } else {
            Self(Some(Rc::new(HidesetNode {
                name: name.to_string(),
                parent: self.clone(),
            })))
        }
    }

    pub(crate) fn union(&self, other: &Hideset) -> Self {
        let mut names = Vec::new();
        self.collect_names(&mut names);
        other.collect_names(&mut names);
        let mut result = Hideset::empty();
        for name in names {
            result = result.with(&name);
        }
        result
    }

    /// Names present in both sets. A function-like invocation intersects the
    /// hidesets of its name token and its closing `)` before adding the macro
    /// name. That is what allows a macro to be re-entered through a
    /// macro-expanded argument while still suppressing genuine recursion.
    pub(crate) fn intersection(&self, other: &Hideset) -> Self {
        let mut result = Hideset::empty();
        let mut current = self;
        while let Some(node) = &current.0 {
            if other.contains(&node.name) {
                result = result.with(&node.name);
            }
            current = &node.parent;
        }
        result
    }

    fn collect_names(&self, out: &mut Vec<String>) {
        let mut current = self;
        while let Some(node) = &current.0 {
            out.push(node.name.clone());
            current = &node.parent;
        }
    }
}

/// A token plus the expansion state that must travel with it.
#[derive(Clone, Debug)]
pub(crate) struct ExpToken {
    pub(crate) token: Token,
    pub(crate) hideset: Hideset,
    /// Location to blame for diagnostics and for `__LINE__`/`__FILE__` when
    /// this token is a macro replacement. `None` means "use `token.span`".
    pub(crate) origin: Option<Span>,
}

impl ExpToken {
    pub(crate) fn plain(token: Token) -> Self {
        Self {
            token,
            hideset: Hideset::empty(),
            origin: None,
        }
    }

    /// The location a user would point at.
    pub(crate) fn span(&self) -> Span {
        self.origin.unwrap_or(self.token.span)
    }
}

impl Preprocessor<'_> {
    /// Handles `#define`; `arguments` starts with the macro name.
    pub(crate) fn define_directive(
        &mut self,
        arguments: &[Token],
        hash_span: Span,
    ) -> Result<(), Diagnostic> {
        let Some(name_token) = arguments.first() else {
            return Err(Diagnostic::new(
                hash_span,
                "expected a macro name after #define",
            ));
        };
        let TokenKind::Ident(name) = &name_token.kind else {
            return Err(Diagnostic::new(
                name_token.span,
                "expected a macro name after #define",
            ));
        };
        if name == "defined" {
            return Err(Diagnostic::new(
                name_token.span,
                "`defined` is a preprocessor operator and cannot be defined as a macro",
            ));
        }

        let mut index = 1;
        let mut params = None;
        let mut variadic = false;
        let function_like = arguments
            .get(1)
            .is_some_and(|token| is_punct(token, "(") && !token.leading_space);
        if function_like {
            index = 2;
            let mut list = Vec::new();
            if arguments
                .get(index)
                .is_some_and(|token| is_punct(token, ")"))
            {
                index += 1;
            } else {
                loop {
                    let Some(parameter) = arguments.get(index) else {
                        return Err(Diagnostic::new(
                            hash_span,
                            "unterminated macro parameter list",
                        ));
                    };
                    let is_variadic = match &parameter.kind {
                        TokenKind::Ident(parameter_name) => {
                            if parameter_name == "__VA_ARGS__" {
                                return Err(Diagnostic::new(
                                    parameter.span,
                                    "`__VA_ARGS__` cannot be a named macro parameter",
                                ));
                            }
                            if list.contains(parameter_name) {
                                return Err(Diagnostic::new(
                                    parameter.span,
                                    format!("duplicate macro parameter `{parameter_name}`"),
                                ));
                            }
                            list.push(parameter_name.clone());
                            false
                        }
                        TokenKind::Punct(punct) if punct == "..." => true,
                        _ => {
                            return Err(Diagnostic::new(
                                parameter.span,
                                "expected a macro parameter name or `...`",
                            ));
                        }
                    };
                    variadic |= is_variadic;
                    index += 1;
                    match arguments.get(index).map(|token| &token.kind) {
                        Some(TokenKind::Punct(punct)) if punct == ")" => {
                            index += 1;
                            break;
                        }
                        Some(TokenKind::Punct(punct)) if punct == "," => {
                            if is_variadic {
                                return Err(Diagnostic::new(
                                    parameter.span,
                                    "`...` must be the last macro parameter",
                                ));
                            }
                            index += 1;
                        }
                        Some(_) => {
                            return Err(Diagnostic::new(
                                parameter.span,
                                "expected `,` or `)` in the macro parameter list",
                            ));
                        }
                        None => {
                            return Err(Diagnostic::new(
                                hash_span,
                                "unterminated macro parameter list",
                            ));
                        }
                    }
                }
            }
            params = Some(list);
        }

        let body = arguments[index..].to_vec();
        let definition = Macro {
            params,
            variadic,
            body,
            span: name_token.span,
        };
        self.validate_body(&definition)?;

        if let Some(previous) = self.macros.get(name) {
            if previous.same_as(&definition) {
                return Ok(());
            }
            let (line, _) = self
                .sources
                .file(previous.span.file)
                .line_column(previous.span.start);
            return Err(Diagnostic::new(
                name_token.span,
                format!("macro `{name}` redefined (previous definition on line {line})"),
            ));
        }
        self.macros.insert(name.clone(), definition);
        Ok(())
    }

    /// Handles `#undef`; `arguments` starts with the macro name.
    pub(crate) fn undef_directive(
        &mut self,
        arguments: &[Token],
        hash_span: Span,
    ) -> Result<(), Diagnostic> {
        let [
            Token {
                kind: TokenKind::Ident(name),
                ..
            },
        ] = arguments
        else {
            return Err(Diagnostic::new(
                hash_span,
                "expected a single macro name after #undef",
            ));
        };
        self.remove_macro(name);
        Ok(())
    }

    /// Checks that `#` and `##` have valid operands and that
    /// `__VA_ARGS__` belongs to a variadic macro.
    fn validate_body(&self, definition: &Macro) -> Result<(), Diagnostic> {
        for (index, token) in definition.body.iter().enumerate() {
            if is_punct(token, "#") {
                if definition.params.is_none() {
                    return Err(Diagnostic::new(
                        token.span,
                        "`#` (stringify) is only valid in a function-like macro",
                    ));
                }
                let Some(next) = definition.body.get(index + 1) else {
                    return Err(Diagnostic::new(
                        token.span,
                        "`#` must be followed by a macro parameter",
                    ));
                };
                let TokenKind::Ident(parameter) = &next.kind else {
                    return Err(Diagnostic::new(
                        token.span,
                        "`#` must be followed by a macro parameter",
                    ));
                };
                if parameter_index(definition, parameter).is_none() {
                    return Err(Diagnostic::new(
                        token.span,
                        format!("`#{parameter}` does not name a macro parameter"),
                    ));
                }
            }
            if is_punct(token, "##") {
                if index == 0 || index + 1 == definition.body.len() {
                    return Err(Diagnostic::new(
                        token.span,
                        "`##` cannot appear at the start or end of a macro replacement list",
                    ));
                }
                if is_punct(&definition.body[index + 1], "##") {
                    return Err(Diagnostic::new(
                        token.span,
                        "`##` cannot be followed by another `##`",
                    ));
                }
            }
        }

        if !definition.variadic
            && let Some(token) = definition.body.iter().find(
                |token| matches!(&token.kind, TokenKind::Ident(name) if name == "__VA_ARGS__"),
            )
        {
            return Err(Diagnostic::new(
                token.span,
                "`__VA_ARGS__` is only valid in a variadic macro",
            ));
        }

        Ok(())
    }
}

/// Index of `name` in the invocation's argument vector. The variadic argument
/// is stored after the named parameters.
pub(crate) fn parameter_index(definition: &Macro, name: &str) -> Option<usize> {
    if name == "__VA_ARGS__" && definition.variadic {
        return Some(definition.params.as_ref().map_or(0, Vec::len));
    }
    definition
        .params
        .as_ref()?
        .iter()
        .position(|parameter| parameter == name)
}

pub(crate) fn is_punct(token: &Token, wanted: &str) -> bool {
    matches!(&token.kind, TokenKind::Punct(punct) if punct == wanted)
}

/// Source spelling of a token, independent of any source text. Macro-generated
/// tokens keep the spelling stored in the token itself.
pub(crate) fn spelling(token: &Token) -> String {
    match &token.kind {
        TokenKind::Ident(text) => text.clone(),
        TokenKind::Number(text) => text.clone(),
        TokenKind::StringLiteral { prefix, raw } => format!("{prefix}\"{raw}\""),
        TokenKind::CharLiteral { prefix, raw } => format!("{prefix}'{raw}'"),
        TokenKind::Punct(text) => text.clone(),
        TokenKind::Newline => "\n".to_string(),
        TokenKind::Eof => String::new(),
    }
}
