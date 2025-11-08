//! ShyISA ISA definitions: registers, opcodes, I/O and memory map.

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
            0x01 => Register::R1x,
            0x02 => Register::R2x,
            0x03 => Register::R3x,
            0x04 => Register::R4x,
            0x05 => Register::R5x,
            0x06 => Register::R6x,
            0x07 => Register::R7x,
            0x08 => Register::R8x,
            0x09 => Register::R9x,
            0x0A => Register::Rax,
            0x0B => Register::Rbx,
            0x0C => Register::Rcx,
            0x0D => Register::Rdx,
            0x0E => Register::Rex,
            0x0F => Register::Rfx,
            0x10 => Register::PC,
            0x11 => Register::MD,
            0x12 => Register::SP,
            0x13 => Register::TM,
            0x14 => Register::TA1,
            0x15 => Register::TA2,
            0x16 => Register::M1,
            0x17 => Register::M2,
            0x18 => Register::M3,
            0x19 => Register::M4,
            0x1A => Register::RS,
            0x1B => Register::EX,
            0x1C => Register::BLTS,
            0x1D => Register::BLTL,
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
            0x20 => Opcode::Adda,
            0x21 => Opcode::Addn,
            0x22 => Opcode::Suba,
            0x23 => Opcode::Subn,
            0x24 => Opcode::Mula,
            0x25 => Opcode::Muln,
            0x26 => Opcode::Diva,
            0x27 => Opcode::Divn,
            0x28 => Opcode::Lsa,
            0x29 => Opcode::Lsn,
            0x2A => Opcode::Rsa,
            0x2B => Opcode::Rsn,
            0x2C => Opcode::Anda,
            0x2D => Opcode::Andn,
            0x2E => Opcode::Ora,
            0x2F => Opcode::Orn,
            0x30 => Opcode::Xora,
            0x31 => Opcode::Xorn,
            0x32 => Opcode::Nota,
            0x33 => Opcode::Equa,
            0x34 => Opcode::Equn,
            0x35 => Opcode::Biga,
            0x36 => Opcode::Bign,
            0x37 => Opcode::Bigequa,
            0x38 => Opcode::Bigequn,
            0x39 => Opcode::Smaa,
            0x3A => Opcode::Sman,
            0x3B => Opcode::Smaequa,
            0x3C => Opcode::Smaequn,
            0x3D => Opcode::Seta,
            0x3E => Opcode::Setn,
            0x3F => Opcode::Geta,
            0x40 => Opcode::Getn,
            0x41 => Opcode::Puta,
            0x42 => Opcode::Putn,
            0x43 => Opcode::Pusha,
            0x44 => Opcode::Pushn,
            0x45 => Opcode::Popa,
            0x46 => Opcode::Pop,
            0x47 => Opcode::Jmpa,
            0x48 => Opcode::Jmpn,
            0x49 => Opcode::Ujmpa,
            0x4A => Opcode::Ujmpn,
            0x4B => Opcode::Calla,
            0x4C => Opcode::Calln,
            0x4D => Opcode::Ret,
            0x4E => Opcode::Ina,
            0x4F => Opcode::Inaasc,
            0x50 => Opcode::Outa,
            0x51 => Opcode::Outn,
            0x52 => Opcode::Outaasc,
            0x53 => Opcode::Outnasc,
            0x54 => Opcode::Blta,
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
