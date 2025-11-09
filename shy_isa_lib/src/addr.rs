// src/addr.rs - Addr 模块入口文件
// 使用现代化的同名文件+文件夹组织方式

pub mod addr;
pub mod address_space;
pub mod memory;
pub mod reg;
pub mod io;
pub mod vram;

// 重新导出主要类型
pub use addr::{Addr, AddrKind};
pub use address_space::Address;
pub use memory::Memory;
pub use reg::{RegFile, SpecialReg};
pub use io::Io;
pub use vram::Vram;