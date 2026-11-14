//! Declaration specifiers, declarators, and aggregate definitions.
//!
//! Declarators are built **from the declared name outward**, exactly as
//! [`crate::ast::Declarator`] documents: `int *a[3]` is `[Array, Pointer]`,
//! while `int (*a)[3]` is `[Pointer, Array]`. Later stages therefore never have
//! to re-solve the parentheses precedence puzzle.

use crate::ast::{
    ArrayDeclarator, ArrayLength, BuiltinType, DeclSpecs, Declarator, DerivedDeclarator, EnumSpec,
    Enumerator, FunctionDeclarator, GenericArg, GenericArgKind, GenericParam, GenericParamKind,
    GenericSpec, MemberDecl, MemberDeclarator, ParamDecl, PointerDeclarator, Qualifier, RecordKind,
    RecordSpec, StorageClass, TraitBound, TypeName, TypeSpecifier, VariantField, VariantPayload,
};
use crate::lexer::TokenKind;
use crate::source::Diagnostic;

use super::Parser;
use super::cursor::unsupported_message;
use super::probe::{Answer, Arguments, Place};

impl Parser {
    // -- declaration specifiers ------------------------------------------

    /// Parses the specifier list in front of a declaration or type name.
    ///
    /// The list may be empty: old-style C allows an omitted `int`, and the
    /// parser preserves that instead of inventing a specifier.
    /// Expands a `const fn` call that stands in a type position.
    ///
    /// The call must be `name(...)` for a const fn whose result is a token
    /// sequence; the tokens are parsed back as one complete type name, so an
    /// expansion can never swallow the declarator that follows it.
    fn expand_type_call(&mut self) -> Result<Option<TypeName>, Diagnostic> {
        let (index, span) = {
            let index = match &self.peek().kind {
                TokenKind::Ident(name) if self.at_punct_ahead(1, "(") => {
                    self.const_fns.iter().position(|f| &f.name == name)
                }
                _ => None,
            };
            match index {
                Some(index) => (index, self.here()),
                None => return Ok(None),
            }
        };
        let name = self.const_fns[index].name.clone();
        self.record_const_call(index, span);
        self.bump();
        let (tokens, close) = self.capture_token_argument()?;
        let span = span.join(close);
        // Probe mode has no way to parse this declaration without the type, so
        // an unanswered call is reported, never guessed.
        let arguments = Arguments::Tokens(tokens);
        let key = self.probe_key(
            index,
            Place::Type,
            span,
            super::probe::fingerprint(&arguments),
        );
        let tokens = match self.probe_answer(&key) {
            Some(Answer::Tokens(tokens)) => tokens,
            Some(Answer::Failed { message, span: at }) => {
                return Err(Diagnostic::new(at.unwrap_or(span), message));
            }
            _ => {
                // The type decides how the rest of this declaration parses, so
                // the call cannot be filled in later in the same pass. The
                // request is recorded and the declaration falls back to `int`:
                // a round with requests emits nothing, and the next one parses
                // the file again with the type in hand.
                self.probe_record(super::probe::ProbeRequest {
                    key,
                    function: index,
                    name: name.clone(),
                    place: Place::Type,
                    site: span,
                    arguments,
                });
                return Ok(Some(TypeName {
                    span,
                    specs: DeclSpecs {
                        span,
                        storage: None,
                        is_inline: false,
                        is_noreturn: false,
                        is_pub: false,
                        is_shy_fn: false,
                        qualifiers: Vec::new(),
                        type_specs: vec![TypeSpecifier::Builtin(BuiltinType::Int)],
                    },
                    declarator: None,
                }));
            }
        };
        parse_type_fragment(tokens, span, self)
            .map(Some)
            .map_err(|error| self.fragment_error(error, index))
    }

