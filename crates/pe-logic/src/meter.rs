//! The validator and the injected metering pass (docs/spec/logic.md 3.1, 5.2, 5.2b, 5.3).
//!
//! [`instrument`] first validates a module: the feature set of 5.3, the ABI v0 imports and exports of [`crate::abi`],
//! one memory, no start function, no reserved export names. It then rewrites every defined function so that:
//! - **fuel:** a function keeps the fuel in a local, loaded from the global `pe_fuel` on entry. Every straight-line
//!   segment (the entry, and after every `block`, `loop`, `if`, `else` and `end`) subtracts its static instruction
//!   count. The local is checked at the entry and at every loop header; when it is negative the function stores it
//!   to the global and traps through `unreachable`. It is flushed to the global around every call and on exit.
//!   `memory.fill` and `memory.copy` also cost one unit per [`BULK_BYTES_PER_FUEL`] bytes of their length, and
//!   `memory.grow` that per byte of the pages it asks for, charged and checked just before the operator. A fuel trap
//!   leaves [`FUEL_TRAPPED`] in the global, so the host can tell it from any other `unreachable`;
//! - **depth:** the global `pe_depth` grows by the function's static frame weight (1 + parameters + locals + the
//!   maximum operand stack height) on entry and shrinks on exit; past the limit it traps. The body is wrapped in one
//!   block, so `return` and branches to the function's own label leave through the epilogue;
//! - **globals:** every mutable global the module defines is exported as `pe_g<index>`, so the host can reset it;
//! - **NaN at observation points** (optional, [`Config::nan_observe`], 5.2b): a float is canonicalized just before a
//!   store, a reinterpret, `copysign`'s sign operand and a `global.set`, instead of after every float operation by
//!   Cranelift. It refuses SIMD.
//!
//! The output is a pure function of the input bytes, the [`Config`] and [`PASS_VERSION`]. The pass is part of a
//! build's identity (5.2b, rule 3): a change to its output is a new pass version.
//!
//! The measured cost and the trap results are in logic.md 5.2a (probe `spikes/iter-bench/logic-v0`, whose
//! `instrument.rs` this is, without its slower global-fuel mode).

use crate::abi;
use wasm_encoder::reencode::{Reencode, RoundtripReencoder};
use wasm_encoder::{
    BlockType, CodeSection, ConstExpr, ExportKind, ExportSection, Function, GlobalSection, GlobalType,
    Instruction as I, Module, RawSection, TypeSection, ValType,
};
use wasmparser::{ExternalKind, FuncValidatorAllocations, Operator, Parser, Payload, TypeRef, Validator, WasmFeatures};

/// The version of this pass's output. Pinned under the SDK hash in `CommitScript` (5.2b, rule 3).
pub const PASS_VERSION: u32 = 1;

/// Bytes of a `memory.fill` or `memory.copy` length that cost one unit of fuel, as a power of two: 64 bytes a unit
/// (a unit is about one operator; a 64-byte memset step costs a few). `memory.grow` costs the same per byte asked.
pub const BULK_BYTES_PER_FUEL_LOG2: u32 = 6;
pub const BULK_BYTES_PER_FUEL: u32 = 1 << BULK_BYTES_PER_FUEL_LOG2;

/// What a fuel trap leaves in the fuel global: no flushed fuel count can reach it (budgets stay below 2^62).
pub const FUEL_TRAPPED: i64 = i64::MIN;

/// The names the pass adds; a module that exports one of them itself is refused.
pub const FUEL_GLOBAL: &str = "pe_fuel";
pub const DEPTH_GLOBAL: &str = "pe_depth";
pub const GLOBAL_PREFIX: &str = "pe_g";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Config {
    /// Trap when the accumulated frame weight exceeds this.
    pub depth_limit: u32,
    /// Canonicalize NaNs at observation points (5.2b) instead of relying on Cranelift's canonicalization.
    /// Off by default: the default stays Cranelift's until the aarch64 test of 5.2b passes.
    pub nan_observe: bool,
}

impl Default for Config {
    fn default() -> Self {
        Config { depth_limit: 1 << 16, nan_observe: false }
    }
}

