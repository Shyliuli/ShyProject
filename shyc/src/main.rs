//! Small frontend driver; the existing `shycc` driver owns assembly and linking.
use std::env;
use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use shyc::lexer::{self, Token, TokenKind};
use shyc::preprocess::{self, Attributes};
use shyc::source::SourceMap;

#[derive(Clone, Copy)]
enum Stage {
    Tokens,
    Preprocess,
    Ast,
}

struct Options {
    stage: Stage,
    input: PathBuf,
    output: Option<PathBuf>,
    preprocess: preprocess::Options,
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("shyc: {message}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    let Some(options) = arguments()? else {
        return Ok(());
    };
    let text = fs::read_to_string(&options.input)
        .map_err(|error| format!("{}: {error}", options.input.display()))?;
    let mut sources = SourceMap::default();
    let file = sources.add(options.input, text);
    let output = match options.stage {
        Stage::Tokens => {
            let tokens = lexer::lex(&sources, file).map_err(|error| sources.render(&error))?;
            format!("{tokens:#?}\n")
        }
        Stage::Preprocess | Stage::Ast => {
            let preprocessed = preprocess::preprocess(&mut sources, file, &options.preprocess)
                .map_err(|error| sources.render(&error))?;
            match options.stage {
                Stage::Preprocess => {
                    render_preprocessed(&preprocessed.tokens, &preprocessed.attributes)
                }
                Stage::Ast => {
                    let unit = shyc::parser::parse(&preprocessed.tokens)
                        .map_err(|error| sources.render(&error))?;
                    format!("{unit:#?}\n")
                }
                Stage::Tokens => unreachable!(),
            }
        }
    };
    if let Some(path) = options.output {
        fs::write(&path, output).map_err(|error| format!("{}: {error}", path.display()))?;
    } else {
        io::stdout()
            .lock()
            .write_all(output.as_bytes())
            .map_err(|error| error.to_string())?;
    }
    Ok(())
}

fn arguments() -> Result<Option<Options>, String> {
    let mut args = env::args().skip(1);
    let mut stage = None;
    let mut input = None;
    let mut output = None;
    let mut preprocess = preprocess::Options::default();
    while let Some(arg) = args.next() {
        let selected = match arg.as_str() {
            "--help" | "-h" => {
                println!(
                    "usage: shyc (-E | --dump-tokens | --dump-ast) [options] file\n\
                    options: -I dir, -D name[=value], -U name, -o file\n\
                    frontend inspection is available; assembly generation is under development"
                );
                return Ok(None);
            }
            "--dump-tokens" => Some(Stage::Tokens),
            "-E" => Some(Stage::Preprocess),
            "--dump-ast" => Some(Stage::Ast),
            "-o" => {
                output = Some(PathBuf::from(args.next().ok_or("-o requires a path")?));
                None
            }
            "-I" | "-D" | "-U" => {
                let value = args
                    .next()
                    .ok_or_else(|| format!("{arg} requires an argument"))?;
                preprocess_option(&mut preprocess, &arg, &value)?;
                None
            }
            _ if arg.starts_with("-I") || arg.starts_with("-D") || arg.starts_with("-U") => {
                preprocess_option(&mut preprocess, &arg[..2], &arg[2..])?;
                None
            }
            _ if arg.starts_with('-') => return Err(format!("unknown option: {arg}")),
            _ => {
                if input.replace(PathBuf::from(arg)).is_some() {
                    return Err("expected one input source file".into());
                }
                None
            }
        };
        if let Some(selected) = selected {
            if stage.replace(selected).is_some() {
                return Err("select exactly one of -E, --dump-tokens, or --dump-ast".into());
            }
        }
    }
    Ok(Some(Options {
        stage: stage.ok_or(
            "select -E, --dump-tokens, or --dump-ast; code generation is not available yet",
        )?,
        input: input.ok_or("no input source file")?,
        output,
        preprocess,
    }))
}

fn preprocess_option(
    options: &mut preprocess::Options,
    flag: &str,
    value: &str,
) -> Result<(), String> {
    if value.is_empty() {
        return Err(format!("{flag} requires a nonempty argument"));
    }
    match flag {
        "-I" => options.include_dirs.push(PathBuf::from(value)),
        "-D" => {
            let (name, replacement) = value.split_once('=').unwrap_or((value, "1"));
            if name.is_empty() {
                return Err("-D requires a macro name".into());
            }
            // Keep the final binding for each name, respecting command-line order.
            options.undefines.retain(|old| old != name);
            options.defines.retain(|(old, _)| old != name);
            options.defines.push((name.into(), replacement.into()));
        }
        "-U" => {
            options.defines.retain(|(name, _)| name != value);
            options.undefines.retain(|old| old != value);
            options.undefines.push(value.into());
        }
        _ => unreachable!(),
    }
    Ok(())
}

fn render_preprocessed(tokens: &[Token], attributes: &Attributes) -> String {
    let mut output = String::new();
    if attributes.no_main {
        output.push_str("#![no_main]\n");
    }
    if let Some(bytes) = attributes.memory {
        output.push_str(&format!("#![mem({bytes})]\n"));
    }
    if let Some(bytes) = attributes.stack {
        output.push_str(&format!("#![stack({bytes})]\n"));
    }
    for token in tokens {
        let spelling = match &token.kind {
            TokenKind::Ident(text) | TokenKind::Number(text) | TokenKind::Punct(text) => {
                text.clone()
            }
            TokenKind::StringLiteral { prefix, raw } => format!("{prefix}\"{raw}\""),
            TokenKind::CharLiteral { prefix, raw } => format!("{prefix}'{raw}'"),
            TokenKind::Newline | TokenKind::Eof => continue,
        };
        if token.at_line_start && !output.is_empty() && !output.ends_with('\n') {
            output.push('\n');
        }
        output.push_str(&spelling);
        // A separator prevents adjacent expanded tokens from fusing on re-lexing.
        output.push(' ');
    }
    output.push('\n');
    output
}