    pub(crate) fn decl_specs(&mut self) -> Result<DeclSpecs, Diagnostic> {
        // A const fn may stand where the type of a declaration is written.
        if self.const_fn_call_ahead().is_some() {
            if let Some(type_name) = self.expand_type_call()? {
                self.pending_declarator = type_name.declarator;
                return Ok(type_name.specs);
            }
        }
        let start = self.here();
        let before = self.last_span();
        let mut specs = DeclSpecs {
            span: start,
            storage: None,
            is_inline: false,
            is_noreturn: false,
            is_pub: false,
            is_shy_fn: false,
            qualifiers: Vec::new(),
            type_specs: Vec::new(),
        };

        while let TokenKind::Ident(word) = self.peek().kind.clone() {
            if let Some(builtin) = builtin_type(&word) {
                self.bump();
                specs.type_specs.push(TypeSpecifier::Builtin(builtin));
                continue;
            }

            match word.as_str() {
                "typedef" => self.set_storage(&mut specs, StorageClass::Typedef)?,
                "extern" => self.set_storage(&mut specs, StorageClass::Extern)?,
                "static" => self.set_storage(&mut specs, StorageClass::Static)?,
                "auto" => self.set_storage(&mut specs, StorageClass::Auto)?,
                "register" => self.set_storage(&mut specs, StorageClass::Register)?,
                "inline" | "__inline" | "__inline__" => {
                    specs.is_inline = true;
                    self.bump();
                }
                "_Noreturn" => {
                    specs.is_noreturn = true;
                    self.bump();
                }
                // `pub` 是导出标记：不写就是文件级私有。
                "pub" => {
                    specs.is_pub = true;
                    self.bump();
                }
                "const" => {
                    specs.qualifiers.push(Qualifier::Const);
                    self.bump();
                }
                "volatile" => {
                    specs.qualifiers.push(Qualifier::Volatile);
                    self.bump();
                }
                "restrict" | "__restrict" | "__restrict__" => {
                    specs.qualifiers.push(Qualifier::Restrict);
                    self.bump();
                }
                "_Atomic" if self.at_punct_ahead(1, "(") => {
                    if !specs.type_specs.is_empty() {
                        break;
                    }
                    self.bump();
                    self.expect_punct("(")?;
                    let inner = self.type_name()?;
                    self.expect_punct(")")?;
                    specs.type_specs.push(TypeSpecifier::Atomic(inner));
                }
                "_Atomic" => {
                    specs.qualifiers.push(Qualifier::Atomic);
                    self.bump();
                }
                "struct" | "union" => {
                    if !specs.type_specs.is_empty() {
                        break;
                    }
                    let kind = if word == "struct" {
                        RecordKind::Struct
                    } else {
                        RecordKind::Union
                    };
                    let record = self.record_spec(kind)?;
                    specs.type_specs.push(TypeSpecifier::Record(record));
                }
                "enum" => {
                    if !specs.type_specs.is_empty() {
                        break;
                    }
                    let enumeration = self.enum_spec()?;
                    specs.type_specs.push(TypeSpecifier::Enum(enumeration));
                }
                "self" => {
                    if self.impl_depth == 0 || !specs.type_specs.is_empty() {
                        break;
                    }
                    self.bump();
                    specs.type_specs.push(TypeSpecifier::SelfType);
                }
                _ => {
                    if let Some(message) = unsupported_message(&word) {
                        return Err(Diagnostic::new(self.here(), message));
                    }
                    // A user type name is only a specifier if no type specifier
                    // has appeared yet; this is what makes `T T;` parse as a
                    // declaration of a variable named `T`.
                    if !specs.type_specs.is_empty() {
                        break;
                    }
                    if !self.prescan && !self.is_type_name(&word) {
                        break;
                    }
                    let name = self.expect_ident()?;
                    if self.at_punct("<") {
                        let args = self.generic_args()?;
                        let span = name.span.join(self.last_span());
                        specs.type_specs.push(TypeSpecifier::Generic(GenericSpec {
                            span,
                            name,
                            args,
                        }));
                    } else {
                        specs.type_specs.push(TypeSpecifier::TypedefName(name));
                    }
                }
            }
        }

        // An empty specifier list (implicit `int`) gets the current token's
        // span instead of a range running back into the previous token.
        specs.span = if self.last_span() == before {
            start
        } else {
            self.finish(start)
        };
        Ok(specs)
    }

    fn set_storage(
        &mut self,
        specs: &mut DeclSpecs,
        class: StorageClass,
    ) -> Result<(), Diagnostic> {
        if specs.storage.is_some() {
            return Err(Diagnostic::new(
                self.here(),
                "multiple storage-class specifiers are not allowed",
            ));
        }
        specs.storage = Some(class);
        self.bump();
        Ok(())
    }

