// src/addr/vram.rs
use crate::{
    device::AddrPort,
    error::CoreError,
    types::Word,
};

/// VRAM 设备实现（最小实现）
/// 显存设备
pub struct Vram {
    data: Vec<Word>,
}

impl Vram {
    pub fn new(words: usize) -> Self {
        Self {
            data: vec![0; words],
        }
    }

    pub fn data(&self) -> &[Word] {
        &self.data
    }

    pub fn data_mut(&mut self) -> &mut [Word] {
        &mut self.data
    }
}

impl AddrPort for Vram {
    fn read(&self, off: usize) -> Result<Word, CoreError> {
        self.data.get(off).copied().ok_or_else(|| {
            CoreError::MemoryError(format!("VRAM offset 0x{:x} out of bounds (len=0x{:x})", off, self.data.len()))
        })
    }

    fn write(&mut self, off: usize, val: Word) -> Result<(), CoreError> {
        self.data.get_mut(off).map(|slot| *slot = val).ok_or_else(|| {
            CoreError::MemoryError(format!("VRAM offset 0x{:x} out of bounds (len=0x{:x})", off, self.data.len()))
        })
    }

    fn len(&self) -> usize {
        self.data.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_vram_creation() {
        let vram = Vram::new(256);
        assert_eq!(vram.len(), 256);
        assert!(vram.data().iter().all(|&x| x == 0));
    }

    #[test]
    fn test_vram_hello_world() {
        let mut vram = Vram::new(64);
        assert!(vram.write(0, 0x12345678).is_ok());
        assert_eq!(vram.read(0).unwrap(), 0x12345678);
    }
}