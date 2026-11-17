//! Building a row's version for attestation (logic.md 6.2 step 3, 6.2a item 2, 6.2b): `BuildFailed`'s reason for
//! each way a build fails, the retries before a failure is reported, and the compile limits that make a runaway
//! build the same error on every machine.

use pe_logic::build::{
    AUTHOR_MAX_BUILD, BuildError, CommitRefusal, FailReason, Toolchain, build_archive, build_for_commit, build_for_row,
    engine_sdk, pack,
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

#[test]
fn a_rustc_failure_is_compile_only_with_a_located_diagnostic() {
    let r = |out: &str| BuildError::Rustc(out.into()).reason();
    assert_eq!(
        r("src\\lib.rs:3:5: error[E0425]: cannot find value `x`\nerror: could not compile `script`\n"),
        FailReason::Compile
    );
    assert_eq!(r("C:\\w\\script\\src\\lib.rs:12:1: error: unknown attribute\n"), FailReason::Compile);
    assert_eq!(
        r("src/lib.rs:7:9: error: allow(long_running_const_eval) incompatible with previous forbid\n"),
        FailReason::Compile
    );
    assert_eq!(r("error: failed to write `S:\\t\\x`: The disk is full\n"), FailReason::Local);
    assert_eq!(r("warning: src/lib.rs:1:1: unused\n"), FailReason::Local);
    assert_eq!(BuildError::Refused("x".into()).reason(), FailReason::Refused);
    assert_eq!(BuildError::Spawn("x".into()).reason(), FailReason::Local);
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
