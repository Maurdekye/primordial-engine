//! End to end on a real script (logic.md appendix A): the build service compiles `tests/scripts/lamp` (pe-sdk, no
//! build script, no procedural macro, `forbid(unsafe_code)`), the validator and the pass accept rustc's output, the
//! manifest decodes, and one batch through `pulse` writes the rows the script's rule gives.

use pe_logic::manifest::{self, Manifest};
use pe_logic::runtime::{self, Instance, NoServices, Script, Settings};
use pe_sdk::layout::{Access, BatchHeader, HEADER_BYTES, Stage};
use std::path::Path;

const BIG: u64 = 1 << 40;

fn big_stack(f: impl FnOnce() + Send + 'static) {
    let s = Settings::default();
    std::thread::Builder::new().stack_size(s.thread_stack()).spawn(f).unwrap().join().unwrap();
}

fn lamp() -> Vec<u8> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests").join("scripts").join("lamp");
    let target = Path::new(env!("CARGO_TARGET_TMPDIR")).join("scripts");
    pe_logic::build::build(&dir, &pe_logic::build::engine_sdk(), &target, true).unwrap_or_else(|e| panic!("{e}"))
}

fn manifest_of(i: &mut Instance) -> Manifest {
    let (packed, _) = i.call::<(), i64>("pe_manifest", (), BIG).unwrap();
    let (ptr, len) = ((packed as u64 & 0xffff_ffff) as usize, (packed as u64 >> 32) as usize);
    manifest::decode(&i.memory()[ptr..ptr + len]).unwrap()
}

fn align16(x: usize) -> usize {
    x.div_ceil(16) * 16
}

#[test]
fn the_lamp_builds_loads_and_runs() {
    big_stack(|| {
        let wasm = lamp();
        let s = Settings::default();
        let e = runtime::engine(&s).unwrap();
        let script = Script::load(&e, s, &wasm).unwrap_or_else(|e| panic!("{e}"));
        let mut i = script.instantiate(Box::new(NoServices)).unwrap();
        let m = manifest_of(&mut i);
        assert_eq!(m.abi, pe_logic::abi::ABI_VERSION as u32);
        assert_eq!(m.sdk, pe_sdk::layout::SDK_VERSION);
        assert_eq!(
            m.components.iter().map(|c| (c.name.as_str(), c.size)).collect::<Vec<_>>(),
            [("Blink", 12), ("Glow", 16)]
        );
        let pulse = &m.systems[0];
        assert_eq!((pulse.name.as_str(), pulse.stage), ("pulse", Stage::PrePhysics));
        assert_eq!(pulse.bound, [("Blink".to_string(), Access::Read), ("Glow".to_string(), Access::Write)]);

        // One batch of three lamps: periods 1 s, phases 0, 0.5 and 0.25; frame 15 at 60 Hz is t = 0.25 s.
        let n = 3usize;
        let ids = align16(HEADER_BYTES);
        let blink = align16(ids + 16 * n);
        let glow = align16(blink + 12 * n);
        let cmd = align16(glow + 16 * n);
        let mut header = BatchHeader {
            frame: 15,
            seed: 7,
            rate_hz: 60,
            n: n as u32,
            ids: ids as u32,
            cmd: cmd as u32,
            cmd_cap: 256,
            ..Default::default()
        };
        header.columns[0] = blink as u32;
        header.columns[1] = glow as u32;
        let base = m.batch_addr as usize;
        let mem = i.memory_mut();
        let hb = header.to_bytes();
        mem[base..base + HEADER_BYTES].copy_from_slice(&hb);
        for k in 0..n {
            mem[base + ids + 16 * k] = k as u8 + 1;
            let row: Vec<u8> = [1.0f32, [0.0, 0.5, 0.25][k], 2.0].iter().flat_map(|f| f.to_le_bytes()).collect();
            mem[base + blink + 12 * k..base + blink + 12 * (k + 1)].copy_from_slice(&row);
        }
        let used = i.run_system(0, 0, BIG).unwrap();
        assert!(used > 0);
        let mem = i.memory();
        let intensity = |k: usize| {
            f32::from_le_bytes(mem[base + glow + 16 * k + 12..base + glow + 16 * k + 16].try_into().unwrap())
        };
        // t + phase = 0.25, 0.75, 0.5: on while (t mod 1) < 0.5.
        assert_eq!([intensity(0), intensity(1), intensity(2)], [1.0, 0.0, 0.0]);
        // A second instance gives the same fuel: the module is deterministic.
        let mut j = script.instantiate(Box::new(NoServices)).unwrap();
        let _ = manifest_of(&mut j);
        let base_mem = i.memory()[base..base + cmd].to_vec();
        j.memory_mut()[base..base + cmd].copy_from_slice(&base_mem);
        let mut fresh_header = header;
        fresh_header.cmd_len = 0;
        j.memory_mut()[base..base + HEADER_BYTES].copy_from_slice(&fresh_header.to_bytes());
        assert_eq!(j.run_system(0, 0, BIG).unwrap(), used);
    });
}