    /// Whether the token `ahead` tokens from the cursor can start a type name.
    ///
    /// This is the C parser's classic typedef-name question, plus Shy tags.
    /// Unsupported keywords answer `true` so that the user gets their specific
    /// diagnostic instead of a confusing "expected an expression".
    pub(crate) fn starts_type_name(&self, ahead: usize) -> bool {
        let TokenKind::Ident(word) = &self.peek_ahead(ahead).kind else {
            return false;
        };
        if builtin_type(word).is_some() {
            return true;
        }
        match word.as_str() {
            "typedef" | "extern" | "static" | "auto" | "register" => true,
            "const" | "volatile" | "restrict" | "__restrict" | "__restrict__" | "_Atomic"
            | "inline" | "__inline" | "__inline__" | "_Noreturn" => true,
            "struct" | "union" | "enum" => true,
            "_Alignas" | "_Static_assert" | "_Thread_local" | "__thread" | "typeof"
            | "__typeof" | "__typeof__" | "_Generic" | "_Complex" | "_Imaginary"
            | "__attribute__" | "__attribute" | "__declspec" => true,
            // `self` is the receiver's type name, but `self.x` and `self->x`
            // start an expression, not a declaration.
            "self" => {
                self.impl_depth > 0
                    && !matches!(
                        &self.peek_ahead(ahead + 1).kind,
                        TokenKind::Punct(punct) if punct == "." || punct == "->"
                    )
            }
            other => self.is_type_name(other),
        }
    }

    // -- structs, unions, enums ------------------------------------------

    fn record_spec(&mut self, kind: RecordKind) -> Result<RecordSpec, Diagnostic> {
        let args_builtin = self.pending_args_builtin;
        self.pending_args_builtin = false;
        let start = self.bump().span;
        let tag = if self.ident_here().is_some() {
            Some(self.expect_ident()?)
        } else {
            None
        };
        // The tag is in scope inside its own definition, so recursive members
        // such as `struct Node { struct Node *next; };` work.
        if let Some(tag) = &tag {
            self.declare_tag(&tag.text);
        }

        // `struct Vec<T>` puts `T` in scope for the member declarations. The
        // scope is only for the definition; a later `Vec<int>` resolves its
        // arguments at the use site.
        let generic_scope = self.at_punct("<");
        if generic_scope {
            self.push_scope();
        }
        let parsed = (|| {
            let generics = if generic_scope {
                self.generic_params()?
            } else {
                Vec::new()
            };
            let members = if self.eat_punct("{") {
                if self.prescan {
                    self.skip_braced_body()?;
                    None
                } else {
                    let members = self.record_members()?;
                    self.expect_punct("}")?;
                    Some(members)
                }
            } else {
                None
            };
            Ok((generics, members))
        })();
        if generic_scope {
            self.pop_scope();
        }
        let (generics, members) = parsed?;
        if args_builtin && (kind != RecordKind::Struct || !generics.is_empty()) {
            return Err(Diagnostic::new(
                start,
                "`#[args_builtin]` applies to a non-generic struct definition",
            ));
        }
        Ok(RecordSpec {
            span: self.finish(start),
            kind,
            tag,
            generics,
            args_builtin,
            members,
        })
    }

    fn record_members(&mut self) -> Result<Vec<MemberDecl>, Diagnostic> {
        let mut members = Vec::new();
        while !self.at_punct("}") {
            if self.at_eof() {
                return Err(Diagnostic::new(
                    self.here(),
                    "unterminated struct or union body",
                ));
            }
            // A lone `;` inside an aggregate body is tolerated, as compilers
            // do for an empty member declaration; the AST has no place for it.
            if self.eat_punct(";") {
                continue;
            }
            // A const fn may expand to zero or more members.
            if let Some(expanded) = self.expand_member_list()? {
                members.extend(expanded);
                continue;
            }
            members.push(self.member_decl()?);
        }
        Ok(members)
    }

