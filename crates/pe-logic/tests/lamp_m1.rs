//! The M1 recording's lamp (M1 redo F5): the source the agent committed, `tests/scripts/lamp-m1`, writes the engine's
//! `Light` without listing it in `script!`'s components. Such a module never binds: in the recording it built on both
//! machines and never ran, and nothing said why. Here: the refusal names each use and the fix as the author types it;
//! with `Light` listed, the same source binds and blinks the engine's Light.

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Arc;

use pe_core::{Frame, Hash, LineId, ObjectId};
use pe_logic::manifest::{Component, Manifest};
use pe_logic::run::{self, EngineComponent, Loaded, Logic, RunSettings, StageCtx};
use pe_logic::runtime::{self, Script, Settings};
use pe_sdk::layout::Stage;
use pe_sim::{ComponentId, SimStore};
use pe_world::components::Light;

/// The refusal for the recorded source: each use, then `script!`'s line as the author would write it.
const REFUSAL: &str = "system blink uses pe.Light, which the script doesn't declare, so it would never run: a script \
                       lists every component its systems use, the engine's too. Write `pe_sdk::script!(components: \
                       [Blink, Light], systems: [blink]);` (Light is pe_sdk::engine::Light)";

fn big_stack(f: impl FnOnce() + Send + 'static) {
    let s = Settings::default();
    std::thread::Builder::new().stack_size(s.thread_stack()).spawn(f).unwrap().join().unwrap();
}

/// The recorded source, as it is in the repo.
fn recorded() -> String {
    let here = Path::new(env!("CARGO_MANIFEST_DIR"));
    std::fs::read_to_string(here.join("tests/scripts/lamp-m1/src/lib.rs")).unwrap()
}

/// The recorded source with `Light` listed, as the refusal says.
fn fixed() -> String {
    let src = recorded();
    assert!(src.contains("components: [Blink]"), "the recorded source moved");
    src.replace("components: [Blink]", "components: [Blink, Light]")
}

/// `src` built as `tests/scripts/lamp-m1` (a copy, under its own name, so two builds never share a folder), with V.
fn build(tag: &str, src: &str) -> (Hash, Vec<u8>) {
    let here = Path::new(env!("CARGO_MANIFEST_DIR"));
    let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("lamp-m1").join(tag);
    let _ = std::fs::remove_dir_all(&root);
    let lamp = root.join("crates/pe-logic/tests/scripts/lamp-m1");
    std::fs::create_dir_all(lamp.join("src")).unwrap();
    for f in ["Cargo.toml", "Cargo.lock"] {
        std::fs::copy(here.join("tests/scripts/lamp-m1").join(f), lamp.join(f)).unwrap();
    }
    std::fs::write(lamp.join("src/lib.rs"), src).unwrap();
    // pe-sdk where the crate's Cargo.toml looks for it (`../../../../pe-sdk`).
    copy_tree(&here.join("../pe-sdk"), &root.join("crates/pe-sdk"));
    let wasm = pe_logic::build::build(
        &pe_logic::build::Toolchain::current().unwrap(),
        &lamp,
        &root.join("crates/pe-sdk"),
        &root.join("target"),
        true,
    )
    .unwrap_or_else(|e| panic!("{e}"));
    (Hash::of("pe.test.source", src.as_bytes()), wasm)
}

fn copy_tree(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for e in std::fs::read_dir(from).unwrap() {
        let e = e.unwrap();
        if e.file_name() == "target" {
            continue;
        }
        if e.file_type().unwrap().is_dir() {
            copy_tree(&e.path(), &to.join(e.file_name()));
        } else {
            std::fs::copy(e.path(), to.join(e.file_name())).unwrap();
        }
    }
}

fn script_of(wasm: &[u8]) -> (Script, Manifest) {
    let s = Settings::default();
    let script = Script::load(&runtime::engine(&s).unwrap(), s, wasm).unwrap();
    let m = run::read_manifest(&script).unwrap();
    (script, m)
}

fn id_of(v: &Hash, c: &Component) -> ComponentId {
    let d = run::component_desc(c, ComponentId(0)).unwrap();
    pe_world::script::script_component_id(v, &c.name, d.fields().to_vec()).unwrap()
}

fn engine() -> Vec<EngineComponent<'static>> {
    (pe_world::script::ENGINE_COMPONENTS.iter())
        .map(|e| EngineComponent { name: e.name, id: e.id, scripts_may_write: e.scripts_may_write })
        .collect()
}

