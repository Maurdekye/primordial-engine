//! The batch runner over a real store (logic.md 3.3-3.6), with `tests/scripts/tally`: systems run in order and see
//! each other's writes, the copy-in skip changes nothing a script sees, a system's objects are split into batches,
//! creates get the ids pe-core derives, destroys and events apply, random streams are keyed by object and stream,
//! every fault discards its whole call, and a replay after a restore gives the same states.

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::{Arc, OnceLock};

use pe_core::{Frame, Hash, LineId, ObjectId};
use pe_logic::run::{self, EngineComponent, Loaded, Logic, Refused, RunSettings, StageCtx, StageReport};
use pe_logic::runtime::{self, Fault, Script, Settings};
use pe_sdk::layout::Stage;
use pe_sim::{ComponentDesc, ComponentId, FieldDesc, FieldKind, Registry, SimStore};

const KEY: Hash = Hash([7; 32]);

/// The script's tag (logic.md 3.2), under an id the world would derive from the version. Until pe-sim takes
/// zero-field components (world-model's slice D) the test tag is a 1-byte padding row; the runner reads its size.
const TAG: ComponentId = ComponentId(ComponentId::FIRST_USER + 99);

/// The engine components the test worlds let scripts bind: pe-world's `Light`, under its own id and description.
const LIGHT: ComponentId = pe_world::components::ids::LIGHT;
const ENGINE: &[EngineComponent<'static>] = &[EngineComponent { name: "pe.Light", id: LIGHT, scripts_may_write: true }];

/// A `Light` row: kind, intensity, range, colour, inner and outer angle.
fn light(intensity: f32) -> Vec<u8> {
    [
        0u32.to_le_bytes(),
        intensity.to_le_bytes(),
        1f32.to_le_bytes(),
        1f32.to_le_bytes(),
        1f32.to_le_bytes(),
        1f32.to_le_bytes(),
        0f32.to_le_bytes(),
        0f32.to_le_bytes(),
    ]
    .concat()
}

fn intensity(row: &[u8]) -> f32 {
    f32::from_le_bytes(row[4..8].try_into().unwrap())
}

fn tag_desc() -> ComponentDesc {
    ComponentDesc::new(TAG, "pe.tag", vec![FieldDesc::new("_tag", FieldKind::U8, 0, 1)]).unwrap()
}

/// What a fault test expects of the fault.
type Expect = fn(&Fault) -> bool;

fn big_stack(f: impl FnOnce() + Send + 'static) {
    let s = Settings::default();
    std::thread::Builder::new().stack_size(s.thread_stack()).spawn(f).unwrap().join().unwrap();
}

fn tally() -> &'static [u8] {
    static WASM: OnceLock<Vec<u8>> = OnceLock::new();
    WASM.get_or_init(|| {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests").join("scripts").join("tally");
        let target = Path::new(env!("CARGO_TARGET_TMPDIR")).join("scripts");
        pe_logic::build::build(&dir, &pe_logic::build::engine_sdk(), &target, true).unwrap_or_else(|e| panic!("{e}"))
    })
}

/// A world with the tally script bound: its store and its logic, and the store id of each of its components.
struct World {
    store: SimStore,
    logic: Logic,
    names: Vec<String>,
    ids: Vec<ComponentId>,
    line: LineId,
}

impl World {
    fn new(settings: RunSettings) -> World {
        let s = Settings::default();
        let e = runtime::engine(&s).unwrap();
        let script = Script::load(&e, s, tally()).unwrap_or_else(|e| panic!("{e}"));
        let m = run::read_manifest(&script).unwrap();
        // The script's own components get ids here; `pe.Light` is the engine's.
        let own: Vec<_> = m.components.iter().filter(|c| !c.name.starts_with("pe.")).cloned().collect();
        let ids: Vec<ComponentId> =
            (0..own.len()).map(|k| ComponentId(ComponentId::FIRST_USER + 100 + k as u128)).collect();
        let mut registry = Registry::new();
        registry.register(tag_desc()).unwrap();
        registry.register_type::<pe_world::components::Light>().unwrap();
        for (c, &id) in own.iter().zip(&ids) {
            registry.register(run::component_desc(c, id).unwrap()).unwrap();
        }
        let names = own.iter().map(|c| c.name.clone()).collect();
        let store = SimStore::new(Arc::new(registry), 0x5eed);
        let loaded =
            Loaded::bind(KEY, script, m, &ids, TAG, ENGINE, store.registry()).unwrap_or_else(|e| panic!("{e}"));
        let mut logic = Logic::new(settings);
        logic.insert(loaded);
        World { store, logic, names, ids, line: LineId::origin() }
    }

    fn id(&self, name: &str) -> ComponentId {
        self.ids[self.names.iter().position(|n| n == name).unwrap()]
    }

