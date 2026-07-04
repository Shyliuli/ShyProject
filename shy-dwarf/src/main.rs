use std::collections::{BTreeMap, HashMap};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use gimli::write::{
    Address, AttributeValue, CallFrameInstruction, CommonInformationEntry, Dwarf, EndianVec,
    Expression, FrameDescriptionEntry, FrameTable, LineProgram, LineString, Sections, Unit,
    UnitEntryId,
};
use gimli::{
    BigEndian, DW_LANG_C11, Encoding, Format, LineEncoding, Register, SectionId, constants,
};
use object::write::{Object, SectionKind, Symbol, SymbolSection};
use object::{
    Architecture, BinaryFormat, Endianness, SectionFlags, SymbolFlags, SymbolKind, SymbolScope,
};
use serde::Deserialize;

const SHY_FP_REG: Register = Register(15);
const GDB_SP_REG: Register = Register(29);
const GDB_PC_REG: Register = Register(37);
const SHY_INSN_SIZE: u32 = 12;

#[derive(Debug)]
struct Options {
    asm: Vec<PathBuf>,
    sym: PathBuf,
    debug: Vec<PathBuf>,
    output: PathBuf,
}

#[derive(Debug, Deserialize)]
struct DebugInfo {
    #[serde(default)]
    files: Vec<DebugFile>,
    #[serde(default)]
    types: Vec<DebugType>,
    #[serde(default)]
    functions: Vec<DebugFunction>,
    #[serde(default)]
    line_entries: Vec<DebugLine>,
}

#[derive(Debug, Deserialize)]
struct DebugFile {
    index: u32,
    path: String,
}

#[derive(Debug, Deserialize)]
struct DebugFunction {
    name: String,
    symbol: String,
    section: String,
    offset: u32,
    file: u32,
    line: u32,
    #[serde(default)]
    frame_base: Option<String>,
    #[serde(default)]
    params: Vec<DebugVar>,
    #[serde(default)]
    locals: Vec<DebugVar>,
}

#[derive(Clone, Debug, Deserialize)]
struct DebugVar {
    name: String,
    #[serde(rename = "type")]
    ty: u32,
    offset: i32,
    #[serde(default)]
    file: u32,
    #[serde(default)]
    line: u32,
}

#[derive(Clone, Debug, Deserialize)]
struct DebugType {
    id: u32,
    kind: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    size: u32,
    #[serde(default)]
    encoding: Option<String>,
    #[serde(default)]
    base: Option<u32>,
    #[serde(default)]
    array_len: Option<i64>,
    #[serde(default)]
    members: Vec<DebugMember>,
    #[serde(default)]
    enumerators: Vec<DebugEnumerator>,
}

#[derive(Clone, Debug, Deserialize)]
struct DebugMember {
    name: String,
    #[serde(rename = "type")]
    ty: u32,
    offset: u32,
    #[serde(default)]
    bit_offset: Option<u32>,
    #[serde(default)]
    bit_size: Option<u32>,
}

#[derive(Clone, Debug, Deserialize)]
struct DebugEnumerator {
    name: String,
    value: i64,
}

#[derive(Debug, Deserialize)]
struct DebugLine {
    file: u32,
    line: u32,
    section: String,
    offset: u32,
}

#[derive(Clone, Debug)]
struct SymbolInfo {
    name: String,
    addr: u32,
}

#[derive(Clone, Debug)]
struct FunctionRange {
    name: String,
    symbol: String,
    low_pc: u32,
    high_pc: u32,
    file: u32,
    line: u32,
}

fn main() -> Result<()> {
    let opts = parse_args(env::args().skip(1).collect())?;
    run(&opts)
}

fn parse_args(args: Vec<String>) -> Result<Options> {
    let mut asm = Vec::new();
    let mut sym = None;
    let mut debug = Vec::new();
    let mut output = None;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--asm" => {
                i += 1;
                asm.push(PathBuf::from(
                    args.get(i).context("--asm requires an argument")?,
                ));
            }
            "--sym" => {
                i += 1;
                sym = Some(PathBuf::from(
                    args.get(i).context("--sym requires an argument")?,
                ));
            }
            "--debug" => {
                i += 1;
                debug.push(PathBuf::from(
                    args.get(i).context("--debug requires an argument")?,
                ));
            }
            "-o" => {
                i += 1;
                output = Some(PathBuf::from(
                    args.get(i).context("-o requires an argument")?,
                ));
            }
            "--help" | "-h" => {
                print_usage();
                std::process::exit(0);
            }
            other if other.starts_with('-') => bail!("unknown option: {other}"),
            other => bail!("unexpected positional argument: {other}"),
        }
        i += 1;
    }

    Ok(Options {
        asm: if asm.is_empty() {
            bail!("missing --asm <app.shy>");
        } else {
            asm
        },
        sym: sym.context("missing --sym <app.sym>")?,
        debug: if debug.is_empty() {
            bail!("missing --debug <app.shy.debug.json>");
        } else {
            debug
        },
        output: output.context("missing -o <app.debug.elf>")?,
    })
}

