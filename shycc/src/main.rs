use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{Context, Result, bail};

static TMP_COUNTER: AtomicUsize = AtomicUsize::new(0);

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Backend {
    /// The local chibicc fork, kept for early bootstrap and compatibility.
    Chibicc,
    /// The independent `shyc` compiler, which is the default.
    Shyc,
}

struct Options {
    stage: Stage,
    backend: Backend,
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
    emit_debug_info: bool,
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

fn main() -> Result<()> {
    let opts = parse_args(env::args().skip(1).collect())?;
    run(opts)
}

fn parse_args(args: Vec<String>) -> Result<Options> {
    let mut opts = Options {
        stage: Stage::Link,
        backend: match env::var("SHYCC_BACKEND").as_deref() {
            Ok("chibicc") => Backend::Chibicc,
            Ok("shyc") | Err(_) => Backend::Shyc,
            Ok(other) => bail!("unknown SHYCC_BACKEND: {other}"),
        },
        output: None,
        sym: None,
        save_temps: false,
        print_only: false,
        emit_source_lines: false,
        no_default_libs: false,
        target: "shy".to_string(),
        sysroot: None,
        emit: None,
        emit_debug_info: false,
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
            "--backend" => {
                i += 1;
                let Some(value) = args.get(i) else {
                    bail!("option `--backend` requires an argument");
                };
                opts.backend = parse_backend(value)?;
            }
            flag if flag.starts_with("--backend=") => {
                opts.backend = parse_backend(&flag["--backend=".len()..])?;
            }
            "-nostdlib" | "-nodefaultlibs" | "-nolibc" => opts.no_default_libs = true,
            "--target" => {
                i += 1;
                let Some(value) = args.get(i) else {
                    bail!("option `--target` requires an argument");
                };
                opts.target = value.clone();
            }
            flag if flag.starts_with("--target=") => {
                opts.target = flag["--target=".len()..].to_string();
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

    Ok(opts)
}

fn parse_backend(name: &str) -> Result<Backend> {
    match name {
        "chibicc" => Ok(Backend::Chibicc),
        "shyc" => Ok(Backend::Shyc),
        other => bail!("unknown backend: {other}"),
    }
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
/// The rest belong to chibicc. Only `-O0`..`-O3` reach this point as `-O`
/// spellings, because the parse loop rejects every other level.
fn forwarded_to_shyc(arg: &str) -> bool {
    arg.starts_with("-I") || arg.starts_with("-D") || arg.starts_with("-U") || arg.starts_with("-O")
}

fn run(mut opts: Options) -> Result<()> {
    let repo = repo_root();
    opts.target = normalize_target(&opts.target);
    if opts.target == "host" {
        opts.target = clang_triple(None)?;
    } else if opts.target != "shy" {
        // Keep the spelling the user gave when clang is not available; the
        // compile step reports that failure itself.
        if let Ok(triple) = clang_triple(Some(&opts.target)) {
            opts.target = triple;
        }
    }
    if let Some(emit) = opts.emit.clone() {
        for input in &opts.inputs {
            ensure_kind(input, InputKind::C)?;
            run_emit(&repo, &opts, input, opts.output.as_deref(), &emit)?;
        }
        return Ok(());
    }
    if opts.target != "shy" {
        return run_llvm(&repo, &opts);
    }
    if let Some(arg) = opts.link_args.first() {
        bail!("unsupported linker option for Shy linker: {arg}");
    }
    let mut temps = TempFiles::new(opts.save_temps || opts.print_only);

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
                run_compiler(&repo, &opts, input, Some(&asm), false, false)?;
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
            "libshy" => objects.extend(build_libshy(&repo, &opts, &mut temps)?),
            "float" => objects.push(build_float_lib(&repo, &opts, &mut temps)?),
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
    link_runtime: bool,
) -> Result<()> {
    match opts.backend {
        Backend::Chibicc => run_chibicc(repo, opts, input, output, preprocess_only, link_runtime),
        Backend::Shyc => run_shyc(repo, opts, input, output, preprocess_only),
    }
}

/// Add the libshy layers in lookup order: C headers first, then explicit
/// `c/`/`core/` paths, then compatibility wrappers.
fn add_libshy_includes(cmd: &mut Command, _repo: &Path) {
    for layer in shyc::config::shylib_includes(&shylib()) {
        cmd.arg(format!("-I{}", layer.display()));
    }
}

/// The project's C library: `~/.shycc.toml` names it, and a checkout keeps it
/// next to the crates.
fn shylib() -> PathBuf {
    shyc::config::shylib().unwrap_or_else(|| repo_root().join("libshy"))
}

/// Drive the independent compiler. It has no equivalent of chibicc's extra
/// flags: the runtime is linked as a normal object and source lines are not
/// emitted yet.
fn run_shyc(
    repo: &Path,
    opts: &Options,
    input: &str,
    output: Option<&Path>,
    preprocess_only: bool,
) -> Result<()> {
    let mut cmd = shyc_command(repo, opts.print_only)?;
    cmd.arg(format!("--target={}", opts.target));
    if preprocess_only {
        cmd.arg("-E");
    } else {
        cmd.arg("-S");
    }
    add_libshy_includes(&mut cmd, repo);
    for arg in &opts.compile_args {
        if forwarded_to_shyc(arg) {
            cmd.arg(arg);
        }
    }
    if let Some(output) = output {
        cmd.arg("-o").arg(output);
    }
    cmd.arg(input);
    run_command(opts, &mut cmd)
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
) -> Result<()> {
    let mut cmd = shyc_command(repo, opts.print_only)?;
    cmd.arg(format!("--emit={emit}"));
    if emit == "llvm" || opts.target != "shy" {
        cmd.arg(format!("--target={}", opts.target));
    }
    for arg in &opts.compile_args {
        if forwarded_to_shyc(arg) {
            cmd.arg(arg);
        }
    }
    if let Some(output) = output {
        cmd.arg("-o").arg(output);
    }
    cmd.arg(input);
    run_command(opts, &mut cmd)
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
                emit_llvm(repo, opts, Path::new(input), &ll, true)?;
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
            let ll = temps.temp_path(".ll");
            emit_llvm(repo, opts, &repo.join(source), &ll, false)?;
            clang_inputs.push(ll);
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

fn run_chibicc(
    repo: &Path,
    opts: &Options,
    input: &str,
    output: Option<&Path>,
    preprocess_only: bool,
    link_runtime: bool,
) -> Result<()> {
    let chibicc = chibicc_path(repo, opts.print_only)?;
    let mut cmd = Command::new(chibicc);
    cmd.arg("--target=shy");
    if preprocess_only {
        cmd.arg("-E");
    } else {
        cmd.arg("-S");
    }
    if link_runtime {
        cmd.arg("--shy-link-runtime");
    }
    add_libshy_includes(&mut cmd, repo);
    cmd.args(&opts.compile_args);
    if let Some(output) = output {
        cmd.arg("-o").arg(output);
    }
    cmd.arg(input);
    run_command(opts, &mut cmd)
}

fn run_asm(repo: &Path, opts: &Options, input: &Path, output: &Path) -> Result<()> {
    let mut cmd = tool_command(repo, "asm", "shyasm");
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
    let mut cmd = tool_command(repo, "linker", "shyld");
    // libshy is linked as whole objects, not an archive, so drop the functions
    // nothing reaches: a program that only calls `puts` keeps no `printf`.
    cmd.arg("--gc-sections");
    cmd.args(inputs).arg("-o").arg(output);
    if let Some(sym) = sym {
        cmd.arg("--sym").arg(sym);
    }
    run_command(opts, &mut cmd)
}

fn run_shy_dwarf(
    repo: &Path,
    opts: &Options,
    debug_inputs: &[(PathBuf, PathBuf)],
    sym: &Path,
    output: &Path,
) -> Result<()> {
    let mut cmd = if let Ok(path) = env::var("SHYCC_SHY_DWARF") {
        Command::new(path)
    } else {
        let mut cmd = Command::new("cargo");
        cmd.arg("run")
            .arg("-q")
            .arg("--manifest-path")
            .arg(repo.join("Cargo.toml"))
            .arg("-p")
            .arg("shy-dwarf")
            .arg("--");
        cmd
    };
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
    let src = repo.join("third_party/chibicc/shy_runtime_softfloat.c");
    if !src.exists() {
        bail!("missing internal float library source: {}", src.display());
    }

    let runtime_c = temps.temp_path(".float.c");
    let body = fs::read_to_string(&src)
        .with_context(|| format!("failed to read internal float library: {}", src.display()))?;
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
    Ok(obj)
}

/// libshy's layers above the C library, in dependency order.
const LIBSHY_RUST_LAYERS: [&str; 3] = ["core/core.shyc", "alloc/alloc.shyc", "std/std.shyc"];

/// One unit through `shyc --emit=llvm`. A library unit gets only the
/// optimization level, never the user's `-I`/`-D`/`-U`.
fn emit_llvm(repo: &Path, opts: &Options, input: &Path, ll: &Path, user_unit: bool) -> Result<()> {
    let mut cmd = shyc_command(repo, opts.print_only)?;
    cmd.arg("--emit=llvm")
        .arg(format!("--target={}", opts.target));
    if bare_target(&opts.target) {
        cmd.arg("-ffreestanding");
    }
    add_libshy_includes(&mut cmd, repo);
    for arg in &opts.compile_args {
        if forwarded_to_shyc(arg) && (user_unit || arg.starts_with("-O")) {
            cmd.arg(arg);
        }
    }
    cmd.arg("-o").arg(ll).arg(input);
    run_command(opts, &mut cmd)
}

/// The Shy target's whole library: the portable C layer, the Shy
/// implementation of its platform calls, then core, alloc, and std. ShyOS
/// links the same sources with its own system calls instead of `sys/shy`.
fn build_libshy(repo: &Path, opts: &Options, temps: &mut TempFiles) -> Result<Vec<PathBuf>> {
    let mut sources = vec!["libc/libc.c", "sys/shy.shyc"];
    sources.extend(LIBSHY_RUST_LAYERS);
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
        objects.push(obj);
    }
    Ok(objects)
}

fn tool_command(repo: &Path, package: &str, installed_name: &str) -> Command {
    if let Ok(path) = env::var(format!("SHYCC_{}", installed_name.to_ascii_uppercase())) {
        return Command::new(path);
    }
    if let Ok(path) = env::var(format!("SHYCC_{}", package.to_ascii_uppercase())) {
        return Command::new(path);
    }

    if let Ok(current) = env::current_exe()
        && let Some(dir) = current.parent()
    {
        let sibling = dir.join(installed_name);
        if sibling.exists() {
            return Command::new(sibling);
        }
    }

    // Keep the caller's working directory so relative input and output paths
    // keep their meaning; `--manifest-path` finds the workspace instead.
    let mut cmd = Command::new("cargo");
    cmd.arg("run")
        .arg("-q")
        .arg("--manifest-path")
        .arg(repo.join("Cargo.toml"))
        .arg("-p")
        .arg(package)
        .arg("--");
    cmd
}

fn chibicc_path(repo: &Path, print_only: bool) -> Result<PathBuf> {
    if let Ok(path) = env::var("SHYCC_CHIBICC") {
        return Ok(PathBuf::from(path));
    }

    if let Ok(current) = env::current_exe()
        && let Some(dir) = current.parent()
    {
        let sibling = dir.join("chibicc");
        if sibling.exists() {
            return Ok(sibling);
        }
    }

    let path = repo.join("third_party/chibicc/chibicc");
    if path.exists() || print_only {
        return Ok(path);
    }

    let status = Command::new("make")
        .current_dir(repo.join("third_party/chibicc"))
        .arg("chibicc")
        .status()
        .context("failed to run `make -C third_party/chibicc chibicc`")?;
    ensure_success(status, "make chibicc")?;
    Ok(path)
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
        Some("sobj") => Ok(InputKind::Obj),
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
         inputs: .shyc/.c, .shy, .sobj\n\
         backend: --backend shyc|chibicc (default shyc)\n\
         target: --target <name> (default shy); rv64/riscv64, rv32/riscv32, x64/x86_64,\n\
         \x20 arm64/aarch64 and la64/loong64/loongarch64 name Linux targets (rv32 needs\n\
         \x20 a sysroot), a `-bare` suffix makes them freestanding, `host` is the local\n\
         \x20 clang triple, and a full triple is handed to clang as written\n\
         sysroot: --sysroot <dir> for a cross link (default /usr/<triple>)\n\
         optimization: -O0, -O1, -O2, or -O3 (forwarded to shyc; -O0 by default)\n\
         libraries: libshy and the soft-float runtime are linked by default;\n\
         \x20 -nostdlib/-nodefaultlibs disables that, -llibshy and -lfloat\n\
         \x20 are accepted and never duplicated"
    );
}
