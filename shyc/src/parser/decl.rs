//! File-scope items, declarations, `impl` blocks, and the typedef/tag prescan.

use std::collections::HashSet;

use crate::ast::{
    DeclSpecs, Declaration, Declarator, DerivedDeclarator, FunctionDecl, ImplBlock, InitDeclarator,
    Item, StorageClass,
};
use crate::lexer::{Token, TokenKind};
use crate::source::Diagnostic;

use super::Parser;

/// File-scope type names collected before the real parse.
pub(crate) struct GlobalNames {
    pub(crate) typedefs: HashSet<String>,
    pub(crate) tags: HashSet<String>,
}

/// Collects file-scope typedef names and tags in one cheap pass.
///
/// Bodies are skipped by brace matching, so function-local typedefs and tags
/// never leak. Unknown identifiers are accepted as type names, which makes the
/// pass insensitive to declaration order: the real parse gets the complete
/// file-scope sets no matter where a typedef or tag is written.
pub(crate) fn prescan(tokens: &[Token]) -> GlobalNames {
    let mut parser = Parser::new(tokens, true);
    parser.prescan_items();
    GlobalNames {
        typedefs: std::mem::take(&mut parser.scopes[0].typedefs),
        tags: std::mem::take(&mut parser.scopes[0].tags),
    }
}

