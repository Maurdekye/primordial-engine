//! A script version's source: one canonical archive blob (logic.md 4.1a, R-L1; world-model.md 7.9).
//!
//! [`check_source`] is **consensus code**: what it accepts decides which `CommitScript`s apply, so a change to it is a
//! change to pe-world's apply rules, and its tests pin accept and refuse on exact bytes (@coordinator-opus
//! m9f5e6b2ce690, m043445294e6a). pe-world runs it at apply on the bytes the commit entry carries; pe-logic's build
//! service runs the same function before it unpacks an archive, and [`pack_source`] writes one.
//!
//! The format: the bytes `"pe.src.v0"`, then for each file, in strictly ascending byte order of its path (so no
//! duplicate), `lp(path) ‖ lp(bytes)`, `lp` a `u32` little-endian length. Paths are portable, so one archive unpacks
//! to the same files on every filesystem (@design-reviewer m370657a286a8): `src/` followed by directory segments
//! and a file segment. `Cargo.toml` is not in it: the build service writes it from one template (logic.md 4.1a,
//! @design-reviewer's Low mad71ca06567b), so no manifest an author writes can make a build fail or differ. Every segment is 1 to 64 bytes of `[a-z0-9_]`, the file
//! segment's ending `.rs` included (so its stem is 1 to 61 bytes), and no segment's stem is a Windows reserved name.
//! A whole path is at most [`MAX_PATH`] (128) bytes and [`MAX_SEGMENTS`] (8) segments, `src` included.
//! ASCII lower case only, so no two allowed paths collide after case folding or Unicode normalization.
//! `src/lib.rs` must be present, and every file is UTF-8: anything else could never build, and an
//! archive no machine builds would be a build fault everywhere (logic.md 6.2 step 3).

/// The largest source archive a `CommitScript` may carry, in bytes (logic.md 4.1a; @coordinator-opus m46018cd9eb14).
/// Checked first, from the length alone; over it the commit is refused `TooLarge { limit }`.
pub const MAX_SOURCE_BYTES: u32 = 256 * 1024;

/// The archive's first bytes, its format version.
pub const SOURCE_MAGIC: &[u8] = b"pe.src.v0";

/// The longest path segment, in bytes, a file segment's `.rs` included.
pub const MAX_SEGMENT: usize = 64;

/// The longest path, in bytes, and the most segments in it, `src` included: unpacked under the build service's folder,
/// a path stays well inside Windows's 260-character `MAX_PATH`, so no machine's build fails on its OS alone
/// (@design-reviewer's Medium on 1babd1b7).
pub const MAX_PATH: usize = 128;
pub const MAX_SEGMENTS: usize = 8;

/// Why bytes are not a canonical source archive. pe-world refuses such a commit as `Invalid`; the variant is for
/// messages and tests.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SourceError {
    /// Over [`MAX_SOURCE_BYTES`].
    TooLarge,
    /// No `"pe.src.v0"` at the start.
    Magic,
    /// A length runs past the end of the bytes.
    Truncated,
    /// A path outside the portable rules (the module doc).
    Path(String),
    /// A path not after the one before it in byte order: a duplicate, or out of order.
    Order(String),
    /// A file that is not UTF-8.
    NotUtf8(String),
    /// `src/lib.rs` missing.
    Missing(&'static str),
}

impl std::fmt::Display for SourceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SourceError::TooLarge => write!(f, "the source is over {MAX_SOURCE_BYTES} bytes"),
            SourceError::Magic => f.write_str("not a pe.src.v0 archive"),
            SourceError::Truncated => f.write_str("the archive ends inside a length or an entry"),
            SourceError::Path(p) => write!(f, "{p:?} is not a portable source path"),
            SourceError::Order(p) => write!(f, "{p:?} is a duplicate or out of byte order"),
            SourceError::NotUtf8(p) => write!(f, "{p:?} is not UTF-8"),
            SourceError::Missing(p) => write!(f, "the archive has no {p}"),
        }
    }
}

