//! Per-function code generation: frame, prologue, blocks, edges, and calls.
//!
//! Every SSA value is materialised in a frame spill slot. Scratch registers
//! are used only inside one instruction:
//!
//! - `1x`/`2x`: first operand, result low/high word;
//! - `3x`/`cx`: second operand, memory address, byte-copy source;
//! - `dx`: address scratch and byte-copy temporary;
//! - `ex`: indirect call target and a helper temporary;
//! - `4x`..`bx`: ABI argument registers;
//! - `fx`: frame pointer.
//!
//! No value lives in a register across instruction boundaries, so calls,
//! branches, and inline runtime helpers can freely clobber caller-saved
//! registers.

use super::abi::{self, part_mask};
use super::frame::{Frame, is_64};
use super::{Options, fallback_span};
use crate::abi::{CallPlan, Location, Passing};
use crate::ir::{Block, BlockId, Function, Operand, Terminator, Type, ValueId};
use crate::source::{Diagnostic, Span};

/// 64-bit helper routines that this function calls.
#[derive(Default)]
pub(crate) struct Helpers {
    pub(crate) u64_mul: bool,
    pub(crate) u64_shl: bool,
    pub(crate) u64_lshr: bool,
    pub(crate) u64_ashr: bool,
    pub(crate) u64_div: bool,
    pub(crate) u64_mod: bool,
    pub(crate) i64_div: bool,
    pub(crate) i64_mod: bool,
}

pub(crate) struct FnWriter<'a> {
    pub(crate) function: &'a Function,
    pub(crate) frame: Frame,
    pub(crate) plan: CallPlan,
    pub(crate) out: String,
    pub(crate) comments: bool,
    pub(crate) labels: u32,
    pub(crate) index: usize,
    pub(crate) epilogue: String,
    pub(crate) helpers: Helpers,
}

pub(crate) fn emit(
    function: &Function,
    index: usize,
    options: &Options,
) -> Result<String, Diagnostic> {
    if function.layout != options.layout {
        return Err(Diagnostic::new(
            fallback_span(),
            format!(
                "function '{}' was built for a different target layout than the backend options",
                function.name
            ),
        ));
    }
    let plan = abi::plan_function(function, fallback_span())?;
    if plan.args.len() != function.signature.params.len() {
        return Err(Diagnostic::new(
            fallback_span(),
            format!(
                "Shy ABI v1 could not classify every parameter of '{}'",
                function.name
            ),
        ));
    }
    let frame = Frame::build(function, options.layout);
    let epilogue = format!(".L.shyc.{index}.return");
    let mut writer = FnWriter {
        function,
        frame,
        plan,
        out: String::new(),
        comments: options.emit_source_comments,
        labels: 0,
        index,
        epilogue,
        helpers: Helpers::default(),
    };
    writer.run()?;
    Ok(writer.out)
}

