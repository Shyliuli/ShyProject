//! ShyISA 模拟器核心 CPU。
//!
//! 实现要点（见 `ShyISA.md`）：
//! - 16 个 32 位通用寄存器 + PC/SEGS/SP/TM/STATUS/TRAP/EPC/CAUSE/KSP/SEGE/RS/EXIT/M1-M4。
//! - 32 位字节寻址，大端序，普通内存 32 位访问要求 4 字节对齐。
//! - 内核态普通内存全局直通；用户态普通内存按 SEGS/SEGE 做段转换。
//! - 统一 trap：保存 STATUS 到内部栈，写 EPC/CAUSE，用户态交换 SP/KSP，切内核态关中断，跳 TRAP。
//! - 定时器 TM：非 0 时每 10ms 减 1，减到 1 时清零并产生可屏蔽中断请求。
//! - `wait` 仅内核态且中断使能时有效，唤醒时 EPC=PC+12。
//! - 致命状态（未初始化 TRAP 时 trap、trap 栈溢出、空栈 iret）直接 panic。

use std::collections::{HashSet, VecDeque};
use std::io::{BufRead, BufReader, Read, Stdout, Write, stdin, stdout};
use std::time::{Duration, Instant};

use shy_isa_lib::address::Address;
use shy_isa_lib::op::OpType;

use crate::probe;

/// 默认内存大小：32MiB。
pub const DEFAULT_MEM_SIZE: u32 = 0x0200_0000;
/// 翻译缓存容量。直接映射，条目数保持 2 的幂，便于快速取模。
const INSTR_CACHE_ENTRIES: usize = 16 * 1024;
/// 程序入口地址。
const ENTRY: u32 = 0x0000_0100;
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
    /// 用户态程序写 EXIT：请求内核结束当前进程，而不是停机。
    UserExit = 6,
}

/// 单条指令执行后的控制流。
enum Flow {
    /// 继续执行下一条指令。
    Continue,
    /// 程序退出，携带退出码。
    Exit(u32),
    /// 触发 trap，携带原因与 EPC。
    Trap { cause: TrapCause, epc: u32 },
    /// 命中 GDB 数据 watchpoint，携带被监视地址。
    Watchpoint(u32),
}

/// Public result of one emulator scheduling step.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StepOutcome {
    Continue,
    Exited(u32),
    Watchpoint(u32),
}

/// Reason why debugger-controlled execution stopped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StopReason {
    Breakpoint,
    SingleStep,
    Exited(u32),
    Interrupt,
    Watchpoint(u32),
}

/// GDB data watchpoint access kind.
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
pub enum WatchpointKind {
    Read,
    Write,
    Access,
}

/// Guest virtual-address data watchpoint.
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
pub struct Watchpoint {
    pub addr: u32,
    pub len: usize,
    pub kind: WatchpointKind,
}

impl Watchpoint {
    pub fn new(addr: u32, len: usize, kind: WatchpointKind) -> Self {
        Self { addr, len, kind }
    }
}

/// 取指失败原因。
type FetchErr = TrapCause;

#[derive(Clone, Copy)]
enum MemAccessKind {
    Read,
    Write,
}

enum MemAccessErr {
    Trap(TrapCause),
    Watchpoint(u32),
}

type MemAccessResult<T> = Result<T, MemAccessErr>;

impl From<TrapCause> for MemAccessErr {
    fn from(cause: TrapCause) -> Self {
        Self::Trap(cause)
    }
}

#[derive(Clone, Copy)]
struct CachedInstr {
    pc: u32,
    user: bool,
    segs: u32,
    sege: u32,
    op: OpType,
    arg1: u32,
    arg2: u32,
}

/// Parent CPU state saved while a nested child runs.
struct CpuSnapshot {
    regs: [u32; 16],
    pc: u32,
    segs: u32,
    sp: u32,
    tm: u32,
    status: u32,
    trap: u32,
    music: [u32; 4],
    rs: u32,
    epc: u32,
    cause: u32,
    ksp: u32,
    sege: u32,
    trap_stack: Vec<u32>,
    timer_pending: bool,
    last_tick: Instant,
    exit_code: Option<u32>,
    pending_watchpoint: Option<u32>,
    debug: bool,
}
/// One old-memory snapshot taken before a child write.
struct WriteEntry {
    phys: usize,
    start: usize,
    len: usize,
}

/// Undo log for a nested child run. Normal Emu runs keep this as None.
#[derive(Default)]
struct WriteJournal {
    old: Vec<u8>,
    entries: Vec<WriteEntry>,
}

impl WriteJournal {
    fn rollback(self, mem: &mut [u8]) {
        for entry in self.entries.into_iter().rev() {
            mem[entry.phys..entry.phys + entry.len]
                .copy_from_slice(&self.old[entry.start..entry.start + entry.len]);
        }
    }
}

pub struct Emu {
    regs: [u32; 16],
    pc: u32,
    segs: u32,
    sp: u32,
    tm: u32,
    status: u32,
    trap: u32,
    music: [u32; 4],
    rs: u32,
    epc: u32,
    cause: u32,
    ksp: u32,
    sege: u32,
    mem_size: u32,
    mem: Vec<u8>,
    instr_cache: Vec<Option<CachedInstr>>,
    trap_stack: Vec<u32>,
    timer_pending: bool,
    last_tick: Instant,
    exit_code: Option<u32>,
    input: BufReader<std::io::Stdin>,
    input_chars: VecDeque<char>,
    output: Stdout,
    debug: bool,
    pending_watchpoint: Option<u32>,
    probe: Option<probe::Probe>,
    journal: Option<WriteJournal>,
}

impl Emu {
    /// 创建一个空白 CPU，内存清零，PC 指向入口，处于内核态、中断关闭。
    pub fn new(debug: bool, mem_size: u32) -> Self {
        Self {
            regs: [0; 16],
            pc: ENTRY,
            segs: 0,
            sp: 0,
            tm: 0,
            status: 0,
            trap: 0,
            music: [0; 4],
            rs: 0,
            epc: 0,
            cause: 0,
            ksp: 0,
            sege: mem_size,
            mem_size,
            mem: vec![0; mem_size as usize],
            instr_cache: vec![None; INSTR_CACHE_ENTRIES],
            trap_stack: Vec::new(),
            timer_pending: false,
            last_tick: Instant::now(),
            exit_code: None,
            input: BufReader::new(stdin()),
            input_chars: VecDeque::new(),
            output: stdout(),
            debug,
            pending_watchpoint: None,
            probe: None,
            journal: None,
        }
    }

    /// Install the opt-in probe protocol. Until this is called the CPU has no
    /// probe state at all, so syscall 999 follows the ordinary trap path.
    pub fn attach_probe(&mut self, options: probe::ProbeOptions) -> anyhow::Result<()> {
        options.validate()?;
        self.probe = Some(probe::Probe::new(options));
        Ok(())
    }

    /// Run a bounded probe until a structured stop is recorded.
    ///
    /// This is separate from run(): it never loops forever, and every
    /// blocking/broken condition (trap, input, wait, fuel, timeout) becomes a
    /// Stop value instead of a panic or a sleep.
    pub fn run_probe(&mut self) -> probe::Stop {
        self.run_probe_inner(true)
    }

    fn run_probe_inner(&mut self, reset_timeout: bool) -> probe::Stop {
        let probe = self
            .probe
            .as_mut()
            .expect("run_probe requires attach_probe first");
        if reset_timeout {
            probe.begin_run();
        }
        loop {
            if let Some(stop) = self.probe_stop() {
                return stop;
            }
            if let Some(stop) = self.probe_tick() {
                return stop;
            }
            match self.step_one() {
                StepOutcome::Continue => {}
                StepOutcome::Exited(code) => {
                    if let Some(probe) = &mut self.probe {
                        probe.stop(probe::Stop::Exited(code));
                    }
                    return self.probe_stop().expect("probe stop was just recorded");
                }
                StepOutcome::Watchpoint(_) => {}
            }
        }
    }

    pub fn probe_stop(&self) -> Option<probe::Stop> {
        self.probe.as_ref().and_then(probe::Probe::stop_reason)
    }

    pub fn probe_steps(&self) -> u64 {
        self.probe.as_ref().map_or(0, probe::Probe::steps)
    }

    pub fn probe_exit(&self) -> u32 {
        self.probe.as_ref().map_or(0, probe::Probe::exit)
    }

    pub fn take_probe_output(&mut self) -> Vec<u8> {
        self.probe
            .as_mut()
            .map_or_else(Vec::new, probe::Probe::take_output)
    }

    pub fn probe_output_truncated(&self) -> bool {
        self.probe
            .as_ref()
            .is_some_and(probe::Probe::output_truncated)
    }

    pub fn take_probe_results(&mut self) -> Vec<probe::ResultFrame> {
        self.probe
            .as_mut()
            .map_or_else(Vec::new, probe::Probe::take_results)
    }

