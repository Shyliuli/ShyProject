//! ShyISA 模拟器核心 CPU。
//!
//! 实现要点（见 `ShyISA.md`）：
//! - 16 个 32 位通用寄存器 + PC/SP/TM/STATUS/TRAP/EPC/CAUSE/KSP/RS/EXIT/M1-M4。
//! - 32 位字节寻址，大端序，普通内存 32 位访问要求 4 字节对齐。
//! - 内核区 `0x00000100-0x000FFFFF`，用户区 `0x00100000` 及以上。
//! - 统一 trap：保存 STATUS 到内部栈，写 EPC/CAUSE，用户态交换 SP/KSP，切内核态关中断，跳 TRAP。
//! - 定时器 TM：非 0 时每 10ms 减 1，减到 1 时清零并产生可屏蔽中断请求。
//! - `wait` 仅内核态且中断使能时有效，唤醒时 EPC=PC+12。
//! - 致命状态（未初始化 TRAP 时 trap、trap 栈溢出、空栈 iret）直接 panic。

use std::io::{BufRead, BufReader, Read, Stdout, Write, stdout, stdin};
use std::time::{Duration, Instant};

use shy_isa_lib::address::Address;
use shy_isa_lib::op::OpType;

/// 普通内存大小：16MiB，覆盖内核区与用户区。
const MEM_SIZE: usize = 0x0100_0000;
/// 程序入口地址。
const ENTRY: u32 = 0x0000_0100;
/// 用户区起始地址。
const USER_BASE: u32 = 0x0010_0000;
/// 特殊映射区上界（不含），低于此地址不是普通内存。
const SPECIAL_TOP: u32 = 0x0000_0100;

/// trap 原因，对应 CAUSE 寄存器值。
#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TrapCause {
    Syscall = 1,
    Timer = 2,
    IllegalInstr = 3,
    IllegalAddr = 4,
    Permission = 5,
}

/// 单条指令执行后的控制流。
enum Flow {
    /// 继续执行下一条指令。
    Continue,
    /// 程序退出，携带退出码。
    Exit(u32),
    /// 触发 trap，携带原因与 EPC。
    Trap { cause: TrapCause, epc: u32 },
}

/// 取指失败原因。
type FetchErr = TrapCause;

pub struct Emu {
    regs: [u32; 16],
    pc: u32,
    sp: u32,
    tm: u32,
    status: u32,
    trap: u32,
    music: [u32; 4],
    rs: u32,
    epc: u32,
    cause: u32,
    ksp: u32,
    mem: Vec<u8>,
    trap_stack: Vec<u32>,
    timer_pending: bool,
    last_tick: Instant,
    exit_code: Option<u32>,
    input: BufReader<std::io::Stdin>,
    output: Stdout,
    debug: bool,
}

impl Emu {
    /// 创建一个空白 CPU，内存清零，PC 指向入口，处于内核态、中断关闭。
    pub fn new(debug: bool) -> Self {
        Self {
            regs: [0; 16],
            pc: ENTRY,
            sp: 0,
            tm: 0,
            status: 0,
            trap: 0,
            music: [0; 4],
            rs: 0,
            epc: 0,
            cause: 0,
            ksp: 0,
            mem: vec![0; MEM_SIZE],
            trap_stack: Vec::new(),
            timer_pending: false,
            last_tick: Instant::now(),
            exit_code: None,
            input: BufReader::new(stdin()),
            output: stdout(),
            debug,
        }
    }

    /// 加载 `.sfs` raw 内存镜像：字节偏移即地址。
    pub fn load_image(&mut self, image: &[u8]) -> anyhow::Result<()> {
        if image.len() > MEM_SIZE {
            anyhow::bail!(
                "image size {} exceeds memory size {}",
                image.len(),
                MEM_SIZE
            );
        }
        self.mem[..image.len()].copy_from_slice(image);
        Ok(())
    }

    fn is_user(&self) -> bool {
        self.status & 0b01 == 0b01
    }

    fn interrupts_enabled(&self) -> bool {
        self.status & 0b10 == 0b10
    }

    /// 推进定时器：每 10ms 把 TM 减 1，减到 1 时清零并产生中断请求。
    fn tick_timer(&mut self) {
        if self.tm == 0 {
            return;
        }
        let elapsed = self.last_tick.elapsed();
        let ticks = elapsed.as_millis() / 10;
        if ticks == 0 {
            return;
        }
        // 只消耗已被 tick 覆盖的时间，余数留给下一次。
        self.last_tick += Duration::from_millis((ticks as u64) * 10);
        for _ in 0..ticks {
            if self.tm == 0 {
                break;
            }
            if self.tm == 1 {
                self.tm = 0;
                self.timer_pending = true;
                break;
            }
            self.tm -= 1;
        }
    }

    fn deliverable_interrupt(&self) -> bool {
        self.timer_pending && self.interrupts_enabled()
    }

