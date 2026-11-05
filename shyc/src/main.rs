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
use shyc::source::{Diagnostic, SourceMap, Span};
use shyc::{c, export_instances, llvm, parser, passes, probe, shy};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Stage {
    Tokens,
    Preprocess,
    CSource,
    CHeader,
    CWrapper,
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
    /// Explicit generic instance table (`--instances`).
    instances: Option<PathBuf>,
    /// `--constfn-probe`: compile one probe round instead of the program.
    probe: bool,
    /// `--probe-answers`: answers of the previous round.
    probe_answers: Option<PathBuf>,
    /// `--probe-manifest`: where this round's requests are written.
    probe_manifest: Option<PathBuf>,
    /// `--probe-object`: where a round with requests writes its probe unit.
    probe_object: Option<PathBuf>,
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
    if options.instances.is_some()
        && matches!(
            options.stage,
            Stage::Tokens | Stage::Preprocess | Stage::Ast
        )
    {
        return Err(
            "--instances is compile-time input for a lowering output and has no effect on \
             -E, --dump-tokens, or --dump-ast"
                .into(),
        );
    }
    if options.probe {
        return probe_round(&options);
    }
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
            let unit =
                parser::parse(&preprocessed.tokens).map_err(|error| sources.render(&error))?;
            render_c_source(&options, &unit, &preprocessed.attributes, &mut sources)?
        }
        Stage::CHeader => {
            let preprocessed = preprocess::preprocess(&mut sources, file, &options.preprocess)
                .map_err(|error| sources.render(&error))?;
            let unit =
                parser::parse(&preprocessed.tokens).map_err(|error| sources.render(&error))?;
            // The output path names the include guard. Without `-o` the header
            // goes to stdout, so the input path names it instead.
            let guard_path = options.output.as_deref().unwrap_or(options.input.as_path());
            let header_options = c::header::HeaderOptions {
                guard: c::header::header_guard(guard_path),
                root: file,
                includes: preprocessed.includes,
                defines: preprocessed.defines,
            };
            render_header(
                &options,
                &unit,
                &header_options,
                &preprocessed.attributes,
                &mut sources,
            )?
        }
        Stage::CWrapper => {
            let Some(_) = options.instances else {
                return Err("--emit=c-wrapper requires --instances <file>".into());
            };
            let preprocessed = preprocess::preprocess(&mut sources, file, &options.preprocess)
                .map_err(|error| sources.render(&error))?;
            let unit =
                parser::parse(&preprocessed.tokens).map_err(|error| sources.render(&error))?;
            let lowered = lower_instances(&options, &unit, &preprocessed.attributes, &mut sources)?;
            export_instances::render_wrapper(&lowered)
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
            let exports = instance_requests(&options, &unit, &mut sources)?;
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
                    disable_default_static: preprocessed.attributes.disable_default_static,
                    exports,
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
    write_output(options.output.as_deref(), &output)
}

/// Parses the explicit instance table, if the driver was given one.
fn instance_requests(
    options: &Options,
    unit: &shyc::ast::TranslationUnit,
    sources: &mut SourceMap,
) -> Result<Vec<lower::ExportRequest>, String> {
    let Some(path) = &options.instances else {
        return Ok(Vec::new());
    };
    let text = fs::read_to_string(path).map_err(|error| format!("{}: {error}", path.display()))?;
    let file = sources.add(path.clone(), text);
    let tokens = lexer::lex(sources, file).map_err(|error| sources.render(&error))?;
    let names = parser::type_names(unit);
    let table =
        export_instances::parse_table(&tokens, &names).map_err(|error| sources.render(&error))?;
    Ok(table
        .entries
        .into_iter()
        .map(|entry| lower::ExportRequest {
            name: entry.name,
            args: entry.args,
            alias: entry.alias,
        })
        .collect())
}

/// Lowers the unit only to produce the instance export plan for a C header or
/// wrapper. No entry point is required; a table may export from a library.
fn lower_instances(
    options: &Options,
    unit: &shyc::ast::TranslationUnit,
    attributes: &Attributes,
    sources: &mut SourceMap,
) -> Result<lower::Lowered, String> {
    let target = resolve_target(options)?;
    let exports = instance_requests(options, unit, sources)?;
    lower::lower(
        unit,
        &lower::Options {
            module_name: module_name(&options.input),
            layout: target.layout,
            startup: StartupMode::Manual,
            abi: target.abi,
            disable_nonexhaustive_switch: attributes.disable_nonexhaustive_switch,
            disable_rustic_mut: attributes.disable_rustic_mut,
            disable_default_static: attributes.disable_default_static,
            exports,
        },
    )
    .map_err(|error| sources.render(&error))
}

