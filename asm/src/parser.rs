pub use anyhow::{Context, Result, bail};
use shy_isa_lib::{file::shyfile, op, reg};
use std::{collections::HashMap, vec};
/// 解析后的 ShyISA 汇编源码。
///
/// 当前阶段负责：
/// - 把源码行分到 `___DEFINE___`、`___DATA___` 和 `___CODE___` 三个段中
/// - 将 DEFINE 段解析为 `name → value` 的常量映射表
#[derive(Debug)]
pub struct ParsedSource {
    /// `___DEFINE___` 段的解析结果：名字 → 32 位值。
    pub defines: HashMap<String, u32>,
    /// 删除注释后，`___DATA___` 段中的非空源码行。
    pub data: Vec<String>,
    /// 删除注释后，`___CODE___` 段中的非空源码行。
    pub code: Vec<String>,
}

#[derive(Clone, Copy)]
enum SectionState {
    Start,
    Define,
    Data,
    Code,
}

/// 解析 ShyISA 汇编中的 u32 数字字面量。
///
/// 支持十进制、`0x`/`0X` 十六进制、`b`/`B` 后缀二进制。
pub fn parse_u32_literal(raw: &str) -> Result<u32> {
    let raw = raw.trim();
    let raw_lower = raw.to_ascii_lowercase();

    // 十六进制：以 0x 或 0X 开头。必须先于二进制后缀判断，
    // 否则 0x1b 这类合法十六进制会被误判成二进制。
    if let Some(hex) = raw_lower.strip_prefix("0x") {
        return u32::from_str_radix(hex, 16).with_context(|| format!("无效的十六进制常量: {raw}"));
    }

    // 二进制：以 b 或 B 结尾
    if let Some(digits) = raw_lower.strip_suffix('b') {
        return u32::from_str_radix(digits, 2).with_context(|| format!("无效的二进制常量: {raw}"));
    }

    // 十进制
    raw_lower
        .parse::<u32>()
        .with_context(|| format!("无效的数字常量: {raw}"))
}

/// 将 DEFINE 行的 value 部分解析为 u32。
fn resolve_define_value(raw: &str) -> u32 {
    parse_u32_literal(raw).unwrap_or_else(|err| panic!("{err}"))
}

// 已知寄存器名（小写），用于检查 DEFINE 名是否冲突。
fn is_register_name(name: &str) -> bool {
    matches!(
        name,
        "0x" | "1x"
            | "2x"
            | "3x"
            | "4x"
            | "5x"
            | "6x"
            | "7x"
            | "8x"
            | "9x"
            | "ax"
            | "bx"
            | "cx"
            | "dx"
            | "ex"
            | "fx"
            | "pc"
            | "sp"
            | "tm"
            | "status"
            | "trap"
            | "m1"
            | "m2"
            | "m3"
            | "m4"
            | "rs"
            | "exit"
            | "epc"
            | "cause"
            | "ksp"
    )
}

impl ParsedSource {
    /// 将汇编源码切分为 ShyISA 的三个顶层段，并解析 DEFINE 常量。
    ///
    /// 解析器会取得 `src` 的所有权，先删除注释，再忽略空行，并保存去掉首尾空白后的
    /// 源码行，供后续解析阶段继续处理。
    pub fn new(mut src: String) -> ParsedSource {
        // 先统一删除注释，后面的段切分就只需要处理有效源码行。
        ParsedSource::remove_comment(&mut src);
        let mut state = SectionState::Start;

        let mut defines_raw: Vec<String> = Vec::new();
        let mut data: Vec<String> = Vec::new();
        let mut code: Vec<String> = Vec::new();

        // ShyISA 汇编文件按 DEFINE -> DATA -> CODE 的顺序组织。
        // 这里用一个小状态机记录当前所在段，并把普通源码行放入对应 Vec。
        for l in src.lines().map(str::trim).filter(|l| !l.is_empty()) {
            match state {
                SectionState::Start => {
                    if ParsedSource::is_section_marker(l, "___DEFINE___") {
                        state = SectionState::Define;
                    }
                }
                SectionState::Define => {
                    if ParsedSource::is_section_marker(l, "___DATA___") {
                        state = SectionState::Data;
                    } else {
                        defines_raw.push(l.to_owned());
                    }
                }
                SectionState::Data => {
                    if ParsedSource::is_section_marker(l, "___CODE___") {
                        state = SectionState::Code;
                    } else {
                        data.push(l.to_owned());
                    }
                }
                SectionState::Code => {
                    code.push(l.to_owned());
                }
            }
        }

        ParsedSource {
            defines: ParsedSource::parse_defines(defines_raw),
            data,
            code,
        }
    }

