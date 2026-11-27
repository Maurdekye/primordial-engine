//! Positions for script authors (logic.md 8) over a real store, with `tests/scripts/positions`:
//! - pe-sdk's `Pos` in wasm gives pe-core's bytes for `checked_add`, `Pos - Pos` and the distance, on 100,000 seeded
//!   cases and the range's edges;
//! - `pe.Pose` is the store's composed world transform, a root's `Transform` place is a `Pos` and a child's an
//!   offset, and `translate` moves both;
//! - the trapping `+` faults the call deterministically; `pe.Pose` can't be written or read from other objects.

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::{Arc, OnceLock};

use pe_core::{Frame, Hash, LineId, ObjectId, Offset, Pos};
use pe_logic::manifest::Manifest;
use pe_logic::pose;
use pe_logic::run::{self, EngineComponent, Loaded, Logic, RunSettings, StageCtx, StageReport};
use pe_logic::runtime::{self, Fault, Script, Settings};
use pe_sdk::layout::{Access, Stage};
use pe_sim::{ComponentDesc, ComponentId, Registry, SimStore, Transform};

const KEY: Hash = Hash([9; 32]);
const TAG: ComponentId = ComponentId(ComponentId::FIRST_USER + 99);
const ENGINE: &[EngineComponent<'static>] =
    &[EngineComponent { name: "pe.Transform", id: ComponentId::TRANSFORM, scripts_may_write: true }];

fn big_stack(f: impl FnOnce() + Send + 'static) {
    let s = Settings::default();
    std::thread::Builder::new().stack_size(s.thread_stack()).spawn(f).unwrap().join().unwrap();
}

fn positions() -> &'static [u8] {
    static WASM: OnceLock<Vec<u8>> = OnceLock::new();
    WASM.get_or_init(|| {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests").join("scripts").join("positions");
        let target = Path::new(env!("CARGO_TARGET_TMPDIR")).join("scripts");
        pe_logic::build::build(
            &pe_logic::build::Toolchain::current().unwrap(),
            &dir,
            &pe_logic::build::engine_sdk(),
            &target,
            true,
        )
        .unwrap_or_else(|e| panic!("{e}"))
    })
}

struct World {
    store: SimStore,
    logic: Logic,
    names: Vec<String>,
    ids: Vec<ComponentId>,
}

fn load() -> (Script, Manifest) {
    let s = Settings::default();
    let e = runtime::engine(&s).unwrap();
    let script = Script::load(&e, s, positions()).unwrap_or_else(|e| panic!("{e}"));
    let m = run::read_manifest(&script).unwrap();
    (script, m)
}

fn registry_for(m: &Manifest) -> (Registry, Vec<String>, Vec<ComponentId>) {
    let own: Vec<_> = m.components.iter().filter(|c| !c.name.starts_with("pe.")).cloned().collect();
    let ids: Vec<ComponentId> =
        (0..own.len()).map(|k| ComponentId(ComponentId::FIRST_USER + 100 + k as u128)).collect();
    let mut registry = Registry::new();
    registry.register(ComponentDesc::new(TAG, "pe.tag", Vec::new()).unwrap()).unwrap();
    for (c, &id) in own.iter().zip(&ids) {
        registry.register(run::component_desc(c, id).unwrap()).unwrap();
    }
    (registry, own.iter().map(|c| c.name.clone()).collect(), ids)
}

impl World {
    fn new() -> World {
        let (script, m) = load();
        let (registry, names, ids) = registry_for(&m);
        let store = SimStore::new(Arc::new(registry), 0x5eed);
        let loaded =
            Loaded::bind(KEY, script, m, &ids, TAG, ENGINE, store.registry()).unwrap_or_else(|e| panic!("{e}"));
        let mut logic = Logic::new(RunSettings::default());
        logic.insert(loaded);
        World { store, logic, names, ids }
    }

    fn id(&self, name: &str) -> ComponentId {
        self.ids[self.names.iter().position(|n| n == name).unwrap()]
    }

    fn object(&mut self, n: u128, rows: &[(&str, Vec<u8>)]) -> ObjectId {
        let o = ObjectId::from_u128(n);
        self.store.create(o).unwrap();
        for (name, row) in rows {
            self.store.insert_raw(self.id(name), o, row).unwrap();
        }
        self.store.insert_raw(TAG, o, &[]).unwrap();
        o
    }

    fn row(&self, name: &str, o: ObjectId) -> Vec<u8> {
        self.store.get_raw(self.id(name), o).unwrap().to_vec()
    }

