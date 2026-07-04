mod cpu;

use std::env;
use std::path::Path;

use anyhow::{Context, Result, bail};
use shy_isa_lib::file::shyfile::File;

use crate::cpu::{DEFAULT_MEM_SIZE, Emu};

/// 解析内存大小，支持 K/M 后缀，如 "32M"、"128K"、"512M"。
fn parse_mem_size(s: &str) -> Result<u32> {
    let s = s.trim();
    if s.is_empty() {
        bail!("empty --mem value");
    }

    let s_upper = s.to_uppercase();
    let (num_str, suffix) = match s_upper.as_str() {
        s if s.ends_with('K') => (&s[..s.len() - 1], 1024u64),
        s if s.ends_with('M') => (&s[..s.len() - 1], 1024 * 1024),
        s if s.ends_with('G') => (&s[..s.len() - 1], 1024 * 1024 * 1024),
        _ => (s, 1u64),
    };

    let num: u64 = num_str
        .parse()
        .map_err(|_| anyhow::anyhow!("invalid --mem size: {s}"))?;
    let bytes = num
        .checked_mul(suffix)
        .ok_or_else(|| anyhow::anyhow!("--mem size overflow: {s}"))?;
    if bytes > u32::MAX as u64 {
        bail!("--mem size too large: {s}");
    }
    Ok(bytes as u32)
}

fn main() -> Result<()> {
    let args: Vec<String> = env::args().collect();

    // usage: emu <input.sfs> [--debug] [--mem <size>]
    let mut input: Option<String> = None;
    let mut debug = false;
    let mut mem_size: u32 = DEFAULT_MEM_SIZE;

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--debug" => debug = true,
            "--mem" => {
                i += 1;
                if i >= args.len() {
                    bail!("--mem requires a size argument, e.g. 32M, 128M, 512M");
                }
                mem_size = parse_mem_size(&args[i])?;
            }
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

    let mut emu = Emu::new(debug, mem_size);
    emu.load_image(&image)
        .with_context(|| format!("failed to load image: {input}"))?;

    let code = emu.run();
    std::process::exit(code as i32 & 0xFF);
}
