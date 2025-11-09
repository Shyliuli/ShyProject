// src/addr/addr.rs

/// 带类型的地址值
#[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct Addr(u32);

/// 地址分类
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum AddrKind {
    Reg,      // 寄存器 0x00-0x1D
    Opcode,   // 指令操作码 0x20-0x54（不可读写内存）
    Io,       // I/O 端口 0x70-0xFF
    Vram,     // 显存 0x0000_0100-0x0010_00FF
    Ram,      // 主存 ≥0x0010_0100
    Reserved, // 其他未映射区域
}

// 地址边界常量
pub const REG_START: u32 = 0x00;
pub const REG_END:   u32 = 0x1D;

pub const OPC_START: u32 = 0x20;
pub const OPC_END:   u32 = 0x54;

pub const IO_START:  u32 = 0x70;
pub const IO_END:    u32 = 0xFF;

pub const VRAM_START: u32 = 0x0000_0100;
pub const VRAM_MAX_END: u32 = 0x0010_00FF; // 规范允许的上限

pub const RAM_BASE:  u32 = 0x0010_0100;

impl Addr {
    /// 从原始 u32 创建地址
    #[inline]
    pub const fn new(raw: u32) -> Self {
        Self(raw)
    }

    /// 获取原始 u32 值
    #[inline]
    pub const fn raw(self) -> u32 {
        self.0
    }

    /// 判断地址类型
    pub fn kind(self) -> AddrKind {
        match self.0 {
            REG_START..=REG_END => AddrKind::Reg,
            OPC_START..=OPC_END => AddrKind::Opcode,
            IO_START..=IO_END   => AddrKind::Io,
            VRAM_START..=VRAM_MAX_END => AddrKind::Vram,
            a if a >= RAM_BASE  => AddrKind::Ram,
            _ => AddrKind::Reserved,
        }
    }

    /// 计算设备内偏移（以字为单位）
    /// 只在对应 kind 下有意义；其他返回 0
    pub fn offset(self) -> usize {
        match self.kind() {
            AddrKind::Reg  => (self.0 - REG_START) as usize,
            AddrKind::Io   => (self.0 - IO_START) as usize,
            AddrKind::Vram => (self.0 - VRAM_START) as usize,
            AddrKind::Ram  => (self.0 - RAM_BASE) as usize,
            _ => 0,
        }
    }

    /// 从 RAM 索引构造地址（兼容老测试）
    #[inline]
    pub fn from_ram_idx(idx: usize) -> Self {
        Self::new(RAM_BASE + idx as u32)
    }
}

impl From<u32> for Addr {
    fn from(raw: u32) -> Self {
        Self::new(raw)
    }
}

impl From<Addr> for u32 {
    fn from(a: Addr) -> u32 {
        a.raw()
    }
}

// 添加用于格式化的 trait 实现
impl std::fmt::LowerHex for Addr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:08x}", self.0)
    }
}

impl std::fmt::UpperHex for Addr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:08X}", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addr_kind_detection() {
        assert_eq!(Addr::new(0x00).kind(), AddrKind::Reg);
        assert_eq!(Addr::new(0x10).kind(), AddrKind::Reg); // PC
        assert_eq!(Addr::new(0x1D).kind(), AddrKind::Reg);
        assert_eq!(Addr::new(0x20).kind(), AddrKind::Opcode);
        assert_eq!(Addr::new(0x54).kind(), AddrKind::Opcode);
        assert_eq!(Addr::new(0x70).kind(), AddrKind::Io);
        assert_eq!(Addr::new(0xFF).kind(), AddrKind::Io);
        assert_eq!(Addr::new(0x0000_0100).kind(), AddrKind::Vram);
        assert_eq!(Addr::new(0x0010_0100).kind(), AddrKind::Ram);
    }

    #[test]
    fn addr_offset_calculation() {
        assert_eq!(Addr::new(0x00).offset(), 0);
        assert_eq!(Addr::new(0x10).offset(), 0x10); // PC
        assert_eq!(Addr::new(0x70).offset(), 0);
        assert_eq!(Addr::new(0x80).offset(), 0x10); // ASCII 'P'
        assert_eq!(Addr::new(0x0000_0100).offset(), 0);
        assert_eq!(Addr::new(0x0010_0100).offset(), 0);
        assert_eq!(Addr::new(0x0010_0105).offset(), 5);
    }
}