    /// 进入统一 trap 流程。
    fn enter_trap(&mut self, cause: TrapCause, epc: u32) {
        if self.trap < SPECIAL_TOP {
            panic!("trap occurred with uninitialized TRAP register (TRAP=0x{:08X})", self.trap);
        }
        if self.trap_stack.len() >= 64 {
            panic!("trap status stack overflow (depth >= 64)");
        }
        let old_status = self.status;
        self.trap_stack.push(old_status);
        self.epc = epc;
        self.cause = cause as u32;
        // 用户态进入 trap 时交换 SP 与 KSP。
        if old_status & 0b01 == 0b01 {
            std::mem::swap(&mut self.sp, &mut self.ksp);
        }
        // 切到内核态并关闭可屏蔽中断。
        self.status = 0;
        self.pc = self.trap;
    }

    /// `iret`：从 trap 返回。
    fn iret(&mut self) -> Flow {
        if self.trap_stack.is_empty() {
            panic!("iret with empty trap status stack");
        }
        let old_status = self.trap_stack.pop().unwrap();
        self.pc = self.epc;
        self.status = old_status;
        // 恢复到用户态时交换 SP 与 KSP。
        if old_status & 0b01 == 0b01 {
            std::mem::swap(&mut self.sp, &mut self.ksp);
        }
        Flow::Continue
    }

    // ── 普通内存访问 ──────────────────────────────────────────────

    fn check_mem(&self, addr: u32, aligned: bool, size: usize) -> Result<(), TrapCause> {
        if addr < SPECIAL_TOP {
            return Err(TrapCause::IllegalAddr);
        }
        if addr as usize + size > MEM_SIZE {
            return Err(TrapCause::IllegalAddr);
        }
        if aligned && addr % 4 != 0 {
            return Err(TrapCause::IllegalAddr);
        }
        if self.is_user() && addr < USER_BASE {
            return Err(TrapCause::Permission);
        }
        Ok(())
    }

    fn read_mem32(&self, addr: u32) -> Result<u32, TrapCause> {
        self.check_mem(addr, true, 4)?;
        let s = &self.mem[addr as usize..addr as usize + 4];
        Ok(u32::from_be_bytes([s[0], s[1], s[2], s[3]]))
    }

    fn write_mem32(&mut self, addr: u32, val: u32) -> Result<(), TrapCause> {
        self.check_mem(addr, true, 4)?;
        self.mem[addr as usize..addr as usize + 4].copy_from_slice(&val.to_be_bytes());
        Ok(())
    }

    fn read_mem16(&self, addr: u32) -> Result<u32, TrapCause> {
        self.check_mem(addr, false, 2)?;
        let s = &self.mem[addr as usize..addr as usize + 2];
        Ok(u32::from(((s[0] as u32) << 8) | s[1] as u32))
    }

    fn write_mem16(&mut self, addr: u32, val: u32) -> Result<(), TrapCause> {
        self.check_mem(addr, false, 2)?;
        let bytes = val.to_be_bytes();
        self.mem[addr as usize] = bytes[2];
        self.mem[addr as usize + 1] = bytes[3];
        Ok(())
    }

    fn read_mem8(&self, addr: u32) -> Result<u32, TrapCause> {
        self.check_mem(addr, false, 1)?;
        Ok(self.mem[addr as usize] as u32)
    }

    fn write_mem8(&mut self, addr: u32, val: u32) -> Result<(), TrapCause> {
        self.check_mem(addr, false, 1)?;
        self.mem[addr as usize] = val as u8;
        Ok(())
    }

    // ── 特殊映射区（寄存器 / I/O）访问 ────────────────────────────

    /// 读取特殊映射区地址（0x00-0xFF）的 32 位值。
    fn read_reg(&mut self, addr: u32) -> Result<u32, TrapCause> {
        match addr {
            0x00..=0x0F => Ok(self.regs[addr as usize]),
            0x10 => Ok(self.pc),
            0x12 => Ok(self.sp),
            0x13 if !self.is_user() => Ok(self.tm),
            0x14 if !self.is_user() => Ok(self.status),
            0x15 if !self.is_user() => Ok(self.trap),
            0x16..=0x19 => Ok(self.music[(addr - 0x16) as usize]),
            0x1A => Ok(self.rs),
            0x1B => Ok(0),
            0x1C if !self.is_user() => Ok(self.epc),
            0x1D if !self.is_user() => Ok(self.cause),
            0x1E if !self.is_user() => Ok(self.ksp),
            0x70 if !self.is_user() => Ok(self.read_uart_data()),
            0x71 if !self.is_user() => Ok(self.uart_status()),
            // 受保护寄存器在用户态访问 -> 权限错误
            0x13 | 0x14 | 0x15 | 0x1C | 0x1D | 0x1E | 0x70 | 0x71 if self.is_user() => {
                Err(TrapCause::Permission)
            }
            // 保留地址与指令操作码区作为数据读 -> 非法地址
            _ => Err(TrapCause::IllegalAddr),
        }
    }

