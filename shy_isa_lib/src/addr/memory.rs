// src/addr/memory.rs
use crate::{
    device::AddrPort,
    error::CoreError,
    types::Word,
};

/// Memory 设备实现（主存）
/// 继承原有 memory.rs 的所有功能，并实现 addr_port trait
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

    /// 从镜像文件加载
    pub fn load_img(&mut self, file_name: &str) -> Result<(), CoreError> {
        let img = std::fs::read(file_name)
            .map_err(|e| CoreError::IOError(format!("Failed to read image file: {}", e)))?;

        // 检查镜像大小是否正确
        if img.len() != self.values.len() * 4 {
            return Err(CoreError::IOError("Image size mismatch".into()));
        }

        self.values = img
            .chunks_exact(4)
            .map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
            .collect();
        Ok(())
    }

    /// 检查地址边界
    fn check_bounds(&self, addr: usize) -> Result<(), CoreError> {
        if addr >= self.values.len() {
            Err(CoreError::MemoryError(format!(
                "Address 0x{:08x} out of bounds (len=0x{:x})",
                addr,
                self.values.len()
            )))
        } else {
            Ok(())
        }
    }

    /// 加载字到内存（兼容老接口）
    pub fn load(&mut self, addr: usize, value: Word) -> Result<(), CoreError> {
        self.check_bounds(addr)?;
        self.values[addr] = value;
        Ok(())
    }

    /// 从内存读取字（兼容老接口）
    pub fn read(&self, addr: usize) -> Result<Word, CoreError> {
        self.check_bounds(addr)?;
        Ok(self.values[addr])
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

    /// 保存到镜像文件
    pub fn to_img(&self, file_name: &str) -> Result<(), CoreError> {
        std::fs::write(file_name, self.to_u8_vec())
            .map_err(|e| CoreError::IOError(format!("Failed to write image file: {}", e)))
    }

    /// 转换为字节数组
    fn to_u8_vec(&self) -> Vec<u8> {
        self.values
            .iter()
            .flat_map(|&word| word.to_be_bytes()) // 大端序
            .collect()
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