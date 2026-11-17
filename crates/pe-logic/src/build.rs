//! The build service, v0 (docs/spec/logic.md 4.2): compile one script crate to wasm32 with flags the service fixes.
//!
//! - `cargo rustc --release --target wasm32-unknown-unknown`, the release profile overridden to the user's
//!   `rel-fast` (opt-level 3, no LTO, 16 codegen units, `panic = "abort"`, no debug information, symbols stripped)
//!   **except incremental, which is off**: a warm cache and a clean build are not promised the same bytes, and fuel
//!   follows the bytes (design-reviewer m1f57766b6f55, shader-reviewer L1). Every machine's build is the canonical
//!   one, the author's included, since its hash is the one the others compare (6.2 step 3);
//! - `-F unsafe_code` on the script crate only (its dependency pe-sdk keeps its own `unsafe`), 4.1;
//! - a 64 KiB stack (`-zstack-size=65536`), and `--remap-path-prefix` for the script's folder, pe-sdk's folder, the
//!   target directory and Cargo's home, so the bytes do not depend on where the machine keeps them (shader-reviewer
//!   M3: pe-sdk's panic locations survive `strip`);
//! - the script's `Cargo.toml` is checked first against 4.1 by a strict allowlist ([`check_manifest`]): no build
//!   script, no dependency but pe-sdk, no profile, patch or target sections; and its pe-sdk must be the engine's,
//!   the folder the caller names, so a script cannot have another crate built in pe-sdk's place, with a build
//!   script or a procedural macro of its own (shader-reviewer, round 2);
//! - every other output-affecting setting pinned: the CPU and the feature list of 5.3, debug assertions and overflow
//!   checks off, `CARGO_INCREMENTAL=0`, no rustc wrapper (coordinator mcda7c814aebf);
//! - its own target directory, given by the caller (one per world);
//! - **isolated from the machine** (logic.md 6.2b, under the build-asymmetry ruling m327e1d2d1222: a build one
//!   member passes and another fails splits the session): the pinned toolchain's own cargo and rustc by absolute
//!   path ([`Toolchain`]), never a rustup proxy, which picks the compiler from a `rust-toolchain` file around the
//!   working folder; a cleared environment with a short fixed list ([`KEEP_ENV`]); the crate itself as the working
//!   folder; and a `CARGO_HOME` of the service's own, inside the target directory. [`build_archive`], the path every
//!   module takes (4.2), also refuses a work folder that sits under a cargo configuration, and after the build
//!   refuses a module whose compile read a file outside its archive and pe-sdk, or any environment variable
//!   ([`check_reads`], from rustc's dep-info).
//!
//! [`build_for_row`] is what a voter attests with: the archive built, metered and compared with the row's
//! `author_module`, retried as 6.2b says before a failure is reported, with [`FailReason`] for `BuildFailed`.
//!
//! Not yet in v0: the pinned toolchain's hash and fetching it (D75), idle priority for the child, one build at a
//! time per machine, the memory cap and the pinned job count of 6.2a item 5, and rustc's JSON
//! diagnostics for `code_check` (the messages are returned as text).

use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug)]
pub enum BuildError {
    /// cargo could not be started.
    Spawn(String),
    /// The compiler refused the script: its output.
    Rustc(String),
    /// The build succeeded but its module was not where expected.
    NoModule(PathBuf),
    /// The script's manifest has no `[package] name`.
    NoName,
    /// The script crate breaks a rule of 4.1: the line, and why.
    Manifest(String),
    /// The source archive is not canonical, or a crate holds a file under `src` it cannot carry (4.1a).
    Source(pe_world::script::SourceError),
    /// A file of the crate or of the work folder could not be read or written.
    Io(String),
    /// The toolchain is not one the service runs: not found, or a rustup proxy (logic.md 6.2b).
    Toolchain(String),
    /// The machine around the build could change it: a cargo configuration above the work folder (6.2b).
    Environment(String),
    /// The compile read something outside the archive and pe-sdk: a file, or an environment variable (6.2b's
    /// dep-info check). `BuildFailed`'s reason `Refused`, the same on every machine. The text is for this machine's
    /// console and log only, never for an entry or anything replicated: it can name a local path (6.2a item 2;
    /// @shader-reviewer on v8-r1).
    Refused(String),
}

impl std::fmt::Display for BuildError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BuildError::Spawn(e) => write!(f, "could not run cargo: {e}"),
            BuildError::Rustc(e) => write!(f, "the script did not compile:\n{e}"),
            BuildError::NoModule(p) => write!(f, "no module at {}", p.display()),
            BuildError::NoName => f.write_str("the script's Cargo.toml has no package name"),
            BuildError::Manifest(e) => write!(f, "the script's crate is refused (logic.md 4.1): {e}"),
            BuildError::Source(e) => write!(f, "the script's source is refused (logic.md 4.1a): {e}"),
            BuildError::Io(e) => write!(f, "the build service's files: {e}"),
            BuildError::Toolchain(e) => write!(f, "the build service's toolchain (logic.md 6.2b): {e}"),
            BuildError::Environment(e) => write!(f, "the build service's folder (logic.md 6.2b): {e}"),
            BuildError::Refused(e) => write!(f, "the script reads outside its source (logic.md 6.2b): {e}"),
        }
    }
}

impl std::error::Error for BuildError {}

/// The pinned toolchain the service runs, by absolute path (logic.md 6.2b): its own `cargo` and `rustc`. Never a
/// rustup proxy: a proxy picks the compiler from a `rust-toolchain` file in or above the working folder, or from
/// `RUSTUP_TOOLCHAIN`, so the same command can build with another compiler and give other bytes, without an error
/// [V `scripts/iter_bench_toolchain_min.py`: through the proxy outside the repo, nightly 1.94 and other hashes].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Toolchain {
    pub cargo: PathBuf,
    pub rustc: PathBuf,
}

