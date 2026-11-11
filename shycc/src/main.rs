use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{Context, Result, bail};

mod cache;
mod probe;

use cache::{Cache, Fingerprint, cache};

static TMP_COUNTER: AtomicUsize = AtomicUsize::new(0);

/// Default limits of one probe round: the emulator's own defaults.
const DEFAULT_PROBE_STEPS: u64 = 32_000_000;
const DEFAULT_PROBE_TIMEOUT_MS: u64 = 2_000;
/// A probe that keeps asking for the same thing is a bug; stop instead.
const DEFAULT_PROBE_ROUNDS: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    Preprocess,
    Asm,
    Object,
    Link,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InputKind {
    C,
    Asm,
    Obj,
}

struct Options {
    stage: Stage,
    output: Option<PathBuf>,
    sym: Option<PathBuf>,
    save_temps: bool,
    print_only: bool,
    emit_source_lines: bool,
    /// `-nostdlib` / `-nodefaultlibs`: do not add libshy or the float runtime.
    no_default_libs: bool,
    /// Target triple. `shy` selects the Shy ISA pipeline, `host` is replaced
    /// by the machine's own triple, and anything else is lowered to LLVM IR
    /// and finished by clang.
    target: String,
    /// `--sysroot <dir>`: the C library root for a hosted cross link.
    sysroot: Option<PathBuf>,
    /// `--emit=<what>`: print one of the compiler's text outputs instead of
    /// compiling. C source, C header, and the C wrapper have no object form, so
    /// this stops after the frontend.
    emit: Option<String>,
    /// Whether the user spelled `--target`. The C source output takes the
    /// host data model when they did not, because a host C compiler builds it.
    target_given: bool,
    emit_debug_info: bool,
    /// Whether `const fn` calls run as probe rounds in the emulator.
    ///
    /// On by default (`--constfn-probe` spells it, `--no-constfn-probe` turns
    /// it off): the probe image is temporary and the final image is never the
    /// thing that is executed.
    probe: bool,
    /// Whether the user spelled `--constfn-probe`; the default quietly turns
    /// off where a probe cannot run.
    probe_explicit: bool,
    /// Instruction budget of one probe round.
    constfn_steps: u64,
    /// Wall-clock budget of one probe round, in milliseconds.
    constfn_timeout_ms: u64,
    /// Rounds a probe may take before it counts as unsettled.
    constfn_rounds: usize,
    /// `--probe-only`: run the probe rounds of one input and stop.
    probe_only: bool,
    /// Where `--probe-only` writes the answers its rounds collected.
    probe_answers: Option<PathBuf>,
    /// `--no-cache`: build every default-library artifact from source instead of
    /// reusing the one an earlier run stored.
    no_cache: bool,
    compile_args: Vec<String>,
    /// `-Wl,` items, forwarded to clang; the Shy linker validates them itself.
    link_args: Vec<String>,
    inputs: Vec<String>,
    libs: Vec<String>,
}

#[derive(Debug)]
struct TempFiles {
    keep: bool,
    paths: Vec<PathBuf>,
}

impl TempFiles {
    fn new(keep: bool) -> Self {
        Self {
            keep,
            paths: Vec::new(),
        }
    }

    fn temp_path(&mut self, ext: &str) -> PathBuf {
        let idx = TMP_COUNTER.fetch_add(1, Ordering::SeqCst);
        let pid = std::process::id();
        let path = env::temp_dir().join(format!("shycc-{pid}-{idx}{ext}"));
        self.paths.push(path.clone());
        path
    }
}

impl Drop for TempFiles {
    fn drop(&mut self) {
        if self.keep {
            return;
        }
        for path in &self.paths {
            let _ = fs::remove_file(path);
        }
    }
}

/// Default library objects a probe image links, and the final link reuses when
/// it links the same libraries.
///
/// A probe unit may call the same String and Display helpers the program would,
/// so the probe image always links the default libraries, even under
/// `-nostdlib`: that flag governs the program's dependencies, not the compiler's
/// const-fn execution environment. The objects reach the temporary probe image;
/// the artifact still gets exactly what the final link lists. Building them once
/// keeps each probe round from recompiling libshy.
struct ProbeLibs {
    libshy: Vec<PathBuf>,
    float: PathBuf,
}

fn main() -> Result<()> {
    let opts = parse_args(env::args().skip(1).collect())?;
    run(opts)
}

