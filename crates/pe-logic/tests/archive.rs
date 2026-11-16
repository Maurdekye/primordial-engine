//! The build service's source archive (logic.md 4.1a): a script crate packs into pe-world's canonical archive, and an
//! archive builds from a fresh folder with the service's own `Cargo.toml`, the same module wherever it is unpacked.

use pe_logic::build::{BuildError, build_archive, engine_sdk, pack};
use pe_world::script::{SourceError, check_source, source_files};
use std::path::{Path, PathBuf};

fn lamp_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/scripts/lamp")
}

fn scratch(name: &str) -> PathBuf {
    let d = Path::new(env!("CARGO_TARGET_TMPDIR")).join("archive").join(name);
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

#[test]
fn the_lamp_packs_canonically_and_builds_the_same_module_from_any_folder() {
    let archive = pack(&lamp_dir()).unwrap_or_else(|e| panic!("{e}"));
    assert!(check_source(&archive).is_ok());
    assert_eq!(pack(&lamp_dir()).unwrap(), archive, "packing is canonical");
    let files = source_files(&archive).unwrap();
    assert!(files.iter().all(|(p, _)| p.starts_with("src/") && p.ends_with(".rs")));
    assert!(files.iter().any(|(p, _)| *p == "src/lib.rs"));

    let root = scratch("same");
    let a = build_archive(
        &pe_logic::build::Toolchain::current().unwrap(),
        &archive,
        &engine_sdk(),
        &root.join("a"),
        &root.join("ta"),
        true,
    )
    .unwrap_or_else(|e| panic!("{e}"));
    let b = build_archive(
        &pe_logic::build::Toolchain::current().unwrap(),
        &archive,
        &engine_sdk(),
        &root.join("deeper/b/c"),
        &root.join("tb"),
        true,
    )
    .unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(a, b, "one archive, one module, wherever it is unpacked");
    assert_eq!(&a[..4], b"\0asm");
    // The service's manifest, not the author's: the author's `lamp` package name is gone.
    let toml = std::fs::read_to_string(root.join("a/script/Cargo.toml")).unwrap();
    assert!(toml.contains("name = \"script\"") && !toml.contains("lamp"));
    // A rebuild into the same work folder replaces what the last one wrote.
    let again = build_archive(
        &pe_logic::build::Toolchain::current().unwrap(),
        &archive,
        &engine_sdk(),
        &root.join("a"),
        &root.join("ta"),
        true,
    )
    .unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(again, a);
}

#[test]
fn pack_refuses_a_file_the_archive_cannot_carry() {
    for (name, file) in [("txt", "src/data.txt"), ("upper", "src/Big.rs"), ("hyphen", "src/a-b.rs")] {
        let dir = scratch(&format!("pack-{name}"));
        copy_tree(&lamp_dir(), &dir);
        std::fs::write(dir.join(file), "pub fn f() {}\n").unwrap();
        match pack(&dir) {
            Err(BuildError::Source(SourceError::Path(p))) => assert!(p.ends_with(&file[4..]), "{name}: {p}"),
            other => panic!("{name}: {other:?}"),
        }
    }
    let dir = scratch("pack-utf8");
    copy_tree(&lamp_dir(), &dir);
    std::fs::write(dir.join("src/bad.rs"), [0xff, 0xfe]).unwrap();
    assert!(matches!(pack(&dir), Err(BuildError::Source(SourceError::NotUtf8(_)))));
}

#[test]
fn build_archive_refuses_a_non_canonical_archive_before_writing_anything() {
    let root = scratch("refuse");
    let mut archive = pack(&lamp_dir()).unwrap();
    archive[0] ^= 1;
    match build_archive(
        &pe_logic::build::Toolchain::current().unwrap(),
        &archive,
        &engine_sdk(),
        &root.join("w"),
        &root.join("t"),
        true,
    ) {
        Err(BuildError::Source(SourceError::Magic)) => {}
        other => panic!("{other:?}"),
    }
    assert!(!root.join("w").exists(), "nothing unpacked");
}

#[test]
fn build_archive_will_not_delete_a_folder_it_did_not_write() {
    let root = scratch("foreign");
    let dir = root.join("w/script");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("Cargo.toml"), "[package]\nname = \"mine\"\n").unwrap();
    let archive = pack(&lamp_dir()).unwrap();
    assert!(matches!(
        build_archive(
            &pe_logic::build::Toolchain::current().unwrap(),
            &archive,
            &engine_sdk(),
            &root.join("w"),
            &root.join("t"),
            true
        ),
        Err(BuildError::Io(_))
    ));
    assert_eq!(std::fs::read_to_string(dir.join("Cargo.toml")).unwrap(), "[package]\nname = \"mine\"\n");
}

#[test]
fn pack_refuses_a_link_under_src() {
    // A file symlink where the machine allows one; otherwise (Windows without the privilege) a directory junction,
    // which needs none. Either is neither a plain file nor a folder to `pack`.
    let dir = scratch("pack-link");
    copy_tree(&lamp_dir(), &dir);
    let outside = dir.join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(
        outside.join("far.rs"),
        "pub fn f() {}
",
    )
    .unwrap();
    #[cfg(windows)]
    let made = std::os::windows::fs::symlink_file(outside.join("far.rs"), dir.join("src/linked.rs")).is_ok() || {
        let st = std::process::Command::new("cmd")
            .args(["/c", "mklink", "/J"])
            // mklink takes backslashes only, and the target folder may be given with forward ones.
            .arg(dir.join("src").join("linked").to_string_lossy().replace('/', "\\"))
            .arg(outside.to_string_lossy().replace('/', "\\"))
            .output();
        st.is_ok_and(|o| o.status.success())
    };
    #[cfg(unix)]
    let made = std::os::unix::fs::symlink(outside.join("far.rs"), dir.join("src/linked.rs")).is_ok();
    assert!(made, "could make neither a symlink nor a junction");
    match pack(&dir) {
        Err(BuildError::Source(SourceError::Path(p))) => assert!(p.contains("linked"), "{p}"),
        other => panic!("{other:?}"),
    }
}

/// The author's module is `build_archive(pack(dir))`, the archive it commits (logic.md 4.2; @shader-reviewer's
/// Low 2 on v6-r1): other machines build the archive with the template's package name, never the author's crate.
/// This pins how a direct `build(dir)` of the author's own crate relates to it, so a change shows up.
#[test]
fn a_direct_build_of_the_authors_crate_differs_only_as_pinned_here() {
    let root = scratch("author");
    let direct = pe_logic::build::build(
        &pe_logic::build::Toolchain::current().unwrap(),
        &lamp_dir(),
        &engine_sdk(),
        &root.join("td"),
        true,
    )
    .unwrap_or_else(|e| panic!("{e}"));
    let archive = pack(&lamp_dir()).unwrap();
    let via = build_archive(
        &pe_logic::build::Toolchain::current().unwrap(),
        &archive,
        &engine_sdk(),
        &root.join("w"),
        &root.join("ta"),
        true,
    )
    .unwrap_or_else(|e| panic!("{e}"));
    eprintln!("direct {} bytes, archive {} bytes, equal: {}", direct.len(), via.len(), direct == via);
    assert_eq!(direct == via, DIRECT_EQUALS_ARCHIVE);
}

/// Measured on v6-r2: equal (the lamp, 9,473 bytes either way); the package name does not reach the stripped module.
/// The rule stands regardless (4.2): if this ever flips, an author's tooling must not have used `build(dir)`.
const DIRECT_EQUALS_ARCHIVE: bool = true;
