mod cpu;

use std::env;
use std::path::Path;

use anyhow::{Context, Result, bail};
use shy_isa_lib::file::shyfile::File;

use crate::cpu::Emu;

fn main() -> Result<()> {
    let args: Vec<String> = env::args().collect();

    // usage: emu <input.sfs> [--debug]
    let mut input: Option<String> = None;
    let mut debug = false;

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--debug" => debug = true,
            s if s.starts_with('-') => bail!("unknown option: {s}"),
            s => {
                if input.is_none() {
                    input = Some(s.to_string());
                } else {
                    bail!("multiple input files given; expected exactly one .sfs");
                }
            }
        }
        i += 1;
    }

    let Some(input) = input else {
        bail!("usage:{} <input.sfs> [--debug]", args[0]);
    };

    if !Path::new(&input).exists() {
        bail!("input file does not exist: {input}");
    }

    // 读取 .sfs raw 内存镜像。
    let Ok(file) = File::open(&input) else {
        bail!("failed to open input file: {input}");
    };
    let image = file.as_slice().to_vec();

    let mut emu = Emu::new(debug);
    emu.load_image(&image)
        .with_context(|| format!("failed to load image: {input}"))?;

    let code = emu.run();
    std::process::exit(code as i32 & 0xFF);
}
