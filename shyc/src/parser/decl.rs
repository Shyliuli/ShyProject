//! File-scope items, declarations, `impl` blocks, and the typedef/tag prescan.

use std::collections::HashSet;

use crate::ast::{
    BuiltinType, DeclSpecs, Declaration, Declarator, DerivedDeclarator, FunctionDecl, ImplBlock,
    InitDeclarator, Item, StorageClass, TraitDecl, TraitMethod, TypeName, TypeSpecifier,
};
use crate::lexer::{Token, TokenKind};
use crate::source::{Diagnostic, Span};

use super::Parser;
use super::cursor::is_keyword;

/// File-scope type names collected before the real parse.
pub(crate) struct GlobalNames {
    pub(crate) typedefs: HashSet<String>,
    pub(crate) tags: HashSet<String>,
    pub(crate) traits: HashSet<String>,
}

/// Collects file-scope typedef names, tags, and trait names in one cheap pass.
///
/// Bodies are skipped by brace matching, so function-local typedefs and tags
/// never leak. Unknown identifiers are accepted as type names, which makes the
/// pass insensitive to declaration order: the real parse gets the complete
/// file-scope sets no matter where a typedef or tag is written.
pub(crate) fn prescan(tokens: &[Token]) -> GlobalNames {
    let mut parser = Parser::new(tokens.to_vec(), true);
    parser.prescan_items();
    GlobalNames {
        typedefs: std::mem::take(&mut parser.scopes[0].typedefs),
        tags: std::mem::take(&mut parser.scopes[0].tags),
        traits: std::mem::take(&mut parser.scopes[0].traits),
    }
}

impl Parser {
    // -- top-level items --------------------------------------------------

