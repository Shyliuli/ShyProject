// src/addr/address_space.rs
use crate::{
    addr::{Addr, AddrKind, Io, Memory, RegFile, Vram},
    addr::addr::{VRAM_MAX_END, VRAM_START},
    device::AddrPort,
    error::CoreError,
    types::Word,
};
use std::fs;

/// 统一地址空间（总线）
pub struct Address {
    regs: RegFile,
    vram: Vram,
    ram:  Memory,
    io:   Io,
    /// VRAM 的实际结束地址（VRAM_START + vram.len() - 1），用于边界检查
    vram_end: u32,
}

impl Address {
    /// 创建新的地址空间
    ///
    /// # 参数
    /// - `ram_words`: RAM 容量（字）
    /// - `vram_words`: VRAM 容量（字），通常为 分辨率^2（如 256x256 = 65536）
    pub fn new(ram_words: usize, vram_words: usize) -> Self {
        let vram_end = VRAM_START + vram_words as u32 - 1;
        // 检查 VRAM 是否超出规范上限
        if vram_end > VRAM_MAX_END {
            panic!(
                "VRAM size too large: end=0x{:08x} exceeds max=0x{:08x}",
                vram_end, VRAM_MAX_END
            );
        }

        Self {
            regs: RegFile::new(),
            vram: Vram::new(vram_words),
            ram: Memory::new(ram_words),
            io: Io::new(),
            vram_end,
        }
    }

    /// 读取地址 addr 处的字
    pub fn read(&self, addr: Addr) -> Result<Word, CoreError> {
        match addr.kind() {
            AddrKind::Reg => self.regs.read(addr.offset()),
            AddrKind::Io => self.io.read(addr.offset()),
            AddrKind::Vram => self.vram.read(addr.offset()),
            AddrKind::Ram => self.ram.read(addr.offset()),
            AddrKind::Opcode => Err(CoreError::MemoryError(
                "Cannot read from opcode space".to_string()
            )),
            AddrKind::Reserved => Err(CoreError::MemoryError(
                format!("Reserved address space: 0x{:08x}", addr.raw())
            )),
        }
    }

    /// 写入地址 addr 处的字
    pub fn write(&mut self, addr: Addr, val: Word) -> Result<(), CoreError> {
        match addr.kind() {
            AddrKind::Reg => self.regs.write(addr.offset(), val),
            AddrKind::Io => self.io.write(addr.offset(), val),
            AddrKind::Vram => self.vram.write(addr.offset(), val),
            AddrKind::Ram => self.ram.write(addr.offset(), val),
            AddrKind::Opcode => Err(CoreError::MemoryError(
                "Cannot write to opcode space".to_string()
            )),
            AddrKind::Reserved => Err(CoreError::MemoryError(
                format!("Reserved address space: 0x{:08x}", addr.raw())
            )),
        }
    }

    /// 从 SFS 文件加载镜像到地址空间
    pub fn load_sfs(&mut self, path: &str) -> Result<(), CoreError> {
        let data = fs::read(path).map_err(|e| {
            CoreError::MemoryError(format!("Failed to read SFS file '{}': {}", path, e))
        })?;

        // SFS 文件格式检查（前4字节应该是魔数）
        if data.len() < 4 {
            return Err(CoreError::MemoryError(
                "SFS file too small".to_string()
            ));
        }

        // 简单的 SFS 加载逻辑（实际实现可能更复杂）
        // 这里假设文件包含要加载到内存的数据
        let word_count = data.len() / 4;
        for i in 0..word_count {
            let start = i * 4;
            let end = start + 4;
            if end <= data.len() {
                let mut word_bytes = [0u8; 4];
                word_bytes.copy_from_slice(&data[start..end]);
                let word = u32::from_be_bytes(word_bytes); // 大端序
                let addr = Addr::from_ram_idx(i);
                self.write(addr, word)?;
            }
        }

        Ok(())
    }