/// The C source of one unit. It names what the compiler resolved: symbols,
/// instance types, and specializations, so it lowers the unit first.
fn render_c_source(
    options: &Options,
    unit: &shyc::ast::TranslationUnit,
    attributes: &Attributes,
    sources: &mut SourceMap,
) -> Result<String, String> {
    let lowered = lower_instances(options, unit, attributes, sources)?;
    c::source::render(unit, &lowered).map_err(|error| sources.render(&error))
}

/// The C header of one unit, plus the explicit instance section when the
/// driver gave a table.
fn render_header(
    options: &Options,
    unit: &shyc::ast::TranslationUnit,
    header_options: &c::header::HeaderOptions,
    attributes: &Attributes,
    sources: &mut SourceMap,
) -> Result<String, String> {
    if options.instances.is_none() {
        return Ok(c::header::render_c_header(unit, header_options));
    }
    let lowered = lower_instances(options, unit, attributes, sources)?;
    let unspelled = c::header::unspelled_function_names(unit, header_options.root);
    let section = export_instances::render_header_section(&lowered, &unspelled);
    Ok(c::header::render_c_header_with(
        unit,
        header_options,
        &section,
    ))
}

struct Target {
    triple: String,
    abi: abi_plan::AbiKind,
    layout: Layout,
}

/// Resolve the target triple and the IR data model it implies.
///
/// An explicit target is never guessed: the Shy backend only accepts `shy`,
/// and the LLVM backend accepts the 64-bit architectures whose layout is
/// [`Layout::LP64`] plus RISC-V ILP32, whose layout is [`Layout::ILP32`].
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
    let layout = target_layout(arch);
    match backend {
        Some(Backend::Shy) if name != "shy" => Err(format!(
            "the Shy backend emits ShyISA (`--target shy`), not `{name}`; \
             use --emit=llvm for that target"
        )),
        Some(Backend::Llvm) => match layout {
            Some(layout) => Ok(Target {
                triple: name.into(),
                abi: abi_kind(name),
                layout,
            }),
            None => Err(format!(
                "unsupported --target `{name}`: --emit=llvm supports x86_64, riscv32, \
                 riscv64, loongarch64, and aarch64"
            )),
        },
        _ => match arch {
            "shy" if name == "shy" => Ok(Target {
                triple: name.into(),
                abi: abi_plan::AbiKind::ShyV1,
                layout: Layout::SHY,
            }),
            _ => match layout {
                Some(layout) => Ok(Target {
                    triple: name.into(),
                    abi: abi_kind(name),
                    layout,
                }),
                None => Err(format!(
                    "unknown target `{name}`: expected `shy` or an LLVM triple \
                     (x86_64, riscv32, riscv64, loongarch64, aarch64)"
                )),
            },
        },
    }
}