fn run(opts: &Options) -> Result<()> {
    if opts.asm.len() != opts.debug.len() {
        bail!(
            "expected the same number of --asm and --debug inputs, got {} and {}",
            opts.asm.len(),
            opts.debug.len()
        );
    }
    let debug = read_debug_inputs(&opts.debug)?;
    let symbols = read_sym(&opts.sym)?;
    let sym_by_name: HashMap<_, _> = symbols.iter().map(|s| (s.name.as_str(), s.addr)).collect();
    let section_sizes = compute_text_section_sizes(&opts.asm)?;
    let functions = function_ranges(&debug, &symbols, &sym_by_name, &section_sizes)?;
    let dwarf_sections = build_dwarf(&debug, &sym_by_name, &functions)?;
    let elf = build_elf(&symbols, &section_sizes, &sym_by_name, dwarf_sections)?;
    fs::write(&opts.output, elf)
        .with_context(|| format!("failed to write {}", opts.output.display()))?;
    Ok(())
}

fn read_debug_inputs(paths: &[PathBuf]) -> Result<DebugInfo> {
    let mut merged = DebugInfo {
        files: Vec::new(),
        types: Vec::new(),
        functions: Vec::new(),
        line_entries: Vec::new(),
    };
    let mut global_file_by_path: HashMap<String, u32> = HashMap::new();

    for path in paths {
        let input: DebugInfo = serde_json::from_slice(
            &fs::read(path).with_context(|| format!("failed to read {}", path.display()))?,
        )
        .with_context(|| format!("failed to parse {}", path.display()))?;

        let mut file_map = HashMap::new();
        for file in input.files {
            let global = if let Some(index) = global_file_by_path.get(&file.path).copied() {
                index
            } else {
                let index = (merged.files.len() + 1) as u32;
                global_file_by_path.insert(file.path.clone(), index);
                merged.files.push(DebugFile {
                    index,
                    path: file.path.clone(),
                });
                index
            };
            file_map.insert(file.index, global);
        }

        let mut type_map = HashMap::new();
        let type_start = merged.types.len();
        for mut ty in input.types {
            let global = (merged.types.len() + 1) as u32;
            type_map.insert(ty.id, global);
            ty.id = global;
            merged.types.push(ty);
        }

        for ty in &mut merged.types[type_start..] {
            if let Some(base) = ty.base {
                if let Some(global) = type_map.get(&base).copied() {
                    ty.base = Some(global);
                }
            }
            for member in &mut ty.members {
                if let Some(global) = type_map.get(&member.ty).copied() {
                    member.ty = global;
                }
            }
        }

        for mut function in input.functions {
            if let Some(global) = file_map.get(&function.file).copied() {
                function.file = global;
            }
            for var in function.params.iter_mut().chain(function.locals.iter_mut()) {
                if let Some(global) = file_map.get(&var.file).copied() {
                    var.file = global;
                }
                if let Some(global) = type_map.get(&var.ty).copied() {
                    var.ty = global;
                }
            }
            merged.functions.push(function);
        }
        for mut line in input.line_entries {
            if let Some(global) = file_map.get(&line.file).copied() {
                line.file = global;
            }
            merged.line_entries.push(line);
        }
    }

    Ok(merged)
}

fn read_sym(path: &Path) -> Result<Vec<SymbolInfo>> {
    let text =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    let mut out = Vec::new();
    for (idx, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let mut parts = line.split_whitespace();
        let name = parts
            .next()
            .with_context(|| format!("invalid sym line {}: {line}", idx + 1))?;
        let addr = parts
            .next()
            .with_context(|| format!("invalid sym line {}: {line}", idx + 1))?;
        if parts.next().is_some() {
            bail!("invalid sym line {}: {line}", idx + 1);
        }
        let addr = parse_u32_hex(addr)
            .with_context(|| format!("invalid address on sym line {}: {line}", idx + 1))?;
        out.push(SymbolInfo {
            name: name.to_string(),
            addr,
        });
    }
    out.sort_by_key(|s| (s.addr, s.name.clone()));
    Ok(out)
}