fn parse_args(args: Vec<String>) -> Result<Options> {
    let mut opts = Options {
        stage: Stage::Link,
        output: None,
        sym: None,
        save_temps: false,
        print_only: false,
        emit_source_lines: false,
        no_default_libs: false,
        target: "shy".to_string(),
        sysroot: None,
        emit: None,
        target_given: false,
        emit_debug_info: false,
        probe: cfg!(feature = "constfn-probe"),
        probe_explicit: false,
        constfn_steps: DEFAULT_PROBE_STEPS,
        constfn_timeout_ms: DEFAULT_PROBE_TIMEOUT_MS,
        constfn_rounds: DEFAULT_PROBE_ROUNDS,
        probe_only: false,
        probe_answers: None,
        no_cache: false,
        compile_args: Vec::new(),
        link_args: Vec::new(),
        inputs: Vec::new(),
        libs: Vec::new(),
    };

    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        match arg.as_str() {
            "--help" => {
                print_usage();
                std::process::exit(0);
            }
            "--version" => {
                println!("shycc {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            "-nostdlib" | "-nodefaultlibs" | "-nolibc" => opts.no_default_libs = true,
            "--target" => {
                i += 1;
                let Some(value) = args.get(i) else {
                    bail!("option `--target` requires an argument");
                };
                opts.target = value.clone();
                opts.target_given = true;
            }
            flag if flag.starts_with("--target=") => {
                opts.target = flag["--target=".len()..].to_string();
                opts.target_given = true;
            }
            "--sysroot" => {
                i += 1;
                let Some(value) = args.get(i) else {
                    bail!("option `--sysroot` requires an argument");
                };
                opts.sysroot = Some(PathBuf::from(value));
            }
            flag if flag.starts_with("--sysroot=") => {
                opts.sysroot = Some(PathBuf::from(&flag["--sysroot=".len()..]));
            }
            flag if flag.starts_with("--emit=") => {
                let what = &flag["--emit=".len()..];
                if !matches!(what, "c" | "c-header" | "c-wrapper" | "shy" | "llvm") {
                    bail!(
                        "unknown --emit value: {what} (expected c, c-header, c-wrapper, shy, or llvm)"
                    );
                }
                opts.emit = Some(what.to_string());
            }
            "-###" => opts.print_only = true,
            "--no-cache" => opts.no_cache = true,
            "--constfn-probe" | "--probe" => {
                opts.probe = true;
                opts.probe_explicit = true;
            }
            "--no-constfn-probe" => opts.probe = false,
            "--probe-only" => {
                opts.probe = true;
                opts.probe_only = true;
            }
            "--probe-answers" => {
                i += 1;
                let Some(value) = args.get(i) else {
                    bail!("option `--probe-answers` requires an argument");
                };
                opts.probe_answers = Some(PathBuf::from(value));
            }
            "--constfn-steps" => {
                i += 1;
                let Some(value) = args.get(i) else {
                    bail!("option `--constfn-steps` requires an argument");
                };
                opts.constfn_steps = value.parse().with_context(|| {
                    format!("`--constfn-steps` needs an unsigned integer, got `{value}`")
                })?;
            }
            "--constfn-timeout-ms" => {
                i += 1;
                let Some(value) = args.get(i) else {
                    bail!("option `--constfn-timeout-ms` requires an argument");
                };
                opts.constfn_timeout_ms = value.parse().with_context(|| {
                    format!("`--constfn-timeout-ms` needs an unsigned integer, got `{value}`")
                })?;
            }
            "--constfn-rounds" => {
                i += 1;
                let Some(value) = args.get(i) else {
                    bail!("option `--constfn-rounds` requires an argument");
                };
                opts.constfn_rounds = value.parse().with_context(|| {
                    format!("`--constfn-rounds` needs an unsigned integer, got `{value}`")
                })?;
            }
            "-save-temps" | "--save-temps" => opts.save_temps = true,
            "-E" => opts.stage = Stage::Preprocess,
            "-S" => opts.stage = Stage::Asm,
            "-c" => opts.stage = Stage::Object,
            "-o" => {
                i += 1;
                let Some(v) = args.get(i) else {
                    bail!("option `-o` requires an argument");
                };
                opts.output = Some(PathBuf::from(v));
            }
            "--sym" => {
                i += 1;
                let Some(v) = args.get(i) else {
                    bail!("option `--sym` requires an argument");
                };
                opts.sym = Some(PathBuf::from(v));
            }
            "-Xlinker" => {
                i += 1;
                let Some(v) = args.get(i) else {
                    bail!("option `-Xlinker` requires an argument");
                };
                if v == "--sym" {
                    i += 1;
                    let Some(sym) = args.get(i) else {
                        bail!("linker option `--sym` requires an argument");
                    };
                    opts.sym = Some(PathBuf::from(sym));
                } else {
                    bail!("unsupported linker option for Shy linker: {v}");
                }
            }
            "-I" | "-D" | "-U" | "-include" | "-idirafter" | "-x" | "-MF" | "-MT" | "-MQ" => {
                opts.compile_args.push(arg.clone());
                i += 1;
                let Some(v) = args.get(i) else {
                    bail!("option `{arg}` requires an argument");
                };
                opts.compile_args.push(v.clone());
            }
            _ if arg.starts_with("-o") && arg.len() > 2 => {
                opts.output = Some(PathBuf::from(&arg[2..]));
            }
            _ if arg.starts_with("-l") && arg.len() > 2 => {
                opts.libs.push(arg[2..].to_string());
            }
            _ if arg.starts_with("-Wl,") => {
                for item in arg[4..].split(',') {
                    if let Some(value) = item.strip_prefix("--sym=") {
                        opts.sym = Some(PathBuf::from(value));
                    } else {
                        opts.link_args.push(item.to_string());
                    }
                }
            }
            "--shy-emit-source-lines" => {
                opts.emit_source_lines = true;
                opts.compile_args.push(arg.clone());
            }
            "--shy-emit-debug-info" => {
                opts.emit_debug_info = true;
                opts.compile_args.push(arg.clone());
            }
            // `-O` is a real option of the `shyc` frontend, so a level it does
            // not accept is a usage error instead of a flag to pass along.
            _ if arg.starts_with("-O")
                && !matches!(arg.as_str(), "-O0" | "-O1" | "-O2" | "-O3") =>
            {
                bail!("unsupported optimization level: {arg} (expected -O0, -O1, -O2, or -O3)");
            }
            _ if is_compile_option(arg) => opts.compile_args.push(arg.clone()),
            _ if arg.starts_with('-') => bail!("unknown option: {arg}"),
            _ => opts.inputs.push(arg.clone()),
        }
        i += 1;
    }

    if opts.inputs.is_empty() && opts.libs.is_empty() {
        bail!("no input files");
    }

    if opts.stage != Stage::Link && opts.sym.is_some() {
        bail!("`--sym` is only valid when linking");
    }

    if opts.stage != Stage::Link && opts.output.is_some() && opts.inputs.len() > 1 {
        bail!("cannot specify `-o` with `-E`, `-S` or `-c` and multiple input files");
    }

    if opts.stage != Stage::Link && !opts.libs.is_empty() {
        bail!("`-l...` libraries are only valid when linking");
    }

    if opts.emit_debug_info && !opts.emit_source_lines {
        bail!("--shy-emit-debug-info requires --shy-emit-source-lines");
    }

    // A probe round is a frontend step: assembling, linking, and the text
    // outputs all stand behind it, and it runs for every target the Shy
    // backend can compile a probe unit for.
    let probe_applies = opts.stage != Stage::Preprocess
        && opts
            .emit
            .as_deref()
            .is_none_or(|emit| matches!(emit, "c" | "c-header" | "llvm"))
        && !opts.print_only;
    if opts.probe && !opts.probe_explicit && !probe_applies {
        opts.probe = false;
    }
    if opts.probe {
        if opts.constfn_rounds == 0 {
            bail!("`--constfn-rounds` must be at least 1");
        }
        probe::check_limits(opts.constfn_steps, opts.constfn_timeout_ms)?;
        if opts.stage == Stage::Preprocess {
            bail!("`--constfn-probe` cannot be combined with -E, which parses nothing");
        }
        if let Some(emit) = &opts.emit
            && !matches!(emit.as_str(), "llvm" | "c" | "c-header")
        {
            bail!(
                "`--constfn-probe` only applies to a final link, --emit=llvm, \
                 --emit=c, or --emit=c-header; `--emit={emit}` cannot run probe rounds"
            );
        }
        if opts.print_only {
            bail!("`--constfn-probe` cannot be combined with -###, which only prints commands");
        }
    }
    if opts.probe_only && opts.probe_answers.is_none() {
        bail!("`--probe-only` requires `--probe-answers <path>`");
    }
    if opts.probe_answers.is_some() && !opts.probe_only {
        bail!("`--probe-answers` is only valid with `--probe-only`");
    }
    if opts.probe_only {
        if opts.inputs.len() != 1 {
            bail!("`--probe-only` takes exactly one input file");
        }
        if opts.emit.is_some() {
            bail!("`--probe-only` cannot be combined with --emit");
        }
    }

    Ok(opts)
}

/// The user-mode Shy target: the ShyISA pipeline with the shyemu-user runtime.
const SHY_USER: &str = "shy-user";

/// The target name `shyc` knows. The user-mode target is still ShyISA code.
fn shyc_target(target: &str) -> &str {
    if target == SHY_USER { "shy" } else { target }
}

fn clang_command() -> String {
    env::var("SHYCC_CLANG").unwrap_or_else(|_| "clang".to_string())
}

/// The triple clang spells for `target`, or the host triple when `target` is
/// `None`.
///
/// A module states its own triple and clang compares it with the command line,
/// so IR for `riscv64-linux-gnu` has to carry `riscv64-unknown-linux-gnu`; the
/// driver asks the same clang that will compile it, which also means
/// `SHYCC_CLANG` is honored.
fn clang_triple(target: Option<&str>) -> Result<String> {
    let clang = clang_command();
    let mut cmd = Command::new(&clang);
    if let Some(target) = target {
        cmd.arg(format!("--target={target}"));
    }
    let output = cmd
        .arg("-dumpmachine")
        .output()
        .with_context(|| format!("failed to run `{clang} -dumpmachine` to resolve the target"))?;
    if !output.status.success() {
        bail!("`{clang} -dumpmachine` failed");
    }
    let triple = String::from_utf8(output.stdout)
        .context("`-dumpmachine` printed invalid UTF-8")?
        .trim()
        .to_string();
    if triple.is_empty() {
        bail!("`{clang} -dumpmachine` printed no target triple");
    }
    Ok(triple)
}

/// Expand a short target name to the triple it means.
///
/// `rv64`, `rv64-linux` and `riscv64-linux` are the RISC-V Linux target, and
/// `rv64-bare` is the freestanding one; `rv32`, `rv32-linux` and `riscv32-linux`
/// mean the same for RISC-V ILP32, whose `rv32-bare` form is the supported
/// freestanding target. A full triple such as `riscv64-unknown-linux-gnu`
/// passes through unchanged, as do `shy` and `host`, which the driver resolves
/// itself.
fn normalize_target(name: &str) -> String {
    let (base, freestanding) = match name.rsplit_once('-') {
        Some((base, "bare")) => (base, true),
        Some((base, "linux")) => (base, false),
        _ => (name, false),
    };
    let Some(arch) = arch_triple(base) else {
        return name.to_string();
    };
    if freestanding {
        format!("{arch}-unknown-none")
    } else {
        format!("{arch}-linux-gnu")
    }
}

/// The triple for a short architecture name, or `None` when `name` is already
/// a triple.
fn arch_triple(name: &str) -> Option<&'static str> {
    match name {
        "rv64" | "riscv64" => Some("riscv64"),
        "rv32" | "riscv32" => Some("riscv32"),
        "x64" | "x86_64" => Some("x86_64"),
        "arm64" | "aarch64" => Some("aarch64"),
        "la64" | "loong64" | "loongarch64" => Some("loongarch64"),
        _ => None,
    }
}

fn target_arch(triple: &str) -> &str {
    triple.split('-').next().unwrap_or(triple)
}