    fn frame(&mut self, f: u64) -> (StageReport, StageReport) {
        self.store.set_frame(Frame(f));
        let none = BTreeSet::new();
        let ok = |_: ComponentId, _: ObjectId, _: &[u8]| Ok(());
        let mut out = Vec::new();
        for stage in [Stage::PrePhysics, Stage::PostPhysics] {
            let mut ctx = StageCtx::new(LineId::origin(), 60, stage, &none);
            ctx.validate = Some(&ok);
            out.push(self.logic.run_stage(&mut self.store, &mut ctx));
        }
        let post = out.pop().unwrap();
        (out.pop().unwrap(), post)
    }
}

/// A pe-core `Pos` from its parts (the corner of `chunk`, moved by `local`, which is exact for `local` in [0, 1024)).
fn pos(chunk: [i64; 3], local: [f64; 3]) -> Pos {
    Pos::at_chunk(chunk).unwrap().checked_add(Offset(local)).unwrap()
}

fn f32s(b: &[u8]) -> Vec<f32> {
    b.chunks(4).map(|c| f32::from_le_bytes(c.try_into().unwrap())).collect()
}

fn canon32(x: f32) -> f32 {
    if x == 0.0 { 0.0 } else { x }
}

/// xorshift64, for the seeded cases.
struct Seq(u64);

impl Seq {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn pick<T: Copy>(&mut self, xs: &[T]) -> T {
        xs[(self.next() % xs.len() as u64) as usize]
    }

    fn chunk(&mut self) -> i64 {
        match self.next() % 6 {
            0 => (self.next() % 9) as i64 - 4,
            1 => (self.next() >> 24) as i64 * self.pick(&[1, -1]),
            2 => self.pick(&[i64::MAX, i64::MAX - 1, i64::MIN + 1, i64::MIN + 2, 1 << 61, -(1 << 61), 1 << 53]),
            _ => (self.next() % 2001) as i64 - 1000,
        }
    }

    fn local(&mut self) -> f64 {
        match self.next() % 8 {
            0 => self.pick(&[0.0, 1023.0, f64::from_bits(1024f64.to_bits() - 1), 5e-324, 512.0]),
            _ => (self.next() >> 11) as f64 / (1u64 << 53) as f64 * 1024.0,
        }
    }

    fn d(&mut self) -> f32 {
        match self.next() % 10 {
            // Finite only: the store refuses a non-finite f32 row (pe-sdk's own tests cover infinite and NaN offsets).
            0 => self.pick(&[0.0, -0.0, f32::MAX, f32::MIN, -1e-45, 1024.0, -1024.0]),
            1 => Some(f32::from_bits(self.next() as u32)).filter(|x| x.is_finite()).unwrap_or(1.5),
            2 => ((self.next() % 4_000_001) as f32 - 2_000_000.0) * 1e15,
            _ => ((self.next() % 2_000_001) as f32 - 1_000_000.0) / 64.0,
        }
    }
}

fn case_row(a: Pos, b: Pos, d: [f32; 3]) -> Vec<u8> {
    let mut row = Vec::with_capacity(176);
    row.extend_from_slice(&pose::pos_row(a));
    row.extend_from_slice(&pose::pos_row(b));
    row.extend_from_slice(&pose::pos_row(Pos::ORIGIN));
    for v in d.iter().chain(&[0.0; 3]).chain(&[0.0]) {
        row.extend_from_slice(&v.to_le_bytes());
    }
    row.extend_from_slice(&0u32.to_le_bytes());
    row
}

#[test]
fn the_sdk_pos_in_wasm_gives_pe_cores_bytes() {
    big_stack(|| {
        let mut w = World::new();
        let mut seq = Seq(0x9E37_79B9_7F4A_7C15);
        let n = 100_000u128;
        let mut cases = Vec::new();
        for k in 0..n {
            let a = pos([seq.chunk(), seq.chunk(), seq.chunk()], [seq.local(), seq.local(), seq.local()]);
            let b = pos([seq.chunk(), seq.chunk(), seq.chunk()], [seq.local(), seq.local(), seq.local()]);
            let d = [seq.d(), seq.d(), seq.d()];
            let o = w.object(1 + k, &[("PosCase", case_row(a, b, d))]);
            cases.push((o, a, b, d));
        }
        let (pre, _) = w.frame(1);
        assert!(pre.faults.is_empty(), "{:?}", pre.faults);
        let (mut ok, mut refused) = (0, 0);
        for (o, a, b, d) in cases {
            let row = w.row("PosCase", o);
            let want = a.checked_add(Offset(d.map(|v| v as f64)));
            let got_ok = u32::from_le_bytes(row[172..176].try_into().unwrap());
            match want {
                Ok(s) => {
                    assert_eq!(got_ok, 1, "{a:?} + {d:?}");
                    assert_eq!(&row[96..144], &pose::pos_row(s), "{a:?} + {d:?}");
                    ok += 1;
                }
                Err(_) => {
                    assert_eq!(got_ok, 0, "{a:?} + {d:?} is out of range");
                    refused += 1;
                }
            }
            let diff = (a - b).0.map(|x| canon32(x as f32));
            let got = f32s(&row[156..168]);
            assert_eq!(got.iter().map(|x| x.to_bits()).collect::<Vec<_>>(), diff.map(f32::to_bits), "{a:?} - {b:?}");
            let dist = (a - b).length2().sqrt() as f32;
            assert_eq!(f32s(&row[168..172])[0].to_bits(), dist.to_bits(), "|{a:?} - {b:?}|");
        }
        // Both outcomes are exercised, each many times.
        assert!(ok > 10_000 && refused > 1_000, "{ok} in range, {refused} out");
    });
}