fn compute_text_section_sizes(paths: &[PathBuf]) -> Result<HashMap<String, u32>> {
    let mut current: Option<String> = None;
    let mut sizes = HashMap::new();

    for path in paths {
        let text = fs::read_to_string(path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        for raw in text.lines() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with("//") || line.starts_with("#![") {
                continue;
            }
            if let Some(section) = line.strip_prefix(".section ") {
                let section = section.trim().to_string();
                current = Some(section.clone());
                sizes.entry(section).or_insert(0);
                continue;
            }
            if line.starts_with('.') || line.ends_with(':') {
                continue;
            }
            let Some(section) = current.as_ref() else {
                continue;
            };
            if section.starts_with("text.") {
                *sizes.entry(section.clone()).or_insert(0) += 12;
            }
        }
    }

    Ok(sizes)
}

fn function_ranges(
    debug: &DebugInfo,
    symbols: &[SymbolInfo],
    sym_by_name: &HashMap<&str, u32>,
    section_sizes: &HashMap<String, u32>,
) -> Result<Vec<FunctionRange>> {
    let mut ranges = Vec::new();
    for function in &debug.functions {
        let section_base = sym_by_name
            .get(function.section.as_str())
            .copied()
            .with_context(|| format!("missing section symbol {}", function.section))?;
        let low_pc = sym_by_name
            .get(function.symbol.as_str())
            .copied()
            .unwrap_or(section_base + function.offset);
        let high_pc = next_text_symbol_addr(symbols, low_pc)
            .or_else(|| {
                section_sizes
                    .get(&function.section)
                    .map(|size| section_base + *size)
            })
            .unwrap_or(low_pc + 12);
        ranges.push(FunctionRange {
            name: function.name.clone(),
            symbol: function.symbol.clone(),
            low_pc,
            high_pc: high_pc.max(low_pc + 12),
            file: function.file,
            line: function.line,
        });
    }
    ranges.sort_by_key(|r| (r.low_pc, r.name.clone()));
    Ok(ranges)
}

fn next_text_symbol_addr(symbols: &[SymbolInfo], addr: u32) -> Option<u32> {
    symbols
        .iter()
        .filter(|s| s.name.starts_with("text.") && s.addr > addr)
        .map(|s| s.addr)
        .min()
}