    fn parse_defines(lines: Vec<String>) -> HashMap<String, u32> {
        let mut map: HashMap<String, u32> = HashMap::new();
        for line in lines {
            let mut parts = line.split_whitespace();
            let name = match parts.next() {
                Some(n) => n.to_owned(),
                None => continue,
            };
            let name_lower = name.to_lowercase();
            let value_str = match parts.next() {
                Some(v) => v,
                None => panic!("DEFINE 行缺少 value: {line}"),
            };
            if parts.next().is_some() {
                panic!("DEFINE 行多余内容: {line}");
            }

            // 不允许 DEFINE 名字与寄存器名冲突
            if is_register_name(&name_lower) {
                panic!("DEFINE 名字与寄存器名冲突: {name}");
            }

            let resolved = resolve_define_value(value_str);

            if map.contains_key(&name) {
                panic!("DEFINE 中名字重复定义: {name}");
            }
            map.insert(name, resolved);
        }
        map
    }

    fn is_section_marker(line: &str, marker: &str) -> bool {
        // 允许段标记内部夹杂空白，例如 `___ DEFINE ___`。
        line.chars()
            .filter(|c| !c.is_whitespace())
            .eq(marker.chars())
    }

    /// 删除 `//` 与 `/* ... */` 注释，但不把字符串字面量内部的注释标记当作注释。
    ///
    /// 转义序列会原样保留给后续数据段解析。本阶段处理转义的目的，只是避免把
    /// `"hello \" world"` 这类字符串中的转义双引号误判为字符串结束。
    fn remove_comment(src: &mut String) {
        let mut out = String::with_capacity(src.len());
        let mut chars = src.chars().peekable();

        let mut in_block_comment = false;
        let mut in_string = false;
        let mut escaped = false;

        while let Some(c) = chars.next() {
            if in_block_comment {
                // 块注释内容全部丢弃，但保留换行，方便以后做行号诊断。
                if c == '*' && chars.peek() == Some(&'/') {
                    chars.next();
                    in_block_comment = false;
                } else if c == '\n' {
                    out.push('\n');
                }
                continue;
            }

            if in_string {
                // 字符串中的内容原样保留，注释标记不会在这里生效。
                out.push(c);

                if escaped {
                    escaped = false;
                    continue;
                }

                if c == '\\' {
                    escaped = true;
                    continue;
                }

                if c == '"' {
                    in_string = false;
                }

                continue;
            }

            if c == '"' {
                // 进入字符串后交给 in_string 分支处理，直到遇到未转义的双引号。
                in_string = true;
                out.push(c);
                continue;
            }

            if c == '/' && chars.peek() == Some(&'/') {
                // 行注释从 `//` 持续到换行；换行保留下来维持源码行结构。
                chars.next();

                for next in chars.by_ref() {
                    if next == '\n' {
                        out.push('\n');
                        break;
                    }
                }
                continue;
            }

            if c == '/' && chars.peek() == Some(&'*') {
                // 块注释可以跨行，退出条件在 in_block_comment 分支中处理。
                chars.next();
                in_block_comment = true;
                continue;
            }

            out.push(c);
        }

        *src = out;
    }
    pub fn impl_defines(&mut self) {
        // 分别替换data和code段
        for i in self.defines.iter() {
            for s in &mut self.data {
                if s.contains(i.0) {
                    *s = s.replace(i.0, &i.1.to_string());
                }
            }
            for s in &mut self.code {
                if s.contains(i.0) {
                    *s = s.replace(i.0, &i.1.to_string());
                }
            }
        }
    }
}
impl Obj {
    pub fn from(source: ParsedSource) -> Result<Self> {
        let mut labels: Vec<ObjSymbol> = vec![];
        let mut sections: Vec<ObjSection> = vec![];
        let mut symbols: Vec<ObjSymbol> = vec![];
        let mut reloc: Vec<ObjRelocation> = vec![];

        // 创建初始section
        let mut start = 0usize;
        let first_section = source
            .code
            .first()
            .and_then(|line| line.strip_prefix(".section "));

        // 根据前面的结果 决定是否跳过初始section定义
        if let Some(section_name) = first_section {
            sections.push(ObjSection {
                name: section_name.to_string(),
                bytes: vec![],
            });
            symbols.push(ObjSymbol {
                name: section_name.to_string(),
                section: section_name.to_string(),
                offset: 0,
            });
            start = 1;
        } else {
            sections.push(ObjSection {
                name: ".text".to_string(),
                bytes: vec![],
            });
            symbols.push(ObjSymbol {
                name: ".text".to_string(),
                section: ".text".to_string(),
                offset: 0,
            });
        }
        //code生成
        for i in start..source.code.len() {
            if let Some(section_name) = source.code[i].strip_prefix(".section ") {
                // 新section
                sections.push(ObjSection {
                    name: section_name.to_string(),
                    bytes: vec![],
                });
                symbols.push(ObjSymbol {
                    name: section_name.to_string(),
                    section: section_name.to_string(),
                    offset: 0,
                });
            } else if let Some(symbol_name) = source.code[i].strip_prefix(".symbol ") {
                // 新symbol
                let section = sections
                    .last()
                    .ok_or_else(|| anyhow::anyhow!("没有初始section!"))?;
                symbols.push(ObjSymbol {
                    name: symbol_name.to_string(),
                    section: section.name.clone(),
                    offset: section.bytes.len() as u32,
                });
            } else if let Some(label_name) = source.code[i].strip_suffix(":") {
                // 新label
                let section = sections
                    .last()
                    .ok_or_else(|| anyhow::anyhow!("没有初始section!"))?;
                labels.push(ObjSymbol {
                    name: label_name.to_string(),
                    section: section.name.clone(),
                    offset: section.bytes.len() as u32,
                });
            } else {
                // 应当是正常语句
                let section = sections
                    .last_mut()
                    .ok_or_else(|| anyhow::anyhow!("没有初始section!"))?;
                // pc
                let offset = section.bytes.len() as u32;

                // 分token
                let tokens: Vec<&str> = source.code[i].split_whitespace().collect();
                // command
                let command = op::OpType::from_str(tokens[0])?.to_u32();
                // 其他token
                let mut res: [u32; 2] = [0; 2];

                for arg_i in 0..2 {
                    let Some(token) = tokens.get(arg_i + 1) else {
                        continue;
                    };
                    if is_register_name(token) {
                        // 是寄存器
                        res[arg_i] = reg::RegType::from_str(token)?.to_u32();
                    } else if let Ok(num) = parse_u32_literal(token) {
                        // 字面值
                        res[arg_i] = num;
                    }
                    // label之类的 先统一解析为symbol
                    else if let Some((base, addend)) =
                        token.strip_suffix(")").and_then(|s| s.split_once("("))
                    {
                        reloc.push(ObjRelocation {
                            section: section.name.clone(),
                            offset: offset + 4 * (arg_i as u32 + 1),
                            target: RelocTarget::Symbol(base.to_string()),
                            addend: parse_u32_literal(addend)?,
                        });
                    } else {
                        reloc.push(ObjRelocation {
                            section: section.name.clone(),
                            offset: offset + 4 * (arg_i as u32 + 1),
                            target: RelocTarget::Symbol(token.to_string()),
                            addend: 0,
                        });
                    }
                }

                section.bytes.extend_from_slice(&command.to_be_bytes());
                section.bytes.extend_from_slice(&res[0].to_be_bytes());
                section.bytes.extend_from_slice(&res[1].to_be_bytes());
            }
        }
        //data生成
        let mut data_start = 0usize;
        let first_data_section = source
            .data
            .first()
            .and_then(|line| line.strip_prefix(".section "));

        if let Some(section_name) = first_data_section {
            sections.push(ObjSection {
                name: section_name.to_string(),
                bytes: vec![],
            });
            symbols.push(ObjSymbol {
                name: section_name.to_string(),
                section: section_name.to_string(),
                offset: 0,
            });
            data_start = 1;
        } else if !source.data.is_empty() {
            sections.push(ObjSection {
                name: "data".to_string(),
                bytes: vec![],
            });
            symbols.push(ObjSymbol {
                name: "data".to_string(),
                section: "data".to_string(),
                offset: 0,
            });
        }

        for i in data_start..source.data.len() {
            if let Some(section_name) = source.data[i].strip_prefix(".section ") {
                // 新data section
                sections.push(ObjSection {
                    name: section_name.to_string(),
                    bytes: vec![],
                });
                symbols.push(ObjSymbol {
                    name: section_name.to_string(),
                    section: section_name.to_string(),
                    offset: 0,
                });
            } else if let Some(symbol_name) = source.data[i].strip_prefix(".symbol ") {
                // 新data symbol
                let section = sections
                    .last()
                    .ok_or_else(|| anyhow::anyhow!("没有初始data section!"))?;
                symbols.push(ObjSymbol {
                    name: symbol_name.to_string(),
                    section: section.name.clone(),
                    offset: section.bytes.len() as u32,
                });
            } else {
                // 应当是正常data语句
                let section = sections
                    .last_mut()
                    .ok_or_else(|| anyhow::anyhow!("没有初始data section!"))?;

                // 分成写入位置和写入内容
                let mut tokens = source.data[i].splitn(2, char::is_whitespace);
                let addr = tokens
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("data 行缺少写入位置!"))?;
                let value = tokens
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("data 行缺少写入内容: {}", source.data[i]))?
                    .trim();

                // 解析data写入位置
                let (base, addend) = match addr.strip_suffix(")").and_then(|s| s.split_once("(")) {
                    Some((base, addend)) => (base, parse_u32_literal(addend)?),
                    None => (addr, 0),
                };
                let write_offset = if let Ok(num) = parse_u32_literal(base) {
                    num + addend
                } else if section.name == base {
                    addend
                } else if let Some(label) = labels.iter().find(|label| label.name == base) {
                    if label.section != section.name {
                        bail!("data 写入位置不在当前section: {addr}");
                    }
                    label.offset + addend
                } else if let Some(symbol) = symbols.iter().find(|symbol| symbol.name == base) {
                    if symbol.section != section.name {
                        bail!("data 写入位置不在当前section: {addr}");
                    }
                    symbol.offset + addend
                } else {
                    bail!("无法解析data写入位置: {addr}");
                } as usize;

                if section.bytes.len() < write_offset {
                    section.bytes.resize(write_offset, 0);
                }

                // 写入字符串
                if let Some(s) = value.strip_prefix("\"").and_then(|s| s.strip_suffix("\"")) {
                    if section.bytes.len() == write_offset {
                        section.bytes.extend_from_slice(s.as_bytes());
                        section.bytes.push(0);
                    } else {
                        let end = write_offset + s.len() + 1;
                        if section.bytes.len() < end {
                            section.bytes.resize(end, 0);
                        }
                        section.bytes[write_offset..write_offset + s.len()]
                            .copy_from_slice(s.as_bytes());
                        section.bytes[end - 1] = 0;
                    }
                }
                // 写入数组
                else if let Some(array) =
                    value.strip_prefix("{").and_then(|s| s.strip_suffix("}"))
                {
                    let mut offset = write_offset;
                    for item in array.split(",").map(str::trim).filter(|s| !s.is_empty()) {
                        let num = parse_u32_literal(item)?;
                        if section.bytes.len() < offset + 4 {
                            section.bytes.resize(offset + 4, 0);
                        }
                        section.bytes[offset..offset + 4].copy_from_slice(&num.to_be_bytes());
                        offset += 4;
                    }
                }
                // 写入数字
                else if let Ok(num) = parse_u32_literal(value) {
                    if section.bytes.len() < write_offset + 4 {
                        section.bytes.resize(write_offset + 4, 0);
                    }
                    section.bytes[write_offset..write_offset + 4]
                        .copy_from_slice(&num.to_be_bytes());
                }
                // 写入symbol占位
                else {
                    let (base, addend) =
                        match value.strip_suffix(")").and_then(|s| s.split_once("(")) {
                            Some((base, addend)) => (base, parse_u32_literal(addend)?),
                            None => (value, 0),
                        };
                    if section.bytes.len() < write_offset + 4 {
                        section.bytes.resize(write_offset + 4, 0);
                    }
                    reloc.push(ObjRelocation {
                        section: section.name.clone(),
                        offset: write_offset as u32,
                        target: RelocTarget::Symbol(base.to_string()),
                        addend,
                    });
                }
            }
        }

        // 尝试解析本地可解析的label/section/symbol
        for r in &mut reloc {
            let RelocTarget::Symbol(name) = &r.target else {
                continue;
            };
            if let Some(label) = labels.iter().find(|label| label.name == *name) {
                r.target = RelocTarget::SectionOffset {
                    section: label.section.clone(),
                    offset: label.offset,
                };
            } else if let Some(symbol) = symbols.iter().find(|symbol| symbol.name == *name) {
                r.target = RelocTarget::SectionOffset {
                    section: symbol.section.clone(),
                    offset: symbol.offset,
                };
            }
        }

        Ok(Self {
            section: sections,
            symbol: symbols,
            relocation: reloc,
        })
    }
    /*AIGC:codex*/
    pub fn to_file(self, mut f: shyfile::File) -> Result<()> {
        const MAGIC: u32 = 0x66CCFF00;
        const HEADER_SIZE: usize = 16;

        if !f.is_empty() {
            bail!("output object file must be empty");
        }

        fn push_u32(buf: &mut Vec<u8>, value: u32) {
            buf.extend_from_slice(&value.to_be_bytes());
        }

        fn push_c_str(buf: &mut Vec<u8>, value: &str) -> Result<()> {
            if value.as_bytes().contains(&0) {
                bail!("object string contains NUL byte: {value:?}");
            }
            buf.extend_from_slice(value.as_bytes());
            buf.push(0);
            Ok(())
        }

        fn file_offset(value: usize) -> Result<u32> {
            u32::try_from(value).context("object file offset exceeds u32::MAX")
        }

        fn next_offset(buf_len: usize, node_len: usize, has_next: bool) -> Result<u32> {
            if has_next {
                file_offset(
                    buf_len
                        .checked_add(node_len)
                        .context("object file offset overflow")?,
                )
            } else {
                Ok(0)
            }
        }

        let section_start = if self.section.is_empty() {
            0
        } else {
            HEADER_SIZE as u32
        };

        let mut buf = Vec::new();
        push_u32(&mut buf, MAGIC);
        push_u32(&mut buf, section_start);
        push_u32(&mut buf, 0);
        push_u32(&mut buf, 0);

        for (index, section) in self.section.iter().enumerate() {
            let node_len = 4 + section.name.len() + 1 + 4 + section.bytes.len();
            let next_section = next_offset(buf.len(), node_len, index + 1 < self.section.len())?;

            push_u32(&mut buf, next_section);
            push_c_str(&mut buf, &section.name)?;
            push_u32(
                &mut buf,
                u32::try_from(section.bytes.len()).context("section byte_size exceeds u32::MAX")?,
            );
            buf.extend_from_slice(&section.bytes);
        }

        let symbol_start = if self.symbol.is_empty() {
            0
        } else {
            file_offset(buf.len())?
        };

        for (index, symbol) in self.symbol.iter().enumerate() {
            let node_len = 4 + 4 + symbol.section.len() + 1 + symbol.name.len() + 1;
            let next_symbol = next_offset(buf.len(), node_len, index + 1 < self.symbol.len())?;

            push_u32(&mut buf, next_symbol);
            push_u32(&mut buf, symbol.offset);
            push_c_str(&mut buf, &symbol.section)?;
            push_c_str(&mut buf, &symbol.name)?;
        }

        let relocation_start = if self.relocation.is_empty() {
            0
        } else {
            file_offset(buf.len())?
        };

        for (index, relocation) in self.relocation.iter().enumerate() {
            let target_len = match &relocation.target {
                RelocTarget::Symbol(name) => name.len() + 1,
                RelocTarget::SectionOffset { section, .. } => 4 + section.len() + 1,
            };
            let node_len = 4 + 4 + 4 + 4 + relocation.section.len() + 1 + target_len;
            let next_relocation =
                next_offset(buf.len(), node_len, index + 1 < self.relocation.len())?;

            push_u32(&mut buf, next_relocation);
            push_u32(&mut buf, relocation.offset);
            push_u32(&mut buf, relocation.addend);
            match &relocation.target {
                RelocTarget::Symbol(name) => {
                    push_u32(&mut buf, 1);
                    push_c_str(&mut buf, &relocation.section)?;
                    push_c_str(&mut buf, name)?;
                }
                RelocTarget::SectionOffset { section, offset } => {
                    push_u32(&mut buf, 0);
                    push_c_str(&mut buf, &relocation.section)?;
                    push_u32(&mut buf, *offset);
                    push_c_str(&mut buf, section)?;
                }
            }
        }

        buf[8..12].copy_from_slice(&symbol_start.to_be_bytes());
        buf[12..16].copy_from_slice(&relocation_start.to_be_bytes());

        f.push_back_slice(&buf)?;
        f.flush()?;
        Ok(())
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjSection {
    pub name: String,
    pub bytes: Vec<u8>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjSymbol {
    pub name: String,
    pub section: String,
    pub offset: u32,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjRelocation {
    pub section: String,
    pub offset: u32,
    pub target: RelocTarget,
    pub addend: u32,
}
/// 输出格式
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Obj {
    pub section: Vec<ObjSection>,
    pub symbol: Vec<ObjSymbol>,
    pub relocation: Vec<ObjRelocation>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RelocTarget {
    Symbol(String),
    SectionOffset { section: String, offset: u32 },
}
#[cfg(test)]
mod tests {
    use super::{
        Obj, ObjRelocation, ObjSection, ObjSymbol, ParsedSource, RelocTarget, parse_u32_literal,
    };
    use shy_isa_lib::file::shyfile;

    #[test]
    fn parses_u32_literals_with_supported_radices() {
        assert_eq!(parse_u32_literal("123").unwrap(), 123);
        assert_eq!(parse_u32_literal("0x10").unwrap(), 16);
        assert_eq!(parse_u32_literal("0X1B").unwrap(), 27);
        assert_eq!(parse_u32_literal("1100b").unwrap(), 12);
        assert_eq!(parse_u32_literal("1010B").unwrap(), 10);
        assert_eq!(parse_u32_literal(" 42 ").unwrap(), 42);
    }

    #[test]
    fn rejects_invalid_u32_literals() {
        assert!(parse_u32_literal("0xGG").is_err());
        assert!(parse_u32_literal("102b").is_err());
        assert!(parse_u32_literal("-1").is_err());
    }

    #[test]
    fn writes_sobj_binary_format() {
        let path = format!(
            "target/test-{}-{}.sobj",
            std::process::id(),
            "writes_sobj_binary_format"
        );
        std::fs::create_dir_all("target").unwrap();
        let _ = std::fs::remove_file(&path);

        let obj = Obj {
            section: vec![ObjSection {
                name: "text._start".to_string(),
                bytes: vec![0xAA, 0xBB, 0xCC, 0xDD],
            }],
            symbol: vec![ObjSymbol {
                name: "_start".to_string(),
                section: "text._start".to_string(),
                offset: 0,
            }],
            relocation: vec![ObjRelocation {
                section: "text._start".to_string(),
                offset: 4,
                target: RelocTarget::Symbol("print".to_string()),
                addend: 8,
            }],
        };

        let file = shyfile::File::open(&path).unwrap();
        obj.to_file(file).unwrap();

        let bytes = std::fs::read(&path).unwrap();
        let _ = std::fs::remove_file(&path);

        assert_eq!(&bytes[0..4], &0x66CCFF00u32.to_be_bytes());
        assert_eq!(&bytes[4..8], &16u32.to_be_bytes());

        let symbol_start = u32::from_be_bytes(bytes[8..12].try_into().unwrap()) as usize;
        let relocation_start = u32::from_be_bytes(bytes[12..16].try_into().unwrap()) as usize;
        assert!(symbol_start > 16);
        assert!(relocation_start > symbol_start);

        assert_eq!(&bytes[16..20], &0u32.to_be_bytes());
        assert_eq!(&bytes[20..32], b"text._start\0");
        assert_eq!(&bytes[32..36], &4u32.to_be_bytes());
        assert_eq!(&bytes[36..40], &[0xAA, 0xBB, 0xCC, 0xDD]);

        assert_eq!(&bytes[symbol_start..symbol_start + 4], &0u32.to_be_bytes());
        assert_eq!(
            &bytes[relocation_start + 12..relocation_start + 16],
            &1u32.to_be_bytes()
        );
    }

    #[test]
    fn builds_obj_sections_symbols_and_relocations() {
        let source = ParsedSource::new(
            r#"
            ___DEFINE___
            ___DATA___
            ___CODE___
            .section text._start
            .symbol _start
            loop:
            calln print
            jmpn loop(12)
            .section text.print
            .symbol print
            ret
            "#
            .to_owned(),
        );

        let obj = Obj::from(source).unwrap();

        assert_eq!(obj.section.len(), 2);
        assert_eq!(obj.section[0].name, "text._start");
        assert_eq!(obj.section[0].bytes.len(), 24);
        assert_eq!(obj.section[1].name, "text.print");
        assert_eq!(obj.section[1].bytes.len(), 12);

        assert!(
            obj.symbol
                .iter()
                .any(|symbol| symbol.name == "text._start" && symbol.offset == 0)
        );
        assert!(
            obj.symbol
                .iter()
                .any(|symbol| symbol.name == "_start" && symbol.section == "text._start")
        );
        assert!(
            obj.symbol
                .iter()
                .any(|symbol| symbol.name == "print" && symbol.section == "text.print")
        );

        assert_eq!(obj.relocation.len(), 2);
        assert_eq!(obj.relocation[0].section, "text._start");
        assert_eq!(obj.relocation[0].offset, 4);
        assert_eq!(
            obj.relocation[0].target,
            RelocTarget::SectionOffset {
                section: "text.print".to_string(),
                offset: 0
            }
        );
        assert_eq!(obj.relocation[0].addend, 0);
        assert_eq!(obj.relocation[1].offset, 16);
        assert_eq!(
            obj.relocation[1].target,
            RelocTarget::SectionOffset {
                section: "text._start".to_string(),
                offset: 0
            }
        );
        assert_eq!(obj.relocation[1].addend, 12);
    }

    #[test]
    fn parses_define_constants() {
        let sections = ParsedSource::new(
            r#"
            ___DEFINE___
            PI 314159
            STACK_INIT 0xEFFFFFFC
            FLAG 1100b

            ___DATA___
            0x100 "hello"

            ___CODE___
            outn PI
            "#
            .to_owned(),
        );

        assert_eq!(sections.defines["PI"], 314159);
        assert_eq!(sections.defines["STACK_INIT"], 0xEFFFFFFC);
        assert_eq!(sections.defines["FLAG"], 12);
        assert_eq!(sections.defines.len(), 3);
        assert_eq!(sections.data, [r#"0x100 "hello""#]);
        assert_eq!(sections.code, ["outn PI"]);
    }

    #[test]
    #[should_panic(expected = "DEFINE 名字与寄存器名冲突")]
    fn define_name_cannot_be_register() {
        ParsedSource::new(
            r#"
            ___DEFINE___
            SP 12345
            ___DATA___
            ___CODE___
            "#
            .to_owned(),
        );
    }

    #[test]
    #[should_panic(expected = "DEFINE 中名字重复定义")]
    fn define_duplicate_name_panics() {
        ParsedSource::new(
            r#"
            ___DEFINE___
            A 1
            A 2
            ___DATA___
            ___CODE___
            "#
            .to_owned(),
        );
    }

    #[test]
    fn define_names_are_case_sensitive() {
        let sections = ParsedSource::new(
            r#"
            ___DEFINE___
            A 1
            a 2
            ___DATA___
            ___CODE___
            "#
            .to_owned(),
        );

        assert_eq!(sections.defines["A"], 1);
        assert_eq!(sections.defines["a"], 2);
    }

    #[test]
    #[should_panic(expected = "无效的十六进制常量")]
    fn define_invalid_hex_panics() {
        ParsedSource::new(
            r#"
            ___DEFINE___
            BAD 0xGGGG
            ___DATA___
            ___CODE___
            "#
            .to_owned(),
        );
    }

    #[test]
    fn ignores_whitespace_inside_section_markers() {
        let sections = ParsedSource::new(
            r#"
            ___ DEFINE ___
            X 42
            ___ DATA ___
            0x100 2
            ___ CODE ___
            outn X
            "#
            .to_owned(),
        );

        assert_eq!(sections.defines["X"], 42);
        assert_eq!(sections.data, ["0x100 2"]);
        assert_eq!(sections.code, ["outn X"]);
    }

    #[test]
    fn removes_line_and_block_comments() {
        let sections = ParsedSource::new(
            r#"
            ___DEFINE___
            A 1 // line comment
            /* whole block
               comment */
            B 2

            ___DATA___
            0x100 3 /* inline block comment */

            ___CODE___
            outn A // trailing comment
            "#
            .to_owned(),
        );

        assert_eq!(sections.defines["A"], 1);
        assert_eq!(sections.defines["B"], 2);
        assert_eq!(sections.defines.len(), 2);
        assert_eq!(sections.data, ["0x100 3"]);
        assert_eq!(sections.code, ["outn A"]);
    }

    #[test]
    fn keeps_comment_markers_inside_strings() {
        let sections = ParsedSource::new(
            r#"
            ___DEFINE___
            URL 1
            ___DATA___
            0x100 "http://example.test/*not-comment*/"
            ___CODE___
            oututfn URL
            "#
            .to_owned(),
        );

        assert_eq!(sections.defines["URL"], 1);
        assert_eq!(
            sections.data,
            [r#"0x100 "http://example.test/*not-comment*/""#]
        );
    }

    #[test]
    fn escaped_quote_does_not_end_string_during_comment_removal() {
        let sections = ParsedSource::new(
            r#"
            ___DEFINE___
            V 5
            ___DATA___
            0x100 "hello \" // still string"
            ___CODE___
            oututfn V
            "#
            .to_owned(),
        );

        assert_eq!(sections.defines["V"], 5);
        assert_eq!(sections.data, [r#"0x100 "hello \" // still string""#]);
    }
}
