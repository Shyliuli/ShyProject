//! Opt-in constfn probe execution.
//!
//! syscall 999 is not an ISA feature. The ISA says syscall raises CAUSE=1 and
//! jumps to TRAP (see ShyISA.md and cpu.rs). An ordinary Emu has no probe and
//! therefore keeps that exact behavior. Only Emu::attach_probe (or the
//! high-level probe::run) installs the host service described here.
//!
//! Register convention for probe syscalls:
//!
//! | register | meaning                                                |
//! |----------|--------------------------------------------------------|
//! | 0x       | service family, always 999                             |
//! | 1x       | service number                                         |
//! | 2x       | argument 0 (HELLO magic, RESULT id, SUSPEND request id) |
//! | 3x       | argument 1 (HELLO ABI, RESULT kind, SUSPEND queue kind) |
//! | 4x       | argument 2 (RESULT low, HOST request len, SUSPEND owner) |
//! | 5x       | argument 3 (RESULT high, HOST response ptr, SUSPEND trait) |
//! | 6x       | argument 4 (HOST response cap)                         |
//!
//! Service 0 (HELLO) requires magic 0x50524642 ("PRFB") in 2x and ABI version
//! 1 in 3x. It must be the first probe call and is answered with 0 in 0x.
//! Service 1 (EXIT) stops the probe with Stop::Done; 2x is copied to the
//! outcome's exit field for diagnostics.
//! Service 2 (RESULT) appends a ResultFrame built from 2x=id, 3x=kind,
//! 4x=low and 5x=high. Results are accepted only after HELLO. Supported kinds
//! are 0=int32, 1=int64 (low is the least significant 32 bits, high the most
//! significant) and 2=unit. RESULT is scalar-only in ABI v1: Tokens, Str,
//! aggregates and arbitrary COMMIT bytes do not fit low/high and must use
//! HOST_REQUEST instead. RESULT_KIND_STRING is reserved but rejected today.
//! Service 3 (HOST_REQUEST) pauses the probe with Stop::Request. Its mailbox
//! layout is:
//!
//! 2x = host service id
//! 3x = request pointer
//! 4x = request length
//! 5x = response pointer
//! 6x = response capacity
//! request bytes:  [request_ptr, request_ptr + request_len)
//! response bytes: [response_ptr, response_ptr + response_cap)
//!
//! The request bytes are copied with the same segment/bounds checks as a
//! normal user-memory access. Emu::respond_probe writes at most response_cap
//! bytes, stores the response length in 0x, clears the stop, and execution
//! resumes at the already-advanced PC. This module never interprets the
//! payload; host hooks stay outside the emulator.
//!
//! Service 5 (SUSPEND) ends the run with Stop::Suspended. It is not a
//! HOST_REQUEST: there is no response mailbox and nothing resumes the guest.
//! Its arguments are:
//!
//! 2x = request id
//! 3x = queue kind: 1 = need, 0 = offer
//! 4x = owner C string pointer
//! 5x = trait C string pointer
//!
//! Both strings are copied byte by byte under the same segment/bounds checks
//! as a normal user-memory access. Each must be valid UTF-8 and NUL-terminated
//! within MAX_PROBE_STRING bytes, terminator included.
//!
//! A probe call never enters the guest trap handler: it does not change
//! STATUS, push the trap stack, or swap SP/KSP. The PC advances by 12 before
//! returning to the run loop. A syscall with a number other than 999 is
//! reported as ForeignSyscall and stops the probe.

use std::time::{Duration, Instant};

use anyhow::{Result, bail};

use crate::cpu::DEFAULT_MEM_SIZE;

/// Probe syscall family number carried in register 0 (0x).
pub const SYSCALL_NUMBER: u32 = 999;
/// ASCII "PRFB" in big-endian order.
pub const MAGIC: u32 = 0x5052_4642;
/// Probe ABI version carried in register 3 (3x) during HELLO.
pub const ABI_VERSION: u32 = 1;
/// Service 0: HELLO.
pub const SERVICE_HELLO: u32 = 0;
/// Service 1: EXIT.
pub const SERVICE_EXIT: u32 = 1;
/// Service 2: RESULT.
pub const SERVICE_RESULT: u32 = 2;
/// Service 3: HOST_REQUEST (pausable request/response mailbox).
pub const SERVICE_HOST_REQUEST: u32 = 3;
/// Service 4: COMMIT. The guest names a byte range in its own memory and the
/// run stops with a private copy of it, so a result too large for a RESULT
/// frame travels as one blob. Register 2 holds the pointer, register 3 the
/// length.
pub const SERVICE_COMMIT: u32 = 4;
/// Service 5: SUSPEND. A generator reports the impl it needs or offers, then
/// stops with Stop::Suspended.
pub const SERVICE_SUSPEND: u32 = 5;

