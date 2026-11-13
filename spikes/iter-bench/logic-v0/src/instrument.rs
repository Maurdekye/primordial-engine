//! Injected metering (docs/spec/logic.md 5.2), probe version.
//!
//! Validates a module against the allowed feature set, then rewrites it so that:
//! * a new mutable global `pe_fuel: i64` is charged at the start of every straight-line segment
//!   (function entry, after every `block`/`loop`/`if`/`else`, after every `end`), with the segment's
//!   static instruction count; a negative result traps through `unreachable`;
//! * a new mutable global `pe_depth: i32` grows by each function's static frame weight on entry
//!   (1 + params + locals + maximum operand stack height) and shrinks on every exit; past the limit it
//!   traps. The original body is wrapped in one block so that `return` and branches to the function's
//!   own label all leave through the epilogue;
//! * every mutable global the module defines is exported as `pe_g<i>`, so the host can reset it.
//!
//! The output is a pure function of the input bytes and `Config`.

use wasm_encoder::reencode::{Reencode, RoundtripReencoder};
use wasm_encoder::{
    BlockType, CodeSection, ConstExpr, ExportKind, ExportSection, Function, GlobalSection, GlobalType,
    Instruction as I, Module, RawSection, TypeSection, ValType,
};
use wasmparser::{
    ExternalKind, FuncValidatorAllocations, Operator, Parser, Payload, TypeRef, Validator,
    WasmFeatures,
};

