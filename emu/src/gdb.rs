use std::collections::HashSet;
use std::io::{ErrorKind, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;

use anyhow::{Context, Result};

use crate::cpu::{Emu, StepOutcome, StopReason, Watchpoint, WatchpointKind};

const DEFAULT_SIGNAL: &str = "S05";
const SHYISA_ENTRY: u32 = 0x0000_0100;
const SHYISA_INSTR_SIZE: u32 = 12;

enum SessionExit {
    Detach,
    Kill,
}

pub fn run_gdb_server(
    emu: &mut Emu,
    port: u16,
    wait: bool,
    exec_file: Option<PathBuf>,
) -> Result<()> {
    let listener = TcpListener::bind(("127.0.0.1", port))
        .with_context(|| format!("failed to bind GDB RSP server on 127.0.0.1:{port}"))?;
    eprintln!(
        "GDB RSP listening on 127.0.0.1:{port}, pc=0x{:08X}",
        emu.pc()
    );

    let (stream, peer) = if wait {
        listener
            .accept()
            .with_context(|| "failed to accept GDB connection")?
    } else {
        listener
            .set_nonblocking(true)
            .with_context(|| "failed to set GDB listener nonblocking")?;
        loop {
            match listener.accept() {
                Ok(conn) => break conn,
                Err(err) if err.kind() == ErrorKind::WouldBlock => match emu.step_one() {
                    StepOutcome::Continue => {}
                    StepOutcome::Exited(code) => std::process::exit((code & 0xFF) as i32),
                    StepOutcome::Watchpoint(_) => {}
                },
                Err(err) => return Err(err).with_context(|| "failed to accept GDB connection"),
            }
        }
    };

    eprintln!("GDB RSP client connected from {peer}");
    match GdbSession::new(stream, exec_file).run(emu)? {
        SessionExit::Detach => {
            let code = emu.run();
            std::process::exit((code & 0xFF) as i32);
        }
        SessionExit::Kill => std::process::exit(0),
    }
}

struct GdbSession {
    stream: TcpStream,
    no_ack: bool,
    breakpoints: HashSet<u32>,
    watchpoints: HashSet<Watchpoint>,
    last_stop: String,
    exited: Option<u32>,
    exec_file: Option<String>,
}

impl GdbSession {
    fn new(stream: TcpStream, exec_file: Option<PathBuf>) -> Self {
        Self {
            stream,
            no_ack: false,
            breakpoints: HashSet::new(),
            watchpoints: HashSet::new(),
            last_stop: DEFAULT_SIGNAL.to_string(),
            exited: None,
            exec_file: exec_file.map(|path| path.to_string_lossy().into_owned()),
        }
    }

    fn run(&mut self, emu: &mut Emu) -> Result<SessionExit> {
        loop {
            let packet = match self.read_packet()? {
                Some(p) => p,
                None => return Ok(SessionExit::Detach),
            };
            let response = match self.handle_packet(emu, &packet)? {
                PacketResult::Reply(response) => response,
                PacketResult::Detach => {
                    self.write_packet("OK")?;
                    return Ok(SessionExit::Detach);
                }
                PacketResult::Kill => return Ok(SessionExit::Kill),
            };
            self.write_packet(&response)?;
        }
    }

    fn read_packet(&mut self) -> Result<Option<String>> {
        loop {
            let mut start = [0u8; 1];
            match self.stream.read(&mut start) {
                Ok(0) => return Ok(None),
                Ok(1) => match start[0] {
                    b'$' => break,
                    b'\x03' => {
                        if !self.no_ack {
                            self.stream.write_all(b"+")?;
                        }
                        return Ok(Some("\x03".to_string()));
                    }
                    _ => continue,
                },
                Ok(_) => continue,
                Err(e) => return Err(e.into()),
            }
        }

        let mut payload = Vec::new();
        loop {
            let mut b = [0u8; 1];
            match self.stream.read(&mut b) {
                Ok(0) => return Ok(None),
                Ok(1) => {
                    if b[0] == b'#' {
                        break;
                    }
                    payload.push(b[0]);
                }
                Ok(_) => continue,
                Err(e) => return Err(e.into()),
            }
        }

        let mut checksum = [0u8; 2];
        self.stream.read_exact(&mut checksum)?;
        let expected = parse_hex_byte(checksum[0], checksum[1])
            .ok_or_else(|| anyhow::anyhow!("invalid packet checksum"))?;
        let actual = payload.iter().fold(0u8, |sum, b| sum.wrapping_add(*b));
        if actual != expected {
            if !self.no_ack {
                self.stream.write_all(b"-")?;
            }
            return self.read_packet();
        }
        if !self.no_ack {
            self.stream.write_all(b"+")?;
        }
        Ok(Some(
            String::from_utf8(payload).with_context(|| "GDB packet was not valid UTF-8")?,
        ))
    }

    fn write_packet(&mut self, payload: &str) -> Result<()> {
        let checksum = payload
            .as_bytes()
            .iter()
            .fold(0u8, |sum, b| sum.wrapping_add(*b));
        write!(self.stream, "${payload}#{checksum:02x}")?;
        self.stream.flush()?;
        Ok(())
    }

    fn handle_packet(&mut self, emu: &mut Emu, packet: &str) -> Result<PacketResult> {
        if packet == "\x03" {
            self.last_stop = stop_reply(StopReason::Interrupt);
            return Ok(PacketResult::Reply(self.last_stop.clone()));
        }

        if packet == "qSupported" || packet.starts_with("qSupported:") {
            let mut supported =
                "PacketSize=4000;QStartNoAckMode+;swbreak+;hwbreak+;watch+;vContSupported+"
                    .to_string();
            if self.exec_file.is_some() {
                supported.push_str(";qXfer:exec-file:read+");
            }
            return Ok(PacketResult::Reply(supported));
        }
        if packet == "QStartNoAckMode" {
            self.no_ack = true;
            return Ok(PacketResult::Reply("OK".to_string()));
        }

        match packet {
            "?" => return Ok(PacketResult::Reply(self.last_stop.clone())),
            "g" => return Ok(PacketResult::Reply(encode_regs(&emu.read_debug_regs()))),
            "Hc-1" | "Hc0" | "Hc1" | "Hg-1" | "Hg0" | "Hg1" => {
                return Ok(PacketResult::Reply("OK".to_string()));
            }
            "qC" => return Ok(PacketResult::Reply("QC1".to_string())),
            "qfThreadInfo" => return Ok(PacketResult::Reply("m1".to_string())),
            "qsThreadInfo" => return Ok(PacketResult::Reply("l".to_string())),
            "T1" => return Ok(PacketResult::Reply("OK".to_string())),
            "vCont?" => return Ok(PacketResult::Reply("vCont;c;s".to_string())),
            "D" => return Ok(PacketResult::Detach),
            "k" => return Ok(PacketResult::Kill),
            _ => {}
        }

        if let Some(hex) = packet.strip_prefix('G') {
            let regs = decode_regs(hex)?;
            emu.write_debug_regs(&regs)?;
            return Ok(PacketResult::Reply("OK".to_string()));
        }
        if let Some(rest) = packet.strip_prefix('m') {
            return Ok(PacketResult::Reply(match parse_addr_len(rest) {
                Some((addr, len)) => match emu.debug_read_mem(addr, len) {
                    Ok(bytes) => encode_bytes(&bytes),
                    Err(_) => "E14".to_string(),
                },
                None => "E01".to_string(),
            }));
        }
        if let Some(rest) = packet.strip_prefix('M') {
            return Ok(PacketResult::Reply(match write_memory(emu, rest) {
                Ok(()) => "OK".to_string(),
                Err(_) => "E14".to_string(),
            }));
        }
        if let Some(rest) = packet.strip_prefix('c') {
            return self.resume(emu, rest, false);
        }
        if let Some(rest) = packet.strip_prefix('s') {
            return self.resume(emu, rest, true);
        }
        if packet == "vCont;c" || packet.starts_with("vCont;c:") {
            return self.resume(emu, "", false);
        }
        if packet == "vCont;s" || packet.starts_with("vCont;s:") {
            return self.resume(emu, "", true);
        }
        if let Some(rest) = packet.strip_prefix("qXfer:exec-file:read:") {
            return Ok(PacketResult::Reply(self.read_exec_file(rest)));
        }
        if let Some(rest) = packet.strip_prefix('Z') {
            return Ok(PacketResult::Reply(self.set_breakpoint(rest, true)));
        }
        if let Some(rest) = packet.strip_prefix('z') {
            return Ok(PacketResult::Reply(self.set_breakpoint(rest, false)));
        }

        Ok(PacketResult::Reply(String::new()))
    }

    fn resume(&mut self, emu: &mut Emu, addr: &str, single_step: bool) -> Result<PacketResult> {
        if let Some(code) = self.exited {
            let reply = format!("W{:02x}", code & 0xFF);
            self.last_stop = reply.clone();
            return Ok(PacketResult::Reply(reply));
        }

        if !addr.is_empty() {
            let pc = parse_u32_hex(addr).ok_or_else(|| anyhow::anyhow!("invalid resume addr"))?;
            emu.set_pc(pc);
        }

        let reason = if single_step {
            emu.run_until_stop(&self.breakpoints, &self.watchpoints, true)
        } else {
            self.continue_until_stop(emu)?
        };
        if let StopReason::Exited(code) = reason {
            self.exited = Some(code);
        }
        let reply = stop_reply(reason);
        self.last_stop = reply.clone();
        Ok(PacketResult::Reply(reply))
    }

    fn continue_until_stop(&mut self, emu: &mut Emu) -> Result<StopReason> {
        self.stream
            .set_nonblocking(true)
            .with_context(|| "failed to set GDB stream nonblocking")?;
        let result = self.continue_until_stop_inner(emu);
        let restore = self.stream.set_nonblocking(false);
        if let Err(err) = restore {
            return Err(err).with_context(|| "failed to restore blocking GDB stream");
        }
        result
    }

    fn continue_until_stop_inner(&mut self, emu: &mut Emu) -> Result<StopReason> {
        let mut ignore_current_breakpoint = self.breakpoints.contains(&emu.pc());
        loop {
            if self.ctrl_c_pending()? {
                return Ok(StopReason::Interrupt);
            }
            if ignore_current_breakpoint {
                ignore_current_breakpoint = false;
            } else if self.breakpoints.contains(&emu.pc()) {
                return Ok(StopReason::Breakpoint);
            }
            match emu.step_one_with_watchpoints(&self.watchpoints) {
                StepOutcome::Continue => {}
                StepOutcome::Exited(code) => return Ok(StopReason::Exited(code)),
                StepOutcome::Watchpoint(addr) => return Ok(StopReason::Watchpoint(addr)),
            }
        }
    }

    fn ctrl_c_pending(&mut self) -> Result<bool> {
        let mut buf = [0u8; 1];
        match self.stream.peek(&mut buf) {
            Ok(0) => {
                // Client closed the connection; treat it as an interrupt so
                // that the main loop reads EOF and performs a clean detach.
                Ok(true)
            }
            Ok(_) if buf[0] == b'\x03' => {
                self.stream.read_exact(&mut buf)?;
                Ok(true)
            }
            Ok(_) => Ok(false),
            Err(err) if err.kind() == ErrorKind::WouldBlock => Ok(false),
            Err(err) => Err(err).with_context(|| "failed to poll GDB stream"),
        }
    }

    fn set_breakpoint(&mut self, spec: &str, insert: bool) -> String {
        let mut parts = spec.split(',');
        let Some(kind) = parts.next() else {
            return "E01".to_string();
        };
        let Some(addr) = parts.next().and_then(parse_u32_hex) else {
            return "E01".to_string();
        };
        if kind == "0" || kind == "1" {
            let addr = normalize_breakpoint_addr(addr);
            if insert {
                self.breakpoints.insert(addr);
            } else {
                self.breakpoints.remove(&addr);
            }
            return "OK".to_string();
        }

        let Some(len) = parts.next().and_then(parse_usize_hex) else {
            return "E01".to_string();
        };
        if len == 0 {
            return "E01".to_string();
        }
        let watch_kind = match kind {
            "2" => WatchpointKind::Write,
            "3" => WatchpointKind::Read,
            "4" => WatchpointKind::Access,
            _ => return String::new(),
        };
        let watchpoint = Watchpoint::new(addr, len, watch_kind);
        if insert {
            self.watchpoints.insert(watchpoint);
        } else {
            self.watchpoints.remove(&watchpoint);
        }
        "OK".to_string()
    }

    fn read_exec_file(&self, spec: &str) -> String {
        let Some(path) = self.exec_file.as_ref() else {
            return String::new();
        };
        let Some((_annex, range)) = spec.split_once(':') else {
            return "E01".to_string();
        };
        let Some((offset, len)) = parse_addr_len(range) else {
            return "E01".to_string();
        };

        let bytes = path.as_bytes();
        let offset = offset as usize;
        if offset >= bytes.len() {
            return "l".to_string();
        }
        let end = bytes.len().min(offset + len);
        let more = end < bytes.len();
        let mut out = String::new();
        out.push(if more { 'm' } else { 'l' });
        out.push_str(&encode_binary_packet(&bytes[offset..end]));
        out
    }
}

enum PacketResult {
    Reply(String),
    Detach,
    Kill,
}

fn encode_regs(regs: &[u32]) -> String {
    let mut out = String::with_capacity(regs.len() * 8);
    for reg in regs {
        out.push_str(&format!("{reg:08x}"));
    }
    out
}

fn decode_regs(hex: &str) -> Result<Vec<u32>> {
    if hex.len() != 38 * 8 {
        anyhow::bail!("invalid register packet length");
    }
    let mut regs = Vec::with_capacity(38);
    for chunk in hex.as_bytes().chunks_exact(8) {
        let s = std::str::from_utf8(chunk)?;
        regs.push(u32::from_str_radix(s, 16)?);
    }
    Ok(regs)
}

fn encode_bytes(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

fn encode_binary_packet(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len());
    for b in bytes {
        match *b {
            b'#' | b'$' | b'}' | b'*' => {
                out.push('}');
                out.push((*b ^ 0x20) as char);
            }
            b => out.push(b as char),
        }
    }
    out
}

fn decode_bytes(hex: &str) -> Result<Vec<u8>> {
    if hex.len() % 2 != 0 {
        anyhow::bail!("odd-length hex byte string");
    }
    let mut out = Vec::with_capacity(hex.len() / 2);
    for pair in hex.as_bytes().chunks_exact(2) {
        let hi = hex_nibble(pair[0]).ok_or_else(|| anyhow::anyhow!("invalid hex"))?;
        let lo = hex_nibble(pair[1]).ok_or_else(|| anyhow::anyhow!("invalid hex"))?;
        out.push((hi << 4) | lo);
    }
    Ok(out)
}

fn write_memory(emu: &mut Emu, spec: &str) -> Result<()> {
    let Some((range, data_hex)) = spec.split_once(':') else {
        anyhow::bail!("missing memory payload");
    };
    let Some((addr, len)) = parse_addr_len(range) else {
        anyhow::bail!("invalid memory range");
    };
    let data = decode_bytes(data_hex)?;
    if data.len() != len {
        anyhow::bail!("memory payload length mismatch");
    }
    emu.debug_write_mem(addr, &data)
}

fn parse_addr_len(spec: &str) -> Option<(u32, usize)> {
    let (addr, len) = spec.split_once(',')?;
    Some((parse_u32_hex(addr)?, parse_usize_hex(len)?))
}

fn parse_u32_hex(s: &str) -> Option<u32> {
    let s = s.strip_prefix("0x").unwrap_or(s);
    u32::from_str_radix(s, 16).ok()
}

fn normalize_breakpoint_addr(addr: u32) -> u32 {
    if addr < SHYISA_ENTRY {
        return addr;
    }
    let offset = addr - SHYISA_ENTRY;
    let rem = offset % SHYISA_INSTR_SIZE;
    if rem == 0 {
        addr
    } else {
        addr.saturating_add(SHYISA_INSTR_SIZE - rem)
    }
}

fn parse_usize_hex(s: &str) -> Option<usize> {
    let s = s.strip_prefix("0x").unwrap_or(s);
    usize::from_str_radix(s, 16).ok()
}

fn parse_hex_byte(hi: u8, lo: u8) -> Option<u8> {
    Some((hex_nibble(hi)? << 4) | hex_nibble(lo)?)
}

fn hex_nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn stop_reply(reason: StopReason) -> String {
    match reason {
        StopReason::Breakpoint | StopReason::SingleStep => DEFAULT_SIGNAL.to_string(),
        StopReason::Exited(code) => format!("W{:02x}", code & 0xFF),
        StopReason::Interrupt => "S02".to_string(),
        StopReason::Watchpoint(addr) => format!("T05watch:{:x};thread:1;", addr),
    }
}
