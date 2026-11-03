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
use shyc::{c, export, export_instances, llvm, parser, passes, shy};

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
            if options.instances.is_some() {
                return Err(
                    "--instances exports concrete generics and needs --emit=c-header, \
                     --emit=c-wrapper, or one of the code-generation outputs; --emit=c cannot \
                     spell a generic instance"
                        .into(),
                );
            }
            let preprocessed = preprocess::preprocess(&mut sources, file, &options.preprocess)
                .map_err(|error| sources.render(&error))?;
            let unit = parser::parse(&preprocessed.tokens)
                .map_err(|error| sources.render(&error))?;
            c::source::render(&unit).map_err(|error| sources.render(&error))?
        }
        Stage::CHeader => {
            let preprocessed = preprocess::preprocess(&mut sources, file, &options.preprocess)
                .map_err(|error| sources.render(&error))?;
            let unit =
                parser::parse(&preprocessed.tokens).map_err(|error| sources.render(&error))?;
            // The output path names the include guard. Without `-o` the header
            // goes to stdout, so the input path names it instead.
            let guard_path = options.output.as_deref().unwrap_or(options.input.as_path());
            let header_options = export::HeaderOptions {
                guard: export::header_guard(guard_path),
                root: file,
                includes: preprocessed.includes,
                defines: preprocessed.defines,
            };
            if options.instances.is_none() {
                export::render_c_header(&unit, &header_options)
            } else {
                let lowered =
                    lower_instances(&options, &unit, &preprocessed.attributes, &mut sources)?;
                let unspelled = export::unspelled_function_names(&unit, file);
                let section = export_instances::render_header_section(&lowered, &unspelled);
                export::render_c_header_with(&unit, &header_options, &section)
            }
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
                     --emit=c-header, --emit=c-wrapper, -E, --dump-tokens, --dump-ast, \
                     --dump-ir\n\
                     options: -O0..-O3, --target <triple>, -ffreestanding, -fhosted, \
                     -I dir, -D name[=value], -U name, -o file, --instances <file>\n\
                     --emit=llvm requires a 64-bit --target triple; \
                     assembly and linking stay in shycc; \
                     --instances lists explicit generic instances to export to C \
                     (with --emit=c-header / --emit=c-wrapper or a code generator)"
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
    Ok(Some(Options {
        stage: stage.unwrap_or(Stage::Emit(Backend::Shy)),
        input: input.ok_or("no input source file")?,
        output,
        target,
        opt_level,
        freestanding,
        instances,
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
    output.push_str(&export::render_c_source(tokens));
    output
}