/// SUSPEND queue kind: the generator needs an impl.
pub const SUSPEND_NEED: u32 = 1;
/// SUSPEND queue kind: the generator offers an impl.
pub const SUSPEND_OFFER: u32 = 0;

/// Upper bound for a SUSPEND owner or trait C string, NUL terminator
/// included. A string that does not terminate inside this many bytes is
/// rejected instead of scanning further.
pub const MAX_PROBE_STRING: usize = 4096;

/// Upper bound for a HOST_REQUEST request payload.
pub const MAX_PROBE_REQUEST: usize = 1024 * 1024;
/// Upper bound for a HOST_REQUEST response payload.
pub const MAX_PROBE_RESPONSE: usize = 1024 * 1024;

/// RESULT kind: signed 32-bit value in low.
pub const RESULT_KIND_INT32: u32 = 0;
/// RESULT kind: 64-bit value with low = least significant word.
pub const RESULT_KIND_INT64: u32 = 1;
/// RESULT kind: no payload.
pub const RESULT_KIND_UNIT: u32 = 2;
/// RESULT kind reserved for a future guest-memory string encoding.
pub const RESULT_KIND_STRING: u32 = 3;

/// Maximum number of RESULT frames kept for one run. Fuel and timeout still
/// bound instructions; this cap keeps a guest result loop from growing host
/// memory without bound.
pub const MAX_PROBE_RESULTS: usize = 4096;

/// BadHandshake reason: HELLO was not the first probe call.
pub const HANDSHAKE_EXPECTED_HELLO: u32 = 1;
/// BadHandshake reason: the magic in 2x was wrong.
pub const HANDSHAKE_BAD_MAGIC: u32 = 2;
/// BadHandshake reason: the ABI version in 3x was wrong.
pub const HANDSHAKE_BAD_ABI: u32 = 3;
/// BadHandshake reason: HELLO was sent more than once.
pub const HANDSHAKE_DUPLICATE: u32 = 4;

/// Upper bound for ProbeOptions::mem_size. The probe runs small programs;
/// refusing multi-gigabyte allocations keeps a typo from exhausting the host.
pub const MAX_PROBE_MEM_SIZE: u32 = 64 * 1024 * 1024;
/// Upper bound for ProbeOptions::max_steps.
pub const MAX_PROBE_STEPS: u64 = 100_000_000;
/// Upper bound for ProbeOptions::timeout.
pub const MAX_PROBE_TIMEOUT: Duration = Duration::from_secs(60);
/// Upper bound for ProbeOptions::output_cap.
pub const MAX_PROBE_OUTPUT: usize = 1024 * 1024;

/// A pausable HOST_REQUEST copied out of guest memory.
///
/// The request pointer/length are not repeated here: bytes already contains a
/// private copy. response_ptr/response_cap describe where Emu::respond_probe
/// may write the answer under the same user-memory checks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostRequest {
    /// Caller-defined host service id (2x).
    pub service: u32,
    /// Safely copied request payload.
    pub bytes: Vec<u8>,
    /// Guest response buffer address (5x).
    pub response_ptr: u32,
    /// Guest response buffer capacity in bytes (6x).
    pub response_cap: u32,
}