    /// Parses one file-scope item: a declaration, a function definition, an
    /// `impl` block, or a `trait` declaration.
    pub(crate) fn item(&mut self) -> Result<Item, Diagnostic> {
        if self.at_ident("trait") {
            return Ok(Item::Trait(self.trait_decl()?));
        }
        if self.at_ident("impl") {
            return Ok(Item::Impl(self.impl_block()?));
        }
        if self.at_fn_declaration() {
            return self.fn_item();
        }
        if self.at_ident("asm") || self.at_punct("asm!") {
            return Err(Diagnostic::new(
                self.here(),
                "file-scope assembly is not supported",
            ));
        }
        if self.at_let_declaration() {
            return Err(Diagnostic::new(
                self.here(),
                "a `let` declaration is only allowed inside a function body",
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

    // -- Shy `fn` declarations --------------------------------------------

    /// Whether the cursor starts `fn name(...)` or `fn name<...>(...)`.
    ///
    /// `fn` is a soft keyword: `int fn;` and calls to a function named `fn`
    /// remain ordinary C.
    pub(crate) fn at_fn_declaration(&self) -> bool {
        self.at_ident("fn")
            && matches!(&self.peek_ahead(1).kind, TokenKind::Ident(name) if !is_keyword(name))
            && (self.at_punct_ahead(2, "(") || self.at_punct_ahead(2, "<"))
    }

    /// Parses `fn name [<generics>] (params) [-> return-type]` and leaves the
    /// generic-parameter scope open for the caller's body or prototype.
    fn fn_signature(&mut self) -> Result<(Span, DeclSpecs, Declarator), Diagnostic> {
        let start = self.bump().span;
        self.push_scope();
        let name = self.expect_ident()?;
        let generics = if self.at_punct("<") {
            self.generic_params()?
        } else {
            Vec::new()
        };
        let mut function = self.parameter_list()?;
        function.span = name.span.join(self.last_span());
        function.generics = generics;

        let (specs, return_derived) = if self.eat_punct("->") {
            let TypeName {
                specs, declarator, ..
            } = self.type_name()?;
            (
                specs,
                declarator
                    .map(|declarator| declarator.derived)
                    .unwrap_or_default(),
            )
        } else {
            (
                DeclSpecs {
                    span: name.span,
                    storage: None,
                    is_inline: false,
                    is_noreturn: false,
                    qualifiers: Vec::new(),
                    type_specs: vec![TypeSpecifier::Builtin(BuiltinType::Void)],
                },
                Vec::new(),
            )
        };

        let mut derived = vec![DerivedDeclarator::Function(function)];
        derived.extend(return_derived);
        let declarator = Declarator {
            span: start.join(self.last_span()),
            name: Some(name),
            derived,
        };
        Ok((start, specs, declarator))
    }

    fn fn_item(&mut self) -> Result<Item, Diagnostic> {
        let (start, specs, declarator) = self.fn_signature()?;
        let declarator_span = declarator.span;
        let item = if self.at_punct("{") {
            let body = self.block_with_params(Some(&declarator))?;
            Item::Function(FunctionDecl {
                span: start.join(body.span),
                specs,
                declarator,
                body,
            })
        } else {
            let semicolon = self.expect_punct(";")?;
            Item::Decl(Declaration {
                span: start.join(semicolon),
                specs,
                declarators: vec![InitDeclarator {
                    span: declarator_span,
                    declarator,
                    init: None,
                }],
            })
        };
        self.pop_scope();
        Ok(item)
    }

    fn fn_method(&mut self) -> Result<FunctionDecl, Diagnostic> {
        let (start, specs, declarator) = self.fn_signature()?;
        if !self.at_punct("{") {
            return Err(Diagnostic::new(
                self.here(),
                "method declarations inside impl must have a body",
            ));
        }
        let body = self.block_with_params(Some(&declarator))?;
        self.pop_scope();
        Ok(FunctionDecl {
            span: start.join(body.span),
            specs,
            declarator,
            body,
        })
    }

    fn impl_block(&mut self) -> Result<ImplBlock, Diagnostic> {
        let start = self.bump().span;
        // `impl Trait for Type` and `impl Trait<Args> for Type` are trait
        // implementations; `impl Type` keeps its old inherent meaning.
        let (trait_name, trait_args) = if self.at_trait_impl_header() {
            let name = self.expect_ident()?;
            let args = if self.at_punct("<") {
                self.generic_args()?
            } else {
                Vec::new()
            };
            self.expect_keyword("for")?;
            (Some(name), args)
        } else {
            (None, Vec::new())
        };
        let self_type = self.type_name()?;
        self.expect_punct("{")?;

        self.impl_depth += 1;
        self.push_scope();
        let mut methods = Vec::new();
        while !self.at_punct("}") {
            if self.at_eof() {
                return Err(Diagnostic::new(self.here(), "unterminated impl block"));
            }
            if self.at_fn_declaration() {
                methods.push(self.fn_method()?);
                continue;
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
            trait_name,
            trait_args,
            self_type,
            methods,
        })
    }

    /// Whether the cursor starts `Trait for` or `Trait<Args> for`.
    ///
    /// The generic form needs a speculative parse because `impl Name<Args>`
    /// without `for` is an inherent impl. `probe` restores the cursor after
    /// deciding.
    fn at_trait_impl_header(&mut self) -> bool {
        if !matches!(&self.peek().kind, TokenKind::Ident(word) if !is_keyword(word)) {
            return false;
        }
        if self.at_ident_ahead(1, "for") {
            return true;
        }
        if !self.at_punct_ahead(1, "<") {
            return false;
        }
        self.probe(|parser| {
            parser.bump();
            parser.generic_args()?;
            Ok(parser.at_ident("for"))
        })
        .unwrap_or(false)
    }

    /// Parses `trait Name { method signatures }`.
    ///
    /// A method is written like a function declaration and ends with `;`, or
    /// with the `fn` spelling and no body. `self` is allowed in those
    /// signatures, so the trait can be checked against each implementing type.
    fn trait_decl(&mut self) -> Result<TraitDecl, Diagnostic> {
        let start = self.bump().span;
        let name = self.expect_ident()?;
        self.declare_trait(&name.text);
        self.push_scope();
        let generics = if self.at_punct("<") {
            self.generic_params()?
        } else {
            Vec::new()
        };
        self.expect_punct("{")?;

        self.impl_depth += 1;
        let mut methods = Vec::new();
        while !self.at_punct("}") {
            if self.at_eof() {
                return Err(Diagnostic::new(
                    self.here(),
                    "unterminated trait declaration",
                ));
            }
            methods.push(self.trait_method()?);
        }
        let close = self.expect_punct("}")?;
        self.pop_scope();
        self.impl_depth -= 1;

        Ok(TraitDecl {
            span: start.join(close),
            name,
            generics,
            methods,
        })
    }

    /// One method declaration inside a trait. A body is rejected here; the
    /// declarator's shape and storage class are checked by semantic analysis.
    fn trait_method(&mut self) -> Result<TraitMethod, Diagnostic> {
        if self.at_fn_declaration() {
            let (start, specs, declarator) = self.fn_signature()?;
            if self.at_punct("{") {
                return Err(Diagnostic::new(
                    self.here(),
                    "trait method declarations cannot have a body",
                ));
            }
            let semicolon = self.expect_punct(";")?;
            self.pop_scope();
            return Ok(TraitMethod {
                span: start.join(semicolon),
                specs,
                declarator,
            });
        }

        let specs = self.decl_specs()?;
        if self.at_punct(";") {
            return Err(Diagnostic::new(
                self.here(),
                "a trait member must declare a method",
            ));
        }
        let declarator = self.declarator(false)?;
        if self.at_punct("{") {
            return Err(Diagnostic::new(
                self.here(),
                "trait method declarations cannot have a body",
            ));
        }
        let semicolon = self.expect_punct(";")?;
        Ok(TraitMethod {
            span: specs.span.join(semicolon),
            specs,
            declarator,
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
        if self.at_ident("trait") {
            self.bump();
            let name = self.expect_ident()?;
            self.declare_trait(&name.text);
            self.push_scope();
            if self.at_punct("<") {
                self.generic_params()?;
            }
            self.expect_punct("{")?;
            self.pop_scope();
            return self.skip_braced_body();
        }
        if self.at_ident("impl") {
            self.bump();
            // Skip the trait name and its arguments of `impl Trait<Args> for
            // Type` the same way the real parser recognizes them.
            if self.at_trait_impl_header() {
                self.bump();
                if self.at_punct("<") {
                    self.generic_args()?;
                }
                self.bump();
            }
            self.type_name()?;
            self.expect_punct("{")?;
            return self.skip_braced_body();
        }
        if self.at_fn_declaration() {
            return self.prescan_fn();
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

    /// Skips a `fn` item during prescan. The signature goes through the real
    /// parser so generic names and return types are consumed the same way;
    /// only the body is skipped.
    fn prescan_fn(&mut self) -> Result<(), Diagnostic> {
        let parsed = (|| {
            self.fn_signature()?;
            if self.at_punct("{") {
                self.bump();
                self.skip_braced_body()?;
            } else {
                self.expect_punct(";")?;
            }
            Ok(())
        })();
        self.pop_scope();
        parsed
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
            } else if self.at_punct("<") {
                if !self.skip_angle_group() {
                    self.bump();
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
                || self.at_ident("trait")
                || self.at_fn_declaration()
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
            if self.at_punct("<") {
                if !self.skip_angle_group() {
                    self.bump();
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

    /// Skips one `<...>` group while prescanning, if it is really a group.
    ///
    /// A `<` in an initializer is an ordinary comparison. The attempt is
    /// speculative and rolls back when the group does not close before the end
    /// of the declaration, so ordinary C prescan is unaffected.
    fn skip_angle_group(&mut self) -> bool {
        self.try_parse(|parser| {
            parser.expect_punct("<")?;
            let mut depth = 1usize;
            while depth > 0 {
                if parser.at_eof() {
                    return Err(Diagnostic::new(parser.here(), "unbalanced '<'"));
                }
                if parser.at_punct("<") {
                    depth += 1;
                    parser.bump();
                } else if parser.at_punct(">>") {
                    parser.close_angle()?;
                    depth -= 1;
                } else if parser.at_punct(">") {
                    depth -= 1;
                    parser.bump();
                } else if parser.at_punct(";") || parser.at_punct("{") || parser.at_punct("}") {
                    return Err(Diagnostic::new(parser.here(), "not an angle group"));
                } else {
                    parser.bump();
                }
            }
            Ok(())
        })
        .is_some()
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