/// The IR data model of an architecture shyc can lower to.
fn target_layout(arch: &str) -> Option<Layout> {
    match arch {
        "riscv32" => Some(Layout::ILP32),
        "x86_64" | "riscv64" | "loongarch64" | "aarch64" => Some(Layout::LP64),
        _ => None,
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
        "riscv32" => abi_plan::AbiKind::Riscv32,
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
    let mut instances = None;
    let mut probe = false;
    let mut probe_answers = None;
    let mut probe_manifest = None;
    let mut probe_object = None;
    let mut preprocess = preprocess::Options::default();
    // The bundled headers describe the Shy target, so they are searched before
    // any `-I` directory and before the host C library.
    preprocess
        .include_dirs
        .push(Path::new(env!("CARGO_MANIFEST_DIR")).join("include"));
    // The project's C library is not part of the compiler: `~/.shycc.toml`
    // names it, and a checkout provides it next to the crates. Its layers are
    // what a `use std::io::*` resolves to.
    if let Some(shylib) = shyc::config::shylib() {
        preprocess
            .include_dirs
            .extend(shyc::config::shylib_includes(&shylib));
    }
    while let Some(arg) = args.next() {
        let selected = match arg.as_str() {
            "--help" | "-h" => {
                println!(
                    "usage: shyc [options] file\n\
                     output: -S / --emit=shy (default), --emit=llvm, --emit=c, \
                     --emit=c-header, --emit=c-wrapper, -E, --dump-tokens, --dump-ast, \
                     --dump-ir\n\
                     options: -O0..-O3, --target <triple>, -ffreestanding, -fhosted, \
                     -I dir, -D name[=value], -U name, -o file, --instances <file>\n\
                     --emit=llvm requires a 64-bit --target triple; \
                     assembly and linking stay in shycc; \
                     --instances lists explicit generic instances to export to C \
                     (with --emit=c-header / --emit=c-wrapper or a code generator); \
                     --constfn-probe --probe-answers <file> --probe-manifest <file> \
                     [--probe-object <file>] compiles one const-fn probe round \
                     instead of the program (-S, --emit=c, --emit=c-header; \
                     --probe-object receives the probe unit of a round with \
                     requests, which then writes no main output)"
                );
                return Ok(None);
            }
            "--version" => {
                println!("shyc {}", env!("CARGO_PKG_VERSION"));
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
            "--instances" => {
                instances = Some(PathBuf::from(
                    args.next().ok_or("--instances requires a path")?,
                ));
                None
            }
            flag if flag.starts_with("--instances=") => {
                instances = Some(PathBuf::from(&flag["--instances=".len()..]));
                None
            }
            "--constfn-probe" | "--probe" => {
                probe = true;
                None
            }
            "--probe-answers" => {
                probe_answers = Some(PathBuf::from(
                    args.next().ok_or("--probe-answers requires a path")?,
                ));
                None
            }
            "--probe-manifest" => {
                probe_manifest = Some(PathBuf::from(
                    args.next().ok_or("--probe-manifest requires a path")?,
                ));
                None
            }
            "--probe-object" => {
                probe_object = Some(PathBuf::from(
                    args.next().ok_or("--probe-object requires a path")?,
                ));
                None
            }
            flag if flag.starts_with("--probe-answers=") => {
                probe_answers = Some(PathBuf::from(&flag["--probe-answers=".len()..]));
                None
            }
            flag if flag.starts_with("--probe-manifest=") => {
                probe_manifest = Some(PathBuf::from(&flag["--probe-manifest=".len()..]));
                None
            }
            flag if flag.starts_with("--probe-object=") => {
                probe_object = Some(PathBuf::from(&flag["--probe-object=".len()..]));
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
                "c-wrapper" => Stage::CWrapper,
                other => return Err(format!("unknown --emit value: {other}")),
            }),
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
                 or --emit=<output>"
                    .into(),
            );
        }
    }
    if target.as_deref().is_some_and(is_bare_target) {
        freestanding = true;
    }
    // `__shy__` means the ShyISA target. A freestanding riscv64 build is not
    // Shy: it must get LLVM's varargs, not the Shy descriptor.
    if target.as_deref().is_some_and(|target| target != "shy") {
        preprocess.undefines.push("__shy__".to_string());
    }
    let target_name = target.as_deref();
    let arch = target_name.and_then(|name| name.split('-').next());
    let is_shy = target_name.is_none_or(|name| name == "shy");
    // A hosted C library only applies to the host itself: a cross target must
    // not search the host's headers. These directories come after the bundled
    // and `-I` headers, as with gcc.
    if !is_shy && !freestanding && arch == Some(env::consts::ARCH) {
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
    }
    if is_shy {
        preprocess
            .defines
            .push(("__SIZEOF_POINTER__".to_string(), "4".to_string()));
    } else {
        // Data model and architecture facts. A freestanding header still has
        // to spell `size_t` and `int64_t` correctly, so these do not depend on
        // a hosted C library. The Shy target uses its own bundled headers and
        // only needs the pointer size above.
        let mut macros: Vec<(&str, &str)> = vec![
            ("__STDC__", "1"),
            ("__STDC_VERSION__", "201710L"),
            ("__WCHAR_TYPE__", "int"),
            ("__CHAR_BIT__", "8"),
            ("__INT_MAX__", "2147483647"),
            ("__WCHAR_MAX__", "2147483647"),
            ("__WCHAR_MIN__", "(-2147483647 - 1)"),
            ("__SCHAR_MAX__", "127"),
            ("__SHRT_MAX__", "32767"),
            ("__INT8_TYPE__", "signed char"),
            ("__UINT8_TYPE__", "unsigned char"),
            ("__INT16_TYPE__", "short int"),
            ("__UINT16_TYPE__", "short unsigned int"),
            ("__INT32_TYPE__", "int"),
            ("__UINT32_TYPE__", "unsigned int"),
            ("__LONG_LONG_MAX__", "9223372036854775807LL"),
            ("__FLT_MAX__", "3.40282346638528859812e+38F"),
            ("__DBL_MAX__", "1.79769313486231570815e+308"),
            ("__DBL_MIN__", "2.22507385850720138309e-308"),
            ("__DBL_EPSILON__", "2.22044604925031308085e-16"),
            ("__ELF__", "1"),
        ];
        if arch == Some("riscv32") {
            // ILP32: `long`, pointers, and `size_t` are 32 bits; only
            // `long long` and `intmax_t` stay 64 bits.
            macros.extend([
                ("__SIZEOF_POINTER__", "4"),
                ("__SIZEOF_LONG__", "4"),
                ("__SIZE_TYPE__", "unsigned int"),
                ("__PTRDIFF_TYPE__", "int"),
                ("__INTPTR_TYPE__", "int"),
                ("__UINTPTR_TYPE__", "unsigned int"),
                ("__INT64_TYPE__", "long long int"),
                ("__UINT64_TYPE__", "long long unsigned int"),
                ("__INTMAX_TYPE__", "long long int"),
                ("__UINTMAX_TYPE__", "long long unsigned int"),
                ("__LONG_MAX__", "2147483647"),
                ("__SIZE_MAX__", "4294967295U"),
                ("__INTMAX_MAX__", "9223372036854775807LL"),
                ("__UINTMAX_MAX__", "18446744073709551615ULL"),
                ("__PTRDIFF_MAX__", "2147483647"),
            ]);
        } else {
            macros.extend([
                ("__SIZEOF_POINTER__", "8"),
                ("__SIZEOF_LONG__", "8"),
                ("__SIZE_TYPE__", "long unsigned int"),
                ("__PTRDIFF_TYPE__", "long int"),
                ("__INTPTR_TYPE__", "long int"),
                ("__UINTPTR_TYPE__", "long unsigned int"),
                ("__INT64_TYPE__", "long int"),
                ("__UINT64_TYPE__", "long unsigned int"),
                ("__INTMAX_TYPE__", "long int"),
                ("__UINTMAX_TYPE__", "long unsigned int"),
                ("__LONG_MAX__", "9223372036854775807L"),
                ("__SIZE_MAX__", "18446744073709551615UL"),
                ("__INTMAX_MAX__", "9223372036854775807L"),
                ("__UINTMAX_MAX__", "18446744073709551615UL"),
                ("__PTRDIFF_MAX__", "9223372036854775807L"),
                ("__LP64__", "1"),
            ]);
        }
        // The architecture macro comes from the triple, not from the host.
        let arch_macros: &[(&str, &str)] = match arch {
            Some("x86_64") => &[("__x86_64__", "1")],
            Some("aarch64") => &[("__aarch64__", "1")],
            Some("riscv64") => &[("__riscv", "1"), ("__riscv64", "1"), ("__riscv_xlen", "64")],
            Some("riscv32") => &[("__riscv", "1"), ("__riscv32", "1"), ("__riscv_xlen", "32")],
            Some("loongarch64") => &[("__loongarch64", "1")],
            _ => &[],
        };
        macros.extend(arch_macros);
        if !freestanding {
            macros.extend([
                ("__STDC_HOSTED__", "1"),
                ("__linux__", "1"),
                ("__unix__", "1"),
            ]);
        }
        for (name, value) in macros {
            preprocess
                .defines
                .push((name.to_string(), value.to_string()));
        }
    }
    let stage = stage.unwrap_or(Stage::Emit(Backend::Shy));
    if probe {
        // A probe round runs Shy assembly, so only the outputs whose final
        // text can follow a round take part: the assembly itself and the two C
        // text outputs. A probe unit is not a program, so an export table
        // would name the program instead of the probe.
        match stage {
            Stage::Emit(Backend::Shy) | Stage::CSource | Stage::CHeader => {}
            Stage::Emit(Backend::Llvm) => {
                return Err(
                    "--constfn-probe cannot be combined with --emit=llvm: a probe round is \
                     Shy assembly"
                        .into(),
                );
            }
            Stage::CWrapper => {
                return Err(
                    "--constfn-probe cannot be combined with --emit=c-wrapper: a probe round \
                     produces no wrapper"
                        .into(),
                );
            }
            _ => {
                return Err(
                    "--constfn-probe only applies to -S / --emit=shy, --emit=c, or \
                     --emit=c-header"
                        .into(),
                );
            }
        }
        if matches!(stage, Stage::CSource | Stage::CHeader) && probe_object.is_none() {
            // The round's assembly is not the C text the user asked for, so it
            // needs a path of its own; repurposing the main output would print
            // Shy assembly as C.
            return Err(
                "--constfn-probe with --emit=c or --emit=c-header requires --probe-object <path>"
                    .into(),
            );
        }
        if target.as_deref().is_some_and(|name| name != "shy") {
            return Err(
                "--constfn-probe only applies to the shy target: other targets link through \
                 clang and have no Shy probe image"
                    .into(),
            );
        }
        if instances.is_some() {
            return Err("--constfn-probe cannot be combined with --instances".into());
        }
        if probe_manifest.is_none() {
            return Err("--constfn-probe requires --probe-manifest <path>".into());
        }
    } else if probe_answers.is_some() || probe_manifest.is_some() || probe_object.is_some() {
        return Err(
            "--probe-answers, --probe-manifest, and --probe-object only apply to --constfn-probe"
                .into(),
        );
    }
    Ok(Some(Options {
        stage,
        input: input.ok_or("no input source file")?,
        output,
        target,
        opt_level,
        freestanding,
        instances,
        probe,
        probe_answers,
        probe_manifest,
        probe_object,
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

/// 预处理结果的一段可以原样喂给下一次 shyc 运行（`-E`）。
///
/// 属性行写成注释形式 `// #![...]`：这份输出同时可能作为 C 兼容源码被普通 C
/// 编译器编译，裸属性会让它编译失败；写成注释，shyc 仍然认得。
fn render_preprocessed(tokens: &[Token], attributes: &Attributes) -> String {
    let mut output = String::new();
    if attributes.no_main {
        output.push_str("// #![no_main]\n");
    }
    if let Some(bytes) = attributes.memory {
        output.push_str(&format!("// #![mem({bytes})]\n"));
    }
    if let Some(bytes) = attributes.stack {
        output.push_str(&format!("// #![stack({bytes})]\n"));
    }
    if attributes.disable_nonexhaustive_switch {
        output.push_str("// #![disable_nonexhaustive_switch]\n");
    }
    if attributes.disable_rustic_mut {
        output.push_str("// #![disable_rustic_mut]\n");
    }
    if attributes.disable_default_static {
        output.push_str("// #![disable_default_static]\n");
    }
    output.push_str(&render_tokens(tokens));
    output
}

/// Write the preprocessed token stream back out as one C-compatible line each.
///
/// Every token keeps the text the lexer gave it and a space separates them all,
/// so adjacent macro expansions cannot fuse into one token when the output is
/// lexed again. ShyC import declarations are dropped: they are a compile-time
/// check with no C spelling.
fn render_tokens(tokens: &[Token]) -> String {
    let mut output = String::new();
    let mut index = 0;
    while index < tokens.len() {
        let token = &tokens[index];
        index += 1;
        if let Some(end) = use_declaration_end(tokens, index - 1) {
            if token.at_line_start && !output.is_empty() && !output.ends_with('\n') {
                output.push('\n');
            }
            index = end + 1;
            continue;
        }
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

/// The index of the `;` ending the import that starts at `start`, if the tokens
/// there have the `use name :: ...` shape.
fn use_declaration_end(tokens: &[Token], start: usize) -> Option<usize> {
    if !matches!(&tokens[start].kind, TokenKind::Ident(word) if word == "use") {
        return None;
    }
    let name = next_significant(tokens, start + 1)?;
    if !matches!(tokens[name].kind, TokenKind::Ident(_)) {
        return None;
    }
    let separator = next_significant(tokens, name + 1)?;
    if !matches!(&tokens[separator].kind, TokenKind::Punct(punct) if punct == "::") {
        return None;
    }
    // The parser already validated the path, so the first `;` closes it.
    (separator..tokens.len())
        .find(|&index| matches!(&tokens[index].kind, TokenKind::Punct(punct) if punct == ";"))
}

/// The first index at or after `from` whose token is not a newline marker.
fn next_significant(tokens: &[Token], from: usize) -> Option<usize> {
    (from..tokens.len()).find(|&index| !matches!(tokens[index].kind, TokenKind::Newline))
}
/// First line of both files of the const-fn probe protocol.
const PROBE_HEADER: &str = "shyc-probe 1";

/// The manifest word for one result kind.
///
/// The probe module classifies the result; this only spells the classification
/// for the driver, so both sides agree on what the returned 32-bit word means.
fn kind_word(kind: probe::ResultKind) -> &'static str {
    match kind {
        probe::ResultKind::I32Signed => "i32s",
        probe::ResultKind::I32Unsigned => "i32u",
        probe::ResultKind::Bool => "bool",
        probe::ResultKind::Tokens => "tokens",
    }
}

/// The value the program sees, from the decimal a driver wrote for a scalar
/// answer, or why this kind has no decimal form.
fn int_answer(kind: probe::ResultKind, decimal: i128) -> Result<i64, String> {
    let bits = decimal as u64 as u32;
    let (range, value) = match kind {
        probe::ResultKind::I32Signed => (-2_147_483_648..=2_147_483_647, i64::from(bits as i32)),
        probe::ResultKind::I32Unsigned => (0..=4_294_967_295, i64::from(bits)),
        probe::ResultKind::Bool => (0..=1, i64::from(bits)),
        probe::ResultKind::Tokens => {
            return Err("a Tokens result has no decimal answer".to_string());
        }
    };
    if !range.contains(&decimal) {
        return Err(format!(
            "{decimal} is out of range for a `{}` answer",
            kind_word(kind)
        ));
    }
    Ok(value)
}

/// Compiles one probe round of a translation unit.
///
/// With requests, the round produces the internal probe assembly the driver
/// links and runs, and the manifest names them: `--probe-object` receives that
/// assembly, and without it the assembly is the main output (`-S`). Without
/// requests, the output is what the selected stage would produce for the
/// program itself and the manifest is header-only.
fn probe_round(options: &Options) -> Result<(), String> {
    let text = fs::read_to_string(&options.input)
        .map_err(|error| format!("{}: {error}", options.input.display()))?;
    let mut sources = SourceMap::default();
    let file = sources.add(options.input.clone(), text);
    let preprocessed = preprocess::preprocess(&mut sources, file, &options.preprocess)
        .map_err(|error| sources.render(&error))?;
    let manifest_path = options
        .probe_manifest
        .as_deref()
        .ok_or("--constfn-probe requires --probe-manifest <path>")?;

    // An answer's reading comes from the declaration, and the round's requests
    // name the function each key belongs to. Both come from a parse without
    // answers; the source is the same, so the keys are the same.
    let mut parsed = parser::parse_for_probe(&preprocessed.tokens, parser::Answers::default())
        .map_err(|error| sources.render(&error))?;
    if let Some(path) = options.probe_answers.as_deref() {
        // One answer can unlock requests that only exist after an expansion (a
        // generator may emit a call of its own), so keep feeding answers until
        // the file has nothing new for the parse it just produced.
        let mut collected: Vec<(parser::RequestKey, parser::Answer)> = Vec::new();
        loop {
            let fresh = read_answers(path, &parsed, &collected)?;
            if fresh.is_empty() {
                break;
            }
            collected.extend(fresh);
            let mut answers = parser::Answers::default();
            for (key, answer) in &collected {
                answers.insert(key.clone(), answer.clone());
            }
            parsed = parser::parse_for_probe(&preprocessed.tokens, answers)
                .map_err(|error| sources.render(&error))?;
        }
    }

    let target = resolve_target(options)?;
    let attributes = &preprocessed.attributes;
    if !parsed.requests.is_empty() {
        // A declaration this round has not shaped yet is layout or data, and no
        // probe run can trap a read of it. Those requests are answered before
        // any other, so a generator never reads a template the file has not
        // finished; the sites come from the whole request list, so a request
        // the trim below drops still keeps its declaration out of the image.
        if parsed
            .requests
            .iter()
            .any(|request| matches!(request.place, parser::Place::Type | parser::Place::Member))
        {
            // Values can depend on a type produced by this round, so finish
            // layout requests before evaluating global initializers as well.
            parsed.requests.retain(|request| {
                matches!(request.place, parser::Place::Type | parser::Place::Member)
            });
        } else if !parsed.unresolved_declarations.is_empty() {
            let declarations = parsed.unresolved_declarations.as_slice();
            parsed
                .requests
                .retain(|request| declarations.contains(&request.site));
        }
        // A probe image is not a program. Its unit holds the `const fn` clones
        // and the `_start` trampoline that calls them, so it is lowered without
        // the compiler's automatic startup and never reaches a `main`.
        let program = match probe::build(&mut parsed) {
            probe::Outcome::Unit(program) => program,
            probe::Outcome::Nothing => {
                return Err("const-fn probe round has requests but no probe unit".into());
            }
            probe::Outcome::Unsupported {
                function,
                span,
                reason,
            } => {
                let error = Diagnostic::new(
                    span,
                    format!("const fn '{function}' cannot be probed in this round: {reason}"),
                );
                return Err(sources.render(&error));
            }
        };
        let lowered = lower::lower(
            &program.unit,
            &lower::Options {
                // A probe image is linked next to the packaged library objects,
                // so its module name must not repeat theirs: a user file named
                // `std.shyc` would otherwise collide on `.L.std.*` sections.
                module_name: "probe".to_string(),
                layout: target.layout,
                startup: StartupMode::Manual,
                abi: target.abi,
                disable_nonexhaustive_switch: attributes.disable_nonexhaustive_switch,
                disable_rustic_mut: attributes.disable_rustic_mut,
                disable_default_static: attributes.disable_default_static,
                exports: Vec::new(),
            },
        )
        .map_err(|error| sources.render(&error))?;
        let assembly = shy::emit(
            &lowered.module,
            &shy::Options {
                layout: target.layout,
                emit_source_comments: false,
            },
        )
        .map_err(|error| sources.render(&error))?;
        let trampoline = probe::emit_probe_asm(&lowered, &program.calls)
            .map_err(|reason| format!("{}: probe trampoline: {reason}", options.input.display()))?;
        let manifest = round_manifest(&parsed, &sources)?;
        fs::write(manifest_path, manifest)
            .map_err(|error| format!("{}: {error}", manifest_path.display()))?;
        let assembly = format!("{assembly}{trampoline}");
        // A C output has no room for assembly, and a round with requests is not
        // its final text: the driver sends the probe unit to its own path.
        if let Some(path) = options.probe_object.as_deref() {
            return fs::write(path, assembly)
                .map_err(|error| format!("{}: {error}", path.display()));
        }
        return write_output(options.output.as_deref(), &assembly);
    }
    // Nothing is left to ask, so this unit is the program: the selected stage
    // renders it exactly as it would without probe rounds.
    let output = match options.stage {
        Stage::CSource => render_c_source(options, &parsed.unit, attributes, &mut sources)?,
        Stage::CHeader => {
            // The output path names the include guard. Without `-o` the header
            // goes to stdout, so the input path names it instead.
            let guard_path = options.output.as_deref().unwrap_or(options.input.as_path());
            let header_options = c::header::HeaderOptions {
                guard: c::header::header_guard(guard_path),
                root: file,
                includes: preprocessed.includes,
                defines: preprocessed.defines,
            };
            render_header(
                options,
                &parsed.unit,
                &header_options,
                attributes,
                &mut sources,
            )?
        }
        _ => {
            let startup = if attributes.no_main || options.freestanding {
                StartupMode::Manual
            } else {
                StartupMode::Main
            };
            let mut lowered = lower::lower(
                &parsed.unit,
                &lower::Options {
                    module_name: module_name(&options.input),
                    layout: target.layout,
                    startup,
                    abi: target.abi,
                    disable_nonexhaustive_switch: attributes.disable_nonexhaustive_switch,
                    disable_rustic_mut: attributes.disable_rustic_mut,
                    disable_default_static: attributes.disable_default_static,
                    exports: Vec::new(),
                },
            )
            .map_err(|error| sources.render(&error))?;
            lowered.module.resources.memory_bytes = attributes.memory.map(u64::from);
            lowered.module.resources.stack_bytes = attributes.stack.map(u64::from);
            if options.opt_level > 0 {
                for function in &mut lowered.module.functions {
                    passes::optimize(function, options.opt_level);
                }
            }
            shy::emit(
                &lowered.module,
                &shy::Options {
                    layout: target.layout,
                    emit_source_comments: false,
                },
            )
            .map_err(|error| sources.render(&error))?
        }
    };
    // A round that reaches here has no request, so the manifest is header-only
    // and the output above is the program.
    let manifest = round_manifest(&parsed, &sources)?;
    fs::write(manifest_path, manifest)
        .map_err(|error| format!("{}: {error}", manifest_path.display()))?;
    write_output(options.output.as_deref(), &output)
}

/// Reads the answers of the previous round.
///
/// A scalar answer is `<key>\tint32\t<decimal>`, where the decimal is shaped
/// by the kind the manifest named for that request. A Tokens answer is
/// `<key>\ttokens\t<count>\t<kind>:<hex>;...`, one semicolon-terminated token
/// record per entry, and a count of zero spells no entries at all. A missing
/// file is a first round, not an error.
fn read_answers(
    path: &Path,
    parsed: &parser::ParseOutput,
    known: &[(parser::RequestKey, parser::Answer)],
) -> Result<Vec<(parser::RequestKey, parser::Answer)>, String> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let text = fs::read_to_string(path).map_err(|error| format!("{}: {error}", path.display()))?;
    let mut lines = text.lines();
    if lines.next().map(str::trim_end) != Some(PROBE_HEADER) {
        return Err(format!(
            "{}: the first line must be `{PROBE_HEADER}`",
            path.display()
        ));
    }
    let mut fresh = Vec::new();
    for (index, line) in lines.enumerate() {
        let line = line.trim_end();
        if line.is_empty() {
            continue;
        }
        let number = index + 2;
        let mut fields = line.splitn(5, '\t');
        let (Some(key), Some(kind), Some(value)) = (fields.next(), fields.next(), fields.next())
        else {
            return Err(format!(
                "{}:{number}: expected `<key>\\t<kind>\\t<value>`",
                path.display()
            ));
        };
        let detail = fields.next();
        let extra = fields.next();
        let Some(key) = parser::RequestKey::from_text(key) else {
            return Err(format!(
                "{}:{number}: malformed request key `{key}`",
                path.display()
            ));
        };
        // An answer for a call this parse does not ask about cannot change the
        // result: a file that keeps earlier rounds' entries is expected. The
        // ones already handed to the parse are skipped, so the caller can tell
        // when feeding more stops changing anything.
        if known.iter().any(|(seen, _)| *seen == key) {
            continue;
        }
        let Some(request) = parsed.requests.iter().find(|request| request.key == key) else {
            continue;
        };
        let answer = match kind {
            "int32" => {
                let kind =
                    probe::result_kind(&parsed.const_fns[request.function]).map_err(|reason| {
                        format!(
                            "{}:{number}: const fn '{}': {reason}",
                            path.display(),
                            request.name
                        )
                    })?;
                let decimal: i128 = value.trim().parse().map_err(|_| {
                    format!(
                        "{}:{number}: `{}` is not a decimal integer",
                        path.display(),
                        value.trim()
                    )
                })?;
                let value = int_answer(kind, decimal)
                    .map_err(|reason| format!("{}:{number}: {reason}", path.display()))?;
                parser::Answer::Int(value)
            }
            "tokens" => {
                if extra.is_some() {
                    return Err(format!(
                        "{}:{number}: unexpected fields after token answer",
                        path.display()
                    ));
                }
                parser::Answer::Tokens(
                    token_answer(value, detail, request.site)
                        .map_err(|reason| format!("{}:{number}: {reason}", path.display()))?,
                )
            }
            "suspended" => {
                let need = match value {
                    "0" => false,
                    "1" => true,
                    _ => {
                        return Err(format!(
                            "{}:{number}: suspension kind must be 0 or 1",
                            path.display()
                        ));
                    }
                };
                let owner = detail.and_then(decode_hex).ok_or_else(|| {
                    format!("{}:{number}: invalid suspension owner hex", path.display())
                })?;
                let trait_name = extra.and_then(decode_hex).ok_or_else(|| {
                    format!("{}:{number}: invalid suspension trait hex", path.display())
                })?;
                parser::Answer::Suspended {
                    need,
                    owner: String::from_utf8(owner).map_err(|_| {
                        format!("{}:{number}: suspension owner is not UTF-8", path.display())
                    })?,
                    trait_name: String::from_utf8(trait_name).map_err(|_| {
                        format!("{}:{number}: suspension trait is not UTF-8", path.display())
                    })?,
                }
            }
            other => {
                return Err(format!(
                    "{}:{number}: `{other}` is not an answer kind this stage reads",
                    path.display()
                ));
            }
        };
        fresh.push((key, answer));
    }
    Ok(fresh)
}

/// The tokens one `tokens` answer names, or why the line is malformed.
///
/// Every spelling is a byte record the probe committed, in the order the
/// generator produced them, and every token carries the span of the call that
/// asked for the answer, the way tokens generated in this process do.
fn token_answer(count: &str, items: Option<&str>, span: Span) -> Result<Vec<Token>, String> {
    let count: usize = count
        .trim()
        .parse()
        .map_err(|_| format!("`{count}` is not a token count"))?;
    let mut tokens = Vec::with_capacity(count);
    for item in items.unwrap_or("").split_terminator(';') {
        let item = item.trim();
        if item.is_empty() {
            continue;
        }
        let (kind, hex) = item
            .split_once(':')
            .ok_or_else(|| format!("`{item}` is not `<kind>:<hex>`"))?;
        let number: u32 = kind
            .trim()
            .parse()
            .map_err(|_| format!("`{kind}` is not a token kind number"))?;
        let bytes = decode_hex(hex.trim()).ok_or_else(|| format!("`{hex}` is not hex"))?;
        let spelling =
            String::from_utf8(bytes).map_err(|_| format!("token {number} is not valid UTF-8"))?;
        let Some(kind) = parser::token_from_probe(number, &spelling) else {
            return Err(format!("`{number}` is not a token kind"));
        };
        let at_line_start = tokens.last().map_or(true, |previous: &Token| {
            matches!(previous.kind, TokenKind::Newline)
        });
        tokens.push(Token {
            kind,
            span,
            at_line_start,
            leading_space: true,
        });
    }
    if tokens.len() != count {
        return Err(format!(
            "the answer names {count} token(s) but spells {}",
            tokens.len()
        ));
    }
    Ok(tokens)
}

/// The bytes a hex string spells, or `None` when it is not hex.
fn decode_hex(text: &str) -> Option<Vec<u8>> {
    let digits = text.as_bytes();
    if !digits.len().is_multiple_of(2) {
        return None;
    }
    let mut bytes = Vec::with_capacity(digits.len() / 2);
    for pair in digits.chunks_exact(2) {
        let pair = std::str::from_utf8(pair).ok()?;
        bytes.push(u8::from_str_radix(pair, 16).ok()?);
    }
    Some(bytes)
}

/// The manifest of one round: `id\tkey\tkind` per request.
///
/// The id is the request's index, which is the adapter `probe::build`
/// generates for it, so a RESULT frame can be paired with the request that
/// asked. The kind is what the driver needs to shape the answer it writes.
fn manifest_text(
    requests: &[parser::ProbeRequest],
    const_fns: &[parser::ConstFn],
) -> Result<String, (usize, String)> {
    let mut text = String::from(PROBE_HEADER);
    text.push('\n');
    for (id, request) in requests.iter().enumerate() {
        let kind =
            probe::result_kind(&const_fns[request.function]).map_err(|reason| (id, reason))?;
        text.push_str(&format!(
            "{id}\t{}\t{}\n",
            request.key.text(),
            kind_word(kind)
        ));
    }
    Ok(text)
}

/// The manifest of one round, or the diagnostic of a request it cannot name.
fn round_manifest(parsed: &parser::ParseOutput, sources: &SourceMap) -> Result<String, String> {
    match manifest_text(&parsed.requests, &parsed.const_fns) {
        Ok(text) => Ok(text),
        Err((id, reason)) => {
            let request = &parsed.requests[id];
            let error = Diagnostic::new(
                request.site,
                format!(
                    "const fn '{}' cannot be probed in this round: {reason}",
                    request.name
                ),
            );
            Err(sources.render(&error))
        }
    }
}

/// Writes a backend's text to `-o`, or to stdout.
fn write_output(path: Option<&Path>, text: &str) -> Result<(), String> {
    match path {
        Some(path) => fs::write(path, text).map_err(|error| format!("{}: {error}", path.display())),
        None => io::stdout()
            .lock()
            .write_all(text.as_bytes())
            .map_err(|error| error.to_string()),
    }
}