/// Why a probe run stopped.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Stop {
    /// HELLO passed and the guest completed service EXIT.
    Done,
    /// The guest wrote the kernel EXIT register.
    Exited(u32),
    /// A trap was raised. Probe mode reports it instead of entering TRAP.
    Trap { cause: u32, epc: u32, pc: u32 },
    /// A blocking ISA input was reached and injected input was exhausted.
    BlockedIo(&'static str),
    /// The guest executed wait; probe mode has no timer to wake it.
    Wait,
    /// The guest issued a syscall whose number was not 999.
    ForeignSyscall(u32),
    /// The guest used a service number this ABI does not define.
    UnknownService(u32),
    /// A HOST_REQUEST paused the run; call Emu::respond_probe to resume.
    Request(HostRequest),
    /// SERVICE_SUSPEND stopped the run with a private copy of the request.
    Suspended {
        /// Caller-defined request id (2x).
        request_id: u32,
        /// Queue kind in 3x: true for need (1), false for offer (0).
        need: bool,
        /// Copied owner C string (4x).
        owner: String,
        /// Copied trait C string (5x).
        trait_name: String,
    },
    /// SERVICE_COMMIT stopped the run with a private copy of a guest blob.
    Commit(Vec<u8>),
    /// A HOST_REQUEST mailbox could not be copied safely or was too large.
    BadMailbox { service: u32, ptr: u32, len: u32 },
    /// A SERVICE_SUSPEND argument was invalid: kind was not 0/1, or a string
    /// was unreadable, not NUL-terminated within MAX_PROBE_STRING bytes, or
    /// not UTF-8. ptr is the offending string pointer, or 0 for a bad kind.
    BadSuspension {
        request_id: u32,
        kind: u32,
        ptr: u32,
    },
    /// A RESULT used a kind this ABI does not support yet (for example string).
    UnsupportedResultKind(u32),
    /// A RESULT exceeded MAX_PROBE_RESULTS.
    TooManyResults { max: usize },
    /// HELLO was missing, repeated, or carried the wrong magic/ABI.
    BadHandshake(u32),
    /// The step budget was exhausted before the guest stopped.
    OutOfFuel { steps: u64 },
    /// The wall-clock budget was exhausted before the guest stopped.
    Timeout { millis: u64 },
}

/// Options for one probe run.
///
/// The defaults are intentionally small and deterministic: empty input,
/// captured output, 32 MiB of memory, one million instructions and two
/// seconds. All resource limits must be finite; zero or absurd values are
/// rejected before the image is loaded.
#[derive(Clone, Debug)]
pub struct ProbeOptions {
    /// Guest memory size in bytes.
    pub mem_size: u32,
    /// Instruction budget. Must be greater than zero.
    pub max_steps: u64,
    /// Wall-clock budget. Must be greater than zero.
    pub timeout: Duration,
    /// Bytes returned by guest input instructions. Empty means "not injected";
    /// the first read then stops with BlockedIo instead of blocking on stdin.
    pub input: Vec<u8>,
    /// Capture UART/out instruction output. When false, output is discarded.
    pub capture_output: bool,
    /// Maximum number of output bytes retained.
    pub output_cap: usize,
    /// Print a per-instruction trace to stderr. Intended for the CLI --debug.
    pub debug: bool,
}

impl Default for ProbeOptions {
    fn default() -> Self {
        Self {
            mem_size: DEFAULT_MEM_SIZE,
            max_steps: 32_000_000,
            timeout: Duration::from_secs(2),
            input: Vec::new(),
            capture_output: true,
            output_cap: 4096,
            debug: false,
        }
    }
}

/// Options for one nested child run.
///
/// The child shares the parent's guest memory and runs in kernel mode with a
/// fresh register file and the caller-provided stack. parent_depth is the
/// current host-call depth; the child handler receives parent_depth + 1.
#[derive(Clone, Debug)]
pub struct ChildOptions {
    /// Opaque compiler site id, echoed in ChildResult.
    pub site: u32,
    /// Child entry PC.
    pub entry: u32,
    /// Child stack base; ShyISA stack grows upward from here.
    pub sp: u32,
    /// Reserved/validated stack extent; no soft stack guard in this version.
    pub stack_bytes: u32,
    /// Current host-call depth; the child runs at parent_depth + 1.
    pub parent_depth: u32,
    /// Child instruction budget.
    pub max_steps: u64,
    /// Child wall-clock budget.
    pub timeout: Duration,
    /// Maximum captured child output bytes.
    pub output_cap: usize,
}