    /// Answer the current HOST_REQUEST and resume from the already-advanced PC.
    ///
    /// The response is written to the guest mailbox through the normal
    /// translate_mem checks (user segment bounds included). A response larger
    /// than response_cap is rejected without clearing the stop, so the caller
    /// can retry with a smaller payload.
    pub fn respond_probe(&mut self, bytes: &[u8]) -> anyhow::Result<()> {
        let Some(probe) = self.probe.as_ref() else {
            anyhow::bail!("respond_probe requires attach_probe first");
        };
        let Some(request) = probe.request() else {
            anyhow::bail!("respond_probe requires a pending Stop::Request");
        };
        let response_ptr = request.response_ptr;
        let response_cap = request.response_cap;

        if bytes.len() > response_cap as usize {
            anyhow::bail!(
                "probe response is {} bytes, guest mailbox capacity is {}",
                bytes.len(),
                response_cap
            );
        }
        if bytes.len() > probe::MAX_PROBE_RESPONSE {
            anyhow::bail!(
                "probe response is {} bytes, limit is {}",
                bytes.len(),
                probe::MAX_PROBE_RESPONSE
            );
        }

        self.debug_write_mem(response_ptr, bytes)?;
        self.regs[0] = bytes.len() as u32;
        if let Some(probe) = &mut self.probe {
            probe.clear_request();
        }
        Ok(())
    }

    /// Run a compiler-generated child entry in a nested sandbox.
    ///
    /// This is [`Emu::run_child_with_host`] for a responder that needs only the
    /// request and the nesting depth: the child emulator stays out of reach.
    pub fn run_child_with<F>(
        &mut self,
        options: probe::ChildOptions,
        mut respond: F,
    ) -> anyhow::Result<probe::ChildResult>
    where
        F: FnMut(&probe::HostRequest, u32) -> anyhow::Result<Vec<u8>>,
    {
        self.run_child_with_host(options, |request, _child, depth| respond(request, depth))
    }

    /// Run a compiler-generated child entry in a nested sandbox, with the child
    /// emulator in reach of the responder.
    ///
    /// The parent must be paused at Stop::Request. The child shares guest
    /// memory, runs with a fresh register file and the caller-provided stack,
    /// and answers its own HOST_REQUEST calls through respond, which receives
    /// the request, this emulator (stepping the child), and the child nesting
    /// depth. Child memory writes are journaled; on success they stay, on
    /// failure they are rolled back and the parent CPU/probe state is restored.
    ///
    /// The child max_steps/timeout are capped by the parent's remaining budget,
    /// and child steps are charged back to the parent Probe, so a child cannot
    /// reset or escape the parent fuel.
    pub fn run_child_with_host<F>(
        &mut self,
        options: probe::ChildOptions,
        mut respond: F,
    ) -> anyhow::Result<probe::ChildResult>
    where
        F: FnMut(&probe::HostRequest, &mut Emu, u32) -> anyhow::Result<Vec<u8>>,
    {
        self.validate_child(&options)?;
        let depth = options.parent_depth.saturating_add(1);

        let parent = self
            .probe
            .as_ref()
            .expect("validate_child checked the parent probe");
        let parent_steps = parent.steps();
        let child_max_steps = options.max_steps.min(parent.remaining_steps());
        let child_timeout = options.timeout.min(parent.remaining_timeout());
        let parent_elapsed = parent.elapsed_millis();

        if child_max_steps == 0 {
            return Ok(Self::child_failure(
                options.site,
                depth,
                probe::Stop::OutOfFuel {
                    steps: parent_steps,
                },
            ));
        }
        if child_timeout.is_zero() {
            return Ok(Self::child_failure(
                options.site,
                depth,
                probe::Stop::Timeout {
                    millis: parent_elapsed,
                },
            ));
        }

        let snapshot = self.snapshot_cpu();
        let mut parent_probe = self.probe.take();
        let child_probe = probe::Probe::new_child(probe::ProbeOptions {
            mem_size: self.mem_size,
            max_steps: child_max_steps,
            timeout: child_timeout,
            input: Vec::new(),
            capture_output: true,
            output_cap: options.output_cap,
            debug: false,
        });
        self.install_child(&options, child_probe);

        let stop = match self.run_child_loop(depth, &mut respond) {
            Ok(stop) => stop,
            Err(error) => {
                let child_steps = self.probe_steps();
                if let Some(parent) = parent_probe.as_mut() {
                    parent.add_steps(child_steps);
                }
                self.restore_child(snapshot, parent_probe, false);
                return Err(error);
            }
        };

        let committed = matches!(&stop, probe::Stop::Done | probe::Stop::Exited(0));
        let stdout = if committed {
            self.take_probe_output()
        } else {
            Vec::new()
        };
        let results = if committed {
            self.take_probe_results()
        } else {
            Vec::new()
        };
        let exit = self.probe_exit();
        let steps = self.probe_steps();
        if let Some(parent) = parent_probe.as_mut() {
            parent.add_steps(steps);
        }
        self.restore_child(snapshot, parent_probe, committed);

        Ok(probe::ChildResult {
            site: options.site,
            depth,
            committed,
            stop,
            exit,
            stdout,
            results,
            steps,
        })
    }

    fn child_failure(site: u32, depth: u32, stop: probe::Stop) -> probe::ChildResult {
        probe::ChildResult {
            site,
            depth,
            committed: false,
            stop,
            exit: 0,
            stdout: Vec::new(),
            results: Vec::new(),
            steps: 0,
        }
    }

    fn validate_child(&self, options: &probe::ChildOptions) -> anyhow::Result<()> {
        if self.probe.is_none() {
            anyhow::bail!("run_child_with requires attach_probe first");
        }
        if !matches!(self.probe_stop(), Some(probe::Stop::Request(_))) {
            anyhow::bail!("run_child_with requires the parent to be paused at Stop::Request");
        }
        if options.entry < SPECIAL_TOP
            || !options.entry.is_multiple_of(4)
            || options.entry >= self.mem_size
        {
            anyhow::bail!(
                "child entry must be 4-aligned and inside memory: 0x{:08X}",
                options.entry
            );
        }
        if options.sp < SPECIAL_TOP || !options.sp.is_multiple_of(4) {
            anyhow::bail!(
                "child stack base must be 4-aligned and at least 0x{SPECIAL_TOP:08X}: 0x{:08X}",
                options.sp
            );
        }
        if options.stack_bytes < 4 || !options.stack_bytes.is_multiple_of(4) {
            anyhow::bail!(
                "child stack_bytes must be a positive multiple of 4, got {}",
                options.stack_bytes
            );
        }
        match options.sp.checked_add(options.stack_bytes) {
            Some(end) if end <= self.mem_size => {}
            _ => anyhow::bail!(
                "child stack 0x{:08X}+{} exceeds memory size 0x{:08X}",
                options.sp,
                options.stack_bytes,
                self.mem_size
            ),
        }
        if options.max_steps == 0 || options.max_steps > probe::MAX_PROBE_STEPS {
            anyhow::bail!(
                "child max_steps must be in 1..={}, got {}",
                probe::MAX_PROBE_STEPS,
                options.max_steps
            );
        }
        if options.timeout.is_zero() || options.timeout > probe::MAX_PROBE_TIMEOUT {
            anyhow::bail!(
                "child timeout must be greater than zero and at most {}s, got {:?}",
                probe::MAX_PROBE_TIMEOUT.as_secs(),
                options.timeout
            );
        }
        if options.output_cap == 0 || options.output_cap > probe::MAX_PROBE_OUTPUT {
            anyhow::bail!(
                "child output_cap must be in 1..={}, got {}",
                probe::MAX_PROBE_OUTPUT,
                options.output_cap
            );
        }
        Ok(())
    }

    fn run_child_loop<F>(&mut self, depth: u32, respond: &mut F) -> anyhow::Result<probe::Stop>
    where
        F: FnMut(&probe::HostRequest, &mut Emu, u32) -> anyhow::Result<Vec<u8>>,
    {
        loop {
            let stop = self.run_probe_inner(false);
            match stop {
                probe::Stop::Request(request) => {
                    let response = respond(&request, self, depth)?;
                    self.respond_probe(&response)?;
                }
                other => return Ok(other),
            }
        }
    }

    fn snapshot_cpu(&self) -> CpuSnapshot {
        CpuSnapshot {
            regs: self.regs,
            pc: self.pc,
            segs: self.segs,
            sp: self.sp,
            tm: self.tm,
            status: self.status,
            trap: self.trap,
            music: self.music,
            rs: self.rs,
            epc: self.epc,
            cause: self.cause,
            ksp: self.ksp,
            sege: self.sege,
            trap_stack: self.trap_stack.clone(),
            timer_pending: self.timer_pending,
            last_tick: self.last_tick,
            exit_code: self.exit_code,
            pending_watchpoint: self.pending_watchpoint,
            debug: self.debug,
        }
    }

