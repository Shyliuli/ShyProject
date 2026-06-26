mod parser;
use std::env;

use anyhow::bail;
use parser::ParsedSource;
use shy_isa_lib::file::shyfile::File;

use crate::parser::Obj;

fn main()->anyhow::Result<()> {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {

        bail!("usage:{} <filename>", args[0]);
    }

    let file_name = &args[1];
    let Ok(file) = File::open(file_name) else {
        bail!("failed to open file: {file_name}");

    };

    let src = String::from_utf8_lossy(file.as_slice());

    let mut source = ParsedSource::new(src.to_string());
    source.impl_defines();
    let obj=Obj::from(source)?;
    //todo: 定义OBJ的二进制格式 然后转换(isa_lib)
    println!("{:#?}", obj);
    Ok(())
}