/// Result of a nested child run.
#[derive(Debug)]
pub struct ChildResult {
    /// Opaque compiler site id from ChildOptions.
    pub site: u32,
    /// Nesting depth the child ran at (parent_depth + 1).
    pub depth: u32,
    /// True when child memory writes were kept.
    pub committed: bool,
    /// Child final stop: Done/Exited(0) on success, the failure reason otherwise.
    pub stop: Stop,
    /// Full child exit code when it reached an EXIT path.
    pub exit: u32,
    /// Captured child output; empty when not committed.
    pub stdout: Vec<u8>,
    /// Child RESULT frames; empty when not committed.
    pub results: Vec<ResultFrame>,
    /// Number of child instructions attempted.
    pub steps: u64,
}
impl ProbeOptions {
    pub(crate) fn validate(&self) -> Result<()> {
        if self.mem_size == 0 || self.mem_size > MAX_PROBE_MEM_SIZE {
            bail!(
                "probe mem_size must be in 1..={MAX_PROBE_MEM_SIZE} bytes, got {}",
                self.mem_size
            );
        }
        if self.max_steps == 0 || self.max_steps > MAX_PROBE_STEPS {
            bail!(
                "probe max_steps must be in 1..={MAX_PROBE_STEPS}, got {}",
                self.max_steps
            );
        }
        if self.timeout.is_zero() || self.timeout > MAX_PROBE_TIMEOUT {
            bail!(
                "probe timeout must be greater than zero and at most {}s, got {:?}",
                MAX_PROBE_TIMEOUT.as_secs(),
                self.timeout
            );
        }
        if self.output_cap == 0 || self.output_cap > MAX_PROBE_OUTPUT {
            bail!(
                "probe output_cap must be in 1..={MAX_PROBE_OUTPUT} bytes, got {}",
                self.output_cap
            );
        }
        Ok(())
    }
}

/// One scalar RESULT service call (ABI v1).
///
/// For RESULT_KIND_INT32 the value is in low. For RESULT_KIND_INT64 the value
/// is (high as u64) << 32 | low as u64. RESULT_KIND_UNIT ignores low/high.
/// Non-scalar values (Tokens, Str, aggregates, arbitrary COMMIT bytes) belong
/// to HOST_REQUEST; this frame is deliberately scalar-only.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResultFrame {
    /// Caller-defined result site id.
    pub id: u32,
    /// One of the RESULT_KIND_* constants.
    pub kind: u32,
    /// Low 32 bits of the payload.
    pub low: u32,
    /// High 32 bits of the payload.
    pub high: u32,
}

/// Structured result of probe::run.
#[derive(Debug)]
pub struct ProbeOutcome {
    /// Why execution stopped.
    pub stop: Stop,
    /// Full 32-bit guest exit code (not truncated to eight bits).
    pub exit: u32,
    /// Captured stdout/UART bytes. Empty when capture_output is false.
    pub stdout: Vec<u8>,
    /// Scalar RESULT frames in guest emission order. Only meaningful when
    /// stop is Stop::Done; a caller must not consume them on any other stop.
    /// Non-scalar host results travel through Stop::Request instead.
    pub results: Vec<ResultFrame>,
    /// Number of instructions attempted.
    pub steps: u64,
    /// True when output exceeded output_cap and the tail was dropped.
    pub output_truncated: bool,
}

/// Runtime state installed by Emu::attach_probe.
pub(crate) struct Probe {
    options: ProbeOptions,
    hello: bool,
    stop: Option<Stop>,
    steps: u64,
    started: Instant,
    output: Vec<u8>,
    output_truncated: bool,
    exit: u32,
    input_pos: usize,
    results: Vec<ResultFrame>,
}

impl Probe {
    pub(crate) fn new(options: ProbeOptions) -> Self {
        Self {
            options,
            hello: false,
            stop: None,
            steps: 0,
            started: Instant::now(),
            output: Vec::new(),
            output_truncated: false,
            exit: 0,
            input_pos: 0,
            results: Vec::new(),
        }
    }

    pub(crate) fn new_child(options: ProbeOptions) -> Self {
        let mut probe = Self::new(options);
        // A compiler-generated child entry is a function body, not another
        // full probe wrapper; do not require a second HELLO.
        probe.hello = true;
        probe
    }

    pub(crate) fn begin_run(&mut self) {
        self.started = Instant::now();
    }

    pub(crate) fn stop_reason(&self) -> Option<Stop> {
        self.stop.clone()
    }

    pub(crate) fn stop(&mut self, stop: Stop) {
        if self.stop.is_none() {
            if let Stop::Exited(code) = &stop {
                self.exit = *code;
            }
            self.stop = Some(stop);
        }
    }

    pub(crate) fn handshake_done(&self) -> bool {
        self.hello
    }

