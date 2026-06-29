//! ShyISA 链接器核心：按 `ObjFormat.md` 第 10 节的默认规则布局 section、解析
//! symbol、回填 relocation，并生成 `.sfs` raw 内存镜像。
//!
//! 默认布局规则：
//! - `text._start` 必须存在，放到程序入口地址 `0x00000100`。
//! - 其他 `text.*` section 接在 `text._start` 后面，按输入顺序依次放置。
//! - `data` 和 `data.*` section 接在所有 `text.*` section 后面，按输入顺序依次放置。
//! - 其他 section 名暂不定义默认布局，链接器报错。
//! - section 起始地址按 4 字节对齐。

use std::collections::{HashMap, HashSet};

use anyhow::{Context, Result, bail};

use crate::obj::{ObjectFile, RelocTarget};

/// 程序入口地址，`text._start` 必须放到这里。
pub const ENTRY: u32 = 0x00000100;
/// 没有任何 object 声明 `#![mem(...)]` 时写入 `.sfs` 的默认内存提示。
pub const DEFAULT_MEM_HINT: u32 = 32 * 1024 * 1024;
/// 没有任何 object 声明 `#![stack(...)]` 时写入 `.sfs` 的默认栈提示。
pub const DEFAULT_STACK_HINT: u32 = 4 * 1024;

/// 链接输出。
pub struct LinkedOutput {
    /// `.sfs` raw 内存镜像，字节偏移即地址。
    pub image: Vec<u8>,
    /// symbol 名 -> 最终绝对地址，按地址升序排列。
    pub symbols: Vec<(String, u32)>,
}

fn parse_shy_method_symbol(name: &str) -> Option<(&str, &str)> {
    let rest = name.strip_prefix("____")?;
    let (ty, method) = rest.split_once("__")?;
    if ty.is_empty() || method.is_empty() {
        return None;
    }
    Some((ty, method))
}

