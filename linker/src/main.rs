mod link;
mod obj;

use std::env;
use std::path::Path;

use anyhow::{Context, Result, bail};
use shy_isa_lib::file::shyfile::File;

use crate::link::link;
use crate::obj::ObjectFile;

fn main() -> Result<()> {
    let args: Vec<String> = env::args().collect();

    // usage: linker <input.sobj>... [-o <output.sfs>] [--sym <symfile>]
    let mut inputs: Vec<String> = Vec::new();
    let mut output: Option<String> = None;
    let mut sym: Option<String> = None;

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "-o" => {
                i += 1;
                let Some(v) = args.get(i) else {
                    bail!("option `-o` requires an argument");
                };
                output = Some(v.clone());
            }
            "--sym" => {
                i += 1;
                let Some(v) = args.get(i) else {
                    bail!("option `--sym` requires an argument");
                };
                sym = Some(v.clone());
            }
            s if s.starts_with('-') => bail!("unknown option: {s}"),
            s => inputs.push(s.to_string()),
        }
        i += 1;
    }

    if inputs.is_empty() {
        bail!(
            "usage:{} <input.sobj>... [-o <output.sfs>] [--sym <symfile>]",
            args[0]
        );
    }

    let output = output.unwrap_or_else(|| "a.sfs".to_string());

    // 1. 读取并解析所有 .sobj 输入文件。
    let mut objects = Vec::with_capacity(inputs.len());
    for input in &inputs {
        if !Path::new(input).exists() {
            bail!("input file does not exist: {input}");
        }
        let Ok(file) = File::open(input) else {
            bail!("failed to open input file: {input}");
        };
        let buf = file.as_slice().to_vec();
        let obj = ObjectFile::from_bytes(&buf)
            .with_context(|| format!("failed to parse object file: {input}"))?;
        objects.push(obj);
    }

    // 2. 链接。
    let linked = link(objects)?;

    // 3. 写出 .sfs raw 内存镜像。shyfile 只能追加写入，所以先删掉旧输出文件。
    if Path::new(&output).exists() {
        std::fs::remove_file(&output)?;
    }
    let Ok(mut out) = File::open(&output) else {
        bail!("failed to open output file: {output}");
    };
    out.push_back_slice(&linked.image)
        .with_context(|| format!("failed to write output file: {output}"))?;
    out.flush()?;

    // 4. 可选：写出 .sym 符号表文本文件。
    if let Some(sym_path) = sym {
        let mut text = String::new();
        for (name, addr) in &linked.symbols {
            text.push_str(&format!("{name} 0x{addr:08x}\n"));
        }
        std::fs::write(&sym_path, text)
            .with_context(|| format!("failed to write symbol file: {sym_path}"))?;
    }

    Ok(())
}