    pub(crate) fn request(&self) -> Option<&HostRequest> {
        match &self.stop {
            Some(Stop::Request(request)) => Some(request),
            _ => None,
        }
    }

    pub(crate) fn clear_request(&mut self) {
        if matches!(self.stop, Some(Stop::Request(_))) {
            self.stop = None;
        }
    }

    pub(crate) fn bump_step(&mut self) {
        self.steps += 1;
    }

    pub(crate) fn steps(&self) -> u64 {
        self.steps
    }

    pub(crate) fn add_steps(&mut self, steps: u64) {
        self.steps = self.steps.saturating_add(steps);
    }

    pub(crate) fn remaining_steps(&self) -> u64 {
        self.options.max_steps.saturating_sub(self.steps)
    }

    pub(crate) fn remaining_timeout(&self) -> Duration {
        self.options.timeout.saturating_sub(self.started.elapsed())
    }

    pub(crate) fn elapsed_millis(&self) -> u64 {
        self.started.elapsed().as_millis() as u64
    }

    pub(crate) fn exit(&self) -> u32 {
        self.exit
    }

    pub(crate) fn take_output(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.output)
    }

    pub(crate) fn output_truncated(&self) -> bool {
        self.output_truncated
    }

    pub(crate) fn take_results(&mut self) -> Vec<ResultFrame> {
        std::mem::take(&mut self.results)
    }

    /// Check the stop flag, fuel and wall clock before the next instruction.
    pub(crate) fn tick(&mut self) -> Option<Stop> {
        if let Some(stop) = &self.stop {
            return Some(stop.clone());
        }
        if self.steps >= self.options.max_steps {
            let stop = Stop::OutOfFuel { steps: self.steps };
            self.stop = Some(stop.clone());
            return Some(stop);
        }
        // Checking the clock every 1024 steps keeps the hot path cheap while
        // still bounding a run by wall time.
        if self.steps.is_multiple_of(1024) {
            let elapsed = self.started.elapsed();
            if elapsed > self.options.timeout {
                let stop = Stop::Timeout {
                    millis: elapsed.as_millis() as u64,
                };
                self.stop = Some(stop.clone());
                return Some(stop);
            }
        }
        None
    }

    /// Handle a syscall 999. Returns the value to place in 0x, or the stop
    /// that ends the run.
    pub(crate) fn handle_syscall(
        &mut self,
        service: u32,
        arg0: u32,
        arg1: u32,
        arg2: u32,
        arg3: u32,
    ) -> std::result::Result<u32, Stop> {
        if service == SERVICE_HELLO {
            if self.hello {
                return Err(Stop::BadHandshake(HANDSHAKE_DUPLICATE));
            }
            if arg0 != MAGIC {
                return Err(Stop::BadHandshake(HANDSHAKE_BAD_MAGIC));
            }
            if arg1 != ABI_VERSION {
                return Err(Stop::BadHandshake(HANDSHAKE_BAD_ABI));
            }
            self.hello = true;
            return Ok(0);
        }
        if !self.hello {
            return Err(Stop::BadHandshake(HANDSHAKE_EXPECTED_HELLO));
        }
        match service {
            SERVICE_EXIT => {
                self.exit = arg0;
                Err(Stop::Done)
            }
            SERVICE_RESULT => {
                if self.results.len() >= MAX_PROBE_RESULTS {
                    return Err(Stop::TooManyResults {
                        max: MAX_PROBE_RESULTS,
                    });
                }
                if !matches!(
                    arg1,
                    RESULT_KIND_INT32 | RESULT_KIND_INT64 | RESULT_KIND_UNIT
                ) {
                    return Err(Stop::UnsupportedResultKind(arg1));
                }
                self.results.push(ResultFrame {
                    id: arg0,
                    kind: arg1,
                    low: arg2,
                    high: arg3,
                });
                Ok(0)
            }
            _ => Err(Stop::UnknownService(service)),
        }
    }

    pub(crate) fn push_output(&mut self, bytes: &[u8]) {
        if !self.options.capture_output || bytes.is_empty() {
            return;
        }
        let remaining = self.options.output_cap.saturating_sub(self.output.len());
        let take = remaining.min(bytes.len());
        self.output.extend_from_slice(&bytes[..take]);
        if take < bytes.len() {
            self.output_truncated = true;
        }
    }

    pub(crate) fn read_byte(&mut self) -> Option<u8> {
        let byte = self.options.input.get(self.input_pos).copied()?;
        self.input_pos += 1;
        Some(byte)
    }

    pub(crate) fn read_line(&mut self) -> Option<String> {
        let input = &self.options.input;
        let mut pos = self.input_pos;
        if pos >= input.len() {
            return None;
        }
        let start = pos;
        while pos < input.len() {
            let byte = input[pos];
            pos += 1;
            if byte == b'\n' {
                break;
            }
        }
        self.input_pos = pos;
        Some(String::from_utf8_lossy(&input[start..pos]).into_owned())
    }

    pub(crate) fn read_char(&mut self) -> Option<char> {
        let input = &self.options.input;
        let pos = self.input_pos;
        if pos >= input.len() {
            return None;
        }
        let rest = &input[pos..];
        let (ch, width) = match std::str::from_utf8(rest) {
            Ok(text) => {
                let ch = text.chars().next()?;
                (ch, ch.len_utf8())
            }
            Err(error) => {
                let valid = std::str::from_utf8(&rest[..error.valid_up_to()]).ok()?;
                match valid.chars().next() {
                    Some(ch) => (ch, ch.len_utf8()),
                    // Invalid leading byte: keep deterministic byte semantics.
                    None => (rest[0] as char, 1),
                }
            }
        };
        self.input_pos = pos + width;
        Some(ch)
    }
}

