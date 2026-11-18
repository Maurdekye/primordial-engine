//! The build id's source part (app.md 4, "The build id"; @coordinator-opus, after @fable-reviewer): one hash of the sources a build came from. pe-app's build script (build.rs) includes this file and
//! sets `PE_SOURCE_HASH` from it. The library compiles it only for its tests, which check that the two agree.

use std::path::{Path, PathBuf};

/// What it hashes, from the workspace's root: every regular file under `crates/`, then `Cargo.lock` and the root
/// `Cargo.toml`. Equal hashes mean the same crate sources, dependency versions and workspace settings. A document
/// outside `crates/` changes nothing.
pub const INPUTS: [&str; 3] = ["crates", "Cargo.lock", "Cargo.toml"];

/// The sources' hash: FNV-1a with 128 bits over each file, in the order of its path from `root` (`/` between its
/// parts). It takes the path, a zero byte, the length (u64, little-endian) and the bytes. It tells two trees apart by
/// accident only, not against someone choosing a collision, and that is all the build id asks of it: it names the
/// same sources, not a trusted build. It is the same on every platform, since `.gitattributes` checks every text file
/// out with LF.
pub fn hash(root: &Path) -> std::io::Result<u128> {
    let mut files = Vec::new();
    for input in INPUTS {
        let path = root.join(input);
        if path.is_dir() {
            walk(root, &path, &mut files)?;
        } else {
            files.push((input.to_string(), path));
        }
    }
    files.sort();
    let mut h = Fnv::new();
    for (rel, path) in &files {
        let bytes = std::fs::read(path)?;
        h.write(rel.as_bytes());
        h.write(&[0]);
        h.write(&(bytes.len() as u64).to_le_bytes());
        h.write(&bytes);
    }
    Ok(h.0)
}

/// Every regular file below `dir`, by its path from `root`. Symbolic links are skipped.
fn walk(root: &Path, dir: &Path, out: &mut Vec<(String, PathBuf)>) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let (kind, path) = (entry.file_type()?, entry.path());
        if kind.is_dir() {
            walk(root, &path, out)?;
        } else if kind.is_file() {
            let rel: Vec<String> = (path.strip_prefix(root).unwrap_or(&path).components())
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect();
            out.push((rel.join("/"), path));
        }
    }
    Ok(())
}

/// FNV-1a, 128 bits.
struct Fnv(u128);

impl Fnv {
    fn new() -> Fnv {
        Fnv(0x6c62_272e_07bb_0142_62b8_2175_6295_c58d)
    }

    fn write(&mut self, bytes: &[u8]) {
        const PRIME: u128 = 0x0000_0000_0100_0000_0000_0000_0000_013b;
        for b in bytes {
            self.0 = (self.0 ^ u128::from(*b)).wrapping_mul(PRIME);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The build script's hash is this tree's. cargo runs the script again whenever a file under `crates/`,
    /// `Cargo.lock` or `Cargo.toml` changes, so the id names exactly the sources it was built from.
    #[test]
    fn the_build_id_s_source_hash_is_this_tree_s() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        assert_eq!(format!("{:032x}", hash(&root).expect("the sources")), env!("PE_SOURCE_HASH"));
    }

    /// One changed byte, a moved boundary between two files or a renamed file changes the hash. The same tree gives
    /// the same hash.
    #[test]
    fn a_changed_byte_boundary_or_name_changes_the_hash() {
        let dir = std::env::temp_dir().join(format!("pe-app-sources-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let write = |rel: &str, text: &str| {
            let p = dir.join(rel);
            std::fs::create_dir_all(p.parent().expect("a folder")).expect("the folder");
            std::fs::write(p, text).expect("the file");
        };
        write("Cargo.toml", "[workspace]");
        write("Cargo.lock", "");
        write("crates/a/src/lib.rs", "ab");
        write("crates/b/src/lib.rs", "c");
        let base = hash(&dir).expect("hashed");
        assert_eq!(hash(&dir).expect("hashed"), base, "the same tree, the same hash");
        write("crates/a/src/lib.rs", "aB");
        assert_ne!(hash(&dir).expect("hashed"), base, "a byte");
        write("crates/a/src/lib.rs", "a");
        write("crates/b/src/lib.rs", "bc");
        assert_ne!(hash(&dir).expect("hashed"), base, "a boundary between files");
        write("crates/a/src/lib.rs", "ab");
        write("crates/b/src/lib.rs", "c");
        assert_eq!(hash(&dir).expect("hashed"), base, "back as it was");
        std::fs::rename(dir.join("crates/b/src/lib.rs"), dir.join("crates/b/src/main.rs")).expect("renamed");
        assert_ne!(hash(&dir).expect("hashed"), base, "a name");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