    /// An object the script is attached to: its rows, and the script's tag.
    fn object(&mut self, n: u128, rows: &[(&str, Vec<u8>)]) -> ObjectId {
        let o = self.untagged(n, rows);
        self.store.insert_raw(TAG, o, &[0]).unwrap();
        o
    }

    /// An object with rows of the script's components but not its tag: the script is not attached to it.
    fn untagged(&mut self, n: u128, rows: &[(&str, Vec<u8>)]) -> ObjectId {
        let o = ObjectId::from_u128(n);
        self.store.create(o).unwrap();
        for (name, row) in rows {
            self.store.insert_raw(self.id(name), o, row).unwrap();
        }
        o
    }

    fn row(&self, name: &str, o: ObjectId) -> Option<Vec<u8>> {
        self.store.get_raw(self.id(name), o).map(<[u8]>::to_vec)
    }

    fn stage(&mut self, stage: Stage, faulted: &BTreeSet<Hash>) -> StageReport {
        self.logic.run_stage(&mut self.store, &mut StageCtx::new(self.line, 60, stage, faulted))
    }

    /// One frame with a guard: the objects it names may not be destroyed.
    fn frame_guarded(&mut self, f: u64, held: &[ObjectId]) -> (StageReport, StageReport) {
        self.store.set_frame(Frame(f));
        let none = BTreeSet::new();
        let guard = |o: ObjectId| held.contains(&o);
        let mut out = Vec::new();
        for stage in [Stage::PrePhysics, Stage::PostPhysics] {
            let mut ctx = StageCtx::new(self.line, 60, stage, &none);
            ctx.guard = Some(&guard);
            out.push(self.logic.run_stage(&mut self.store, &mut ctx));
        }
        let post = out.pop().unwrap();
        (out.pop().unwrap(), post)
    }

    /// One frame with the world's edit check on engine rows.
    fn frame_checked(&mut self, f: u64, check: run::Validate<'_>) -> (StageReport, StageReport) {
        self.store.set_frame(Frame(f));
        let none = BTreeSet::new();
        let mut out = Vec::new();
        for stage in [Stage::PrePhysics, Stage::PostPhysics] {
            let mut ctx = StageCtx::new(self.line, 60, stage, &none);
            ctx.validate = Some(check);
            out.push(self.logic.run_stage(&mut self.store, &mut ctx));
        }
        let post = out.pop().unwrap();
        (out.pop().unwrap(), post)
    }

    /// One frame: both logic stages, as the replica runs them.
    fn frame(&mut self, f: u64) -> (StageReport, StageReport) {
        self.store.set_frame(Frame(f));
        let none = BTreeSet::new();
        (self.stage(Stage::PrePhysics, &none), self.stage(Stage::PostPhysics, &none))
    }
}

fn le(words: &[u64], widths: &[usize]) -> Vec<u8> {
    words.iter().zip(widths).flat_map(|(w, &n)| w.to_le_bytes()[..n].to_vec()).collect()
}

fn counter(n: u32, step: u32) -> Vec<u8> {
    le(&[n as u64, step as u64], &[4, 4])
}

fn u32s(b: &[u8]) -> Vec<u32> {
    b.chunks(4).map(|w| u32::from_le_bytes(w.try_into().unwrap())).collect()
}

fn u64s(b: &[u8]) -> Vec<u64> {
    b.chunks(8).map(|w| u64::from_le_bytes(w.try_into().unwrap())).collect()
}

/// An object id's bytes as rows hold them: `hi` then `lo`, little-endian.
fn id_bytes(o: ObjectId) -> Vec<u8> {
    let v = o.to_u128();
    le(&[(v >> 64) as u64, v as u64], &[8, 8])
}

fn trouble(trigger: u32, victim: ObjectId) -> Vec<u8> {
    let mut r = le(&[trigger as u64, 0f32.to_bits() as u64], &[4, 4]);
    r.extend(id_bytes(victim));
    r
}

#[test]
fn systems_run_in_order_and_the_copy_in_skip_changes_nothing() {
    big_stack(|| {
        let mut out = Vec::new();
        for skip in [true, false] {
            let mut w = World::new(RunSettings { skip_unchanged: skip, ..RunSettings::default() });
            let objs: Vec<ObjectId> = (1..=3)
                .map(|k| w.object(k, &[("Counter", counter(0, k as u32)), ("Doubled", le(&[0], &[8]))]))
                .collect();
            let mut fuel = 0;
            let mut skipped = 0;
            for f in 1..=3 {
                let (pre, post) = w.frame(f);
                assert!(pre.faults.is_empty() && post.faults.is_empty(), "{pre:?} {post:?}");
                fuel += pre.fuel + post.fuel;
                skipped += pre.skipped_in;
            }
            for (k, &o) in objs.iter().enumerate() {
                let n = 3 * (k as u32 + 1);
                assert_eq!(u32s(&w.row("Counter", o).unwrap()), [n, k as u32 + 1]);
                // `double` runs after `count` in the same stage and sees its writes (3.5).
                assert_eq!(u64s(&w.row("Doubled", o).unwrap()), [2 * n as u64]);
            }
            // `double` reads the Counter column `count` just wrote: skipped when the skip is on.
            assert_eq!(skipped > 0, skip);
            out.push((w.store.state_hash(), fuel));
        }
        assert_eq!(out[0], out[1], "the skip must change neither the state nor the fuel");
    });
}