    fn install_child(&mut self, options: &probe::ChildOptions, child_probe: probe::Probe) {
        self.regs = [0; 16];
        self.pc = options.entry;
        self.segs = 0;
        self.sp = options.sp;
        self.tm = 0;
        self.status = 0;
        self.trap = 0;
        self.music = [0; 4];
        self.rs = 0;
        self.epc = 0;
        self.cause = 0;
        self.ksp = 0;
        self.sege = self.mem_size;
        self.trap_stack.clear();
        self.timer_pending = false;
        self.last_tick = Instant::now();
        self.exit_code = None;
        self.pending_watchpoint = None;
        self.debug = false;
        self.probe = Some(child_probe);
        self.journal = Some(WriteJournal::default());
    }

    fn restore_child(
        &mut self,
        snapshot: CpuSnapshot,
        parent_probe: Option<probe::Probe>,
        committed: bool,
    ) {
        let journal = self.journal.take();
        if !committed && let Some(journal) = journal {
            journal.rollback(&mut self.mem);
        }
        self.clear_instr_cache();
        self.regs = snapshot.regs;
        self.pc = snapshot.pc;
        self.segs = snapshot.segs;
        self.sp = snapshot.sp;
        self.tm = snapshot.tm;
        self.status = snapshot.status;
        self.trap = snapshot.trap;
        self.music = snapshot.music;
        self.rs = snapshot.rs;
        self.epc = snapshot.epc;
        self.cause = snapshot.cause;
        self.ksp = snapshot.ksp;
        self.sege = snapshot.sege;
        self.trap_stack = snapshot.trap_stack;
        self.timer_pending = snapshot.timer_pending;
        self.last_tick = snapshot.last_tick;
        self.exit_code = snapshot.exit_code;
        self.pending_watchpoint = snapshot.pending_watchpoint;
        self.debug = snapshot.debug;
        self.probe = parent_probe;
    }
    fn probe_tick(&mut self) -> Option<probe::Stop> {
        self.probe.as_mut().and_then(probe::Probe::tick)
    }

    fn probe_read_line(&mut self, kind: &'static str) -> Option<String> {
        let probe = self.probe.as_mut()?;
        match probe.read_line() {
            Some(line) => Some(line),
            None => {
                probe.stop(probe::Stop::BlockedIo(kind));
                None
            }
        }
    }

    fn probe_read_char(&mut self, kind: &'static str) -> Option<char> {
        let probe = self.probe.as_mut()?;
        match probe.read_char() {
            Some(ch) => Some(ch),
            None => {
                probe.stop(probe::Stop::BlockedIo(kind));
                None
            }
        }
    }

    fn probe_syscall(&mut self, cur: u32) -> Flow {
        let number = self.regs[0];
        let service = self.regs[1];
        let arg0 = self.regs[2];
        let arg1 = self.regs[3];
        let arg2 = self.regs[4];
        let arg3 = self.regs[5];
        let response_cap = self.regs[6];
        let mut stop = None;
        if number != probe::SYSCALL_NUMBER {
            stop = Some(probe::Stop::ForeignSyscall(number));
        } else if service == probe::SERVICE_COMMIT {
            let handshake = self
                .probe
                .as_ref()
                .is_some_and(probe::Probe::handshake_done);
            if !handshake {
                stop = Some(probe::Stop::BadHandshake(probe::HANDSHAKE_EXPECTED_HELLO));
            } else if arg1 as usize > probe::MAX_PROBE_REQUEST {
                stop = Some(probe::Stop::BadMailbox {
                    service: probe::SERVICE_COMMIT,
                    ptr: arg0,
                    len: arg1,
                });
            } else {
                match self.debug_read_mem(arg0, arg1 as usize) {
                    Ok(bytes) => stop = Some(probe::Stop::Commit(bytes)),
                    Err(_) => {
                        stop = Some(probe::Stop::BadMailbox {
                            service: probe::SERVICE_COMMIT,
                            ptr: arg0,
                            len: arg1,
                        });
                    }
                }
            }
        } else if service == probe::SERVICE_HOST_REQUEST {
            let handshake = self
                .probe
                .as_ref()
                .is_some_and(probe::Probe::handshake_done);
            if !handshake {
                stop = Some(probe::Stop::BadHandshake(probe::HANDSHAKE_EXPECTED_HELLO));
            } else if arg2 as usize > probe::MAX_PROBE_REQUEST {
                stop = Some(probe::Stop::BadMailbox {
                    service: arg0,
                    ptr: arg1,
                    len: arg2,
                });
            } else {
                match self.debug_read_mem(arg1, arg2 as usize) {
                    Ok(bytes) => {
                        if let Some(probe) = &mut self.probe {
                            probe.stop(probe::Stop::Request(probe::HostRequest {
                                service: arg0,
                                bytes,
                                response_ptr: arg3,
                                response_cap,
                            }));
                        }
                    }
                    Err(_) => {
                        stop = Some(probe::Stop::BadMailbox {
                            service: arg0,
                            ptr: arg1,
                            len: arg2,
                        });
                    }
                }
            }
        } else if service == probe::SERVICE_SUSPEND {
            let handshake = self
                .probe
                .as_ref()
                .is_some_and(probe::Probe::handshake_done);
            if !handshake {
                stop = Some(probe::Stop::BadHandshake(probe::HANDSHAKE_EXPECTED_HELLO));
            } else if !matches!(arg1, probe::SUSPEND_OFFER | probe::SUSPEND_NEED) {
                stop = Some(probe::Stop::BadSuspension {
                    request_id: arg0,
                    kind: arg1,
                    ptr: 0,
                });
            } else {
                let owner = probe::read_c_string(arg2, probe::MAX_PROBE_STRING, |addr| {
                    self.probe_read_byte(addr)
                });
                let trait_name = probe::read_c_string(arg3, probe::MAX_PROBE_STRING, |addr| {
                    self.probe_read_byte(addr)
                });
                match (owner, trait_name) {
                    (Some(owner), Some(trait_name)) => {
                        if let Some(probe) = &mut self.probe {
                            probe.stop(probe::Stop::Suspended {
                                request_id: arg0,
                                need: arg1 == probe::SUSPEND_NEED,
                                owner,
                                trait_name,
                            });
                        }
                    }
                    (None, _) => {
                        stop = Some(probe::Stop::BadSuspension {
                            request_id: arg0,
                            kind: arg1,
                            ptr: arg2,
                        });
                    }
                    (_, None) => {
                        stop = Some(probe::Stop::BadSuspension {
                            request_id: arg0,
                            kind: arg1,
                            ptr: arg3,
                        });
                    }
                }
            }
        } else if let Some(probe) = &mut self.probe {
            match probe.handle_syscall(service, arg0, arg1, arg2, arg3) {
                Ok(value) => self.regs[0] = value,
                Err(reason) => stop = Some(reason),
            }
        }
        if let Some(stop) = stop
            && let Some(probe) = &mut self.probe
        {
            probe.stop(stop);
        }
        self.pc = cur.wrapping_add(12);
        Flow::Continue
    }

    /// 加载 `.sfs` raw 内存镜像：字节偏移即地址。
    pub fn load_image(&mut self, image: &[u8]) -> anyhow::Result<()> {
        if image.len() > self.mem_size as usize {
            anyhow::bail!(
                "image size {} exceeds memory size {}",
                image.len(),
                self.mem_size
            );
        }
        self.mem[..image.len()].copy_from_slice(image);
        self.clear_instr_cache();
        Ok(())
    }

    pub fn pc(&self) -> u32 {
        self.pc
    }

    pub fn set_pc(&mut self, pc: u32) {
        self.pc = pc;
    }

    pub fn read_debug_regs(&self) -> Vec<u32> {
        // GDB 默认使用内置 MIPS 寄存器布局（38 个寄存器）。
        // 将 ShyISA 的寄存器映射到 MIPS 布局，未覆盖的寄存器填 0。
        let mut out = vec![0u32; 38];
        out[0..16].copy_from_slice(&self.regs);
        out[29] = self.sp;
        out[32] = self.status;
        out[36] = self.cause;
        out[37] = self.pc;
        out
    }

    pub fn write_debug_regs(&mut self, regs: &[u32]) -> anyhow::Result<()> {
        if regs.len() != 38 {
            anyhow::bail!("expected 38 MIPS-layout registers, got {}", regs.len());
        }
        self.regs.copy_from_slice(&regs[0..16]);
        self.sp = regs[29];
        self.status = regs[32] & 0b11;
        self.cause = regs[36];
        self.pc = regs[37];
        Ok(())
    }

    /// One guest byte under the same checks as an ordinary load, or None when
    /// the address is outside the current segment. Probe string decoding uses
    /// this so a bad pointer becomes a stop instead of a trap.
    fn probe_read_byte(&self, addr: u32) -> Option<u8> {
        let phys = self.translate_mem(addr, 1).ok()?;
        Some(self.mem[phys])
    }

    pub fn debug_read_mem(&self, addr: u32, len: usize) -> anyhow::Result<Vec<u8>> {
        if len == 0 {
            return Ok(Vec::new());
        }
        let phys = self
            .translate_mem(addr, len)
            .map_err(|cause| anyhow::anyhow!("debug memory read failed: {cause:?}"))?;
        Ok(self.mem[phys..phys + len].to_vec())
    }