/// Why a module was refused. Each kind maps to one of the API's diagnostic kinds (`code_check`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RefusalKind {
    /// Not a valid module, or a construct the pass cannot rewrite.
    Malformed,
    /// A wasm feature outside 5.3.
    Feature,
    /// An import outside ABI v0's list, or with the wrong signature.
    Import,
    /// A missing or mistyped export, a memory other than exactly one, or a start function.
    Abi,
    /// An export with a name the pass reserves.
    ReservedName,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refusal {
    pub kind: RefusalKind,
    pub message: String,
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}: {}", self.kind, self.message)
    }
}

impl std::error::Error for Refusal {}

fn refuse<T>(kind: RefusalKind, message: impl Into<String>) -> Result<T, Refusal> {
    Err(Refusal { kind, message: message.into() })
}

/// A wasmparser error: a feature error when the same bytes validate with every feature on.
fn parse_err(wasm: &[u8]) -> impl Fn(wasmparser::BinaryReaderError) -> Refusal + '_ {
    move |e| {
        let kind = if Validator::new_with_features(WasmFeatures::all()).validate_all(wasm).is_ok() {
            RefusalKind::Feature
        } else {
            RefusalKind::Malformed
        };
        Refusal { kind, message: e.to_string() }
    }
}

fn enc_err<E: std::fmt::Display>(e: E) -> Refusal {
    Refusal { kind: RefusalKind::Malformed, message: e.to_string() }
}

/// The feature set a script may use (5.3): wasm 2.0 (the MVP, mutable globals, sign extension, non-trapping
/// float-to-int, multi-value, bulk memory, reference types, fixed-width SIMD). No relaxed SIMD, threads, memory64,
/// exceptions, tail calls, GC or the component model. Without SIMD when NaNs are canonicalized at observation points.
pub fn features(cfg: &Config) -> WasmFeatures {
    if cfg.nan_observe { WasmFeatures::WASM2.difference(WasmFeatures::SIMD) } else { WasmFeatures::WASM2 }
}

/// What the pass found and added.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Report {
    pub functions: u32,
    pub segments: u64,
    pub max_weight: u32,
    /// Indices of the module's own mutable globals, exported as `pe_g<index>`.
    pub mutable_globals: Vec<u32>,
}

struct FuncInfo {
    weight: u32,
    nparams: u32,
    results: Vec<wasmparser::ValType>,
}

fn reserved(name: &str) -> bool {
    name == FUEL_GLOBAL
        || name == DEPTH_GLOBAL
        || name.strip_prefix(GLOBAL_PREFIX).is_some_and(|d| !d.is_empty() && d.bytes().all(|b| b.is_ascii_digit()))
}