fn build_dwarf(
    debug: &DebugInfo,
    sym_by_name: &HashMap<&str, u32>,
    functions: &[FunctionRange],
) -> Result<BTreeMap<String, Vec<u8>>> {
    let encoding = Encoding {
        format: Format::Dwarf32,
        version: 4,
        address_size: 4,
    };
    let mut dwarf = Dwarf::default();
    let mut unit = Unit::new(
        encoding,
        LineProgram::new(
            encoding,
            LineEncoding::default(),
            LineString::String(b".".to_vec()),
            None,
            LineString::String(
                debug
                    .files
                    .first()
                    .map(|f| f.path.as_bytes().to_vec())
                    .unwrap_or_else(|| b"input.c".to_vec()),
            ),
            None,
        ),
    );

    let files_by_index: HashMap<u32, &DebugFile> =
        debug.files.iter().map(|f| (f.index, f)).collect();
    let types_by_id: HashMap<u32, &DebugType> = debug.types.iter().map(|t| (t.id, t)).collect();
    let mut line_files = HashMap::new();
    for file in &debug.files {
        let path = Path::new(&file.path);
        let dir = path
            .parent()
            .and_then(|p| p.to_str())
            .unwrap_or("")
            .as_bytes()
            .to_vec();
        let name = path
            .file_name()
            .and_then(|p| p.to_str())
            .unwrap_or(&file.path)
            .as_bytes()
            .to_vec();
        let dir_id = if dir.is_empty() {
            unit.line_program.default_directory()
        } else {
            unit.line_program.add_directory(LineString::String(dir))
        };
        let file_id = unit
            .line_program
            .add_file(LineString::String(name), dir_id, None);
        line_files.insert(file.index, file_id);
    }

    let root = unit.root();
    {
        let root = unit.get_mut(root);
        root.set(
            constants::DW_AT_producer,
            AttributeValue::String(b"shy-dwarf".to_vec()),
        );
        root.set(
            constants::DW_AT_language,
            AttributeValue::Language(DW_LANG_C11),
        );
        if let Some(file) = debug.files.first() {
            root.set(
                constants::DW_AT_name,
                AttributeValue::String(file.path.as_bytes().to_vec()),
            );
        }
        if let (Some(low), Some(high)) = (
            functions.iter().map(|f| f.low_pc).min(),
            functions.iter().map(|f| f.high_pc).max(),
        ) {
            root.set(
                constants::DW_AT_low_pc,
                AttributeValue::Address(Address::Constant(low as u64)),
            );
            root.set(
                constants::DW_AT_high_pc,
                AttributeValue::Udata((high - low) as u64),
            );
        }
    }

    let mut type_cache = HashMap::new();
    for ty in &debug.types {
        let _ = ensure_type_die(&mut unit, root, &types_by_id, &mut type_cache, ty.id);
    }

    for function in functions {
        let die = unit.add(root, constants::DW_TAG_subprogram);
        let node = unit.get_mut(die);
        node.set(
            constants::DW_AT_name,
            AttributeValue::String(function.name.as_bytes().to_vec()),
        );
        node.set(
            constants::DW_AT_linkage_name,
            AttributeValue::String(function.symbol.as_bytes().to_vec()),
        );
        node.set(
            constants::DW_AT_low_pc,
            AttributeValue::Address(Address::Constant(function.low_pc as u64)),
        );
        node.set(
            constants::DW_AT_high_pc,
            AttributeValue::Udata((function.high_pc - function.low_pc) as u64),
        );
        node.set(
            constants::DW_AT_decl_line,
            AttributeValue::Udata(function.line as u64),
        );
        if let Some(file) = line_files.get(&function.file) {
            node.set(
                constants::DW_AT_decl_file,
                AttributeValue::FileIndex(Some(*file)),
            );
        }

        let Some(debug_function) = debug
            .functions
            .iter()
            .find(|candidate| candidate.symbol == function.symbol)
        else {
            continue;
        };
        if matches!(
            debug_function.frame_base.as_deref(),
            Some("r15") | Some("fx")
        ) {
            let mut expr = Expression::new();
            expr.op(constants::DW_OP_reg15);
            unit.get_mut(die)
                .set(constants::DW_AT_frame_base, AttributeValue::Exprloc(expr));
        }
        add_debug_vars(
            &mut unit,
            die,
            &debug_function.params,
            true,
            &types_by_id,
            &line_files,
            &mut type_cache,
        );
        add_debug_vars(
            &mut unit,
            die,
            &debug_function.locals,
            false,
            &types_by_id,
            &line_files,
            &mut type_cache,
        );
    }

    let mut rows = Vec::new();
    for entry in &debug.line_entries {
        let Some(section_base) = sym_by_name.get(entry.section.as_str()).copied() else {
            continue;
        };
        let Some(file_id) = line_files.get(&entry.file).copied() else {
            continue;
        };
        let address = section_base + entry.offset;
        let file = files_by_index
            .get(&entry.file)
            .map(|f| f.path.as_str())
            .unwrap_or("-");
        if file == "-" {
            continue;
        }
        rows.push((address, file_id, entry.line));
    }
    rows.sort_by_key(|(address, _, line)| (*address, *line));
    rows.dedup_by_key(|(address, file, line)| (*address, *file, *line));
    if let Some((base, _, _)) = rows.first().copied() {
        unit.line_program
            .begin_sequence(Some(Address::Constant(base as u64)));
        for (address, file, line) in rows {
            let row = unit.line_program.row();
            row.address_offset = (address - base) as u64;
            row.file = file;
            row.line = line as u64;
            row.column = 0;
            row.is_statement = true;
            unit.line_program.generate_row();
        }
        let end = functions
            .iter()
            .map(|f| f.high_pc)
            .max()
            .unwrap_or(base + 12)
            .max(base + 12);
        unit.line_program.end_sequence((end - base) as u64);
    }

    dwarf.units.add(unit);

    let mut sections = Sections::new(EndianVec::new(BigEndian));
    dwarf
        .write(&mut sections)
        .map_err(|err| anyhow::anyhow!("failed to write DWARF: {err}"))?;
    build_frame_table(encoding, functions)
        .write_debug_frame(&mut sections.debug_frame)
        .map_err(|err| anyhow::anyhow!("failed to write DWARF CFI: {err}"))?;

    let mut out = BTreeMap::new();
    sections
        .for_each(|id, data| {
            out.insert(section_name(id).to_string(), data.slice().to_vec());
            Ok(())
        })
        .map_err(|err: gimli::Error| anyhow::anyhow!("failed to collect DWARF: {err}"))?;
    Ok(out)
}