#[test]
fn a_script_with_unsafe_code_is_refused_by_the_build() {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("unsafe-script");
    std::fs::create_dir_all(dir.join("src")).unwrap();
    let sdk = Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("pe-sdk");
    std::fs::write(
        dir.join("Cargo.toml"),
        format!(
            "[package]\nname = \"bad\"\nversion = \"0.0.0\"\nedition = \"2024\"\n[workspace]\n[lib]\ncrate-type = [\"cdylib\"]\n[dependencies]\npe-sdk = {{ path = {:?} }}\n",
            sdk.display().to_string().replace('\\', "/")
        ),
    )
    .unwrap();
    std::fs::write(dir.join("src/lib.rs"), "#![no_std]\npub fn f(p: *const u8) -> u8 { unsafe { *p } }\n").unwrap();
    let target = Path::new(env!("CARGO_TARGET_TMPDIR")).join("scripts");
    match pe_logic::build::build(&dir, &pe_logic::build::engine_sdk(), &target, true) {
        Err(pe_logic::build::BuildError::Rustc(msg)) => assert!(msg.contains("unsafe"), "{msg}"),
        other => panic!("expected a refusal, got {:?}", other.map(|b| b.len())),
    }
}

/// Copy a folder's files (not its `target`), recursively.
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

/// The engine's pe-sdk and the lamp, laid out as in the repository under `root`, with a workspace root for pe-sdk's
/// inherited keys.
fn lamp_under(root: &Path) -> std::path::PathBuf {
    let _ = std::fs::remove_dir_all(root);
    let here = Path::new(env!("CARGO_MANIFEST_DIR"));
    copy_tree(&here.join("../pe-sdk"), &root.join("crates/pe-sdk"));
    let lamp = root.join("crates/pe-logic/tests/scripts/lamp");
    copy_tree(&here.join("tests/scripts/lamp"), &lamp);
    std::fs::write(
        root.join("Cargo.toml"),
        "[workspace]\nmembers = [\"crates/pe-sdk\"]\nresolver = \"3\"\n[workspace.package]\nedition = \"2024\"\nlicense = \"MIT OR Apache-2.0\"\npublish = false\n",
    )
    .unwrap();
    lamp
}

#[test]
fn the_module_is_the_same_bytes_from_any_folder_and_from_a_warm_cache() {
    // Two copies of the engine's sources at different places and depths, each with its own target directory, and a
    // rebuild in the first after the script changed and changed back (shader-reviewer M3, L1; design-reviewer).
    let tmp = Path::new(env!("CARGO_TARGET_TMPDIR")).join("repro");
    let (ra, rb) = (tmp.join("a"), tmp.join("second copy").join("deeper"));
    let (a, b) = (lamp_under(&ra), lamp_under(&rb));
    let (sa, sb) = (ra.join("crates/pe-sdk"), rb.join("crates/pe-sdk"));
    let (ta, tb) = (tmp.join("target-a"), tmp.join("t").join("target b"));
    let _ = std::fs::remove_dir_all(&ta);
    let _ = std::fs::remove_dir_all(&tb);
    let first = pe_logic::build::build(&a, &sa, &ta, true).unwrap_or_else(|e| panic!("{e}"));
    let second = pe_logic::build::build(&b, &sb, &tb, true).unwrap_or_else(|e| panic!("{e}"));
    let lib = a.join("src/lib.rs");
    let src = std::fs::read_to_string(&lib).unwrap();
    std::fs::write(&lib, format!("{src}\npub fn __changed() -> u32 {{ 7 }}\n")).unwrap();
    pe_logic::build::build(&a, &sa, &ta, true).unwrap_or_else(|e| panic!("{e}"));
    std::fs::write(&lib, &src).unwrap();
    let warm = pe_logic::build::build(&a, &sa, &ta, true).unwrap_or_else(|e| panic!("{e}"));
    for (name, m) in [("another folder", &second), ("a warm rebuild", &warm)] {
        assert!(m == &first, "{name}: {} vs {} bytes", m.len(), first.len());
    }
    // No absolute path of either copy is left in the bytes.
    let text = String::from_utf8_lossy(&first);
    for needle in ["second copy", "target-a", "repro", "pe-logic"] {
        assert!(!text.contains(needle), "{needle:?} is in the module");
    }
    // The hash every machine compares (6.2 step 3) is the same for both.
    let s = Settings::default();
    let e = runtime::engine(&s).unwrap();
    let ha = Script::load(&e, s, &first).unwrap().module_hash();
    let hb = Script::load(&e, s, &second).unwrap().module_hash();
    assert_eq!(ha, hb);
}

