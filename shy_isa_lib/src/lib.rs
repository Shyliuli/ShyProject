pub mod types;   // Custom types (Word, etc.)
pub mod isa_def; // registers, opcodes, I/O and memory constants
pub mod memory;
pub mod error;
// Re-exports for convenient use: shy_isa_lib::{Word, Opcode, Register, ...}
pub use types::*;
pub use isa_def::*;
pub use memory::*;
pub use error::*;