fn is_compile_option(arg: &str) -> bool {
    arg.starts_with("-I")
        || arg.starts_with("-D")
        || arg.starts_with("-U")
        || arg.starts_with("-O")
        || arg.starts_with("-W")
        || arg.starts_with("-g")
        || arg.starts_with("-std=")
        || matches!(
            arg,
            "-M" | "-MD"
                | "-MMD"
                | "-MP"
                | "-fcommon"
                | "-fno-common"
                | "-ffreestanding"
                | "-fno-builtin"
                | "-fno-omit-frame-pointer"
                | "-fno-stack-protector"
                | "-fno-strict-aliasing"
                | "-w"
        )
}

/// Options of `compile_args` that the `shyc` frontend accepts verbatim.
///
/// The rest are C compiler flags with no `shyc` equivalent. Only `-O0`..`-O3`
/// reach this point as `-O` spellings, because the parse loop rejects every
/// other level.
fn forwarded_to_shyc(arg: &str) -> bool {
    arg.starts_with("-I") || arg.starts_with("-D") || arg.starts_with("-U") || arg.starts_with("-O")
}

fn run(mut opts: Options) -> Result<()> {
    // Installed before the first compile step: every artifact key below reads
    // the cache this one call chose.
    Cache::install(Cache::from_env(opts.no_cache));
    if opts.probe {
        // The runner is compiled in only with the `constfn-probe` feature; a
        // default build refuses the flag before anything runs.
        probe::ensure_runner()?;
    }
    let repo = repo_root();
    opts.target = normalize_target(&opts.target);
    if opts.target == "host" {
        opts.target = clang_triple(None)?;
    } else if !opts.target_given && opts.emit.as_deref() == Some("c") {
        // A C source output is built by the host C compiler, so it takes the
        // host data model when the user did not name a target: a 32-bit
        // `size_t` would disagree with the C library's own declarations.
        opts.target = clang_triple(None)
            .unwrap_or_else(|_| format!("{}-unknown-linux-gnu", env::consts::ARCH));
    } else if opts.target != "shy" && opts.target != SHY_USER {
        // Keep the spelling the user gave when clang is not available; the
        // compile step reports that failure itself.
        if let Ok(triple) = clang_triple(Some(&opts.target)) {
            opts.target = triple;
        }
    }
    if opts.probe_only {
        // The editor only needs the answers, not an assembly file or an image.
        let mut temps = TempFiles::new(false);
        let mut libs: Option<ProbeLibs> = None;
        let asm = temps.temp_path(".probe.shy");
        run_probe_unit(&repo, &opts, &opts.inputs[0], &asm, &mut temps, &mut libs)?;
        return Ok(());
    }
    if let Some(emit) = opts.emit.clone() {
        // Probe rounds keep one set of temporary files and one copy of the
        // default libraries for every input of this run.
        let mut probe_temps = TempFiles::new(false);
        let mut probe_libs: Option<ProbeLibs> = None;
        for input in &opts.inputs {
            ensure_kind(input, InputKind::C)?;
            if opts.probe {
                run_emit_probe(
                    &repo,
                    &opts,
                    input,
                    opts.output.as_deref(),
                    &emit,
                    &mut probe_temps,
                    &mut probe_libs,
                )?;
            } else {
                run_emit(&repo, &opts, input, opts.output.as_deref(), &emit, None)?;
            }
        }
        return Ok(());
    }
    if opts.target != "shy" && opts.target != SHY_USER {
        return run_llvm(&repo, &opts);
    }
    if let Some(arg) = opts.link_args.first() {
        bail!("unsupported linker option for Shy linker: {arg}");
    }
    let mut temps = TempFiles::new(opts.save_temps || opts.print_only);
    // Probe images and round files are never part of the build's output, so
    // they go away even under `-save-temps`.
    let mut probe_temps = TempFiles::new(false);
    // Default libraries of the probe image, built on the first round and reused
    // by the final link.
    let mut probe_libs: Option<ProbeLibs> = None;

    if opts.stage == Stage::Preprocess {
        for input in &opts.inputs {
            ensure_kind(input, InputKind::C)?;
            let output = opts.output.clone();
            run_compiler(&repo, &opts, input, output.as_deref(), true, false)?;
        }
        return Ok(());
    }

    let mut objects = Vec::new();
    let mut debug_inputs: Vec<(PathBuf, PathBuf)> = Vec::new();
    for input in &opts.inputs {
        match input_kind(input)? {
            InputKind::C => {
                let asm = if opts.stage == Stage::Asm {
                    opts.output
                        .clone()
                        .unwrap_or_else(|| replace_ext(input, ".shy"))
                } else if opts.save_temps {
                    replace_ext(input, ".shy")
                } else {
                    temps.temp_path(".shy")
                };
                if opts.probe {
                    run_probe_unit(&repo, &opts, input, &asm, &mut probe_temps, &mut probe_libs)?;
                } else {
                    run_compiler(&repo, &opts, input, Some(&asm), false, false)?;
                }
                collect_debug_input(&opts, &mut temps, &mut debug_inputs, &asm)?;
                if opts.stage == Stage::Asm {
                    continue;
                }

                let obj = if opts.stage == Stage::Object {
                    opts.output
                        .clone()
                        .unwrap_or_else(|| replace_ext(input, ".sobj"))
                } else if opts.save_temps {
                    replace_ext(input, ".sobj")
                } else {
                    temps.temp_path(".sobj")
                };
                run_asm(&repo, &opts, &asm, &obj)?;
                objects.push(obj);
            }
            InputKind::Asm => {
                if opts.stage == Stage::Asm {
                    let output = opts
                        .output
                        .clone()
                        .unwrap_or_else(|| replace_ext(input, ".shy"));
                    if Path::new(input) != output {
                        copy_file(input, &output)?;
                    }
                    continue;
                }
                collect_debug_input(&opts, &mut temps, &mut debug_inputs, Path::new(input))?;

                let obj = if opts.stage == Stage::Object {
                    opts.output
                        .clone()
                        .unwrap_or_else(|| replace_ext(input, ".sobj"))
                } else if opts.save_temps {
                    replace_ext(input, ".sobj")
                } else {
                    temps.temp_path(".sobj")
                };
                run_asm(&repo, &opts, Path::new(input), &obj)?;
                objects.push(obj);
            }
            InputKind::Obj => objects.push(PathBuf::from(input)),
        }
    }

    if opts.stage == Stage::Asm {
        return Ok(());
    }

    if opts.stage == Stage::Object {
        return Ok(());
    }

    // libshy and the soft-float runtime are the target's C library and are
    // linked by default, the way a hosted compiler links libc. `-nostdlib`
    // and `-nodefaultlibs` turn that off, and an explicit `-l` is not
    // duplicated.
    let mut libraries: Vec<String> = opts.libs.clone();
    if !opts.no_default_libs {
        for default in ["libshy", "float"] {
            if !libraries.iter().any(|lib| lib == default) {
                libraries.push(default.to_string());
            }
        }
    }

    for lib in &libraries {
        match lib.as_str() {
            // A user-mode program links its own platform layer, so the
            // bare probe objects only stand in for the bare target.
            "libshy" => match &probe_libs {
                Some(built) if opts.target != SHY_USER => {
                    objects.extend(built.libshy.iter().cloned())
                }
                _ => objects.extend(build_libshy(&repo, &opts, &mut temps, false)?),
            },
            "float" => match &probe_libs {
                Some(built) => objects.push(built.float.clone()),
                None => objects.push(build_float_lib(&repo, &opts, &mut temps)?),
            },
            _ => bail!("unknown internal Shy library: -l{lib}"),
        }
    }

    let output = opts
        .output
        .clone()
        .unwrap_or_else(|| PathBuf::from("a.sfs"));
    let temp_sym;
    let sym = if opts.emit_debug_info {
        temp_sym = opts.sym.clone().unwrap_or_else(|| temps.temp_path(".sym"));
        Some(temp_sym.as_path())
    } else {
        opts.sym.as_deref()
    };
    run_linker(&repo, &opts, &objects, &output, sym)?;
    if opts.emit_debug_info && !debug_inputs.is_empty() {
        let sym = sym.expect("debug info path above always creates a sym file");
        let debug_elf = debug_elf_path(&output);
        run_shy_dwarf(&repo, &opts, &debug_inputs, sym, &debug_elf)?;
    }
    Ok(())
}