fn build_frame_table(encoding: Encoding, functions: &[FunctionRange]) -> FrameTable {
    let mut frames = FrameTable::default();
    let mut cie = CommonInformationEntry::new(encoding, 1, 4, GDB_PC_REG);

    // GDB assumes downward-growing stacks. ShyISA's stack grows upward, so
    // report a synthetic CFA that decreases for deeper frames. Register
    // locations are recovered with expressions based on the real sp/fx values,
    // not from the synthetic CFA address.
    cie.add_instruction(CallFrameInstruction::CfaExpression(negated_cfa_expr(
        GDB_SP_REG,
    )));
    cie.add_instruction(CallFrameInstruction::Expression(
        GDB_PC_REG,
        actual_address_expr(GDB_SP_REG, -4),
    ));
    cie.add_instruction(CallFrameInstruction::SameValue(SHY_FP_REG));

    let cie_id = frames.add_cie(cie);
    for function in functions {
        let length = function.high_pc.saturating_sub(function.low_pc);
        if length == 0 {
            continue;
        }

        let mut fde = FrameDescriptionEntry::new(Address::Constant(function.low_pc as u64), length);

        // Standard ShyC prologue:
        //   0:  calla pushed return address at [old_sp]; sp = old_sp + 4
        //   0:  pusha fx saves old fx at [sp]; sp += 4
        //   12: seta fx sp makes fx the frame base (bottom of frame)
        //
        // At offset 12, return address is at sp - 8 and saved fx at sp - 4.
        if length >= SHY_INSN_SIZE {
            fde.add_instruction(
                SHY_INSN_SIZE,
                CallFrameInstruction::Expression(GDB_PC_REG, actual_address_expr(GDB_SP_REG, -8)),
            );
            fde.add_instruction(
                SHY_INSN_SIZE,
                CallFrameInstruction::Expression(SHY_FP_REG, actual_address_expr(GDB_SP_REG, -4)),
            );
        }
        // At offset 24, the stable frame pointer is fx. Use CFA = -fx so that
        // deeper frames still have decreasing CFAs for GDB.
        if length >= SHY_INSN_SIZE * 2 {
            fde.add_instruction(
                SHY_INSN_SIZE * 2,
                CallFrameInstruction::CfaExpression(negated_cfa_expr(SHY_FP_REG)),
            );
            fde.add_instruction(
                SHY_INSN_SIZE * 2,
                CallFrameInstruction::Expression(GDB_PC_REG, actual_address_expr(SHY_FP_REG, -8)),
            );
            fde.add_instruction(
                SHY_INSN_SIZE * 2,
                CallFrameInstruction::Expression(SHY_FP_REG, actual_address_expr(SHY_FP_REG, -4)),
            );
        }

        // Epilogue: the last instructions restore sp/fx and return. GDB rarely
        // unwinds from an epilogue, but describe it with the pre-return state.
        if length > SHY_INSN_SIZE * 3 {
            fde.add_instruction(
                length - SHY_INSN_SIZE,
                CallFrameInstruction::CfaExpression(negated_cfa_expr(GDB_SP_REG)),
            );
            fde.add_instruction(
                length - SHY_INSN_SIZE,
                CallFrameInstruction::Expression(GDB_PC_REG, actual_address_expr(GDB_SP_REG, -4)),
            );
            fde.add_instruction(
                length - SHY_INSN_SIZE,
                CallFrameInstruction::SameValue(SHY_FP_REG),
            );
        }

        frames.add_fde(cie_id, fde);
    }

    frames
}

fn negated_cfa_expr(register: Register) -> Expression {
    let mut expr = Expression::new();
    expr.op_breg(register, 0);
    expr.op(constants::DW_OP_neg);
    expr
}

fn actual_address_expr(register: Register, offset: i64) -> Expression {
    let mut expr = Expression::new();
    expr.op_breg(register, 0);
    if offset != 0 {
        expr.op_consts(offset);
        expr.op(constants::DW_OP_plus);
    }
    expr
}

fn add_debug_vars(
    unit: &mut Unit,
    parent: UnitEntryId,
    vars: &[DebugVar],
    params: bool,
    types_by_id: &HashMap<u32, &DebugType>,
    line_files: &HashMap<u32, gimli::write::FileId>,
    type_cache: &mut HashMap<u32, UnitEntryId>,
) {
    let root = unit.root();
    for var in vars {
        if var.name.is_empty() {
            continue;
        }
        let tag = if params {
            constants::DW_TAG_formal_parameter
        } else {
            constants::DW_TAG_variable
        };
        let die = unit.add(parent, tag);
        if let Some(type_die) = ensure_type_die(unit, root, types_by_id, type_cache, var.ty) {
            unit.get_mut(die)
                .set(constants::DW_AT_type, AttributeValue::UnitRef(type_die));
        }
        let mut expr = Expression::new();
        expr.op_breg(Register(15), var.offset as i64);
        let node = unit.get_mut(die);
        node.set(
            constants::DW_AT_name,
            AttributeValue::String(var.name.as_bytes().to_vec()),
        );
        node.set(constants::DW_AT_location, AttributeValue::Exprloc(expr));
        if let Some(file) = line_files.get(&var.file) {
            node.set(
                constants::DW_AT_decl_file,
                AttributeValue::FileIndex(Some(*file)),
            );
        }
        if var.line != 0 {
            node.set(
                constants::DW_AT_decl_line,
                AttributeValue::Udata(var.line as u64),
            );
        }
    }
}