#[test]
fn a_system_over_many_objects_runs_in_batches() {
    big_stack(|| {
        let mut w = World::new(RunSettings::default());
        let n = 5000u128;
        for k in 1..=n {
            w.object(k, &[("Counter", counter(k as u32, 1)), ("Doubled", le(&[0], &[8]))]);
        }
        let (pre, _) = w.frame(1);
        assert!(pre.faults.is_empty(), "{:?}", pre.faults);
        // Two systems over 5000 rows, more than one batch each.
        assert!(pre.calls > 2, "calls {}", pre.calls);
        assert_eq!(pre.written.len(), 2 * n as usize);
        for k in [1u128, 2, 1999, 2000, 2001, 4999, 5000] {
            let o = ObjectId::from_u128(k);
            assert_eq!(u32s(&w.row("Counter", o).unwrap())[0], k as u32 + 1);
            assert_eq!(u64s(&w.row("Doubled", o).unwrap())[0], 2 * (k as u64 + 1));
        }
    });
}

#[test]
fn creates_destroys_and_events_apply_and_replay_identically() {
    big_stack(|| {
        let mut w = World::new(RunSettings::default());
        let sp = w.object(1, &[("Spawner", le(&[3, 0], &[4, 4]))]);
        let start = w.store.snapshot();
        let mut first = Vec::new();
        for f in 1..=6 {
            let (pre, post) = w.frame(f);
            assert!(pre.faults.is_empty() && post.faults.is_empty(), "{pre:?} {post:?}");
            first.push((w.store.state_hash(), pre.created.clone(), post.destroyed.clone(), pre.events.clone()));
        }
        // Frames 1-3 each create one child, with the id pe-core derives for the spawner's first spawn of the frame.
        for f in 1..=3u64 {
            let child = ObjectId::spawned(w.line, sp, Frame(f), 0);
            assert_eq!(first[f as usize - 1].1, [(child, sp)]);
            let ev = &first[f as usize - 1].3;
            assert_eq!(
                (ev.len(), ev[0].index, ev[0].object, ev[0].bytes.clone()),
                (1, 0, sp, (f as u32 - 1).to_le_bytes().to_vec())
            );
            // Reaped in the post-physics stage two frames after its birth.
            assert_eq!(first[f as usize + 1].2, [child]);
        }
        assert!(first[3].1.is_empty() && first[0].2.is_empty());
        assert_eq!(u32s(&w.row("Spawner", sp).unwrap()), [0, 3]);
        assert_eq!(w.store.objects(), [sp]);

        // Restore and replay: the same states, ids and events.
        w.store.restore(&start).unwrap();
        for f in 1..=6 {
            let (pre, post) = w.frame(f);
            assert_eq!((w.store.state_hash(), pre.created, post.destroyed, pre.events), first[f as usize - 1]);
        }
    });
}

#[test]
fn random_streams_are_keyed_by_frame_object_and_stream() {
    big_stack(|| {
        let draws = |frame: u64| {
            let mut w = World::new(RunSettings::default());
            let a = w.object(1, &[("Dice", vec![0; 16])]);
            let b = w.object(2, &[("Dice", vec![0; 16])]);
            w.frame(frame);
            [u64s(&w.row("Dice", a).unwrap()), u64s(&w.row("Dice", b).unwrap())].concat()
        };
        let d = draws(5);
        let set: BTreeSet<u64> = d.iter().copied().collect();
        assert_eq!(set.len(), 4, "four distinct draws: {d:x?}");
        assert_eq!(draws(5), d, "the same in a fresh world");
        assert_ne!(draws(6), d, "another frame, other bits");
    });
}