impl FnWriter<'_> {
    fn run(&mut self) -> Result<(), Diagnostic> {
        if self
            .function
            .blocks
            .get(self.function.entry.0 as usize)
            .and_then(Option::as_ref)
            .is_none()
        {
            return Err(Diagnostic::new(
                fallback_span(),
                format!(
                    "entry block of function '{}' was removed",
                    self.function.name
                ),
            ));
        }

        self.line(format!(".section text.{}", self.function.name));
        self.line(format!(".symbol {}", self.function.name));
        self.prologue()?;

        let entry = self.block_label(self.function.entry);
        self.line(format!("ujmpn {entry}"));

        let function = self.function;
        for (id, block) in function.live_blocks() {
            self.block(id, block)?;
        }

        self.line(format!("{}:", self.epilogue));
        self.line("seta sp fx");
        self.line("popa fx");
        self.line("ret");
        self.runtime_helpers();
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Output helpers
    // -----------------------------------------------------------------------

    pub(crate) fn line(&mut self, text: impl AsRef<str>) {
        self.out.push_str(text.as_ref());
        self.out.push('\n');
    }

    pub(crate) fn fresh(&mut self, tag: &str) -> String {
        let index = self.labels;
        self.labels += 1;
        format!(".L.shyc.{}.{}.{}", self.index, tag, index)
    }

    pub(crate) fn block_label(&self, block: BlockId) -> String {
        format!(".L.shyc.{}.bb{}", self.index, block.0)
    }

    pub(crate) fn helper_label(&self, name: &str) -> String {
        format!(".L.shyc.{}.{}", self.index, name)
    }

    pub(crate) fn comment(&mut self, span: Option<Span>) {
        if !self.comments {
            return;
        }
        if let Some(span) = span {
            self.line(format!("//source file {}:{}", span.file.0, span.start));
        }
    }

    pub(crate) fn call_helper(&mut self, name: &str) {
        match name {
            "u64mul" => self.helpers.u64_mul = true,
            "u64shl" => self.helpers.u64_shl = true,
            "u64lshr" => self.helpers.u64_lshr = true,
            "u64ashr" => self.helpers.u64_ashr = true,
            "u64div" => self.helpers.u64_div = true,
            "u64mod" => self.helpers.u64_mod = true,
            "i64div" => self.helpers.i64_div = true,
            "i64mod" => self.helpers.i64_mod = true,
            _ => unreachable!("unknown Shy helper"),
        }
        let label = self.helper_label(name);
        self.line(format!("calln {label}"));
    }

    // -----------------------------------------------------------------------
    // Frame access
    // -----------------------------------------------------------------------

    pub(crate) fn frame_addr(&mut self, offset: u32, register: &str) {
        if offset == 0 {
            self.line(format!("seta {register} fx"));
        } else {
            self.line(format!("setn {register} {offset}"));
            self.line(format!("adda {register} fx"));
        }
    }

    pub(crate) fn load_word(&mut self, offset: u32, register: &str) {
        self.frame_addr(offset, "dx");
        self.line(format!("geta {register} dx"));
    }

    pub(crate) fn store_word(&mut self, offset: u32, register: &str) {
        self.frame_addr(offset, "dx");
        self.line(format!("puta dx {register}"));
    }

    pub(crate) fn load_pair(&mut self, offset: u32, high: &str, low: &str) {
        self.frame_addr(offset, "dx");
        self.line(format!("geta {high} dx"));
        self.line("addn dx 4");
        self.line(format!("geta {low} dx"));
    }

    pub(crate) fn store_pair(&mut self, offset: u32, high: &str, low: &str) {
        self.frame_addr(offset, "dx");
        self.line(format!("puta dx {high}"));
        self.line("addn dx 4");
        self.line(format!("puta dx {low}"));
    }

    pub(crate) fn store_value_word(&mut self, value: ValueId, register: &str) {
        let offset = self.frame.value(value);
        self.store_word(offset, register);
    }

    pub(crate) fn store_value_pair(&mut self, value: ValueId, high: &str, low: &str) {
        let offset = self.frame.value(value);
        self.store_pair(offset, high, low);
    }

    /// Byte copy used by aggregate loads/stores, stack argument copies, and
    /// `memcpy`. It clobbers `cx`, `dx`, and both address registers.
    pub(crate) fn copy_bytes(
        &mut self,
        dst: &str,
        src: &str,
        bytes: u32,
    ) -> Result<(), Diagnostic> {
        if bytes == 0 {
            return Ok(());
        }
        let loop_label = self.fresh("copy");
        let done_label = self.fresh("copy_done");
        self.line(format!("setn cx {bytes}"));
        self.line(format!("{loop_label}:"));
        self.line("equn cx 0");
        self.line(format!("jmpn {done_label}"));
        self.line(format!("get8a dx {src}"));
        self.line(format!("put8a {dst} dx"));
        self.line(format!("addn {src} 1"));
        self.line(format!("addn {dst} 1"));
        self.line("subn cx 1");
        self.line(format!("ujmpn {loop_label}"));
        self.line(format!("{done_label}:"));
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Prologue
    // -----------------------------------------------------------------------

    fn prologue(&mut self) -> Result<(), Diagnostic> {
        let layout = self.function.layout;
        let signature = self.function.signature.clone();
        self.line("pusha fx");
        self.line("seta fx sp");
        if self.frame.size() > 0 {
            self.line(format!("addn sp {}", self.frame.size()));
        }
        if let Some(sret) = self.frame.sret() {
            self.store_word(sret, "4x");
        }

        let plan_args = self.plan.args.clone();
        for (index, argument) in plan_args.iter().enumerate() {
            let ty = signature.params[index];
            let value = self.function.params[index];
            if ty.is_float() {
                return Err(Diagnostic::new(
                    fallback_span(),
                    "floating-point parameters are not implemented by the Shy backend",
                ));
            }
            if ty.is_aggregate() {
                let offset = match &argument.passing {
                    Passing::Direct { parts } => {
                        parts.iter().find_map(|part| match part.location {
                            Location::Stack { offset } => Some(offset),
                            Location::Register(_) => None,
                        })
                    }
                    Passing::Indirect { .. } | Passing::Ignore => None,
                }
                .ok_or_else(|| {
                    Diagnostic::new(
                        fallback_span(),
                        format!(
                            "Shy ABI v1 did not place aggregate parameter {index} on the stack"
                        ),
                    )
                })?;
                let source = (offset - 8) as u32;
                let object = self.frame.object(value);
                self.frame_addr(source, "1x");
                self.frame_addr(object, "3x");
                let bytes = ty.size(layout).unwrap_or(0);
                self.copy_bytes("3x", "1x", bytes)?;
                self.frame_addr(object, "1x");
                let spill = self.frame.value(value);
                self.store_word(spill, "1x");
                continue;
            }

            let Passing::Direct { parts } = &argument.passing else {
                return Err(Diagnostic::new(
                    fallback_span(),
                    format!("unsupported Shy ABI v1 classification for parameter {index}"),
                ));
            };
            for part in parts {
                match part.location {
                    Location::Register(register) => {
                        let name = abi::argument_register(register.index).ok_or_else(|| {
                            Diagnostic::new(
                                fallback_span(),
                                format!(
                                    "Shy ABI v1 used argument register slot {} for parameter {index}",
                                    register.index
                                ),
                            )
                        })?;
                        let mask = part_mask(ty, part.size);
                        if let Some(mask) = mask {
                            self.line(format!("andn {name} 0x{mask:x}"));
                        }
                        let spill = self.frame.value(value) + part.source_offset;
                        self.store_word(spill, name);
                    }
                    Location::Stack { offset } => {
                        let address = (offset - 8) as u32;
                        self.frame_addr(address, "dx");
                        let spill = self.frame.value(value);
                        match part.size {
                            8 => {
                                self.line("geta 2x dx");
                                self.line("addn dx 4");
                                self.line("geta 1x dx");
                                self.store_pair(spill, "2x", "1x");
                            }
                            1 => {
                                self.line("get8a 1x dx");
                                self.store_word(spill, "1x");
                            }
                            2 => {
                                self.line("get16a 1x dx");
                                self.store_word(spill, "1x");
                            }
                            4 => {
                                self.line("geta 1x dx");
                                self.store_word(spill, "1x");
                            }
                            size => {
                                return Err(Diagnostic::new(
                                    fallback_span(),
                                    format!(
                                        "Shy ABI v1 stack parameter {index} has unsupported size {size}"
                                    ),
                                ));
                            }
                        }
                    }
                }
            }
        }
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Blocks and control flow
    // -----------------------------------------------------------------------

    fn block(&mut self, id: BlockId, block: &Block) -> Result<(), Diagnostic> {
        self.line(format!("{}:", self.block_label(id)));
        for phi in &block.phis {
            if phi.ty.is_aggregate() {
                return Err(Diagnostic::new(
                    phi.span.unwrap_or_else(fallback_span),
                    "aggregate phi values are not implemented by the Shy backend",
                ));
            }
            if phi.ty.is_float() {
                return Err(Diagnostic::new(
                    phi.span.unwrap_or_else(fallback_span),
                    "floating-point phi values are not implemented by the Shy backend",
                ));
            }
        }
        for instruction in &block.insts {
            self.instruction(instruction)?;
        }
        self.terminator(id, &block.terminator)
    }

    fn terminator(&mut self, from: BlockId, terminator: &Terminator) -> Result<(), Diagnostic> {
        self.comment(terminator.span());
        match terminator {
            Terminator::Jump { target, .. } => {
                self.emit_phi_copies(from, *target)?;
                self.line(format!("ujmpn {}", self.block_label(*target)));
            }
            Terminator::Branch {
                condition,
                then_block,
                else_block,
                ..
            } => {
                self.load_operand(condition, "1x", None)?;
                let then_label = self.fresh("edge");
                let else_label = self.fresh("edge");
                self.line("equn 1x 0");
                self.line(format!("jmpn {else_label}"));
                self.line(format!("ujmpn {then_label}"));
                self.emit_edge(from, *then_block, &then_label)?;
                self.emit_edge(from, *else_block, &else_label)?;
            }
            Terminator::Switch {
                value,
                ty,
                default,
                cases,
                ..
            } => {
                self.load_operand(value, "1x", Some("2x"))?;
                let mut edges = Vec::with_capacity(cases.len());
                for case in cases {
                    let label = self.fresh("edge");
                    if is_64(*ty) {
                        let bits = case.value.as_u64();
                        let low = bits as u32;
                        let high = (bits >> 32) as u32;
                        let check = self.fresh("case");
                        let next = self.fresh("case");
                        self.line(format!("equn 2x 0x{high:x}"));
                        self.line(format!("jmpn {check}"));
                        self.line(format!("ujmpn {next}"));
                        self.line(format!("{check}:"));
                        self.line(format!("equn 1x 0x{low:x}"));
                        self.line(format!("jmpn {label}"));
                        self.line(format!("{next}:"));
                    } else {
                        let bits = case.value.as_u64() as u32;
                        self.line(format!("equn 1x 0x{bits:x}"));
                        self.line(format!("jmpn {label}"));
                    }
                    edges.push((label, case.target));
                }
                let default_label = self.fresh("edge");
                self.line(format!("ujmpn {default_label}"));
                for (label, target) in edges {
                    self.emit_edge(from, target, &label)?;
                }
                self.emit_edge(from, *default, &default_label)?;
            }
            Terminator::Return { value, span } => {
                self.emit_return(value.as_ref(), *span)?;
                self.line(format!("ujmpn {}", self.epilogue));
            }
            Terminator::Unreachable { .. } => {
                let label = self.block_label(from);
                self.line(format!("ujmpn {label}"));
            }
        }
        Ok(())
    }

    fn emit_edge(&mut self, from: BlockId, to: BlockId, label: &str) -> Result<(), Diagnostic> {
        self.line(format!("{label}:"));
        self.emit_phi_copies(from, to)?;
        self.line(format!("ujmpn {}", self.block_label(to)));
        Ok(())
    }

    /// Lower phi nodes as a parallel copy through a frame temporary.
    ///
    /// All inputs are first written to per-phi temporaries and only then
    /// written to the phi values, which makes the copy simultaneous even when
    /// one phi input is another phi of the same block.
    fn emit_phi_copies(&mut self, from: BlockId, to: BlockId) -> Result<(), Diagnostic> {
        let function = self.function;
        let block = function.block(to);
        if block.phis.is_empty() {
            return Ok(());
        }
        for phi in &block.phis {
            let temp = self.frame.phi(phi.result).ok_or_else(|| {
                Diagnostic::new(
                    phi.span.unwrap_or_else(fallback_span),
                    "phi value has no frame temporary",
                )
            })?;
            let incoming = phi.incoming_for(from).ok_or_else(|| {
                Diagnostic::new(
                    phi.span.unwrap_or_else(fallback_span),
                    format!(
                        "phi {} has no incoming value for predecessor {}",
                        phi.result, from
                    ),
                )
            })?;
            if is_64(phi.ty) {
                self.load_operand(incoming, "1x", Some("2x"))?;
                self.store_pair(temp, "2x", "1x");
            } else {
                self.load_operand(incoming, "1x", None)?;
                self.store_word(temp, "1x");
            }
        }
        for phi in &block.phis {
            let temp = self.frame.phi(phi.result).expect("phi temporary");
            if is_64(phi.ty) {
                self.load_pair(temp, "2x", "1x");
                self.store_value_pair(phi.result, "2x", "1x");
            } else {
                self.load_word(temp, "1x");
                self.store_value_word(phi.result, "1x");
            }
        }
        Ok(())
    }

    fn emit_return(
        &mut self,
        value: Option<&Operand>,
        span: Option<Span>,
    ) -> Result<(), Diagnostic> {
        let layout = self.function.layout;
        let ret = self.function.signature.ret;
        if ret.is_float() {
            return Err(Diagnostic::new(
                span.unwrap_or_else(fallback_span),
                "floating-point return values are not implemented by the Shy backend",
            ));
        }
        if ret.is_void() {
            if value.is_some() {
                return Err(Diagnostic::new(
                    span.unwrap_or_else(fallback_span),
                    "void function returns a value",
                ));
            }
            return Ok(());
        }
        let value = value.ok_or_else(|| {
            Diagnostic::new(
                span.unwrap_or_else(fallback_span),
                format!("function '{}' returns without a value", self.function.name),
            )
        })?;
        if ret.is_aggregate() {
            let sret = self.frame.sret().ok_or_else(|| {
                Diagnostic::new(
                    span.unwrap_or_else(fallback_span),
                    "aggregate return has no hidden result pointer",
                )
            })?;
            let bytes = self.operand_type(value).size(layout).unwrap_or(0);
            self.load_word(sret, "3x");
            self.load_operand(value, "1x", None)?;
            self.copy_bytes("3x", "1x", bytes)?;
            return Ok(());
        }
        self.load_operand(value, "1x", Some("2x"))?;
        if !is_64(ret) {
            self.mask_to("1x", ret);
        }
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Raw helpers used by `inst`
    // -----------------------------------------------------------------------

    pub(crate) fn mask_to(&mut self, register: &str, ty: Type) {
        let mask = match ty {
            Type::I1 => 1,
            Type::I8 => 0xff,
            Type::I16 => 0xffff,
            _ => return,
        };
        self.line(format!("andn {register} 0x{mask:x}"));
    }
}