#[derive(Clone, Copy, Debug)]
pub struct Config {
    /// Trap when the accumulated frame weight exceeds this.
    pub depth_limit: u32,
    pub fuel: FuelMode,
    /// Canonicalize NaNs where their bits become observable (float stores, reinterprets, copysign's sign
    /// operand, global.set), instead of after every float operation. Refuses SIMD.
    pub nan_observe: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum FuelMode {
    /// v0: fuel in the global, checked at every segment.
    Global,
    /// v1: fuel in a local, checked at function entry and at loop headers, flushed around calls and on exit.
    Local,
}

impl Default for Config {
    fn default() -> Self {
        Config { depth_limit: 1 << 16, fuel: FuelMode::Local, nan_observe: false }
    }
}

/// The feature set a script may use (spec 5.3): wasm 2.0 (MVP, mutable globals, sign extension,
/// saturating float-to-int, multi-value, bulk memory, reference types, SIMD). No relaxed SIMD,
/// threads, memory64, exceptions, tail calls or GC.
pub fn allowed_features() -> WasmFeatures {
    WasmFeatures::WASM2
}

fn features(cfg: &Config) -> WasmFeatures {
    if cfg.nan_observe { WasmFeatures::WASM2.difference(WasmFeatures::SIMD) } else { WasmFeatures::WASM2 }
}

#[derive(Debug, Default, Clone)]
pub struct Report {
    pub functions: u32,
    pub segments: u64,
    pub max_weight: u32,
    pub mutable_globals: Vec<u32>,
    pub fuel_global: u32,
    pub depth_global: u32,
}

struct FuncInfo {
    weight: u32,
    nparams: u32,
    results: Vec<wasmparser::ValType>,
}

fn err<E: std::fmt::Display>(e: E) -> String {
    e.to_string()
}

pub fn instrument(wasm: &[u8], cfg: Config) -> Result<(Vec<u8>, Report), String> {
    // ---- Pass 1: validate, collect types and per-function weights.
    let mut v = Validator::new_with_features(features(&cfg));
    let mut types: Vec<wasmparser::FuncType> = Vec::new();
    let mut func_types: Vec<u32> = Vec::new(); // defined functions only
    let mut infos: Vec<FuncInfo> = Vec::new();
    let mut n_globals = 0u32;
    let mut mutable_globals = Vec::new();
    let mut global_types: Vec<wasmparser::ValType> = Vec::new();
    let mut export_names: Vec<String> = Vec::new();
    let mut alloc = FuncValidatorAllocations::default();
    for p in Parser::new(0).parse_all(wasm) {
        let p = p.map_err(err)?;
        match &p {
            Payload::TypeSection(r) => {
                for t in r.clone().into_iter_err_on_gc_types() {
                    types.push(t.map_err(err)?);
                }
            }
            Payload::ImportSection(r) => {
                for imp in r.clone().into_imports() {
                    let imp = imp.map_err(err)?;
                    if imp.module != "pe" {
                        return Err(format!("import from module {:?}: only \"pe\" is allowed", imp.module));
                    }
                    if !matches!(imp.ty, TypeRef::Func(_)) {
                        return Err(format!("import {:?} is not a function", imp.name));
                    }
                }
            }
            Payload::FunctionSection(r) => {
                for t in r.clone() {
                    func_types.push(t.map_err(err)?);
                }
            }
            Payload::GlobalSection(r) => {
                for g in r.clone() {
                    let g = g.map_err(err)?;
                    if g.ty.mutable {
                        mutable_globals.push(n_globals);
                    }
                    global_types.push(g.ty.content_type);
                    n_globals += 1;
                }
            }
            Payload::ExportSection(r) => {
                for e in r.clone() {
                    let e = e.map_err(err)?;
                    if e.name.starts_with("pe_fuel") || e.name.starts_with("pe_depth") || e.name.starts_with("pe_g") {
                        return Err(format!("export {:?} uses a reserved name", e.name));
                    }
                    export_names.push(e.name.to_string());
                }
            }
            _ => {}
        }
        match v.payload(&p).map_err(err)? {
            wasmparser::ValidPayload::Func(f, body) => {
                let idx = infos.len();
                let ty = &types[func_types[idx] as usize];
                let mut fv = f.into_validator(std::mem::take(&mut alloc));
                let mut nlocals = ty.params().len() as u32;
                let off = body.range().start as u64;
                for l in body.get_locals_reader().map_err(err)? {
                    let (count, t) = l.map_err(err)?;
                    fv.define_locals(off, count, t).map_err(err)?;
                    nlocals += count;
                }
                let mut r = body.get_operators_reader().map_err(err)?;
                let mut max_h = 0u32;
                while !r.eof() {
                    let (op, off) = r.read_with_offset().map_err(err)?;
                    fv.op(off, &op).map_err(err)?;
                    max_h = max_h.max(fv.operand_stack_height());
                }
                r.finish().map_err(err)?;
                alloc = fv.into_allocations();
                infos.push(FuncInfo { weight: 1 + nlocals + max_h, nparams: ty.params().len() as u32, results: ty.results().to_vec() });
            }
            wasmparser::ValidPayload::End(_) => break,
            _ => {}
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
            1 => BlockType::Result(re.val_type(f.results[0]).map_err(err)?),
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
        fuel_global: fuel_g,
        depth_global: depth_g,
        max_weight: infos.iter().map(|f| f.weight).max().unwrap_or(0),
        ..Default::default()
    };

    // ---- Pass 2: rewrite.
    let mut out = Module::new();
    let mut code = CodeSection::new();
    let mut fidx = 0usize;
    let (mut globals_done, mut exports_done) = (false, false);
    for p in Parser::new(0).parse_all(wasm) {
        let p = p.map_err(err)?;
        // A module without a global or export section gets one, in its place in the section order.
        let id = match &p {
            Payload::End(_) => Some(0xff),
            Payload::CustomSection(_) => None,
            other => other.as_section().map(|(id, _)| id),
        };
        if let Some(id) = id {
            if !globals_done && matches!(id, 7 | 8 | 9 | 10 | 11 | 12 | 0xff) && !matches!(p, Payload::GlobalSection(_)) {
                let mut gs = GlobalSection::new();
                our_globals(&mut gs);
                out.section(&gs);
                globals_done = true;
            }
            if !exports_done && matches!(id, 8 | 9 | 10 | 11 | 12 | 0xff) && !matches!(p, Payload::ExportSection(_)) {
                let mut es = ExportSection::new();
                our_exports(&mut es, fuel_g, depth_g, &mutable_globals);
                out.section(&es);
                exports_done = true;
            }
        }
        match p {
            Payload::Version { .. } => {}
            Payload::TypeSection(r) => {
                let mut ts = TypeSection::new();
                re.parse_type_section(&mut ts, r).map_err(err)?;
                for t in &extra_types {
                    let rs: Vec<ValType> = t.iter().map(|x| re.val_type(*x).unwrap()).collect();
                    ts.ty().function([], rs);
                }
                out.section(&ts);
            }
            Payload::GlobalSection(r) => {
                let mut gs = GlobalSection::new();
                re.parse_global_section(&mut gs, r).map_err(err)?;
                our_globals(&mut gs);
                out.section(&gs);
                globals_done = true;
            }
            Payload::ExportSection(r) => {
                let mut es = ExportSection::new();
                for e in r {
                    let e = e.map_err(err)?;
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
                let f = rewrite_body(&body, &infos[fidx], block_types[fidx], fuel_g, depth_g, cfg, &global_types, &mut report)?;
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
    let bytes = out.finish();
    // The output must validate under the same features.
    Validator::new_with_features(features(&cfg)).validate_all(&bytes).map_err(|e| format!("output: {e}"))?;
    Ok((bytes, report))
}

fn our_globals(gs: &mut GlobalSection) {
    gs.global(GlobalType { val_type: ValType::I64, mutable: true, shared: false }, &ConstExpr::i64_const(0));
    gs.global(GlobalType { val_type: ValType::I32, mutable: true, shared: false }, &ConstExpr::i32_const(0));
}

fn our_exports(es: &mut ExportSection, fuel_g: u32, depth_g: u32, mutable_globals: &[u32]) {
    es.export("pe_fuel", ExportKind::Global, fuel_g);
    es.export("pe_depth", ExportKind::Global, depth_g);
    for g in mutable_globals {
        es.export(&format!("pe_g{g}"), ExportKind::Global, *g);
    }
}

/// Subtract a segment's cost from the fuel. `Global` (v0) keeps fuel in the global and checks at every
/// segment; `Local` (v1) keeps it in a local, checks only at function entry and loop headers, and flushes
/// it to the global around calls and on exit.
fn charge(f: &mut Function, cost: u64, fuel_g: u32, fl: u32, mode: FuelMode, check: bool) {
    match mode {
        FuelMode::Global => {
            if cost == 0 {
                return;
            }
            f.instruction(&I::GlobalGet(fuel_g));
            f.instruction(&I::I64Const(cost as i64));
            f.instruction(&I::I64Sub);
            f.instruction(&I::LocalTee(fl));
            f.instruction(&I::GlobalSet(fuel_g));
            f.instruction(&I::LocalGet(fl));
            f.instruction(&I::I64Const(0));
            f.instruction(&I::I64LtS);
            f.instruction(&I::If(BlockType::Empty));
            f.instruction(&I::Unreachable);
            f.instruction(&I::End);
        }
        FuelMode::Local => {
            if cost > 0 {
                f.instruction(&I::LocalGet(fl));
                f.instruction(&I::I64Const(cost as i64));
                f.instruction(&I::I64Sub);
                f.instruction(&I::LocalSet(fl));
            }
            if check {
                f.instruction(&I::LocalGet(fl));
                f.instruction(&I::I64Const(0));
                f.instruction(&I::I64LtS);
                f.instruction(&I::If(BlockType::Empty));
                f.instruction(&I::LocalGet(fl));
                f.instruction(&I::GlobalSet(fuel_g));
                f.instruction(&I::Unreachable);
                f.instruction(&I::End);
            }
        }
    }
}

/// The float type on top of the stack whose bits `op` makes observable, if any.
fn nan_point(op: &Operator, globals: &[wasmparser::ValType]) -> Option<ValType> {
    use wasmparser::ValType as V;
    match op {
        Operator::F32Store { .. } | Operator::I32ReinterpretF32 | Operator::F32Copysign => Some(ValType::F32),
        Operator::F64Store { .. } | Operator::I64ReinterpretF64 | Operator::F64Copysign => Some(ValType::F64),
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

fn opens(op: &Operator) -> bool {
    matches!(op, Operator::Block { .. } | Operator::Loop { .. } | Operator::If { .. })
}

fn is_call(op: &Operator) -> bool {
    matches!(op, Operator::Call { .. } | Operator::CallIndirect { .. } | Operator::CallRef { .. })
}

fn rewrite_body(
    body: &wasmparser::FunctionBody,
    info: &FuncInfo,
    bt: BlockType,
    fuel_g: u32,
    depth_g: u32,
    cfg: Config,
    globals: &[wasmparser::ValType],
    report: &mut Report,
) -> Result<Function, String> {
    let mut re = RoundtripReencoder;
    let mut locals = Vec::new();
    let mut declared = 0u32;
    for l in body.get_locals_reader().map_err(err)? {
        let (c, t) = l.map_err(err)?;
        declared += c;
        locals.push((c, re.val_type(t).map_err(err)?));
    }
    let ops: Vec<Operator> = {
        let mut r = body.get_operators_reader().map_err(err)?;
        let mut v = Vec::new();
        while !r.eof() {
            v.push(r.read().map_err(err)?);
        }
        v
    };
    // Our two temporaries follow the parameters and the declared locals.
    let first_free = info.nparams + declared;
    let fl = first_free; // i64: the fuel (Local) or a temporary (Global)
    let tmp32 = first_free + 1;
    locals.push((1, ValType::I64));
    locals.push((1, ValType::I32));
    let (t32, t64) = (first_free + 2, first_free + 3);
    locals.push((1, ValType::F32));
    locals.push((1, ValType::F64));
    let mut f = Function::new(locals);
    let local = cfg.fuel == FuelMode::Local;

    // Segment costs: a segment starts at index 0 and after every opener, `else` and `end`.
    let n = ops.len();
    let mut seg_cost = vec![0u64; n + 1]; // cost of the segment starting at op i
    let mut starts = vec![false; n + 1];
    let mut check_at = vec![false; n + 1];
    starts[0] = true;
    check_at[0] = true; // function entry is a check point
    for (i, op) in ops.iter().enumerate() {
        if opens(op) || matches!(op, Operator::Else | Operator::End) {
            starts[i + 1] = true;
        }
        if matches!(op, Operator::Loop { .. }) {
            check_at[i + 1] = true; // every loop iteration is a check point
        }
    }
    let mut cur = 0usize;
    for i in 0..n {
        if starts[i] {
            cur = i;
        }
        seg_cost[cur] += 1;
    }

    // Prologue: depth.
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
    if local {
        f.instruction(&I::GlobalGet(fuel_g));
        f.instruction(&I::LocalSet(fl));
    }
    // The wrapper block: every exit of the original body leaves through it.
    f.instruction(&I::Block(bt));

    let mut depth = 0u32; // open blocks inside the wrapper
    for (i, op) in ops.into_iter().enumerate() {
        if starts[i] && i < n - 1 {
            charge(&mut f, seg_cost[i], fuel_g, fl, cfg.fuel, check_at[i]);
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
                if local {
                    f.instruction(&I::LocalGet(fl));
                    f.instruction(&I::GlobalSet(fuel_g));
                }
                f.instruction(&I::GlobalGet(depth_g));
                f.instruction(&I::I32Const(w));
                f.instruction(&I::I32Sub);
                f.instruction(&I::GlobalSet(depth_g));
                f.instruction(&I::End);
            }
            Operator::ReturnCall { .. } | Operator::ReturnCallIndirect { .. } | Operator::ReturnCallRef { .. } => {
                return Err("tail calls are not allowed".into());
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
                let call = local && is_call(&op);
                if call {
                    f.instruction(&I::LocalGet(fl));
                    f.instruction(&I::GlobalSet(fuel_g));
                }
                f.instruction(&re.instruction(op).map_err(err)?);
                if call {
                    f.instruction(&I::GlobalGet(fuel_g));
                    f.instruction(&I::LocalSet(fl));
                }
            }
        }
    }
    Ok(f)
}