    /// 写入特殊映射区地址（0x00-0xFF）的 32 位值。
    fn write_reg(&mut self, addr: u32, val: u32) -> Result<(), TrapCause> {
        match addr {
            0x00..=0x0F => self.regs[addr as usize] = val,
            0x10 => self.pc = val,
            0x12 => self.sp = val,
            0x13 if !self.is_user() => self.tm = val,
            0x14 if !self.is_user() => self.status = val & 0b11,
            0x15 if !self.is_user() => self.trap = val,
            0x16..=0x19 => self.music[(addr - 0x16) as usize] = val,
            0x1A => self.rs = val,
            0x1B => {
                // 任何改变都触发程序退出。
                self.exit_code = Some(val);
            }
            0x1C if !self.is_user() => self.epc = val,
            0x1D if !self.is_user() => self.cause = val,
            0x1E if !self.is_user() => self.ksp = val,
            0x70 if !self.is_user() => self.write_uart_data(val),
            0x71 if !self.is_user() => { /* UART 状态寄存器写入忽略 */ }
            0x13 | 0x14 | 0x15 | 0x1C | 0x1D | 0x1E | 0x70 | 0x71 if self.is_user() => {
                return Err(TrapCause::Permission);
            }
            _ => return Err(TrapCause::IllegalAddr),
        }
        Ok(())
    }

    /// 读取地址参数的 32 位值：寄存器或普通内存。
    fn r(&mut self, addr: u32) -> Result<u32, TrapCause> {
        if addr < SPECIAL_TOP {
            self.read_reg(addr)
        } else {
            self.read_mem32(addr)
        }
    }

    /// 写入地址参数的 32 位值：寄存器或普通内存。
    fn w(&mut self, addr: u32, val: u32) -> Result<(), TrapCause> {
        if addr < SPECIAL_TOP {
            self.write_reg(addr, val)
        } else {
            self.write_mem32(addr, val)
        }
    }

    // ── UART / 宿主 I/O ───────────────────────────────────────────

    fn read_uart_data(&mut self) -> u32 {
        // 从标准输入读取一个字节。
        let mut buf = [0u8; 1];
        match self.input.read(&mut buf) {
            Ok(1) => buf[0] as u32,
            _ => 0,
        }
    }

    fn write_uart_data(&mut self, val: u32) {
        let _ = self.output.write_all(&[val as u8]);
        let _ = self.output.flush();
    }

    fn uart_status(&self) -> u32 {
        // bit0=可读，bit1=可写。教学模拟器始终可读写。
        0b11
    }

    /// 读取一行标准输入。`None` 表示 EOF。
    fn read_input_line(&mut self) -> Option<String> {
        let mut line = String::new();
        match self.input.read_line(&mut line) {
            Ok(0) => None,
            Ok(_) => Some(line),
            Err(_) => None,
        }
    }

    /// `ina`：从标准输入读取十六进制文本，解析为 u32。
    fn op_ina(&mut self, addr: u32, cur: u32) -> Flow {
        let Some(line) = self.read_input_line() else {
            return Flow::Exit(0);
        };
        let trimmed = line.trim();
        let hex = trimmed
            .strip_prefix("0x")
            .or_else(|| trimmed.strip_prefix("0X"))
            .unwrap_or(trimmed);
        let val = u32::from_str_radix(hex, 16).unwrap_or(0);
        match self.w(addr, val) {
            Ok(()) => Flow::Continue,
            Err(cause) => Flow::Trap { cause, epc: cur },
        }
    }

    /// `inutfa`：从标准输入读取一个 UTF-8 字符，解码为 code point 写入地址。
    fn op_inutfa(&mut self, addr: u32, cur: u32) -> Flow {
        let Some(line) = self.read_input_line() else {
            return Flow::Exit(0);
        };
        let Some(ch) = line.chars().next() else {
            return Flow::Exit(0);
        };
        match self.w(addr, ch as u32) {
            Ok(()) => Flow::Continue,
            Err(cause) => Flow::Trap { cause, epc: cur },
        }
    }

    fn op_outa(&mut self, addr: u32, cur: u32) -> Flow {
        match self.r(addr) {
            Ok(val) => {
                let _ = write!(self.output, "{:08X}", val);
                let _ = self.output.flush();
                Flow::Continue
            }
            Err(cause) => Flow::Trap { cause, epc: cur },
        }
    }

    fn op_outn(&mut self, n: u32) -> Flow {
        let _ = write!(self.output, "{:08X}", n);
        let _ = self.output.flush();
        Flow::Continue
    }

    fn op_oututfa(&mut self, addr: u32, cur: u32) -> Flow {
        match self.r(addr) {
            Ok(cp) => self.emit_utf8(cp, cur),
            Err(cause) => Flow::Trap { cause, epc: cur },
        }
    }

    fn op_oututfn(&mut self, n: u32, cur: u32) -> Flow {
        self.emit_utf8(n, cur)
    }

    /// 把 code point 编码为 UTF-8 输出；无效 code point 触发非法指令 trap。
    fn emit_utf8(&mut self, cp: u32, cur: u32) -> Flow {
        match char::from_u32(cp) {
            Some(ch) => {
                let _ = write!(self.output, "{ch}");
                let _ = self.output.flush();
                Flow::Continue
            }
            None => Flow::Trap {
                cause: TrapCause::IllegalInstr,
                epc: cur,
            },
        }
    }

    // ── 栈操作 ────────────────────────────────────────────────────

    fn push(&mut self, val: u32, cur: u32) -> Flow {
        match self.write_mem32(self.sp, val) {
            Ok(()) => {
                self.sp = self.sp.wrapping_add(4);
                Flow::Continue
            }
            Err(cause) => Flow::Trap { cause, epc: cur },
        }
    }

