//! `ShyISA` ISA definitions: registers, opcodes, I/O and memory map.

use crate::types::Word;

// Registers (0x01..=0x1D)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum Register {
    // General-purpose (0x01..=0x0F)
    R1x = 0x01,
    R2x = 0x02,
    R3x = 0x03,
    R4x = 0x04,
    R5x = 0x05,
    R6x = 0x06,
    R7x = 0x07,
    R8x = 0x08,
    R9x = 0x09,
    Rax = 0x0A,
    Rbx = 0x0B,
    Rcx = 0x0C,
    Rdx = 0x0D,
    Rex = 0x0E,
    Rfx = 0x0F,

    // Special-purpose (0x10..=0x1D)
    PC = 0x10,   // Program Counter
    MD = 0x11,   // Mode Switch
    SP = 0x12,   // Stack Pointer (must init manually)
    TM = 0x13,   // Timer value
    TA1 = 0x14,  // Interrupt handler entry
    TA2 = 0x15,  // Interrupt return address storage
    M1 = 0x16,   // Sound: sine
    M2 = 0x17,   // Sound: square
    M3 = 0x18,   // Sound: triangle
    M4 = 0x19,   // Sound: sawtooth
    RS = 0x1A,   // Result status (cmp flag): 1/0
    EX = 0x1B,   // Exit register (any change exits)
    BLTS = 0x1C, // Block transfer source
    BLTL = 0x1D, // Block transfer length
}

impl TryFrom<Word> for Register {
    type Error = &'static str;
    fn try_from(v: Word) -> Result<Self, Self::Error> {
        Ok(match v {
            0x01 => Self::R1x,
            0x02 => Self::R2x,
            0x03 => Self::R3x,
            0x04 => Self::R4x,
            0x05 => Self::R5x,
            0x06 => Self::R6x,
            0x07 => Self::R7x,
            0x08 => Self::R8x,
            0x09 => Self::R9x,
            0x0A => Self::Rax,
            0x0B => Self::Rbx,
            0x0C => Self::Rcx,
            0x0D => Self::Rdx,
            0x0E => Self::Rex,
            0x0F => Self::Rfx,
            0x10 => Self::PC,
            0x11 => Self::MD,
            0x12 => Self::SP,
            0x13 => Self::TM,
            0x14 => Self::TA1,
            0x15 => Self::TA2,
            0x16 => Self::M1,
            0x17 => Self::M2,
            0x18 => Self::M3,
            0x19 => Self::M4,
            0x1A => Self::RS,
            0x1B => Self::EX,
            0x1C => Self::BLTS,
            0x1D => Self::BLTL,
            _ => return Err("invalid register id"),
        })
    }
}

// Opcodes (0x20..=0x54)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum Opcode {
    // Arithmetic
    Adda = 0x20,
    Addn = 0x21,
    Suba = 0x22,
    Subn = 0x23,
    Mula = 0x24,
    Muln = 0x25,
    Diva = 0x26,
    Divn = 0x27,
    // Bitwise
    Lsa = 0x28,
    Lsn = 0x29,
    Rsa = 0x2A,
    Rsn = 0x2B,
    Anda = 0x2C,
    Andn = 0x2D,
    Ora = 0x2E,
    Orn = 0x2F,
    Xora = 0x30,
    Xorn = 0x31,
    Nota = 0x32,
    // Compare
    Equa = 0x33,
    Equn = 0x34,
    Biga = 0x35,
    Bign = 0x36,
    Bigequa = 0x37,
    Bigequn = 0x38,
    Smaa = 0x39,
    Sman = 0x3A,
    Smaequa = 0x3B,
    Smaequn = 0x3C,
    // Memory direct
    Seta = 0x3D,
    Setn = 0x3E,
    // Memory indirect
    Geta = 0x3F,
    Getn = 0x40,
    Puta = 0x41,
    Putn = 0x42,
    // Stack
    Pusha = 0x43,
    Pushn = 0x44,
    Popa = 0x45,
    Pop = 0x46,
    // Control flow
    Jmpa = 0x47,
    Jmpn = 0x48,
    Ujmpa = 0x49,
    Ujmpn = 0x4A,
    Calla = 0x4B,
    Calln = 0x4C,
    Ret = 0x4D,
    // I/O
    Ina = 0x4E,
    Inaasc = 0x4F,
    Outa = 0x50,
    Outn = 0x51,
    Outaasc = 0x52,
    Outnasc = 0x53,
    // Special
    Blta = 0x54,
}

impl TryFrom<Word> for Opcode {
    type Error = &'static str;
    fn try_from(v: Word) -> Result<Self, Self::Error> {
        Ok(match v {
            0x20 => Self::Adda,
            0x21 => Self::Addn,
            0x22 => Self::Suba,
            0x23 => Self::Subn,
            0x24 => Self::Mula,
            0x25 => Self::Muln,
            0x26 => Self::Diva,
            0x27 => Self::Divn,
            0x28 => Self::Lsa,
            0x29 => Self::Lsn,
            0x2A => Self::Rsa,
            0x2B => Self::Rsn,
            0x2C => Self::Anda,
            0x2D => Self::Andn,
            0x2E => Self::Ora,
            0x2F => Self::Orn,
            0x30 => Self::Xora,
            0x31 => Self::Xorn,
            0x32 => Self::Nota,
            0x33 => Self::Equa,
            0x34 => Self::Equn,
            0x35 => Self::Biga,
            0x36 => Self::Bign,
            0x37 => Self::Bigequa,
            0x38 => Self::Bigequn,
            0x39 => Self::Smaa,
            0x3A => Self::Sman,
            0x3B => Self::Smaequa,
            0x3C => Self::Smaequn,
            0x3D => Self::Seta,
            0x3E => Self::Setn,
            0x3F => Self::Geta,
            0x40 => Self::Getn,
            0x41 => Self::Puta,
            0x42 => Self::Putn,
            0x43 => Self::Pusha,
            0x44 => Self::Pushn,
            0x45 => Self::Popa,
            0x46 => Self::Pop,
            0x47 => Self::Jmpa,
            0x48 => Self::Jmpn,
            0x49 => Self::Ujmpa,
            0x4A => Self::Ujmpn,
            0x4B => Self::Calla,
            0x4C => Self::Calln,
            0x4D => Self::Ret,
            0x4E => Self::Ina,
            0x4F => Self::Inaasc,
            0x50 => Self::Outa,
            0x51 => Self::Outn,
            0x52 => Self::Outaasc,
            0x53 => Self::Outnasc,
            0x54 => Self::Blta,
            _ => return Err("invalid opcode"),
        })
    }
}

// Memory-mapped I/O (keyboard)
pub const IO_KEY_UP: Word = 0x70;
pub const IO_KEY_DOWN: Word = 0x71;
pub const IO_KEY_LEFT: Word = 0x72;
pub const IO_KEY_RIGHT: Word = 0x73;
pub const IO_KEY_ENTER: Word = 0x74;
pub const IO_KEY_ESC: Word = 0x75;
// 0x76 reserved
pub const IO_ASCII_START: Word = 0x80;
pub const IO_ASCII_END: Word = 0xFF;

// Video memory and code start
pub const MEM_VRAM_START: Word = 0x0000_0100;
pub const MEM_VRAM_END: Word = 0x0010_00FF;
pub const MEM_CODE_START: Word = 0x0010_0100; // program entry

// User RW space begins here (same as code start)
pub const MEM_USER_START: Word = 0x0010_0100;
