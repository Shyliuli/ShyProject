use crate::{mem::MemType, op::OpType, reg::RegType};

/// 将 32 位地址值解析为 ShyISA 地址空间的分类。
pub enum Address {
    Reg(RegType),
    Reserved(u32),
    Opcode(OpType),
    Memory(u32, MemType),
}

impl Address {
    pub fn from_u32(addr: u32) -> Address {
        use OpType::*;
        use RegType::*;

        match addr {
            // ── 通用寄存器 0x00-0x0F ──
            reg @ 0x00..=0x0F => Address::Reg(Regular(reg)),
            // ── 特殊寄存器 0x10-0x1F ──
            0x10 => Address::Reg(PC),
            0x11 => Address::Reg(SegmentStart),
            0x60..=0x6F | 0x72..=0x7F | 0x80..=0xFF => Address::Reserved(addr),
            0x12 => Address::Reg(SP),
            0x13 => Address::Reg(TM),
            0x14 => Address::Reg(Status),
            0x15 => Address::Reg(Trap),
            m @ 0x16..=0x19 => Address::Reg(Music(m - 0x15)),
            0x1A => Address::Reg(Result),
            0x1B => Address::Reg(Exit),
            0x1C => Address::Reg(EPC),
            0x1D => Address::Reg(Cause),
            0x1E => Address::Reg(KernelSP),
            0x1F => Address::Reg(SegmentEnd),
            // ── 算术运算 0x20-0x27 ──
            0x20 => Address::Opcode(Adda),
            0x21 => Address::Opcode(Addn),
            0x22 => Address::Opcode(Suba),
            0x23 => Address::Opcode(Subn),
            0x24 => Address::Opcode(Mula),
            0x25 => Address::Opcode(Muln),
            0x26 => Address::Opcode(Diva),
            0x27 => Address::Opcode(Divn),
            // ── 位运算 0x28-0x32 ──
            0x28 => Address::Opcode(Lsa),
            0x29 => Address::Opcode(Lsn),
            0x2A => Address::Opcode(Rsa),
            0x2B => Address::Opcode(Rsn),
            0x2C => Address::Opcode(Anda),
            0x2D => Address::Opcode(Andn),
            0x2E => Address::Opcode(Ora),
            0x2F => Address::Opcode(Orn),
            0x30 => Address::Opcode(Xora),
            0x31 => Address::Opcode(Xorn),
            0x32 => Address::Opcode(Nota),
            // ── 比较指令 0x33-0x3C ──
            0x33 => Address::Opcode(Equa),
            0x34 => Address::Opcode(Equn),
            0x35 => Address::Opcode(Biga),
            0x36 => Address::Opcode(Bign),
            0x37 => Address::Opcode(Bigequa),
            0x38 => Address::Opcode(Bigequn),
            0x39 => Address::Opcode(Smaa),
            0x3A => Address::Opcode(Sman),
            0x3B => Address::Opcode(Smaequa),
            0x3C => Address::Opcode(Smaequn),
            // ── 内存直接操作 0x3D-0x3E ──
            0x3D => Address::Opcode(Seta),
            0x3E => Address::Opcode(Setn),
            // ── 内存间接操作 0x3F-0x42 ──
            0x3F => Address::Opcode(Geta),
            0x40 => Address::Opcode(Getn),
            0x41 => Address::Opcode(Puta),
            0x42 => Address::Opcode(Putn),
            // ── 栈操作 0x43-0x46 ──
            0x43 => Address::Opcode(Pusha),
            0x44 => Address::Opcode(Pushn),
            0x45 => Address::Opcode(Popa),
            0x46 => Address::Opcode(Pop),
            // ── 控制流 0x47-0x4D ──
            0x47 => Address::Opcode(Jmpa),
            0x48 => Address::Opcode(Jmpn),
            0x49 => Address::Opcode(Ujmpa),
            0x4A => Address::Opcode(Ujmpn),
            0x4B => Address::Opcode(Calla),
            0x4C => Address::Opcode(Calln),
            0x4D => Address::Opcode(Ret),
            // ── I/O 指令 0x4E-0x53 ──
            0x4E => Address::Opcode(Ina),
            0x4F => Address::Opcode(Inutfa),
            0x50 => Address::Opcode(Outa),
            0x51 => Address::Opcode(Outn),
            0x52 => Address::Opcode(Oututfa),
            0x53 => Address::Opcode(Oututfn),
            // ── Trap 指令 0x54-0x55 ──
            0x54 => Address::Opcode(Syscall),
            0x55 => Address::Opcode(Iret),
            // ── 窄内存间接操作 0x56-0x5D ──
            0x56 => Address::Opcode(Get8a),
            0x57 => Address::Opcode(Get8n),
            0x58 => Address::Opcode(Get16a),
            0x59 => Address::Opcode(Get16n),
            0x5A => Address::Opcode(Put8a),
            0x5B => Address::Opcode(Put8n),
            0x5C => Address::Opcode(Put16a),
            0x5D => Address::Opcode(Put16n),
            // ── CPU 等待 0x5E ──
            0x5E => Address::Opcode(Wait),
            // ── 原子内存操作 0x5F ──
            0x5F => Address::Opcode(Atoma),
            // ── UART 0x70-0x71 ──
            0x70 => Address::Reg(UartData),
            0x71 => Address::Reg(UartStatus),
            // ── 普通内存 ──
            addr @ 0x00000100..=0x000FFFFF => Address::Memory(addr, MemType::Kernel),
            addr => Address::Memory(addr, MemType::User),
        }
    }

    pub fn to_u32(&self) -> u32 {
        match self {
            Address::Reg(r) => r.to_u32(),
            Address::Reserved(v) => *v,
            Address::Opcode(op) => op.to_u32(),
            Address::Memory(addr, _) => *addr,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Address;
    use crate::op::OpType;

    #[test]
    fn maps_0x5f_to_atoma_opcode() {
        match Address::from_u32(0x5F) {
            Address::Opcode(OpType::Atoma) => {}
            other => panic!("expected atoma opcode, got {}", other.to_u32()),
        }
    }

    #[test]
    fn keeps_0x60_reserved() {
        match Address::from_u32(0x60) {
            Address::Reserved(0x60) => {}
            other => panic!("expected reserved address, got {}", other.to_u32()),
        }
    }
}
