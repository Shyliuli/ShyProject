//! ShyISA 链接器核心：按 `ObjFormat.md` 第 10 节的默认规则布局 section、解析
//! symbol、回填 relocation，并生成 `.sfs` raw 内存镜像。
//!
//! 默认布局规则：
//! - `text._start` 必须存在，放到程序入口地址 `0x00000100`。
//! - 其他 `text.*` section 接在 `text._start` 后面，按输入顺序依次放置。
//! - `data` 和 `data.*` section 从 `0x00200000` 开始依次放置。
//! - 其他 section 名暂不定义默认布局，链接器报错。
//! - section 起始地址按 4 字节对齐。

use std::collections::HashMap;

use anyhow::{Context, Result, bail};

use crate::obj::{ObjectFile, RelocTarget};

/// 程序入口地址，`text._start` 必须放到这里。
pub const ENTRY: u32 = 0x00000100;
/// `data` / `data.*` section 的起始地址。
pub const DATA_BASE: u32 = 0x0020_0000;

/// 链接输出。
pub struct LinkedOutput {
    /// `.sfs` raw 内存镜像，字节偏移即地址。
    pub image: Vec<u8>,
    /// symbol 名 -> 最终绝对地址，按地址升序排列。
    pub symbols: Vec<(String, u32)>,
}

fn align4(v: u32) -> u32 {
    (v + 3) & !3
}

fn is_text(name: &str) -> bool {
    name.starts_with("text.")
}

fn is_data(name: &str) -> bool {
    name == "data" || name.starts_with("data.")
}