impl Toolchain {
    /// The toolchain whose executables are in `bin` (D75's installed copy, or a rustup toolchain's own `bin`).
    /// Refuses a folder holding `rustup`, which is where rustup's proxies live.
    pub fn in_bin(bin: &Path) -> Result<Toolchain, BuildError> {
        let bin = std::path::absolute(bin).map_err(|e| BuildError::Toolchain(e.to_string()))?;
        let exe = |n: &str| bin.join(format!("{n}{}", std::env::consts::EXE_SUFFIX));
        if exe("rustup").exists() {
            return Err(BuildError::Toolchain(format!("{} holds rustup's proxies, not a toolchain", bin.display())));
        }
        let tc = Toolchain { cargo: exe("cargo"), rustc: exe("rustc") };
        for f in [&tc.cargo, &tc.rustc] {
            if !f.is_file() {
                return Err(BuildError::Toolchain(format!("no {}", f.display())));
            }
        }
        Ok(tc)
    }

    /// The toolchain of the cargo that runs this process: `CARGO`, which cargo sets to its own executable, the
    /// toolchain's real one even when cargo was started through a rustup proxy. For tests and the dev engine,
    /// until D75 installs the pinned toolchain.
    pub fn current() -> Result<Toolchain, BuildError> {
        let cargo = std::env::var_os("CARGO").ok_or(BuildError::Toolchain("CARGO is not set".into()))?;
        let bin = Path::new(&cargo).parent().ok_or(BuildError::Toolchain("CARGO has no folder".into()))?;
        Toolchain::in_bin(bin)
    }
}

/// The only variables of the engine's environment the build's child keeps (logic.md 6.2b): the OS's own, which
/// processes need to start and to make temporary files. Everything else, `RUSTFLAGS`, `RUSTUP_TOOLCHAIN`,
/// `CARGO_*`, wrappers and anything a script could read with `option_env!`, is cleared [V `iter_bench_build_asym.py`:
/// one inherited variable changed the module's hash].
pub const KEEP_ENV: &[&str] = &["SYSTEMROOT", "SYSTEMDRIVE", "WINDIR", "TEMP", "TMP", "TMPDIR"];

/// The build's job count, the same on every machine (logic.md 6.2b) [I: 4 builds about as fast as 16 here].
pub const BUILD_JOBS: u32 = 4;

/// The `--config` overrides that make `--release` the `rel-fast` profile.
const PROFILE: &[&str] = &[
    "profile.release.opt-level=3",
    "profile.release.lto=false",
    "profile.release.codegen-units=16",
    "profile.release.incremental=false",
    "profile.release.panic=\"abort\"",
    "profile.release.debug=false",
    "profile.release.strip=\"symbols\"",
    // Release defaults today, pinned so that nothing output-affecting is left to a default (coordinator mcda7c814aebf).
    "profile.release.debug-assertions=false",
    "profile.release.overflow-checks=false",
    "profile.release.rpath=false",
    // The linker too: a machine's own cargo configuration may name another (shader-reviewer, round 2).
    "target.wasm32-unknown-unknown.linker=\"rust-lld\"",
];

/// The code generation flags the service fixes besides the profile: the CPU and exactly the wasm features of 5.3, so
/// a toolchain whose default feature set moves cannot change the bytes silently.
const CODEGEN: &[&str] = &[
    "-C",
    "target-cpu=mvp",
    "-C",
    "target-feature=+bulk-memory,+multivalue,+mutable-globals,+nontrapping-fptoint,+reference-types,+sign-ext",
    "-C",
    "link-arg=-zstack-size=65536",
];

fn package_name(manifest: &str) -> Option<String> {
    let mut in_package = false;
    for line in manifest.lines() {
        let l = line.trim();
        if l.starts_with('[') {
            in_package = l == "[package]";
        } else if in_package && let Some(v) = l.strip_prefix("name") {
            let v = v.trim_start().strip_prefix('=')?.trim().trim_matches('"');
            return Some(v.to_string());
        }
    }
    None
}

/// What a script's `Cargo.toml` may hold (4.1), line by line: a section header, or `key = value` under it. Anything
/// else, including dotted keys, `[dependencies.x]` tables and keys this list does not name, is refused rather than
/// interpreted. Returns pe-sdk's `path`.
pub fn check_manifest(text: &str) -> Result<String, BuildError> {
    const KEYS: &[(&str, &[&str])] = &[
        ("[package]", &["name", "version", "edition", "publish", "license", "description"]),
        ("[lib]", &["crate-type", "path"]),
        ("[dependencies]", &["pe-sdk"]),
        ("[workspace]", &[]),
    ];
    let bad = |n: usize, why: &str| Err(BuildError::Manifest(format!("line {}: {why}", n + 1)));
    let mut section: Option<&[&str]> = None;
    let mut seen = Vec::new();
    let mut sdk = None;
    for (n, line) in text.lines().enumerate() {
        let l = line.trim();
        if l.is_empty() || l.starts_with('#') {
            continue;
        }
        if l.starts_with('[') {
            let Some(&(h, keys)) = KEYS.iter().find(|(h, _)| *h == l) else {
                return bad(n, &format!("section {l} is not allowed (package, lib, dependencies, workspace only)"));
            };
            if seen.contains(&h) {
                return bad(n, "a section given twice");
            }
            seen.push(h);
            section = Some(keys);
            continue;
        }
        let Some((key, value)) = l.split_once('=') else { return bad(n, "not `key = value`") };
        let (key, value) = (key.trim(), value.trim());
        let Some(keys) = section else { return bad(n, "a key outside a section") };
        if !keys.contains(&key) {
            return bad(n, &format!("key `{key}` is not allowed here"));
        }
        if key == "path" && value != "\"src/lib.rs\"" {
            return bad(n, "the library's path must be src/lib.rs");
        }
        if key == "crate-type" && value != "[\"cdylib\"]" {
            return bad(n, "crate-type must be [\"cdylib\"]");
        }
        if key == "pe-sdk" {
            // Exactly `{ path = "..." }`: no version, git, features or other source.
            let path = value
                .strip_prefix('{')
                .and_then(|v| v.strip_suffix('}'))
                .map(str::trim)
                .and_then(|v| v.strip_prefix("path"))
                .map(str::trim_start)
                .and_then(|v| v.strip_prefix('='))
                .map(str::trim)
                .and_then(|v| v.strip_prefix('"'))
                .and_then(|v| v.strip_suffix('"'))
                .filter(|p| !p.contains('"'));
            let Some(path) = path else { return bad(n, "pe-sdk must be given as { path = \"...\" } only") };
            sdk = Some(path.to_string());
        }
    }
    sdk.ok_or(BuildError::Manifest("no pe-sdk dependency".into()))
}