    /// One member declaration: specifiers, declarators, and the semicolon.
    fn member_decl(&mut self) -> Result<MemberDecl, Diagnostic> {
        let specs = self.decl_specs()?;
        let mut declarators = Vec::new();
        if !self.at_punct(";") {
            loop {
                let colon_span = self.here();
                let declarator = if self.at_punct(":") {
                    Declarator {
                        span: colon_span,
                        name: None,
                        derived: Vec::new(),
                    }
                } else {
                    self.declarator(false)?
                };
                let bit_width = if self.eat_punct(":") {
                    Some(self.conditional()?)
                } else {
                    None
                };
                let span = match &bit_width {
                    Some(width) => declarator.span.join(width.span()),
                    None => declarator.span,
                };
                declarators.push(MemberDeclarator {
                    span,
                    declarator,
                    bit_width,
                });
                if !self.eat_punct(",") {
                    break;
                }
            }
        }
        let semicolon = self.expect_punct(";")?;
        Ok(MemberDecl {
            span: specs.span.join(semicolon),
            specs,
            declarators,
        })
    }

    /// Expands a `const fn` call that stands where members are written.
    fn expand_member_list(&mut self) -> Result<Option<Vec<MemberDecl>>, Diagnostic> {
        let Some(index) = self.const_fn_call_ahead() else {
            return Ok(None);
        };
        let start = self.here();
        self.record_const_call(index, start);
        self.bump();
        let (tokens, close) = self.capture_token_argument()?;
        let span = start.join(close);
        let name = self.const_fns[index].name.clone();
        let arguments = Arguments::Tokens(tokens);
        let key = self.probe_key(
            index,
            Place::Member,
            span,
            super::probe::fingerprint(&arguments),
        );
        let returned = match self.probe_answer(&key) {
            Some(Answer::Tokens(tokens)) => tokens,
            Some(Answer::Failed { message, span: at }) => {
                return Err(Diagnostic::new(at.unwrap_or(span), message));
            }
            _ => {
                // The members a call would add are unknown this round, so the
                // request is recorded and the aggregate keeps the members it
                // has written out. A round with requests emits nothing, and
                // the next one parses the file again with them in hand.
                self.probe_record(super::probe::ProbeRequest {
                    key,
                    function: index,
                    name: name.clone(),
                    place: Place::Member,
                    site: span,
                    arguments,
                });
                return Ok(Some(Vec::new()));
            }
        };
        if returned.is_empty() {
            return Ok(Some(Vec::new()));
        }
        self.parse_member_fragment(returned, span)
            .map(Some)
            .map_err(|error| self.fragment_error(error, index))
    }

    /// Parses a token sequence as aggregate members.
    fn parse_member_fragment(
        &mut self,
        mut tokens: Vec<crate::lexer::Token>,
        span: crate::source::Span,
    ) -> Result<Vec<MemberDecl>, Diagnostic> {
        let needs_semicolon = !matches!(
            tokens.last().map(|token| &token.kind),
            Some(TokenKind::Punct(punct)) if punct == ";" || punct == "}"
        );
        if needs_semicolon {
            tokens.push(crate::lexer::Token {
                kind: TokenKind::Punct(";".to_string()),
                span,
                at_line_start: false,
                leading_space: false,
            });
        }
        tokens.push(crate::lexer::Token {
            kind: TokenKind::Eof,
            span,
            at_line_start: false,
            leading_space: false,
        });
        let mut parser = Parser::new(tokens, false);
        parser.scopes = self.scopes.clone();
        parser.probe = self.probe.clone();
        parser.defer_body_calls = self.defer_body_calls;
        parser.body_stack = self.body_stack.clone();
        parser.function_bodies = self.function_bodies.clone();
        let mut members = Vec::new();
        while !parser.at_eof() {
            if parser.eat_punct(";") {
                continue;
            }
            members.push(parser.member_decl()?);
        }
        self.bodies_deferred |= parser.bodies_deferred;
        self.deferred_body_functions
            .extend(parser.deferred_body_functions.iter().cloned());
        Ok(members)
    }

