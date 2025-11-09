use crate::error::*;
use crate::types::*;
pub struct Memory {
    values: Vec<Word>,
}
impl Memory {
    pub fn new(nsize: usize) -> Self {
        Self {
            values: vec![0; nsize],
        }
    }
    pub fn load_img(&mut self, file_name: &str) -> Result<(), CoreError> {
        let img = std::fs::read(file_name)
            .map_err(|e| CoreError::IOError(format!("Failed to read image file: {}", e)))?;
        //check if img size is correct
        if img.len() != self.values.len() * 4 {
            return Err(CoreError::IOError("Image size mismatch".into()));
        }
        self.values = img
            .chunks_exact(4)
            .map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
            .collect();
        Ok(())
    }

    fn check_bounds(&self, addr: Addr) -> Result<(), CoreError> {
        let idx = addr as usize;
        if idx >= self.values.len() {
            Err(CoreError::MemoryError(format!("Address 0x{:08x} out of bounds", addr)))
        } else {
            Ok(())
        }
    }

    pub fn load(&mut self, addr: Addr, value: Word) -> Result<(), CoreError> {
        self.check_bounds(addr)?;
        self.values[addr as usize] = value;
        Ok(())
    }
    pub fn read(&self, addr: Addr) -> Result<Word, CoreError> {
        self.check_bounds(addr)?;
        Ok(self.values[addr as usize])
    }

    // 保留不安全版本用于内部使用
    pub fn load_unsafe(&mut self, addr: Addr, value: Word) {
        self.values[addr as usize] = value;
    }
    pub fn read_unsafe(&self, addr: Addr) -> Word {
        self.values[addr as usize]
    }
    pub fn to_img(&self, file_name: &str) -> Result<(), CoreError> {
        std::fs::write(file_name, self.to_u8_vec())
            .map_err(|e| CoreError::IOError(format!("Failed to write image file: {}", e)))
    }
    fn to_u8_vec(&self) -> Vec<u8> {
        self.values
            .iter()
            .flat_map(|&word| word.to_be_bytes()) //big endian
            .collect()
    }
}
