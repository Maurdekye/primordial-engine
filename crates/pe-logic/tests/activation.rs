//! Activation on a machine whose build lags (logic.md 6.2; user ruling Q3a, "objects keep running the old code, then
//! jump"; @coordinator-opus). At `activate_at` the world re-keys every row of the script to the new
//! version's ids (ids hash the version, world-model.md 3.2), so a machine still building binds its *old* module under
//! ids derived from the *new* version: a logic-only edit keeps running the old code; an edit that changed a
//! component's schema cannot bind there and pauses. Either way the machine replays from `activate_at` once its build
//! is ready and reaches the author's state.
//!
//! This test plays the parts pe-world and pe-replica play (id derivation, re-key, binding); pe-logic's part is that
//! `Loaded::bind` accepts the old module under the new ids, or refuses it when a schema changed.

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Arc;

use pe_core::{Frame, Hash, LineId, ObjectId};
use pe_logic::manifest::{Component, Manifest};
use pe_logic::run::{self, Loaded, Logic, RunSettings, StageCtx};
use pe_logic::runtime::{self, Script, Settings};
use pe_sdk::layout::Stage;
use pe_sim::{ComponentDesc, ComponentId, Registry, SimStore};

const KEY: Hash = Hash([9; 32]);
const ACTIVATE: u64 = 10;
const LAG: u64 = 6;

fn big_stack(f: impl FnOnce() + Send + 'static) {
    let s = Settings::default();
    std::thread::Builder::new().stack_size(s.thread_stack()).spawn(f).unwrap().join().unwrap();
}

fn copy_tree(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for e in std::fs::read_dir(from).unwrap() {
        let e = e.unwrap();
        let name = e.file_name();
        if name == "target" {
            continue;
        }
        if e.file_type().unwrap().is_dir() {
            copy_tree(&e.path(), &to.join(&name));
        } else {
            std::fs::copy(e.path(), to.join(&name)).unwrap();
        }
    }
}

/// One version of the lamp: its source edited by `edit`, built, with V (the source's hash, as world-model.md 3.2).
struct Version {
    v: Hash,
    wasm: Vec<u8>,
}

fn version(tag: &str, edit: impl Fn(String) -> String) -> Version {
    let here = Path::new(env!("CARGO_MANIFEST_DIR"));
    let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("activation").join(tag);
    let _ = std::fs::remove_dir_all(&root);
    copy_tree(&here.join("../pe-sdk"), &root.join("crates/pe-sdk"));
    let lamp = root.join("crates/pe-logic/tests/scripts/lamp");
    copy_tree(&here.join("tests/scripts/lamp"), &lamp);
    let lib = lamp.join("src/lib.rs");
    let src = edit(std::fs::read_to_string(&lib).unwrap());
    std::fs::write(&lib, &src).unwrap();
    std::fs::write(
        root.join("Cargo.toml"),
        "[workspace]\nmembers = [\"crates/pe-sdk\"]\nresolver = \"3\"\n[workspace.package]\nedition = \"2024\"\nlicense = \"MIT OR Apache-2.0\"\npublish = false\n",
    )
    .unwrap();
    let target = root.join("target");
    let wasm = pe_logic::build::build(
        &pe_logic::build::Toolchain::current().unwrap(),
        &lamp,
        &root.join("crates/pe-sdk"),
        &target,
        true,
    )
    .unwrap_or_else(|e| panic!("{e}"));
    Version { v: Hash::of("pe.test.source", src.as_bytes()), wasm }
}

/// A component's id under version `v`, by the world's own derivation (world-model.md 7.9, pe-world
/// `script_component_id`), the one pe-world's `Register` check (`register_script`) rehashes.
fn id_of(v: &Hash, c: &Component) -> ComponentId {
    let d = run::component_desc(c, ComponentId(0)).unwrap();
    pe_world::script::script_component_id(v, &c.name, d.fields().to_vec()).unwrap()
}

/// Version `v`'s tag: zero size, as the world registers it.
fn tag_desc(v: &Hash) -> ComponentDesc {
    pe_world::script::tag_desc(v)
}