#[test]
fn a_fault_discards_the_whole_call_and_the_script_stops() {
    big_stack(|| {
        let cases: [(u32, Expect); 5] = [
            (1, |f| *f == Fault::InvalidRow { component: "Trouble".into(), field: "out".into() }),
            (2, |f| *f == Fault::Unreachable),
            (3, |f| matches!(f, Fault::Fuel { .. })),
            (4, |f| matches!(f, Fault::Depth { .. })),
            // The shadow stack overflows below address 0 before the depth meter fires (logic.md 3.6).
            (6, |f| *f == Fault::OutOfBounds),
        ];
        for (trigger, expect) in cases {
            let mut w = World::new(RunSettings { fuel_per_call: 2_000_000, ..RunSettings::default() });
            // A well-behaved row first: its write is discarded with the faulting row's.
            let calm = w.object(1, &[("Trouble", trouble(0, ObjectId::NONE))]);
            w.object(2, &[("Trouble", trouble(trigger, ObjectId::NONE))]);
            let before = w.row("Trouble", calm);
            let (_, post) = w.frame(1);
            assert_eq!(post.faults.len(), 1, "trigger {trigger}: {post:?}");
            let fault = &post.faults[0];
            assert!(expect(&fault.fault), "trigger {trigger}: {:?}", fault.fault);
            assert_eq!((fault.script, fault.system.as_str(), fault.frame), (KEY, "trouble", Frame(1)));
            assert!(post.written.is_empty());
            assert_eq!(w.row("Trouble", calm), before, "trigger {trigger}");

            // Kept faulted by the caller, the script runs nothing in the next frame.
            w.store.set_frame(Frame(2));
            let faulted = BTreeSet::from([KEY]);
            let pre = w.stage(Stage::PrePhysics, &faulted);
            let post = w.stage(Stage::PostPhysics, &faulted);
            assert_eq!((pre.calls, post.calls), (0, 0));
        }
    });
}

#[test]
fn a_script_destroys_only_objects_carrying_its_tag() {
    big_stack(|| {
        // An object with no component of the script.
        let mut w = World::new(RunSettings::default());
        let foreign = ObjectId::from_u128(50);
        w.store.create(foreign).unwrap();
        w.object(1, &[("Trouble", trouble(5, foreign))]);
        let (_, post) = w.frame(1);
        assert!(matches!(post.faults[0].fault, Fault::BadCommand(_)), "{post:?}");
        assert!(w.store.exists(foreign));

        // The hole the coordinator named (mfe83e04db541): an object given one of the script's components, as an edit
        // could, but not the script's tag, is not the script's to destroy.
        let mut w = World::new(RunSettings::default());
        let theirs = w.untagged(60, &[("Counter", counter(0, 1))]);
        w.object(1, &[("Trouble", trouble(5, theirs))]);
        let (_, post) = w.frame(1);
        assert!(matches!(post.faults[0].fault, Fault::BadCommand(_)), "{post:?}");
        assert!(w.store.exists(theirs));

        // An object the script is attached to (it carries the tag), though the script did not create it, is
        // (coordinator m3abd64580972: the destroy right is the tag; attaching needs the object's edit right).
        let mut w = World::new(RunSettings::default());
        let attached = w.object(70, &[("Counter", counter(0, 1))]);
        w.object(1, &[("Trouble", trouble(5, attached))]);
        let (_, post) = w.frame(1);
        assert!(post.faults.is_empty(), "{post:?}");
        assert_eq!(post.destroyed, [attached]);
        assert!(!w.store.exists(attached));
    });
}

#[test]
fn only_objects_carrying_the_tag_run_the_script_and_detaching_keeps_their_rows() {
    big_stack(|| {
        // Three objects with a Counter: 1 and 3 carry the tag, 2 does not. Then 3 is detached (its tag removed).
        let mut w = World::new(RunSettings::default());
        let a = w.object(1, &[("Counter", counter(0, 1))]);
        let b = w.untagged(2, &[("Counter", counter(0, 1))]);
        let c = w.object(3, &[("Counter", counter(0, 1))]);
        let c_id = w.id("Counter");
        let (pre, _) = w.frame(1);
        assert_eq!(pre.written, vec![(c_id, a), (c_id, c)]);
        assert_eq!(u32s(&w.row("Counter", b).unwrap()), [0, 1]);
        w.store.remove_raw(TAG, c).unwrap();
        let (pre, _) = w.frame(2);
        assert_eq!(pre.written, vec![(c_id, a)]);
        assert_eq!(u32s(&w.row("Counter", a).unwrap()), [2, 1]);
        assert_eq!(u32s(&w.row("Counter", c).unwrap()), [1, 1], "a detached object keeps its rows");
    });
}

#[test]
fn what_a_script_creates_carries_its_tag() {
    big_stack(|| {
        let mut w = World::new(RunSettings::default());
        let sp = w.object(1, &[("Spawner", le(&[1, 0], &[4, 4]))]);
        let (pre, _) = w.frame(1);
        let child = ObjectId::spawned(w.line, sp, Frame(1), 0);
        assert_eq!(pre.created, [(child, sp)]);
        assert_eq!(w.store.get_raw(TAG, child), Some(&[0u8][..]));
    });
}

#[test]
fn a_held_spawn_is_not_destroyed() {
    big_stack(|| {
        let mut w = World::new(RunSettings::default());
        let sp = w.object(1, &[("Spawner", le(&[1, 0], &[4, 4]))]);
        w.frame(1);
        let child = ObjectId::spawned(w.line, sp, Frame(1), 0);
        w.frame(2);
        // Due for reaping at frame 3, but held: refused, not a fault, and the child stays.
        let (_, post) = w.frame_guarded(3, &[child]);
        assert!(post.faults.is_empty(), "{post:?}");
        assert_eq!(post.refused, [(child, Refused::Guarded(child))]);
        assert!(w.store.exists(child));
        // Released, it goes at the next frame.
        let (_, post) = w.frame(4);
        assert_eq!(post.destroyed, [child]);
    });
}