/// Validate `wasm` and rewrite it with metering. See the module documentation.
pub fn instrument(wasm: &[u8], cfg: Config) -> Result<(Vec<u8>, Report), Refusal> {
    let perr = parse_err(wasm);
    // ---- Pass 1: validate, collect types, imports, exports and per-function weights.
    let mut v = Validator::new_with_features(features(&cfg));
    let mut types: Vec<wasmparser::FuncType> = Vec::new();
    let mut imported_funcs: Vec<u32> = Vec::new(); // type index of each imported function
    let mut func_types: Vec<u32> = Vec::new(); // defined functions only
    let mut infos: Vec<FuncInfo> = Vec::new();
    let mut n_globals = 0u32;
    let mut mutable_globals = Vec::new();
    let mut global_types: Vec<wasmparser::ValType> = Vec::new();
    let mut memories = 0u32;
    let mut exports: Vec<(String, ExternalKind, u32)> = Vec::new();
    let mut alloc = FuncValidatorAllocations::default();
    for p in Parser::new(0).parse_all(wasm) {
        let p = p.map_err(&perr)?;
        // The validator first, so that nothing below reads an invalid section.
        let valid = v.payload(&p).map_err(&perr)?;
        match &p {
            Payload::TypeSection(r) => {
                for t in r.clone().into_iter_err_on_gc_types() {
                    types.push(t.map_err(&perr)?);
                }
            }
            Payload::ImportSection(r) => {
                for imp in r.clone().into_imports() {
                    let imp = imp.map_err(&perr)?;
                    if imp.module != abi::IMPORT_MODULE {
                        return refuse(
                            RefusalKind::Import,
                            format!(
                                "import {:?} from module {:?}: only {:?} is allowed",
                                imp.name,
                                imp.module,
                                abi::IMPORT_MODULE
                            ),
                        );
                    }
                    let TypeRef::Func(ti) = imp.ty else {
                        return refuse(RefusalKind::Import, format!("import {:?} is not a function", imp.name));
                    };
                    let Some(sig) = abi::import(imp.name) else {
                        return refuse(
                            RefusalKind::Import,
                            format!("import {:?} is not a host function of ABI v0", imp.name),
                        );
                    };
                    if !sig.matches(&types[ti as usize]) {
                        return refuse(
                            RefusalKind::Import,
                            format!("import {:?} has the wrong signature: ABI v0 has {}", imp.name, sig.describe()),
                        );
                    }
                    imported_funcs.push(ti);
                }
            }
            Payload::FunctionSection(r) => {
                for t in r.clone() {
                    func_types.push(t.map_err(&perr)?);
                }
            }
            Payload::MemorySection(r) => memories += r.count(),
            Payload::GlobalSection(r) => {
                for g in r.clone() {
                    let g = g.map_err(&perr)?;
                    if g.ty.mutable {
                        mutable_globals.push(n_globals);
                    }
                    global_types.push(g.ty.content_type);
                    n_globals += 1;
                }
            }
            Payload::ExportSection(r) => {
                for e in r.clone() {
                    let e = e.map_err(&perr)?;
                    if reserved(e.name) {
                        return refuse(
                            RefusalKind::ReservedName,
                            format!("export {:?} uses a name the metering pass reserves", e.name),
                        );
                    }
                    exports.push((e.name.to_string(), e.kind, e.index));
                }
            }
            Payload::StartSection { .. } => {
                return refuse(
                    RefusalKind::Abi,
                    "a start function would run at instantiation, outside any system call",
                );
            }
            _ => {}
        }
        match valid {
            wasmparser::ValidPayload::Func(f, body) => {
                let idx = infos.len();
                let ty = &types[func_types[idx] as usize];
                let mut fv = f.into_validator(std::mem::take(&mut alloc));
                let mut nlocals = ty.params().len() as u32;
                let off = body.range().start;
                for l in body.get_locals_reader().map_err(&perr)? {
                    let (count, t) = l.map_err(&perr)?;
                    fv.define_locals(off, count, t).map_err(&perr)?;
                    nlocals += count;
                }
                let mut r = body.get_operators_reader().map_err(&perr)?;
                let mut max_h = 0u32;
                while !r.eof() {
                    let (op, off) = r.read_with_offset().map_err(&perr)?;
                    fv.op(off, &op).map_err(&perr)?;
                    if let Some(what) = outside_reset(&op) {
                        return refuse(
                            RefusalKind::Feature,
                            format!("{what} changes state the per-frame reset does not restore (logic.md 5.3)"),
                        );
                    }
                    max_h = max_h.max(fv.operand_stack_height());
                }
                r.finish().map_err(&perr)?;
                alloc = fv.into_allocations();
                infos.push(FuncInfo {
                    weight: 1 + nlocals + max_h,
                    nparams: ty.params().len() as u32,
                    results: ty.results().to_vec(),
                });
            }
            wasmparser::ValidPayload::End(_) => break,
            _ => {}
        }
    }

    // ---- The ABI's exports and memory.
    if memories != 1 {
        return refuse(RefusalKind::Abi, format!("a script defines exactly one memory, not {memories}"));
    }
    let func_type = |idx: u32| -> &wasmparser::FuncType {
        let n = imported_funcs.len() as u32;
        let ti = if idx < n { imported_funcs[idx as usize] } else { func_types[(idx - n) as usize] };
        &types[ti as usize]
    };
    match exports.iter().find(|e| e.0 == abi::MEMORY_EXPORT) {
        Some((_, ExternalKind::Memory, 0)) => {}
        _ => {
            return refuse(
                RefusalKind::Abi,
                format!("the module's memory must be exported as {:?}", abi::MEMORY_EXPORT),
            );
        }
    }
    for sig in abi::EXPORTS {
        match exports.iter().find(|e| e.0 == sig.name) {
            Some((_, ExternalKind::Func, idx)) if sig.matches(func_type(*idx)) => {}
            Some(_) => return refuse(RefusalKind::Abi, format!("export {:?} must be {}", sig.name, sig.describe())),
            None => return refuse(RefusalKind::Abi, format!("missing export {}", sig.describe())),
        }
    }

    // Block types for the wrapper block: [] -> results. Multi-result functions need a new type.
    let n_types = types.len() as u32;
    let mut extra_types: Vec<Vec<wasmparser::ValType>> = Vec::new();
    let mut block_types: Vec<BlockType> = Vec::new();
    let mut re = RoundtripReencoder;
    for f in &infos {
        let bt = match f.results.len() {
            0 => BlockType::Empty,
            1 => BlockType::Result(re.val_type(f.results[0]).map_err(enc_err)?),
            _ => {
                let i = match extra_types.iter().position(|t| *t == f.results) {
                    Some(i) => i,
                    None => {
                        extra_types.push(f.results.clone());
                        extra_types.len() - 1
                    }
                };
                BlockType::FunctionType(n_types + i as u32)
            }
        };
        block_types.push(bt);
    }

    let fuel_g = n_globals;
    let depth_g = n_globals + 1;
    let mut report = Report {
        functions: infos.len() as u32,
        mutable_globals: mutable_globals.clone(),
        max_weight: infos.iter().map(|f| f.weight).max().unwrap_or(0),
        ..Default::default()
    };

    // ---- Pass 2: rewrite.
    let mut out = Module::new();
    let mut code = CodeSection::new();
    let mut fidx = 0usize;
    let (mut globals_done, mut exports_done) = (false, false);
    for p in Parser::new(0).parse_all(wasm) {
        let p = p.map_err(&perr)?;
        // A module without a global section gets one, in its place in the section order (an export section it has).
        let id = match &p {
            Payload::End(_) => Some(0xff),
            Payload::CustomSection(_) => None,
            other => other.as_section().map(|(id, _)| id),
        };
        if let Some(id) = id
            && !globals_done
            && matches!(id, 7 | 8 | 9 | 10 | 11 | 12 | 0xff)
            && !matches!(p, Payload::GlobalSection(_))
        {
            let mut gs = GlobalSection::new();
            our_globals(&mut gs);
            out.section(&gs);
            globals_done = true;
        }
        match p {
            Payload::Version { .. } => {}
            Payload::TypeSection(r) => {
                let mut ts = TypeSection::new();
                re.parse_type_section(&mut ts, r).map_err(enc_err)?;
                for t in &extra_types {
                    let rs: Vec<ValType> =
                        t.iter().map(|x| re.val_type(*x)).collect::<Result<_, _>>().map_err(enc_err)?;
                    ts.ty().function([], rs);
                }
                out.section(&ts);
            }
            Payload::GlobalSection(r) => {
                let mut gs = GlobalSection::new();
                re.parse_global_section(&mut gs, r).map_err(enc_err)?;
                our_globals(&mut gs);
                out.section(&gs);
                globals_done = true;
            }
            Payload::ExportSection(r) => {
                let mut es = ExportSection::new();
                for e in r {
                    let e = e.map_err(&perr)?;
                    let kind = match e.kind {
                        ExternalKind::Func | ExternalKind::FuncExact => ExportKind::Func,
                        ExternalKind::Table => ExportKind::Table,
                        ExternalKind::Memory => ExportKind::Memory,
                        ExternalKind::Global => ExportKind::Global,
                        ExternalKind::Tag => ExportKind::Tag,
                    };
                    es.export(e.name, kind, e.index);
                }
                our_exports(&mut es, fuel_g, depth_g, &mutable_globals);
                out.section(&es);
                exports_done = true;
            }
            Payload::CodeSectionStart { .. } => {}
            Payload::CodeSectionEntry(body) => {
                let f = rewrite_body(
                    &body,
                    &infos[fidx],
                    block_types[fidx],
                    fuel_g,
                    depth_g,
                    cfg,
                    &global_types,
                    &mut report,
                    &perr,
                )?;
                code.function(&f);
                fidx += 1;
                if fidx == infos.len() {
                    out.section(&code);
                }
            }
            Payload::CustomSection(c) => {
                // Names and producers are kept as they are; they do not affect execution.
                out.section(&wasm_encoder::CustomSection { name: c.name().into(), data: c.data().into() });
            }
            Payload::End(_) => break,
            other => {
                if let Some((id, range)) = other.as_section() {
                    out.section(&RawSection { id, data: &wasm[range.start as usize..range.end as usize] });
                }
            }
        }
    }
    // The ABI's exports exist, so the module had an export section and it carried ours.
    debug_assert!(exports_done);
    let bytes = out.finish();
    // The output must validate under the same features.
    if let Err(e) = Validator::new_with_features(features(&cfg)).validate_all(&bytes) {
        return refuse(RefusalKind::Malformed, format!("the metering pass produced an invalid module: {e}"));
    }
    Ok((bytes, report))
}