    fn pop(&mut self, cur: u32) -> Result<u32, Flow> {
        self.sp = self.sp.wrapping_sub(4);
        match self.read_mem32(self.sp) {
            Ok(v) => Ok(v),
            Err(cause) => Err(Flow::Trap { cause, epc: cur }),
        }
    }

    // ── 取指 ──────────────────────────────────────────────────────

    fn fetch(&self) -> Result<(OpType, u32, u32), FetchErr> {
        // 取指权限检查：用户态不能执行内核区。
        if self.is_user() && self.pc < USER_BASE && self.pc >= SPECIAL_TOP {
            return Err(TrapCause::Permission);
        }
        if self.pc < SPECIAL_TOP || self.pc as usize + 12 > MEM_SIZE {
            return Err(TrapCause::IllegalAddr);
        }
        if self.pc % 4 != 0 {
            return Err(TrapCause::IllegalAddr);
        }
        let base = self.pc as usize;
        let opcode_word = u32::from_be_bytes([
            self.mem[base],
            self.mem[base + 1],
            self.mem[base + 2],
            self.mem[base + 3],
        ]);
        let arg1 = u32::from_be_bytes([
            self.mem[base + 4],
            self.mem[base + 5],
            self.mem[base + 6],
            self.mem[base + 7],
        ]);
        let arg2 = u32::from_be_bytes([
            self.mem[base + 8],
            self.mem[base + 9],
            self.mem[base + 10],
            self.mem[base + 11],
        ]);
        let op = match Address::from_u32(opcode_word) {
            Address::Opcode(op) => op,
            // 0x60-0x6F 保留操作码取指 -> 非法指令；其他值同样非法。
            _ => return Err(TrapCause::IllegalInstr),
        };
        Ok((op, arg1, arg2))
    }

    // ── 执行 ──────────────────────────────────────────────────────