#[test]
fn destroying_a_spawn_orphans_what_others_put_beneath_it() {
    big_stack(|| {
        let mut w = World::new(RunSettings::default());
        let sp = w.object(1, &[("Spawner", le(&[1, 0], &[4, 4]))]);
        w.frame(1);
        let child = ObjectId::spawned(w.line, sp, Frame(1), 0);
        // Someone's add-on under the spawn, with its own child.
        let addon = ObjectId::from_u128(70);
        let inner = ObjectId::from_u128(71);
        w.store.create(addon).unwrap();
        w.store.create(inner).unwrap();
        w.store.set_parent(addon, Some(child), None).unwrap();
        w.store.set_parent(inner, Some(addon), None).unwrap();
        w.frame(2);
        let (_, post) = w.frame(3);
        assert!(post.faults.is_empty(), "{post:?}");
        assert_eq!((post.destroyed.as_slice(), post.orphaned.as_slice()), ([child].as_slice(), [addon].as_slice()));
        assert!(!w.store.exists(child));
        assert_eq!((w.store.parent(addon), w.store.parent(inner)), (None, Some(addon)));
    });
}

#[test]
fn a_fault_while_applying_discards_the_rows_written_before_it() {
    big_stack(|| {
        // `spawn` writes its Spawner row, then creates. The create fails because an object already has the id pe-core
        // derives for it, as a world limit could make it fail: the row written first must go too (3.6;
        // design-reviewer m6ff8571e295a).
        let mut w = World::new(RunSettings::default());
        let sp = w.object(1, &[("Spawner", le(&[3, 0], &[4, 4]))]);
        let squatter = ObjectId::spawned(w.line, sp, Frame(1), 0);
        w.store.create(squatter).unwrap();
        let (hash, row) = (w.store.state_hash(), w.row("Spawner", sp));
        let (pre, _) = w.frame(1);
        assert_eq!(pre.faults.len(), 1, "{pre:?}");
        assert!(matches!(&pre.faults[0].fault, Fault::Other(m) if m.starts_with("create")), "{pre:?}");
        assert_eq!(pre.faults[0].system, "spawn");
        assert_eq!(w.row("Spawner", sp), row, "the row written before the failed create is discarded");
        assert!(pre.written.is_empty() && pre.created.is_empty() && pre.events.is_empty(), "{pre:?}");
        let mut objects = vec![sp, squatter];
        objects.sort();
        assert_eq!(w.store.objects(), objects, "nothing created");
        w.store.set_frame(Frame(0));
        assert_eq!(w.store.state_hash(), hash);
    });
}

#[test]
fn a_destroy_the_world_refuses_leaves_the_orphans_where_they_were() {
    big_stack(|| {
        let mut w = World::new(RunSettings::default());
        let sp = w.object(1, &[("Spawner", le(&[1, 0], &[4, 4]))]);
        w.frame(1);
        let child = ObjectId::spawned(w.line, sp, Frame(1), 0);
        let addon = ObjectId::from_u128(70);
        w.store.create(addon).unwrap();
        w.store.set_parent(addon, Some(child), None).unwrap();
        w.frame(2);
        // Due at frame 3; the world's own destroy refuses it.
        w.store.set_frame(Frame(3));
        let none = BTreeSet::new();
        let mut refuse = |_: &mut SimStore, o: ObjectId| Err(pe_core::Error::UnknownObject(o));
        for stage in [Stage::PrePhysics, Stage::PostPhysics] {
            let mut ctx = StageCtx::new(w.line, 60, stage, &none);
            ctx.destroy = Some(&mut refuse);
            let r = w.logic.run_stage(&mut w.store, &mut ctx);
            assert!(r.faults.is_empty(), "{r:?}");
            if stage == Stage::PostPhysics {
                assert!(matches!(r.refused.as_slice(), [(c, Refused::World(_))] if *c == child), "{r:?}");
                assert!(r.destroyed.is_empty() && r.orphaned.is_empty(), "{r:?}");
            }
        }
        // The add-on was not made a root: the refusal undid the orphaning with it.
        assert!(w.store.exists(child));
        assert_eq!(w.store.parent(addon), Some(child));
    });
}