fn our_globals(gs: &mut GlobalSection) {
    gs.global(GlobalType { val_type: ValType::I64, mutable: true, shared: false }, &ConstExpr::i64_const(0));
    gs.global(GlobalType { val_type: ValType::I32, mutable: true, shared: false }, &ConstExpr::i32_const(0));
}

fn our_exports(es: &mut ExportSection, fuel_g: u32, depth_g: u32, mutable_globals: &[u32]) {
    es.export(FUEL_GLOBAL, ExportKind::Global, fuel_g);
    es.export(DEPTH_GLOBAL, ExportKind::Global, depth_g);
    for g in mutable_globals {
        es.export(&format!("{GLOBAL_PREFIX}{g}"), ExportKind::Global, *g);
    }
}

/// Subtract a segment's cost from the fuel local; at a check point, trap if it is negative (storing it first).
fn charge(f: &mut Function, cost: u64, fuel_g: u32, fl: u32, check: bool) {
    if cost > 0 {
        f.instruction(&I::LocalGet(fl));
        f.instruction(&I::I64Const(cost as i64));
        f.instruction(&I::I64Sub);
        f.instruction(&I::LocalSet(fl));
    }
    if check {
        check_fuel(f, fuel_g, fl);
    }
}

/// Trap if the fuel local is negative, leaving [`FUEL_TRAPPED`] in the global.
fn check_fuel(f: &mut Function, fuel_g: u32, fl: u32) {
    f.instruction(&I::LocalGet(fl));
    f.instruction(&I::I64Const(0));
    f.instruction(&I::I64LtS);
    f.instruction(&I::If(BlockType::Empty));
    f.instruction(&I::I64Const(FUEL_TRAPPED));
    f.instruction(&I::GlobalSet(fuel_g));
    f.instruction(&I::Unreachable);
    f.instruction(&I::End);
}