fn ensure_type_die(
    unit: &mut Unit,
    root: UnitEntryId,
    types_by_id: &HashMap<u32, &DebugType>,
    cache: &mut HashMap<u32, UnitEntryId>,
    id: u32,
) -> Option<UnitEntryId> {
    if let Some(entry) = cache.get(&id).copied() {
        return Some(entry);
    }
    let ty = *types_by_id.get(&id)?;
    let tag = match ty.kind.as_str() {
        "pointer" => constants::DW_TAG_pointer_type,
        "array" => constants::DW_TAG_array_type,
        "struct" => constants::DW_TAG_structure_type,
        "union" => constants::DW_TAG_union_type,
        "enum" => constants::DW_TAG_enumeration_type,
        "typedef" => constants::DW_TAG_typedef,
        "function" => constants::DW_TAG_subroutine_type,
        "base" if ty.encoding.as_deref() == Some("void") => constants::DW_TAG_unspecified_type,
        _ => constants::DW_TAG_base_type,
    };
    let die = unit.add(root, tag);
    cache.insert(id, die);

    if let Some(name) = dwarf_type_name(ty) {
        if !name.is_empty() && name != "pointer" && name != "array" && name != "function" {
            unit.get_mut(die).set(
                constants::DW_AT_name,
                AttributeValue::String(name.into_bytes()),
            );
        }
    }

    match ty.kind.as_str() {
        "base" => {
            if ty.encoding.as_deref() != Some("void") {
                let node = unit.get_mut(die);
                node.set(
                    constants::DW_AT_byte_size,
                    AttributeValue::Udata(ty.size as u64),
                );
                node.set(
                    constants::DW_AT_encoding,
                    AttributeValue::Encoding(base_encoding(ty)),
                );
            }
        }
        "pointer" => {
            let node = unit.get_mut(die);
            node.set(
                constants::DW_AT_byte_size,
                AttributeValue::Udata(ty.size as u64),
            );
            if let Some(base) = ty
                .base
                .and_then(|base| ensure_type_die(unit, root, types_by_id, cache, base))
            {
                unit.get_mut(die)
                    .set(constants::DW_AT_type, AttributeValue::UnitRef(base));
            }
        }
        "array" => {
            if let Some(base) = ty
                .base
                .and_then(|base| ensure_type_die(unit, root, types_by_id, cache, base))
            {
                unit.get_mut(die)
                    .set(constants::DW_AT_type, AttributeValue::UnitRef(base));
            }
            if let Some(len) = ty.array_len.filter(|len| *len >= 0) {
                let subrange = unit.add(die, constants::DW_TAG_subrange_type);
                unit.get_mut(subrange)
                    .set(constants::DW_AT_count, AttributeValue::Udata(len as u64));
            }
        }
        "struct" | "union" => {
            unit.get_mut(die).set(
                constants::DW_AT_byte_size,
                AttributeValue::Udata(ty.size as u64),
            );
            for member in &ty.members {
                let member_die = unit.add(die, constants::DW_TAG_member);
                if let Some(member_type) =
                    ensure_type_die(unit, root, types_by_id, cache, member.ty)
                {
                    unit.get_mut(member_die)
                        .set(constants::DW_AT_type, AttributeValue::UnitRef(member_type));
                }
                let node = unit.get_mut(member_die);
                node.set(
                    constants::DW_AT_name,
                    AttributeValue::String(member.name.as_bytes().to_vec()),
                );
                node.set(
                    constants::DW_AT_data_member_location,
                    AttributeValue::Udata(member.offset as u64),
                );
                if let Some(size) = member.bit_size {
                    node.set(
                        constants::DW_AT_bit_size,
                        AttributeValue::Udata(size as u64),
                    );
                }
                if let Some(offset) = member.bit_offset {
                    node.set(
                        constants::DW_AT_bit_offset,
                        AttributeValue::Udata(offset as u64),
                    );
                }
            }
        }
        "enum" => {
            unit.get_mut(die).set(
                constants::DW_AT_byte_size,
                AttributeValue::Udata(ty.size as u64),
            );
            for enumerator in &ty.enumerators {
                let enumerator_die = unit.add(die, constants::DW_TAG_enumerator);
                let node = unit.get_mut(enumerator_die);
                node.set(
                    constants::DW_AT_name,
                    AttributeValue::String(enumerator.name.as_bytes().to_vec()),
                );
                node.set(
                    constants::DW_AT_const_value,
                    AttributeValue::Sdata(enumerator.value),
                );
            }
        }
        "typedef" => {
            if let Some(base) = ty
                .base
                .and_then(|base| ensure_type_die(unit, root, types_by_id, cache, base))
            {
                unit.get_mut(die)
                    .set(constants::DW_AT_type, AttributeValue::UnitRef(base));
            }
        }
        "function" => {
            if let Some(base) = ty
                .base
                .and_then(|base| ensure_type_die(unit, root, types_by_id, cache, base))
            {
                unit.get_mut(die)
                    .set(constants::DW_AT_type, AttributeValue::UnitRef(base));
            }
        }
        _ => {}
    }

    Some(die)
}

