//! Command-line driver for the independent ShyC compiler.
//!
//! One invocation reads one source file, preprocesses it, parses it, lowers it
//! to the typed IR, optionally optimizes it, and emits the selected backend's
//! text. Assembly and linking belong to the existing `shycc` driver.
use std::env;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use shyc::ir::Layout;
use shyc::lexer::{self, Token, TokenKind};
use shyc::lower::{self, StartupMode};
use shyc::preprocess::{self, Attributes};
use shyc::source::SourceMap;
use shyc::{llvm, parser, passes, shy};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Stage {
    Tokens,
    Preprocess,
    Ast,
    Ir,
    Emit(Backend),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Backend {
    Shy,
    Llvm,
}

struct Options {
    stage: Stage,
    input: PathBuf,
    output: Option<PathBuf>,
    target: Option<String>,
    opt_level: u8,
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
    let file = sources.add(options.input.clone(), text);
    let output = match options.stage {
        Stage::Tokens => {
            let tokens = lexer::lex(&sources, file).map_err(|error| sources.render(&error))?;
            format!("{tokens:#?}\n")
        }
        Stage::Preprocess => {
            let preprocessed = preprocess::preprocess(&mut sources, file, &options.preprocess)
                .map_err(|error| sources.render(&error))?;
            render_preprocessed(&preprocessed.tokens, &preprocessed.attributes)
        }
        Stage::Ast => {
            let preprocessed = preprocess::preprocess(&mut sources, file, &options.preprocess)
                .map_err(|error| sources.render(&error))?;
            let unit =
                parser::parse(&preprocessed.tokens).map_err(|error| sources.render(&error))?;
            format!("{unit:#?}\n")
        }
        Stage::Ir | Stage::Emit(_) => {
            let preprocessed = preprocess::preprocess(&mut sources, file, &options.preprocess)
                .map_err(|error| sources.render(&error))?;
            let unit =
                parser::parse(&preprocessed.tokens).map_err(|error| sources.render(&error))?;
            let target = resolve_target(&options)?;
            let startup = if preprocessed.attributes.no_main {
                StartupMode::Manual
            } else {
                StartupMode::Main
            };
            let mut lowered = lower::lower(
                &unit,
                &lower::Options {
                    module_name: module_name(&options.input),
                    layout: target.layout,
                    startup,
                    abi: target.abi,
                },
            )
            .map_err(|error| sources.render(&error))?;
            lowered.module.resources.memory_bytes = preprocessed.attributes.memory.map(u64::from);
            lowered.module.resources.stack_bytes = preprocessed.attributes.stack.map(u64::from);
            if options.opt_level > 0 {
                for function in &mut lowered.module.functions {
                    passes::optimize(function, options.opt_level);
                }
            }
            match options.stage {
                Stage::Ir => lowered.module.to_string(),
                Stage::Emit(Backend::Llvm) => llvm::emit(
                    &lowered.module,
                    &llvm::Options {
                        triple: target.triple,
                        layout: target.layout,
                    },
                )
                .map_err(|error| sources.render(&error))?,
                Stage::Emit(Backend::Shy) => shy::emit(
                    &lowered.module,
                    &shy::Options {
                        layout: target.layout,
                        emit_source_comments: false,
                    },
                )
                .map_err(|error| sources.render(&error))?,
                _ => unreachable!(),
            }
        }
    };
    match options.output {
        Some(path) => {
            fs::write(&path, output).map_err(|error| format!("{}: {error}", path.display()))
        }
        None => io::stdout()
            .lock()
            .write_all(output.as_bytes())
            .map_err(|error| error.to_string()),
    }
}

struct Target {
    triple: String,
    abi: abi_plan::AbiKind,
    layout: Layout,
}

/// Resolve the target triple and the IR pointer layout it implies.
///
/// An explicit target is never guessed: the Shy backend only accepts `shy`,
/// and the LLVM backend only accepts the 64-bit architectures whose layout is
/// [`Layout::LP64`].
fn resolve_target(options: &Options) -> Result<Target, String> {
    let backend = match options.stage {
        Stage::Emit(backend) => Some(backend),
        _ => None,
    };
    let Some(name) = options.target.as_deref() else {
        return match backend {
            Some(Backend::Llvm) => Err("--emit=llvm requires --target <triple>".into()),
            _ => Ok(Target {
                triple: "shy".into(),
                abi: abi_plan::AbiKind::ShyV1,
                layout: Layout::SHY,
            }),
        };
    };
    let arch = name.split('-').next().unwrap_or(name);
    match backend {
        Some(Backend::Shy) if name != "shy" => Err(format!(
            "the Shy backend emits ShyISA (`--target shy`), not `{name}`; \
             use --emit=llvm for that target"
        )),
        Some(Backend::Llvm) => match arch {
            "x86_64" | "riscv64" | "loongarch64" | "aarch64" => Ok(Target {
                triple: name.into(),
                abi: abi_kind(name),
                layout: Layout::LP64,
            }),
            _ => Err(format!(
                "unsupported --target `{name}`: --emit=llvm supports x86_64, riscv64, \
                 loongarch64, and aarch64"
            )),
        },
        _ => match arch {
            "shy" if name == "shy" => Ok(Target {
                triple: name.into(),
                abi: abi_plan::AbiKind::ShyV1,
                layout: Layout::SHY,
            }),
            "x86_64" | "riscv64" | "loongarch64" | "aarch64" => Ok(Target {
                triple: name.into(),
                abi: abi_kind(name),
                layout: Layout::LP64,
            }),
            _ => Err(format!(
                "unknown target `{name}`: expected `shy` or a 64-bit LLVM triple \
                 (x86_64, riscv64, loongarch64, aarch64)"
            )),
        },
    }
}

/// The target C ABI behind a triple.
fn abi_kind(triple: &str) -> abi_plan::AbiKind {
    match triple.split('-').next().unwrap_or(triple) {
        "x86_64" if triple.contains("windows") => abi_plan::AbiKind::WindowsX64,
        "x86_64" => abi_plan::AbiKind::SysV64,
        "aarch64" => abi_plan::AbiKind::Aapcs64,
        "riscv64" => abi_plan::AbiKind::Riscv64,
        "loongarch64" => abi_plan::AbiKind::LoongArch64,
        _ => abi_plan::AbiKind::ShyV1,
    }
}

fn module_name(input: &Path) -> String {
    input
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("module")
        .to_string()
}

fn arguments() -> Result<Option<Options>, String> {
    let mut args = env::args().skip(1);
    let mut stage = None;
    let mut input = None;
    let mut output = None;
    let mut target = None;
    let mut opt_level = 0;
    let mut preprocess = preprocess::Options::default();
    // Hosted targets use the system C library, so the preprocessor searches the
    // same directories gcc and clang do. The target's own directory comes
    // first, as with gcc.
    preprocess
        .include_dirs
        .push(Path::new(env!("CARGO_MANIFEST_DIR")).join("include"));
    for directory in [
        "/usr/lib/clang/23/include",
        "/usr/local/include",
        "/usr/include/x86_64-linux-gnu",
        "/usr/include",
    ] {
        let path = Path::new(directory);
        if path.is_dir() {
            preprocess.include_dirs.push(path.to_path_buf());
        }
    }
    while let Some(arg) = args.next() {
        let selected = match arg.as_str() {
            "--help" | "-h" => {
                println!(
                    "usage: shyc [options] file\n\
                     output: -S / --emit=shy (default), --emit=llvm, -E, --dump-tokens, \
                     --dump-ast, --dump-ir\n\
                     options: -O0..-O3, --target <triple>, -I dir, -D name[=value], \
                     -U name, -o file\n\
                     --emit=llvm requires a 64-bit --target triple; \
                     assembly and linking stay in shycc"
                );
                return Ok(None);
            }
            "--dump-tokens" => Some(Stage::Tokens),
            "-E" => Some(Stage::Preprocess),
            "--dump-ast" => Some(Stage::Ast),
            "--dump-ir" => Some(Stage::Ir),
            "-S" => Some(Stage::Emit(Backend::Shy)),
            "-o" => {
                output = Some(PathBuf::from(args.next().ok_or("-o requires a path")?));
                None
            }
            "--target" => {
                target = Some(args.next().ok_or("--target requires a triple")?);
                None
            }
            "-O0" => {
                opt_level = 0;
                None
            }
            "-O1" => {
                opt_level = 1;
                None
            }
            "-O2" => {
                opt_level = 2;
                None
            }
            "-O3" => {
                opt_level = 3;
                None
            }
            "-I" | "-D" | "-U" => {
                let value = args
                    .next()
                    .ok_or_else(|| format!("{arg} requires an argument"))?;
                preprocess_option(&mut preprocess, &arg, &value)?;
                None
            }
            _ if arg.starts_with("--emit=") => Some(Stage::Emit(match &arg[7..] {
                "shy" => Backend::Shy,
                "llvm" => Backend::Llvm,
                other => return Err(format!("unknown --emit value: {other}")),
            })),
            _ if arg.starts_with("--target=") => {
                target = Some(arg[9..].to_string());
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
        if let Some(selected) = selected
            && stage.replace(selected).is_some()
        {
            return Err(
                "select exactly one of -E, --dump-tokens, --dump-ast, --dump-ir, -S, \
                 or --emit=<backend>"
                    .into(),
            );
        }
    }
    // Target facts that a hosted C library needs before it includes anything.
    if target.as_deref().is_some_and(|target| target != "shy") {
        for (name, value) in [
            ("__STDC__", "1"),
            ("__STDC_HOSTED__", "1"),
            ("__STDC_VERSION__", "201710L"),
            ("__SIZE_TYPE__", "long unsigned int"),
            ("__PTRDIFF_TYPE__", "long int"),
            ("__WCHAR_TYPE__", "int"),
            ("__CHAR_BIT__", "8"),
            ("__INT_MAX__", "2147483647"),
            ("__LONG_MAX__", "9223372036854775807L"),
            ("__x86_64__", "1"),
            ("__linux__", "1"),
            ("__unix__", "1"),
            ("__LP64__", "1"),
            ("__ELF__", "1"),
        ] {
            preprocess
                .defines
                .push((name.to_string(), value.to_string()));
        }
    }
    Ok(Some(Options {
        stage: stage.unwrap_or(Stage::Emit(Backend::Shy)),
        input: input.ok_or("no input source file")?,
        output,
        target,
        opt_level,
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