/// Charge a bulk operator by the i32 on top of the stack (its length, or for `memory.grow` its pages), which stays
/// there, then check (5.2: "extra weight by their length argument").
fn charge_bulk(f: &mut Function, fuel_g: u32, fl: u32, tmp32: u32, pages: bool) {
    f.instruction(&I::LocalTee(tmp32));
    f.instruction(&I::LocalGet(fl));
    f.instruction(&I::LocalGet(tmp32));
    f.instruction(&I::I64ExtendI32U);
    if pages {
        f.instruction(&I::I64Const(16 - BULK_BYTES_PER_FUEL_LOG2 as i64));
        f.instruction(&I::I64Shl);
    } else {
        f.instruction(&I::I64Const(BULK_BYTES_PER_FUEL_LOG2 as i64));
        f.instruction(&I::I64ShrU);
    }
    f.instruction(&I::I64Sub);
    f.instruction(&I::LocalSet(fl));
    check_fuel(f, fuel_g, fl);
}

/// The float type on top of the stack whose bits `op` makes observable, if any (5.2b).
fn nan_point(op: &Operator, globals: &[wasmparser::ValType]) -> Option<ValType> {
    use wasmparser::ValType as V;
    match op {
        Operator::F32Store { .. } | Operator::I32ReinterpretF32 | Operator::F32Copysign => Some(ValType::F32),
        Operator::F64Store { .. } | Operator::I64ReinterpretF64 | Operator::F64Copysign => Some(ValType::F64),
        // By global index: safe because imports are functions only, so no imported global shifts the index space.
        Operator::GlobalSet { global_index } => match globals.get(*global_index as usize) {
            Some(V::F32) => Some(ValType::F32),
            Some(V::F64) => Some(ValType::F64),
            _ => None,
        },
        _ => None,
    }
}