fn transform(chunk: [i64; 3], p: [f64; 3], rot: [f32; 4], scale: [f32; 3]) -> Transform {
    Transform { chunk, pos: p, rot, scale, _pad: 0 }
}

fn where_row(aim: [f32; 3], step: [f32; 3]) -> Vec<u8> {
    let mut row = Vec::with_capacity(136);
    row.extend_from_slice(&pose::pos_row(Pos::ORIGIN));
    row.extend_from_slice(&pose::pos_row(Pos::ORIGIN));
    for v in aim {
        row.extend_from_slice(&v.to_le_bytes());
    }
    row.extend_from_slice(&0u32.to_le_bytes());
    for v in [0.0f32; 3].iter().chain(&step) {
        row.extend_from_slice(&v.to_le_bytes());
    }
    row
}

#[test]
fn pose_is_the_composed_world_transform_and_place_follows_the_hierarchy() {
    big_stack(|| {
        let mut w = World::new();
        let h = std::f32::consts::FRAC_1_SQRT_2;
        // A root far out, turned a quarter about z and scaled by 2; a child under it, and a grandchild under that.
        let root = w.object(1, &[("Where", where_row([1.0, 0.0, 0.0], [1.0, 0.0, 0.0]))]);
        let root_t = transform([7_000_000_000, -3, 0], [100.5, 2.0, 1023.0], [0.0, 0.0, h, h], [2.0; 3]);
        w.store.insert(root, root_t).unwrap();
        let child = w.object(2, &[("Where", where_row([0.0, 0.0, 0.0], [0.0, 0.5, 0.0]))]);
        w.store.insert(child, transform([0; 3], [3.0, 0.0, 0.0], [0.0, 0.0, 0.0, 1.0], [1.0; 3])).unwrap();
        w.store.set_parent(child, Some(root), None).unwrap();
        let grand = w.object(3, &[("Where", where_row([0.0, 1.0, 0.0], [0.0; 3]))]);
        w.store.insert(grand, transform([0; 3], [0.0, 0.0, 4.0], [0.0, 0.0, 0.0, 1.0], [0.5; 3])).unwrap();
        w.store.set_parent(grand, Some(child), None).unwrap();

        let (pre, post) = w.frame(1);
        assert!(pre.faults.is_empty() && post.faults.is_empty(), "{pre:?} {post:?}");
        for (o, root_flag) in [(root, 1u32), (child, 0), (grand, 0)] {
            let row = w.row("Where", o);
            let world = w.store.world_transform(o).unwrap();
            assert_eq!(&row[0..48], &pose::pos_row(world.pos), "pe.Pose's position is the store's world position");
            assert_eq!(u32::from_le_bytes(row[108..112].try_into().unwrap()), root_flag);
        }
        // `walk` ran in pre-physics, before `locate`: the root moved one metre along its parent's (the world's) x,
        // the child half a metre along its parent's y.
        assert_eq!(w.store.get::<Transform>(root).unwrap().pos, [101.5, 2.0, 1023.0]);
        assert_eq!(w.store.get::<Transform>(child).unwrap().pos, [3.0, 0.5, 0.0]);
        // A root's place is its Pos (no difference from the pose's), a child's its offset from its parent.
        assert_eq!(f32s(&w.row("Where", root)[112..124]), [0.0; 3]);
        assert_eq!(f32s(&w.row("Where", child)[112..124]), [3.0, 0.5, 0.0]);
        assert_eq!(f32s(&w.row("Where", grand)[112..124]), [0.0, 0.0, 4.0]);
        // `ahead` is the aim point in the object's own space, placed in the world: own +x of the root is world +y
        // at scale 2, so 2 m along world y from its pose.
        let r = w.row("Where", root);
        let (home, ahead) = (w.store.world_transform(root).unwrap().pos, decode(&r[48..96]));
        let d = (ahead - home).0;
        assert!(d[0].abs() < 1e-6 && (d[1] - 2.0).abs() < 1e-6 && d[2].abs() < 1e-6, "{d:?}");
        // The grandchild's world scale is the product down the chain; own +y of it is 0.5 * 2 = 1 m along world -x.
        let g = w.row("Where", grand);
        let d = (decode(&g[48..96]) - w.store.world_transform(grand).unwrap().pos).0;
        assert!((d[0] + 1.0).abs() < 1e-5 && d[1].abs() < 1e-5 && d[2].abs() < 1e-5, "{d:?}");
    });
}

