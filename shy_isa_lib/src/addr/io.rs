// src/addr/io.rs
use crate::{
    device::AddrPort,
    error::CoreError,
    types::Word,
};

/// I/O 端口设备（最小实现）
/// 包含 0x70-0xFF 共 144 个端口
pub struct Io {
    ports: [Word; 0x90], // 0x70-0xFF 共 144 个
}

impl Io {
    pub fn new() -> Self {
        Self { ports: [0; 0x90] }
    }

    pub fn get_raw(&self, off: usize) -> Option<Word> {
        self.ports.get(off).copied()
    }

    pub fn set_raw(&mut self, off: usize, val: Word) -> Result<(), CoreError> {
        if off >= self.ports.len() {
            return Err(CoreError::MemoryError(format!("IO offset 0x{:x} OOB", off)));
        }
        self.ports[off] = val;
        Ok(())
    }

    /// 获取端口的实际地址（0x70-0xFF）
    pub fn port_addr(offset: usize) -> u32 {
        0x70 + offset as u32
    }

    /// 从实际端口地址计算偏移
    pub fn port_offset(addr: u32) -> usize {
        (addr - 0x70) as usize
    }
}

impl Default for Io {
    fn default() -> Self {
        Self::new()
    }
}

impl AddrPort for Io {
    fn read(&self, off: usize) -> Result<Word, CoreError> {
        self.ports.get(off).copied().ok_or_else(|| {
            CoreError::MemoryError(format!("IO offset 0x{:x} out of bounds", off))
        })
    }

    fn write(&mut self, off: usize, val: Word) -> Result<(), CoreError> {
        if off >= self.ports.len() {
            return Err(CoreError::MemoryError(format!("IO offset 0x{:x} out of bounds", off)));
        }
        self.ports[off] = val;

        // TODO: I/O 副作用（如键盘中断、声音输出等）

        Ok(())
    }

    fn len(&self) -> usize {
        self.ports.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_io_creation() {
        let io = Io::new();
        assert_eq!(io.len(), 0x90); // 144 个端口

        // 检查所有端口初始化为 0
        for i in 0..0x90 {
            assert_eq!(io.read(i).unwrap(), 0);
        }
    }

    #[test]
    fn test_io_read_write() {
        let mut io = Io::new();

        // 测试端口读写
        assert!(io.write(0x00, 0x12345678).is_ok()); // 0x70 端口
        assert!(io.write(0x10, 0xABCDEF00).is_ok()); // 0x80 端口
        assert!(io.write(0x8F, 0xFFFFFFFF).is_ok()); // 0xFF 端口

        assert_eq!(io.read(0x00).unwrap(), 0x12345678);
        assert_eq!(io.read(0x10).unwrap(), 0xABCDEF00);
        assert_eq!(io.read(0x8F).unwrap(), 0xFFFFFFFF);
    }

    #[test]
    fn test_io_bounds() {
        let mut io = Io::new();

        // 测试越界访问
        assert!(io.read(0x90).is_err());
        assert!(io.write(0x90, 0x12345678).is_err());
    }

    #[test]
    fn test_port_address_conversion() {
        // 测试端口地址转换
        assert_eq!(Io::port_addr(0x00), 0x70);
        assert_eq!(Io::port_addr(0x10), 0x80);
        assert_eq!(Io::port_addr(0x8F), 0xFF);

        assert_eq!(Io::port_offset(0x70), 0x00);
        assert_eq!(Io::port_offset(0x80), 0x10);
        assert_eq!(Io::port_offset(0xFF), 0x8F);
    }

    #[test]
    fn test_raw_access() {
        let mut io = Io::new();

        // 测试原始访问方法
        assert!(io.set_raw(0x10, 0x12345678).is_ok());
        assert_eq!(io.get_raw(0x10), Some(0x12345678));
        assert_eq!(io.get_raw(0x90), None); // 越界
    }

    #[test]
    fn test_hello_world_level() {
        // 最小化的 hello world 级别测试
        let io = Io::new();
        assert_eq!(io.len(), 144); // 0x90 个端口
        assert!(!io.is_empty());

        // 基本功能验证
        let mut io = Io::new();
        assert!(io.write(0, 0x42).is_ok());
        assert_eq!(io.read(0).unwrap(), 0x42);
    }
}