    fn enum_spec(&mut self) -> Result<EnumSpec, Diagnostic> {
        // `#[result_niche]` 写在声明处，说明这个模板想用零值 niche 布局。
        // 条目解析器已经吃掉了前缀，标记通过 `pending_enum_niche` 传进来。
        let mut result_niche = self.pending_enum_niche;
        self.pending_enum_niche = false;
        let option_niche = self.pending_option_niche;
        self.pending_option_niche = false;
        while self.at_punct("#") {
            self.bump();
            self.expect_punct("[")?;
            let name = self.expect_ident()?;
            self.expect_punct("]")?;
            if name.text == "result_niche" {
                result_niche = true;
            } else {
                return Err(Diagnostic::new(
                    name.span,
                    format!("unknown enum attribute '#[{}]'", name.text),
                ));
            }
        }
        let start = self.bump().span;
        let underlying = if self.eat_punct("(") {
            let type_name = self.type_name()?;
            self.expect_punct(")")?;
            Some(type_name)
        } else {
            None
        };
        let tag = if self.ident_here().is_some() {
            Some(self.expect_ident()?)
        } else {
            None
        };
        if let Some(tag) = &tag {
            self.declare_tag(&tag.text);
        }

        // `enum Option<T>` puts `T` in scope for the variant payload types. The
        // scope is only for the definition; a later `Option<int>` resolves its
        // arguments at the use site.
        let generic_scope = self.at_punct("<");
        if generic_scope {
            self.push_scope();
        }
        let parsed = (|| {
            let generics = if generic_scope {
                self.generic_params()?
            } else {
                Vec::new()
            };
            let enumerators = if self.eat_punct("{") {
                if self.prescan {
                    self.skip_braced_body()?;
                    None
                } else {
                    let mut list = Vec::new();
                    while !self.at_punct("}") {
                        let name = self.expect_ident()?;
                        // Enumerators are ordinary identifiers and may shadow a
                        // typedef name from an outer scope.
                        self.declare_value(&name.text);
                        let payload = if self.eat_punct("(") {
                            let type_name = self.type_name()?;
                            self.expect_punct(")")?;
                            VariantPayload::Tuple(type_name)
                        } else if self.eat_punct("{") {
                            let mut fields = Vec::new();
                            while !self.at_punct("}") {
                                let field_name = self.expect_ident()?;
                                self.expect_punct(":")?;
                                let ty = self.type_name()?;
                                let span = field_name.span.join(ty.span);
                                fields.push(VariantField {
                                    span,
                                    name: field_name,
                                    ty,
                                });
                                if !self.eat_punct(",") {
                                    break;
                                }
                            }
                            self.expect_punct("}")?;
                            if fields.is_empty() {
                                return Err(Diagnostic::new(
                                    name.span,
                                    "a struct variant needs at least one field",
                                ));
                            }
                            VariantPayload::Struct(fields)
                        } else {
                            VariantPayload::None
                        };
                        if payload.is_some() && self.at_punct("=") {
                            return Err(Diagnostic::new(
                                self.here(),
                                format!(
                                    "payload variant '{}' cannot have an explicit value",
                                    name.text
                                ),
                            ));
                        }
                        let value = if self.eat_punct("=") {
                            Some(self.conditional()?)
                        } else {
                            None
                        };
                        let mut span = match &value {
                            Some(value) => name.span.join(value.span()),
                            None => name.span,
                        };
                        if let VariantPayload::Tuple(payload) = &payload {
                            span = span.join(payload.span);
                        } else if let VariantPayload::Struct(fields) = &payload
                            && let Some(last) = fields.last()
                        {
                            span = span.join(last.span);
                        }
                        list.push(Enumerator {
                            span,
                            name,
                            value,
                            payload,
                        });
                        if !self.eat_punct(",") {
                            break;
                        }
                    }
                    self.expect_punct("}")?;
                    Some(list)
                }
            } else {
                None
            };
            Ok((generics, enumerators))
        })();
        if generic_scope {
            self.pop_scope();
        }
        let (generics, enumerators) = parsed?;

        Ok(EnumSpec {
            result_niche,
            option_niche,
            span: self.finish(start),
            underlying,
            tag,
            generics,
            enumerators,
        })
    }

    // -- declarators ------------------------------------------------------