#[test]
fn a_column_a_script_declared_read_is_copied_in_again_whatever_it_wrote_there() {
    big_stack(|| {
        // `sneak` declares Sneak read and writes 999 into it anyway; `peek`, next in the stage over the same objects,
        // must see the store's row. Before the fix the skip trusted the read column and `peek` saw 999.
        let mut w = World::new(RunSettings::default());
        let o = w.object(1, &[("Sneak", le(&[5], &[4])), ("Peeked", le(&[0], &[4]))]);
        let (pre, _) = w.frame(1);
        assert!(pre.faults.is_empty(), "{pre:?}");
        assert_eq!(u32s(&w.row("Sneak", o).unwrap()), [5], "a write to a read column is never applied");
        assert_eq!(u32s(&w.row("Peeked", o).unwrap()), [5], "peek saw the store's row, not the forged write");
    });
}

#[test]
fn the_write_back_finds_exactly_the_changed_rows_in_runs_and_gaps() {
    big_stack(|| {
        // 1000 objects whose counter steps by 0, but for two: only their rows change, though the 256-byte blocks
        // around them hold many unchanged rows (column_raw, N12).
        let mut w = World::new(RunSettings::default());
        for k in 1..=1000u128 {
            let step = if k == 500 || k == 777 { 1 } else { 0 };
            if k == 3 {
                // A gap in the first rows: `double`'s first batch is not one run of the Counter column.
                w.object(k, &[("Counter", counter(7, step))]);
            } else {
                w.object(k, &[("Counter", counter(7, step)), ("Doubled", le(&[14], &[8]))]);
            }
        }
        // A gap: objects with a Counter and no Doubled, so `double`'s batch is not one run of the Counter column.
        for k in [2000u128, 2002] {
            w.object(k, &[("Counter", counter(1, 1))]);
        }
        w.object(2001, &[("Counter", counter(3, 0)), ("Doubled", le(&[0], &[8]))]);
        let (pre, _) = w.frame(1);
        assert!(pre.faults.is_empty(), "{pre:?}");
        let c = w.id("Counter");
        let d = w.id("Doubled");
        let o = ObjectId::from_u128;
        let mut want =
            vec![(c, o(500)), (c, o(777)), (c, o(2000)), (c, o(2002)), (d, o(500)), (d, o(777)), (d, o(2001))];
        let mut got = pre.written.clone();
        want.sort();
        got.sort();
        assert_eq!(got, want);
        assert_eq!(u64s(&w.row("Doubled", o(2001)).unwrap()), [6]);
        assert_eq!(u32s(&w.row("Counter", o(2002)).unwrap()), [2, 1]);
        assert_eq!(w.row("Doubled", o(2000)), None);
        assert_eq!(w.row("Doubled", o(3)), None);
        assert_eq!(u64s(&w.row("Doubled", o(4)).unwrap()), [14]);
    });
}

#[test]
fn a_change_only_in_the_tail_of_a_row_that_straddles_a_block_is_written() {
    big_stack(|| {
        // Wide rows are 24 bytes: row 10 holds bytes 240..264 of the batch's column, across the first 256-byte
        // boundary, and its `tail` (bytes 256..264) is the only change: block 0 compares equal, block 1 does not
        // (shader-reviewer on 569cf551).
        let mut w = World::new(RunSettings::default());
        for k in 1..=40u128 {
            let flag = u64::from(k == 11);
            w.object(k, &[("Wide", le(&[flag, 5, 0], &[8, 8, 8]))]);
        }
        let (_, post) = w.frame(1);
        assert!(post.faults.is_empty(), "{post:?}");
        let o = ObjectId::from_u128;
        assert_eq!(post.written, vec![(w.id("Wide"), o(11))]);
        assert_eq!(u64s(&w.row("Wide", o(11)).unwrap()), [1, 5, 1]);
        assert_eq!(u64s(&w.row("Wide", o(12)).unwrap()), [0, 5, 0]);
    });
}

#[test]
fn finding_rows_is_one_merge_walk_per_column_per_call_whatever_the_batches() {
    big_stack(|| {
        // Enough objects for several batches of `count` and `double`. Linear: `count` joins Counter and the tag (2 N)
        // and walks Counter for rows (N); `double` joins Counter, Doubled and the tag (3 N) and walks two columns
        // for rows (2 N): 8 N steps. A walk per batch from row 0 adds about N (B - 1) / 2 steps per column
        // (shader-reviewer on 569cf551).
        const N: u128 = 6000;
        let mut w = World::new(RunSettings::default());
        for k in 1..=N {
            w.object(k, &[("Counter", counter(1, 1)), ("Doubled", le(&[2], &[8]))]);
        }
        let (pre, _) = w.frame(1);
        assert!(pre.faults.is_empty(), "{pre:?}");
        assert!(pre.batches >= 6, "want several batches per system: {}", pre.batches);
        assert!(pre.join_steps <= 8 * N as u64, "{} steps for {N} objects", pre.join_steps);
        assert_eq!(u64s(&w.row("Doubled", ObjectId::from_u128(N)).unwrap()), [4]);
    });
}

