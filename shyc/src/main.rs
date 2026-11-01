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
use shyc::lexer::{self, Token};
use shyc::lower::{self, StartupMode};
use shyc::preprocess::{self, Attributes};
use shyc::source::SourceMap;
use shyc::{export, llvm, parser, passes, shy};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Stage {
    Tokens,
    Preprocess,
    CSource,
    CHeader,
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
    freestanding: bool,
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
        Stage::CSource => {
            let preprocessed = preprocess::preprocess(&mut sources, file, &options.preprocess)
                .map_err(|error| sources.render(&error))?;
            export::reject_let(&preprocessed.tokens).map_err(|error| sources.render(&error))?;
            export::render_c_source(&preprocessed.tokens)
        }
        Stage::CHeader => {
            let preprocessed = preprocess::preprocess(&mut sources, file, &options.preprocess)
                .map_err(|error| sources.render(&error))?;
            let unit =
                parser::parse(&preprocessed.tokens).map_err(|error| sources.render(&error))?;
            // The output path names the include guard. Without `-o` the header
            // goes to stdout, so the input path names it instead.
            let guard_path = options.output.as_deref().unwrap_or(options.input.as_path());
            export::render_c_header(
                &unit,
                &export::HeaderOptions {
                    guard: export::header_guard(guard_path),
                    root: file,
                    includes: preprocessed.includes,
                    defines: preprocessed.defines,
                },
            )
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
            let startup = if preprocessed.attributes.no_main || options.freestanding {
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
                    disable_nonexhaustive_switch: preprocessed
                        .attributes
                        .disable_nonexhaustive_switch,
                    disable_rustic_mut: preprocessed.attributes.disable_rustic_mut,
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

fn is_bare_target(triple: &str) -> bool {
    let lower = triple.to_ascii_lowercase();
    lower.contains("none") || lower.contains("unknown-elf") || lower.ends_with("-elf")
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
    let mut freestanding = false;
    let mut preprocess = preprocess::Options::default();
    // The bundled headers describe the Shy target, so they are searched before
    // any `-I` directory and before the host C library.
    preprocess
        .include_dirs
        .push(Path::new(env!("CARGO_MANIFEST_DIR")).join("include"));
    while let Some(arg) = args.next() {
        let selected = match arg.as_str() {
            "--help" | "-h" => {
                println!(
                    "usage: shyc [options] file\n\
                     output: -S / --emit=shy (default), --emit=llvm, --emit=c, \
                     --emit=c-header, -E, --dump-tokens, --dump-ast, --dump-ir\n\
                     options: -O0..-O3, --target <triple>, -ffreestanding, -fhosted, \
                     -I dir, -D name[=value], -U name, -o file\n\
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
            flag if flag.starts_with("--target=") => {
                target = Some(flag["--target=".len()..].to_string());
                None
            }
            "-ffreestanding" => {
                freestanding = true;
                None
            }
            "-fhosted" => {
                freestanding = false;
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
            _ if arg.starts_with("--emit=") => Some(match &arg[7..] {
                "shy" => Stage::Emit(Backend::Shy),
                "llvm" => Stage::Emit(Backend::Llvm),
                "c" => Stage::CSource,
                "c-header" => Stage::CHeader,
                other => return Err(format!("unknown --emit value: {other}")),
            }),
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
    if target.as_deref().is_some_and(is_bare_target) {
        freestanding = true;
    }
    // Target facts that a hosted C library needs before it includes anything.
    if target.as_deref().is_some_and(|target| target != "shy") && !freestanding {
        // The host C library only applies to a hosted target, and these
        // directories come after the bundled and `-I` headers, as with gcc.
        // The Shy target has no host library, and its headers must not win
        // over the bundled ones.
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
        // `__shy__` is predefined for the Shy target; a hosted build must not
        // see it, or headers pick the Shy variant.
        preprocess.undefines.push("__shy__".to_string());
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
            ("__INTPTR_TYPE__", "long int"),
            ("__UINTPTR_TYPE__", "long unsigned int"),
            ("__INTMAX_TYPE__", "long int"),
            ("__UINTMAX_TYPE__", "long unsigned int"),
            ("__INT8_TYPE__", "signed char"),
            ("__UINT8_TYPE__", "unsigned char"),
            ("__INT16_TYPE__", "short int"),
            ("__UINT16_TYPE__", "short unsigned int"),
            ("__INT32_TYPE__", "int"),
            ("__UINT32_TYPE__", "unsigned int"),
            ("__INT64_TYPE__", "long int"),
            ("__UINT64_TYPE__", "long unsigned int"),
            ("__SIZE_MAX__", "18446744073709551615UL"),
            ("__INTMAX_MAX__", "9223372036854775807L"),
            ("__UINTMAX_MAX__", "18446744073709551615UL"),
            ("__PTRDIFF_MAX__", "9223372036854775807L"),
            ("__WCHAR_MAX__", "2147483647"),
            ("__WCHAR_MIN__", "(-2147483647 - 1)"),
            ("__SCHAR_MAX__", "127"),
            ("__SHRT_MAX__", "32767"),
            ("__LONG_LONG_MAX__", "9223372036854775807LL"),
            ("__FLT_MAX__", "3.40282346638528859812e+38F"),
            ("__DBL_MAX__", "1.79769313486231570815e+308"),
            ("__DBL_MIN__", "2.22507385850720138309e-308"),
            ("__DBL_EPSILON__", "2.22044604925031308085e-16"),
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
        freestanding,
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
    if attributes.disable_nonexhaustive_switch {
        output.push_str("#![disable_nonexhaustive_switch]\n");
    }
    if attributes.disable_rustic_mut {
        output.push_str("#![disable_rustic_mut]\n");
    }
    output.push_str(&export::render_c_source(tokens));
    output
}