    fn execute(&mut self, op: OpType, a1: u32, a2: u32) -> Flow {
        let cur = self.pc;
        // 默认非控制流指令执行后 PC 推进 12 字节。
        // 控制流指令自行设置 PC 并提前返回。
        macro_rules! r {
            ($e:expr) => {
                match $e {
                    Ok(v) => v,
                    Err(cause) => return Flow::Trap { cause, epc: cur },
                }
            };
        }
        macro_rules! w {
            ($e:expr) => {
                match $e {
                    Ok(()) => {}
                    Err(cause) => return Flow::Trap { cause, epc: cur },
                }
            };
        }

        match op {
            // ── 算术运算 ──
            OpType::Adda => {
                let v = r![self.r(a1)].wrapping_add(r![self.r(a2)]);
                w![self.w(a1, v)];
            }
            OpType::Addn => {
                let v = r![self.r(a1)].wrapping_add(a2);
                w![self.w(a1, v)];
            }
            OpType::Suba => {
                let v = r![self.r(a1)].wrapping_sub(r![self.r(a2)]);
                w![self.w(a1, v)];
            }
            OpType::Subn => {
                let v = r![self.r(a1)].wrapping_sub(a2);
                w![self.w(a1, v)];
            }
            OpType::Mula => {
                let v = r![self.r(a1)].wrapping_mul(r![self.r(a2)]);
                w![self.w(a1, v)];
            }
            OpType::Muln => {
                let v = r![self.r(a1)].wrapping_mul(a2);
                w![self.w(a1, v)];
            }
            OpType::Diva => {
                let d = r![self.r(a2)];
                let n = r![self.r(a1)];
                w![self.w(a1, if d == 0 { 0xFFFFFFFF } else { n / d })];
            }
            OpType::Divn => {
                let n = r![self.r(a1)];
                w![self.w(a1, if a2 == 0 { 0xFFFFFFFF } else { n / a2 })];
            }
            // ── 位运算 ──
            OpType::Lsa => {
                let sh = r![self.r(a2)];
                let v = r![self.r(a1)];
                w![self.w(a1, if sh >= 32 { 0 } else { v << sh })];
            }
            OpType::Lsn => {
                let v = r![self.r(a1)];
                w![self.w(a1, if a2 >= 32 { 0 } else { v << a2 })];
            }
            OpType::Rsa => {
                let sh = r![self.r(a2)];
                let v = r![self.r(a1)];
                w![self.w(a1, if sh >= 32 { 0 } else { v >> sh })];
            }
            OpType::Rsn => {
                let v = r![self.r(a1)];
                w![self.w(a1, if a2 >= 32 { 0 } else { v >> a2 })];
            }
            OpType::Anda => {
                let v = r![self.r(a1)] & r![self.r(a2)];
                w![self.w(a1, v)];
            }
            OpType::Andn => {
                let v = r![self.r(a1)] & a2;
                w![self.w(a1, v)];
            }
            OpType::Ora => {
                let v = r![self.r(a1)] | r![self.r(a2)];
                w![self.w(a1, v)];
            }
            OpType::Orn => {
                let v = r![self.r(a1)] | a2;
                w![self.w(a1, v)];
            }
            OpType::Xora => {
                let v = r![self.r(a1)] ^ r![self.r(a2)];
                w![self.w(a1, v)];
            }
            OpType::Xorn => {
                let v = r![self.r(a1)] ^ a2;
                w![self.w(a1, v)];
            }
            OpType::Nota => {
                let v = !r![self.r(a1)];
                w![self.w(a1, v)];
            }
            // ── 比较指令 ──
            OpType::Equa => self.rs = u32::from(r![self.r(a1)] == r![self.r(a2)]),
            OpType::Equn => self.rs = u32::from(r![self.r(a1)] == a2),
            OpType::Biga => self.rs = u32::from(r![self.r(a1)] > r![self.r(a2)]),
            OpType::Bign => self.rs = u32::from(r![self.r(a1)] > a2),
            OpType::Bigequa => self.rs = u32::from(r![self.r(a1)] >= r![self.r(a2)]),
            OpType::Bigequn => self.rs = u32::from(r![self.r(a1)] >= a2),
            OpType::Smaa => self.rs = u32::from(r![self.r(a1)] < r![self.r(a2)]),
            OpType::Sman => self.rs = u32::from(r![self.r(a1)] < a2),
            OpType::Smaequa => self.rs = u32::from(r![self.r(a1)] <= r![self.r(a2)]),
            OpType::Smaequn => self.rs = u32::from(r![self.r(a1)] <= a2),
            // ── 内存直接操作 ──
            OpType::Seta => {
                let v = r![self.r(a2)];
                w![self.w(a1, v)];
            }
            OpType::Setn => {
                w![self.w(a1, a2)];
            }
            // ── 内存间接操作 ──
            OpType::Geta => {
                let ptr = r![self.r(a2)];
                let v = r![self.read_mem32(ptr)];
                w![self.w(a1, v)];
            }
            OpType::Getn => {
                let v = r![self.read_mem32(a2)];
                w![self.w(a1, v)];
            }
            OpType::Puta => {
                let ptr = r![self.r(a1)];
                let v = r![self.r(a2)];
                w![self.write_mem32(ptr, v)];
            }
            OpType::Putn => {
                let ptr = r![self.r(a1)];
                w![self.write_mem32(ptr, a2)];
            }
            // ── 栈操作 ──
            OpType::Pusha => {
                let v = r![self.r(a1)];
                match self.push(v, cur) {
                    Flow::Continue => {}
                    other => return other,
                }
            }
            OpType::Pushn => {
                match self.push(a1, cur) {
                    Flow::Continue => {}
                    other => return other,
                }
            }
            OpType::Popa => {
                match self.pop(cur) {
                    Ok(v) => w![self.w(a1, v)],
                    Err(flow) => return flow,
                }
            }
            OpType::Pop => {
                match self.pop(cur) {
                    Ok(_) => {}
                    Err(flow) => return flow,
                }
            }
            // ── 控制流 ──
            OpType::Jmpa => {
                if self.rs == 1 {
                    self.rs = 0;
                    let target = r![self.r(a1)];
                    self.pc = target;
                    return Flow::Continue;
                }
            }
            OpType::Jmpn => {
                if self.rs == 1 {
                    self.rs = 0;
                    self.pc = a1;
                    return Flow::Continue;
                }
            }
            OpType::Ujmpa => {
                let target = r![self.r(a1)];
                self.pc = target;
                return Flow::Continue;
            }
            OpType::Ujmpn => {
                self.pc = a1;
                return Flow::Continue;
            }
            OpType::Calla => {
                let target = r![self.r(a1)];
                let ret = cur.wrapping_add(12);
                return match self.push(ret, cur) {
                    Flow::Continue => {
                        self.pc = target;
                        Flow::Continue
                    }
                    other => other,
                };
            }
            OpType::Calln => {
                let ret = cur.wrapping_add(12);
                return match self.push(ret, cur) {
                    Flow::Continue => {
                        self.pc = a1;
                        Flow::Continue
                    }
                    other => other,
                };
            }
            OpType::Ret => match self.pop(cur) {
                Ok(target) => {
                    self.pc = target;
                    return Flow::Continue;
                }
                Err(flow) => return flow,
            },
            // ── I/O 指令 ──
            OpType::Ina => match self.op_ina(a1, cur) {
                Flow::Continue => {}
                other => return other,
            },
            OpType::Inutfa => match self.op_inutfa(a1, cur) {
                Flow::Continue => {}
                other => return other,
            },
            OpType::Outa => match self.op_outa(a1, cur) {
                Flow::Continue => {}
                other => return other,
            },
            OpType::Outn => match self.op_outn(a1) {
                Flow::Continue => {}
                other => return other,
            },
            OpType::Oututfa => match self.op_oututfa(a1, cur) {
                Flow::Continue => {}
                other => return other,
            },
            OpType::Oututfn => match self.op_oututfn(a1, cur) {
                Flow::Continue => {}
                other => return other,
            },
            // ── Trap 指令 ──
            OpType::Syscall => {
                let epc = cur.wrapping_add(12);
                return Flow::Trap {
                    cause: TrapCause::Syscall,
                    epc,
                };
            }
            OpType::Iret => return self.iret(),
            // ── 窄内存间接操作 ──
            OpType::Get8a => {
                let ptr = r![self.r(a2)];
                let v = r![self.read_mem8(ptr)];
                w![self.w(a1, v)];
            }
            OpType::Get8n => {
                let v = r![self.read_mem8(a2)];
                w![self.w(a1, v)];
            }
            OpType::Get16a => {
                let ptr = r![self.r(a2)];
                let v = r![self.read_mem16(ptr)];
                w![self.w(a1, v)];
            }
            OpType::Get16n => {
                let v = r![self.read_mem16(a2)];
                w![self.w(a1, v)];
            }
            OpType::Put8a => {
                let ptr = r![self.r(a1)];
                let v = r![self.r(a2)];
                w![self.write_mem8(ptr, v)];
            }
            OpType::Put8n => {
                let ptr = r![self.r(a1)];
                w![self.write_mem8(ptr, a2)];
            }
            OpType::Put16a => {
                let ptr = r![self.r(a1)];
                let v = r![self.r(a2)];
                w![self.write_mem16(ptr, v)];
            }
            OpType::Put16n => {
                let ptr = r![self.r(a1)];
                w![self.write_mem16(ptr, a2)];
            }
            // ── CPU 等待 ──
            OpType::Wait => return self.op_wait(cur),
            // ── 原子内存操作 ──
            OpType::Atoma => {
                let ptr = r![self.r(a1)];
                let new = r![self.r(a2)];
                let old = r![self.read_mem32(ptr)];
                w![self.write_mem32(ptr, new)];
                w![self.w(a2, old)];
            }
        }

        // 非控制流指令：PC 推进 12 字节。
        self.pc = cur.wrapping_add(12);
        Flow::Continue
    }

