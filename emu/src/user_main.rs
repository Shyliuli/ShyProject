//! `shyemu-user`：以用户态进程运行 `.sfs`，系统调用直接透传到宿主。
//!
//! 用法：`shyemu-user prog.sfs [args...]`。整机模式仍然是 `shyemu`。

use std::env;
use std::ffi::{OsStr, OsString};
use std::path::Path;

use anyhow::{Context, Result, bail};
use emu::cpu::{DEFAULT_MEM_SIZE, Emu};
use emu::user;
use shy_isa_lib::file::shyfile::File;

fn main() -> Result<()> {
    let args: Vec<OsString> = env::args_os().skip(1).collect();
    let Some(path_arg) = args.first() else {
        bail!("usage: shyemu-user <program.sfs> [args...]");
    };
    let path = Path::new(path_arg);
    if path_arg == OsStr::new("--version") {
        println!("shyemu-user {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    if path_arg.to_string_lossy().starts_with('-') {
        bail!("unknown option: {}", path.display());
    }
    if !path.exists() {
        bail!("input file does not exist: {}", path.display());
    }
    let Some(path_text) = path_arg.to_str() else {
        bail!("input file path is not valid UTF-8: {}", path.display());
    };

    let Ok(file) = File::open(path_text) else {
        bail!("failed to open input file: {}", path.display());
    };
    let image = file.as_slice().to_vec();

    let mut emu = Emu::new(false, DEFAULT_MEM_SIZE);
    emu.load_image(&image)
        .with_context(|| format!("failed to load image: {}", path.display()))?;

    // 宿主环境按原始字节传递，argv[0] 就是程序路径。环境变量不要求是可读
    // Unicode；guest 侧只看到普通的 NUL 结尾字符串。
    let guest_args: Vec<Vec<u8>> = args
        .iter()
        .map(|arg| arg.as_encoded_bytes().to_vec())
        .collect();
    let environment: Vec<Vec<u8>> = env::vars_os()
        .map(|(key, value)| {
            let mut entry = key.as_encoded_bytes().to_vec();
            entry.push(b'=');
            entry.extend_from_slice(value.as_encoded_bytes());
            entry
        })
        .collect();
    let entry = user::setup_stack(&mut emu, &guest_args, &environment)?;
    emu.enter_user(entry);

    let code = emu.run();
    std::process::exit((code & 0xFF) as i32);
}