fn tag_of(v: &Hash) -> ComponentId {
    tag_desc(v).id()
}

fn loaded(ver: &Version, ids_from: &Hash, registry: &Registry) -> Result<Loaded, String> {
    let s = Settings::default();
    let e = runtime::engine(&s).unwrap();
    let script = Script::load(&e, s, &ver.wasm).unwrap();
    let m: Manifest = run::read_manifest(&script).unwrap();
    let ids: Vec<ComponentId> = m.components.iter().map(|c| id_of(ids_from, c)).collect();
    Loaded::bind(KEY, script, m, &ids, tag_of(ids_from), &[], registry).map_err(|e| e.to_string())
}

fn manifest(ver: &Version) -> Manifest {
    let s = Settings::default();
    let e = runtime::engine(&s).unwrap();
    run::read_manifest(&Script::load(&e, s, &ver.wasm).unwrap()).unwrap()
}

fn run_frames(store: &mut SimStore, logic: &mut Logic, from: u64, n: u64) {
    let none = BTreeSet::new();
    for f in from..from + n {
        store.set_frame(Frame(f));
        for stage in [Stage::PrePhysics, Stage::PostPhysics] {
            let r = logic.run_stage(store, &mut StageCtx::new(LineId::origin(), 60, stage, &none));
            assert!(r.faults.is_empty(), "{r:?}");
        }
    }
}

/// The world at `activate_at`: three lamps carrying v1's rows and tag, re-keyed to v2 (rows of the same name move,
/// fields mapped by name and kind, new fields zero; the tag moves too), as `CommitScript` does (world-model.md 7.9).
fn world_at_activation(v1: &Version, v2: &Version) -> (SimStore, Vec<ObjectId>) {
    let (m1, m2) = (manifest(v1), manifest(v2));
    let mut registry = Registry::new();
    for (v, m) in [(&v1.v, &m1), (&v2.v, &m2)] {
        registry.register(tag_desc(v)).unwrap();
        for c in &m.components {
            registry.register(run::component_desc(c, id_of(v, c)).unwrap()).unwrap();
        }
    }
    let mut store = SimStore::new(Arc::new(registry), 0x5eed);
    store.set_frame(Frame(ACTIVATE));
    let mut lamps = Vec::new();
    for k in 0..3u128 {
        let o = ObjectId::from_u128(100 + k);
        store.create(o).unwrap();
        let blink: Vec<u8> = [1.0f32, 0.25 * k as f32, 0.0].iter().flat_map(|x| x.to_le_bytes()).collect();
        let c1 = |n: &str| m1.components.iter().find(|c| c.name == n).unwrap();
        store.insert_raw(id_of(&v1.v, c1("Blink")), o, &blink).unwrap();
        store.insert_raw(id_of(&v1.v, c1("Glow")), o, &vec![0; c1("Glow").size as usize]).unwrap();
        store.insert_raw(tag_of(&v1.v), o, &[]).unwrap();
        lamps.push(o);
    }
    // The re-key.
    for &o in &lamps {
        for c1 in &m1.components {
            let Some(c2) = m2.components.iter().find(|c| c.name == c1.name) else { continue };
            let old = store.get_raw(id_of(&v1.v, c1), o).unwrap().to_vec();
            let mut new = vec![0u8; c2.size as usize];
            for f2 in &c2.fields {
                if let Some(f1) =
                    c1.fields.iter().find(|f| f.name == f2.name && f.kind == f2.kind && f.count == f2.count)
                {
                    let len = (f1.kind.size() * f1.count) as usize;
                    new[f2.offset as usize..][..len].copy_from_slice(&old[f1.offset as usize..][..len]);
                }
            }
            store.remove_raw(id_of(&v1.v, c1), o).unwrap();
            store.insert_raw(id_of(&v2.v, c2), o, &new).unwrap();
        }
        store.remove_raw(tag_of(&v1.v), o).unwrap();
        store.insert_raw(tag_of(&v2.v), o, &[]).unwrap();
    }
    (store, lamps)
}