/// x on the stack -> (x == x ? x : canonical NaN).
fn canon32(f: &mut Function, t: u32) {
    f.instruction(&I::LocalTee(t));
    f.instruction(&I::F32Const(f32::from_bits(0x7fc0_0000).into()));
    f.instruction(&I::LocalGet(t));
    f.instruction(&I::LocalGet(t));
    f.instruction(&I::F32Eq);
    f.instruction(&I::Select);
}

fn canon64(f: &mut Function, t: u32) {
    f.instruction(&I::LocalTee(t));
    f.instruction(&I::F64Const(f64::from_bits(0x7ff8_0000_0000_0000).into()));
    f.instruction(&I::LocalGet(t));
    f.instruction(&I::LocalGet(t));
    f.instruction(&I::F64Eq);
    f.instruction(&I::Select);
}

/// Operators that change instance state outside linear memory and the globals: the tables and the dropped state of
/// passive segments. The per-frame reset restores memory and globals only (5.1), so a module using them is refused.
fn outside_reset(op: &Operator) -> Option<&'static str> {
    Some(match op {
        Operator::TableSet { .. } => "table.set",
        Operator::TableGrow { .. } => "table.grow",
        Operator::TableFill { .. } => "table.fill",
        Operator::TableCopy { .. } => "table.copy",
        Operator::TableInit { .. } => "table.init",
        Operator::ElemDrop { .. } => "elem.drop",
        Operator::MemoryInit { .. } => "memory.init",
        Operator::DataDrop { .. } => "data.drop",
        _ => return None,
    })
}

fn opens(op: &Operator) -> bool {
    matches!(op, Operator::Block { .. } | Operator::Loop { .. } | Operator::If { .. })
}

fn is_call(op: &Operator) -> bool {
    matches!(op, Operator::Call { .. } | Operator::CallIndirect { .. } | Operator::CallRef { .. })
}