/// Emits best-effort RAII diagnostics using only linked symbol names.
///
/// This intentionally does not change link success: without type metadata in `.sobj`,
/// the linker can only infer that an object probably uses `Foo` when it references a
/// mangled `____Foo__method` symbol.
pub fn raii_drop_warnings(files: &[(&str, &ObjectFile)]) -> Vec<String> {
    let mut drop_defs = HashSet::new();
    for (_, file) in files {
        for sym in &file.symbols {
            if let Some((ty, "drop")) = parse_shy_method_symbol(&sym.name) {
                drop_defs.insert(ty.to_string());
            }
        }
    }

    let mut warnings = Vec::new();
    for (name, file) in files {
        let mut method_refs: HashMap<String, HashSet<String>> = HashMap::new();
        let mut drop_known = HashSet::new();

        for sym in &file.symbols {
            if let Some((ty, "drop")) = parse_shy_method_symbol(&sym.name) {
                drop_known.insert(ty.to_string());
            }
        }

        for reloc in &file.relocations {
            let RelocTarget::Symbol(target) = &reloc.target else {
                continue;
            };
            let Some((ty, method)) = parse_shy_method_symbol(target) else {
                continue;
            };
            if method == "drop" {
                drop_known.insert(ty.to_string());
            } else {
                method_refs
                    .entry(ty.to_string())
                    .or_default()
                    .insert(method.to_string());
            }
        }

        let mut types: Vec<_> = method_refs.keys().cloned().collect();
        types.sort();
        for ty in types {
            if !drop_defs.contains(&ty) || drop_known.contains(&ty) {
                continue;
            }

            let mut methods: Vec<_> = method_refs[&ty].iter().cloned().collect();
            methods.sort();
            let refs = methods
                .into_iter()
                .map(|method| format!("____{ty}__{method}"))
                .collect::<Vec<_>>()
                .join(", ");
            warnings.push(format!(
                "{name}: references {refs} but not ____{ty}__drop; another object defines ____{ty}__drop. Did you forget `impl {ty} {{ void drop(self *s); }}` in the header?"
            ));
        }
    }

    warnings
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
    let mut mem_hint_sum = 0u32;
    let mut stack_hint_sum = 0u32;
    let mut has_mem_hint = false;
    let mut has_stack_hint = false;

    for file in &files {
        if let Some(v) = file.mem_hint {
            has_mem_hint = true;
            mem_hint_sum = mem_hint_sum
                .checked_add(v)
                .context("combined mem hint exceeds u32::MAX")?;
        }
        if let Some(v) = file.stack_hint {
            has_stack_hint = true;
            stack_hint_sum = stack_hint_sum
                .checked_add(v)
                .context("combined stack hint exceeds u32::MAX")?;
        }
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
        if s.name != "text._start" && !is_text(&s.name) && !is_data(&s.name) {
            bail!(
                "no default layout for section `{}`: only `text.*` and `data`/`data.*` are supported",
                s.name
            );
        }
    }

    for s in &sections {
        if s.name == "text._start" {
            continue;
        }
        if is_text(&s.name) {
            bases.insert(s.name.clone(), text_cur);
            text_cur = align4(text_cur + s.bytes.len() as u32);
        }
    }

    let mut data_cur = text_cur;
    for s in &sections {
        if is_data(&s.name) {
            bases.insert(s.name.clone(), data_cur);
            data_cur = align4(data_cur + s.bytes.len() as u32);
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
    let mem_hint = if has_mem_hint {
        mem_hint_sum
    } else {
        DEFAULT_MEM_HINT
    };
    let stack_hint = if has_stack_hint {
        stack_hint_sum
    } else {
        DEFAULT_STACK_HINT
    };
    image[4..8].copy_from_slice(&mem_hint.to_be_bytes());
    image[8..12].copy_from_slice(&stack_hint.to_be_bytes());
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
            mem_hint: None,
            stack_hint: None,
            sections,
            symbols,
            relocations,
        }
    }

    fn obj_with_hints(
        mem_hint: Option<u32>,
        stack_hint: Option<u32>,
        sections: Vec<ObjSection>,
        symbols: Vec<ObjSymbol>,
        relocations: Vec<ObjRelocation>,
    ) -> ObjectFile {
        ObjectFile {
            mem_hint,
            stack_hint,
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
        // text._start 在 0x100，data.message 紧跟在 text 后。
        assert_eq!(&out.image[0x100..0x10C], &[
            0x3E, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x01, 0x0C, // message 地址 0x0000010C 大端序
            0x00, 0x00, 0x00, 0x01,
        ]);
        assert_eq!(&out.image[0x10C..0x10F], b"Hi\0");
        assert!(out.symbols.iter().any(|(n, a)| n == "_start" && *a == 0x100));
        assert!(out.symbols.iter().any(|(n, a)| n == "message" && *a == 0x10C));
        assert_eq!(&out.image[0x04..0x08], &DEFAULT_MEM_HINT.to_be_bytes());
        assert_eq!(&out.image[0x08..0x0C], &DEFAULT_STACK_HINT.to_be_bytes());
    }

    #[test]
    fn metadata_hints_sum_across_objects() {
        let a = obj_with_hints(
            Some(10 * 1024 * 1024),
            None,
            vec![sec("text._start", &[0; 12])],
            vec![sym("text._start", "text._start", 0)],
            vec![],
        );
        let b = obj_with_hints(
            Some(2 * 1024 * 1024),
            Some(8 * 1024),
            vec![sec("text.main", &[0; 12])],
            vec![sym("text.main", "text.main", 0)],
            vec![],
        );

        let out = link(vec![a, b]).unwrap();
        assert_eq!(
            &out.image[0x04..0x08],
            &(12 * 1024 * 1024u32).to_be_bytes()
        );
        assert_eq!(&out.image[0x08..0x0C], &(8 * 1024u32).to_be_bytes());
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
    fn data_sections_follow_all_text_sections() {
        let s = obj(
            vec![
                sec("text._start", &[0; 12]),
                sec("text.main", &[0; 8]),
                sec("data", &[0; 4]),
                sec("data.extra", &[0; 8]),
            ],
            vec![
                sym("text._start", "text._start", 0),
                sym("text.main", "text.main", 0),
                sym("data", "data", 0),
                sym("data.extra", "data.extra", 0),
            ],
            vec![],
        );
        let out = link(vec![s]).unwrap();
        assert!(out.symbols.iter().any(|(n, a)| n == "text.main" && *a == 0x10C));
        assert!(out.symbols.iter().any(|(n, a)| n == "data" && *a == 0x114));
        // data len 4 -> align4(0x00000118) = 0x00000118
        assert!(out.symbols.iter().any(|(n, a)| n == "data.extra" && *a == 0x118));
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
        // message 在 0x10C，+4 = 0x110
        assert_eq!(&out.image[0x108..0x10C], &0x00000110u32.to_be_bytes());
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

    #[test]
    fn warns_when_object_references_method_but_not_known_drop() {
        let main = obj(
            vec![sec("text._start", &[0; 12])],
            vec![sym("text._start", "text._start", 0)],
            vec![reloc_symbol("text._start", 4, "____Foo__new", 0)],
        );
        let foo = obj(
            vec![sec("text.foo_drop", &[0; 12])],
            vec![sym("____Foo__drop", "text.foo_drop", 0)],
            vec![],
        );

        let warnings = raii_drop_warnings(&[("main.sobj", &main), ("foo.sobj", &foo)]);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("main.sobj"));
        assert!(warnings[0].contains("____Foo__new"));
        assert!(warnings[0].contains("____Foo__drop"));
    }

    #[test]
    fn no_raii_warning_when_object_already_references_drop() {
        let main = obj(
            vec![sec("text._start", &[0; 12])],
            vec![sym("text._start", "text._start", 0)],
            vec![
                reloc_symbol("text._start", 4, "____Foo__new", 0),
                reloc_symbol("text._start", 8, "____Foo__drop", 0),
            ],
        );
        let foo = obj(
            vec![sec("text.foo_drop", &[0; 12])],
            vec![sym("____Foo__drop", "text.foo_drop", 0)],
            vec![],
        );

        let warnings = raii_drop_warnings(&[("main.sobj", &main), ("foo.sobj", &foo)]);
        assert!(warnings.is_empty());
    }

    #[test]
    fn no_raii_warning_without_drop_definition() {
        let main = obj(
            vec![sec("text._start", &[0; 12])],
            vec![sym("text._start", "text._start", 0)],
            vec![reloc_symbol("text._start", 4, "____Foo__new", 0)],
        );

        let warnings = raii_drop_warnings(&[("main.sobj", &main)]);
        assert!(warnings.is_empty());
    }
}