fn glow(store: &SimStore, v: &Version, o: ObjectId) -> Vec<u8> {
    let m = manifest(v);
    let c = m.components.iter().find(|c| c.name == "Glow").unwrap();
    store.get_raw(id_of(&v.v, c), o).unwrap().to_vec()
}

/// The author's run from `activate_at`, the lagging machine's run (with what it could bind), then its replay.
fn activate(v1: &Version, v2: &Version) -> (bool, Vec<u8>, Vec<u8>, u64, u64) {
    let (start, lamps) = world_at_activation(v1, v2);
    let mut author = start.clone();
    let mut logic = Logic::new(RunSettings::default());
    logic.insert(loaded(v2, &v2.v, author.registry()).unwrap());
    run_frames(&mut author, &mut logic, ACTIVATE, LAG);

    // The lagging machine: its old module under the new version's ids, if it binds.
    let mut lagging = start.clone();
    let mut logic = Logic::new(RunSettings::default());
    let bound = match loaded(v1, &v2.v, lagging.registry()) {
        Ok(l) => {
            logic.insert(l);
            true
        }
        Err(e) => {
            assert!(e.contains("not registered as declared"), "{e}");
            false
        }
    };
    let before = glow(&lagging, v2, lamps[0]);
    run_frames(&mut lagging, &mut logic, ACTIVATE, LAG);
    let during = glow(&lagging, v2, lamps[0]);

    // Its build is ready: back to `activate_at`, the new module, the same frames again.
    let mut replay = start.clone();
    let mut logic = Logic::new(RunSettings::default());
    logic.insert(loaded(v2, &v2.v, replay.registry()).unwrap());
    run_frames(&mut replay, &mut logic, ACTIVATE, LAG);
    assert_eq!(glow(&replay, v2, lamps[0]), glow(&author, v2, lamps[0]));
    (bound, before, during, replay.state_hash(), author.state_hash())
}

#[test]
fn a_logic_only_edit_keeps_running_the_old_code_on_a_lagging_machine_then_jumps() {
    big_stack(|| {
        let v1 = version("v1-a", |s| s);
        let v2 = version("v2-red", |s| {
            s.replace(
                "Glow { r: 1.0, g: 0.8, b: 0.6, intensity: 1.0 }",
                "Glow { r: 1.0, g: 0.0, b: 0.0, intensity: 1.0 }",
            )
        });
        let (bound, before, during, replay, author) = activate(&v1, &v2);
        assert!(bound, "the old module binds under the new version's ids");
        assert_ne!(during, before, "the old code kept running");
        let warm: Vec<u8> = [1.0f32, 0.8, 0.6, 1.0].iter().flat_map(|x| x.to_le_bytes()).collect();
        let off = vec![0u8; 16];
        assert!(during == warm || during == off, "the old code's colours: {during:?}");
        assert_eq!(replay, author, "the replay reaches the author's state");
    });
}

#[test]
fn a_schema_edit_pauses_the_script_on_a_lagging_machine_then_jumps() {
    big_stack(|| {
        let v1 = version("v1-b", |s| s);
        let v2 = version("v2-schema", |s| {
            s.replace(
                "pub struct Glow { r: f32, g: f32, b: f32, intensity: f32 }",
                "pub struct Glow { r: f32, g: f32, b: f32, intensity: f32, pulse: f32 }",
            )
            .replace(
                "Glow { r: 1.0, g: 0.8, b: 0.6, intensity: 1.0 }",
                "Glow { r: 1.0, g: 0.8, b: 0.6, intensity: 1.0, pulse: 1.0 }",
            )
            .replace(
                "Glow { r: 0.0, g: 0.0, b: 0.0, intensity: 0.0 }",
                "Glow { r: 0.0, g: 0.0, b: 0.0, intensity: 0.0, pulse: 0.0 }",
            )
        });
        let (bound, before, during, replay, author) = activate(&v1, &v2);
        assert!(!bound, "Glow's schema changed: the old module cannot bind under the new ids");
        assert_eq!(during, before, "the script paused");
        assert_eq!(replay, author, "the replay reaches the author's state");
    });
}