fn run_compiler(
    repo: &Path,
    opts: &Options,
    input: &str,
    output: Option<&Path>,
    preprocess_only: bool,
    libshy_includes: bool,
) -> Result<()> {
    run_shyc(
        repo,
        opts,
        input,
        output,
        preprocess_only,
        None,
        libshy_includes,
    )
}

/// Add the libshy layers in lookup order. Internal library units use these
/// headers even when the active target is hosted: they define the library's
/// own C ABI and must not pick up the host compiler's headers.
fn add_libshy_includes(cmd: &mut Command) {
    for layer in shyc::config::shylib_includes(&shylib()) {
        cmd.arg(format!("-I{}", layer.display()));
    }
}

/// The project's C library: `~/.shycc.toml` names it, and a checkout keeps it
/// next to the crates.
fn shylib() -> PathBuf {
    shyc::config::shylib().unwrap_or_else(|| repo_root().join("libshy"))
}

/// Drive the independent compiler. The runtime is linked as a normal object
/// and source lines are not emitted yet.
/// Files one probe round exchanges with `shyc`.
struct ProbeArgs<'a> {
    manifest: &'a Path,
    answers: &'a Path,
    /// Where a round with requests writes its probe unit. `None` keeps the
    /// assembly as the main output, which is what `-S` expects.
    object: Option<&'a Path>,
}

fn run_shyc(
    repo: &Path,
    opts: &Options,
    input: &str,
    output: Option<&Path>,
    preprocess_only: bool,
    probe: Option<&ProbeArgs>,
    libshy_includes: bool,
) -> Result<()> {
    let mut cmd = shyc_command(repo, opts.print_only)?;
    cmd.arg(format!("--target={}", shyc_target(&opts.target)));
    if opts.target == SHY_USER {
        // The user-mode runtime supplies _start, so the program itself has no
        // automatic startup.
        cmd.arg("-ffreestanding");
    }
    if preprocess_only {
        cmd.arg("-E");
    } else {
        cmd.arg("-S");
    }
    if libshy_includes {
        add_libshy_includes(&mut cmd);
    }
    for arg in &opts.compile_args {
        if forwarded_to_shyc(arg) {
            cmd.arg(arg);
        }
    }
    if let Some(probe) = probe {
        cmd.arg("--constfn-probe")
            .arg("--probe-manifest")
            .arg(probe.manifest)
            .arg("--probe-answers")
            .arg(probe.answers);
        if let Some(object) = probe.object {
            cmd.arg("--probe-object").arg(object);
        }
    }
    if let Some(output) = output {
        cmd.arg("-o").arg(output);
    }
    cmd.arg(input);
    run_command(opts, &mut cmd)
}

/// Drive one input unit through answer rounds until `shyc` asks for nothing.
///
/// A non-empty manifest means the assembly just written is a probe unit: it is
/// assembled and linked with the default libshy and float objects (always, even
/// under `-nostdlib`; built once and shared with the final link), run in the
/// emulator, and its RESULT frames and committed token stream become the next
/// answers. The probe unit brings its own `_start` trampoline and the driver
/// never looks for a `main`; the linker's entry check is the only requirement.
/// Only the round with an empty manifest leaves user assembly behind, and that
/// is what `output` receives.
#[cfg(feature = "constfn-probe")]
fn run_probe_unit(
    repo: &Path,
    opts: &Options,
    input: &str,
    output: &Path,
    temps: &mut TempFiles,
    libs: &mut Option<ProbeLibs>,
) -> Result<()> {
    let answers = match &opts.probe_answers {
        Some(path) => path.clone(),
        None => temps.temp_path(".probe.answers"),
    };
    // A user-named answers file is an output, not a cache: moving it into the
    // cache would delete the file the caller asked for.
    let mut state = ProbeAnswers::open(opts, input, &answers)?;
    let mut answered: Vec<probe::Answer> = Vec::new();
    for _ in 0..opts.constfn_rounds {
        let manifest = temps.temp_path(".probe.manifest");
        let asm = temps.temp_path(".probe.shy");
        run_shyc(
            repo,
            opts,
            input,
            Some(&asm),
            false,
            Some(&ProbeArgs {
                manifest: &manifest,
                answers: &answers,
                object: None,
            }),
            false,
        )?;

        let requests = probe::read_manifest(&manifest)?;
        if requests.is_empty() {
            fs::copy(&asm, output).with_context(|| {
                format!(
                    "failed to write the assembly of `{input}` to {}",
                    output.display()
                )
            })?;
            state.settled(&answers);
            return Ok(());
        }

        if state.discard(&answers)? {
            continue;
        }

        run_probe_image(
            repo,
            opts,
            &asm,
            &requests,
            &answers,
            &mut answered,
            temps,
            libs,
        )?;
    }
    bail!(
        "const-fn probe did not settle after {} rounds for `{input}`",
        opts.constfn_rounds
    )
}

/// The data model a probe image is lowered with.
///
/// The image is ShyISA code, but its object layout has to be the program's:
/// a const fn result -- `sizeof`, an offset, a `long` width -- is used in the
/// program, so it must be computed for the target the program is built for.
fn probe_layout(target: &str) -> Result<shyc::ir::Layout> {
    shyc::ir::Layout::for_arch(target_arch(shyc_target(target)))
        .with_context(|| format!("no data model for probe target `{target}`"))
}

/// Assemble one probe unit, link it into a temporary image, run it, and write
/// the answers the next round reads.
///
/// The probe image is never the user's artifact: it links the default libshy
/// and float objects, which are built once and reused, and it goes away with
/// the temporary files.
#[cfg(feature = "constfn-probe")]
fn run_probe_image(
    repo: &Path,
    opts: &Options,
    asm: &Path,
    requests: &[probe::Request],
    answers: &Path,
    answered: &mut Vec<probe::Answer>,
    temps: &mut TempFiles,
    libs: &mut Option<ProbeLibs>,
) -> Result<()> {
    let obj = temps.temp_path(".probe.sobj");
    run_asm(repo, opts, asm, &obj)?;
    if libs.is_none() {
        *libs = Some(ProbeLibs {
            libshy: build_libshy(repo, opts, temps, true)?,
            float: build_float_lib(repo, opts, temps)?,
        });
    }
    let built = libs.as_ref().expect("probe libraries built above");
    let mut objects = vec![obj];
    objects.extend(built.libshy.iter().cloned());
    objects.push(built.float.clone());
    let image = temps.temp_path(".probe.sfs");
    run_linker(repo, opts, &objects, &image, None)?;
    let bytes = fs::read(&image)
        .with_context(|| format!("failed to read probe image {}", image.display()))?;
    // Answers accumulate: a later round may ask for something a generator only
    // produced after an earlier answer, so a round that dropped the previous
    // answers would ask for them again forever.
    let options = probe::options(opts.constfn_steps, opts.constfn_timeout_ms);
    answered.extend(probe::run_answers(
        &bytes,
        requests,
        options,
        probe_layout(&opts.target)?,
    )?);
    probe::write_answers(answers, answered)
}

/// Without the runner there is nothing to drive; the flag was refused earlier.
#[cfg(not(feature = "constfn-probe"))]
fn run_probe_unit(
    _repo: &Path,
    _opts: &Options,
    _input: &str,
    _output: &Path,
    _temps: &mut TempFiles,
    _libs: &mut Option<ProbeLibs>,
) -> Result<()> {
    probe::ensure_runner()
}

/// Print one of the compiler's text outputs instead of compiling.
///
/// The compiler owns the search path of its own standard library, so this adds
/// no include directory of its own: an installed toolchain finds it through its
/// configuration, exactly like a direct shyc run.
fn run_emit(
    repo: &Path,
    opts: &Options,
    input: &str,
    output: Option<&Path>,
    emit: &str,
    probe: Option<&ProbeArgs>,
) -> Result<()> {
    let mut cmd = shyc_command(repo, opts.print_only)?;
    cmd.arg(format!("--emit={emit}"));
    // Every C output states its data model: the compiler defaults `--emit=c`
    // to the host's, so an explicit `--target` has to reach it, including the
    // Shy one the probe object was built for.
    if emit == "llvm" || emit == "c" || (opts.target != "shy" && opts.target != SHY_USER) {
        cmd.arg(format!("--target={}", shyc_target(&opts.target)));
    }
    for arg in &opts.compile_args {
        if forwarded_to_shyc(arg) {
            cmd.arg(arg);
        }
    }
    if let Some(probe) = probe {
        cmd.arg("--constfn-probe")
            .arg("--probe-manifest")
            .arg(probe.manifest)
            .arg("--probe-answers")
            .arg(probe.answers);
        if let Some(object) = probe.object {
            cmd.arg("--probe-object").arg(object);
        }
    }
    if let Some(output) = output {
        cmd.arg("-o").arg(output);
    }
    cmd.arg(input);
    run_command(opts, &mut cmd)
}

