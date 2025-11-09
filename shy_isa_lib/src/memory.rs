use crate::error::*;
use crate::types::*;
    // Memory struct
pub struct Memory {
    //beacuse we use 32 bit word
    //Memory as a vector of words
    values: Vec<Word>,
}
impl Memory {
    //constructor
    pub fn new(nsize: usize) -> Self {
        Self {
            values: vec![0; nsize],
        }
    }
    //load image
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
    //function to check address
    fn check_bounds(&self, addr: Addr) -> Result<(), CoreError> {
        let idx = addr as usize;
        if idx >= self.values.len() {
            Err(CoreError::MemoryError(format!("Address 0x{:08x} out of bounds", addr)))
        } else {
            Ok(())
        }
    }
    //load a word to memory
    pub fn load(&mut self, addr: Addr, value: Word) -> Result<(), CoreError> {
        self.check_bounds(addr)?;
        self.values[addr as usize] = value;
        Ok(())
    }
    //read a word from memory
    pub fn read(&self, addr: Addr) -> Result<Word, CoreError> {
        self.check_bounds(addr)?;
        Ok(self.values[addr as usize])
    }
/*********read/load without check *********/
    pub fn load_unsafe(&mut self, addr: Addr, value: Word) {
        self.values[addr as usize] = value;
    }
    pub fn read_unsafe(&self, addr: Addr) -> Word {
        self.values[addr as usize]
    }
    //write to file
    pub fn to_img(&self, file_name: &str) -> Result<(), CoreError> {
        std::fs::write(file_name, self.to_u8_vec())
            .map_err(|e| CoreError::IOError(format!("Failed to write image file: {}", e)))
    }
    //function  convert u32 vec to u8 vec
    fn to_u8_vec(&self) -> Vec<u8> {
        self.values
            .iter()
            .flat_map(|&word| word.to_be_bytes()) //big endian
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn test_memory_new() {
        let mem = Memory::new(100);
        assert_eq!(mem.values.len(), 100);

        // Check all values are initialized to 0
        for i in 0..100 {
            assert_eq!(mem.values[i], 0);
        }
    }

    #[test]
    fn test_check_bounds_valid() {
        let mem = Memory::new(100);

        // Test valid addresses
        assert!(mem.check_bounds(0).is_ok());
        assert!(mem.check_bounds(50).is_ok());
        assert!(mem.check_bounds(99).is_ok());
    }

    #[test]
    fn test_check_bounds_invalid() {
        let mem = Memory::new(100);

        // Test invalid addresses
        assert!(mem.check_bounds(100).is_err());
        assert!(mem.check_bounds(200).is_err());
        assert!(mem.check_bounds(u32::MAX).is_err());

        // Check error message contains address
        let result = mem.check_bounds(100);
        match result {
            Err(CoreError::MemoryError(msg)) => {
                assert!(msg.contains("0x00000064"));
            }
            _ => panic!("Expected MemoryError"),
        }
    }

    #[test]
    fn test_load_and_read() {
        let mut mem = Memory::new(100);

        // Test loading and reading values
        assert!(mem.load(0, 0x12345678).is_ok());
        assert!(mem.load(50, 0xABCDEF00).is_ok());
        assert!(mem.load(99, 0xFFFFFFFF).is_ok());

        assert_eq!(mem.read(0).unwrap(), 0x12345678);
        assert_eq!(mem.read(50).unwrap(), 0xABCDEF00);
        assert_eq!(mem.read(99).unwrap(), 0xFFFFFFFF);
    }

    #[test]
    fn test_load_out_of_bounds() {
        let mut mem = Memory::new(100);

        // Test loading to invalid address
        assert!(mem.load(100, 0x12345678).is_err());
        assert!(mem.load(200, 0x12345678).is_err());
    }

    #[test]
    fn test_read_out_of_bounds() {
        let mem = Memory::new(100);

        // Test reading from invalid address
        assert!(mem.read(100).is_err());
        assert!(mem.read(200).is_err());
    }

    #[test]
    fn test_load_unsafe_and_read_unsafe() {
        let mut mem = Memory::new(100);

        // Test unsafe operations (no bounds checking)
        mem.load_unsafe(0, 0x12345678);
        mem.load_unsafe(50, 0xABCDEF00);
        mem.load_unsafe(99, 0xFFFFFFFF);

        assert_eq!(mem.read_unsafe(0), 0x12345678);
        assert_eq!(mem.read_unsafe(50), 0xABCDEF00);
        assert_eq!(mem.read_unsafe(99), 0xFFFFFFFF);
    }

    #[test]
    fn test_to_u8_vec() {
        let mut mem = Memory::new(3);
        mem.load_unsafe(0, 0x12345678);
        mem.load_unsafe(1, 0xABCDEF00);
        mem.load_unsafe(2, 0xFFFFFFFF);

        let bytes = mem.to_u8_vec();

        // Check length (3 words * 4 bytes = 12 bytes)
        assert_eq!(bytes.len(), 12);

        // Check big endian conversion
        assert_eq!(bytes[0..4], [0x12, 0x34, 0x56, 0x78]);
        assert_eq!(bytes[4..8], [0xAB, 0xCD, 0xEF, 0x00]);
        assert_eq!(bytes[8..12], [0xFF, 0xFF, 0xFF, 0xFF]);
    }

    #[test]
    fn test_load_img_and_to_img() -> Result<(), CoreError> {
        let mut mem = Memory::new(3);

        // Set up initial values
        mem.load_unsafe(0, 0x12345678);
        mem.load_unsafe(1, 0xABCDEF00);
        mem.load_unsafe(2, 0xFFFFFFFF);

        // Create temporary file
        let temp_file = NamedTempFile::new()
            .map_err(|e| CoreError::IOError(format!("Failed to create temp file: {}", e)))?;
        let temp_path = temp_file.path().to_str().unwrap();

        // Save to image file
        mem.to_img(temp_path)?;

        // Create new memory and load from file
        let mut mem2 = Memory::new(3);
        mem2.load_img(temp_path)?;

        // Verify loaded values match
        assert_eq!(mem2.read(0)?, 0x12345678);
        assert_eq!(mem2.read(1)?, 0xABCDEF00);
        assert_eq!(mem2.read(2)?, 0xFFFFFFFF);

        Ok(())
    }

    #[test]
    fn test_load_img_file_not_found() {
        let mut mem = Memory::new(100);

        // Test loading non-existent file
        let result = mem.load_img("non_existent_file.img");
        assert!(result.is_err());

        match result.unwrap_err() {
            CoreError::IOError(msg) => {
                assert!(msg.contains("Failed to read image file"));
            }
            _ => panic!("Expected IOError"),
        }
    }

    #[test]
    fn test_load_img_size_mismatch() -> Result<(), CoreError> {
        let mut mem = Memory::new(100); // 100 words = 400 bytes expected

        // Create temporary file with wrong size
        let mut temp_file = NamedTempFile::new()
            .map_err(|e| CoreError::IOError(format!("Failed to create temp file: {}", e)))?;

        // Write only 10 bytes instead of expected 400 bytes
        temp_file.write_all(&[0x12, 0x34, 0x56, 0x78, 0xAB, 0xCD, 0xEF, 0x00, 0xFF, 0x00])
            .map_err(|e| CoreError::IOError(format!("Failed to write to temp file: {}", e)))?;

        let temp_path = temp_file.path().to_str().unwrap();

        // Try to load file with wrong size
        let result = mem.load_img(temp_path);
        assert!(result.is_err());

        match result.unwrap_err() {
            CoreError::IOError(msg) => {
                assert!(msg.contains("Image size mismatch"));
            }
            _ => panic!("Expected IOError"),
        }

        Ok(())
    }

    #[test]
    fn test_load_img_valid_file() -> Result<(), CoreError> {
        let mut mem = Memory::new(3); // 3 words = 12 bytes

        // Create temporary file with exact size
        let mut temp_file = NamedTempFile::new()
            .map_err(|e| CoreError::IOError(format!("Failed to create temp file: {}", e)))?;

        // Write 12 bytes (3 words in big endian)
        let test_data = [
            0x12, 0x34, 0x56, 0x78, // word 0: 0x12345678
            0xAB, 0xCD, 0xEF, 0x00, // word 1: 0xABCDEF00
            0xFF, 0xFF, 0xFF, 0xFF, // word 2: 0xFFFFFFFF
        ];
        temp_file.write_all(&test_data)
            .map_err(|e| CoreError::IOError(format!("Failed to write to temp file: {}", e)))?;

        let temp_path = temp_file.path().to_str().unwrap();

        // Load the file
        mem.load_img(temp_path)?;

        // Verify loaded values
        assert_eq!(mem.read(0)?, 0x12345678);
        assert_eq!(mem.read(1)?, 0xABCDEF00);
        assert_eq!(mem.read(2)?, 0xFFFFFFFF);

        Ok(())
    }

    #[test]
    fn test_to_img_io_error() -> Result<(), CoreError> {
        let mem = Memory::new(3);

        // Try to write to invalid path
        let result = mem.to_img("/invalid/path/that/does/not/exist/test.img");
        assert!(result.is_err());

        match result.unwrap_err() {
            CoreError::IOError(msg) => {
                assert!(msg.contains("Failed to write image file"));
            }
            _ => panic!("Expected IOError"),
        }

        Ok(())
    }

    #[test]
    fn test_edge_cases() {
        let mut mem = Memory::new(1);

        // Test single word memory
        assert!(mem.load(0, 0xFFFFFFFF).is_ok());
        assert_eq!(mem.read(0).unwrap(), 0xFFFFFFFF);
        assert!(mem.load(1, 0x12345678).is_err()); // out of bounds
        assert!(mem.read(1).is_err()); // out of bounds

        // Test empty memory
        let mut empty_mem = Memory::new(0);
        assert!(empty_mem.load(0, 0x12345678).is_err());
        assert!(empty_mem.read(0).is_err());
    }

    #[test]
    fn test_word_boundary_operations() {
        let mut mem = Memory::new(256);

        // Test operations at word boundaries
        for i in 0..256 {
            let test_value = (i as u32) * 0x01010101;
            assert!(mem.load(i, test_value).is_ok());
            assert_eq!(mem.read(i).unwrap(), test_value);
        }
    }
}