    /// Parses a declarator. `abstract_ok` permits an omitted name, which is
    /// what type names, parameters, and `sizeof` need.
    pub(crate) fn declarator(&mut self, abstract_ok: bool) -> Result<Declarator, Diagnostic> {
        // A type-position expansion may have carried a declarator with it,
        // as in "echo(int *) p": the written declarator continues that one.
        let prefix = self.pending_declarator.take();
        let mut declarator = self.declarator_tree(abstract_ok)?;
        if let Some(prefix) = prefix {
            let mut derived = prefix.derived;
            derived.extend(declarator.derived);
            declarator.derived = derived;
        }
        Ok(declarator)
    }

    fn declarator_tree(&mut self, abstract_ok: bool) -> Result<Declarator, Diagnostic> {
        let start = self.here();
        let before_declarator = self.last_span();

        // Step 1: the leading `*` list, kept in source order for now.
        let mut pointers = Vec::new();
        while self.at_punct("*") {
            let pointer_start = self.bump().span;
            let mut qualifiers = Vec::new();
            loop {
                match self.ident_here() {
                    Some("const") => qualifiers.push(Qualifier::Const),
                    Some("volatile") => qualifiers.push(Qualifier::Volatile),
                    Some("restrict" | "__restrict" | "__restrict__") => {
                        qualifiers.push(Qualifier::Restrict)
                    }
                    Some("_Atomic") if !self.at_punct_ahead(1, "(") => {
                        qualifiers.push(Qualifier::Atomic)
                    }
                    _ => break,
                }
                self.bump();
            }
            let span = self.finish(pointer_start);
            pointers.push(PointerDeclarator { span, qualifiers });
        }

        // Step 2: the direct declarator and its suffixes.
        let mut declarator = self.direct_declarator(abstract_ok)?;

        // Step 3: pointers go outward in reverse source order, so the rightmost
        // `*` is the one nearest the declared name.
        for pointer in pointers.into_iter().rev() {
            declarator.derived.push(DerivedDeclarator::Pointer(pointer));
        }
        declarator.span = if self.last_span() == before_declarator {
            start
        } else {
            self.finish(start)
        };
        Ok(declarator)
    }

    fn direct_declarator(&mut self, abstract_ok: bool) -> Result<Declarator, Diagnostic> {
        let start = self.here();

        // `(` is a parenthesised declarator unless this is an abstract
        // declarator whose `(` opens a parameter list (`int (void)`,
        // `int ()`). `int (*)(void)` has `*` after the parenthesis and so
        // stays a parenthesised declarator.
        let parenthesised = self.at_punct("(")
            && !(abstract_ok && (self.at_punct_ahead(1, ")") || self.starts_type_name(1)));
        if parenthesised {
            self.bump();
            let mut inner = self.declarator_tree(abstract_ok)?;
            self.expect_punct(")")?;
            inner.derived.extend(self.declarator_suffixes()?);
            inner.span = self.finish(start);
            return Ok(inner);
        }

        if abstract_ok && (self.at_punct("(") || self.at_punct("[")) {
            let derived = self.declarator_suffixes()?;
            return Ok(Declarator {
                span: self.finish(start),
                name: None,
                derived,
            });
        }

        if self.ident_here().is_some() {
            let name = self.expect_ident()?;
            let derived = self.declarator_suffixes()?;
            return Ok(Declarator {
                span: self.finish(start),
                name: Some(name),
                derived,
            });
        }

        if abstract_ok {
            return Ok(Declarator {
                span: start,
                name: None,
                derived: Vec::new(),
            });
        }

        Err(self.expected("a declarator name"))
    }

    fn declarator_suffixes(&mut self) -> Result<Vec<DerivedDeclarator>, Diagnostic> {
        let mut derived = Vec::new();
        loop {
            if self.at_punct("[") {
                derived.push(DerivedDeclarator::Array(self.array_declarator()?));
            } else if self.at_punct("(") {
                derived.push(DerivedDeclarator::Function(self.parameter_list()?));
            } else {
                break;
            }
        }
        Ok(derived)
    }

