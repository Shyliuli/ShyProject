use crate::ast;
use crate::source::{Diagnostic, Span};

/// Give every struct variant a private record type before semantic analysis.
///
/// The rest of the compiler keeps using the existing single-payload enum
/// representation. Only this front-end step knows about variant field syntax.
pub(crate) fn struct_variants(items: &mut Vec<ast::Item>) -> Result<(), Diagnostic> {
    let mut output = Vec::with_capacity(items.len());
    let mut serial = 0usize;
    for item in std::mem::take(items) {
        let ast::Item::Decl(mut declaration) = item else {
            output.push(item);
            continue;
        };
        let mut records = Vec::new();
        for spec in &mut declaration.specs.type_specs {
            let ast::TypeSpecifier::Enum(enumeration) = spec else {
                continue;
            };
            let enum_name = enumeration
                .tag
                .as_ref()
                .map(|tag| tag.text.clone())
                .unwrap_or_else(|| "anonymous".to_string());
            let generics = enumeration.generics.clone();
            let Some(enumerators) = &mut enumeration.enumerators else {
                continue;
            };
            for enumerator in enumerators {
                let ast::VariantPayload::Struct(fields) = &enumerator.payload else {
                    continue;
                };
                let record_name = format!(
                    "__shy_variant_{}_{}_{}",
                    enum_name, enumerator.name.text, serial
                );
                serial += 1;
                records.push(record_declaration(
                    &record_name,
                    &generics,
                    fields,
                    enumerator.span,
                ));
                enumerator.payload = ast::VariantPayload::Tuple(synthetic_type(
                    &record_name,
                    &generics,
                    enumerator.span,
                ));
            }
        }
        output.extend(records);
        output.push(ast::Item::Decl(declaration));
    }
    *items = output;
    Ok(())
}

fn record_declaration(
    name: &str,
    generics: &[ast::GenericParam],
    fields: &[ast::VariantField],
    span: Span,
) -> ast::Item {
    let members = fields
        .iter()
        .map(|field| {
            let derived = field
                .ty
                .declarator
                .as_ref()
                .map(|declarator| declarator.derived.clone())
                .unwrap_or_default();
            ast::MemberDecl {
                span: field.span,
                specs: field.ty.specs.clone(),
                declarators: vec![ast::MemberDeclarator {
                    span: field.span,
                    declarator: ast::Declarator {
                        span: field.span,
                        name: Some(field.name.clone()),
                        derived,
                    },
                    bit_width: None,
                }],
            }
        })
        .collect();
    let record = ast::RecordSpec {
        span,
        kind: ast::RecordKind::Struct,
        tag: Some(ast::Name::new(name, span)),
        generics: generics.to_vec(),
        args_builtin: false,
        members: Some(members),
    };
    let mut specs = specs(vec![ast::TypeSpecifier::Record(record)], span);
    let declarators = if generics.is_empty() {
        specs.storage = Some(ast::StorageClass::Typedef);
        vec![ast::InitDeclarator {
            span,
            declarator: ast::Declarator {
                span,
                name: Some(ast::Name::new(name, span)),
                derived: Vec::new(),
            },
            init: None,
        }]
    } else {
        Vec::new()
    };
    ast::Item::Decl(ast::Declaration {
        span,
        specs,
        declarators,
        is_defer: false,
    })
}

fn synthetic_type(name: &str, generics: &[ast::GenericParam], span: Span) -> ast::TypeName {
    let specifier = if generics.is_empty() {
        ast::TypeSpecifier::TypedefName(ast::Name::new(name, span))
    } else {
        let args = generics
            .iter()
            .map(|parameter| {
                let parameter_name = ast::Name::new(parameter.name.text.clone(), parameter.span);
                let kind = match parameter.kind {
                    ast::GenericParamKind::Type => ast::GenericArgKind::Type(ast::TypeName {
                        span: parameter.span,
                        specs: specs(
                            vec![ast::TypeSpecifier::TypedefName(parameter_name)],
                            parameter.span,
                        ),
                        declarator: None,
                    }),
                    ast::GenericParamKind::Const => {
                        ast::GenericArgKind::Const(ast::Expr::Name(parameter_name))
                    }
                };
                ast::GenericArg {
                    span: parameter.span,
                    kind,
                }
            })
            .collect();
        ast::TypeSpecifier::Generic(ast::GenericSpec {
            span,
            name: ast::Name::new(name, span),
            args,
        })
    };
    ast::TypeName {
        span,
        specs: specs(vec![specifier], span),
        declarator: None,
    }
}

fn specs(type_specs: Vec<ast::TypeSpecifier>, span: Span) -> ast::DeclSpecs {
    ast::DeclSpecs {
        span,
        storage: None,
        is_inline: false,
        is_noreturn: false,
        is_pub: false,
        is_shy_fn: false,
        qualifiers: Vec::new(),
        type_specs,
    }
}