#[allow(clippy::too_many_arguments)]
fn rewrite_body(
    body: &wasmparser::FunctionBody,
    info: &FuncInfo,
    bt: BlockType,
    fuel_g: u32,
    depth_g: u32,
    cfg: Config,
    globals: &[wasmparser::ValType],
    report: &mut Report,
    perr: &impl Fn(wasmparser::BinaryReaderError) -> Refusal,
) -> Result<Function, Refusal> {
    let mut re = RoundtripReencoder;
    let mut locals = Vec::new();
    let mut declared = 0u32;
    for l in body.get_locals_reader().map_err(perr)? {
        let (c, t) = l.map_err(perr)?;
        declared += c;
        locals.push((c, re.val_type(t).map_err(enc_err)?));
    }
    let ops: Vec<Operator> = {
        let mut r = body.get_operators_reader().map_err(perr)?;
        let mut v = Vec::new();
        while !r.eof() {
            v.push(r.read().map_err(perr)?);
        }
        v
    };
    // Our temporaries follow the parameters and the declared locals.
    let first_free = info.nparams + declared;
    let fl = first_free; // i64: the fuel
    let tmp32 = first_free + 1;
    let (t32, t64) = (first_free + 2, first_free + 3);
    locals.push((1, ValType::I64));
    locals.push((1, ValType::I32));
    locals.push((1, ValType::F32));
    locals.push((1, ValType::F64));
    let mut f = Function::new(locals);

    // Segment costs: a segment starts at index 0 and after every opener, `else` and `end`.
    let n = ops.len();
    let mut seg_cost = vec![0u64; n + 1]; // cost of the segment starting at op i
    let mut starts = vec![false; n + 1];
    let mut check_at = vec![false; n + 1];
    starts[0] = true;
    check_at[0] = true; // the function entry is a check point
    for (i, op) in ops.iter().enumerate() {
        if opens(op) || matches!(op, Operator::Else | Operator::End) {
            starts[i + 1] = true;
        }
        if matches!(op, Operator::Loop { .. }) {
            check_at[i + 1] = true; // every loop iteration is a check point
        }
    }
    let mut cur = 0usize;
    for (i, &start) in starts.iter().take(n).enumerate() {
        if start {
            cur = i;
        }
        seg_cost[cur] += 1;
    }

    // Prologue: depth, then the fuel into its local.
    let w = info.weight as i32;
    f.instruction(&I::GlobalGet(depth_g));
    f.instruction(&I::I32Const(w));
    f.instruction(&I::I32Add);
    f.instruction(&I::LocalTee(tmp32));
    f.instruction(&I::GlobalSet(depth_g));
    f.instruction(&I::LocalGet(tmp32));
    f.instruction(&I::I32Const(cfg.depth_limit as i32));
    f.instruction(&I::I32GtU);
    f.instruction(&I::If(BlockType::Empty));
    f.instruction(&I::Unreachable);
    f.instruction(&I::End);
    f.instruction(&I::GlobalGet(fuel_g));
    f.instruction(&I::LocalSet(fl));
    // The wrapper block: every exit of the original body leaves through it.
    f.instruction(&I::Block(bt));

    let mut depth = 0u32; // open blocks inside the wrapper
    for (i, op) in ops.into_iter().enumerate() {
        if starts[i] && i < n - 1 {
            charge(&mut f, seg_cost[i], fuel_g, fl, check_at[i]);
            report.segments += 1;
        }
        let last = i == n - 1;
        match op {
            Operator::Return => {
                f.instruction(&I::Br(depth));
            }
            Operator::End if last => {
                // Closes the wrapper block instead of the function, then the epilogue.
                f.instruction(&I::End);
                f.instruction(&I::LocalGet(fl));
                f.instruction(&I::GlobalSet(fuel_g));
                f.instruction(&I::GlobalGet(depth_g));
                f.instruction(&I::I32Const(w));
                f.instruction(&I::I32Sub);
                f.instruction(&I::GlobalSet(depth_g));
                f.instruction(&I::End);
            }
            Operator::ReturnCall { .. } | Operator::ReturnCallIndirect { .. } | Operator::ReturnCallRef { .. } => {
                // The validator's feature set already refuses tail calls; this keeps the pass sound on its own.
                return refuse(RefusalKind::Feature, "tail calls are not allowed");
            }
            op => {
                if opens(&op) {
                    depth += 1;
                } else if matches!(op, Operator::End) {
                    depth -= 1;
                }
                if cfg.nan_observe {
                    match nan_point(&op, globals) {
                        Some(ValType::F32) => canon32(&mut f, t32),
                        Some(ValType::F64) => canon64(&mut f, t64),
                        _ => {}
                    }
                }
                match op {
                    Operator::MemoryFill { .. } | Operator::MemoryCopy { .. } => {
                        charge_bulk(&mut f, fuel_g, fl, tmp32, false)
                    }
                    Operator::MemoryGrow { .. } => charge_bulk(&mut f, fuel_g, fl, tmp32, true),
                    _ => {}
                }
                let call = is_call(&op);
                if call {
                    f.instruction(&I::LocalGet(fl));
                    f.instruction(&I::GlobalSet(fuel_g));
                }
                f.instruction(&re.instruction(op).map_err(enc_err)?);
                if call {
                    f.instruction(&I::GlobalGet(fuel_g));
                    f.instruction(&I::LocalSet(fl));
                }
            }
        }
    }
    Ok(f)
}

