pub mod types;   // Custom types (Word, etc.)
pub mod isa_def; // registers, opcodes, I/O and memory constants
pub mod error;
pub mod device;  // addr_port trait
pub mod addr;    // Address crate with modules

// Re-exports for convenient use: shy_isa_lib::{Word, Opcode, Register, ...}
pub use types::*;
pub use isa_def::*;
pub use error::*;
pub use device::*;  // 导出 addr_port trait
pub use addr::*;    // 导出 Addr, Address, Memory, RegFile, Io, Vram 等