/// An absolute path with `.` and `..` resolved lexically, as Cargo passes a path dependency's sources to rustc.
fn lexical(p: &Path) -> PathBuf {
    let abs = std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf());
    let mut out = PathBuf::new();
    for c in abs.components() {
        match c {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            c => out.push(c),
        }
    }
    out
}

/// `canonicalize` without Windows' `\\?\` prefix, which rustc's paths do not carry.
fn canonical(p: &Path) -> PathBuf {
    let c = std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let s = c.to_string_lossy();
    s.strip_prefix(r"\\?\").map(PathBuf::from).unwrap_or(c)
}

/// Every file under `dir/src` (sources, and anything `include!` may bring in), relative to `dir`, in a stable order.
fn src_files(dir: &Path) -> Vec<PathBuf> {
    fn walk(root: &Path, at: &Path, out: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(root.join(at)) else { return };
        for e in entries.flatten() {
            let rel = at.join(e.file_name());
            match e.file_type() {
                Ok(t) if t.is_dir() => walk(root, &rel, out),
                Ok(t) if t.is_file() => out.push(rel),
                _ => {}
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, Path::new("src"), &mut out);
    out.sort();
    out
}

/// The first line of the `Cargo.toml` [`build_archive`] writes; it deletes a work folder only if its manifest starts so.
const TEMPLATE_MARK: &str = "# Written by pe-logic's build service from logic.md 4.1a's template; not the author's.";

/// The canonical source archive of the script crate in `crate_dir` (logic.md 4.1a): every file under its `src`,
/// packed by pe-world's [`pe_world::script::pack_source`]. Its `Cargo.toml` is not in it: [`build_archive`] writes
/// one from the template. A file under `src` that the archive cannot carry is refused, not skipped (a file not
/// ending `.rs`, an upper-case or otherwise non-portable name, a non-UTF-8 file): another machine would build
/// without it, or not at all. So is anything under `src` that is neither a plain file nor a folder (a symlink, a
/// junction) and any entry it cannot read (@shader-reviewer's Low 1 on v6-r1): `src_files`, which the remaps use,
/// skips those silently, and the author's own build would still see them.
pub fn pack(crate_dir: &Path) -> Result<Vec<u8>, BuildError> {
    fn walk(root: &Path, at: &Path, out: &mut Vec<PathBuf>) -> Result<(), BuildError> {
        let io = |e: std::io::Error| BuildError::Io(format!("{}: {e}", at.display()));
        for e in std::fs::read_dir(root.join(at)).map_err(io)? {
            let e = e.map_err(io)?;
            let rel = at.join(e.file_name());
            let t = e.file_type().map_err(io)?;
            if t.is_dir() {
                walk(root, &rel, out)?;
            } else if t.is_file() {
                out.push(rel);
            } else {
                return Err(BuildError::Source(pe_world::script::SourceError::Path(
                    rel.to_string_lossy().into_owned(),
                )));
            }
        }
        Ok(())
    }
    let mut rels = Vec::new();
    walk(crate_dir, Path::new("src"), &mut rels)?;
    let mut files: Vec<(String, Vec<u8>)> = Vec::new();
    for rel in rels {
        let parts: Vec<&str> = rel.components().filter_map(|c| c.as_os_str().to_str()).collect();
        if parts.len() != rel.components().count() {
            return Err(BuildError::Source(pe_world::script::SourceError::Path(rel.to_string_lossy().into_owned())));
        }
        let body =
            std::fs::read(crate_dir.join(&rel)).map_err(|e| BuildError::Io(format!("{}: {e}", rel.display())))?;
        files.push((parts.join("/"), body));
    }
    let refs: Vec<(&str, &[u8])> = files.iter().map(|(p, b)| (p.as_str(), b.as_slice())).collect();
    pe_world::script::pack_source(&refs).map_err(BuildError::Source)
}

/// Builds a source archive (logic.md 4.1a, 4.2): checks it with pe-world's [`pe_world::script::source_files`], the
/// function `CommitScript` applies, then unpacks it into `work_dir/script`, a fresh folder, beside a `Cargo.toml`
/// written from the template (package `script`, edition 2024, the engine's pe-sdk at `sdk` the only dependency), and
/// builds that with [`build`]. So no path of the author's machine and no manifest of the author's enters the module.
///
/// `work_dir` is the service's own: `work_dir/script` is deleted first, but only if it is missing or its
/// `Cargo.toml` is one this function wrote. Keep `work_dir` short: 4.1a's paths are at most 128 bytes below it.
pub fn build_archive(
    tc: &Toolchain,
    archive: &[u8],
    sdk: &Path,
    work_dir: &Path,
    target_dir: &Path,
    offline: bool,
) -> Result<Vec<u8>, BuildError> {
    let files = pe_world::script::source_files(archive).map_err(BuildError::Source)?;
    // A raised `#![recursion_limit]` or `#![type_length_limit]` lets a script make the compile as long as it likes;
    // refused before the build, from the source alone, so on every machine alike (logic.md 6.2b). Any mention is
    // refused, comments included: the check is a substring, not a parse.
    for (path, body) in &files {
        for limit in ["recursion_limit", "type_length_limit"] {
            if body.windows(limit.len()).any(|w| w == limit.as_bytes()) {
                return Err(BuildError::Refused(format!("{path} names {limit}; the compiler's own limits apply")));
            }
        }
    }
    let dir = work_dir.join("script");
    // Cargo reads `.cargo/config.toml` in every folder above its working one; the service's folder must have none,
    // or that file's `[env]`, `[build]` or `[target]` could change the module on this machine only (6.2b) [V
    // `iter_bench_build_asym.py`: an `[env]` entry changed the hash].
    let abs = std::path::absolute(&dir).map_err(|e| BuildError::Io(e.to_string()))?;
    for a in abs.ancestors() {
        for f in [".cargo/config.toml", ".cargo/config"] {
            if a.join(f).exists() {
                return Err(BuildError::Environment(format!(
                    "{} lies under {}, which could change the build; move the service's folder",
                    work_dir.display(),
                    a.join(f).display()
                )));
            }
        }
    }
    let io = |e: std::io::Error| BuildError::Io(format!("{}: {e}", dir.display()));
    if dir.exists() {
        let ours = std::fs::read_to_string(dir.join("Cargo.toml")).is_ok_and(|t| t.starts_with(TEMPLATE_MARK));
        if !ours {
            return Err(BuildError::Io(format!(
                "{} exists and was not written by the build service; not deleting it",
                dir.display()
            )));
        }
        std::fs::remove_dir_all(&dir).map_err(io)?;
    }
    for (path, body) in &files {
        let to = dir.join(path);
        std::fs::create_dir_all(to.parent().unwrap_or(&dir)).map_err(io)?;
        std::fs::write(&to, body).map_err(io)?;
    }
    let sdk_path = lexical(sdk).to_string_lossy().replace('\\', "/");
    let manifest = format!(
        "{TEMPLATE_MARK}\n[package]\nname = \"script\"\nversion = \"0.0.0\"\nedition = \"2024\"\npublish = false\n\n\
         [workspace]\n\n[lib]\ncrate-type = [\"cdylib\"]\npath = \"src/lib.rs\"\n\n[dependencies]\n\
         pe-sdk = {{ path = \"{sdk_path}\" }}\n"
    );
    std::fs::write(dir.join("Cargo.toml"), manifest).map_err(io)?;
    let module = build(tc, &dir, sdk, target_dir, offline)?;
    check_reads(&dir, sdk, target_dir, "script")?;
    Ok(module)
}

/// The dep-info check (logic.md 6.2b): refuses a build whose compile read anything but the crate's `src` and
/// pe-sdk's. rustc's dep-info files (`deps/<crate>.d`) list every file a crate read, `include!`, `include_str!`,
/// `include_bytes!` and `#[path]` modules among them, and an `# env-dep:` line for every environment variable it
/// asked for, set or not [V `spikes/out/iter-bench/build-asym/depinfo.log`]. Without build scripts and proc macros
/// (4.1) nothing else can read on rustc's behalf [I]. It refuses what the script tried to read, not what it got, so
/// it refuses on every machine alike: a file present on one machine and missing on another is a refusal on the
/// first and a compile error on the second, both failures. `name` is the script crate's (`script` for an archive).
pub fn check_reads(crate_dir: &Path, sdk: &Path, target_dir: &Path, name: &str) -> Result<(), BuildError> {
    let deps = target_dir.join("wasm32-unknown-unknown").join("release").join("deps");
    let mut files = vec![(deps.join(format!("{name}.d")), crate_dir.to_path_buf())];
    let entries = std::fs::read_dir(&deps).map_err(|e| BuildError::Refused(format!("no dep-info: {e}")))?;
    for e in entries.flatten() {
        let n = e.file_name().to_string_lossy().into_owned();
        if n.starts_with("pe_sdk-") && n.ends_with(".d") {
            files.push((e.path(), sdk.to_path_buf()));
        }
    }
    let allowed = [canonical(&crate_dir.join("src")), canonical(&sdk.join("src"))];
    for (file, base) in files {
        let text = std::fs::read_to_string(&file)
            .map_err(|e| BuildError::Refused(format!("no dep-info at {}: {e}", file.display())))?;
        check_dep_info(&text, &base, &allowed)?;
    }
    Ok(())
}

/// One dep-info file's half of [`check_reads`]: `base` resolves its relative paths, `allowed` are the folders (as
/// [`canonical`] gives them) a compile may read in. **Strict** (@shader-reviewer's Medium on v8-r1): rustc escapes
/// only spaces in a file name, and Linux and macOS allow a newline in one, which would split a rule over lines that
/// each look harmless. So every non-empty line must be one of
/// - `# env-dep:NAME[=VALUE]`: refused, since any variable can differ between machines;
/// - a target rule, `<output>: <deps>`, the only kind of line with an unescaped `": "` (every space in a file name is
///   escaped), whose dependencies each have a rule of their own below;
/// - a per-file rule, `<path>:`, whose path must **resolve** (everything listed was read moments ago, so a path that
///   does not is never legitimate) to a file inside an allowed folder;
///
/// and anything else, such as a fragment of a split name, is refused.
pub fn check_dep_info(text: &str, base: &Path, allowed: &[PathBuf]) -> Result<(), BuildError> {
    let refuse = |why: String| Err(BuildError::Refused(why));
    for line in text.lines() {
        if line.is_empty() {
            continue;
        }
        if let Some(var) = line.strip_prefix("# env-dep:") {
            let var = var.split('=').next().unwrap_or(var);
            return refuse(format!("the compile asked for the environment variable {var}"));
        }
        let unescaped_colon_space = line
            .as_bytes()
            .windows(2)
            .enumerate()
            .any(|(k, w)| w == b": " && !(k > 0 && line.as_bytes()[k - 1] == b'\\'));
        if unescaped_colon_space {
            continue;
        }
        let Some(path) = line.strip_suffix(':').filter(|p| !p.is_empty() && !p.starts_with('#')) else {
            return refuse(format!("an unexpected line in the dep-info: {line:?}"));
        };
        let path = PathBuf::from(path.replace("\\ ", " "));
        let path = if path.is_absolute() { path } else { base.join(path) };
        let Ok(read) = std::fs::canonicalize(&path) else {
            return refuse(format!("the compile read {}, which does not resolve", path.display()));
        };
        let read = canonical(&read);
        if !allowed.iter().any(|a| read.starts_with(a)) {
            return refuse(format!("the compile read {}, outside the script's src and pe-sdk's", path.display()));
        }
    }
    Ok(())
}

/// The pe-sdk of the engine's own source tree, for callers that build from it (tests, the dev engine).
pub fn engine_sdk() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("pe-sdk")
}

/// Build the script crate in `crate_dir` against the engine's pe-sdk in `sdk` into `target_dir`; returns the
/// module's bytes. The script's own `pe-sdk` path must lead to `sdk`.
pub fn build(
    tc: &Toolchain,
    crate_dir: &Path,
    sdk: &Path,
    target_dir: &Path,
    offline: bool,
) -> Result<Vec<u8>, BuildError> {
    let manifest = crate_dir.join("Cargo.toml");
    let text = std::fs::read_to_string(&manifest).map_err(|e| BuildError::Spawn(e.to_string()))?;
    let name = package_name(&text).ok_or(BuildError::NoName)?.replace('-', "_");
    let sdk_rel = check_manifest(&text)?;
    for f in ["build.rs", ".cargo"] {
        if crate_dir.join(f).exists() {
            return Err(BuildError::Manifest(format!("the script crate may not have {f}")));
        }
    }
    std::fs::create_dir_all(target_dir).map_err(|e| BuildError::Spawn(e.to_string()))?;
    let sdk_dir = crate_dir.join(&sdk_rel);
    if canonical(&sdk_dir) != canonical(sdk) {
        return Err(BuildError::Manifest(format!(
            "pe-sdk at {sdk_rel:?} is not the engine's pe-sdk; a script builds only against the engine's own"
        )));
    }
    // The service's own CARGO_HOME, so no configuration of the machine's cargo applies (6.2b). Inside the target
    // directory, which is remapped; the script's only dependency is pe-sdk by path, so nothing is fetched into it.
    let cargo_home = target_dir.join("cargo-home");
    std::fs::create_dir_all(&cargo_home).map_err(|e| BuildError::Spawn(e.to_string()))?;
    let mut rustflags: Vec<String> = CODEGEN.iter().map(|s| s.to_string()).collect();
    // rustc applies the last remap that matches, so the most specific (the script's folder, which may sit inside
    // the engine's or the target's) goes last. Each folder is given both as the file system names it and lexically.
    let pairs = [(target_dir, "/target"), (sdk_dir.as_path(), "/pe-sdk"), (crate_dir, "/script")];
    for (from, to) in pairs {
        let mut forms = vec![canonical(from), lexical(from)];
        forms.dedup();
        for f in forms {
            rustflags.push(format!("--remap-path-prefix={}={to}", f.display()));
        }
    }
    rustflags.push(format!("--remap-path-prefix={}=/cargo", cargo_home.display()));
    // Below a remapped folder the host's separator survives: Windows wrote `/pe-sdk\src\guest.rs` and `src\lib.rs`
    // into panic locations where Linux writes `/pe-sdk/src/guest.rs` and `src/lib.rs`, so the same script built to
    // different bytes on the two (3 bytes of the lamp, scripts/iter_bench_xos.sh). On Windows every file under
    // `src` (sources and `include!`d files) is therefore remapped whole to its forward-slash name, after the folders, so it wins; the script's
    // own files also by the relative path cargo hands rustc. A module brought in from elsewhere with `#[path]` is
    // not covered: its bytes may then differ by OS, which the module hash comparison (6.2) reports as a build fault.
    if cfg!(windows) {
        for (dir, to) in [(sdk_dir.as_path(), "/pe-sdk"), (crate_dir, "/script")] {
            let mut forms = vec![canonical(dir), lexical(dir)];
            forms.dedup();
            for rel in src_files(dir) {
                let fwd: Vec<String> = rel.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect();
                let fwd = fwd.join("/");
                for f in &forms {
                    rustflags.push(format!("--remap-path-prefix={}={to}/{fwd}", f.join(&rel).display()));
                }
                if to == "/script" {
                    rustflags.push(format!("--remap-path-prefix={}={fwd}", rel.display()));
                }
            }
        }
    }
    // The per-file remaps make the flags long on Windows (114,652 characters for 301 files, measured): no 32,767-
    // character cap applies, because std passes its own environment block to CreateProcessW and cargo moves a long
    // rustc command line into an argument file [V `tests/lamp.rs`, `a_script_with_many_files_builds...`].
    let encoded = rustflags.join("\u{1f}");
    let mut cmd = Command::new(&tc.cargo);
    // The crate is the working folder, so cargo looks for configuration only above it (6.2b); and the environment
    // is cleared to KEEP_ENV, so nothing of the engine's process reaches cargo, rustc or the script.
    cmd.current_dir(crate_dir).env_clear();
    for k in KEEP_ENV {
        if let Some(v) = std::env::var_os(k) {
            cmd.env(k, v);
        }
    }
    cmd.env("RUSTC", &tc.rustc).env("CARGO_HOME", &cargo_home);
    cmd.arg("rustc")
        // The job count is pinned: the module's bytes do not depend on it, but its peak memory does, which a memory
        // cap measures (logic.md 6.2a item 5, 6.2b) [V `iter_bench_build_asym.py`: the 6.4k-line guest peaked at
        // 142.8, 151.4 and 157.1 MiB at 1, 4 and 16 jobs, with the same bytes].
        .arg("--jobs")
        .arg(BUILD_JOBS.to_string())
        .arg("--release")
        .arg("--lib")
        .arg("--target")
        .arg("wasm32-unknown-unknown")
        .arg("--manifest-path")
        .arg(&manifest)
        .arg("--message-format=short");
    if offline {
        cmd.arg("--offline");
    }
    for c in PROFILE {
        cmd.arg("--config").arg(c);
    }
    // `-F long_running_const_eval`: a script may not `allow` rustc's lint for a long const-eval loop, so such a loop
    // is the same compile error on every machine rather than a build that ends only when a slow machine gives up
    // (logic.md 6.2b; @fable-reviewer).
    cmd.arg("--").arg("-F").arg("unsafe_code").arg("-F").arg("long_running_const_eval");
    // The flags go through CARGO_ENCODED_RUSTFLAGS (0x1f-separated), so paths with spaces survive.
    cmd.env("CARGO_ENCODED_RUSTFLAGS", encoded)
        .env_remove("RUSTFLAGS")
        // The environment can override a profile: CARGO_INCREMENTAL beats `incremental`, and a wrapper (sccache and
        // the like) sits between cargo and rustc. CARGO_ENCODED_RUSTFLAGS already beats every other flags source.
        .env("CARGO_INCREMENTAL", "0")
        .env("RUSTC_WRAPPER", "")
        .env("RUSTC_WORKSPACE_WRAPPER", "")
        .env("CARGO_TARGET_DIR", target_dir);
    let out = cmd.output().map_err(|e| BuildError::Spawn(e.to_string()))?;
    if !out.status.success() {
        return Err(BuildError::Rustc(String::from_utf8_lossy(&out.stderr).into_owned()));
    }
    let path = target_dir.join("wasm32-unknown-unknown").join("release").join(format!("{name}.wasm"));
    std::fs::read(&path).map_err(|_| BuildError::NoModule(path))
}

#[cfg(test)]
mod tests {
    #[test]
    fn package_name_is_read() {
        let m = "[package]\nname = \"lamp-x\"\nversion = \"0\"\n[dependencies]\nname = \"no\"\n";
        assert_eq!(super::package_name(m).as_deref(), Some("lamp-x"));
    }
}

/// Why a voter cannot attest a version: `BuildFailed`'s reason (logic.md 6.2a item 2). An enum and hashes go into
/// the entry, never compiler output, which can carry local paths.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FailReason {
    /// rustc refused the source: a function of the source and the build identity, the same on every machine.
    Compile,
    /// The build service refused it (6.2b's dep-info check, a raised compiler limit, a non-canonical archive): from
    /// the source alone, the same on every machine.
    Refused,
    /// Over the memory cap (6.2a item 5). Not produced yet: the cap is not built.
    Cap,
    /// The module built, but its hash is not the row's `author_module`, or the metering pass refused it.
    Mismatch,
    /// This machine's own trouble: cargo could not start, a file could not be written, no diagnostic.
    Local,
}

impl BuildError {
    /// The `BuildFailed` reason of this error. A rustc failure is `Compile` only when its output holds a diagnostic
    /// at a source location (`path:line:col: error`, the short format the service asks for); cargo's own errors and a
    /// killed compiler print none, and are `Local`.
    pub fn reason(&self) -> FailReason {
        match self {
            BuildError::Rustc(out) => {
                let located = out.lines().any(|l| {
                    // `src\lib.rs:3:5: error[E0425]: ...`; split from the right, so a drive letter's colon stays in
                    // the path.
                    let Some(i) = l.find(": error") else { return false };
                    let mut parts = l[..i].rsplitn(3, ':');
                    let (Some(col), Some(line), Some(path)) = (parts.next(), parts.next(), parts.next()) else {
                        return false;
                    };
                    !path.is_empty() && line.parse::<u32>().is_ok() && col.parse::<u32>().is_ok()
                });
                if located { FailReason::Compile } else { FailReason::Local }
            }
            BuildError::Refused(_) | BuildError::Source(_) | BuildError::Manifest(_) | BuildError::NoName => {
                FailReason::Refused
            }
            BuildError::Spawn(_)
            | BuildError::NoModule(_)
            | BuildError::Io(_)
            | BuildError::Toolchain(_)
            | BuildError::Environment(_) => FailReason::Local,
        }
    }
}

/// A version this machine cannot attest, after its retries: what `BuildFailed` reports, plus the local detail.
#[derive(Debug)]
pub struct Failed {
    pub reason: FailReason,
    /// Present exactly for a `Mismatch` (pe-world refuses a `BuildFailed` otherwise, @world-model m4bd3e101e652): the
    /// hash of the metered module this machine built, or, when the metering pass refused it, the raw module's
    /// `Hash::of("pe.logic.raw", wasm)`, a domain no `author_module` is in.
    pub module: Option<pe_core::Hash>,
    /// How many builds were tried.
    pub attempts: u32,
    /// The last error, for this machine's log and its bug bundle; never sent (it can carry local paths).
    pub detail: String,
}

/// Deletes what the build service itself wrote, so the next build starts clean: `work_dir/script` (which
/// [`build_archive`] replaces anyway) and, in `target_dir`, only the wasm32 output and the service's `CARGO_HOME`,
/// never the caller's folder itself, which may be shared (@shader-reviewer's Low 1 on v9-r1).
fn clean(work_dir: &Path, target_dir: &Path) {
    let _ = std::fs::remove_dir_all(work_dir.join("script"));
    let _ = std::fs::remove_dir_all(target_dir.join("wasm32-unknown-unknown"));
    let _ = std::fs::remove_dir_all(target_dir.join("cargo-home"));
}

/// How often a `Local` failure is retried before it is reported (logic.md 6.2b): under the build-asymmetry ruling a
/// reported failure of an activated version costs the member the session.
pub const LOCAL_RETRIES: u32 = 2;

/// Builds a row's version for attestation (logic.md 6.2 step 3, 6.2a, 6.2b): [`build_archive`], then the metering
/// pass and the module hash, which must equal `author_module`. Before a failure is reported it is retried: a
/// `Local` one [`LOCAL_RETRIES`] times, the last of them from clean; a `Mismatch` once, from clean ([`clean`]),
/// since a stale or corrupt build folder gives a wrong hash with no diagnostic. `Compile` and `Refused` are not
/// retried: they are the same on every attempt and every machine. Returns the loaded module on success.
#[allow(clippy::too_many_arguments)]
pub fn build_for_row(
    tc: &Toolchain,
    archive: &[u8],
    sdk: &Path,
    work_dir: &Path,
    target_dir: &Path,
    offline: bool,
    engine: &wasmtime::Engine,
    settings: crate::runtime::Settings,
    author_module: pe_core::Hash,
) -> Result<crate::runtime::Script, Failed> {
    let mut attempts = 0;
    let mut locals = 0;
    let mut cleaned = false;
    loop {
        attempts += 1;
        let fail = |reason, module, detail: String| Failed { reason, module, attempts, detail };
        let err = match build_archive(tc, archive, sdk, work_dir, target_dir, offline) {
            Ok(wasm) => match crate::runtime::Script::load(engine, settings, &wasm) {
                Ok(script) if script.module_hash() == author_module => return Ok(script),
                Ok(script) => fail(
                    FailReason::Mismatch,
                    Some(script.module_hash()),
                    format!("module {:?}, the row names {author_module:?}", script.module_hash()),
                ),
                Err(crate::runtime::LoadError::Refused(r)) => fail(
                    FailReason::Mismatch,
                    // No metered hash exists, so the raw module's, in its own domain: it never equals a metered
                    // `author_module`, as pe-world requires of a `Mismatch` (@world-model m4bd3e101e652).
                    Some(pe_core::Hash::of("pe.logic.raw", &wasm)),
                    format!(
                        "the metering pass refused the module (raw {:?}): {r}",
                        pe_core::Hash::of("pe.logic.raw", &wasm)
                    ),
                ),
                Err(e) => fail(FailReason::Local, None, e.to_string()),
            },
            Err(e) => fail(e.reason(), None, e.to_string()),
        };
        match err.reason {
            FailReason::Local if locals < LOCAL_RETRIES => {
                locals += 1;
                // The last retry starts clean: a corrupt artifact (a half-written rlib after a killed build) fails
                // the same way every time it is reused (@shader-reviewer's Low 2 on v9-r1).
                if locals == LOCAL_RETRIES {
                    clean(work_dir, target_dir);
                }
            }
            FailReason::Mismatch if !cleaned => {
                cleaned = true;
                clean(work_dir, target_dir);
            }
            _ => return Err(err),
        }
    }
}

/// The author's margin on build time (logic.md 6.2a item 5): the editor refuses to commit a script whose own build took
/// longer, so that a machine about 4 times slower still builds it inside `T_build` (20 s) [I; the factor is to be
/// measured on the second PC]. It protects honest authors only; attesters have no time limit (6.2 step 3).
pub const AUTHOR_MAX_BUILD: std::time::Duration = std::time::Duration::from_secs(15);

/// Why the author's tooling does not commit a script (logic.md 6.2a item 5, 4.2).
#[derive(Debug)]
pub enum CommitRefusal {
    /// The build failed; its reason is [`BuildError::reason`].
    Build(BuildError),
    /// The module did not load (the metering pass refused it, or the engine could not compile it).
    Load(crate::runtime::LoadError),
    /// The build took longer than the author's margin.
    TooSlow { took: std::time::Duration, max: std::time::Duration },
}

impl std::fmt::Display for CommitRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CommitRefusal::Build(e) => e.fmt(f),
            CommitRefusal::Load(e) => write!(f, "the module did not load: {e}"),
            CommitRefusal::TooSlow { took, max } => write!(
                f,
                "the script took {:.1} s to build here, over the {:.1} s margin that keeps it buildable on slower \
                 machines (logic.md 6.2a item 5)",
                took.as_secs_f64(),
                max.as_secs_f64()
            ),
        }
    }
}

