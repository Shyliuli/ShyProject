mod parser;
use std::{env, fs, path::Path};

use anyhow::bail;
use parser::ParsedSource;
use shy_isa_lib::file::shyfile::File;

use crate::parser::Obj;

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = env::args().collect();
    if args.len() != 2 && args.len() != 4 {
        bail!("usage:{} <filename> (-o <output_file>)", args[0]);
    }

    let file_name = &args[1];

    // 没有指定 -o 时，把输入文件后缀替换成 .sobj。
    let output_file_name = if args.len() == 2 {
        let mut name = file_name.to_string();
        if let Some(index) = name.rfind('.') {
            name.truncate(index);
        }
        name.push_str(".sobj");
        name
    } else {
        if args[2] != "-o" {
            bail!("usage:{} <filename> (-o <output_file>)", args[0]);
        }
        args[3].clone()
    };

    if !Path::new(file_name).exists() {
        bail!("input file does not exist: {file_name}");
    }

    if output_file_name == *file_name {
        bail!("output file must not be the same as input file");
    }

    // shyfile 只能追加写入，所以这里先删掉旧输出文件。
    if Path::new(&output_file_name).exists() {
        fs::remove_file(&output_file_name)?;
    }

    let Ok(file) = File::open(file_name) else {
        bail!("failed to open file: {file_name}");
    };

    let src = String::from_utf8_lossy(file.as_slice());

    // 先解析源码生成内存中的 Obj，再按 .sobj 格式写出。
    let mut source = ParsedSource::new(src.to_string());
    source.impl_defines();
    let obj = Obj::from(source)?;

    let Ok(output_file) = File::open(&output_file_name) else {
        bail!("failed to open output file: {output_file_name}");
    };
    obj.to_file(output_file)?;

    Ok(())
}
