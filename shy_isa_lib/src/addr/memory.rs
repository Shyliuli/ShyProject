// src/addr/memory.rs
use crate::{
    device::AddrPort,
    error::CoreError,
    types::Word,
    isa_def::MEM_CODE_START,
};

/// Memory 设备实现（主存）
pub struct Memory {
    values: Vec<Word>,
}

impl Memory {
    /// 构造函数
    pub fn new(nsize: usize) -> Self {
        Self {
            values: vec![0; nsize],
        }
    }


    /// 检查地址边界
    fn check_bounds(&self, addr: usize) -> Result<(), CoreError> {
        if addr >= self.values.len() {
            Err(CoreError::MemoryError(format!(
                "Address 0x{:08x} out of bounds (len=0x{:x})",
                addr as u32+MEM_CODE_START,
                self.values.len()
            )))
        } else {
            Ok(())
        }
    }

    /// 不安全写入（无边界检查）
    pub fn load_unsafe(&mut self, addr: usize, value: Word) {
        if addr < self.values.len() {
            self.values[addr] = value;
        }
    }

    /// 不安全读取（无边界检查）
    pub fn read_unsafe(&self, addr: usize) -> Word {
        if addr < self.values.len() {
            self.values[addr]
        } else {
            0 // 默认返回 0，而不是 panic
        }
    }




    /// 直接访问底层数据（测试/调试用）
    pub fn data(&self) -> &[Word] {
        &self.values
    }

    pub fn data_mut(&mut self) -> &mut [Word] {
        &mut self.values
    }
}

impl AddrPort for Memory {
    /// 读取设备内偏移处的字（实现 addr_port trait）
    fn read(&self, off: usize) -> Result<Word, CoreError> {
        self.check_bounds(off)?;
        Ok(self.values[off])
    }

    /// 写入设备内偏移处的字（实现 addr_port trait）
    fn write(&mut self, off: usize, val: Word) -> Result<(), CoreError> {
        self.check_bounds(off)?;
        self.values[off] = val;
        Ok(())
    }

    /// 设备容量（实现 addr_port trait）
    fn len(&self) -> usize {
        self.values.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_memory_new() {
        let mem = Memory::new(100);
        assert_eq!(mem.len(), 100);
    }

    #[test]
    fn test_hello_world_level() {
        // 最小化的 hello world 级别测试
        let mut mem = Memory::new(64);
        assert!(mem.write(0, 42).is_ok());
        assert_eq!(mem.read(0).unwrap(), 42);
        assert_eq!(mem.len(), 64);
        assert!(!mem.is_empty());
    }
}