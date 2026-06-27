//! ShyISA `.sobj` object 文件二进制格式解析。
//!
//! 格式定义见 `ObjFormat.md` 第 12 节。所有 `u32` 使用大端序，所有链表指针都是
//! 文件内绝对偏移，`0` 表示 `NULL`，`*_c_str` 是以 `0x00` 结尾的 UTF-8 字节串。

use anyhow::{Context, Result, bail};

/// `.sobj` 文件头 magic。
pub const MAGIC: u32 = 0x66CCFF00;

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
pub enum RelocTarget {
    Symbol(String),
    SectionOffset { section: String, offset: u32 },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjRelocation {
    pub section: String,
    pub offset: u32,
    pub target: RelocTarget,
    pub addend: u32,
}

/// 解析后的 object 文件，对应 `ObjFormat.md` 中的 `ObjectFile`。
#[derive(Debug, Clone)]
pub struct ObjectFile {
    pub sections: Vec<ObjSection>,
    pub symbols: Vec<ObjSymbol>,
    pub relocations: Vec<ObjRelocation>,
}

fn read_u32(buf: &[u8], off: usize) -> Result<u32> {
    let s = buf.get(off..off + 4).context("object file truncated")?;
    Ok(u32::from_be_bytes([s[0], s[1], s[2], s[3]]))
}

/// 从 `off` 处读取以 `0x00` 结尾的 UTF-8 字节串，返回字符串和结束符之后的偏移。
fn read_cstr(buf: &[u8], off: usize) -> Result<(String, usize)> {
    let rest = buf.get(off..).context("object file truncated")?;
    let end = rest
        .iter()
        .position(|&b| b == 0)
        .context("unterminated c-string in object file")?;
    let s = std::str::from_utf8(&rest[..end])
        .context("non-utf8 string in object file")?
        .to_string();
    Ok((s, off + end + 1))
}

impl ObjectFile {
    /// 从 `.sobj` 二进制字节流解析出 `ObjectFile`。
    pub fn from_bytes(buf: &[u8]) -> Result<Self> {
        if buf.len() < 16 {
            bail!("object file too small for header: {} bytes", buf.len());
        }
        let magic = read_u32(buf, 0)?;
        if magic != MAGIC {
            bail!("bad object magic: 0x{magic:08X}, expected 0x{MAGIC:08X}");
        }
        let section_start = read_u32(buf, 4)? as usize;
        let symbol_start = read_u32(buf, 8)? as usize;
        let relocation_start = read_u32(buf, 12)? as usize;

        let mut sections = Vec::new();
        let mut cur = section_start as u32;
        while cur != 0 {
            let off = cur as usize;
            let next = read_u32(buf, off)?;
            let (name, p) = read_cstr(buf, off + 4)?;
            let byte_size = read_u32(buf, p)? as usize;
            let bytes = buf
                .get(p + 4..p + 4 + byte_size)
                .context("section bytes truncated")?
                .to_vec();
            sections.push(ObjSection { name, bytes });
            cur = next;
        }

        let mut symbols = Vec::new();
        cur = symbol_start as u32;
        while cur != 0 {
            let off = cur as usize;
            let next = read_u32(buf, off)?;
            let offset = read_u32(buf, off + 4)?;
            let (section, p) = read_cstr(buf, off + 8)?;
            let (name, _) = read_cstr(buf, p)?;
            symbols.push(ObjSymbol {
                name,
                section,
                offset,
            });
            cur = next;
        }

        let mut relocations = Vec::new();
        cur = relocation_start as u32;
        while cur != 0 {
            let off = cur as usize;
            let next = read_u32(buf, off)?;
            let offset = read_u32(buf, off + 4)?;
            let addend = read_u32(buf, off + 8)?;
            let target_kind = read_u32(buf, off + 12)?;
            let (section, p) = read_cstr(buf, off + 16)?;
            let target = match target_kind {
                1 => {
                    let (name, _) = read_cstr(buf, p)?;
                    RelocTarget::Symbol(name)
                }
                0 => {
                    let target_offset = read_u32(buf, p)?;
                    let (target_section, _) = read_cstr(buf, p + 4)?;
                    RelocTarget::SectionOffset {
                        section: target_section,
                        offset: target_offset,
                    }
                }
                other => bail!("invalid target_kind: {other}, expected 0 or 1"),
            };
            relocations.push(ObjRelocation {
                section,
                offset,
                target,
                addend,
            });
            cur = next;
        }

        Ok(Self {
            sections,
            symbols,
            relocations,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试辅助：把内存中的 section/symbol/relocation 序列化为 `.sobj` 字节流。
    fn build_sobj(
        sections: &[ObjSection],
        symbols: &[ObjSymbol],
        relocations: &[ObjRelocation],
    ) -> Vec<u8> {
        fn push_u32(buf: &mut Vec<u8>, v: u32) {
            buf.extend_from_slice(&v.to_be_bytes());
        }
        fn push_cstr(buf: &mut Vec<u8>, s: &str) {
            buf.extend_from_slice(s.as_bytes());
            buf.push(0);
        }

        let mut buf = Vec::new();
        push_u32(&mut buf, MAGIC);
        push_u32(&mut buf, 0); // section_start placeholder
        push_u32(&mut buf, 0); // symbol_start placeholder
        push_u32(&mut buf, 0); // relocation_start placeholder

        let section_start = if sections.is_empty() { 0 } else { buf.len() as u32 };
        for (i, s) in sections.iter().enumerate() {
            let node_len = 4 + s.name.len() + 1 + 4 + s.bytes.len();
            let next = if i + 1 < sections.len() {
                (buf.len() + node_len) as u32
            } else {
                0
            };
            push_u32(&mut buf, next);
            push_cstr(&mut buf, &s.name);
            push_u32(&mut buf, s.bytes.len() as u32);
            buf.extend_from_slice(&s.bytes);
        }

        let symbol_start = if symbols.is_empty() { 0 } else { buf.len() as u32 };
        for (i, sym) in symbols.iter().enumerate() {
            let node_len = 4 + 4 + sym.section.len() + 1 + sym.name.len() + 1;
            let next = if i + 1 < symbols.len() {
                (buf.len() + node_len) as u32
            } else {
                0
            };
            push_u32(&mut buf, next);
            push_u32(&mut buf, sym.offset);
            push_cstr(&mut buf, &sym.section);
            push_cstr(&mut buf, &sym.name);
        }

        let relocation_start = if relocations.is_empty() {
            0
        } else {
            buf.len() as u32
        };
        for (i, r) in relocations.iter().enumerate() {
            let target_len = match &r.target {
                RelocTarget::Symbol(name) => name.len() + 1,
                RelocTarget::SectionOffset { section, .. } => 4 + section.len() + 1,
            };
            let node_len = 4 + 4 + 4 + 4 + r.section.len() + 1 + target_len;
            let next = if i + 1 < relocations.len() {
                (buf.len() + node_len) as u32
            } else {
                0
            };
            push_u32(&mut buf, next);
            push_u32(&mut buf, r.offset);
            push_u32(&mut buf, r.addend);
            match &r.target {
                RelocTarget::Symbol(name) => {
                    push_u32(&mut buf, 1);
                    push_cstr(&mut buf, &r.section);
                    push_cstr(&mut buf, name);
                }
                RelocTarget::SectionOffset { section, offset } => {
                    push_u32(&mut buf, 0);
                    push_cstr(&mut buf, &r.section);
                    push_u32(&mut buf, *offset);
                    push_cstr(&mut buf, section);
                }
            }
            let _ = i;
        }

        buf[4..8].copy_from_slice(&section_start.to_be_bytes());
        buf[8..12].copy_from_slice(&symbol_start.to_be_bytes());
        buf[12..16].copy_from_slice(&relocation_start.to_be_bytes());
        buf
    }

    #[test]
    fn parses_empty_object() {
        let buf = build_sobj(&[], &[], &[]);
        let obj = ObjectFile::from_bytes(&buf).unwrap();
        assert!(obj.sections.is_empty());
        assert!(obj.symbols.is_empty());
        assert!(obj.relocations.is_empty());
    }

    #[test]
    fn rejects_bad_magic() {
        let mut buf = build_sobj(&[], &[], &[]);
        buf[0..4].copy_from_slice(&0xDEADBEEFu32.to_be_bytes());
        assert!(ObjectFile::from_bytes(&buf).is_err());
    }

    #[test]
    fn parses_sections_symbols_relocations() {
        let sections = vec![
            ObjSection {
                name: "text._start".to_string(),
                bytes: vec![0x20, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00],
            },
            ObjSection {
                name: "data.message".to_string(),
                bytes: b"Hello!\0".to_vec(),
            },
        ];
        let symbols = vec![
            ObjSymbol {
                name: "text._start".to_string(),
                section: "text._start".to_string(),
                offset: 0,
            },
            ObjSymbol {
                name: "_start".to_string(),
                section: "text._start".to_string(),
                offset: 0,
            },
            ObjSymbol {
                name: "message".to_string(),
                section: "data.message".to_string(),
                offset: 0,
            },
        ];
        let relocations = vec![
            ObjRelocation {
                section: "text._start".to_string(),
                offset: 4,
                target: RelocTarget::Symbol("message".to_string()),
                addend: 0,
            },
            ObjRelocation {
                section: "text._start".to_string(),
                offset: 8,
                target: RelocTarget::SectionOffset {
                    section: "text._start".to_string(),
                    offset: 0,
                },
                addend: 12,
            },
        ];

        let buf = build_sobj(&sections, &symbols, &relocations);
        let obj = ObjectFile::from_bytes(&buf).unwrap();

        assert_eq!(obj.sections, sections);
        assert_eq!(obj.symbols, symbols);
        assert_eq!(obj.relocations, relocations);
    }
}