/// Drive one input through probe rounds for a C text output.
///
/// A round with requests writes its probe unit to a temporary file and no C
/// text: the unit is assembled, linked with the cached probe libraries, and run
/// in the emulator, and its answers go back into the next round. The first
/// round with an empty manifest wrote the requested C text to the output, so
/// that text is final and nothing else runs.
#[cfg(feature = "constfn-probe")]
fn run_emit_probe(
    repo: &Path,
    opts: &Options,
    input: &str,
    output: Option<&Path>,
    emit: &str,
    temps: &mut TempFiles,
    libs: &mut Option<ProbeLibs>,
) -> Result<()> {
    let answers = temps.temp_path(".probe.answers");
    let mut state = ProbeAnswers::open(opts, input, &answers)?;
    let mut answered: Vec<probe::Answer> = Vec::new();
    for _ in 0..opts.constfn_rounds {
        let manifest = temps.temp_path(".probe.manifest");
        let object = temps.temp_path(".probe.shy");
        run_emit(
            repo,
            opts,
            input,
            output,
            emit,
            Some(&ProbeArgs {
                manifest: &manifest,
                answers: &answers,
                object: Some(&object),
            }),
        )?;
        let requests = probe::read_manifest(&manifest)?;
        if requests.is_empty() {
            state.settled(&answers);
            return Ok(());
        }
        if state.discard(&answers)? {
            continue;
        }
        run_probe_image(
            repo,
            opts,
            &object,
            &requests,
            &answers,
            &mut answered,
            temps,
            libs,
        )?;
    }
    bail!(
        "const-fn probe did not settle after {} rounds for `{input}`",
        opts.constfn_rounds
    )
}

/// Without the runner there is nothing to drive; the flag was refused earlier.
#[cfg(not(feature = "constfn-probe"))]
fn run_emit_probe(
    _repo: &Path,
    _opts: &Options,
    _input: &str,
    _output: Option<&Path>,
    _emit: &str,
    _temps: &mut TempFiles,
    _libs: &mut Option<ProbeLibs>,
) -> Result<()> {
    probe::ensure_runner()
}

fn shyc_command(repo: &Path, print_only: bool) -> Result<Command> {
    if let Ok(path) = env::var("SHYCC_SHYC") {
        return Ok(Command::new(path));
    }
    if let Ok(current) = env::current_exe()
        && let Some(dir) = current.parent()
    {
        let sibling = dir.join("shyc");
        if sibling.exists() {
            return Ok(Command::new(sibling));
        }
    }
    let built = repo.join("target/release/shyc");
    if built.exists() || print_only {
        return Ok(Command::new(built));
    }
    let mut cmd = Command::new(env::var("CARGO").unwrap_or_else(|_| "cargo".into()));
    cmd.arg("run")
        .arg("-q")
        .arg("--manifest-path")
        .arg(repo.join("Cargo.toml"))
        .arg("-p")
        .arg("shyc")
        .arg("--");
    Ok(cmd)
}

/// GCC/clang-style freestanding target detection. `unknown-none`, bare
/// `none`, and ELF firmware triples do not get a hosted libc or CRT.
fn bare_target(triple: &str) -> bool {
    let lower = triple.to_ascii_lowercase();
    lower.contains("none") || lower.contains("unknown-elf") || lower.ends_with("-elf")
}

/// The C library root for a hosted link. `--sysroot` wins; otherwise a cross
/// toolchain installed at `/usr/<triple>` (the Arch and Debian layout) is used
/// when it exists.
fn sysroot(opts: &Options) -> Option<PathBuf> {
    if let Some(sysroot) = &opts.sysroot {
        return Some(sysroot.clone());
    }
    if bare_target(&opts.target) || target_arch(&opts.target) == env::consts::ARCH {
        return None;
    }
    let mut candidates = vec![Path::new("/usr").join(&opts.target)];
    let parts: Vec<&str> = opts.target.split('-').collect();
    if parts.len() >= 4 {
        // Cross packages install at /usr/<arch>-<os>-<env>, without the vendor
        // clang adds to the triple.
        candidates.push(Path::new("/usr").join(format!("{}-{}", parts[0], parts[2..].join("-"))));
    }
    candidates.into_iter().find(|path| path.is_dir())
}

/// Lower to LLVM IR with `shyc`, then let clang do instruction selection and
/// linking. This is the same shape as the Shy pipeline, which hands its
/// assembly to `shyasm` and the objects to `shyld`.
fn run_llvm(repo: &Path, opts: &Options) -> Result<()> {
    // There is no riscv32 Linux sysroot to link against, so report that
    // before clang or lld can emit their own loader errors.
    if !bare_target(&opts.target) && target_arch(&opts.target) == "riscv32" {
        bail!(
            "no riscv32 Linux sysroot found; \
             use `--target=rv32-bare` for a freestanding build"
        );
    }
    // Preprocessing does not depend on the target: `shyc -E` owns it for every
    // backend, and there is nothing for clang to do yet.
    if opts.stage == Stage::Preprocess {
        for input in &opts.inputs {
            ensure_kind(input, InputKind::C)?;
            let output = opts.output.clone();
            run_compiler(repo, opts, input, output.as_deref(), true, false)?;
        }
        return Ok(());
    }

    let mut temps = TempFiles::new(opts.save_temps || opts.print_only);
    // Probe rounds keep their own temporary files and their own copy of the
    // default libraries; both go away with the build.
    let mut probe_temps = TempFiles::new(false);
    let mut probe_libs: Option<ProbeLibs> = None;
    let mut clang_inputs: Vec<PathBuf> = Vec::new();

    for input in &opts.inputs {
        match input_kind(input)? {
            InputKind::C => {
                // The IR is an intermediate file: `-o` names what clang makes
                // from it, so `-S -o x.s` holds assembly, not IR.
                let ll = if opts.stage == Stage::Link || opts.output.is_some() {
                    temps.temp_path(".ll")
                } else {
                    replace_ext(input, ".ll")
                };
                if opts.probe {
                    // A round with requests builds the program's probe image
                    // instead of the IR, so the final round is what lands here.
                    run_emit_probe(
                        repo,
                        opts,
                        input,
                        Some(&ll),
                        "llvm",
                        &mut probe_temps,
                        &mut probe_libs,
                    )?;
                } else {
                    emit_llvm(repo, opts, Path::new(input), &ll, true)?;
                }
                clang_inputs.push(ll);
            }
            // `.S` files are target assembly text: clang assembles and preprocesses
            // them, so they pass straight through.
            InputKind::Asm | InputKind::Obj => clang_inputs.push(PathBuf::from(input)),
        }
    }
    // A hosted target takes the C layer from the system libc and links
    // libshy's core, alloc, and std on top of it, as Rust links its std.
    if opts.stage == Stage::Link && !bare_target(&opts.target) && !opts.no_default_libs {
        for source in LIBSHY_RUST_LAYERS {
            let path = shylib().join(source);
            let key = caches_artifacts(opts)
                .then(|| libshy_ll_key(opts, source))
                .flatten();
            if let Some(hit) = key.as_deref().and_then(|key| cache().lookup(key, "ll")) {
                clang_inputs.push(hit);
                continue;
            }
            let ll = temps.temp_path(".ll");
            emit_llvm(repo, opts, &path, &ll, false)?;
            clang_inputs.push(match key {
                Some(key) => cache().store(&key, "ll", &ll),
                None => ll,
            });
        }
    }

    let mut cmd = Command::new(clang_command());
    cmd.arg(format!("--target={}", opts.target));
    if opts.target.starts_with("riscv64") {
        // The kernel links above 0x8000_0000, out of reach of the default
        // medlow addressing range.
        cmd.arg("-mcmodel=medany");
    }
    if let Some(sysroot) = sysroot(opts) {
        cmd.arg(format!("--sysroot={}", sysroot.display()));
    }
    let bare = bare_target(&opts.target);
    if bare {
        cmd.args(["-ffreestanding", "-nostdlib", "-nodefaultlibs", "-static"]);
        if opts.stage == Stage::Link {
            // `-nostartfiles` only means something while linking, and clang
            // warns about it during `-S`/`-c`.
            cmd.arg("-nostartfiles");
            cmd.arg("-Wl,-e,_start");
        }
    } else {
        // Match the code model to what the linker will build. clang only
        // defaults to PIE codegen for a triple with a hosted OS; an arch-only
        // target such as `x86_64` would otherwise carry absolute relocations
        // and the host linker would report a DT_TEXTREL warning.
        cmd.arg("-fPIE");
    }
    if opts.stage == Stage::Link && (bare || target_arch(&opts.target) != env::consts::ARCH) {
        // The host `ld` can only emulate its own architecture, so cross and
        // bare-metal links go through LLVM's linker.
        cmd.arg("-fuse-ld=lld");
    }
    match opts.stage {
        Stage::Asm => {
            cmd.arg("-S");
            if let Some(output) = &opts.output {
                cmd.arg("-o").arg(output);
            }
            cmd.args(&clang_inputs);
        }
        Stage::Object => {
            cmd.arg("-c");
            for (index, input) in clang_inputs.iter().enumerate() {
                let output = match (&opts.output, clang_inputs.len()) {
                    (Some(output), 1) => output.clone(),
                    _ => {
                        let mut path = replace_ext(&input.to_string_lossy(), ".o");
                        if index > 0 {
                            path = PathBuf::from(format!("{index}-{}", path.display()));
                        }
                        path
                    }
                };
                cmd.arg(input).arg("-o").arg(output);
            }
        }
        _ => {
            if let Some(output) = &opts.output {
                cmd.arg("-o").arg(output);
            }
            for lib in &opts.libs {
                cmd.arg(format!("-l{lib}"));
            }
            for arg in &opts.link_args {
                cmd.arg(format!("-Wl,{arg}"));
            }
            cmd.args(&clang_inputs);
        }
    }
    let result = run_command(opts, &mut cmd);
    if result.is_err()
        && opts.stage == Stage::Link
        && !bare
        && target_arch(&opts.target) != env::consts::ARCH
        && sysroot(opts).is_none()
    {
        eprintln!(
            "shycc: no C library for `{}`. Pass `--sysroot <dir>`, or use \
             `--target={}-bare` for a freestanding build.",
            opts.target,
            target_arch(&opts.target)
        );
    }
    result
}