    /// `wait`：仅内核态且中断使能时有效。唤醒时 EPC=PC+12。
    fn op_wait(&mut self, cur: u32) -> Flow {
        if self.is_user() || !self.interrupts_enabled() {
            return Flow::Trap {
                cause: TrapCause::IllegalInstr,
                epc: cur,
            };
        }
        // 先推进 PC 到下一条指令，唤醒后 iret 回到这里。
        self.pc = cur.wrapping_add(12);
        // 若已有可交付中断则立即交付；否则等待。
        while !self.deliverable_interrupt() {
            self.tick_timer();
            if self.deliverable_interrupt() {
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        self.timer_pending = false;
        Flow::Trap {
            cause: TrapCause::Timer,
            epc: self.pc,
        }
    }

    // ── 调试输出 ──────────────────────────────────────────────────

    fn dump_state(&self) {
        eprintln!(
            "PC=0x{:08X} SP=0x{:08X} STATUS=0x{:01X} RS={} TM={} EPC=0x{:08X} CAUSE={}",
            self.pc,
            self.sp,
            self.status,
            self.rs,
            self.tm,
            self.epc,
            self.cause
        );
        eprintln!(
            "regs={:08X?}",
            self.regs
        );
    }

    /// 运行直到程序退出，返回退出码。
    pub fn run(&mut self) -> u32 {
        loop {
            self.tick_timer();

            // 重新开启中断后交付 pending 定时器（在当前 PC 指向的指令执行前）。
            if self.deliverable_interrupt() {
                self.timer_pending = false;
                self.enter_trap(TrapCause::Timer, self.pc);
                continue;
            }

            if let Some(code) = self.exit_code {
                return code;
            }

            if self.debug {
                self.dump_state();
            }

            let (op, a1, a2) = match self.fetch() {
                Ok(t) => t,
                Err(cause) => {
                    if self.debug {
                        eprintln!("fetch trap: cause={cause:?} pc=0x{:08X}", self.pc);
                    }
                    self.enter_trap(cause, self.pc);
                    continue;
                }
            };

            match self.execute(op, a1, a2) {
                Flow::Continue => {
                    if let Some(code) = self.exit_code {
                        return code;
                    }
                }
                Flow::Exit(code) => return code,
                Flow::Trap { cause, epc } => {
                    if self.debug {
                        eprintln!("exec trap: cause={cause:?} epc=0x{epc:08X}");
                    }
                    self.enter_trap(cause, epc);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn emu() -> Emu {
        let mut e = Emu::new(false);
        // 测试中关闭实时定时器干扰。
        e.last_tick = Instant::now() + Duration::from_secs(3600);
        e
    }

    fn encode(op: u32, a1: u32, a2: u32) -> [u8; 12] {
        let mut b = [0u8; 12];
        b[0..4].copy_from_slice(&op.to_be_bytes());
        b[4..8].copy_from_slice(&a1.to_be_bytes());
        b[8..12].copy_from_slice(&a2.to_be_bytes());
        b
    }

    fn put(e: &mut Emu, addr: u32, bytes: &[u8]) {
        e.mem[addr as usize..addr as usize + bytes.len()].copy_from_slice(bytes);
    }

    #[test]
    fn setn_addn_executes() {
        let mut e = emu();
        put(&mut e, 0x100, &encode(0x3E, 0x01, 5)); // setn 1x 5
        put(&mut e, 0x10C, &encode(0x21, 0x01, 3)); // addn 1x 3
        put(&mut e, 0x118, &encode(0x3E, 0x1B, 0)); // setn exit 0
        e.run();
        assert_eq!(e.regs[1], 8);
    }

    #[test]
    fn seta_copies_between_registers() {
        let mut e = emu();
        put(&mut e, 0x100, &encode(0x3E, 0x01, 42)); // setn 1x 42
        put(&mut e, 0x10C, &encode(0x3D, 0x02, 0x01)); // seta 2x 1x
        put(&mut e, 0x118, &encode(0x3E, 0x1B, 0));
        e.run();
        assert_eq!(e.regs[2], 42);
    }

    #[test]
    fn sman_jmpn_loops_then_exits() {
        let mut e = emu();
        // setn 1x 0
        put(&mut e, 0x100, &encode(0x3E, 0x01, 0));
        // loop: addn 1x 1
        put(&mut e, 0x10C, &encode(0x21, 0x01, 1));
        // sman 1x 3
        put(&mut e, 0x118, &encode(0x3A, 0x01, 3));
        // jmpn loop (0x10C)
        put(&mut e, 0x124, &encode(0x48, 0x10C, 0));
        // setn exit 0
        put(&mut e, 0x130, &encode(0x3E, 0x1B, 0));
        e.run();
        assert_eq!(e.regs[1], 3);
    }

    #[test]
    fn ujmpn_unconditional_jump() {
        let mut e = emu();
        put(&mut e, 0x100, &encode(0x4A, 0x118, 0)); // ujmpn 0x118
        put(&mut e, 0x10C, &encode(0x3E, 0x01, 999)); // skipped
        put(&mut e, 0x118, &encode(0x3E, 0x02, 7)); // setn 2x 7
        put(&mut e, 0x124, &encode(0x3E, 0x1B, 0));
        e.run();
        assert_eq!(e.regs[1], 0);
        assert_eq!(e.regs[2], 7);
    }

    #[test]
    fn calln_ret_returns_to_caller() {
        let mut e = emu();
        e.sp = 0x100000;
        // setn 1x 1
        put(&mut e, 0x100, &encode(0x3E, 0x01, 1));
        // calln func (0x130)
        put(&mut e, 0x10C, &encode(0x4C, 0x130, 0));
        // setn 1x 100 (after return)
        put(&mut e, 0x118, &encode(0x3E, 0x01, 100));
        // setn exit 0
        put(&mut e, 0x124, &encode(0x3E, 0x1B, 0));
        // func: setn 2x 2 ; ret
        put(&mut e, 0x130, &encode(0x3E, 0x02, 2));
        put(&mut e, 0x13C, &encode(0x4D, 0, 0)); // ret
        e.run();
        assert_eq!(e.regs[2], 2);
        assert_eq!(e.regs[1], 100);
    }

    #[test]
    fn pushn_popa_roundtrip() {
        let mut e = emu();
        e.sp = 0x100000;
        put(&mut e, 0x100, &encode(0x44, 0xABCD, 0)); // pushn 0xABCD
        put(&mut e, 0x10C, &encode(0x45, 0x01, 0)); // popa 1x
        put(&mut e, 0x118, &encode(0x3E, 0x1B, 0));
        e.run();
        assert_eq!(e.regs[1], 0xABCD);
    }

    #[test]
    fn getn_putn_memory_indirect() {
        let mut e = emu();
        // setn 1x 0x00200000
        put(&mut e, 0x100, &encode(0x3E, 0x01, 0x00200000));
        // putn 1x 0x12345678
        put(&mut e, 0x10C, &encode(0x42, 0x01, 0x12345678));
        // getn 2x 0x00200000
        put(&mut e, 0x118, &encode(0x40, 0x02, 0x00200000));
        put(&mut e, 0x124, &encode(0x3E, 0x1B, 0));
        e.run();
        assert_eq!(e.regs[2], 0x12345678);
    }

    #[test]
    fn divn_by_zero_returns_max() {
        let mut e = emu();
        put(&mut e, 0x100, &encode(0x3E, 0x01, 100)); // setn 1x 100
        put(&mut e, 0x10C, &encode(0x27, 0x01, 0)); // divn 1x 0
        put(&mut e, 0x118, &encode(0x3E, 0x1B, 0));
        e.run();
        assert_eq!(e.regs[1], 0xFFFFFFFF);
    }

    #[test]
    fn lsn_shift_ge_32_is_zero() {
        let mut e = emu();
        put(&mut e, 0x100, &encode(0x3E, 0x01, 0xFFFFFFFF)); // setn 1x 0xFFFFFFFF
        put(&mut e, 0x10C, &encode(0x29, 0x01, 40)); // lsn 1x 40
        put(&mut e, 0x118, &encode(0x3E, 0x1B, 0));
        e.run();
        assert_eq!(e.regs[1], 0);
    }

    #[test]
    fn get8n_put8n_byte_access() {
        let mut e = emu();
        // setn 1x 0x00200000
        put(&mut e, 0x100, &encode(0x3E, 0x01, 0x00200000));
        // put8n 1x 0xAB
        put(&mut e, 0x10C, &encode(0x5B, 0x01, 0xAB));
        // get8n 2x 0x00200000
        put(&mut e, 0x118, &encode(0x57, 0x02, 0x00200000));
        put(&mut e, 0x124, &encode(0x3E, 0x1B, 0));
        e.run();
        assert_eq!(e.regs[2], 0xAB);
    }

    #[test]
    fn atoma_atomic_swap() {
        let mut e = emu();
        // 准备内存 0x00200000 = 0x11111111
        put(&mut e, 0x00200000, &0x11111111u32.to_be_bytes());
        // setn 1x 0x00200000
        put(&mut e, 0x100, &encode(0x3E, 0x01, 0x00200000));
        // setn 2x 0x22222222
        put(&mut e, 0x10C, &encode(0x3E, 0x02, 0x22222222));
        // atoma 1x 2x
        put(&mut e, 0x118, &encode(0x5F, 0x01, 0x02));
        put(&mut e, 0x124, &encode(0x3E, 0x1B, 0));
        e.run();
        // 内存被写入新值，2x 得到旧值。
        let mem_val = u32::from_be_bytes([
            e.mem[0x00200000],
            e.mem[0x00200001],
            e.mem[0x00200002],
            e.mem[0x00200003],
        ]);
        assert_eq!(mem_val, 0x22222222);
        assert_eq!(e.regs[2], 0x11111111);
    }

    #[test]
    fn illegal_instruction_traps() {
        let mut e = emu();
        e.trap = 0x200; // 设置 trap 入口
        // 0x60 是保留操作码。
        put(&mut e, 0x100, &encode(0x60, 0, 0));
        // trap 处理：setn exit 1
        put(&mut e, 0x200, &encode(0x3E, 0x1B, 1));
        let code = e.run();
        assert_eq!(code, 1);
        assert_eq!(e.cause, 3);
    }

    #[test]
    fn unaligned_memory_access_traps() {
        let mut e = emu();
        e.trap = 0x200;
        // setn 1x 0x00200001 (未对齐)
        put(&mut e, 0x100, &encode(0x3E, 0x01, 0x00200001));
        // getn 2x 0x00200001 (未对齐地址读)
        put(&mut e, 0x10C, &encode(0x40, 0x02, 0x00200001));
        put(&mut e, 0x118, &encode(0x3E, 0x1B, 0));
        // trap handler
        put(&mut e, 0x200, &encode(0x3E, 0x1B, 4));
        let code = e.run();
        assert_eq!(code, 4);
        assert_eq!(e.cause, 4);
    }

    #[test]
    fn syscall_traps_with_epc_advanced() {
        let mut e = emu();
        e.trap = 0x200;
        // syscall 在 0x100，EPC 应为 0x10C。
        put(&mut e, 0x100, &encode(0x54, 0, 0));
        // syscall 返回后执行：setn exit 0
        put(&mut e, 0x10C, &encode(0x3E, 0x1B, 0));
        // trap handler: iret
        put(&mut e, 0x200, &encode(0x55, 0, 0));
        e.run();
        assert_eq!(e.epc, 0x10C);
        assert_eq!(e.cause, 1);
    }

    #[test]
    fn user_mode_permission_trap_on_kernel_memory() {
        let mut e = emu();
        e.trap = 0x200;
        e.status = 0b11; // 用户态 + 中断使能
        e.sp = 0x100000;
        e.ksp = 0x100000;
        // 尝试读内核区内存 0x100：getn 1x 0x100
        put(&mut e, 0x100, &encode(0x40, 0x01, 0x100));
        put(&mut e, 0x10C, &encode(0x3E, 0x1B, 0));
        // trap handler: setn exit 5
        put(&mut e, 0x200, &encode(0x3E, 0x1B, 5));
        let code = e.run();
        assert_eq!(code, 5);
        assert_eq!(e.cause, 5);
    }

    #[test]
    fn iret_returns_from_trap() {
        let mut e = emu();
        e.trap = 0x200;
        // syscall
        put(&mut e, 0x100, &encode(0x54, 0, 0));
        // syscall 后：setn 1x 42
        put(&mut e, 0x10C, &encode(0x3E, 0x01, 42));
        // setn exit 0
        put(&mut e, 0x118, &encode(0x3E, 0x1B, 0));
        // trap handler: iret
        put(&mut e, 0x200, &encode(0x55, 0, 0));
        e.run();
        assert_eq!(e.regs[1], 42);
    }

    #[test]
    fn user_to_kernel_sp_swap_on_trap() {
        let mut e = Emu::new(false);
        e.last_tick = Instant::now() + Duration::from_secs(3600);
        e.trap = 0x200;
        e.status = 0b01; // 用户态
        e.sp = 0x200000; // 用户栈
        e.ksp = 0x100000; // 内核栈
        // syscall
        put(&mut e, 0x100, &encode(0x54, 0, 0));
        put(&mut e, 0x10C, &encode(0x3E, 0x1B, 0));
        // trap handler: seta 1x sp (读到内核栈), setn exit 0
        put(&mut e, 0x200, &encode(0x3D, 0x01, 0x12)); // seta 1x sp
        put(&mut e, 0x20C, &encode(0x3E, 0x1B, 0));
        e.run();
        // 进入 trap 后 sp 应为内核栈 0x100000。
        assert_eq!(e.regs[1], 0x100000);
    }

    #[test]
    #[should_panic(expected = "uninitialized TRAP")]
    fn trap_with_uninitialized_trap_panics() {
        let mut e = emu();
        put(&mut e, 0x100, &encode(0x60, 0, 0)); // 非法指令
        put(&mut e, 0x10C, &encode(0x3E, 0x1B, 0));
        let _ = e.run();
    }

    #[test]
    #[should_panic(expected = "empty trap status stack")]
    fn iret_with_empty_stack_panics() {
        let mut e = emu();
        put(&mut e, 0x100, &encode(0x55, 0, 0)); // iret
        let _ = e.run();
    }
}