/// Copy one NUL-terminated guest C string.
///
/// read_byte maps a guest address to one byte using the caller's normal
/// user-memory checks; returning None means the address is not readable.
/// At most max bytes are examined, terminator included. The result must be
/// valid UTF-8, so a non-UTF-8 payload is rejected like an unterminated one.
pub(crate) fn read_c_string(
    ptr: u32,
    max: usize,
    mut read_byte: impl FnMut(u32) -> Option<u8>,
) -> Option<String> {
    let mut bytes = Vec::new();
    for offset in 0..max {
        let addr = ptr.checked_add(offset as u32)?;
        let byte = read_byte(addr)?;
        if byte == 0 {
            return String::from_utf8(bytes).ok();
        }
        bytes.push(byte);
    }
    None
}

fn finish(mut emu: crate::cpu::Emu, stop: Stop) -> ProbeOutcome {
    ProbeOutcome {
        stop,
        exit: emu.probe_exit(),
        stdout: emu.take_probe_output(),
        results: emu.take_probe_results(),
        steps: emu.probe_steps(),
        output_truncated: emu.probe_output_truncated(),
    }
}

/// Run a linked .sfs image under the opt-in probe protocol.
///
/// The image is executed in a fresh Emu. This function is an explicit opt-in:
/// it always installs the probe, so syscall 999 is intercepted only here (and
/// through Emu::attach_probe). An ordinary Emu::new is untouched.
///
/// This function does not answer HOST_REQUEST. If the guest pauses with
/// Stop::Request, the returned outcome carries the request but the Emu is
/// gone; use run_with or drive Emu::run_probe/respond_probe directly.
pub fn run(image: &[u8], options: ProbeOptions) -> Result<ProbeOutcome> {
    options.validate()?;
    let mut emu = crate::cpu::Emu::new(options.debug, options.mem_size);
    emu.load_image(image)?;
    emu.attach_probe(options)?;
    let stop = emu.run_probe();
    Ok(finish(emu, stop))
}

/// Run a linked .sfs image and answer HOST_REQUEST calls with a callback.
///
/// The callback receives a private copy of the request bytes and returns the
/// response bytes. Emu::respond_probe copies the response back to the guest
/// mailbox under the normal user-memory checks, sets 0x to the response
/// length, and resumes after the syscall. Any error aborts the run.
pub fn run_with<F>(image: &[u8], options: ProbeOptions, mut respond: F) -> Result<ProbeOutcome>
where
    F: FnMut(&HostRequest) -> Result<Vec<u8>>,
{
    options.validate()?;
    let mut emu = crate::cpu::Emu::new(options.debug, options.mem_size);
    emu.load_image(image)?;
    emu.attach_probe(options)?;
    loop {
        let stop = emu.run_probe();
        match stop {
            Stop::Request(request) => {
                let response = respond(&request)?;
                emu.respond_probe(&response)?;
            }
            other => return Ok(finish(emu, other)),
        }
    }
}
