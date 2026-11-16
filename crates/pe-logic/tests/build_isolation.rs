//! The build service's isolation from the machine it runs on (logic.md 6.2b). Under the build-asymmetry ruling
//! (m327e1d2d1222), a build that one member passes and another fails splits the session, so a module must depend
//! only on its archive and the build identity. Each test is a probe of `scripts/iter_bench_build_asym.py` or
//! `iter_bench_toolchain_min.py` made permanent (@fable-reviewer).

use pe_logic::build::{BuildError, Toolchain, build_archive, engine_sdk, pack};
use std::path::{Path, PathBuf};

fn tc() -> Toolchain {
    Toolchain::current().unwrap_or_else(|e| panic!("{e}"))
}

fn lamp_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/scripts/lamp")
}

fn scratch(name: &str) -> PathBuf {
    let d = Path::new(env!("CARGO_TARGET_TMPDIR")).join("isolation").join(name);
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn copy_tree(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for e in std::fs::read_dir(from).unwrap().flatten() {
        let t = to.join(e.file_name());
        if e.file_type().unwrap().is_dir() {
            copy_tree(&e.path(), &t);
        } else {
            std::fs::copy(e.path(), t).unwrap();
        }
    }
}

/// The lamp, with its glow's intensity raised by the length of `PE_ISOLATION_PROBE` as rustc saw it when it compiled
/// the script: the module's bytes then show whether a variable reached the compiler.
fn leaky_lamp(root: &Path) -> PathBuf {
    let dir = root.join("leaky");
    copy_tree(&lamp_dir(), &dir);
    let lib = dir.join("src/lib.rs");
    let src = std::fs::read_to_string(&lib).unwrap();
    assert_eq!(src.matches("intensity: 1.0 }").count(), 1);
    let src = src.replace("intensity: 1.0 }", "intensity: 1.0 + PROBE }")
        + "\nconst PROBE: f32 = match option_env!(\"PE_ISOLATION_PROBE\") {\n    Some(s) => s.len() as f32,\n    None => 0.0,\n};\n";
    std::fs::write(&lib, src).unwrap();
    dir
}

/// One test, since it changes the process's environment and working folder, which other tests would share.
#[test]
fn nothing_of_the_engines_environment_or_working_folder_reaches_the_module() {
    let root = scratch("leak");
    let archive = pack(&leaky_lamp(&root)).unwrap_or_else(|e| panic!("{e}"));
    let build = |tag: &str| {
        build_archive(
            &tc(),
            &archive,
            &engine_sdk(),
            &root.join(format!("w-{tag}")),
            &root.join(format!("t-{tag}")),
            true,
        )
        .unwrap_or_else(|e| panic!("{tag}: {e}"))
    };
    let plain = build("plain");

    // A variable of the engine's process [V iter_bench_build_asym.py case b: it changed the hash].
    // SAFETY: no thread of this test binary reads the environment outside std (whose reads take its lock): the other
    // tests only call `var_os` and spawn cargo, both through std (@shader-reviewer's nit on v7-r1).
    unsafe { std::env::set_var("PE_ISOLATION_PROBE", "from-the-engine") };
    let with_var = build("var");
    unsafe { std::env::remove_var("PE_ISOLATION_PROBE") };
    assert!(with_var == plain, "a variable of the engine's environment reached the module");

    // A cargo configuration with `[env]` [V case c] and a rust-toolchain file naming another channel, in the
    // engine's working folder.
    let cwd = root.join("engine-cwd");
    std::fs::create_dir_all(cwd.join(".cargo")).unwrap();
    std::fs::write(cwd.join(".cargo/config.toml"), "[env]\nPE_ISOLATION_PROBE = \"from-a-config\"\n").unwrap();
    std::fs::write(cwd.join("rust-toolchain.toml"), "[toolchain]\nchannel = \"nightly\"\n").unwrap();
    let old = std::env::current_dir().unwrap();
    std::env::set_current_dir(&cwd).unwrap();
    let from_cwd = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| build("cwd")));
    std::env::set_current_dir(old).unwrap();
    assert!(from_cwd.unwrap() == plain, "the engine's working folder reached the module");
}

#[test]
fn a_cargo_configuration_above_the_work_folder_is_refused() {
    let root = scratch("above");
    std::fs::create_dir_all(root.join(".cargo")).unwrap();
    std::fs::write(root.join(".cargo/config.toml"), "[env]\nPE_ISOLATION_PROBE = \"above\"\n").unwrap();
    let archive = pack(&lamp_dir()).unwrap();
    match build_archive(&tc(), &archive, &engine_sdk(), &root.join("deep/w"), &root.join("t"), true) {
        Err(BuildError::Environment(e)) => assert!(e.contains("config.toml"), "{e}"),
        other => panic!("{other:?}"),
    }
    assert!(!root.join("deep/w").exists(), "refused before anything is written");
}

#[test]
fn a_rustup_proxy_folder_is_not_a_toolchain() {
    let exe = |n: &str| format!("{n}{}", std::env::consts::EXE_SUFFIX);
    let dir = scratch("proxy");
    for n in ["cargo", "rustc", "rustup"] {
        std::fs::write(dir.join(exe(n)), b"").unwrap();
    }
    assert!(matches!(Toolchain::in_bin(&dir), Err(BuildError::Toolchain(_))));
    std::fs::remove_file(dir.join(exe("rustup"))).unwrap();
    let tc = Toolchain::in_bin(&dir).unwrap_or_else(|e| panic!("{e}"));
    assert!(tc.cargo.is_absolute() && tc.rustc.is_absolute());
    assert_eq!(tc.rustc, std::path::absolute(&dir).unwrap().join(exe("rustc")));
    // The toolchain this test runs under is a real one, by absolute path, not rustup's proxy folder.
    let current = tc_current_folder();
    assert!(!current.join(exe("rustup")).exists(), "{}", current.display());
}

fn tc_current_folder() -> PathBuf {
    tc().cargo.parent().unwrap().to_path_buf()
}
