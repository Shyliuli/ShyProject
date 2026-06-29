#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpType {
    // 算术运算 0x20-0x27
    Adda,
    Addn,
    Suba,
    Subn,
    Mula,
    Muln,
    Diva,
    Divn,
    // 位运算 0x28-0x32
    Lsa,
    Lsn,
    Rsa,
    Rsn,
    Anda,
    Andn,
    Ora,
    Orn,
    Xora,
    Xorn,
    Nota,
    // 比较指令 0x33-0x3C
    Equa,
    Equn,
    Biga,
    Bign,
    Bigequa,
    Bigequn,
    Smaa,
    Sman,
    Smaequa,
    Smaequn,
    // 内存直接操作 0x3D-0x3E
    Seta,
    Setn,
    // 内存间接操作 0x3F-0x42
    Geta,
    Getn,
    Puta,
    Putn,
    // 栈操作 0x43-0x46
    Pusha,
    Pushn,
    Popa,
    Pop,
    // 控制流 0x47-0x4D
    Jmpa,
    Jmpn,
    Ujmpa,
    Ujmpn,
    Calla,
    Calln,
    Ret,
    // I/O 指令 0x4E-0x53
    Ina,
    Inutfa,
    Outa,
    Outn,
    Oututfa,
    Oututfn,
    // Trap 指令 0x54-0x55
    Syscall,
    Iret,
    // 窄内存间接操作 0x56-0x5D
    Get8a,
    Get8n,
    Get16a,
    Get16n,
    Put8a,
    Put8n,
    Put16a,
    Put16n,
    // CPU 等待 0x5E
    Wait,
    // 原子内存操作 0x5F
    Atoma,
    // 缓存维护 0x60
    Fencei,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseOpError {
    op: String,
}

impl ParseOpError {
    fn new(op: &str) -> Self {
        Self { op: op.to_string() }
    }
}

impl std::fmt::Display for ParseOpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "unknown opcode: {}", self.op)
    }
}

impl std::error::Error for ParseOpError {}

impl OpType {
    pub fn from_str(s: &str) -> Result<Self, ParseOpError> {
        let op = match s.trim().to_ascii_lowercase().as_str() {
            "adda" => OpType::Adda,
            "addn" => OpType::Addn,
            "suba" => OpType::Suba,
            "subn" => OpType::Subn,
            "mula" => OpType::Mula,
            "muln" => OpType::Muln,
            "diva" => OpType::Diva,
            "divn" => OpType::Divn,
            "lsa" => OpType::Lsa,
            "lsn" => OpType::Lsn,
            "rsa" => OpType::Rsa,
            "rsn" => OpType::Rsn,
            "anda" => OpType::Anda,
            "andn" => OpType::Andn,
            "ora" => OpType::Ora,
            "orn" => OpType::Orn,
            "xora" => OpType::Xora,
            "xorn" => OpType::Xorn,
            "nota" => OpType::Nota,
            "equa" => OpType::Equa,
            "equn" => OpType::Equn,
            "biga" => OpType::Biga,
            "bign" => OpType::Bign,
            "bigequa" => OpType::Bigequa,
            "bigequn" => OpType::Bigequn,
            "smaa" => OpType::Smaa,
            "sman" => OpType::Sman,
            "smaequa" => OpType::Smaequa,
            "smaequn" => OpType::Smaequn,
            "seta" => OpType::Seta,
            "setn" => OpType::Setn,
            "geta" => OpType::Geta,
            "getn" => OpType::Getn,
            "puta" => OpType::Puta,
            "putn" => OpType::Putn,
            "pusha" => OpType::Pusha,
            "pushn" => OpType::Pushn,
            "popa" => OpType::Popa,
            "pop" => OpType::Pop,
            "jmpa" => OpType::Jmpa,
            "jmpn" => OpType::Jmpn,
            "ujmpa" => OpType::Ujmpa,
            "ujmpn" => OpType::Ujmpn,
            "calla" => OpType::Calla,
            "calln" => OpType::Calln,
            "ret" => OpType::Ret,
            "ina" => OpType::Ina,
            "inutfa" => OpType::Inutfa,
            "outa" => OpType::Outa,
            "outn" => OpType::Outn,
            "oututfa" => OpType::Oututfa,
            "oututfn" => OpType::Oututfn,
            "syscall" => OpType::Syscall,
            "iret" => OpType::Iret,
            "get8a" => OpType::Get8a,
            "get8n" => OpType::Get8n,
            "get16a" => OpType::Get16a,
            "get16n" => OpType::Get16n,
            "put8a" => OpType::Put8a,
            "put8n" => OpType::Put8n,
            "put16a" => OpType::Put16a,
            "put16n" => OpType::Put16n,
            "wait" => OpType::Wait,
            "atoma" => OpType::Atoma,
            "fencei" | "fence.i" => OpType::Fencei,
            _ => return Err(ParseOpError::new(s)),
        };

        Ok(op)
    }

