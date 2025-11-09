// src/addr/reg.rs
use crate::{
    device::AddrPort,
    error::CoreError,
    types::Word,
};

/// 寄存器文件设备（最小实现）
/// 包含 0x00-0x1F 共 32 个寄存器
pub struct RegFile {
    regs: [Word; 0x20], // 0x00-0x1F 共 32 个
}

/// 特殊寄存器编号（0x10-0x1F）
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum SpecialReg {
    PC   = 0x10,
    MD   = 0x11,
    SP   = 0x12,
    TM   = 0x13,
    TA1  = 0x14,
    TA2  = 0x15,
    M1   = 0x16,
    M2   = 0x17,
    M3   = 0x18,
    M4   = 0x19,
    RS   = 0x1A,
    EX   = 0x1B,
    BLTS = 0x1C,
    BLTL = 0x1D,
}

impl SpecialReg {
    pub fn from_offset(off: usize) -> Option<Self> {
        match off as u8 {
            0x10 => Some(Self::PC),
            0x11 => Some(Self::MD),
            0x12 => Some(Self::SP),
            0x13 => Some(Self::TM),
            0x14 => Some(Self::TA1),
            0x15 => Some(Self::TA2),
            0x16 => Some(Self::M1),
            0x17 => Some(Self::M2),
            0x18 => Some(Self::M3),
            0x19 => Some(Self::M4),
            0x1A => Some(Self::RS),
            0x1B => Some(Self::EX),
            0x1C => Some(Self::BLTS),
            0x1D => Some(Self::BLTL),
            _ => None,
        }
    }
}

impl RegFile {
    pub fn new() -> Self {
        Self { regs: [0; 0x20] }
    }

    /// 直接访问（调试用）
    pub fn get_raw(&self, off: usize) -> Option<Word> {
        self.regs.get(off).copied()
    }

    pub fn set_raw(&mut self, off: usize, val: Word) -> Result<(), CoreError> {
        if off >= self.regs.len() {
            return Err(CoreError::MemoryError(format!("RegFile offset 0x{:x} OOB", off)));
        }
        self.regs[off] = val;
        Ok(())
    }

    /// 访问特殊寄存器
    pub fn get_special(&self, reg: SpecialReg) -> Word {
        self.regs[reg as usize]
    }

    pub fn set_special(&mut self, reg: SpecialReg, val: Word) -> Result<(), CoreError> {
        self.regs[reg as usize] = val;

        // TODO: 特殊寄存器副作用（如 EX 触发退出、TM 递减中断等）
        // 示例：
        // if reg == SpecialReg::EX {
        //     // 触发退出信号
        // }

        Ok(())
    }
}

impl Default for RegFile {
    fn default() -> Self {
        Self::new()
    }
}

impl AddrPort for RegFile {
    fn read(&self, off: usize) -> Result<Word, CoreError> {
        self.regs.get(off).copied().ok_or_else(|| {
            CoreError::MemoryError(format!("RegFile offset 0x{:x} out of bounds", off))
        })
    }

    fn write(&mut self, off: usize, val: Word) -> Result<(), CoreError> {
        if off >= self.regs.len() {
            return Err(CoreError::MemoryError(format!("RegFile offset 0x{:x} out of bounds", off)));
        }
        self.regs[off] = val;
        Ok(())
    }

    fn len(&self) -> usize {
        self.regs.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_regfile_creation() {
        let regs = RegFile::new();
        assert_eq!(regs.len(), 0x20);

        // 检查所有寄存器初始化为 0
        for i in 0..0x20 {
            assert_eq!(regs.read(i).unwrap(), 0);
        }
    }

    #[test]
    fn test_regfile_read_write() {
        let mut regs = RegFile::new();

        // 测试普通寄存器读写
        assert!(regs.write(0x00, 0x12345678).is_ok());
        assert!(regs.write(0x0F, 0xABCDEF00).is_ok());
        assert!(regs.write(0x1F, 0xFFFFFFFF).is_ok());

        assert_eq!(regs.read(0x00).unwrap(), 0x12345678);
        assert_eq!(regs.read(0x0F).unwrap(), 0xABCDEF00);
        assert_eq!(regs.read(0x1F).unwrap(), 0xFFFFFFFF);
    }

    #[test]
    fn test_regfile_bounds() {
        let mut regs = RegFile::new();

        // 测试越界访问
        assert!(regs.read(0x20).is_err());
        assert!(regs.write(0x20, 0x12345678).is_err());
    }

    #[test]
    fn test_special_registers() {
        let mut regs = RegFile::new();

        // 测试特殊寄存器
        assert!(regs.set_special(SpecialReg::PC, 0x00100000).is_ok());
        assert!(regs.set_special(SpecialReg::SP, 0x00300000).is_ok());
        assert!(regs.set_special(SpecialReg::EX, 0x00000001).is_ok());

        assert_eq!(regs.get_special(SpecialReg::PC), 0x00100000);
        assert_eq!(regs.get_special(SpecialReg::SP), 0x00300000);
        assert_eq!(regs.get_special(SpecialReg::EX), 0x00000001);

        // 测试特殊寄存器编号转换
        assert_eq!(SpecialReg::from_offset(0x10), Some(SpecialReg::PC));
        assert_eq!(SpecialReg::from_offset(0x1B), Some(SpecialReg::EX));
        assert_eq!(SpecialReg::from_offset(0x0F), None);
    }

    #[test]
    fn test_raw_access() {
        let mut regs = RegFile::new();

        // 测试原始访问方法
        assert!(regs.set_raw(0x05, 0x12345678).is_ok());
        assert_eq!(regs.get_raw(0x05), Some(0x12345678));
        assert_eq!(regs.get_raw(0x20), None); // 越界
    }

    #[test]
    fn test_hello_world_level() {
        // 最小化的 hello world 级别测试
        let regs = RegFile::new();
        assert_eq!(regs.len(), 32); // 0x20 个寄存器
        assert!(!regs.is_empty());

        // 基本功能验证
        let mut regs = RegFile::new();
        assert!(regs.write(0, 42).is_ok());
        assert_eq!(regs.read(0).unwrap(), 42);
    }
}