impl Parser<'_> {
    // -- top-level items --------------------------------------------------

    /// Parses one file-scope item: a declaration, a function definition, or an
    /// `impl` block.
    pub(crate) fn item(&mut self) -> Result<Item, Diagnostic> {
        if self.at_ident("impl") {
            return Ok(Item::Impl(self.impl_block()?));
        }
        if self.at_ident("asm") || self.at_punct("asm!") {
            return Err(Diagnostic::new(
                self.here(),
                "file-scope assembly is not supported",
            ));
        }

        let specs = self.decl_specs()?;
        let no_specs =
            specs.type_specs.is_empty() && specs.storage.is_none() && specs.qualifiers.is_empty();

        if self.eat_punct(";") {
            // An empty declaration, and the common `struct S { ... };`.
            return Ok(Item::Decl(Declaration {
                span: specs.span,
                specs,
                declarators: Vec::new(),
            }));
        }

        if no_specs && !self.starts_implicit_int_declarator() {
            return Err(self.unknown_type());
        }
        if self.at_eof() {
            // `struct S { ... }` with the closing `;` left out.
            return Err(self.expected("';'"));
        }

        let first = self.declarator(false)?;
        if self.at_punct("{") {
            if specs.storage == Some(StorageClass::Typedef) {
                return Err(Diagnostic::new(
                    self.here(),
                    "a typedef cannot have a function body",
                ));
            }
            return Ok(Item::Function(self.function_definition(specs, first)?));
        }

        let declarators = self.init_declarators(specs.storage, first)?;
        let semicolon = self.expect_punct(";")?;
        Ok(Item::Decl(Declaration {
            span: specs.span.join(semicolon),
            specs,
            declarators,
        }))
    }

    /// A declaration used inside a block or as a `for` initializer.
    pub(crate) fn declaration(&mut self) -> Result<Declaration, Diagnostic> {
        let specs = self.decl_specs()?;
        if specs.type_specs.is_empty() && specs.storage.is_none() && specs.qualifiers.is_empty() {
            return Err(self.expected("a declaration"));
        }

        if self.eat_punct(";") {
            return Ok(Declaration {
                span: specs.span,
                specs,
                declarators: Vec::new(),
            });
        }

        let first = self.declarator(false)?;
        if self.at_punct("{") {
            return Err(Diagnostic::new(
                self.here(),
                "nested function definitions are not supported",
            ));
        }
        let declarators = self.init_declarators(specs.storage, first)?;
        let semicolon = self.expect_punct(";")?;
        Ok(Declaration {
            span: specs.span.join(semicolon),
            specs,
            declarators,
        })
    }

    /// Parses `first ("," declarator initializer?)*` and records the declared
    /// names in the current syntax scope.
    fn init_declarators(
        &mut self,
        storage: Option<StorageClass>,
        first: Declarator,
    ) -> Result<Vec<InitDeclarator>, Diagnostic> {
        let mut declarators = Vec::new();
        let mut declarator = first;
        loop {
            if storage == Some(StorageClass::Typedef) {
                match &declarator.name {
                    Some(name) => self.declare_typedef(&name.text),
                    None => {
                        return Err(Diagnostic::new(
                            declarator.span,
                            "a typedef must declare a name",
                        ));
                    }
                }
            } else if self.scopes.len() > 1
                && let Some(name) = &declarator.name
            {
                // File-scope object names are not tracked: Shy resolves tags
                // and typedefs across the whole unit, and a file-scope name
                // would otherwise hide a later typedef for the parser alone.
                self.declare_value(&name.text);
            }

            let init = if self.eat_punct("=") {
                Some(self.initializer()?)
            } else {
                None
            };
            let span = match &init {
                Some(init) => declarator.span.join(init.span),
                None => declarator.span,
            };
            declarators.push(InitDeclarator {
                span,
                declarator,
                init,
            });

            if !self.eat_punct(",") {
                break;
            }
            declarator = self.declarator(false)?;
        }
        Ok(declarators)
    }

    fn function_definition(
        &mut self,
        specs: DeclSpecs,
        declarator: Declarator,
    ) -> Result<FunctionDecl, Diagnostic> {
        if !matches!(
            declarator.derived.first(),
            Some(DerivedDeclarator::Function(_))
        ) {
            return Err(Diagnostic::new(
                declarator.span,
                "a function definition needs a function declarator",
            ));
        }
        let body = self.block_with_params(Some(&declarator))?;
        Ok(FunctionDecl {
            span: specs.span.join(body.span),
            specs,
            declarator,
            body,
        })
    }

    fn impl_block(&mut self) -> Result<ImplBlock, Diagnostic> {
        let start = self.bump().span;
        let self_type = self.type_name()?;
        self.expect_punct("{")?;

        self.impl_depth += 1;
        self.push_scope();
        let mut methods = Vec::new();
        while !self.at_punct("}") {
            if self.at_eof() {
                return Err(Diagnostic::new(self.here(), "unterminated impl block"));
            }
            let specs = self.decl_specs()?;
            if self.at_punct(";") {
                return Err(Diagnostic::new(
                    self.here(),
                    "method declarations inside impl must have a body",
                ));
            }
            let declarator = self.declarator(false)?;
            if !self.at_punct("{") {
                return Err(Diagnostic::new(
                    self.here(),
                    "method declarations inside impl must have a body",
                ));
            }
            methods.push(self.function_definition(specs, declarator)?);
        }
        let close = self.expect_punct("}")?;
        self.pop_scope();
        self.impl_depth -= 1;

        Ok(ImplBlock {
            span: start.join(close),
            self_type,
            methods,
        })
    }

    /// A C89 implicit-`int` declaration or definition. Only reachable at file
    /// scope with no specifier at all: `main() { ... }`, `x;`.
    fn starts_implicit_int_declarator(&self) -> bool {
        if self.ident_here().is_none() {
            return false;
        }
        matches!(
            self.peek_ahead(1).kind,
            TokenKind::Punct(ref punct)
                if matches!(punct.as_str(), "(" | "[" | ";" | "," | "=")
        )
    }

    fn unknown_type(&self) -> Diagnostic {
        match self.ident_here() {
            Some(name) => Diagnostic::new(self.here(), format!("unknown type name '{name}'")),
            None => Diagnostic::new(
                self.here(),
                format!("expected a declaration, found {}", self.describe_here()),
            ),
        }
    }

    // -- prescan ----------------------------------------------------------

    fn prescan_items(&mut self) {
        while !self.at_eof() {
            let before = self.pos;
            if self.prescan_item().is_err() {
                self.recover_prescan_error();
            }
            if self.pos == before {
                // Guarantee progress on a diagnostic that consumed nothing.
                self.bump();
            }
        }
    }

    fn prescan_item(&mut self) -> Result<(), Diagnostic> {
        if self.at_ident("impl") {
            self.bump();
            self.type_name()?;
            self.expect_punct("{")?;
            return self.skip_braced_body();
        }

        let specs = self.decl_specs()?;
        if self.eat_punct(";") {
            return Ok(());
        }

        let first = self.declarator(false)?;
        if specs.storage == Some(StorageClass::Typedef) {
            let mut declarator = first;
            loop {
                if let Some(name) = &declarator.name {
                    self.declare_typedef(&name.text);
                }
                if !self.eat_punct(",") {
                    break;
                }
                declarator = self.declarator(false)?;
            }
            self.expect_punct(";")?;
        } else if self.at_punct("{") {
            self.bump();
            self.skip_braced_body()?;
        } else {
            self.skip_to_semicolon();
        }
        Ok(())
    }

    /// Skips the rest of a declaration, including initializer braces, up to and
    /// including its `;`.
    fn skip_to_semicolon(&mut self) {
        loop {
            if self.at_eof() || self.eat_punct(";") {
                return;
            }
            if self.at_punct("{") {
                self.bump();
                if self.skip_braced_body().is_err() {
                    return;
                }
            } else if self.at_punct("(") {
                self.bump();
                if self.skip_balanced_group("(", ")").is_err() {
                    return;
                }
            } else if self.at_punct("[") {
                self.bump();
                if self.skip_balanced_group("[", "]").is_err() {
                    return;
                }
            } else {
                self.bump();
            }
        }
    }

    /// Error recovery for prescan: skip to the end of the current item, but
    /// stop in front of a keyword that can start the next file-scope item so a
    /// typedef is never swallowed.
    fn recover_prescan_error(&mut self) {
        loop {
            if self.at_eof() || self.eat_punct(";") {
                return;
            }
            if self.at_punct("{") {
                self.bump();
                if self.skip_braced_body().is_err() {
                    return;
                }
                continue;
            }
            if self.at_ident("typedef")
                || self.at_ident("struct")
                || self.at_ident("union")
                || self.at_ident("enum")
                || self.at_ident("impl")
            {
                return;
            }
            if self.at_punct("(") {
                self.bump();
                if self.skip_balanced_group("(", ")").is_err() {
                    return;
                }
                continue;
            }
            self.bump();
        }
    }

    // -- brace/bracket skipping (prescan and recovery) --------------------

    pub(crate) fn skip_braced_body(&mut self) -> Result<(), Diagnostic> {
        self.skip_balanced_group("{", "}")
    }

    /// Skips to the matching `close`, assuming `open` was already consumed.
    pub(crate) fn skip_balanced_group(
        &mut self,
        open: &str,
        close: &str,
    ) -> Result<(), Diagnostic> {
        let mut depth = 1usize;
        while depth > 0 {
            if self.at_eof() {
                return Err(Diagnostic::new(self.here(), format!("unbalanced '{open}'")));
            }
            if self.at_punct(open) {
                depth += 1;
            } else if self.at_punct(close) {
                depth -= 1;
            }
            self.bump();
        }
        Ok(())
    }
}
