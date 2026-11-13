//! logic.md 10.3: metering, faults, the per-frame reset and validation, on modules built with wasm-encoder (no
//! toolchain needed). Each module has the ABI v0 shape: `memory`, `pe_abi_version`, `pe_manifest`, `pe_run`.

use pe_logic::meter::{self, Config, RefusalKind};
use pe_logic::runtime::{self, Fault, NoServices, Script, Settings};
use wasm_encoder::*;

/// Extra pieces a test adds to the standard module.
#[derive(Default)]
struct Spec {
    /// Imports from `pe`, by name, with their ABI signatures (or a wrong one with `bad_sig`).
    imports: Vec<(&'static str, &'static str, Vec<ValType>, Vec<ValType>)>,
    /// `pe_run`'s body: params 0 = system, 1 = batch; must leave one i32. Function indices: imports first, then
    /// 0 pe_abi_version, 1 pe_manifest, 2 pe_run, 3 rec(n) -> i32, 4 spin(), 5 deep() (calls itself; weight 1).
    run: Vec<Instruction<'static>>,
    run_locals: Vec<(u32, ValType)>,
    shared_memory: bool,
    start: bool,
    extra_export: Option<&'static str>,
    skip_run_export: bool,
    table: bool,
}

fn build(s: Spec) -> Vec<u8> {
    let ni = s.imports.len() as u32;
    let mut m = Module::new();
    let mut t = TypeSection::new();
    t.ty().function([], [ValType::I32]); // 0: pe_abi_version
    t.ty().function([], [ValType::I64]); // 1: pe_manifest
    t.ty().function([ValType::I32, ValType::I32], [ValType::I32]); // 2: pe_run
    t.ty().function([ValType::I32], [ValType::I32]); // 3: rec
    t.ty().function([], []); // 4: spin
    for (_, _, p, r) in &s.imports {
        t.ty().function(p.clone(), r.clone());
    }
    m.section(&t);
    if ni > 0 {
        let mut im = ImportSection::new();
        for (i, (module, name, _, _)) in s.imports.iter().enumerate() {
            im.import(module, name, EntityType::Function(5 + i as u32));
        }
        m.section(&im);
    }
    let mut f = FunctionSection::new();
    for ty in [0, 1, 2, 3, 4, 4] {
        f.function(ty);
    }
    m.section(&f);
    if s.table {
        let mut tb = TableSection::new();
        tb.table(TableType {
            element_type: RefType::FUNCREF,
            minimum: 1,
            maximum: Some(1),
            table64: false,
            shared: false,
        });
        m.section(&tb);
    }
    let mut mem = MemorySection::new();
    mem.memory(MemoryType {
        minimum: 1,
        maximum: Some(16),
        memory64: false,
        shared: s.shared_memory,
        page_size_log2: None,
    });
    m.section(&mem);
    let mut g = GlobalSection::new();
    g.global(GlobalType { val_type: ValType::I32, mutable: true, shared: false }, &ConstExpr::i32_const(0));
    m.section(&g);
    let mut e = ExportSection::new();
    e.export("memory", ExportKind::Memory, 0);
    e.export("pe_abi_version", ExportKind::Func, ni);
    e.export("pe_manifest", ExportKind::Func, ni + 1);
    if !s.skip_run_export {
        e.export("pe_run", ExportKind::Func, ni + 2);
    }
    if let Some(name) = s.extra_export {
        e.export(name, ExportKind::Global, 0);
    }
    m.section(&e);
    if s.start {
        m.section(&StartSection { function_index: ni + 4 });
    }
    let mut c = CodeSection::new();
    let mut v = Function::new([]);
    v.instruction(&Instruction::I32Const(1));
    v.instruction(&Instruction::End);
    c.function(&v);
    let mut mf = Function::new([]);
    mf.instruction(&Instruction::I64Const(0));
    mf.instruction(&Instruction::End);
    c.function(&mf);
    let mut run = Function::new(s.run_locals);
    for i in &s.run {
        run.instruction(i);
    }
    run.instruction(&Instruction::End);
    c.function(&run);
    // rec(n): n == 0 ? 0 : rec(n - 1) + 1
    let mut r = Function::new([]);
    for i in [
        Instruction::LocalGet(0),
        Instruction::I32Eqz,
        Instruction::If(BlockType::Result(ValType::I32)),
        Instruction::I32Const(0),
        Instruction::Else,
        Instruction::LocalGet(0),
        Instruction::I32Const(1),
        Instruction::I32Sub,
        Instruction::Call(ni + 3),
        Instruction::I32Const(1),
        Instruction::I32Add,
        Instruction::End,
        Instruction::End,
    ] {
        r.instruction(&i);
    }
    c.function(&r);
    // spin(): loop { g0 += 1 }
    let mut sp = Function::new([]);
    for i in [
        Instruction::Loop(BlockType::Empty),
        Instruction::GlobalGet(0),
        Instruction::I32Const(1),
        Instruction::I32Add,
        Instruction::GlobalSet(0),
        Instruction::Br(0),
        Instruction::End,
        Instruction::End,
    ] {
        sp.instruction(&i);
    }
    c.function(&sp);
    // deep(): call deep. No parameters, locals or operands: frame weight 1, the most frames per weight unit.
    let mut d = Function::new([]);
    d.instruction(&Instruction::Call(ni + 5));
    d.instruction(&Instruction::End);
    c.function(&d);
    m.section(&c);
    m.finish()
}

