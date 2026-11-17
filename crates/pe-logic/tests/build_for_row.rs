//! Building a row's version for attestation (logic.md 6.2 step 3, 6.2a item 2, 6.2b): `BuildFailed`'s reason for
//! each way a build fails, the retries before a failure is reported, and the compile limits that make a runaway
//! build the same error on every machine.

use pe_logic::build::{
    AUTHOR_MAX_BUILD, BuildError, CommitRefusal, FailReason, RustcMessage, RustcSpan, Toolchain, build_archive,
    build_for_commit, build_for_row, engine_sdk, pack,
};
use pe_logic::runtime::{Script, Settings, engine};
use std::path::{Path, PathBuf};

fn tc() -> Toolchain {
    Toolchain::current().unwrap_or_else(|e| panic!("{e}"))
}

fn lamp_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/scripts/lamp")
}

fn scratch(name: &str) -> PathBuf {
    let d = Path::new(env!("CARGO_TARGET_TMPDIR")).join("for-row").join(name);
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

/// The lamp with `before` put ahead of its `src/lib.rs` and `after` appended, packed.
fn lamp_with(root: &Path, before: &str, after: &str) -> Vec<u8> {
    let dir = root.join("crate");
    copy_tree(&lamp_dir(), &dir);
    let lib = dir.join("src/lib.rs");
    let src = format!("{before}{}{after}", std::fs::read_to_string(&lib).unwrap());
    std::fs::write(&lib, src).unwrap();
    pack(&dir).unwrap_or_else(|e| panic!("{e}"))
}

fn author_module(archive: &[u8], root: &Path) -> pe_core::Hash {
    let s = Settings::default();
    let wasm = build_archive(&tc(), archive, &engine_sdk(), &root.join("author-w"), &root.join("author-t"), true)
        .unwrap_or_else(|e| panic!("{e}"));
    Script::load(&engine(&s).unwrap(), s, &wasm).unwrap().module_hash()
}

fn for_row(
    archive: &[u8],
    root: &Path,
    tc: &Toolchain,
    expected: pe_core::Hash,
) -> Result<Script, pe_logic::build::Failed> {
    let s = Settings::default();
    build_for_row(tc, archive, &engine_sdk(), &root.join("w"), &root.join("t"), true, &engine(&s).unwrap(), s, expected)
}

#[test]
fn a_rows_version_attests_on_the_first_build() {
    let root = scratch("attest");
    let archive = pack(&lamp_dir()).unwrap();
    let expected = author_module(&archive, &root);
    let script = for_row(&archive, &root, &tc(), expected).unwrap_or_else(|f| panic!("{f:?}"));
    assert_eq!(script.module_hash(), expected);
}

#[test]
fn a_mismatch_is_rebuilt_once_from_clean_then_reported_with_this_machines_hash() {
    let root = scratch("mismatch");
    let archive = pack(&lamp_dir()).unwrap();
    let actual = author_module(&archive, &root);
    let other = pe_core::Hash::of("pe.logic.module", b"a module nobody built");
    let f = for_row(&archive, &root, &tc(), other).err().expect("a mismatch");
    assert_eq!((f.reason, f.module, f.attempts), (FailReason::Mismatch, Some(actual), 2), "{}", f.detail);
}

#[test]
fn a_compile_error_is_reported_at_once() {
    let root = scratch("compile");
    let archive = lamp_with(&root, "", "\npub fn broken() -> u32 { no_such_name }\n");
    let f = for_row(&archive, &root, &tc(), pe_core::Hash::of("x", b"")).err().expect("a compile error");
    assert_eq!((f.reason, f.module, f.attempts), (FailReason::Compile, None, 1), "{}", f.detail);
}

#[test]
fn a_local_failure_is_retried_twice_before_it_is_reported() {
    let root = scratch("local");
    let archive = pack(&lamp_dir()).unwrap();
    let gone = Toolchain { cargo: root.join("no-such-cargo"), rustc: root.join("no-such-rustc") };
    let f = for_row(&archive, &root, &gone, pe_core::Hash::of("x", b"")).err().expect("a local failure");
    assert_eq!((f.reason, f.attempts), (FailReason::Local, 3), "{}", f.detail);
}

#[test]
fn allowing_rustcs_long_const_eval_lint_is_a_compile_error() {
    // Forbidden on the command line, so a const-eval loop is the same error everywhere, never a build that runs until
    // a slow machine gives up (logic.md 6.2b).
    let root = scratch("const-eval");
    let archive = lamp_with(&root, "", "\n#[allow(long_running_const_eval)]\npub const X: u32 = 1;\n");
    let f = for_row(&archive, &root, &tc(), pe_core::Hash::of("x", b"")).err().expect("refused by rustc");
    assert_eq!((f.reason, f.attempts), (FailReason::Compile, 1), "{}", f.detail);
}

#[test]
fn a_raised_compiler_limit_is_refused_before_the_build() {
    for (tag, attr) in
        [("recursion", "#![recursion_limit = \"4096\"]\n"), ("type_length", "#![type_length_limit = \"1000000\"]\n")]
    {
        let root = scratch(tag);
        let archive = lamp_with(&root, attr, "");
        match build_archive(&tc(), &archive, &engine_sdk(), &root.join("w"), &root.join("t"), true) {
            Err(BuildError::Refused(e)) => assert!(e.contains("limit"), "{tag}: {e}"),
            other => panic!("{tag}: {other:?}"),
        }
        assert!(!root.join("w/script").exists(), "{tag}: refused before anything is written");
    }
}

fn diag(level: &str, message: &str, at: Option<(&str, u32)>) -> RustcMessage {
    RustcMessage {
        level: level.into(),
        code: None,
        message: message.into(),
        spans: at.map(|(file, line)| RustcSpan { file: file.into(), line, column: 1 }).into_iter().collect(),
    }
}

#[test]
fn a_rustc_failure_is_compile_only_with_an_error_at_a_place_in_the_source() {
    // The crate rustc "ran in": tests/, so a name relative to it exists there and not in the working folder of the
    // tests themselves.
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests");
    let r = |d: Vec<RustcMessage>| {
        BuildError::Rustc { text: String::new(), diagnostics: d, crate_dir: dir.to_path_buf() }.reason()
    };
    let at = Some(("/script/src/lib.rs", 3));
    assert_eq!(
        r(vec![diag("error", "cannot find value `x`", at), diag("error", "aborting", None)]),
        FailReason::Compile
    );
    // No diagnostic (cargo failed first, a killed compiler), only a warning, or an error at no place: Local.
    assert_eq!(r(vec![]), FailReason::Local);
    assert_eq!(r(vec![diag("warning", "unused", at)]), FailReason::Local);
    assert_eq!(r(vec![diag("error", "could not write the output", None)]), FailReason::Local);
    // An internal compiler error is Local even beside a located error (@shader-reviewer).
    assert_eq!(
        r(vec![diag("error", "x", at), diag("error: internal compiler error", "unexpected panic", None)]),
        FailReason::Local
    );
    // A read rustc could not do: Local when the file exists (this machine's trouble), Refused when it does not (the
    // source names a file outside its archive).
    let exists = Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
    let exists = format!("couldn't read `{}`: Access is denied. (os error 5)", exists.display());
    assert_eq!(r(vec![diag("error", &exists, at)]), FailReason::Local);
    let gone = "couldn't read `Z:\\no\\such\\file.txt`: The system cannot find the path specified. (os error 3)";
    assert_eq!(r(vec![diag("error", gone, at)]), FailReason::Refused);
    // A relative name is resolved in the crate rustc ran in, not in the engine's working folder.
    // The names are built with this OS's separator: a backslash is no separator on Linux (@provenance-eng m828dbc2f31e8).
    let rel = Path::new("scripts").join("lamp").join("Cargo.toml");
    let rel = format!("couldn't read `{}`: The process cannot access the file. (os error 32)", rel.display());
    assert_eq!(r(vec![diag("error", &rel, at)]), FailReason::Local);
    let rel_gone = Path::new("src").join("no_such.rs");
    let rel_gone =
        format!("couldn't read `{}`: The system cannot find the file specified. (os error 2)", rel_gone.display());
    assert_eq!(r(vec![diag("error", &rel_gone, at)]), FailReason::Refused);
    assert_eq!(BuildError::Refused("x".into()).reason(), FailReason::Refused);
    assert_eq!(BuildError::Spawn("x".into()).reason(), FailReason::Local);
}

#[test]
fn rustcs_diagnostics_come_back_parsed_with_the_scripts_own_file_names() {
    let root = scratch("diagnostics");
    let archive = lamp_with(&root, "", "\npub fn broken() -> u32 { no_such_name }\n");
    let lines = std::fs::read_to_string(lamp_dir().join("src/lib.rs")).unwrap().lines().count() as u32;
    match build_archive(&tc(), &archive, &engine_sdk(), &root.join("w"), &root.join("t"), true) {
        Err(BuildError::Rustc { text, diagnostics, .. }) => {
            let e = diagnostics.iter().find(|d| d.level == "error").unwrap_or_else(|| panic!("{diagnostics:?}"));
            assert_eq!(e.code.as_deref(), Some("E0425"), "{e:?}");
            assert!(e.message.contains("no_such_name"), "{e:?}");
            assert_eq!(e.spans.len(), 1, "{e:?}");
            // The appended line: after the lamp's last line, one empty line, then the function.
            assert_eq!((e.spans[0].line, e.spans[0].column), (lines + 2, 26), "{e:?}");
            assert_eq!(e.spans[0].file, "src/lib.rs", "{e:?}");
            assert!(text.contains("E0425"), "{text}");
            let root_s = root.to_string_lossy().replace('\\', "/");
            for d in &diagnostics {
                for sp in &d.spans {
                    assert!(!sp.file.replace('\\', "/").contains(&root_s), "a local path: {d:?}");
                }
            }
        }
        other => panic!("{:?}", other.map(|w| w.len())),
    }
}

#[test]
fn reading_a_file_outside_the_archive_is_refused_on_every_machine() {
    // On a machine without the file, rustc's "couldn't read" is Refused, like the dep-info check's refusal on one
    // that has it (logic.md 6.2b), not Compile.
    let root = scratch("read-missing");
    let gone = root.join("no-such-file.txt").to_string_lossy().replace('\\', "/");
    let archive = lamp_with(&root, "", &format!("\npub const X: &str = include_str!(\"{gone}\");\n"));
    let f = for_row(&archive, &root, &tc(), pe_core::Hash::of("x", b"")).err().expect("a refusal");
    assert_eq!((f.reason, f.module, f.attempts), (FailReason::Refused, None, 1), "{}", f.detail);
}

#[test]
fn the_authors_commit_is_the_archive_and_the_module_every_attester_builds() {
    let root = scratch("commit");
    let s = Settings::default();
    let e = engine(&s).unwrap();
    let c = build_for_commit(
        &tc(),
        &lamp_dir(),
        &engine_sdk(),
        &root.join("aw"),
        &root.join("at"),
        true,
        &e,
        s,
        AUTHOR_MAX_BUILD,
    )
    .unwrap_or_else(|r| panic!("{r}"));
    assert_eq!(c.archive, pack(&lamp_dir()).unwrap());
    assert!(c.took <= AUTHOR_MAX_BUILD);
    // An attester's build of that archive, elsewhere, gives the author's module.
    let attested = for_row(&c.archive, &root, &tc(), c.script.module_hash()).unwrap_or_else(|f| panic!("{f:?}"));
    assert_eq!(attested.module_hash(), c.script.module_hash());
}

#[test]
fn a_build_slower_than_the_authors_margin_is_not_committed() {
    let root = scratch("slow");
    let s = Settings::default();
    let e = engine(&s).unwrap();
    let r = build_for_commit(
        &tc(),
        &lamp_dir(),
        &engine_sdk(),
        &root.join("w"),
        &root.join("t"),
        true,
        &e,
        s,
        std::time::Duration::ZERO,
    );
    let r = r.err().expect("too slow");
    assert!(matches!(r, CommitRefusal::TooSlow { .. }), "{r:?}");
    let text = r.to_string();
    assert!(text.contains("over the 0.0 s margin") && !text.contains("  "), "{text}");
}

#[test]
fn a_clean_rebuild_deletes_only_what_the_service_wrote() {
    // The target folder may be shared with other work: a mismatch's clean rebuild must leave the rest of it alone.
    let root = scratch("shared-target");
    let keep = root.join("t").join("someone-elses.txt");
    std::fs::create_dir_all(keep.parent().unwrap()).unwrap();
    std::fs::write(&keep, "keep me").unwrap();
    let archive = pack(&lamp_dir()).unwrap();
    let f = for_row(&archive, &root, &tc(), pe_core::Hash::of("x", b"not it")).err().expect("a mismatch");
    assert_eq!((f.reason, f.attempts), (FailReason::Mismatch, 2));
    assert_eq!(std::fs::read_to_string(&keep).unwrap(), "keep me");
}

/// A file of the script's own `src` that this machine cannot read while rustc runs (an antivirus scanner holding a
/// freshly unpacked file, here an exclusive handle) is this machine's trouble, `Local`, retried; never `Refused`,
/// which would split a member over a passing lock (@shader-reviewer's probe on v11-r1).
#[cfg(windows)]
#[test]
fn a_locked_file_inside_the_script_is_a_local_failure() {
    use std::os::windows::fs::OpenOptionsExt;
    let root = scratch("locked");
    let dir = root.join("crate");
    copy_tree(&lamp_dir(), &dir);
    std::fs::write(dir.join("src/table.rs"), "locked\n").unwrap();
    let lib = dir.join("src/lib.rs");
    let src = std::fs::read_to_string(&lib).unwrap();
    std::fs::write(&lib, format!("{src}\npub const TABLE: &str = include_str!(\"table.rs\");\n")).unwrap();
    let archive = pack(&dir).unwrap();
    // Unpacked and built once, so `w/script` is the service's; then rebuilt into a fresh target with the file held.
    build_archive(&tc(), &archive, &engine_sdk(), &root.join("w"), &root.join("t"), true)
        .unwrap_or_else(|e| panic!("{e}"));
    let held = root.join("w/script/src/table.rs");
    let _lock = std::fs::OpenOptions::new().read(true).share_mode(0).open(&held).unwrap();
    match pe_logic::build::build(&tc(), &root.join("w/script"), &engine_sdk(), &root.join("t2"), true) {
        Err(e @ BuildError::Rustc { .. }) => {
            assert_eq!(e.reason(), FailReason::Local, "{e}");
            assert!(e.to_string().contains("couldn't read"), "{e}");
        }
        other => panic!("{:?}", other.map(|w| w.len())),
    }
}

#[test]
fn the_authors_commit_carries_rustcs_warnings_about_the_script_only() {
    // For pe-api's `Built.warnings` (@api-eng): a passing build still tells the author what rustc warned about, with
    // the place in the archive; pe-sdk's warnings and rustc's summary line are not the author's.
    let root = scratch("warnings");
    let dir = root.join("crate");
    copy_tree(&lamp_dir(), &dir);
    let lib = dir.join("src/lib.rs");
    let src = std::fs::read_to_string(&lib).unwrap();
    let lines = src.lines().count() as u32;
    std::fs::write(&lib, format!("{src}\npub fn noisy() -> u32 {{\n    let unused_here = 7;\n    1\n}}\n")).unwrap();
    let s = Settings::default();
    let c = build_for_commit(
        &tc(),
        &dir,
        &engine_sdk(),
        &root.join("w"),
        &root.join("t"),
        true,
        &engine(&s).unwrap(),
        s,
        AUTHOR_MAX_BUILD,
    )
    .unwrap_or_else(|r| panic!("{r}"));
    assert_eq!(c.warnings.len(), 1, "{:?}", c.warnings);
    let w = &c.warnings[0];
    assert_eq!((w.level.as_str(), w.code.as_deref()), ("warning", Some("unused_variables")), "{w:?}");
    assert!(w.message.contains("unused_here"), "{w:?}");
    assert_eq!(w.spans.len(), 1, "{w:?}");
    assert_eq!((w.spans[0].file.as_str(), w.spans[0].line, w.spans[0].column), ("src/lib.rs", lines + 3, 9));
    // The plain lamp has none.
    let quiet = build_for_commit(
        &tc(),
        &lamp_dir(),
        &engine_sdk(),
        &root.join("w2"),
        &root.join("t2"),
        true,
        &engine(&s).unwrap(),
        s,
        AUTHOR_MAX_BUILD,
    )
    .unwrap_or_else(|r| panic!("{r}"));
    assert!(quiet.warnings.is_empty(), "{:?}", quiet.warnings);
}

#[test]
fn a_mismatch_saves_a_bug_bundle_with_what_finding_the_gap_needs() {
    // logic.md 6.2b: a Mismatch that survives the clean rebuild means prevention has a gap; the machine keeps the
    // archive, both hashes, B, the module, the dep-info and the toolchain's file hashes, locally.
    let root = scratch("bundle");
    let archive = pack(&lamp_dir()).unwrap();
    let other = pe_core::Hash::of("pe.logic.module", b"a module nobody built");
    let f = for_row(&archive, &root, &tc(), other).err().expect("a mismatch");
    assert!(f.wants_bundle(false), "a Mismatch is always bundled");
    let dir = pe_logic::build::save_bug_bundle(
        &tc(),
        &root.join("bugs"),
        &archive,
        other,
        &f,
        &engine_sdk(),
        &root.join("t"),
    )
    .unwrap_or_else(|e| panic!("{e}"));
    assert!(dir.starts_with(root.join("bugs")), "{}", dir.display());
    assert_eq!(std::fs::read(dir.join("source.pe-src")).unwrap(), archive);
    let report = std::fs::read_to_string(dir.join("report.txt")).unwrap();
    for want in [
        "reason: Mismatch".to_string(),
        format!("author_module: {other}"),
        format!("this machine's module: {}", f.module.unwrap()),
        format!("build identity B: {}", pe_logic::build::build_identity(&tc(), &engine_sdk()).unwrap()),
        "release:".to_string(),
        format!("os: {}", std::env::consts::OS),
        "cpu: ".to_string(),
        "this machine's absolute paths".to_string(),
    ] {
        assert!(report.contains(&want), "missing {want:?} in:\n{report}");
    }
    assert!(!report.contains("not gathered"), "{report}");
    // This machine's module, before metering: its metered hash is the one reported.
    let wasm = std::fs::read(dir.join("module.wasm")).unwrap();
    let s = Settings::default();
    assert_eq!(Script::load(&engine(&s).unwrap(), s, &wasm).unwrap().module_hash(), f.module.unwrap());
    assert!(dir.join("deps/script.d").is_file(), "the script's dep-info");
    let tool = std::fs::read_to_string(dir.join("toolchain-files.txt")).unwrap();
    assert!(tool.lines().any(|l| l.starts_with("lib/rustlib/wasm32-unknown-unknown/")), "the wasm32 std");
    assert!(tool.lines().any(|l| l.contains("rust-lld")), "the linker");
    assert!(tool.lines().all(|l| l.split(' ').count() == 3), "path size hash");
    assert!(tool.lines().any(|l| l.starts_with("bin/rustc")), "rustc");
    assert!(!tool.contains(".pdb") && !tool.contains("clippy") && !tool.contains("llvm-objdump"), "{tool}");
    let name = dir.file_name().unwrap().to_str().unwrap();
    assert!(name.is_ascii() && name.starts_with("bug-"), "{name}");
    let sdk = std::fs::read_to_string(dir.join("sdk-files.txt")).unwrap();
    assert!(sdk.lines().any(|l| l.starts_with("Cargo.toml ")) && sdk.lines().any(|l| l.starts_with("src/lib.rs ")));
    // A second bundle of the same failure gets its own folder.
    let again = pe_logic::build::save_bug_bundle(
        &tc(),
        &root.join("bugs"),
        &archive,
        other,
        &f,
        &engine_sdk(),
        &root.join("t"),
    )
    .unwrap();
    assert_ne!(again, dir);
}

#[test]
fn a_bug_bundle_is_for_a_gap_not_for_this_machines_own_trouble() {
    let f = |reason| pe_logic::build::Failed { reason, module: None, attempts: 1, detail: String::new() };
    assert!(f(FailReason::Compile).wants_bundle(true) && f(FailReason::Refused).wants_bundle(true));
    assert!(!f(FailReason::Compile).wants_bundle(false) && !f(FailReason::Refused).wants_bundle(false));
    for r in [FailReason::Local, FailReason::Cap] {
        assert!(!f(r).wants_bundle(true), "{r:?}");
    }
}