    fn array_declarator(&mut self) -> Result<ArrayDeclarator, Diagnostic> {
        let start = self.expect_punct("[")?;

        if self.prescan {
            // Prescan only needs the declared names; sizes are parsed for real
            // later, when more type names are known.
            self.skip_balanced_group("[", "]")?;
            return Ok(ArrayDeclarator {
                span: self.finish(start),
                length: ArrayLength::Unspecified,
            });
        }

        let length = if self.eat_punct("]") {
            ArrayLength::Unspecified
        } else if self.at_punct("*") && self.at_punct_ahead(1, "]") {
            self.bump();
            self.bump();
            ArrayLength::Star
        } else {
            if matches!(
                self.ident_here(),
                Some("static" | "const" | "volatile" | "restrict" | "__restrict" | "__restrict__")
            ) {
                return Err(Diagnostic::new(
                    self.here(),
                    "array parameter qualifiers and 'static' array sizes are not supported",
                ));
            }
            let length = self.assignment()?;
            self.expect_punct("]")?;
            ArrayLength::Expr(Box::new(length))
        };

        Ok(ArrayDeclarator {
            span: self.finish(start),
            length,
        })
    }

    pub(crate) fn parameter_list(&mut self) -> Result<FunctionDeclarator, Diagnostic> {
        let start = self.expect_punct("(")?;
        let mut params = Vec::new();
        let mut variadic = false;

        if !self.at_punct(")") {
            loop {
                if self.at_punct("...") {
                    self.bump();
                    variadic = true;
                    if !self.at_punct(")") {
                        return Err(Diagnostic::new(
                            self.here(),
                            "'...' must be the last parameter",
                        ));
                    }
                    break;
                }

                let specs = self.decl_specs()?;
                if specs.type_specs.is_empty()
                    && specs.storage.is_none()
                    && specs.qualifiers.is_empty()
                {
                    return Err(Diagnostic::new(
                        self.here(),
                        "old-style parameter lists are not supported; give every parameter a type",
                    ));
                }
                let declarator = if self.at_punct(")") || self.at_punct(",") {
                    None
                } else {
                    Some(self.declarator(true)?)
                };
                let span = match &declarator {
                    Some(declarator) => specs.span.join(declarator.span),
                    None => specs.span,
                };
                params.push(ParamDecl {
                    span,
                    specs,
                    declarator,
                });

                if !self.eat_punct(",") {
                    break;
                }
                if self.at_punct(")") {
                    return Err(Diagnostic::new(
                        self.here(),
                        "expected a parameter after ','",
                    ));
                }
            }
        }

        let close = self.expect_punct(")")?;
        Ok(FunctionDeclarator {
            span: start.join(close),
            generics: Vec::new(),
            params,
            variadic,
        })
    }

    // -- generic parameters and arguments --------------------------------

    /// Parses `<T, T: Drawable, T: From<int>, const N>` and declares the names
    /// in the current scope.
    ///
    /// A bound is one or more traits after `:` separated by `+`, each with
    /// optional generic arguments. Bounds on a `const` parameter are rejected
    /// here; the lowering pass checks every bound against each instantiation.
    /// The caller opens the scope that should contain the parameters.
    pub(crate) fn generic_params(&mut self) -> Result<Vec<GenericParam>, Diagnostic> {
        self.expect_punct("<")?;
        let mut params = Vec::new();
        loop {
            let start = self.here();
            let kind = if self.eat_ident("const") {
                GenericParamKind::Const
            } else {
                GenericParamKind::Type
            };
            let name = self.expect_ident()?;
            let mut bounds = Vec::new();
            if self.eat_punct(":") {
                if kind == GenericParamKind::Const {
                    return Err(Diagnostic::new(
                        name.span,
                        format!(
                            "const generic parameter '{}' cannot have a trait bound",
                            name.text
                        ),
                    ));
                }
                loop {
                    let bound = self.expect_ident()?;
                    if self.at_punct("(") {
                        return Err(Diagnostic::new(
                            self.here(),
                            "trait bounds with parenthesized arguments are not implemented yet",
                        ));
                    }
                    let (args, span) = if self.at_punct("<") {
                        let args = self.generic_args()?;
                        (args, bound.span.join(self.last_span()))
                    } else {
                        (Vec::new(), bound.span)
                    };
                    bounds.push(TraitBound {
                        span,
                        name: bound,
                        args,
                    });
                    if !self.eat_punct("+") {
                        break;
                    }
                }
            }
            match kind {
                GenericParamKind::Type => self.declare_typedef(&name.text),
                GenericParamKind::Const => self.declare_value(&name.text),
            }
            params.push(GenericParam {
                span: start.join(name.span),
                name,
                kind,
                bounds,
            });
            if !self.eat_punct(",") {
                break;
            }
        }
        self.close_angle()?;
        Ok(params)
    }