/// What the author commits: the archive, and the module built from it, whose hash is `author_module`.
pub struct Commit {
    pub archive: Vec<u8>,
    pub script: crate::runtime::Script,
    pub took: std::time::Duration,
}

/// The author's side of a commit (logic.md 4.2, 6.2a item 5): packs the crate in `crate_dir`, builds the archive with
/// [`build_archive`], the same path every other machine takes, loads it, and refuses if the build took longer than
/// `max` ([`AUTHOR_MAX_BUILD`] for the editor). The author's memory margin waits for the memory cap's OS binding (M2).
#[allow(clippy::too_many_arguments)]
pub fn build_for_commit(
    tc: &Toolchain,
    crate_dir: &Path,
    sdk: &Path,
    work_dir: &Path,
    target_dir: &Path,
    offline: bool,
    engine: &wasmtime::Engine,
    settings: crate::runtime::Settings,
    max: std::time::Duration,
) -> Result<Commit, CommitRefusal> {
    let archive = pack(crate_dir).map_err(CommitRefusal::Build)?;
    let t0 = std::time::Instant::now();
    let wasm = build_archive(tc, &archive, sdk, work_dir, target_dir, offline).map_err(CommitRefusal::Build)?;
    let took = t0.elapsed();
    if took > max {
        return Err(CommitRefusal::TooSlow { took, max });
    }
    let script = crate::runtime::Script::load(engine, settings, &wasm).map_err(CommitRefusal::Load)?;
    Ok(Commit { archive, script, took })
}