fn base_encoding(ty: &DebugType) -> constants::DwAte {
    match ty.encoding.as_deref() {
        Some("bool") => constants::DW_ATE_boolean,
        Some("float") => constants::DW_ATE_float,
        Some("unsigned") => constants::DW_ATE_unsigned,
        _ => constants::DW_ATE_signed,
    }
}

fn dwarf_type_name(ty: &DebugType) -> Option<String> {
    let name = ty.name.as_ref()?;
    if ty.kind == "struct" {
        return Some(name.strip_prefix("struct ").unwrap_or(name).to_string());
    }
    if ty.kind == "union" {
        return Some(name.strip_prefix("union ").unwrap_or(name).to_string());
    }
    Some(name.clone())
}

fn section_name(id: SectionId) -> &'static str {
    match id {
        SectionId::DebugAbbrev => ".debug_abbrev",
        SectionId::DebugInfo => ".debug_info",
        SectionId::DebugLine => ".debug_line",
        SectionId::DebugLineStr => ".debug_line_str",
        SectionId::DebugStr => ".debug_str",
        SectionId::DebugStrOffsets => ".debug_str_offsets",
        SectionId::DebugTypes => ".debug_types",
        SectionId::DebugLoc => ".debug_loc",
        SectionId::DebugLocLists => ".debug_loclists",
        SectionId::DebugAddr => ".debug_addr",
        SectionId::DebugRanges => ".debug_ranges",
        SectionId::DebugRngLists => ".debug_rnglists",
        SectionId::DebugMacinfo => ".debug_macinfo",
        SectionId::DebugMacro => ".debug_macro",
        _ => id.name(),
    }
}

fn build_elf(
    symbols: &[SymbolInfo],
    section_sizes: &HashMap<String, u32>,
    sym_by_name: &HashMap<&str, u32>,
    dwarf_sections: BTreeMap<String, Vec<u8>>,
) -> Result<Vec<u8>> {
    let mut object = Object::new(BinaryFormat::Elf, Architecture::Mips, Endianness::Big);
    object.flags = object::FileFlags::Elf {
        os_abi: 0,
        abi_version: 0,
        e_flags: 0,
    };

    let (text_addr, text_size) = text_range(section_sizes, sym_by_name);
    let text_id = object.add_section(Vec::new(), b".text".to_vec(), SectionKind::Text);
    object.section_mut(text_id).flags = SectionFlags::Elf {
        sh_flags: (object::elf::SHF_ALLOC | object::elf::SHF_EXECINSTR) as u64,
    };
    if text_size > 0 {
        object.append_section_data(text_id, &vec![0; text_size as usize], 4);
    }

    for (name, data) in dwarf_sections {
        if data.is_empty() {
            continue;
        }
        let id = object.add_section(Vec::new(), name.as_bytes().to_vec(), SectionKind::Debug);
        object.append_section_data(id, &data, 1);
    }

    for sym in symbols {
        object.add_symbol(Symbol {
            name: sym.name.as_bytes().to_vec(),
            value: sym.addr as u64,
            size: 0,
            kind: SymbolKind::Text,
            scope: SymbolScope::Linkage,
            weak: false,
            section: SymbolSection::Absolute,
            flags: SymbolFlags::None,
        });
    }

    let mut bytes = object
        .write()
        .map_err(|err| anyhow::anyhow!("failed to write ELF: {err}"))?;
    patch_elf_header(
        &mut bytes,
        text_addr,
        sym_by_name.get("_start").copied().unwrap_or(text_addr),
    )?;
    Ok(bytes)
}

fn text_range(
    section_sizes: &HashMap<String, u32>,
    sym_by_name: &HashMap<&str, u32>,
) -> (u32, u32) {
    let mut low = u32::MAX;
    let mut high = 0;
    for (section, size) in section_sizes {
        if !section.starts_with("text.") {
            continue;
        }
        if let Some(base) = sym_by_name.get(section.as_str()).copied() {
            low = low.min(base);
            high = high.max(base + *size);
        }
    }
    if low == u32::MAX {
        (0x100, 0)
    } else {
        (low, high - low)
    }
}

