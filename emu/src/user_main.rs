//! `shyemu-user`：以用户态进程运行 `.sfs`，系统调用直接透传到宿主。
//!
//! 用法：`shyemu-user prog.sfs [args...]`。整机模式仍然是 `shyemu`。

use std::env;
use std::path::Path;

use anyhow::{Context, Result, bail};
use emu::cpu::{DEFAULT_MEM_SIZE, Emu};
use emu::user;
use shy_isa_lib::file::shyfile::File;

fn main() -> Result<()> {
    let args: Vec<String> = env::args().skip(1).collect();
    let Some(path) = args.first() else {
        bail!("usage: shyemu-user <program.sfs> [args...]");
    };
    if path == "--version" {
        println!("shyemu-user {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    if path.starts_with('-') {
        bail!("unknown option: {path}");
    }
    if !Path::new(path).exists() {
        bail!("input file does not exist: {path}");
    }

    let Ok(file) = File::open(path) else {
        bail!("failed to open input file: {path}");
    };
    let image = file.as_slice().to_vec();

    let mut emu = Emu::new(false, DEFAULT_MEM_SIZE);
    emu.load_image(&image)
        .with_context(|| format!("failed to load image: {path}"))?;

    // 宿主环境原样传递，argv[0] 就是程序路径。
    let environment: Vec<String> = env::vars()
        .map(|(key, value)| format!("{key}={value}"))
        .collect();
    let entry = user::setup_stack(&mut emu, &args, &environment)?;
    emu.enter_user(entry);

    let code = emu.run();
    std::process::exit((code & 0xFF) as i32);
}