    pub fn debug_write_mem(&mut self, addr: u32, data: &[u8]) -> anyhow::Result<()> {
        if data.is_empty() {
            return Ok(());
        }
        let phys = self
            .translate_mem(addr, data.len())
            .map_err(|cause| anyhow::anyhow!("debug memory write failed: {cause:?}"))?;
        self.record_write(phys, data.len());
        self.mem[phys..phys + data.len()].copy_from_slice(data);
        self.clear_instr_cache();
        Ok(())
    }

    fn is_user(&self) -> bool {
        self.status & 0b01 == 0b01
    }

    fn interrupts_enabled(&self) -> bool {
        self.status & 0b10 == 0b10
    }

    /// 推进定时器：每 10ms 把 TM 减 1，减到 1 时清零并产生中断请求。
    ///
    /// Probe mode has no wall-clock semantics: TM never advances, so the
    /// result cannot depend on how fast the host is.
    fn tick_timer(&mut self) {
        if self.probe.is_some() {
            return;
        }
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
    ///
    /// Probe mode reports the trap as a structured stop instead of entering
    /// the guest handler; an image without TRAP must not panic the host.
    fn enter_trap(&mut self, cause: TrapCause, epc: u32) {
        if let Some(probe) = &mut self.probe {
            probe.stop(probe::Stop::Trap {
                cause: cause as u32,
                epc,
                pc: self.pc,
            });
            return;
        }
        if self.trap < SPECIAL_TOP {
            panic!(
                "trap occurred with uninitialized TRAP register (TRAP=0x{:08X})",
                self.trap
            );
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
            if let Some(probe) = &mut self.probe {
                probe.stop(probe::Stop::Trap {
                    cause: TrapCause::IllegalInstr as u32,
                    epc: self.pc,
                    pc: self.pc,
                });
                return Flow::Continue;
            }
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

    fn translate_mem(&self, addr: u32, size: usize) -> Result<usize, TrapCause> {
        if addr < SPECIAL_TOP {
            return Err(TrapCause::IllegalAddr);
        }
        let (phys, limit) = if self.is_user() {
            (
                self.segs.checked_add(addr).ok_or(TrapCause::IllegalAddr)?,
                self.sege,
            )
        } else {
            (addr, self.mem_size)
        };
        let end = phys
            .checked_add(size as u32)
            .ok_or(TrapCause::IllegalAddr)?;
        if end > limit || end as usize > self.mem_size as usize {
            return Err(TrapCause::IllegalAddr);
        }
        Ok(phys as usize)
    }

    fn check_mem(&self, addr: u32, aligned: bool, size: usize) -> Result<usize, TrapCause> {
        if aligned && !addr.is_multiple_of(4) {
            return Err(TrapCause::IllegalAddr);
        }
        self.translate_mem(addr, size)
    }

    /// Record old bytes before a child write. No-op for ordinary Emu runs.
    fn record_write(&mut self, phys: usize, len: usize) {
        if let Some(journal) = &mut self.journal {
            let start = journal.old.len();
            journal.old.extend_from_slice(&self.mem[phys..phys + len]);
            journal.entries.push(WriteEntry { phys, start, len });
        }
    }

    pub fn clear_instr_cache(&mut self) {
        self.instr_cache.fill(None);
    }

    fn check_watchpoints(
        watchpoints: &HashSet<Watchpoint>,
        access: MemAccessKind,
        addr: u32,
        size: usize,
    ) -> Option<u32> {
        if size == 0 {
            return None;
        }
        watchpoints.iter().find_map(|watchpoint| {
            if watchpoint.len == 0 {
                return None;
            }
            let kind_matches = matches!(
                (access, watchpoint.kind),
                (
                    MemAccessKind::Read,
                    WatchpointKind::Read | WatchpointKind::Access
                ) | (
                    MemAccessKind::Write,
                    WatchpointKind::Write | WatchpointKind::Access
                )
            );
            if !kind_matches {
                return None;
            }
            let access_end = addr.saturating_add(size as u32);
            let watch_end = watchpoint.addr.saturating_add(watchpoint.len as u32);
            (addr < watch_end && watchpoint.addr < access_end).then_some(watchpoint.addr)
        })
    }

    fn read_mem32_watched(
        &self,
        addr: u32,
        watchpoints: &HashSet<Watchpoint>,
    ) -> MemAccessResult<u32> {
        if let Some(hit) = Self::check_watchpoints(watchpoints, MemAccessKind::Read, addr, 4) {
            return Err(MemAccessErr::Watchpoint(hit));
        }
        let phys = self.check_mem(addr, true, 4)?;
        let s = &self.mem[phys..phys + 4];
        Ok(u32::from_be_bytes([s[0], s[1], s[2], s[3]]))
    }

    #[cfg(test)]
    fn write_mem32(&mut self, addr: u32, val: u32) -> Result<(), TrapCause> {
        self.write_mem32_watched(addr, val, &HashSet::new())
            .map_err(|err| match err {
                MemAccessErr::Trap(cause) => cause,
                MemAccessErr::Watchpoint(_) => unreachable!("empty watchpoint set cannot hit"),
            })
    }

    fn write_mem32_watched(
        &mut self,
        addr: u32,
        val: u32,
        watchpoints: &HashSet<Watchpoint>,
    ) -> MemAccessResult<()> {
        if let Some(hit) = Self::check_watchpoints(watchpoints, MemAccessKind::Write, addr, 4) {
            self.pending_watchpoint.get_or_insert(hit);
        }
        let phys = self.check_mem(addr, true, 4)?;
        self.record_write(phys, 4);
        self.mem[phys..phys + 4].copy_from_slice(&val.to_be_bytes());
        Ok(())
    }

    fn read_mem16_watched(
        &self,
        addr: u32,
        watchpoints: &HashSet<Watchpoint>,
    ) -> MemAccessResult<u32> {
        if let Some(hit) = Self::check_watchpoints(watchpoints, MemAccessKind::Read, addr, 2) {
            return Err(MemAccessErr::Watchpoint(hit));
        }
        let phys = self.check_mem(addr, false, 2)?;
        let s = &self.mem[phys..phys + 2];
        Ok(((s[0] as u32) << 8) | s[1] as u32)
    }

    fn write_mem16_watched(
        &mut self,
        addr: u32,
        val: u32,
        watchpoints: &HashSet<Watchpoint>,
    ) -> MemAccessResult<()> {
        if let Some(hit) = Self::check_watchpoints(watchpoints, MemAccessKind::Write, addr, 2) {
            self.pending_watchpoint.get_or_insert(hit);
        }
        let phys = self.check_mem(addr, false, 2)?;
        self.record_write(phys, 2);
        let bytes = val.to_be_bytes();
        self.mem[phys] = bytes[2];
        self.mem[phys + 1] = bytes[3];
        Ok(())
    }

    fn read_mem8_watched(
        &self,
        addr: u32,
        watchpoints: &HashSet<Watchpoint>,
    ) -> MemAccessResult<u32> {
        if let Some(hit) = Self::check_watchpoints(watchpoints, MemAccessKind::Read, addr, 1) {
            return Err(MemAccessErr::Watchpoint(hit));
        }
        let phys = self.check_mem(addr, false, 1)?;
        Ok(self.mem[phys] as u32)
    }

    fn write_mem8_watched(
        &mut self,
        addr: u32,
        val: u32,
        watchpoints: &HashSet<Watchpoint>,
    ) -> MemAccessResult<()> {
        if let Some(hit) = Self::check_watchpoints(watchpoints, MemAccessKind::Write, addr, 1) {
            self.pending_watchpoint.get_or_insert(hit);
        }
        let phys = self.check_mem(addr, false, 1)?;
        self.record_write(phys, 1);
        self.mem[phys] = val as u8;
        Ok(())
    }

    // ── 特殊映射区（寄存器 / I/O）访问 ────────────────────────────

    /// 读取特殊映射区地址（0x00-0xFF）的 32 位值。
    fn read_reg(&mut self, addr: u32) -> Result<u32, TrapCause> {
        match addr {
            0x00..=0x0F => Ok(self.regs[addr as usize]),
            0x10 => Ok(self.pc),
            0x11 if !self.is_user() => Ok(self.segs),
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
            0x1F if !self.is_user() => Ok(self.sege),
            0x70 if !self.is_user() => Ok(self.read_uart_data()),
            0x71 if !self.is_user() => Ok(self.uart_status()),
            // 受保护寄存器在用户态访问 -> 权限错误
            0x11 | 0x13 | 0x14 | 0x15 | 0x1C | 0x1D | 0x1E | 0x1F | 0x70 | 0x71
                if self.is_user() =>
            {
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
            0x11 if !self.is_user() => self.segs = val,
            0x12 => self.sp = val,
            0x13 if !self.is_user() => self.tm = val,
            0x14 if !self.is_user() => self.status = val & 0b11,
            0x15 if !self.is_user() => self.trap = val,
            0x16..=0x19 => self.music[(addr - 0x16) as usize] = val,
            0x1A => self.rs = val,
            0x1B if self.is_user() => {
                // 用户态写 EXIT 是退出请求，不是停机。内核结束当前进程，
                // 否则一个用户程序就能把整个操作系统停掉。退出码放在 0x，
                // trap 处理函数的第一个参数就是它。
                self.regs[0] = val;
                return Err(TrapCause::UserExit);
            }
            0x1B => {
                // 内核态写 EXIT 仍然直接停机。
                self.exit_code = Some(val);
            }
            0x1C if !self.is_user() => self.epc = val,
            0x1D if !self.is_user() => self.cause = val,
            0x1E if !self.is_user() => self.ksp = val,
            0x1F if !self.is_user() => self.sege = val,
            0x70 if !self.is_user() => self.write_uart_data(val),
            0x71 if !self.is_user() => { /* UART 状态寄存器写入忽略 */ }
            0x11 | 0x13 | 0x14 | 0x15 | 0x1C | 0x1D | 0x1E | 0x1F | 0x70 | 0x71
                if self.is_user() =>
            {
                return Err(TrapCause::Permission);
            }
            _ => return Err(TrapCause::IllegalAddr),
        }
        Ok(())
    }

    /// 读取地址参数的 32 位值：寄存器或普通内存。
    fn r(&mut self, addr: u32, watchpoints: &HashSet<Watchpoint>) -> MemAccessResult<u32> {
        if addr < SPECIAL_TOP {
            self.read_reg(addr).map_err(Into::into)
        } else {
            self.read_mem32_watched(addr, watchpoints)
        }
    }

    /// 写入地址参数的 32 位值：寄存器或普通内存。
    fn w(&mut self, addr: u32, val: u32, watchpoints: &HashSet<Watchpoint>) -> MemAccessResult<()> {
        if addr < SPECIAL_TOP {
            self.write_reg(addr, val).map_err(Into::into)
        } else {
            self.write_mem32_watched(addr, val, watchpoints)
        }
    }

    // ── UART / 宿主 I/O ───────────────────────────────────────────

    fn read_uart_data(&mut self) -> u32 {
        if let Some(probe) = &mut self.probe {
            return match probe.read_byte() {
                Some(byte) => byte as u32,
                None => {
                    probe.stop(probe::Stop::BlockedIo("uart"));
                    0
                }
            };
        }
        // 从标准输入读取一个字节。
        let mut buf = [0u8; 1];
        match self.input.read(&mut buf) {
            Ok(1) => buf[0] as u32,
            _ => 0,
        }
    }

    /// Probe mode captures output in a bounded buffer; normal mode keeps
    /// writing to the inherited stdout exactly as before.
    fn emit_bytes(&mut self, bytes: &[u8]) {
        if let Some(probe) = &mut self.probe {
            probe.push_output(bytes);
            return;
        }
        let _ = self.output.write_all(bytes);
        let _ = self.output.flush();
    }

    fn emit_str(&mut self, text: &str) {
        self.emit_bytes(text.as_bytes());
    }

    fn write_uart_data(&mut self, val: u32) {
        self.emit_bytes(&[val as u8]);
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

    fn read_input_char(&mut self) -> Option<char> {
        loop {
            if let Some(ch) = self.input_chars.pop_front() {
                return Some(ch);
            }
            let line = self.read_input_line()?;
            self.input_chars.extend(line.chars());
        }
    }

    fn write_value(
        &mut self,
        addr: u32,
        value: u32,
        cur: u32,
        watchpoints: &HashSet<Watchpoint>,
    ) -> Flow {
        match self.w(addr, value, watchpoints) {
            Ok(()) => Flow::Continue,
            Err(MemAccessErr::Trap(cause)) => Flow::Trap { cause, epc: cur },
            Err(MemAccessErr::Watchpoint(addr)) => Flow::Watchpoint(addr),
        }
    }

    fn parse_hex_line(line: &str) -> u32 {
        let trimmed = line.trim();
        let hex = trimmed
            .strip_prefix("0x")
            .or_else(|| trimmed.strip_prefix("0X"))
            .unwrap_or(trimmed);
        u32::from_str_radix(hex, 16).unwrap_or(0)
    }

    /// `ina`：从标准输入读取十六进制文本，解析为 u32。
    fn op_ina(&mut self, addr: u32, cur: u32, watchpoints: &HashSet<Watchpoint>) -> Flow {
        if self.probe.is_some() {
            let Some(line) = self.probe_read_line("ina") else {
                return Flow::Continue;
            };
            let val = Self::parse_hex_line(&line);
            return self.write_value(addr, val, cur, watchpoints);
        }
        let Some(line) = self.read_input_line() else {
            return Flow::Exit(0);
        };
        let val = Self::parse_hex_line(&line);
        self.write_value(addr, val, cur, watchpoints)
    }

    /// `inutfa`：从标准输入读取一个 UTF-8 字符，解码为 code point 写入地址。
    /// 输入结束时写入 `0xFFFFFFFF`：它不是合法 code point，程序能据此判断 EOF。
    fn op_inutfa(&mut self, addr: u32, cur: u32, watchpoints: &HashSet<Watchpoint>) -> Flow {
        if self.probe.is_some() {
            let Some(ch) = self.probe_read_char("inutfa") else {
                return Flow::Continue;
            };
            return self.write_value(addr, ch as u32, cur, watchpoints);
        }
        let value = self.read_input_char().map_or(u32::MAX, |ch| ch as u32);
        self.write_value(addr, value, cur, watchpoints)
    }

    fn op_outa(&mut self, addr: u32, cur: u32, watchpoints: &HashSet<Watchpoint>) -> Flow {
        match self.r(addr, watchpoints) {
            Ok(val) => {
                self.emit_str(&format!("{:08X}", val));
                Flow::Continue
            }
            Err(MemAccessErr::Trap(cause)) => Flow::Trap { cause, epc: cur },
            Err(MemAccessErr::Watchpoint(addr)) => Flow::Watchpoint(addr),
        }
    }

    fn op_outn(&mut self, n: u32) -> Flow {
        self.emit_str(&format!("{:08X}", n));
        Flow::Continue
    }

    fn op_oututfa(&mut self, addr: u32, cur: u32, watchpoints: &HashSet<Watchpoint>) -> Flow {
        match self.r(addr, watchpoints) {
            Ok(cp) => self.emit_utf8(cp, cur),
            Err(MemAccessErr::Trap(cause)) => Flow::Trap { cause, epc: cur },
            Err(MemAccessErr::Watchpoint(addr)) => Flow::Watchpoint(addr),
        }
    }

    fn op_oututfn(&mut self, n: u32, cur: u32) -> Flow {
        self.emit_utf8(n, cur)
    }

    /// 把 code point 编码为 UTF-8 输出；无效 code point 触发非法指令 trap。
    fn emit_utf8(&mut self, cp: u32, cur: u32) -> Flow {
        match char::from_u32(cp) {
            Some(ch) => {
                self.emit_str(&ch.to_string());
                Flow::Continue
            }
            None => Flow::Trap {
                cause: TrapCause::IllegalInstr,
                epc: cur,
            },
        }
    }

    // ── 栈操作 ────────────────────────────────────────────────────

    fn push(&mut self, val: u32, cur: u32, watchpoints: &HashSet<Watchpoint>) -> Flow {
        match self.write_mem32_watched(self.sp, val, watchpoints) {
            Ok(()) => {
                self.sp = self.sp.wrapping_add(4);
                Flow::Continue
            }
            Err(MemAccessErr::Trap(cause)) => Flow::Trap { cause, epc: cur },
            Err(MemAccessErr::Watchpoint(addr)) => Flow::Watchpoint(addr),
        }
    }

    fn pop(&mut self, cur: u32, watchpoints: &HashSet<Watchpoint>) -> Result<u32, Flow> {
        let sp = self.sp.wrapping_sub(4);
        match self.read_mem32_watched(sp, watchpoints) {
            Ok(v) => {
                self.sp = sp;
                Ok(v)
            }
            Err(MemAccessErr::Trap(cause)) => {
                self.sp = sp;
                Err(Flow::Trap { cause, epc: cur })
            }
            Err(MemAccessErr::Watchpoint(addr)) => Err(Flow::Watchpoint(addr)),
        }
    }

    // ── 取指 ──────────────────────────────────────────────────────

    fn fetch(&mut self) -> Result<(OpType, u32, u32), FetchErr> {
        if !self.pc.is_multiple_of(4) {
            return Err(TrapCause::IllegalAddr);
        }
        let cache_idx = ((self.pc / 4) as usize) & (INSTR_CACHE_ENTRIES - 1);
        let user = self.is_user();
        let cache_segs = if user { self.segs } else { 0 };
        let cache_sege = if user { self.sege } else { self.mem_size };
        if let Some(entry) = self.instr_cache[cache_idx]
            && entry.pc == self.pc
            && entry.user == user
            && entry.segs == cache_segs
            && entry.sege == cache_sege
        {
            return Ok((entry.op, entry.arg1, entry.arg2));
        }

        let base = self.translate_mem(self.pc, 12)?;
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
            // 0x62-0x6F 保留操作码取指 -> 非法指令；其他值同样非法。
            _ => return Err(TrapCause::IllegalInstr),
        };
        self.instr_cache[cache_idx] = Some(CachedInstr {
            pc: self.pc,
            user,
            segs: cache_segs,
            sege: cache_sege,
            op,
            arg1,
            arg2,
        });
        Ok((op, arg1, arg2))
    }

    // ── 执行 ──────────────────────────────────────────────────────

    #[cfg(test)]
    fn execute(&mut self, op: OpType, a1: u32, a2: u32) -> Flow {
        self.execute_watched(op, a1, a2, &HashSet::new())
    }

    fn execute_watched(
        &mut self,
        op: OpType,
        a1: u32,
        a2: u32,
        watchpoints: &HashSet<Watchpoint>,
    ) -> Flow {
        let cur = self.pc;
        // 默认非控制流指令执行后 PC 推进 12 字节。
        // 控制流指令自行设置 PC 并提前返回。
        macro_rules! r {
            ($e:expr) => {
                match $e {
                    Ok(v) => v,
                    Err(MemAccessErr::Trap(cause)) => return Flow::Trap { cause, epc: cur },
                    Err(MemAccessErr::Watchpoint(addr)) => return Flow::Watchpoint(addr),
                }
            };
        }
        macro_rules! w {
            ($e:expr) => {
                match $e {
                    Ok(()) => {}
                    Err(MemAccessErr::Trap(cause)) => return Flow::Trap { cause, epc: cur },
                    Err(MemAccessErr::Watchpoint(addr)) => return Flow::Watchpoint(addr),
                }
            };
        }

        match op {
            // ── 算术运算 ──
            OpType::Adda => {
                let v = r![self.r(a1, watchpoints)].wrapping_add(r![self.r(a2, watchpoints)]);
                w![self.w(a1, v, watchpoints)];
            }
            OpType::Addn => {
                let v = r![self.r(a1, watchpoints)].wrapping_add(a2);
                w![self.w(a1, v, watchpoints)];
            }
            OpType::Suba => {
                let v = r![self.r(a1, watchpoints)].wrapping_sub(r![self.r(a2, watchpoints)]);
                w![self.w(a1, v, watchpoints)];
            }
            OpType::Subn => {
                let v = r![self.r(a1, watchpoints)].wrapping_sub(a2);
                w![self.w(a1, v, watchpoints)];
            }
            OpType::Mula => {
                let v = r![self.r(a1, watchpoints)].wrapping_mul(r![self.r(a2, watchpoints)]);
                w![self.w(a1, v, watchpoints)];
            }
            OpType::Muln => {
                let v = r![self.r(a1, watchpoints)].wrapping_mul(a2);
                w![self.w(a1, v, watchpoints)];
            }
            OpType::Diva => {
                let d = r![self.r(a2, watchpoints)];
                let n = r![self.r(a1, watchpoints)];
                w![self.w(a1, n.checked_div(d).unwrap_or(0xFFFFFFFF), watchpoints)];
            }
            OpType::Divn => {
                let n = r![self.r(a1, watchpoints)];
                w![self.w(a1, n.checked_div(a2).unwrap_or(0xFFFFFFFF), watchpoints)];
            }
            // ── 位运算 ──
            OpType::Lsa => {
                let sh = r![self.r(a2, watchpoints)];
                let v = r![self.r(a1, watchpoints)];
                w![self.w(a1, if sh >= 32 { 0 } else { v << sh }, watchpoints)];
            }
            OpType::Lsn => {
                let v = r![self.r(a1, watchpoints)];
                w![self.w(a1, if a2 >= 32 { 0 } else { v << a2 }, watchpoints)];
            }
            OpType::Rsa => {
                let sh = r![self.r(a2, watchpoints)];
                let v = r![self.r(a1, watchpoints)];
                w![self.w(a1, if sh >= 32 { 0 } else { v >> sh }, watchpoints)];
            }
            OpType::Rsn => {
                let v = r![self.r(a1, watchpoints)];
                w![self.w(a1, if a2 >= 32 { 0 } else { v >> a2 }, watchpoints)];
            }
            OpType::Anda => {
                let v = r![self.r(a1, watchpoints)] & r![self.r(a2, watchpoints)];
                w![self.w(a1, v, watchpoints)];
            }
            OpType::Andn => {
                let v = r![self.r(a1, watchpoints)] & a2;
                w![self.w(a1, v, watchpoints)];
            }
            OpType::Ora => {
                let v = r![self.r(a1, watchpoints)] | r![self.r(a2, watchpoints)];
                w![self.w(a1, v, watchpoints)];
            }
            OpType::Orn => {
                let v = r![self.r(a1, watchpoints)] | a2;
                w![self.w(a1, v, watchpoints)];
            }
            OpType::Xora => {
                let v = r![self.r(a1, watchpoints)] ^ r![self.r(a2, watchpoints)];
                w![self.w(a1, v, watchpoints)];
            }
            OpType::Xorn => {
                let v = r![self.r(a1, watchpoints)] ^ a2;
                w![self.w(a1, v, watchpoints)];
            }
            OpType::Nota => {
                let v = !r![self.r(a1, watchpoints)];
                w![self.w(a1, v, watchpoints)];
            }
            // ── 比较指令 ──
            OpType::Equa => {
                self.rs = u32::from(r![self.r(a1, watchpoints)] == r![self.r(a2, watchpoints)])
            }
            OpType::Equn => self.rs = u32::from(r![self.r(a1, watchpoints)] == a2),
            OpType::Biga => {
                self.rs = u32::from(r![self.r(a1, watchpoints)] > r![self.r(a2, watchpoints)])
            }
            OpType::Bign => self.rs = u32::from(r![self.r(a1, watchpoints)] > a2),
            OpType::Bigequa => {
                self.rs = u32::from(r![self.r(a1, watchpoints)] >= r![self.r(a2, watchpoints)])
            }
            OpType::Bigequn => self.rs = u32::from(r![self.r(a1, watchpoints)] >= a2),
            OpType::Smaa => {
                self.rs = u32::from(r![self.r(a1, watchpoints)] < r![self.r(a2, watchpoints)])
            }
            OpType::Sman => self.rs = u32::from(r![self.r(a1, watchpoints)] < a2),
            OpType::Smaequa => {
                self.rs = u32::from(r![self.r(a1, watchpoints)] <= r![self.r(a2, watchpoints)])
            }
            OpType::Smaequn => self.rs = u32::from(r![self.r(a1, watchpoints)] <= a2),
            // ── 内存直接操作 ──
            OpType::Seta => {
                let v = r![self.r(a2, watchpoints)];
                w![self.w(a1, v, watchpoints)];
            }
            OpType::Setn => {
                w![self.w(a1, a2, watchpoints)];
            }
            // ── 内存间接操作 ──
            OpType::Geta => {
                let ptr = r![self.r(a2, watchpoints)];
                let v = r![self.read_mem32_watched(ptr, watchpoints)];
                w![self.w(a1, v, watchpoints)];
            }
            OpType::Getn => {
                let v = r![self.read_mem32_watched(a2, watchpoints)];
                w![self.w(a1, v, watchpoints)];
            }
            OpType::Puta => {
                let ptr = r![self.r(a1, watchpoints)];
                let v = r![self.r(a2, watchpoints)];
                w![self.write_mem32_watched(ptr, v, watchpoints)];
            }
            OpType::Putn => {
                let ptr = r![self.r(a1, watchpoints)];
                w![self.write_mem32_watched(ptr, a2, watchpoints)];
            }
            // ── 栈操作 ──
            OpType::Pusha => {
                let v = r![self.r(a1, watchpoints)];
                match self.push(v, cur, watchpoints) {
                    Flow::Continue => {}
                    other => return other,
                }
            }
            OpType::Pushn => match self.push(a1, cur, watchpoints) {
                Flow::Continue => {}
                other => return other,
            },
            OpType::Popa => match self.pop(cur, watchpoints) {
                Ok(v) => w![self.w(a1, v, watchpoints)],
                Err(flow) => return flow,
            },
            OpType::Pop => match self.pop(cur, watchpoints) {
                Ok(_) => {}
                Err(flow) => return flow,
            },
            // ── 控制流 ──
            OpType::Jmpa => {
                if self.rs == 1 {
                    self.rs = 0;
                    let target = r![self.r(a1, watchpoints)];
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
                let target = r![self.r(a1, watchpoints)];
                self.pc = target;
                return Flow::Continue;
            }
            OpType::Ujmpn => {
                self.pc = a1;
                return Flow::Continue;
            }
            OpType::Calla => {
                let target = r![self.r(a1, watchpoints)];
                let ret = cur.wrapping_add(12);
                return match self.push(ret, cur, watchpoints) {
                    Flow::Continue => {
                        self.pc = target;
                        Flow::Continue
                    }
                    other => other,
                };
            }
            OpType::Calln => {
                let ret = cur.wrapping_add(12);
                return match self.push(ret, cur, watchpoints) {
                    Flow::Continue => {
                        self.pc = a1;
                        Flow::Continue
                    }
                    other => other,
                };
            }
            OpType::Ret => match self.pop(cur, watchpoints) {
                Ok(target) => {
                    self.pc = target;
                    return Flow::Continue;
                }
                Err(flow) => return flow,
            },
            // ── I/O 指令 ──
            OpType::Ina => match self.op_ina(a1, cur, watchpoints) {
                Flow::Continue => {}
                other => return other,
            },
            OpType::Inutfa => match self.op_inutfa(a1, cur, watchpoints) {
                Flow::Continue => {}
                other => return other,
            },
            OpType::Outa => match self.op_outa(a1, cur, watchpoints) {
                Flow::Continue => {}
                other => return other,
            },
            OpType::Outn => match self.op_outn(a1) {
                Flow::Continue => {}
                other => return other,
            },
            OpType::Oututfa => match self.op_oututfa(a1, cur, watchpoints) {
                Flow::Continue => {}
                other => return other,
            },
            OpType::Oututfn => match self.op_oututfn(a1, cur) {
                Flow::Continue => {}
                other => return other,
            },
            // ── Trap 指令 ──
            OpType::Syscall => {
                if self.probe.is_some() {
                    return self.probe_syscall(cur);
                }
                let epc = cur.wrapping_add(12);
                return Flow::Trap {
                    cause: TrapCause::Syscall,
                    epc,
                };
            }
            OpType::Iret => return self.iret(),
            // ── 窄内存间接操作 ──
            OpType::Get8a => {
                let ptr = r![self.r(a2, watchpoints)];
                let v = r![self.read_mem8_watched(ptr, watchpoints)];
                w![self.w(a1, v, watchpoints)];
            }
            OpType::Get8n => {
                let v = r![self.read_mem8_watched(a2, watchpoints)];
                w![self.w(a1, v, watchpoints)];
            }
            OpType::Get16a => {
                let ptr = r![self.r(a2, watchpoints)];
                let v = r![self.read_mem16_watched(ptr, watchpoints)];
                w![self.w(a1, v, watchpoints)];
            }
            OpType::Get16n => {
                let v = r![self.read_mem16_watched(a2, watchpoints)];
                w![self.w(a1, v, watchpoints)];
            }
            OpType::Put8a => {
                let ptr = r![self.r(a1, watchpoints)];
                let v = r![self.r(a2, watchpoints)];
                w![self.write_mem8_watched(ptr, v, watchpoints)];
            }
            OpType::Put8n => {
                let ptr = r![self.r(a1, watchpoints)];
                w![self.write_mem8_watched(ptr, a2, watchpoints)];
            }
            OpType::Put16a => {
                let ptr = r![self.r(a1, watchpoints)];
                let v = r![self.r(a2, watchpoints)];
                w![self.write_mem16_watched(ptr, v, watchpoints)];
            }
            OpType::Put16n => {
                let ptr = r![self.r(a1, watchpoints)];
                w![self.write_mem16_watched(ptr, a2, watchpoints)];
            }
            // ── CPU 等待 ──
            OpType::Wait => return self.op_wait(cur),
            // ── 原子内存操作 ──
            OpType::Atoma => {
                let ptr = r![self.r(a1, watchpoints)];
                let new = r![self.r(a2, watchpoints)];
                let old = r![self.read_mem32_watched(ptr, watchpoints)];
                w![self.write_mem32_watched(ptr, new, watchpoints)];
                w![self.w(a2, old, watchpoints)];
            }
            // ── 缓存维护 ──
            OpType::Fencei => {
                self.clear_instr_cache();
            }
            // ── 进入用户态 ──
            OpType::EnterUser => {
                if self.is_user() {
                    return Flow::Trap {
                        cause: TrapCause::IllegalInstr,
                        epc: cur,
                    };
                }
                std::mem::swap(&mut self.sp, &mut self.ksp);
                self.status = 0b11;
                self.pc = a1;
                self.clear_instr_cache();
                return Flow::Continue;
            }
        }

        // 非控制流指令：PC 推进 12 字节。
        self.pc = cur.wrapping_add(12);
        Flow::Continue
    }

    /// `wait`：仅内核态且中断使能时有效。唤醒时 EPC=PC+12。
    fn op_wait(&mut self, cur: u32) -> Flow {
        if let Some(probe) = &mut self.probe {
            probe.stop(probe::Stop::Wait);
            self.pc = cur.wrapping_add(12);
            return Flow::Continue;
        }
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
            self.pc, self.sp, self.status, self.rs, self.tm, self.epc, self.cause
        );
        eprintln!("regs={:08X?}", self.regs);
    }

    fn prepare_for_fetch(&mut self) -> Option<StepOutcome> {
        self.tick_timer();

        // 重新开启中断后交付 pending 定时器（在当前 PC 指向的指令执行前）。
        if self.deliverable_interrupt() {
            self.timer_pending = false;
            self.enter_trap(TrapCause::Timer, self.pc);
            return Some(StepOutcome::Continue);
        }

        self.exit_code.map(StepOutcome::Exited)
    }

    fn execute_one_prepared(&mut self, watchpoints: &HashSet<Watchpoint>) -> StepOutcome {
        if let Some(probe) = &mut self.probe {
            probe.bump_step();
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
                return StepOutcome::Continue;
            }
        };

        self.pending_watchpoint = None;
        match self.execute_watched(op, a1, a2, watchpoints) {
            Flow::Continue => match self.exit_code {
                Some(code) => StepOutcome::Exited(code),
                None => match self.pending_watchpoint.take() {
                    Some(addr) => StepOutcome::Watchpoint(addr),
                    None => StepOutcome::Continue,
                },
            },
            Flow::Exit(code) => {
                self.pending_watchpoint = None;
                StepOutcome::Exited(code)
            }
            Flow::Watchpoint(addr) => {
                self.pending_watchpoint = None;
                StepOutcome::Watchpoint(addr)
            }
            Flow::Trap { cause, epc } => {
                self.pending_watchpoint = None;
                if self.debug {
                    eprintln!("exec trap: cause={cause:?} epc=0x{epc:08X}");
                }
                self.enter_trap(cause, epc);
                StepOutcome::Continue
            }
        }
    }

    pub fn step_one(&mut self) -> StepOutcome {
        self.step_one_with_watchpoints(&HashSet::new())
    }

    pub fn step_one_with_watchpoints(&mut self, watchpoints: &HashSet<Watchpoint>) -> StepOutcome {
        if let Some(outcome) = self.prepare_for_fetch() {
            return outcome;
        }
        self.execute_one_prepared(watchpoints)
    }

    pub fn run_until_stop(
        &mut self,
        breakpoints: &HashSet<u32>,
        watchpoints: &HashSet<Watchpoint>,
        single_step: bool,
    ) -> StopReason {
        let mut ignore_current_breakpoint = breakpoints.contains(&self.pc);
        loop {
            if let Some(outcome) = self.prepare_for_fetch() {
                match outcome {
                    StepOutcome::Continue => continue,
                    StepOutcome::Exited(code) => return StopReason::Exited(code),
                    StepOutcome::Watchpoint(addr) => return StopReason::Watchpoint(addr),
                }
            }

            if ignore_current_breakpoint {
                ignore_current_breakpoint = false;
            } else if breakpoints.contains(&self.pc) {
                return StopReason::Breakpoint;
            }

            match self.execute_one_prepared(watchpoints) {
                StepOutcome::Continue => {
                    if single_step {
                        return StopReason::SingleStep;
                    }
                }
                StepOutcome::Exited(code) => return StopReason::Exited(code),
                StepOutcome::Watchpoint(addr) => return StopReason::Watchpoint(addr),
            }
        }
    }

    /// 运行直到程序退出，返回退出码。
    pub fn run(&mut self) -> u32 {
        loop {
            if let StepOutcome::Exited(code) = self.step_one() {
                return code;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn emu() -> Emu {
        let mut e = Emu::new(false, 0x0100_0000);
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
    fn fetch_cache_requires_fencei_for_modified_instruction() {
        let mut e = emu();
        put(&mut e, 0x100, &encode(0x3E, 0x01, 1));

        assert_eq!(e.fetch().unwrap(), (OpType::Setn, 0x01, 1));
        e.write_mem32(0x108, 2).unwrap();
        assert_eq!(e.fetch().unwrap(), (OpType::Setn, 0x01, 1));

        e.pc = 0x200;
        assert!(matches!(e.execute(OpType::Fencei, 0, 0), Flow::Continue));
        e.pc = 0x100;
        assert_eq!(e.fetch().unwrap(), (OpType::Setn, 0x01, 2));
    }

    #[test]
    fn fetch_cache_separates_user_segments() {
        let mut e = emu();
        e.pc = 0x100;
        e.status = 0b01;
        e.segs = 0x00300000;
        e.sege = 0x00301000;
        put(&mut e, 0x00300100, &encode(0x3E, 0x01, 1));
        put(&mut e, 0x00400100, &encode(0x3E, 0x01, 2));

        assert_eq!(e.fetch().unwrap(), (OpType::Setn, 0x01, 1));
        e.segs = 0x00400000;
        e.sege = 0x00401000;
        assert_eq!(e.fetch().unwrap(), (OpType::Setn, 0x01, 2));
    }

    #[test]
    fn fencei_clears_instruction_translation_cache() {
        let mut e = emu();
        put(&mut e, 0x100, &encode(0x60, 0, 0));

        assert_eq!(e.fetch().unwrap(), (OpType::Fencei, 0, 0));
        assert!(e.instr_cache.iter().any(Option::is_some));

        assert!(matches!(e.execute(OpType::Fencei, 0, 0), Flow::Continue));
        assert!(e.instr_cache.iter().all(Option::is_none));
        assert_eq!(e.pc, 0x10C);
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
        // setn 1x 0x2000
        put(&mut e, 0x100, &encode(0x3E, 0x01, 0x2000));
        // putn 1x 0x12345678
        put(&mut e, 0x10C, &encode(0x42, 0x01, 0x12345678));
        // getn 2x 0x2000
        put(&mut e, 0x118, &encode(0x40, 0x02, 0x2000));
        put(&mut e, 0x124, &encode(0x3E, 0x1B, 0));
        e.run();
        assert_eq!(e.regs[2], 0x12345678);
    }

    #[test]
    fn user_memory_uses_segment_offset() {
        let mut e = emu();
        e.segs = 0x00300000;
        e.sege = 0x00300204;
        e.status = 0b01;
        e.sp = 0x100;
        e.ksp = 0x100000;
        e.trap = 0x200;
        put(&mut e, 0x200, &encode(0x3E, 0x1B, 0)); // kernel: setn exit 0
        put(&mut e, 0x00300200, &0x12345678u32.to_be_bytes());
        put(&mut e, 0x00300100, &encode(0x40, 0x01, 0x200)); // getn 1x 0x200
        put(&mut e, 0x0030010C, &encode(0x3E, 0x1B, 0));
        e.run();
        assert_eq!(e.regs[1], 0x12345678);
    }

    #[test]
    fn segment_end_bounds_user_memory() {
        let mut e = emu();
        e.trap = 0x200;
        e.segs = 0x00300000;
        e.sege = 0x00300202;
        e.status = 0b01;
        e.sp = 0x100;
        e.ksp = 0x100000;
        put(&mut e, 0x00300100, &encode(0x40, 0x01, 0x200)); // 32-bit read crosses SEGE
        put(&mut e, 0x0030010C, &encode(0x3E, 0x1B, 0));
        put(&mut e, 0x200, &encode(0x3E, 0x1B, 4));
        let code = e.run();
        assert_eq!(code, 4);
        assert_eq!(e.cause, 4);
    }

    #[test]
    fn kernel_mode_bypasses_segment_translation() {
        let mut e = emu();
        e.segs = 0x00300000;
        e.sege = 0x00300004;
        put(&mut e, 0x00000200, &0xCAFEBABEu32.to_be_bytes());
        put(&mut e, 0x100, &encode(0x40, 0x01, 0x200));
        put(&mut e, 0x10C, &encode(0x3E, 0x1B, 0));
        e.run();
        assert_eq!(e.regs[1], 0xCAFEBABE);
    }

    #[test]
    fn segment_registers_are_kernel_only() {
        let mut e = emu();
        e.trap = 0x200;
        e.segs = 0x00300000;
        e.sege = 0x00300118;
        e.pc = 0x100;
        e.status = 0b01;
        e.sp = 0x100;
        e.ksp = 0x100000;
        put(&mut e, 0x00300100, &encode(0x3D, 0x01, 0x11)); // seta 1x segs
        put(&mut e, 0x0030010C, &encode(0x3E, 0x1B, 0));
        put(&mut e, 0x200, &encode(0x3E, 0x1B, 5));
        let code = e.run();
        assert_eq!(code, 5);
        assert_eq!(e.cause, 5);
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
        // setn 1x 0x2000
        put(&mut e, 0x100, &encode(0x3E, 0x01, 0x2000));
        // put8n 1x 0xAB
        put(&mut e, 0x10C, &encode(0x5B, 0x01, 0xAB));
        // get8n 2x 0x2000
        put(&mut e, 0x118, &encode(0x57, 0x02, 0x2000));
        put(&mut e, 0x124, &encode(0x3E, 0x1B, 0));
        e.run();
        assert_eq!(e.regs[2], 0xAB);
    }

    #[test]
    fn atoma_atomic_swap() {
        let mut e = emu();
        // 准备内存 0x2000 = 0x11111111
        put(&mut e, 0x2000, &0x11111111u32.to_be_bytes());
        // setn 1x 0x2000
        put(&mut e, 0x100, &encode(0x3E, 0x01, 0x2000));
        // setn 2x 0x22222222
        put(&mut e, 0x10C, &encode(0x3E, 0x02, 0x22222222));
        // atoma 1x 2x
        put(&mut e, 0x118, &encode(0x5F, 0x01, 0x02));
        put(&mut e, 0x124, &encode(0x3E, 0x1B, 0));
        e.run();
        // 内存被写入新值，2x 得到旧值。
        let mem_val =
            u32::from_be_bytes([e.mem[0x2000], e.mem[0x2001], e.mem[0x2002], e.mem[0x2003]]);
        assert_eq!(mem_val, 0x22222222);
        assert_eq!(e.regs[2], 0x11111111);
    }

    #[test]
    fn illegal_instruction_traps() {
        let mut e = emu();
        e.trap = 0x200; // 设置 trap 入口
        put(&mut e, 0x100, &encode(0x62, 0, 0));
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
    fn user_mode_low_memory_is_segment_relative() {
        let mut e = emu();
        e.segs = 0x00300000;
        e.sege = 0x00300204;
        e.status = 0b01;
        e.sp = 0x100;
        e.ksp = 0x100000;
        e.trap = 0x300;
        put(&mut e, 0x300, &encode(0x3E, 0x1B, 0)); // kernel: setn exit 0
        put(&mut e, 0x00000200, &0xDEADBEEFu32.to_be_bytes());
        put(&mut e, 0x00300200, &0xCAFEBABEu32.to_be_bytes());
        put(&mut e, 0x00300100, &encode(0x40, 0x01, 0x200));
        put(&mut e, 0x0030010C, &encode(0x3E, 0x1B, 0));
        e.run();
        assert_eq!(e.regs[1], 0xCAFEBABE);
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
        let mut e = Emu::new(false, 0x0100_0000);
        e.last_tick = Instant::now() + Duration::from_secs(3600);
        e.trap = 0x200;
        e.status = 0b01; // 用户态
        e.sp = 0x200000; // 用户栈
        e.ksp = 0x100000; // 内核栈
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
        put(&mut e, 0x100, &encode(0x62, 0, 0)); // 非法指令
        put(&mut e, 0x10C, &encode(0x3E, 0x1B, 0));
        let _ = e.run();
    }

    #[test]
    fn enteruser_switches_to_user_segment_and_swaps_stacks() {
        let mut e = emu();
        e.sp = 0x00100000;
        e.ksp = 0x003ff000;
        e.segs = 0x00400000;
        e.sege = 0x00800000;
        e.trap = 0x20200;
        put(&mut e, 0x20200, &encode(0x3D, 0x1B, 0x00)); // kernel: seta exit 0x
        put(&mut e, 0x100, &encode(0x61, 0x100, 0)); // enteruser 0x100
        put(&mut e, 0x00400100, &encode(0x3E, 0x1B, 7)); // user: setn exit 7
        e.step_one();
        assert_eq!(e.status, 0b11);
        assert_eq!(e.sp, 0x003ff000);
        assert_eq!(e.ksp, 0x00100000);
        let code = e.run();
        assert_eq!(code, 7);
    }

    #[test]
    fn enteruser_in_user_mode_traps_as_illegal_instruction() {
        let mut e = emu();
        e.trap = 0x200;
        e.status = 0b01;
        e.sp = 0x100;
        e.ksp = 0x100000;
        e.segs = 0x00300000;
        e.sege = 0x00300200;
        put(&mut e, 0x00300100, &encode(0x61, 0x100, 0));
        put(&mut e, 0x200, &encode(0x3E, 0x1B, 3));
        let code = e.run();
        assert_eq!(code, 3);
        assert_eq!(e.cause, 3);
    }

    #[test]
    #[should_panic(expected = "empty trap status stack")]
    fn iret_with_empty_stack_panics() {
        let mut e = emu();
        put(&mut e, 0x100, &encode(0x55, 0, 0)); // iret
        let _ = e.run();
    }
}