fn patch_elf_header(bytes: &mut [u8], text_addr: u32, entry: u32) -> Result<()> {
    if bytes.len() < 52 || &bytes[0..4] != b"\x7fELF" {
        bail!("internal error: object writer did not produce ELF32");
    }
    if bytes[4] != 1 || bytes[5] != 2 {
        bail!("internal error: expected ELF32 big-endian output");
    }

    bytes[16..18].copy_from_slice(&object::elf::ET_EXEC.to_be_bytes());
    bytes[24..28].copy_from_slice(&entry.to_be_bytes());

    let shoff = read_be_u32(bytes, 32)? as usize;
    let shentsize = read_be_u16(bytes, 46)? as usize;
    let shnum = read_be_u16(bytes, 48)? as usize;
    let shstrndx = read_be_u16(bytes, 50)? as usize;
    if shentsize < 40 || shoff + shentsize * shnum > bytes.len() || shstrndx >= shnum {
        bail!("internal error: invalid ELF section table");
    }

    let shstr = shoff + shentsize * shstrndx;
    let shstr_off = read_be_u32(bytes, shstr + 16)? as usize;
    let shstr_size = read_be_u32(bytes, shstr + 20)? as usize;
    if shstr_off + shstr_size > bytes.len() {
        bail!("internal error: invalid ELF shstrtab");
    }

    for i in 0..shnum {
        let off = shoff + shentsize * i;
        let name_off = read_be_u32(bytes, off)? as usize;
        let Some(name) = read_cstr(&bytes[shstr_off..shstr_off + shstr_size], name_off) else {
            continue;
        };
        if name == ".text" {
            bytes[off + 12..off + 16].copy_from_slice(&text_addr.to_be_bytes());
            return Ok(());
        }
    }
    Ok(())
}

fn read_be_u16(bytes: &[u8], off: usize) -> Result<u16> {
    let raw = bytes
        .get(off..off + 2)
        .with_context(|| format!("ELF read out of range at {off}"))?;
    Ok(u16::from_be_bytes([raw[0], raw[1]]))
}

fn read_be_u32(bytes: &[u8], off: usize) -> Result<u32> {
    let raw = bytes
        .get(off..off + 4)
        .with_context(|| format!("ELF read out of range at {off}"))?;
    Ok(u32::from_be_bytes([raw[0], raw[1], raw[2], raw[3]]))
}

fn read_cstr(bytes: &[u8], off: usize) -> Option<&str> {
    let rest = bytes.get(off..)?;
    let end = rest.iter().position(|b| *b == 0)?;
    std::str::from_utf8(&rest[..end]).ok()
}

fn parse_u32_hex(s: &str) -> Option<u32> {
    let s = s.strip_prefix("0x").unwrap_or(s);
    u32::from_str_radix(s, 16).ok()
}

fn print_usage() {
    eprintln!(
        "usage: shy-dwarf --asm app.shy --sym app.sym --debug app.shy.debug.json -o app.debug.elf"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn smoke_generates_elf_with_debug_sections() {
        let dir = env::temp_dir().join(format!("shy-dwarf-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let asm = dir.join("app.shy");
        let sym = dir.join("app.sym");
        let debug = dir.join("app.shy.debug.json");
        let out = dir.join("app.debug.elf");

        fs::write(
            &asm,
            "___CODE___\n.section text._start\n.symbol _start\nsetn sp 0x1000\ncalln main\n.section text.main\n.symbol main\nsetn 1x 0\nseta exit 1x\n",
        )
        .unwrap();
        fs::write(
            &sym,
            "text._start 0x00000100\n_start 0x00000100\ntext.main 0x00000118\nmain 0x00000118\n",
        )
        .unwrap();
        fs::write(
            &debug,
            r#"{
  "version": 1,
  "address_size": 4,
  "endian": "big",
  "files": [{"index": 1, "path": "main.c"}],
  "types": [
    {"id": 1, "kind": "base", "name": "int", "size": 4, "encoding": "signed"},
    {"id": 2, "kind": "struct", "name": "struct Point", "size": 8, "members": [
      {"name": "x", "type": 1, "offset": 0},
      {"name": "y", "type": 1, "offset": 4}
    ]}
  ],
  "functions": [{
    "name": "main",
    "symbol": "main",
    "section": "text.main",
    "offset": 0,
    "file": 1,
    "line": 1,
    "frame_base": "r15",
    "params": [{"name": "argc", "type": 1, "offset": 0, "file": 1, "line": 1}],
    "locals": [{"name": "p", "type": 2, "offset": 4, "file": 1, "line": 2}]
  }],
  "line_entries": [{"file": 1, "line": 1, "section": "text.main", "offset": 0}]
}"#,
        )
        .unwrap();

        run(&Options {
            asm: vec![asm],
            sym,
            debug: vec![debug],
            output: out.clone(),
        })
        .unwrap();

        let bytes = fs::read(out).unwrap();
        assert!(bytes.starts_with(&[0x7f, b'E', b'L', b'F']));
        let text = String::from_utf8_lossy(&bytes);
        assert!(text.contains(".debug_info"));
        assert!(text.contains(".debug_line"));
        assert!(text.contains(".debug_frame"));
        assert!(text.contains("argc"));
        assert!(text.contains("Point"));
        assert!(text.contains("p"));

        let _ = fs::remove_dir_all(&dir);
    }
}