/// The build service's own version (logic.md 6.2b), part of [`build_identity`]: bumped on any change to what decides
/// a module's bytes or a build's verdict that the toolchain and pe-sdk hashes do not see: the template, the profile
/// and codegen flags, the remaps, [`KEEP_ENV`], [`BUILD_JOBS`], and the checks (the dep-info check, the compiler
/// limits, the archive rules it applies before building).
pub const BUILD_SERVICE_VERSION: u32 = 1;

/// The toolchain's release, as [`build_identity`] takes it: rustc's `release`, `commit-hash` and `LLVM version`
/// lines from `rustc -vV`, in that order, and not its `host`, since machines on different OSes run different rustc
/// binaries of one release, which give the same wasm32 bytes [V `iter_bench_xos.sh`, Windows and Linux on one PC].
pub fn toolchain_release(tc: &Toolchain) -> Result<String, BuildError> {
    let out = run_rustc(tc, &["-vV"])?;
    let mut lines = Vec::new();
    for key in ["release:", "commit-hash:", "LLVM version:"] {
        let l = out
            .lines()
            .find(|l| l.starts_with(key))
            .ok_or_else(|| BuildError::Toolchain(format!("rustc -vV has no {key} line")))?;
        lines.push(l.trim().to_string());
    }
    Ok(lines.join("\n"))
}