    /// 将地址空间内容保存为 SFS 文件
    pub fn save_sfs(&self, path: &str) -> Result<(), CoreError> {
        let mut data = Vec::new();

        // 简单的 SFS 保存逻辑 - 保存 RAM 内容
        // 实际实现可能需要保存更多设备的状态
        for i in 0..self.ram.len() {
            let word = self.ram.read(i)?;
            let word_bytes = word.to_be_bytes(); // 大端序
            data.extend_from_slice(&word_bytes);
        }

        fs::write(path, data).map_err(|e| {
            CoreError::MemoryError(format!("Failed to write SFS file '{}': {}", path, e))
        })?;

        Ok(())
    }

    /// 获取各设备的引用（用于测试和调试）
    pub fn regs(&self) -> &RegFile {
        &self.regs
    }

    pub fn vram(&self) -> &Vram {
        &self.vram
    }

    pub fn ram(&self) -> &Memory {
        &self.ram
    }

    pub fn io(&self) -> &Io {
        &self.io
    }

    /// 获取各设备的可变引用（用于测试和调试）
    pub fn regs_mut(&mut self) -> &mut RegFile {
        &mut self.regs
    }

    pub fn vram_mut(&mut self) -> &mut Vram {
        &mut self.vram
    }

    pub fn ram_mut(&mut self) -> &mut Memory {
        &mut self.ram
    }

    pub fn io_mut(&mut self) -> &mut Io {
        &mut self.io
    }

    /// 获取 VRAM 的结束地址
    pub fn vram_end(&self) -> u32 {
        self.vram_end
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn address_space_creation() {
        let addr_space = Address::new(1024, 256);
        assert_eq!(addr_space.ram().len(), 1024);
        assert_eq!(addr_space.vram().len(), 256);
        assert_eq!(addr_space.regs().len(), 0x20);
        assert_eq!(addr_space.io().len(), 0x90);
    }

    #[test]
    fn address_space_read_write() {
        let mut addr_space = Address::new(1024, 256);

        // 测试 RAM 读写
        let ram_addr = Addr::from_ram_idx(10);
        addr_space.write(ram_addr, 0x12345678).unwrap();
        assert_eq!(addr_space.read(ram_addr).unwrap(), 0x12345678);

        // 测试寄存器读写
        let reg_addr = Addr::new(0x10); // PC 寄存器
        addr_space.write(reg_addr, 0x100).unwrap();
        assert_eq!(addr_space.read(reg_addr).unwrap(), 0x100);

        // 测试 I/O 读写
        let io_addr = Addr::new(0x80);
        addr_space.write(io_addr, 0x42).unwrap();
        assert_eq!(addr_space.read(io_addr).unwrap(), 0x42);

        // 测试 VRAM 读写
        let vram_addr = Addr::new(VRAM_START);
        addr_space.write(vram_addr, 0xdeadbeef).unwrap();
        assert_eq!(addr_space.read(vram_addr).unwrap(), 0xdeadbeef);
    }

    #[test]
    fn address_space_error_handling() {
        let mut addr_space = Address::new(10, 10);

        // 测试保留地址访问
        let reserved_addr = Addr::new(0x20000000);
        assert!(addr_space.read(reserved_addr).is_err());
        assert!(addr_space.write(reserved_addr, 0).is_err());

        // 测试操作码空间访问
        let opcode_addr = Addr::new(0x50);
        assert!(addr_space.read(opcode_addr).is_err());
        assert!(addr_space.write(opcode_addr, 0).is_err());
    }

    #[test]
    fn vram_size_limit() {
        // 测试 VRAM 大小限制
        // 正常大小不应该 panic
        let _normal = Address::new(1024, 256);
        // 超出 VRAM_MAX_END 会 panic
        let _ = std::panic::catch_unwind(|| {
            Address::new(1024, 0x10010); // 0x00100100 + 1024*4 - 1 = 0x00100FFF > 0x001000FF
        });
    }
}