#[test]
fn a_hold_on_an_orphan_does_not_stop_the_destroy_and_removed_lists_what_went() {
    big_stack(|| {
        // Under the spawn: `kid` (tagged, removed with it) and someone's `addon` (untagged, orphaned) holding
        // `inner` (tagged, but under the add-on, so it stays with it). Holds on the add-on and on `inner` do not stop
        // the destroy (world-model.md 9.2a, @design-reviewer m52c574e02ece); a hold on `kid` does.
        let build = |w: &mut World| {
            let sp = w.object(1, &[("Spawner", le(&[1, 0], &[4, 4]))]);
            w.frame(1);
            let child = ObjectId::spawned(w.line, sp, Frame(1), 0);
            let kid = w.object(80, &[]);
            let addon = w.untagged(81, &[]);
            let inner = w.object(82, &[]);
            w.store.set_parent(kid, Some(child), None).unwrap();
            w.store.set_parent(addon, Some(child), None).unwrap();
            w.store.set_parent(inner, Some(addon), None).unwrap();
            w.frame(2);
            (child, kid, addon, inner)
        };
        let mut w = World::new(RunSettings::default());
        let (child, kid, addon, inner) = build(&mut w);
        let (_, post) = w.frame_guarded(3, &[addon, inner]);
        assert!(post.faults.is_empty() && post.refused.is_empty(), "{post:?}");
        assert_eq!(post.destroyed, [child]);
        let mut removed = post.removed.clone();
        removed.sort();
        let mut want = vec![child, kid];
        want.sort();
        assert_eq!(removed, want);
        assert_eq!(post.orphaned, [addon]);
        assert!(w.store.exists(inner) && w.store.parent(inner) == Some(addon));

        let mut w = World::new(RunSettings::default());
        let (child, kid, _, _) = build(&mut w);
        let (_, post) = w.frame_guarded(3, &[kid]);
        assert_eq!(post.refused, [(child, Refused::Guarded(kid))]);
        assert!(post.removed.is_empty() && w.store.exists(child));
    });
}

#[test]
fn a_script_writes_an_engine_component_only_where_it_is_attached() {
    big_stack(|| {
        let mut w = World::new(RunSettings::default());
        let lamp = w.object(1, &[]);
        w.store.insert_raw(LIGHT, lamp, &light(1.0)).unwrap();
        let other = w.untagged(2, &[]);
        w.store.insert_raw(LIGHT, other, &light(1.0)).unwrap();
        let ok = |_: ComponentId, _: ObjectId, _: &[u8]| Ok(());
        let (_, post) = w.frame_checked(1, &ok);
        assert!(post.faults.is_empty(), "{post:?}");
        assert_eq!(post.written, vec![(LIGHT, lamp)]);
        assert_eq!(intensity(w.store.get_raw(LIGHT, lamp).unwrap()), 2.0);
        assert_eq!(intensity(w.store.get_raw(LIGHT, other).unwrap()), 1.0);
    });
}

#[test]
fn an_engine_row_the_world_refuses_faults_the_call() {
    big_stack(|| {
        // The world's check (pe-world's edit rules in the engine) refuses intensities above 1.5: the call faults with
        // its reason, nothing is clamped, and the row keeps its value (coordinator mcb505a8e97cf).
        let mut w = World::new(RunSettings::default());
        let lamp = w.object(1, &[]);
        w.store.insert_raw(LIGHT, lamp, &light(1.0)).unwrap();
        let check = |c: ComponentId, _: ObjectId, row: &[u8]| {
            if c == LIGHT && intensity(row) > 1.5 { Err("intensity".to_string()) } else { Ok(()) }
        };
        let (_, post) = w.frame_checked(1, &check);
        assert_eq!(post.faults.len(), 1, "{post:?}");
        assert_eq!(post.faults[0].fault, Fault::InvalidRow { component: "Light".into(), field: "intensity".into() });
        assert_eq!(intensity(w.store.get_raw(LIGHT, lamp).unwrap()), 1.0);
    });
}

#[test]
fn a_create_of_an_engine_component_is_refused() {
    big_stack(|| {
        let mut w = World::new(RunSettings::default());
        w.object(1, &[("Beacon", le(&[1], &[4]))]);
        let (_, post) = w.frame(1);
        assert!(matches!(&post.faults[0].fault, Fault::BadCommand(m) if m.contains("engine")), "{post:?}");
        assert!(post.created.is_empty());
    });
}