fn run_rustc(tc: &Toolchain, args: &[&str]) -> Result<String, BuildError> {
    let mut cmd = Command::new(&tc.rustc);
    cmd.args(args).env_clear();
    for k in KEEP_ENV {
        if let Some(v) = std::env::var_os(k) {
            cmd.env(k, v);
        }
    }
    let out = cmd.output().map_err(|e| BuildError::Toolchain(format!("{}: {e}", tc.rustc.display())))?;
    if !out.status.success() {
        return Err(BuildError::Toolchain(format!("{} {args:?} failed", tc.rustc.display())));
    }
    String::from_utf8(out.stdout).map_err(|_| BuildError::Toolchain("rustc printed non-UTF-8".into()))
}

/// Every file under `dir`, relative to it with `/` separators, sorted, with its size.
fn tree_sizes(dir: &Path) -> Result<Vec<(String, u64)>, BuildError> {
    fn walk(root: &Path, at: &Path, out: &mut Vec<(String, u64)>) -> Result<(), BuildError> {
        let io = |e: std::io::Error| BuildError::Toolchain(format!("{}: {e}", root.join(at).display()));
        for e in std::fs::read_dir(root.join(at)).map_err(io)? {
            let e = e.map_err(io)?;
            let rel = at.join(e.file_name());
            let meta = e.metadata().map_err(io)?;
            if meta.is_dir() {
                walk(root, &rel, out)?;
            } else {
                let name: Vec<String> =
                    rel.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect();
                out.push((name.join("/"), meta.len()));
            }
        }
        Ok(())
    }
    let mut out = Vec::new();
    walk(dir, Path::new(""), &mut out)?;
    out.sort();
    Ok(out)
}

