//! pe-app's build script: the build id's source part, and the commit for the console's start line (app.md 4, "The
//! build id"). It reads files and asks git. It runs no compiler and links nothing (docs/landing-gates.md, the
//! native-code audit).

#[path = "src/sources.rs"]
mod sources;

use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    let manifest = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").expect("cargo sets CARGO_MANIFEST_DIR"));
    let root = manifest.parent().and_then(Path::parent).expect("pe-app is crates/pe-app in its workspace");
    let hash = sources::hash(root).unwrap_or_else(|e| panic!("the build id's sources can't be read: {e}"));
    println!("cargo:rustc-env=PE_SOURCE_HASH={hash:032x}");
    for input in sources::INPUTS {
        println!("cargo:rerun-if-changed={}", root.join(input).display());
    }
    let (commit, dirty) = git(root);
    println!("cargo:rustc-env=PE_SOURCE_COMMIT={commit}");
    println!("cargo:rustc-env=PE_SOURCE_DIRTY={dirty}");
}

/// The commit HEAD names, and whether the hashed sources differ from it, for the console's start line only. Neither is
/// part of the build id, and without git both read "unknown". The script also runs again when HEAD moves (HEAD's file
/// and its branch's, where they exist), so the line follows a commit that changed no source.
///
/// "Differ" counts ignored files too, since the hash reads every file under `crates/` (a guest's `target/` built in
/// place, say). git runs without optional locks, so a build never takes the index lock from a landing in that checkout
/// (@design-reviewer's L1 and L2, m435432346cd4).
fn git(root: &Path) -> (String, &'static str) {
    let run = |args: &[&str]| -> Option<String> {
        let out = Command::new("git").arg("--no-optional-locks").arg("-C").arg(root).args(args).output().ok()?;
        out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
    };
    let Some(commit) = run(&["rev-parse", "HEAD"]) else {
        return ("unknown".into(), "unknown");
    };
    for r in [Some("HEAD".to_string()), run(&["symbolic-ref", "-q", "HEAD"])].into_iter().flatten() {
        if let Some(p) = run(&["rev-parse", "--git-path", &r]) {
            let p = PathBuf::from(p);
            let p = if p.is_absolute() { p } else { root.join(p) };
            if p.exists() {
                println!("cargo:rerun-if-changed={}", p.display());
            }
        }
    }
    let mut status = vec!["status", "--porcelain", "--ignored", "--"];
    status.extend(sources::INPUTS);
    let dirty = match run(&status) {
        Some(s) if s.is_empty() => "no",
        Some(_) => "yes",
        None => "unknown",
    };
    (commit, dirty)
}
