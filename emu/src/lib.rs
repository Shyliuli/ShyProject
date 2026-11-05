//! ShyISA emulator library.
//!
//! The binary (`src/main.rs`) is a thin CLI around [`cpu::Emu`]; the
//! [`probe`] module is the opt-in execution API used by tools that need a
//! structured result instead of the process exit status.

pub mod cpu;
pub mod probe;