    /// Parses `<int, 4>` at a use site.
    ///
    /// The argument may be a type name or a constant expression. A name in
    /// scope as a value (including a `const` generic parameter) is a value
    /// argument; everything that starts a type name is a type argument.
    pub(crate) fn generic_args(&mut self) -> Result<Vec<GenericArg>, Diagnostic> {
        self.expect_punct("<")?;
        self.angle_depth += 1;
        let parsed = (|| {
            let mut args = Vec::new();
            loop {
                let kind = if self.starts_type_name(0) {
                    let type_name = self.type_name()?;
                    GenericArgKind::Type(type_name)
                } else {
                    let value = self.assignment()?;
                    GenericArgKind::Const(value)
                };
                let span = match &kind {
                    GenericArgKind::Type(type_name) => type_name.span,
                    GenericArgKind::Const(value) => value.span(),
                };
                args.push(GenericArg { span, kind });
                if !self.eat_punct(",") {
                    break;
                }
            }
            Ok(args)
        })();
        self.angle_depth -= 1;
        let args = parsed?;
        self.close_angle()?;
        Ok(args)
    }

    /// Parses a type name: cast types, `sizeof`, `_Alignof`, compound
    /// literals, `_Atomic(...)`, and `impl` headers.
    pub(crate) fn type_name(&mut self) -> Result<TypeName, Diagnostic> {
        // A const fn may stand where a type is written, as long as it returns
        // tokens: the call is replaced by the type its tokens parse into.
        if let Some(expanded) = self.expand_type_call()? {
            return Ok(expanded);
        }
        let start = self.here();
        let specs = self.decl_specs()?;
        // A type name needs a type specifier; storage classes and qualifiers
        // alone (`(const)x`) are not a type.
        if specs.type_specs.is_empty() {
            return Err(Diagnostic::new(self.here(), "expected a type name"));
        }

        let declarator = if self.at_punct("*") || self.at_punct("(") || self.at_punct("[") {
            let declarator = self.declarator(true)?;
            if declarator.name.is_some() {
                return Err(Diagnostic::new(
                    declarator.span,
                    "a type name cannot declare a name",
                ));
            }
            Some(declarator)
        } else {
            None
        };

        Ok(TypeName {
            span: self.finish(start),
            specs,
            declarator,
        })
    }
}

/// One built-in type keyword, if `word` is one.
pub fn builtin_type(word: &str) -> Option<BuiltinType> {
    Some(match word {
        "void" => BuiltinType::Void,
        "_Bool" => BuiltinType::Bool,
        "char" => BuiltinType::Char,
        "short" => BuiltinType::Short,
        "int" => BuiltinType::Int,
        "long" => BuiltinType::Long,
        "float" => BuiltinType::Float,
        "double" => BuiltinType::Double,
        "signed" => BuiltinType::Signed,
        "unsigned" => BuiltinType::Unsigned,
        _ => return None,
    })
}
/// Parses a token sequence returned by a const fn as one complete type name.
fn parse_type_fragment(
    tokens: Vec<crate::lexer::Token>,
    span: crate::source::Span,
    parent: &Parser,
) -> Result<TypeName, Diagnostic> {
    if tokens.is_empty() {
        return Err(Diagnostic::new(span, "a const fn returned no tokens"));
    }
    let mut stream = tokens;
    stream.push(crate::lexer::Token {
        kind: TokenKind::Eof,
        span,
        at_line_start: false,
        leading_space: false,
    });
    let mut parser = Parser::new(stream, false);
    parser.scopes[0].typedefs = parent.scopes[0].typedefs.clone();
    parser.scopes[0].tags = parent.scopes[0].tags.clone();
    parser.scopes[0].traits = parent.scopes[0].traits.clone();
    parser.probe = parent.probe.clone();
    let type_name = parser.type_name()?;
    if !parser.at_eof() {
        return Err(Diagnostic::new(
            parser.here(),
            "tokens returned by a const fn do not form one type",
        ));
    }
    Ok(type_name)
}