    pub fn to_u32(&self) -> u32 {
        match self {
            OpType::Adda => 0x20,
            OpType::Addn => 0x21,
            OpType::Suba => 0x22,
            OpType::Subn => 0x23,
            OpType::Mula => 0x24,
            OpType::Muln => 0x25,
            OpType::Diva => 0x26,
            OpType::Divn => 0x27,
            OpType::Lsa => 0x28,
            OpType::Lsn => 0x29,
            OpType::Rsa => 0x2A,
            OpType::Rsn => 0x2B,
            OpType::Anda => 0x2C,
            OpType::Andn => 0x2D,
            OpType::Ora => 0x2E,
            OpType::Orn => 0x2F,
            OpType::Xora => 0x30,
            OpType::Xorn => 0x31,
            OpType::Nota => 0x32,
            OpType::Equa => 0x33,
            OpType::Equn => 0x34,
            OpType::Biga => 0x35,
            OpType::Bign => 0x36,
            OpType::Bigequa => 0x37,
            OpType::Bigequn => 0x38,
            OpType::Smaa => 0x39,
            OpType::Sman => 0x3A,
            OpType::Smaequa => 0x3B,
            OpType::Smaequn => 0x3C,
            OpType::Seta => 0x3D,
            OpType::Setn => 0x3E,
            OpType::Geta => 0x3F,
            OpType::Getn => 0x40,
            OpType::Puta => 0x41,
            OpType::Putn => 0x42,
            OpType::Pusha => 0x43,
            OpType::Pushn => 0x44,
            OpType::Popa => 0x45,
            OpType::Pop => 0x46,
            OpType::Jmpa => 0x47,
            OpType::Jmpn => 0x48,
            OpType::Ujmpa => 0x49,
            OpType::Ujmpn => 0x4A,
            OpType::Calla => 0x4B,
            OpType::Calln => 0x4C,
            OpType::Ret => 0x4D,
            OpType::Ina => 0x4E,
            OpType::Inutfa => 0x4F,
            OpType::Outa => 0x50,
            OpType::Outn => 0x51,
            OpType::Oututfa => 0x52,
            OpType::Oututfn => 0x53,
            OpType::Syscall => 0x54,
            OpType::Iret => 0x55,
            OpType::Get8a => 0x56,
            OpType::Get8n => 0x57,
            OpType::Get16a => 0x58,
            OpType::Get16n => 0x59,
            OpType::Put8a => 0x5A,
            OpType::Put8n => 0x5B,
            OpType::Put16a => 0x5C,
            OpType::Put16n => 0x5D,
            OpType::Wait => 0x5E,
            OpType::Atoma => 0x5F,
            OpType::Fencei => 0x60,
        }
    }
}

impl std::str::FromStr for OpType {
    type Err = ParseOpError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        OpType::from_str(s)
    }
}

#[cfg(test)]
mod tests {
    use super::OpType;

    #[test]
    fn atoma_opcode_is_0x5f() {
        assert_eq!(OpType::Atoma.to_u32(), 0x5F);
    }

    #[test]
    fn fencei_opcode_is_0x60() {
        assert_eq!(OpType::Fencei.to_u32(), 0x60);
    }

    #[test]
    fn parses_opcode_names() {
        assert_eq!(OpType::from_str("addn"), Ok(OpType::Addn));
        assert_eq!(OpType::from_str("CALLN"), Ok(OpType::Calln));
        assert_eq!(OpType::from_str("  ret  "), Ok(OpType::Ret));
        assert_eq!("oututfa".parse::<OpType>(), Ok(OpType::Oututfa));
        assert_eq!("fence.i".parse::<OpType>(), Ok(OpType::Fencei));
        assert!(OpType::from_str("unknown").is_err());
    }
}