/// A 48-byte `Pos` row back into a pe-core `Pos`.
fn decode(b: &[u8]) -> Pos {
    let c = |a: usize| i64::from_le_bytes(b[8 * a..8 * a + 8].try_into().unwrap());
    let l = |a: usize| f64::from_le_bytes(b[24 + 8 * a..32 + 8 * a].try_into().unwrap());
    pos([c(0), c(1), c(2)], [l(0), l(1), l(2)])
}

#[test]
fn the_trapping_add_faults_the_call_the_same_way_every_time() {
    big_stack(|| {
        let mut faults = Vec::new();
        for _ in 0..2 {
            let mut w = World::new();
            let near_edge = pos([i64::MAX, 0, 0], [1000.0, 0.0, 0.0]);
            let mut row = pose::pos_row(near_edge).to_vec();
            row.extend_from_slice(&100f32.to_le_bytes());
            row.extend_from_slice(&0u32.to_le_bytes());
            let o = w.object(1, &[("Far", row.clone())]);
            let (pre, _) = w.frame(1);
            assert_eq!(pre.faults.len(), 1, "{pre:?}");
            assert_eq!(w.row("Far", o), row, "a faulted call writes nothing");
            faults.push((pre.faults[0].system.clone(), pre.faults[0].fault.clone()));
        }
        assert_eq!(faults[0], faults[1]);
        assert_eq!(faults[0].0, "overflow");
        assert_eq!(faults[0].1, Fault::Unreachable, "a guest panic is the unreachable trap (pe-sdk's panic handler)");
    });
}

#[test]
fn pose_is_read_only_and_only_of_the_bound_object() {
    let try_bind = |edit: &dyn Fn(&mut Manifest)| {
        let (script, mut m) = load();
        edit(&mut m);
        let (registry, _, ids) = registry_for(&m);
        Loaded::bind(KEY, script, m, &ids, TAG, ENGINE, &registry).err().map(|e| e.to_string())
    };
    assert_eq!(try_bind(&|_| {}), None);
    let write = try_bind(&|m| {
        let s = m.systems.iter_mut().find(|s| s.name == "locate").unwrap();
        s.bound.iter_mut().find(|(n, _)| n == "pe.Pose").unwrap().1 = Access::Write;
    })
    .unwrap();
    assert!(write.contains("may not write pe.Pose"), "{write}");
    let other = try_bind(&|m| m.systems[0].reads_other.push("pe.Pose".into())).unwrap();
    assert!(other.contains("only the bound object"), "{other}");
    let layout = try_bind(&|m| {
        let c = m.components.iter_mut().find(|c| c.name == "pe.Pose").unwrap();
        c.fields.iter_mut().find(|f| f.name == "root").unwrap().kind = pe_sdk::layout::FieldKind::I32;
    })
    .unwrap();
    assert!(layout.contains("layout differs"), "{layout}");
}

#[test]
fn the_sdk_pose_is_the_runners_layout_and_its_id_is_free() {
    use pe_sdk::component::Component as _;
    let sdk: Vec<_> =
        pe_sdk::engine::Pose::FIELDS.iter().map(|f| (f.name.to_string(), f.offset, f.count, f.kind as u8)).collect();
    let desc = pose::desc();
    let kinds = [
        pe_sdk::layout::FieldKind::Pos,
        pe_sdk::layout::FieldKind::F32,
        pe_sdk::layout::FieldKind::F32,
        pe_sdk::layout::FieldKind::U32,
    ];
    let host: Vec<_> =
        desc.fields().iter().zip(kinds).map(|(f, k)| (f.name.clone(), f.offset, f.count, k as u8)).collect();
    assert_eq!(sdk, host);
    assert_eq!(std::mem::size_of::<pe_sdk::engine::Pose>(), pose::SIZE);
    assert_eq!(desc.size(), pose::SIZE);
    // No component of the world's registry uses the id the runner reserves for it.
    let world = pe_world::components::engine_registry();
    assert!(world.get(pose::ID).is_none());
}