#[test]
fn binding_checks_the_engine_table_and_the_layout() {
    // The table leaves `pe.Light` out, forbids writing it, or the registry holds another layout under its id.
    let s = Settings::default();
    let e = runtime::engine(&s).unwrap();
    let try_bind = |engine: &[EngineComponent<'_>], light_desc: ComponentDesc| {
        let script = Script::load(&e, s, tally()).unwrap();
        let m = run::read_manifest(&script).unwrap();
        let own: Vec<_> = m.components.iter().filter(|c| !c.name.starts_with("pe.")).cloned().collect();
        let ids: Vec<ComponentId> =
            (0..own.len()).map(|k| ComponentId(ComponentId::FIRST_USER + 100 + k as u128)).collect();
        let mut registry = Registry::new();
        registry.register(tag_desc()).unwrap();
        registry.register_engine(light_desc).unwrap();
        for (c, &id) in own.iter().zip(&ids) {
            registry.register(run::component_desc(c, id).unwrap()).unwrap();
        }
        Loaded::bind(KEY, script, m, &ids, TAG, engine, &registry).err().map(|e| e.to_string())
    };
    let real = <pe_world::components::Light as pe_sim::Component>::desc();
    assert_eq!(try_bind(ENGINE, real.clone()), None);
    let none = try_bind(&[], real.clone()).unwrap();
    assert!(none.contains("not an engine component"), "{none}");
    let ro = [EngineComponent { name: "pe.Light", id: LIGHT, scripts_may_write: false }];
    let ro = try_bind(&ro, real).unwrap();
    assert!(ro.contains("may not write pe.Light"), "{ro}");
    let other = ComponentDesc::new(LIGHT, "Light", vec![FieldDesc::new("kind", FieldKind::U32, 0, 8)]).unwrap();
    let other = try_bind(ENGINE, other).unwrap();
    assert!(other.contains("layout differs"), "{other}");
}

#[test]
fn the_sdk_engine_components_are_the_engine_layouts() {
    use pe_sdk::component::Component as SdkComponent;
    fn layout<T: SdkComponent>() -> Vec<(String, u8, u32, u32)> {
        T::FIELDS.iter().map(|f| (f.name.to_string(), f.kind as u8, f.offset, f.count)).collect()
    }
    fn engine(d: &ComponentDesc) -> Vec<(String, u8, u32, u32)> {
        // pe-sim's field kinds by name, mapped to the SDK's codes through the runner's own description of each.
        d.fields().iter().map(|f| (f.name.clone(), sdk_code(f.kind), f.offset, f.count)).collect()
    }
    fn sdk_code(k: FieldKind) -> u8 {
        use pe_sdk::layout::FieldKind as S;
        (match k {
            FieldKind::U32 => S::U32,
            FieldKind::I64 => S::I64,
            FieldKind::F32 => S::F32,
            FieldKind::F64 => S::F64,
            other => panic!("no SDK kind for {other:?} in these components"),
        }) as u8
    }
    let light = <pe_world::components::Light as pe_sim::Component>::desc();
    assert_eq!(layout::<pe_sdk::engine::Light>(), engine(&light));
    assert_eq!(std::mem::size_of::<pe_sdk::engine::Light>(), light.size());
    let transform = <pe_sim::Transform as pe_sim::Component>::desc();
    assert_eq!(layout::<pe_sdk::engine::Transform>(), engine(&transform));
    assert_eq!(std::mem::size_of::<pe_sdk::engine::Transform>(), transform.size());
}

#[test]
fn an_engine_row_with_no_world_check_faults_the_call() {
    big_stack(|| {
        // Fail closed (shader-reviewer on 53daefdf): a world that forgot `StageCtx::validate` gets a fault, not an
        // unchecked engine row.
        let mut w = World::new(RunSettings::default());
        let lamp = w.object(1, &[]);
        w.store.insert_raw(LIGHT, lamp, &light(1.0)).unwrap();
        let (_, post) = w.frame(1);
        let no_check = |f: &Fault| matches!(f, Fault::InvalidRow { component, field } if component == "Light" && field.contains("no world check"));
        assert!(no_check(&post.faults[0].fault), "{post:?}");
        assert_eq!(intensity(w.store.get_raw(LIGHT, lamp).unwrap()), 1.0);
    });
}

#[test]
fn an_untagged_object_under_a_surviving_parent_is_not_moved() {
    big_stack(|| {
        // A (tagged, destroyed) > B (untagged) > C (tagged) > D (untagged): only A is removed, B is orphaned, and C
        // and D stay where they are (shader-reviewer on 53daefdf).
        let mut w = World::new(RunSettings::default());
        let a = w.object(90, &[]);
        let b = w.untagged(91, &[]);
        let c = w.object(92, &[]);
        let d = w.untagged(93, &[]);
        w.store.set_parent(b, Some(a), None).unwrap();
        w.store.set_parent(c, Some(b), None).unwrap();
        w.store.set_parent(d, Some(c), None).unwrap();
        w.object(1, &[("Trouble", trouble(5, a))]);
        let (_, post) = w.frame(1);
        assert!(post.faults.is_empty(), "{post:?}");
        assert_eq!((post.removed.as_slice(), post.orphaned.as_slice()), ([a].as_slice(), [b].as_slice()));
        assert_eq!((w.store.parent(b), w.store.parent(c), w.store.parent(d)), (None, Some(b), Some(c)));
    });
}
