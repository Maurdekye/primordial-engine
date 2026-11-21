//! The build service's isolation from the machine it runs on (logic.md 6.2b). Under the build-asymmetry ruling
//! (m327e1d2d1222), a build that one member passes and another fails splits the session, so a module must depend
//! only on its archive and the build identity. Each test is a probe of `scripts/iter_bench_build_asym.py` or
//! `iter_bench_toolchain_min.py` made permanent (@fable-reviewer).

use pe_logic::build::{
    BuildError, Toolchain, build, build_archive, build_archive_reporting, check_dep_info, engine_sdk, pack,
    toolchain_env,
};
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

/// One test, since it changes the process's environment and working folder, which other tests would share. It
/// drives the raw compile, [`build`], on the unpacked folder: [`build_archive`] refuses this script outright for
/// asking for a variable (the dep-info check, below), and the cleared environment must hold without that check too.
#[test]
fn nothing_of_the_engines_environment_or_working_folder_reaches_the_module() {
    let root = scratch("leak");
    let archive = pack(&leaky_lamp(&root)).unwrap_or_else(|e| panic!("{e}"));
    let unpacked = root.join("w");
    assert!(matches!(
        build_archive(&tc(), &archive, &engine_sdk(), &unpacked, &root.join("t-archive"), true),
        Err(BuildError::Refused(_))
    ));
    let crate_dir = unpacked.join("script");
    let build = |tag: &str| {
        build(&tc(), &crate_dir, &engine_sdk(), &root.join(format!("t-{tag}")), true)
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

/// The lamp plus `extra` appended to its `src/lib.rs`, packed: one archive per way of reading outside it.
fn lamp_plus(root: &Path, tag: &str, extra: &str) -> Vec<u8> {
    let dir = root.join(tag);
    copy_tree(&lamp_dir(), &dir);
    let lib = dir.join("src/lib.rs");
    let src = std::fs::read_to_string(&lib).unwrap() + extra;
    std::fs::write(&lib, src).unwrap();
    pack(&dir).unwrap_or_else(|e| panic!("{tag}: {e}"))
}

#[test]
fn a_script_that_reads_outside_its_archive_is_refused_on_every_machine_alike() {
    let root = scratch("reads");
    let outside = root.join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("far.rs"), "pub fn f() -> u32 { 7 }\n").unwrap();
    std::fs::write(outside.join("data.bin"), b"X").unwrap();
    let fwd = |p: &Path| p.to_string_lossy().replace('\\', "/");
    let cases = [
        (
            "option_env",
            "\npub const P: usize = match option_env!(\"PE_UNSET_FOR_SURE\") { Some(s) => s.len(), None => 0 };\n"
                .to_string(),
        ),
        ("env", "\npub const D: &str = env!(\"CARGO_MANIFEST_DIR\");\n".to_string()),
        ("include_bytes", format!("\npub static B: &[u8] = include_bytes!(\"{}\");\n", fwd(&outside.join("data.bin")))),
        ("path_module", format!("\n#[path = \"{}\"]\nmod far;\n", fwd(&outside.join("far.rs")))),
    ];
    for (tag, extra) in cases {
        let archive = lamp_plus(&root, tag, &extra);
        match build_archive(
            &tc(),
            &archive,
            &engine_sdk(),
            &root.join(format!("w-{tag}")),
            &root.join(format!("t-{tag}")),
            true,
        ) {
            Err(BuildError::Refused(e)) => eprintln!("{tag}: refused: {e}"),
            other => panic!("{tag}: {other:?}"),
        }
    }
    // Reading a file inside src is allowed.
    let dir = root.join("inside");
    copy_tree(&lamp_dir(), &dir);
    std::fs::write(dir.join("src/table.rs"), "[1u8, 2, 3]").unwrap();
    let lib = dir.join("src/lib.rs");
    let src = std::fs::read_to_string(&lib).unwrap() + "\npub static T: [u8; 3] = include!(\"table.rs\");\n";
    std::fs::write(&lib, src).unwrap();
    let archive = pack(&dir).unwrap();
    build_archive(&tc(), &archive, &engine_sdk(), &root.join("w-inside"), &root.join("t-inside"), true)
        .unwrap_or_else(|e| panic!("inside: {e}"));
}

/// The parser on hand-written dep-info, so it runs on Windows too (@shader-reviewer's Medium on v8-r1): a newline in
/// a file name, which Linux and macOS allow, splits a rule over lines; and a path through `src/..` that does not
/// resolve must not read as inside `src`.
#[test]
fn the_dep_info_check_parses_strictly_and_refuses_what_does_not_resolve() {
    let root = scratch("dep-info");
    let krate = root.join("crate");
    std::fs::create_dir_all(krate.join("src")).unwrap();
    std::fs::write(krate.join("src/lib.rs"), "").unwrap();
    std::fs::write(krate.join("src/a b.rs"), "").unwrap();
    // As the service compares them: canonical, without Windows' `\\?\` prefix.
    let src = std::fs::canonicalize(krate.join("src")).unwrap().to_string_lossy().into_owned();
    let allowed = [PathBuf::from(src.strip_prefix(r"\\?\").unwrap_or(&src))];
    let ok = |t: &str| check_dep_info(t, &krate, &allowed);
    // What rustc writes: two target rules, then one rule per file read; a space is escaped.
    let normal = "/t/deps/script.d: src/lib.rs src/a\\ b.rs\n\n/t/deps/script.wasm: src/lib.rs src/a\\ b.rs\n\nsrc/lib.rs:\nsrc/a\\ b.rs:\n";
    ok(normal).unwrap_or_else(|e| panic!("{e}"));
    // A name with a newline, `/tmp/e<LF>src/lib.rs`, split over two lines: the fragment is refused.
    let split = "/t/deps/script.d: src/lib.rs /tmp/e\nsrc/lib.rs\n\nsrc/lib.rs:\n/tmp/e\nsrc/lib.rs:\n";
    assert!(matches!(ok(split), Err(BuildError::Refused(e)) if e.contains("unexpected line")));
    // A path through src/.. that does not resolve is refused, though it starts with src component-wise.
    let missing = "/t/deps/script.d: src/../../evil.rs\n\nsrc/lib.rs:\nsrc/../../evil.rs:\n";
    assert!(matches!(ok(missing), Err(BuildError::Refused(e)) if e.contains("does not resolve")));
    // An existing file outside src, and an env-dep, are refused.
    std::fs::write(root.join("outside.rs"), "").unwrap();
    let outside = format!("{}:\n", root.join("outside.rs").display());
    assert!(matches!(ok(&outside), Err(BuildError::Refused(e)) if e.contains("outside")));
    assert!(
        matches!(ok("# env-dep:HOME=/home/x\n"), Err(BuildError::Refused(e)) if e.contains("HOME") && !e.contains("/home/x"))
    );
}

#[test]
fn only_macos_gets_the_toolchain_s_library_folder_and_it_holds_llvm() {
    // rust-lld loads @rpath/libLLVM.dylib, which its rpath doesn't reach once the environment is cleared
    // (tier 2 run 44201283393, macos-arm64). Every wasm-building test on macOS is the end-to-end check.
    let tc = tc();
    let env = toolchain_env(&tc);
    if cfg!(target_os = "macos") {
        let (name, value) = env.expect("macOS sets the loader's fallback path");
        assert_eq!(name, "DYLD_FALLBACK_LIBRARY_PATH");
        let value = value.to_string_lossy().into_owned();
        // The toolchain's lib first, then dyld's defaults (without $HOME/lib: HOME is cleared).
        let (first, rest) = value.split_once(':').expect("a list");
        assert_eq!(rest, "/usr/local/lib:/lib:/usr/lib");
        let lib = PathBuf::from(first);
        assert_eq!(lib, tc.rustc.parent().unwrap().parent().unwrap().join("lib"));
        let llvm = std::fs::read_dir(&lib)
            .unwrap_or_else(|e| panic!("{}: {e}", lib.display()))
            .flatten()
            .any(|e| e.file_name().to_string_lossy().starts_with("libLLVM"));
        assert!(llvm, "no libLLVM in {}", lib.display());
    } else {
        assert_eq!(env, None);
    }
}

/// A target shared with builds against another pe-sdk copy (every deployed pe-app has its own; @api-eng
/// m285cfd7268d9) holds that copy's `pe_sdk-*.d`, which this build never read: the check passes over it. The pe-sdk
/// this build linked is still checked: an outside read added to its dep-info is refused.
#[test]
fn another_pe_sdk_copy_s_dep_info_in_the_target_is_not_this_build_s() {
    let root = scratch("sdk-copies");
    let archive = pack(&lamp_dir()).unwrap();
    let (work, target) = (root.join("w"), root.join("t"));
    let built = build_archive_reporting(&tc(), &archive, &engine_sdk(), &work, &target, true)
        .unwrap_or_else(|e| panic!("first: {e}"));
    let deps = target.join("wasm32-unknown-unknown/release/deps");
    assert!(!built.sdk_dep_info.is_empty(), "no pe-sdk dep-info reported");
    for d in &built.sdk_dep_info {
        let n = d.file_name().unwrap().to_string_lossy();
        assert!(n.starts_with("pe_sdk-") && n.ends_with(".d") && d.is_file(), "{}", d.display());
        assert_eq!(std::fs::canonicalize(d.parent().unwrap()).unwrap(), std::fs::canonicalize(&deps).unwrap());
    }
    // Another copy's, as a build against S:\m1run\pe-sdk left it.
    let other = root.join("other-sdk/src/lib.rs");
    std::fs::create_dir_all(other.parent().unwrap()).unwrap();
    std::fs::write(&other, "pub fn x() {}\n").unwrap();
    let o = other.display().to_string().replace(' ', r"\ ");
    let stale = deps.join("pe_sdk-00000000deadbeef.d");
    std::fs::write(&stale, format!("{}: {o}\n\n{o}:\n", deps.join("libpe_sdk-00000000deadbeef.rlib").display()))
        .unwrap();
    let again = build_archive_reporting(&tc(), &archive, &engine_sdk(), &work, &target, true)
        .unwrap_or_else(|e| panic!("with another copy's dep-info present: {e}"));
    assert_eq!(again.wasm, built.wasm);
    assert_eq!(again.sdk_dep_info, built.sdk_dep_info, "the fresh build reports the same pe-sdk unit");
    // This build's own pe-sdk dep-info, with an outside read: refused.
    let own = &built.sdk_dep_info[0];
    let kept = std::fs::read_to_string(own).unwrap();
    std::fs::write(own, format!("{kept}\n{o}:\n")).unwrap();
    let refused = build_archive_reporting(&tc(), &archive, &engine_sdk(), &work, &target, true);
    std::fs::write(own, kept).unwrap();
    match refused {
        Err(BuildError::Refused(e)) => assert!(e.contains("outside the script's src and pe-sdk's"), "{e}"),
        other => panic!("{other:?}"),
    }
}