fn run_body(ni: u32, which: &str) -> Vec<Instruction<'static>> {
    use Instruction as I;
    match which {
        "rec" => vec![I::LocalGet(1), I::Call(ni + 3), I::Drop, I::I32Const(0)],
        "spin" => vec![I::Call(ni + 4), I::I32Const(0)],
        "status" => vec![I::I32Const(7)],
        "unreachable" => vec![I::Unreachable],
        "div0" => vec![I::I32Const(1), I::LocalGet(1), I::I32DivS],
        "oob" => vec![I::I32Const(-16), I::I32Load(MemArg { offset: 0, align: 2, memory_index: 0 })],
        // memory[64] += 1; g0 += 1; return 0
        "count" => vec![
            I::I32Const(64),
            I::I32Const(64),
            I::I32Load(MemArg { offset: 0, align: 2, memory_index: 0 }),
            I::I32Const(1),
            I::I32Add,
            I::I32Store(MemArg { offset: 0, align: 2, memory_index: 0 }),
            I::GlobalGet(0),
            I::I32Const(1),
            I::I32Add,
            I::GlobalSet(0),
            I::I32Const(0),
        ],
        "grow" => vec![I::I32Const(1), I::MemoryGrow(0), I::Drop, I::I32Const(0)],
        "deep" => vec![I::Call(ni + 5), I::I32Const(0)],
        // loop { op(0, 0, 64 KiB); g0 += 1 }
        "fill" | "copy" => vec![
            I::Loop(BlockType::Empty),
            I::I32Const(0),
            I::I32Const(0),
            I::I32Const(65536),
            if which == "fill" { I::MemoryFill(0) } else { I::MemoryCopy { src_mem: 0, dst_mem: 0 } },
            I::GlobalGet(0),
            I::I32Const(1),
            I::I32Add,
            I::GlobalSet(0),
            I::Br(0),
            I::End,
            I::I32Const(0),
        ],
        // A segment with no check point leaves the fuel negative when it calls rec(batch).
        "late" => vec![I::Block(BlockType::Empty), I::LocalGet(1), I::Call(ni + 3), I::Drop, I::End, I::I32Const(0)],
        _ => unreachable!(),
    }
}

fn module(which: &str) -> Vec<u8> {
    build(Spec { run: run_body(0, which), ..Default::default() })
}

fn load(wasm: &[u8], s: Settings) -> Script {
    let e = runtime::engine(&s).unwrap();
    Script::load(&e, s, wasm).unwrap()
}

const BIG: u64 = 1 << 50;

/// Run `f` on a thread with the native stack a guest thread needs at the largest `max_wasm_stack` used here.
fn big_stack(f: impl FnOnce() + Send + 'static) {
    let s = Settings { max_wasm_stack: 8 << 20, ..Settings::default() };
    std::thread::Builder::new().stack_size(s.thread_stack()).spawn(f).unwrap().join().unwrap();
}

#[test]
fn depth_faults_at_the_same_depth_under_two_stack_limits() {
    big_stack(|| {
        let wasm = module("rec");
        let mut deepest = Vec::new();
        for limit in [1u32 << 12, 1 << 16] {
            for stack in [1usize << 20, 8 << 20] {
                let s = Settings {
                    meter: Config { depth_limit: limit, ..Config::default() },
                    max_wasm_stack: stack,
                    ..Settings::default()
                };
                let script = load(&wasm, s);
                let ok = |n: u32| -> Result<u64, Fault> {
                    script.instantiate(Box::new(NoServices)).unwrap().run_system(0, n, BIG)
                };
                let (mut lo, mut hi) = (0u32, 1 << 20);
                while hi - lo > 1 {
                    let mid = (lo + hi) / 2;
                    if ok(mid).is_ok() { lo = mid } else { hi = mid }
                }
                assert_eq!(ok(hi), Err(Fault::Depth { limit }), "limit {limit}, stack {stack}");
                deepest.push((limit, lo));
            }
        }
        // The same deepest call under both stack settings, for each limit; and a bigger limit goes deeper.
        assert_eq!(deepest[0], deepest[1]);
        assert_eq!(deepest[2], deepest[3]);
        assert!(deepest[2].1 > deepest[0].1);
    });
}