/// 链接一个或多个 object 文件，生成 `.sfs` 内存镜像和符号表。
pub fn link(files: Vec<ObjectFile>) -> Result<LinkedOutput> {
    // 1. 合并所有 object 文件，检测重名 section 和重名 symbol。
    let mut sections = Vec::new();
    let mut symbols = Vec::new();
    let mut relocations = Vec::new();
    let mut seen_section: HashMap<String, ()> = HashMap::new();
    let mut seen_symbol: HashMap<String, ()> = HashMap::new();

    for file in &files {
        for s in &file.sections {
            if seen_section.insert(s.name.clone(), ()).is_some() {
                bail!("duplicate section: {}", s.name);
            }
            sections.push(s.clone());
        }
        for sym in &file.symbols {
            if seen_symbol.insert(sym.name.clone(), ()).is_some() {
                bail!("duplicate symbol: {}", sym.name);
            }
            symbols.push(sym.clone());
        }
        for r in &file.relocations {
            relocations.push(r.clone());
        }
    }

    // 2. 按默认规则为每个 section 分配最终地址。
    let mut bases: HashMap<String, u32> = HashMap::new();
    let mut text_cur: u32;
    let mut data_cur = DATA_BASE;

    // text._start 必须存在并放到入口地址。
    let has_start = sections.iter().any(|s| s.name == "text._start");
    if !has_start {
        bail!("missing entry section `text._start`");
    }
    bases.insert("text._start".to_string(), ENTRY);
    {
        let start = sections
            .iter()
            .find(|s| s.name == "text._start")
            .expect("checked above");
        text_cur = align4(ENTRY + start.bytes.len() as u32);
    }

    for s in &sections {
        if s.name == "text._start" {
            continue;
        }
        if is_text(&s.name) {
            bases.insert(s.name.clone(), text_cur);
            text_cur = align4(text_cur + s.bytes.len() as u32);
        } else if is_data(&s.name) {
            bases.insert(s.name.clone(), data_cur);
            data_cur = align4(data_cur + s.bytes.len() as u32);
        } else {
            bail!(
                "no default layout for section `{}`: only `text.*` and `data`/`data.*` are supported",
                s.name
            );
        }
    }

    // 3. 计算所有 symbol 的最终绝对地址。
    let mut sym_addr: HashMap<String, u32> = HashMap::new();
    for sym in &symbols {
        let base = *bases
            .get(&sym.section)
            .with_context(|| format!("symbol `{}` references unknown section `{}`", sym.name, sym.section))?;
        sym_addr.insert(sym.name.clone(), base + sym.offset);
    }

    // 4. 处理所有 relocation，回填 32 位大端序字段。
    let mut sec_index: HashMap<String, usize> = HashMap::new();
    for (i, s) in sections.iter().enumerate() {
        sec_index.insert(s.name.clone(), i);
    }

    for r in &relocations {
        let target_addr = match &r.target {
            RelocTarget::Symbol(name) => *sym_addr
                .get(name)
                .with_context(|| format!("undefined symbol: {name}"))?,
            RelocTarget::SectionOffset { section, offset } => {
                let base = *bases.get(section).with_context(|| {
                    format!("relocation references unknown section: {section}")
                })?;
                base + offset
            }
        };
        let final_addr = target_addr.wrapping_add(r.addend);

        let idx = *sec_index.get(&r.section).with_context(|| {
            format!("relocation references unknown section: {}", r.section)
        })?;
        let sec = &mut sections[idx];
        let off = r.offset as usize;
        if off + 4 > sec.bytes.len() {
            bail!(
                "relocation offset {off} out of range in section `{}` (len {})",
                sec.name,
                sec.bytes.len()
            );
        }
        sec.bytes[off..off + 4].copy_from_slice(&final_addr.to_be_bytes());
    }

    // 5. 把 section bytes 写入 `.sfs` raw 内存镜像对应地址。
    //    镜像长度至少覆盖最高已写入 section 字节的后一字节，且不小于入口地址。
    let mut max_end: u32 = ENTRY;
    for s in &sections {
        let base = *bases
            .get(&s.name)
            .expect("every section has a base address");
        let end = base + s.bytes.len() as u32;
        if end > max_end {
            max_end = end;
        }
    }
    let mut image = vec![0u8; max_end as usize];
    for s in &sections {
        let base = *bases.get(&s.name).expect("every section has a base address") as usize;
        image[base..base + s.bytes.len()].copy_from_slice(&s.bytes);
    }

    // 6. 整理符号表，按地址升序输出。
    let mut sym_out: Vec<(String, u32)> = sym_addr.into_iter().collect();
    sym_out.sort_by_key(|(_, addr)| *addr);
    sym_out.sort_by(|a, b| a.1.cmp(&b.1).then(a.0.cmp(&b.0)));

    Ok(LinkedOutput {
        image,
        symbols: sym_out,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::obj::{ObjRelocation, ObjSection, ObjSymbol, RelocTarget};

    fn sec(name: &str, bytes: &[u8]) -> ObjSection {
        ObjSection {
            name: name.to_string(),
            bytes: bytes.to_vec(),
        }
    }

    fn sym(name: &str, section: &str, offset: u32) -> ObjSymbol {
        ObjSymbol {
            name: name.to_string(),
            section: section.to_string(),
            offset,
        }
    }

    fn reloc_symbol(section: &str, offset: u32, target: &str, addend: u32) -> ObjRelocation {
        ObjRelocation {
            section: section.to_string(),
            offset,
            target: RelocTarget::Symbol(target.to_string()),
            addend,
        }
    }

    fn reloc_sec(
        section: &str,
        offset: u32,
        target_section: &str,
        target_offset: u32,
        addend: u32,
    ) -> ObjRelocation {
        ObjRelocation {
            section: section.to_string(),
            offset,
            target: RelocTarget::SectionOffset {
                section: target_section.to_string(),
                offset: target_offset,
            },
            addend,
        }
    }

    fn obj(
        sections: Vec<ObjSection>,
        symbols: Vec<ObjSymbol>,
        relocations: Vec<ObjRelocation>,
    ) -> ObjectFile {
        ObjectFile {
            sections,
            symbols,
            relocations,
        }
    }

    #[test]
    fn links_single_file_text_start_at_entry() {
        // text._start: 12 字节，第二条指令 arg1 引用 message。
        let s = obj(
            vec![
                sec(
                    "text._start",
                    &[
                        0x3E, 0x00, 0x00, 0x00, // setn opcode
                        0x00, 0x00, 0x00, 0x00, // arg1 placeholder -> message
                        0x00, 0x00, 0x00, 0x01, // arg2
                    ],
                ),
                sec("data.message", b"Hi\0"),
            ],
            vec![
                sym("text._start", "text._start", 0),
                sym("_start", "text._start", 0),
                sym("data.message", "data.message", 0),
                sym("message", "data.message", 0),
            ],
            vec![reloc_symbol("text._start", 4, "message", 0)],
        );

        let out = link(vec![s]).unwrap();
        // text._start 在 0x100，data.message 在 0x00200000。
        assert_eq!(&out.image[0x100..0x10C], &[
            0x3E, 0x00, 0x00, 0x00,
            0x00, 0x20, 0x00, 0x00, // message 地址 0x00200000 大端序
            0x00, 0x00, 0x00, 0x01,
        ]);
        assert_eq!(&out.image[0x00200000..0x00200003], b"Hi\0");
        assert!(out.symbols.iter().any(|(n, a)| n == "_start" && *a == 0x100));
        assert!(out.symbols.iter().any(|(n, a)| n == "message" && *a == 0x00200000));
    }

    #[test]
    fn missing_text_start_is_error() {
        let s = obj(vec![sec("text.main", &[0; 12])], vec![], vec![]);
        assert!(link(vec![s]).is_err());
    }

    #[test]
    fn duplicate_section_is_error() {
        let a = obj(vec![sec("text._start", &[0; 12])], vec![], vec![]);
        let b = obj(vec![sec("text._start", &[0; 12])], vec![], vec![]);
        assert!(link(vec![a, b]).is_err());
    }

    #[test]
    fn duplicate_symbol_is_error() {
        let a = obj(
            vec![sec("text._start", &[0; 12])],
            vec![sym("dup", "text._start", 0)],
            vec![],
        );
        let b = obj(
            vec![sec("text.main", &[0; 12])],
            vec![sym("dup", "text.main", 0)],
            vec![],
        );
        assert!(link(vec![a, b]).is_err());
    }

    #[test]
    fn undefined_symbol_is_error() {
        let s = obj(
            vec![sec("text._start", &[0; 12])],
            vec![sym("text._start", "text._start", 0)],
            vec![reloc_symbol("text._start", 4, "missing", 0)],
        );
        assert!(link(vec![s]).is_err());
    }

    #[test]
    fn unknown_section_name_is_error() {
        let s = obj(vec![sec("text._start", &[0; 12]), sec("rodata.x", &[0; 4])], vec![], vec![]);
        assert!(link(vec![s]).is_err());
    }

    #[test]
    fn section_offset_relocation_uses_section_base() {
        // text._start 引用自身 offset 0 + addend 12。
        let s = obj(
            vec![sec("text._start", &[0x4A, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0])],
            vec![sym("text._start", "text._start", 0)],
            vec![reloc_sec("text._start", 4, "text._start", 0, 12)],
        );
        let out = link(vec![s]).unwrap();
        // final = 0x100 + 0 + 12 = 0x10C
        assert_eq!(&out.image[0x104..0x108], &0x0000010Cu32.to_be_bytes());
    }

    #[test]
    fn multiple_text_sections_follow_text_start() {
        let s = obj(
            vec![
                sec("text._start", &[0; 12]),
                sec("text.main", &[0; 8]),
            ],
            vec![
                sym("text._start", "text._start", 0),
                sym("text.main", "text.main", 0),
            ],
            vec![],
        );
        let out = link(vec![s]).unwrap();
        // text._start: 0x100, len 12 -> next align4(0x10C) = 0x10C
        // text.main: 0x10C, len 8
        assert!(out.symbols.iter().any(|(n, a)| n == "text.main" && *a == 0x10C));
    }

    #[test]
    fn data_sections_start_at_data_base() {
        let s = obj(
            vec![
                sec("text._start", &[0; 12]),
                sec("data", &[0; 4]),
                sec("data.extra", &[0; 8]),
            ],
            vec![
                sym("text._start", "text._start", 0),
                sym("data", "data", 0),
                sym("data.extra", "data.extra", 0),
            ],
            vec![],
        );
        let out = link(vec![s]).unwrap();
        assert!(out.symbols.iter().any(|(n, a)| n == "data" && *a == 0x00200000));
        // data len 4 -> align4(0x00200004) = 0x00200004
        assert!(out.symbols.iter().any(|(n, a)| n == "data.extra" && *a == 0x00200004));
    }

    #[test]
    fn cross_file_symbol_resolution() {
        let a = obj(
            vec![sec("text._start", &[0x4C, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0])],
            vec![sym("text._start", "text._start", 0)],
            vec![reloc_symbol("text._start", 4, "print", 0)],
        );
        let b = obj(
            vec![sec("text.print", &[0x4D, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0])],
            vec![sym("text.print", "text.print", 0), sym("print", "text.print", 0)],
            vec![],
        );
        let out = link(vec![a, b]).unwrap();
        // text._start: 0x100, len 12 -> text.print: 0x10C
        assert_eq!(&out.image[0x104..0x108], &0x0000010Cu32.to_be_bytes());
    }

    #[test]
    fn addend_is_added_to_target_address() {
        let s = obj(
            vec![
                sec("text._start", &[0; 12]),
                sec("data.message", b"Hello!\0"),
            ],
            vec![
                sym("text._start", "text._start", 0),
                sym("message", "data.message", 0),
            ],
            vec![reloc_symbol("text._start", 8, "message", 4)],
        );
        let out = link(vec![s]).unwrap();
        // message 在 0x00200000，+4 = 0x00200004
        assert_eq!(&out.image[0x108..0x10C], &0x00200004u32.to_be_bytes());
    }

    #[test]
    fn image_is_at_least_entry_size() {
        // text._start 为空，镜像仍至少 0x100 字节。
        let s = obj(
            vec![sec("text._start", &[])],
            vec![sym("text._start", "text._start", 0)],
            vec![],
        );
        let out = link(vec![s]).unwrap();
        assert!(out.image.len() >= 0x100);
    }
}