impl std::error::Error for SourceError {}

/// Windows's reserved device names, which no segment's stem may be.
const RESERVED: [&str; 24] = [
    "con", "prn", "aux", "nul", "com0", "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8", "com9", "lpt0",
    "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9",
];

fn segment_ok(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= MAX_SEGMENT
        && s.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
        && !RESERVED.contains(&s)
}

/// One `lp(..)` item off the front of `rest`.
fn take<'a>(rest: &mut &'a [u8]) -> Result<&'a [u8], SourceError> {
    let all: &'a [u8] = rest;
    let n = all.get(..4).ok_or(SourceError::Truncated)?;
    let n = u32::from_le_bytes([n[0], n[1], n[2], n[3]]) as usize;
    let end = n.checked_add(4).ok_or(SourceError::Truncated)?;
    let body = all.get(4..end).ok_or(SourceError::Truncated)?;
    *rest = &all[end..];
    Ok(body)
}

/// Whether `path` is a portable source path (the module doc).
pub fn path_ok(path: &str) -> bool {
    if path.len() > MAX_PATH || path.split('/').count() > MAX_SEGMENTS {
        return false;
    }
    let Some(rest) = path.strip_prefix("src/") else { return false };
    let mut segs: Vec<&str> = rest.split('/').collect();
    let Some(file) = segs.pop() else { return false };
    file.len() <= MAX_SEGMENT && file.strip_suffix(".rs").is_some_and(segment_ok) && segs.iter().all(|s| segment_ok(s))
}

/// The files of a canonical source archive, in its order, or why it is not one. [`check_source`] is this without
/// the files; pe-logic's build service unpacks what this returns.
pub fn source_files(bytes: &[u8]) -> Result<Vec<(&str, &[u8])>, SourceError> {
    if bytes.len() > MAX_SOURCE_BYTES as usize {
        return Err(SourceError::TooLarge);
    }
    let mut rest = bytes.strip_prefix(SOURCE_MAGIC).ok_or(SourceError::Magic)?;
    let mut files: Vec<(&str, &[u8])> = Vec::new();
    while !rest.is_empty() {
        let path = take(&mut rest)?;
        let body = take(&mut rest)?;
        let path = std::str::from_utf8(path).map_err(|_| SourceError::Path(String::from_utf8_lossy(path).into()))?;
        if !path_ok(path) {
            return Err(SourceError::Path(path.into()));
        }
        if files.last().is_some_and(|(p, _)| p.as_bytes() >= path.as_bytes()) {
            return Err(SourceError::Order(path.into()));
        }
        if std::str::from_utf8(body).is_err() {
            return Err(SourceError::NotUtf8(path.into()));
        }
        files.push((path, body));
    }
    if !files.iter().any(|(p, _)| *p == "src/lib.rs") {
        return Err(SourceError::Missing("src/lib.rs"));
    }
    Ok(files)
}

/// Whether `bytes` is a canonical source archive (the module doc). Consensus code: see the module doc.
pub fn check_source(bytes: &[u8]) -> Result<(), SourceError> {
    source_files(bytes).map(|_| ())
}

/// The canonical archive of `files` (paths relative to the crate root, any order), checked with [`check_source`].
pub fn pack_source(files: &[(&str, &[u8])]) -> Result<Vec<u8>, SourceError> {
    let mut sorted: Vec<&(&str, &[u8])> = files.iter().collect();
    sorted.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
    let mut out = SOURCE_MAGIC.to_vec();
    for (path, body) in sorted {
        for part in [path.as_bytes(), body] {
            let n = u32::try_from(part.len()).map_err(|_| SourceError::TooLarge)?;
            out.extend_from_slice(&n.to_le_bytes());
            out.extend_from_slice(part);
        }
    }
    check_source(&out)?;
    Ok(out)
}