#[test]
fn fuel_faults_after_the_same_work_every_time() {
    big_stack(|| {
        let script = load(&module("spin"), Settings::default());
        let mut seen = Vec::new();
        for _ in 0..3 {
            let mut i = script.instantiate(Box::new(NoServices)).unwrap();
            let r = i.run_system(0, 0, 1_000_000);
            seen.push((r, i.global_bits()));
        }
        assert_eq!(seen[0].0, Err(Fault::Fuel { budget: 1_000_000 }));
        assert!(seen[0].1[0] > 1000, "the loop ran: {:?}", seen[0].1);
        assert!(seen.iter().all(|x| *x == seen[0]));
    });
}

#[test]
fn fuel_used_is_identical_across_instances() {
    big_stack(|| {
        let script = load(&module("rec"), Settings::default());
        let a = script.instantiate(Box::new(NoServices)).unwrap().run_system(0, 500, BIG).unwrap();
        let b = script.instantiate(Box::new(NoServices)).unwrap().run_system(0, 500, BIG).unwrap();
        assert_eq!(a, b);
        assert!(a > 500);
    });
}

#[test]
fn each_trap_kind_is_classified() {
    big_stack(|| {
        for (which, batch, want) in [
            ("status", 0, Fault::Status(7)),
            ("unreachable", 0, Fault::Unreachable),
            ("div0", 0, Fault::DivideByZero),
            ("oob", 0, Fault::OutOfBounds),
        ] {
            let script = load(&module(which), Settings::default());
            let got = script.instantiate(Box::new(NoServices)).unwrap().run_system(0, batch, BIG);
            assert_eq!(got, Err(want.clone()), "{which}");
            assert!(want.is_deterministic());
        }
    });
}

#[test]
fn a_bad_pointer_into_a_host_call_is_a_fault() {
    big_stack(|| {
        use Instruction as I;
        let wasm = build(Spec {
            imports: vec![("pe", "log", vec![ValType::I32, ValType::I32], vec![])],
            run: vec![I::I32Const(-16), I::I32Const(100), I::Call(0), I::I32Const(0)],
            ..Default::default()
        });
        let script = load(&wasm, Settings::default());
        assert_eq!(script.instantiate(Box::new(NoServices)).unwrap().run_system(0, 0, BIG), Err(Fault::BadPointer));
    });
}

#[test]
fn reset_restores_memory_and_globals() {
    big_stack(|| {
        let script = load(&module("count"), Settings::default());
        let mut i = script.instantiate(Box::new(NoServices)).unwrap();
        let before = (i.memory().to_vec(), i.global_bits());
        for _ in 0..3 {
            i.run_system(0, 0, BIG).unwrap();
        }
        assert_eq!(&i.memory()[64..68], &3u32.to_le_bytes());
        assert_eq!(i.global_bits(), vec![3]);
        assert!(i.reset());
        assert_eq!((i.memory().to_vec(), i.global_bits()), before);
        // The same call after a reset gives the same result as on a fresh instance.
        i.run_system(0, 0, BIG).unwrap();
        assert_eq!(&i.memory()[64..68], &1u32.to_le_bytes());
    });
}

#[test]
fn reset_reports_grown_memory() {
    big_stack(|| {
        let script = load(&module("grow"), Settings::default());
        let mut i = script.instantiate(Box::new(NoServices)).unwrap();
        i.run_system(0, 0, BIG).unwrap();
        assert!(!i.reset(), "a grown memory needs a fresh instance");
    });
}

#[test]
fn memory_growth_past_the_limit_fails_the_same_way() {
    big_stack(|| {
        let s = Settings { max_memory: 64 << 10, ..Settings::default() };
        let script = load(&module("grow"), s);
        let mut i = script.instantiate(Box::new(NoServices)).unwrap();
        i.run_system(0, 0, BIG).unwrap(); // memory.grow returned -1 and the module dropped it
        assert!(i.reset(), "the memory did not grow");
    });
}

#[test]
fn the_pass_is_deterministic() {
    big_stack(|| {
        let wasm = module("rec");
        for cfg in [Config::default(), Config { nan_observe: true, ..Config::default() }] {
            let a = meter::instrument(&wasm, cfg).unwrap();
            let b = meter::instrument(&wasm, cfg).unwrap();
            assert_eq!(a, b);
        }
    });
}

fn refused(s: Spec) -> RefusalKind {
    match meter::instrument(&build(s), Config::default()) {
        Err(r) => r.kind,
        Ok(_) => panic!("accepted"),
    }
}