#[test]
fn a_script_crate_outside_the_rules_is_refused_before_it_builds() {
    use pe_logic::build::{BuildError, check_manifest};
    let ok = "[package]\nname = \"x\"\nversion = \"0.0.0\"\nedition = \"2024\"\n[workspace]\n[lib]\ncrate-type = [\"cdylib\"]\npath = \"src/lib.rs\"\n[dependencies]\npe-sdk = { path = \"../pe-sdk\" }\n";
    assert_eq!(check_manifest(ok).unwrap(), "../pe-sdk");
    for (bad, why) in [
        ("build = \"build.rs\"", "a build script"),
        ("[build-dependencies]", "build dependencies"),
        ("[dependencies.pe-sdk]", "a dotted table"),
        ("[profile.release]", "a profile"),
        ("[patch.crates-io]", "a patch"),
        ("[target.wasm32-unknown-unknown.dependencies]", "target dependencies"),
        ("serde = \"1\"", "another dependency"),
        ("dependencies.serde = \"1\"", "a dotted key"),
    ] {
        let text = if bad.starts_with('[') {
            format!("{ok}{bad}\n")
        } else if bad.starts_with("build") {
            ok.replace("[package]\n", &format!("[package]\n{bad}\n"))
        } else {
            format!("{ok}{bad}\n")
        };
        assert!(matches!(check_manifest(&text), Err(BuildError::Manifest(_))), "{why} accepted");
    }
    let sdk_forms =
        ["pe-sdk = \"0.1\"", "pe-sdk = { path = \"../pe-sdk\", features = [\"x\"] }", "pe-sdk = { git = \"x\" }"];
    for form in sdk_forms {
        let text = ok.replace("pe-sdk = { path = \"../pe-sdk\" }", form);
        assert!(matches!(check_manifest(&text), Err(BuildError::Manifest(_))), "{form} accepted");
    }
    // A build.rs file next to an allowed manifest.
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("buildrs-script");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("Cargo.toml"), ok).unwrap();
    std::fs::write(dir.join("build.rs"), "fn main() {}\n").unwrap();
    std::fs::write(dir.join("src/lib.rs"), "#![no_std]\n").unwrap();
    let target = Path::new(env!("CARGO_TARGET_TMPDIR")).join("scripts");
    assert!(matches!(
        pe_logic::build::build(&dir, &pe_logic::build::engine_sdk(), &target, true),
        Err(BuildError::Manifest(_))
    ));
}

#[test]
fn a_script_may_not_choose_its_own_pe_sdk() {
    // A crate named pe-sdk elsewhere, which could carry a build script: refused before cargo runs (shader-reviewer,
    // round 2). The same script against the engine's pe-sdk is accepted by the check.
    let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("fake-sdk");
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("pe-sdk/src")).unwrap();
    std::fs::write(
        root.join("pe-sdk/Cargo.toml"),
        "[package]\nname = \"pe-sdk\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    )
    .unwrap();
    std::fs::write(root.join("pe-sdk/build.rs"), "fn main() {}\n").unwrap();
    std::fs::write(root.join("pe-sdk/src/lib.rs"), "#![no_std]\n").unwrap();
    std::fs::create_dir_all(root.join("script/src")).unwrap();
    std::fs::write(
        root.join("script/Cargo.toml"),
        "[package]\nname = \"s\"\nversion = \"0.0.0\"\nedition = \"2024\"\n[workspace]\n[lib]\ncrate-type = [\"cdylib\"]\n[dependencies]\npe-sdk = { path = \"../pe-sdk\" }\n",
    )
    .unwrap();
    std::fs::write(root.join("script/src/lib.rs"), "#![no_std]\n").unwrap();
    let target = Path::new(env!("CARGO_TARGET_TMPDIR")).join("scripts");
    match pe_logic::build::build(&root.join("script"), &pe_logic::build::engine_sdk(), &target, true) {
        Err(pe_logic::build::BuildError::Manifest(m)) => assert!(m.contains("engine's pe-sdk"), "{m}"),
        other => panic!("expected a refusal, got {:?}", other.map(|b| b.len())),
    }
}