/// The module bound as the world binds it once its commit is registered: the world's engine components, the
/// version's own under V's ids, and its tag. With the store it binds over.
fn bind(v: &Hash, wasm: &[u8]) -> (Result<Loaded, String>, SimStore) {
    let (script, m) = script_of(wasm);
    let own: Vec<&Component> = m.components.iter().filter(|c| !c.name.starts_with("pe.")).collect();
    let mut registry = pe_world::components::engine_registry();
    for c in &own {
        registry.register(run::component_desc(c, id_of(v, c)).unwrap()).unwrap();
    }
    registry.register(pe_world::script::tag_desc(v)).unwrap();
    let ids: Vec<ComponentId> = own.iter().map(|c| id_of(v, c)).collect();
    let tag = pe_world::script::tag_desc(v).id();
    let store = SimStore::new(Arc::new(registry.clone()), 0x5eed);
    let loaded = Loaded::bind(Hash([9; 32]), script, m, &ids, tag, &engine(), &registry).map_err(|e| e.to_string());
    (loaded, store)
}

/// The recorded source builds, and its manifest says why it would never run; bind refuses it with the same words.
#[test]
fn the_recorded_lamp_is_refused_with_the_fix() {
    big_stack(|| {
        let (v, wasm) = build("recorded", &recorded());
        let (_, m) = script_of(&wasm);
        assert_eq!(run::undeclared(&m).as_deref(), Some(REFUSAL));
        let (bound, _) = bind(&v, &wasm);
        assert_eq!(bound.err().as_deref(), Some(format!("bind: {REFUSAL}").as_str()));
    });
}

/// logic.md's Appendix A, the lamp agents learn from, as the spec has it: it builds, lists every component it uses, and
/// binds (@fable-reviewer: the sketch it replaced was not the real macros' syntax).
#[test]
fn the_spec_s_lamp_builds_and_binds() {
    let spec = std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/spec/logic.md")).unwrap();
    let appendix = &spec[spec.find("## Appendix A.").expect("logic.md's Appendix A")..];
    let start = appendix.find("```rust\n").expect("its code block") + "```rust\n".len();
    let src = appendix[start..][..appendix[start..].find("```").expect("the block's end")].to_string();
    big_stack(move || {
        let (v, wasm) = build("spec", &src);
        let (_, m) = script_of(&wasm);
        assert_eq!(run::undeclared(&m), None);
        let (bound, _) = bind(&v, &wasm);
        bound.unwrap_or_else(|e| panic!("{e}"));
    });
}

/// With `Light` listed, as the refusal says, it binds, and the engine's Light blinks: on for the first half of each
/// 1 s period, off for the second (the recorded `Blink { period: 1, phase: 0 }`, at 60 Hz).
#[test]
fn the_fixed_lamp_blinks_the_engine_s_light() {
    big_stack(|| {
        let (v, wasm) = build("fixed", &fixed());
        let (_, m) = script_of(&wasm);
        assert_eq!(run::undeclared(&m), None);
        let (bound, mut store) = bind(&v, &wasm);
        let mut logic = Logic::new(RunSettings::default());
        logic.insert(bound.unwrap_or_else(|e| panic!("{e}")));
        let lamp = ObjectId::from_u128(7);
        store.create(lamp).unwrap();
        let blink = m.components.iter().find(|c| c.name == "Blink").unwrap();
        let bytes: Vec<u8> = [1.0f32, 0.0].iter().flat_map(|x| x.to_le_bytes()).collect();
        store.insert_raw(id_of(&v, blink), lamp, &bytes).unwrap();
        store.insert(lamp, Light { intensity: 1.0, range: 3.0, color: [1.0, 0.8, 0.6], ..Light::default() }).unwrap();
        store.insert_raw(pe_world::script::tag_desc(&v).id(), lamp, &[]).unwrap();
        let none = BTreeSet::new();
        let mut seen = Vec::new();
        for f in 0..120u64 {
            store.set_frame(Frame(f));
            // Engine rows a script writes go through the world's checks (3.3); the world's own is pe-app's test's.
            let allow = |_: ComponentId, _: ObjectId, _: &[u8]| -> Result<(), String> { Ok(()) };
            let mut ctx = StageCtx::new(LineId::origin(), 60, Stage::PrePhysics, &none);
            ctx.validate = Some(&allow);
            let r = logic.run_stage(&mut store, &mut ctx);
            assert!(r.faults.is_empty(), "{r:?}");
            seen.push(store.get::<Light>(lamp).unwrap().intensity);
        }
        let on = |f: usize| if (f % 60) < 30 { 1.0 } else { 0.0 };
        let want: Vec<f32> = (0..120).map(on).collect();
        assert_eq!(seen, want);
    });
}