#[test]
fn modules_outside_the_abi_are_refused() {
    big_stack(|| {
        let ok = || Spec { run: run_body(0, "status"), ..Default::default() };
        let with_import = |m, n, p: Vec<ValType>, r: Vec<ValType>| Spec {
            imports: vec![(m, n, p, r)],
            run: run_body(1, "status"),
            ..Default::default()
        };
        assert!(meter::instrument(&build(ok()), Config::default()).is_ok());
        assert_eq!(refused(with_import("wasi_snapshot_preview1", "fd_write", vec![], vec![])), RefusalKind::Import);
        assert_eq!(refused(with_import("pe", "clock", vec![], vec![ValType::I64])), RefusalKind::Import);
        assert_eq!(refused(with_import("pe", "rand", vec![ValType::I32], vec![ValType::I32])), RefusalKind::Import);
        assert_eq!(refused(Spec { extra_export: Some("pe_fuel"), ..ok() }), RefusalKind::ReservedName);
        assert_eq!(refused(Spec { extra_export: Some("pe_g0"), ..ok() }), RefusalKind::ReservedName);
        assert!(meter::instrument(&build(Spec { extra_export: Some("pe_gain"), ..ok() }), Config::default()).is_ok());
        assert_eq!(refused(Spec { start: true, ..ok() }), RefusalKind::Abi);
        assert_eq!(refused(Spec { skip_run_export: true, ..ok() }), RefusalKind::Abi);
        assert_eq!(refused(Spec { shared_memory: true, ..ok() }), RefusalKind::Feature);
        // A tail call is a feature outside 5.3.
        assert_eq!(
            refused(Spec {
                run: vec![Instruction::I32Const(0), Instruction::I32Const(0), Instruction::ReturnCall(2)],
                ..ok()
            }),
            RefusalKind::Feature
        );
        // table.set changes state the reset does not restore.
        assert_eq!(
            refused(Spec {
                table: true,
                run: vec![
                    Instruction::I32Const(0),
                    Instruction::RefNull(HeapType::FUNC),
                    Instruction::TableSet(0),
                    Instruction::I32Const(0)
                ],
                ..ok()
            }),
            RefusalKind::Feature
        );
    });
}

#[test]
fn bulk_memory_is_charged_by_its_length() {
    big_stack(|| {
        // Each 64 KiB fill or copy costs 1024 units (logic.md 5.2), so a budget of 10^6 stops the loop within about
        // 1000 iterations, after at most ~64 MB. Uncharged, it would run ~10^5 (shader-reviewer M1).
        for which in ["fill", "copy"] {
            let script = load(&module(which), Settings::default());
            let mut i = script.instantiate(Box::new(NoServices)).unwrap();
            assert_eq!(i.run_system(0, 0, 1_000_000), Err(Fault::Fuel { budget: 1_000_000 }), "{which}");
            let n = i.global_bits()[0];
            let per = 1_000_000 / (65536 / meter::BULK_BYTES_PER_FUEL as u64);
            assert!(n > per * 9 / 10 && n <= per, "{which}: {n} iterations");
        }
        // A one-page grow costs a page's worth.
        let script = load(&module("grow"), Settings::default());
        let used = script.instantiate(Box::new(NoServices)).unwrap().run_system(0, 0, BIG).unwrap();
        assert!(used >= 65536 / meter::BULK_BYTES_PER_FUEL as u64, "grow used {used}");
    });
}

#[test]
fn a_depth_trap_is_labelled_depth_when_the_fuel_is_already_negative() {
    big_stack(|| {
        // The smallest limit under which pe_run can call rec(0); one less traps at rec's prologue.
        let wasm = module("late");
        let with =
            |limit: u32| Settings { meter: Config { depth_limit: limit, ..Config::default() }, ..Default::default() };
        let run = |limit: u32, budget: u64| {
            load(&wasm, with(limit)).instantiate(Box::new(NoServices)).unwrap().run_system(0, 0, budget)
        };
        let fits = (1..256).find(|&l| run(l, BIG).is_ok()).expect("a limit that fits");
        assert_eq!(run(fits - 1, BIG), Err(Fault::Depth { limit: fits - 1 }));
        // With a budget the block's segment overdraws, the count flushed at the call is negative: still Depth
        // (shader-reviewer L3). And with room in depth, the same budget is a fuel fault at rec's entry.
        assert_eq!(run(fits - 1, 2), Err(Fault::Depth { limit: fits - 1 }));
        assert_eq!(run(fits, 2), Err(Fault::Fuel { budget: 2 }));
    });
}

#[test]
fn weight_one_recursion_trips_the_depth_meter_before_the_native_stack() {
    big_stack(|| {
        // The worst case of bytes per weight unit: 65,536 frames of weight 1 under the default 8 MiB stack
        // (shader-reviewer L4).
        let s = Settings::default();
        let script = load(&module("deep"), s);
        let r = script.instantiate(Box::new(NoServices)).unwrap().run_system(0, 0, BIG);
        assert_eq!(r, Err(Fault::Depth { limit: s.meter.depth_limit }));
    });
}
