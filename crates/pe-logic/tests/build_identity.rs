//! The build identity B (logic.md 6.2b): what, besides the archive, decides a module's bytes. pe-app's build id
//! carries it, so two machines that would build a version differently never share a session.

use pe_logic::build::{Toolchain, build_identity, engine_sdk, toolchain_release};
use std::path::{Path, PathBuf};

fn tc() -> Toolchain {
    Toolchain::current().unwrap_or_else(|e| panic!("{e}"))
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

#[test]
fn the_build_identity_is_stable_and_moves_with_pe_sdk() {
    let a = build_identity(&tc(), &engine_sdk()).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(build_identity(&tc(), &engine_sdk()).unwrap(), a, "the same inputs, the same B");
    // A copy of pe-sdk elsewhere is the same pe-sdk: B depends on its files, not on where it lies.
    let root: PathBuf = Path::new(env!("CARGO_TARGET_TMPDIR")).join("identity");
    let _ = std::fs::remove_dir_all(&root);
    let copy = root.join("deeper/pe-sdk");
    copy_tree(&engine_sdk(), &copy);
    assert_eq!(build_identity(&tc(), &copy).unwrap(), a, "a copy elsewhere");
    // Any change to pe-sdk's sources moves B, a comment included.
    let lib = copy.join("src/lib.rs");
    let src = std::fs::read_to_string(&lib).unwrap();
    std::fs::write(&lib, format!("{src}\n// changed\n")).unwrap();
    assert_ne!(build_identity(&tc(), &copy).unwrap(), a, "pe-sdk changed");
}

#[test]
fn the_toolchain_release_names_the_release_and_not_the_host() {
    let r = toolchain_release(&tc()).unwrap_or_else(|e| panic!("{e}"));
    let lines: Vec<&str> = r.lines().collect();
    assert_eq!(lines.len(), 3, "{r}");
    assert!(lines[0].starts_with("release:"), "{r}");
    assert!(lines[1].starts_with("commit-hash:"), "{r}");
    assert!(lines[2].starts_with("LLVM version:"), "{r}");
    assert!(!r.contains("host:"), "{r}");
}

#[test]
fn a_copy_of_pe_sdk_elsewhere_builds_the_same_module() {
    // pe-sdk inherits nothing from the engine's workspace (@shader-reviewer's Low on v10-r1): a copy with no workspace
    // above it builds, and builds the same bytes, as B says.
    let root: PathBuf = Path::new(env!("CARGO_TARGET_TMPDIR")).join("identity-build");
    let _ = std::fs::remove_dir_all(&root);
    let copy = root.join("elsewhere/pe-sdk");
    copy_tree(&engine_sdk(), &copy);
    let lamp = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/scripts/lamp");
    let archive = pe_logic::build::pack(&lamp).unwrap();
    let build = |sdk: &Path, tag: &str| {
        pe_logic::build::build_archive(&tc(), &archive, sdk, &root.join(tag).join("w"), &root.join(tag).join("t"), true)
            .unwrap_or_else(|e| panic!("{tag}: {e}"))
    };
    assert_eq!(build(&copy, "copy"), build(&engine_sdk(), "engine"), "the same module from either pe-sdk");
    assert_eq!(build_identity(&tc(), &copy).unwrap(), build_identity(&tc(), &engine_sdk()).unwrap());
}

#[test]
fn a_pe_sdk_that_inherits_from_a_workspace_is_refused() {
    let root: PathBuf = Path::new(env!("CARGO_TARGET_TMPDIR")).join("identity-ws");
    let _ = std::fs::remove_dir_all(&root);
    let copy = root.join("pe-sdk");
    copy_tree(&engine_sdk(), &copy);
    let manifest = copy.join("Cargo.toml");
    let text = std::fs::read_to_string(&manifest).unwrap();
    for (tag, edited) in [
        ("edition", text.replace("edition = \"2024\"", "edition.workspace = true")),
        ("lints", format!("{text}\n[lints]\nworkspace = true\n")),
    ] {
        assert_ne!(edited, text, "{tag}: the edit applies");
        std::fs::write(&manifest, &edited).unwrap();
        match build_identity(&tc(), &copy) {
            Err(pe_logic::build::BuildError::Toolchain(e)) => assert!(e.contains("workspace"), "{tag}: {e}"),
            other => panic!("{tag}: {other:?}"),
        }
    }
    // A comment may say the word.
    std::fs::write(&manifest, format!("# not from the workspace\n{text}")).unwrap();
    build_identity(&tc(), &copy).unwrap_or_else(|e| panic!("{e}"));
}