/// Checks bytes the pass already produced (a fault bundle's module, @diverge-eng's replay) without running the pass
/// again: [`crate::runtime::Script::load_metered`] calls it after the bytes' hash matched the row's `author_module`,
/// which already proves they are the author's metered output. This is defence in depth, and it reads only what the
/// runtime relies on: the module validates under the same features; its imports are ABI v0's; it has one memory,
/// exported, and the ABI's exports with their signatures; the pass's fuel (`i64`) and depth (`i32`) globals are
/// exported as mutable; and every other mutable global the module defines is exported as `pe_g<index>`, so the
/// per-frame reset (3.6) restores all of them. The report's `functions` counts defined functions; `segments` and
/// `max_weight` are the pass's own figures and are 0 here.
pub fn check_metered(wasm: &[u8], cfg: Config) -> Result<Report, Refusal> {
    let perr = parse_err(wasm);
    let mut v = Validator::new_with_features(features(&cfg));
    let mut types: Vec<wasmparser::FuncType> = Vec::new();
    let mut func_types: Vec<u32> = Vec::new(); // whole function index space: imports, then defined
    // Imports are functions only (checked below), so the global index space is the module's own globals.
    let mut globals: Vec<(wasmparser::ValType, bool)> = Vec::new();
    let mut memories = 0u32;
    let mut defined = 0u32;
    let mut exports: Vec<(String, ExternalKind, u32)> = Vec::new();
    for p in Parser::new(0).parse_all(wasm) {
        let p = p.map_err(&perr)?;
        v.payload(&p).map_err(&perr)?;
        match &p {
            Payload::TypeSection(r) => {
                for t in r.clone().into_iter_err_on_gc_types() {
                    types.push(t.map_err(&perr)?);
                }
            }
            Payload::ImportSection(r) => {
                for imp in r.clone().into_imports() {
                    let imp = imp.map_err(&perr)?;
                    let TypeRef::Func(ti) = imp.ty else {
                        return refuse(RefusalKind::Import, format!("import {:?} is not a function", imp.name));
                    };
                    let ok = imp.module == abi::IMPORT_MODULE
                        && abi::import(imp.name).is_some_and(|s| s.matches(&types[ti as usize]));
                    if !ok {
                        return refuse(RefusalKind::Import, format!("import {:?} is not ABI v0's", imp.name));
                    }
                    func_types.push(ti);
                }
            }
            Payload::FunctionSection(r) => {
                for t in r.clone() {
                    func_types.push(t.map_err(&perr)?);
                    defined += 1;
                }
            }
            Payload::MemorySection(r) => memories += r.count(),
            Payload::GlobalSection(r) => {
                for g in r.clone() {
                    let g = g.map_err(&perr)?;
                    globals.push((g.ty.content_type, g.ty.mutable));
                }
            }
            Payload::ExportSection(r) => {
                for e in r.clone() {
                    let e = e.map_err(&perr)?;
                    exports.push((e.name.to_string(), e.kind, e.index));
                }
            }
            Payload::StartSection { .. } => return refuse(RefusalKind::Abi, "a script has no start function"),
            _ => {}
        }
    }
    if memories != 1 {
        return refuse(RefusalKind::Abi, format!("a script defines exactly one memory, not {memories}"));
    }
    let export = |name: &str, kind: ExternalKind| exports.iter().find(|(n, k, _)| n == name && *k == kind).map(|e| e.2);
    if export(abi::MEMORY_EXPORT, ExternalKind::Memory).is_none() {
        return refuse(RefusalKind::Abi, "the memory is not exported as `memory`");
    }
    for sig in abi::EXPORTS {
        let ok = export(sig.name, ExternalKind::Func)
            .and_then(|f| func_types.get(f as usize))
            .is_some_and(|t| sig.matches(&types[*t as usize]));
        if !ok {
            return refuse(RefusalKind::Abi, format!("missing or mistyped export {}", sig.describe()));
        }
    }
    let ours = [(FUEL_GLOBAL, wasmparser::ValType::I64), (DEPTH_GLOBAL, wasmparser::ValType::I32)];
    let mut pass_globals = Vec::new();
    for (name, ty) in ours {
        let g = export(name, ExternalKind::Global);
        if g.and_then(|g| globals.get(g as usize)) != Some(&(ty, true)) {
            return refuse(RefusalKind::Abi, format!("not metered: no mutable {ty:?} global exported as {name}"));
        }
        pass_globals.extend(g);
    }
    let mut mutable_globals = Vec::new();
    for (i, (_, mutable)) in globals.iter().enumerate() {
        let i = i as u32;
        if !mutable || pass_globals.contains(&i) {
            continue;
        }
        if export(&format!("{GLOBAL_PREFIX}{i}"), ExternalKind::Global) != Some(i) {
            return refuse(RefusalKind::Abi, format!("mutable global {i} is not exported as {GLOBAL_PREFIX}{i}"));
        }
        mutable_globals.push(i);
    }
    Ok(Report { functions: defined, segments: 0, max_weight: 0, mutable_globals })
}