fn run_asm(repo: &Path, opts: &Options, input: &Path, output: &Path) -> Result<()> {
    let mut cmd = tool_command(repo, "asm", "shyasm")?;
    cmd.arg(input).arg("-o").arg(output);
    run_command(opts, &mut cmd)
}

fn run_linker(
    repo: &Path,
    opts: &Options,
    inputs: &[PathBuf],
    output: &Path,
    sym: Option<&Path>,
) -> Result<()> {
    let mut cmd = tool_command(repo, "linker", "shyld")?;
    // libshy is linked as whole objects, not an archive, so drop the functions
    // nothing reaches: a program that only calls `puts` keeps no `printf`.
    cmd.arg("--gc-sections");
    cmd.args(inputs).arg("-o").arg(output);
    if let Some(sym) = sym {
        cmd.arg("--sym").arg(sym);
    }
    run_command(opts, &mut cmd)
}

/// The `shy-dwarf` binary: the explicit override, the copy next to this
/// binary, or the one this workspace already built.
///
/// Like `tool_command`, this never builds anything: one `cargo run` per
/// invocation would resolve the workspace and run a debug-profile tool, which
/// is slower than the tool it produces.
fn shy_dwarf_tool(repo: &Path) -> Result<PathBuf> {
    if let Ok(path) = env::var("SHYCC_SHY_DWARF") {
        return Ok(PathBuf::from(path));
    }
    let sibling = env::current_exe()
        .ok()
        .and_then(|exe| Some(exe.parent()?.join("shy-dwarf")));
    if let Some(path) = sibling.filter(|path| path.exists()) {
        return Ok(path);
    }
    let built = repo.join("target/release/shy-dwarf");
    if built.exists() {
        return Ok(built);
    }
    bail!(
        "cannot find `shy-dwarf`: not next to shycc and not at {}.\n\
         Build it with `make testpath` (or `cargo build --release -p shy-dwarf`),\n\
         or set SHYCC_SHY_DWARF to the tool's path.",
        built.display()
    )
}

fn run_shy_dwarf(
    repo: &Path,
    opts: &Options,
    debug_inputs: &[(PathBuf, PathBuf)],
    sym: &Path,
    output: &Path,
) -> Result<()> {
    let mut cmd = Command::new(shy_dwarf_tool(repo)?);
    for (asm, debug) in debug_inputs {
        cmd.arg("--asm").arg(asm);
        cmd.arg("--debug").arg(debug);
    }
    cmd.arg("--sym").arg(sym).arg("-o").arg(output);
    run_command(opts, &mut cmd)
}

fn collect_debug_input(
    opts: &Options,
    temps: &mut TempFiles,
    debug_inputs: &mut Vec<(PathBuf, PathBuf)>,
    asm: &Path,
) -> Result<()> {
    if !opts.emit_debug_info {
        return Ok(());
    }
    let debug = debug_sidecar_path(asm);
    if !debug.exists() {
        return Ok(());
    }
    if !opts.save_temps {
        temps.paths.push(debug.clone());
    }
    debug_inputs.push((asm.to_path_buf(), debug));
    Ok(())
}

fn build_float_lib(repo: &Path, opts: &Options, temps: &mut TempFiles) -> Result<PathBuf> {
    let src = repo.join("third_party/shy_runtime_softfloat.c");
    if !src.exists() {
        bail!("missing internal float library source: {}", src.display());
    }

    let runtime_c = temps.temp_path(".float.c");
    let body = fs::read_to_string(&src)
        .with_context(|| format!("failed to read internal float library: {}", src.display()))?;
    let key = caches_artifacts(opts)
        .then(|| float_object_key(opts, &src))
        .flatten();
    if let Some(hit) = key.as_deref().and_then(|key| cache().lookup(key, "sobj")) {
        return Ok(hit);
    }
    fs::write(&runtime_c, format!("#![no_main]\n{body}")).with_context(|| {
        format!(
            "failed to write temporary float runtime: {}",
            runtime_c.display()
        )
    })?;

    let asm = if opts.save_temps {
        PathBuf::from("libfloat.shy")
    } else {
        temps.temp_path(".float.shy")
    };
    let obj = if opts.save_temps {
        PathBuf::from("libfloat.sobj")
    } else {
        temps.temp_path(".float.sobj")
    };

    let input = runtime_c.to_string_lossy().into_owned();
    run_compiler(repo, opts, &input, Some(&asm), false, true)?;
    run_asm(repo, opts, &asm, &obj)?;
    Ok(match key {
        Some(key) => cache().store(&key, "sobj", &obj),
        None => obj,
    })
}

/// libshy's layers above the C library, in dependency order.
const LIBSHY_RUST_LAYERS: [&str; 4] = [
    "core/core.shyc",
    "alloc/alloc.shyc",
    "std/std.shyc",
    "std/env.shyc",
];

/// One unit through `shyc --emit=llvm`. A library unit gets only the
/// optimization level, never the user's `-I`/`-D`/`-U`.
fn emit_llvm(repo: &Path, opts: &Options, input: &Path, ll: &Path, user_unit: bool) -> Result<()> {
    let mut cmd = shyc_command(repo, opts.print_only)?;
    cmd.arg("--emit=llvm")
        .arg(format!("--target={}", opts.target));
    if bare_target(&opts.target) {
        cmd.arg("-ffreestanding");
    }
    if !user_unit {
        add_libshy_includes(&mut cmd);
    }
    for arg in &opts.compile_args {
        if forwarded_to_shyc(arg) && (user_unit || arg.starts_with("-O")) {
            cmd.arg(arg);
        }
    }
    cmd.arg("-o").arg(ll).arg(input);
    run_command(opts, &mut cmd)
}

