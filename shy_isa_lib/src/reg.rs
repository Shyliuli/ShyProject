#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegType {
    Regular(u32), // 0x00-0x0F，存储地址值（即寄存器编号）
    PC,
    SP,
    TM,
    Status,
    Trap,
    Music(u32), // 0x16-0x19，存储编号 1-4
    Result,
    Exit,
    EPC,
    Cause,
    KernelSP,
    UartData,
    UartStatus,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseRegError {
    reg: String,
}

impl ParseRegError {
    fn new(reg: &str) -> Self {
        Self {
            reg: reg.to_string(),
        }
    }
}

impl std::fmt::Display for ParseRegError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "unknown register: {}", self.reg)
    }
}

impl std::error::Error for ParseRegError {}

impl RegType {
    pub fn from_str(s: &str) -> Result<Self, ParseRegError> {
        let reg = match s.trim().to_ascii_lowercase().as_str() {
            "0x" => RegType::Regular(0x0),
            "1x" => RegType::Regular(0x1),
            "2x" => RegType::Regular(0x2),
            "3x" => RegType::Regular(0x3),
            "4x" => RegType::Regular(0x4),
            "5x" => RegType::Regular(0x5),
            "6x" => RegType::Regular(0x6),
            "7x" => RegType::Regular(0x7),
            "8x" => RegType::Regular(0x8),
            "9x" => RegType::Regular(0x9),
            "ax" => RegType::Regular(0xA),
            "bx" => RegType::Regular(0xB),
            "cx" => RegType::Regular(0xC),
            "dx" => RegType::Regular(0xD),
            "ex" => RegType::Regular(0xE),
            "fx" => RegType::Regular(0xF),
            "pc" => RegType::PC,
            "sp" => RegType::SP,
            "tm" => RegType::TM,
            "status" => RegType::Status,
            "trap" => RegType::Trap,
            "m1" => RegType::Music(1),
            "m2" => RegType::Music(2),
            "m3" => RegType::Music(3),
            "m4" => RegType::Music(4),
            "rs" => RegType::Result,
            "exit" => RegType::Exit,
            "epc" => RegType::EPC,
            "cause" => RegType::Cause,
            "ksp" => RegType::KernelSP,
            "uart_data" => RegType::UartData,
            "uart_status" => RegType::UartStatus,
            _ => return Err(ParseRegError::new(s)),
        };

        Ok(reg)
    }

    pub fn to_u32(&self) -> u32 {
        match self {
            RegType::Regular(v) => *v,
            RegType::PC => 0x10,
            RegType::SP => 0x12,
            RegType::TM => 0x13,
            RegType::Status => 0x14,
            RegType::Trap => 0x15,
            RegType::Music(n) => 0x15 + n,
            RegType::Result => 0x1A,
            RegType::Exit => 0x1B,
            RegType::EPC => 0x1C,
            RegType::Cause => 0x1D,
            RegType::KernelSP => 0x1E,
            RegType::UartData => 0x70,
            RegType::UartStatus => 0x71,
        }
    }
}

impl std::str::FromStr for RegType {
    type Err = ParseRegError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        RegType::from_str(s)
    }
}

#[cfg(test)]
mod tests {
    use super::RegType;

    #[test]
    fn parses_register_names() {
        assert_eq!(RegType::from_str("1x"), Ok(RegType::Regular(1)));
        assert_eq!(RegType::from_str("AX"), Ok(RegType::Regular(0xA)));
        assert_eq!(RegType::from_str("  sp  "), Ok(RegType::SP));
        assert_eq!("ksp".parse::<RegType>(), Ok(RegType::KernelSP));
        assert_eq!(RegType::from_str("uart_data"), Ok(RegType::UartData));
        assert!(RegType::from_str("unknown").is_err());
    }

    #[test]
    fn register_names_map_to_addresses() {
        assert_eq!(RegType::from_str("fx").unwrap().to_u32(), 0x0F);
        assert_eq!(RegType::from_str("m4").unwrap().to_u32(), 0x19);
        assert_eq!(RegType::from_str("uart_status").unwrap().to_u32(), 0x71);
    }
}