/// Every file under `dir`, relative to it with `/` separators, sorted, with its bytes.
fn tree_files(dir: &Path) -> Result<Vec<(String, Vec<u8>)>, BuildError> {
    fn walk(root: &Path, at: &Path, out: &mut Vec<(String, Vec<u8>)>) -> Result<(), BuildError> {
        let io = |e: std::io::Error| BuildError::Toolchain(format!("{}: {e}", root.join(at).display()));
        for e in std::fs::read_dir(root.join(at)).map_err(io)? {
            let e = e.map_err(io)?;
            let rel = at.join(e.file_name());
            if e.file_type().map_err(io)?.is_dir() {
                walk(root, &rel, out)?;
            } else {
                let name: Vec<String> =
                    rel.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect();
                out.push((name.join("/"), std::fs::read(root.join(&rel)).map_err(io)?));
            }
        }
        Ok(())
    }
    let mut out = Vec::new();
    walk(dir, Path::new(""), &mut out)?;
    out.sort();
    Ok(out)
}

/// The build identity B (logic.md 6.2b): a hash of everything besides the archive that decides a module's bytes or a
/// build's verdict, so two machines with equal B build every version alike, and the session compares B when a member
/// joins (pe-app's build id). It covers:
/// - [`BUILD_SERVICE_VERSION`] and pe-logic's [`crate::RESULT_VERSION`] (the metering pass's results);
/// - [`toolchain_release`], and every file of the toolchain's wasm32 std (`lib/rustlib/wasm32-unknown-unknown`,
///   one artifact for every host [I]) by name and size. The names carry each std crate's metadata hash
///   (`libcore-79bd1e8c3e3cb812.rlib`), so with the release's commit they pin the std without reading its 99 MiB,
///   which took about 4.5 s a time in a debug build [V `tests/build_identity.rs`, this desktop];
/// - pe-sdk at `sdk`: its `Cargo.toml` and every file under its `src`, by path and hash.
///
/// A caller computes it once per process. D75's pinned toolchain hash is to replace the release lines and the std
/// listing once the toolchain is installed by hash.
pub fn build_identity(tc: &Toolchain, sdk: &Path) -> Result<pe_core::Hash, BuildError> {
    let mut b = Vec::new();
    let mut lp = |part: &[u8]| {
        b.extend_from_slice(&(part.len() as u64).to_le_bytes());
        b.extend_from_slice(part);
    };
    lp(b"pe.logic.build.v1");
    lp(&BUILD_SERVICE_VERSION.to_le_bytes());
    lp(&crate::RESULT_VERSION.to_le_bytes());
    lp(toolchain_release(tc)?.as_bytes());
    let sysroot = PathBuf::from(run_rustc(tc, &["--print", "sysroot"])?.trim());
    let std_dir = sysroot.join("lib").join("rustlib").join("wasm32-unknown-unknown");
    for (name, size) in tree_sizes(&std_dir)? {
        lp(name.as_bytes());
        lp(&size.to_le_bytes());
    }
    lp(b"pe-sdk");
    let manifest = std::fs::read(sdk.join("Cargo.toml"))
        .map_err(|e| BuildError::Toolchain(format!("{}: {e}", sdk.join("Cargo.toml").display())))?;
    lp(&pe_core::Hash::of("pe.logic.build.file", &manifest).0);
    for (name, bytes) in tree_files(&sdk.join("src"))? {
        lp(name.as_bytes());
        lp(&pe_core::Hash::of("pe.logic.build.file", &bytes).0);
    }
    Ok(pe_core::Hash::of("pe.logic.build", &b))
}