/// Whether this run may reuse artifacts from the cache.
///
/// `-save-temps` names the intermediate files itself and `-###` only prints the
/// commands, so neither run is the one an artifact on disk describes.
fn caches_artifacts(opts: &Options) -> bool {
    !opts.save_temps && !opts.print_only && cache().root().is_some()
}

/// A token that changes whenever the compiler that produces the artifacts does.
///
/// `None` when the driver would fall back to `cargo run`: its output follows the
/// source tree, which no key can describe without hashing the whole workspace.
fn shyc_identity(print_only: bool) -> Option<String> {
    let program = PathBuf::from(shyc_command(&repo_root(), print_only).ok()?.get_program());
    let metadata = program.metadata().ok()?;
    if !metadata.is_file() {
        return None;
    }
    let modified = metadata
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?;
    Some(format!(
        "{}:{}:{}",
        program.display(),
        metadata.len(),
        modified.as_nanos()
    ))
}

/// The answer set one input's probe rounds run on, and the cache entry behind it.
///
/// A stored set is exactly what the rounds would compute again, so feeding it
/// back settles the compile in a single round: `shyc` finds an answer for every
/// `const fn` call and asks for nothing, and no probe image is built.
struct ProbeAnswers {
    /// The cache key, when this run may store and reuse a set.
    key: Option<String>,
    /// Whether a stored set was fed back and has not been disproved yet.
    reused: bool,
}

impl ProbeAnswers {
    /// Start from the stored set when there is one, and from no answers
    /// otherwise.
    fn open(opts: &Options, input: &str, path: &Path) -> Result<ProbeAnswers> {
        // A user-named answers file is an output: storing it would move the
        // file the caller asked to be left in place.
        let key = (opts.probe_answers.is_none() && caches_artifacts(opts))
            .then(|| probe_answers_key(opts, input))
            .flatten();
        let mut state = ProbeAnswers {
            key,
            reused: false,
        };
        let stored = state
            .key
            .as_deref()
            .and_then(|key| cache().lookup(key, "answers"));
        match stored {
            Some(stored) => {
                fs::copy(&stored, path).with_context(|| {
                    format!("failed to reuse const-fn answers from {}", stored.display())
                })?;
                state.reused = true;
            }
            None => probe::write_answers(path, &[])?,
        }
        Ok(state)
    }

    /// Keep the settled set for the next run.
    fn settled(&self, path: &Path) {
        if let Some(key) = &self.key {
            cache().store(key, "answers", path);
        }
    }

    /// Drop a stored set that did not settle this compile.
    ///
    /// Answers that do not answer every call describe different sources, so they
    /// must not stay: the round is re-run with no answers at all, which is what
    /// the driver does when nothing is stored. Returns whether that happened.
    fn discard(&mut self, path: &Path) -> Result<bool> {
        if !self.reused {
            return Ok(false);
        }
        self.reused = false;
        if let Some(stale) = self
            .key
            .as_deref()
            .and_then(|key| cache().lookup(key, "answers"))
        {
            let _ = fs::remove_file(stale);
        }
        probe::write_answers(path, &[])?;
        Ok(true)
    }
}

/// The key of one input's settled `const fn` answers.
///
/// Everything a `const fn` can read has to be in here: the call's arguments, the
/// types and impls its headers declare, the data model of the target, and the
/// compiler that evaluates it. A missed input would reuse answers for a program
/// that no longer exists, so the include path is hashed rather than trusted.
fn probe_answers_key(opts: &Options, input: &str) -> Option<String> {
    let mut fingerprint = Fingerprint::new();
    fingerprint
        .text("probe-answers")
        .text(&shyc_identity(opts.print_only)?)
        .text(&opts.target)
        .text(shyc_target(&opts.target));
    if opts.target == SHY_USER {
        fingerprint.text("-ffreestanding");
    }
    // A round's budgets decide whether a probe finishes, so a different budget
    // is a different set of answers to remember.
    fingerprint
        .number(opts.constfn_steps)
        .number(opts.constfn_timeout_ms);
    for arg in &opts.compile_args {
        if forwarded_to_shyc(arg) {
            fingerprint.text(arg);
        }
    }
    // Every header the include path can reach, and the directory of the input
    // itself, which is where a quoted include starts looking.
    for arg in &opts.compile_args {
        if let Some(dir) = arg.strip_prefix("-I") {
            let dir = if dir.is_empty() { "." } else { dir };
            cache::hash_tree(&mut fingerprint, Path::new(dir)).ok()?;
        }
    }
    let source = Path::new(input);
    if let Some(dir) = source.parent()
        && !dir.as_os_str().is_empty()
    {
        cache::hash_tree(&mut fingerprint, dir).ok()?;
    }
    cache::hash_file(&mut fingerprint, source).ok()?;
    cache::hash_tree(&mut fingerprint, &shylib()).ok()?;
    Some(format!("probe-{}", fingerprint.key()))
}

/// The key of one `libshy` object for the Shy target.
///
/// Every source and header of the library is in the key, not only the unit's
/// own file: a unit's object changes when any header it can reach changes, and
/// the library is small enough to hash whole.
fn libshy_object_key(opts: &Options, probe: bool, source: &str) -> Option<String> {
    let mut fingerprint = Fingerprint::new();
    fingerprint
        .text("libshy-object")
        .text(&shyc_identity(opts.print_only)?)
        .text(&opts.target)
        .text(if probe { "probe" } else { "program" })
        .text(source);
    // The Shy pipeline hands library units the user's `-I`/`-D`/`-U` as well.
    for arg in &opts.compile_args {
        if forwarded_to_shyc(arg) {
            fingerprint.text(arg);
        }
    }
    cache::hash_tree(&mut fingerprint, &shylib()).ok()?;
    Some(format!("libshy-{}", fingerprint.key()))
}

/// The key of the soft-float runtime's object.
fn float_object_key(opts: &Options, source: &Path) -> Option<String> {
    let mut fingerprint = Fingerprint::new();
    fingerprint
        .text("float-object")
        .text(&shyc_identity(opts.print_only)?)
        .text(&opts.target);
    for arg in &opts.compile_args {
        if forwarded_to_shyc(arg) {
            fingerprint.text(arg);
        }
    }
    // The driver wraps the body in `#![no_main]` before compiling it.
    fingerprint.text("#![no_main]");
    cache::hash_file(&mut fingerprint, source).ok()?;
    cache::hash_tree(&mut fingerprint, &shylib()).ok()?;
    Some(format!("float-{}", fingerprint.key()))
}

/// The key of one `libshy` layer's LLVM IR on a hosted target.
fn libshy_ll_key(opts: &Options, source: &str) -> Option<String> {
    let mut fingerprint = Fingerprint::new();
    fingerprint
        .text("libshy-ll")
        .text(&shyc_identity(opts.print_only)?)
        .text(&opts.target)
        .text(source);
    if bare_target(&opts.target) {
        fingerprint.text("-ffreestanding");
    }
    // A library unit is compiled with the optimization level only.
    for arg in &opts.compile_args {
        if arg.starts_with("-O") {
            fingerprint.text(arg);
        }
    }
    cache::hash_tree(&mut fingerprint, &shylib()).ok()?;
    Some(format!("libshy-ll-{}", fingerprint.key()))
}

/// The Shy target's whole library: the portable C layer, the Shy
/// implementation of its platform calls, then core, alloc, and std. ShyOS
/// links the same sources with its own system calls instead of `sys/shy`.
///
/// A probe image runs in the bare emulator and brings its own `_start`, so
/// it always takes the bare platform, whatever the program targets.
fn build_libshy(
    repo: &Path,
    opts: &Options,
    temps: &mut TempFiles,
    probe: bool,
) -> Result<Vec<PathBuf>> {
    // Bare Shy calls the UART; the user-mode target calls the host through
    // syscall and brings its own _start.
    let user = opts.target == SHY_USER && !probe;
    let platform = if user {
        "sys/user.shyc"
    } else {
        "sys/shy.shyc"
    };
    let mut sources = vec!["libc/libc.c", platform];
    sources.extend(LIBSHY_RUST_LAYERS);
    if user {
        sources.push("sys/user_start.shyc");
    }
    let mut objects = Vec::new();
    for source in sources {
        let name = Path::new(source)
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or("libshy");
        let src = shylib().join(source);
        if !src.exists() {
            bail!(
                "missing libshy source: {} (set shylib in ~/.shycc.toml)",
                src.display()
            );
        }
        let key = caches_artifacts(opts)
            .then(|| libshy_object_key(opts, probe, source))
            .flatten();
        if let Some(hit) = key.as_deref().and_then(|key| cache().lookup(key, "sobj")) {
            objects.push(hit);
            continue;
        }
        let (asm, obj) = if opts.save_temps {
            (
                PathBuf::from(format!("{name}.shy")),
                PathBuf::from(format!("{name}.sobj")),
            )
        } else {
            (
                temps.temp_path(&format!(".{name}.shy")),
                temps.temp_path(&format!(".{name}.sobj")),
            )
        };
        let input = src.to_string_lossy().into_owned();
        run_compiler(repo, opts, &input, Some(&asm), false, true)?;
        run_asm(repo, opts, &asm, &obj)?;
        objects.push(match key {
            Some(key) => cache().store(&key, "sobj", &obj),
            None => obj,
        });
    }
    Ok(objects)
}

/// The assembler or linker the driver runs.
///
/// The order is the explicit `SHYCC_<NAME>`/`SHYCC_<PACKAGE>` override, the
/// tool next to this binary, the first one on `PATH`, and finally an error.
///
/// There is deliberately no `cargo run` fallback: every invocation would
/// resolve the workspace and check fingerprints before running anything, and
/// it would silently run a debug-profile tool. A missing tool is a setup error
/// worth reporting, not something to hide behind a build.
fn tool_command(repo: &Path, package: &str, installed_name: &str) -> Result<Command> {
    if let Ok(path) = env::var(format!("SHYCC_{}", installed_name.to_ascii_uppercase())) {
        return Ok(Command::new(path));
    }
    if let Ok(path) = env::var(format!("SHYCC_{}", package.to_ascii_uppercase())) {
        return Ok(Command::new(path));
    }

    let current = env::current_exe().context("cannot locate the running shycc binary")?;
    let dir = current
        .parent()
        .context("the running shycc binary has no parent directory")?;
    let sibling = dir.join(installed_name);
    if sibling.exists() {
        return Ok(Command::new(sibling));
    }

    // A toolchain installed anywhere on `PATH` is the next answer: the driver
    // itself may have been copied out of the build tree that holds the tools.
    if let Some(tool) = find_on_path(installed_name) {
        return Ok(Command::new(tool));
    }

    bail!(
        "cannot find `{installed_name}`: not next to shycc ({}) and not on PATH.\n\
         Build the toolchain into `build/bin` and run that copy:\n  \
         make testpath\n  {}\n\
         or set SHYCC_{} to the tool's path.",
        dir.display(),
        repo.join("build/bin/shycc").display(),
        installed_name.to_ascii_uppercase()
    );
}

/// The first executable `name` on `PATH`, the way a shell resolves it.
///
/// A bare `Command::new(name)` would search `PATH` too, but only when the
/// command is spawned, which is too late to decide that the tool is missing.
/// An empty `PATH` entry means the current directory to a shell, and a
/// toolchain is never installed there, so it is skipped.
fn find_on_path(name: &str) -> Option<PathBuf> {
    let path = env::var_os("PATH")?;
    env::split_paths(&path).find_map(|dir| {
        if dir.as_os_str().is_empty() {
            return None;
        }
        let tool = dir.join(name);
        is_executable(&tool).then_some(tool)
    })
}

/// Whether `path` names a file this user may execute.
#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata()
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

/// Without POSIX permission bits, being a file is the best test there is.
#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

fn run_command(opts: &Options, cmd: &mut Command) -> Result<()> {
    if opts.print_only {
        eprintln!("{}", command_line(cmd));
        return Ok(());
    }

    let status = cmd
        .status()
        .with_context(|| format!("failed to run `{}`", command_line(cmd)))?;
    ensure_success(status, &command_line(cmd))
}

fn ensure_success(status: ExitStatus, what: &str) -> Result<()> {
    if status.success() {
        Ok(())
    } else {
        bail!("command failed: {what}");
    }
}

fn input_kind(path: &str) -> Result<InputKind> {
    let p = Path::new(path);
    match p.extension().and_then(|s| s.to_str()) {
        Some("shyc") | Some("shyh") | Some("c") | Some("h") => Ok(InputKind::C),
        Some("shy") | Some("S") | Some("s") => Ok(InputKind::Asm),
        // `.sobj` is a Shy object; `.o`/`.obj` are what a hosted target
        // produces, and clang links them back as they are.
        Some("sobj") | Some("o") | Some("obj") => Ok(InputKind::Obj),
        _ => bail!("unknown input file extension: {path}"),
    }
}

fn ensure_kind(path: &str, want: InputKind) -> Result<()> {
    let got = input_kind(path)?;
    if got == want {
        Ok(())
    } else {
        bail!("input kind is not supported for this stage: {path}");
    }
}

fn replace_ext(path: &str, ext: &str) -> PathBuf {
    let mut p = PathBuf::from(path);
    p.set_extension(ext.trim_start_matches('.'));
    p
}

fn debug_sidecar_path(asm: &Path) -> PathBuf {
    PathBuf::from(format!("{}.debug.json", asm.display()))
}

fn debug_elf_path(output: &Path) -> PathBuf {
    let mut p = output.to_path_buf();
    p.set_extension("debug.elf");
    p
}

fn copy_file(input: &str, output: &Path) -> Result<()> {
    fs::copy(input, output).with_context(|| {
        format!(
            "failed to copy assembly input `{input}` to `{}`",
            output.display()
        )
    })?;
    Ok(())
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("shycc crate must live under the workspace root")
        .to_path_buf()
}

fn command_line(cmd: &Command) -> String {
    let mut parts = Vec::new();
    parts.push(shell_word(cmd.get_program()));
    parts.extend(cmd.get_args().map(shell_word));
    parts.join(" ")
}

fn shell_word(s: &std::ffi::OsStr) -> String {
    let text = s.to_string_lossy();
    if text
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '_' | '-' | '=' | ':'))
    {
        text.into_owned()
    } else {
        format!("'{}'", text.replace('\'', "'\\''"))
    }
}

fn print_usage() {
    eprintln!(
        "usage: shycc [options] file...\n\
         stages: -E, -S, -c, or link to a.sfs by default\n\
         outputs: -o <file>, --sym <file>, -save-temps, -###,
         \x20 --emit=c|c-header|c-wrapper|shy|llvm (print text instead of linking)\n\
         debug: --shy-emit-source-lines, --shy-emit-debug-info\n\
         probe: const-fn calls run as probe rounds in the emulator for a shy final\n\
         \x20 link and for --emit=c / --emit=c-header; on by default, --no-constfn-\n\
         \x20 probe turns it off (needs the constfn-probe feature at build time)\n\
         \x20 --constfn-steps <n>, --constfn-timeout-ms <n>, --constfn-rounds <n>\n\
         \x20 set one round's instruction and wall-clock budgets and the round cap\n\
         \x20 (default 32000000, 2000, 256)\n\
         \x20 --probe-only <file> --probe-answers <file> runs just those rounds\n\
         \x20 and writes the answers instead of linking\n\
         inputs: .shyc/.c, .shy, .sobj\n\
         target: --target <name> (default shy); shy-user runs the ShyISA pipeline on\n\
         \x20 the shyemu-user runtime with host syscalls; rv64/riscv64, rv32/riscv32,\n\
         \x20 x64/x86_64, arm64/aarch64 and la64/loong64/loongarch64 name Linux targets\n\
         \x20 (rv32 needs a sysroot), a `-bare` suffix makes them freestanding, `host`\n\
         \x20 is the local clang triple, and a full triple is handed to clang as written\n\
         sysroot: --sysroot <dir> for a cross link (default /usr/<triple>)\n\
         optimization: -O0, -O1, -O2, or -O3 (forwarded to shyc; -O0 by default)\n\
         libraries: libshy and the soft-float runtime are linked by default;\n\
         \x20 -nostdlib/-nodefaultlibs disables that, -llibshy and -lfloat\n\
         \x20 are accepted and never duplicated"
    );
}
