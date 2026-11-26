//! Meshes and hulls built from their simulation blobs (world-model.md 3.2; CCD part 2 D), kept by blob.
//!
//! Building a mesh costs about 0.45, 5.2 and 22 ms at 1k, 10k and 50k triangles, and rebuilding a body with a mesh
//! already built about 0.16, 2.4 and 8.9 ms [V CCD part 2 C, pe-physics `examples/mesh_cost.rs`]. A body is rebuilt
//! at every change of its shapes, its body fields or its scale, and the replica reads a body's row, which holds its
//! built meshes, before and after every entry that touches it. So each blob is read and built once, then kept.
//!
//! This is a cache of a pure function of the bytes: what it keeps, and what it lets go, never changes a result, so
//! two machines may keep different sets. It lets go of the least recently used builds once the blobs it keeps pass
//! [`KEPT_BYTES`].

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use pe_physics::{Mesh, check_hull, hull_points_from_blob};

/// How a shape reads its blob: by its `Shape.kind`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Read {
    /// A mesh v1 blob (`PEM1`).
    Mesh,
    /// A hull v1 blob (`PEH1`).
    Hull,
}

/// A blob as built.
#[derive(Clone, Debug)]
pub(crate) enum Built {
    Mesh(Mesh),
    /// The hull's points as the blob gives them, in the shape's own frame, before any scale.
    Hull(Arc<[[f32; 3]]>),
    /// It can't be read or built: the same on every machine (world-model.md 3.2).
    Bad,
}

/// How many bytes of blobs the cache keeps before it lets the least recently used go [I]: a built mesh takes about
/// twice its blob in memory (its transfer does [V CCD part 2 C]), so about 64 MiB of built meshes.
pub(crate) const KEPT_BYTES: u64 = 32 << 20;

/// The built meshes and hulls, by how they are read and the blob's hash. Clones share one cache.
#[derive(Clone)]
pub(crate) struct BlobShapes(Arc<Mutex<Kept>>);

struct Kept {
    /// Each build, with its blob's length and when it was last used.
    builds: BTreeMap<(Read, [u8; 32]), (Built, u64, u64)>,
    bytes: u64,
    clock: u64,
    /// The blobs' bytes it keeps before it lets builds go: [`KEPT_BYTES`].
    budget: u64,
}

impl Default for BlobShapes {
    fn default() -> BlobShapes {
        BlobShapes::with_budget(KEPT_BYTES)
    }
}

impl BlobShapes {
    fn with_budget(budget: u64) -> BlobShapes {
        BlobShapes(Arc::new(Mutex::new(Kept { builds: BTreeMap::new(), bytes: 0, clock: 0, budget })))
    }

    /// `bytes`, whose hash is `hash`, as `read` builds it: built at first sight, then kept.
    pub(crate) fn get(&self, read: Read, hash: [u8; 32], bytes: &[u8]) -> Built {
        let mut kept = self.0.lock().unwrap_or_else(|e| e.into_inner());
        kept.clock += 1;
        let now = kept.clock;
        if let Some((built, _, used)) = kept.builds.get_mut(&(read, hash)) {
            *used = now;
            return built.clone();
        }
        let built = match read {
            Read::Mesh => Mesh::from_blob(bytes).map_or(Built::Bad, Built::Mesh),
            Read::Hull => match hull_points_from_blob(bytes) {
                Ok(points) if check_hull(&points).is_ok() => Built::Hull(points.into()),
                _ => Built::Bad,
            },
        };
        let len = bytes.len() as u64;
        kept.builds.insert((read, hash), (built.clone(), len, now));
        kept.bytes += len;
        // Let the least recently used go, never the build just made.
        while kept.bytes > kept.budget {
            let oldest = kept.builds.iter().filter(|(_, e)| e.2 != now).min_by_key(|(_, e)| e.2).map(|(k, _)| *k);
            let Some(key) = oldest else { break };
            if let Some((_, len, _)) = kept.builds.remove(&key) {
                kept.bytes -= len;
            }
        }
        built
    }

    /// How many builds it keeps, and their blobs' bytes.
    pub(crate) fn kept(&self) -> (usize, u64) {
        let kept = self.0.lock().unwrap_or_else(|e| e.into_inner());
        (kept.builds.len(), kept.bytes)
    }
}

impl std::fmt::Debug for BlobShapes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (builds, bytes) = self.kept();
        f.debug_struct("BlobShapes").field("builds", &builds).field("bytes", &bytes).finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hull(x: f32) -> Vec<u8> {
        let mut b = b"PEH1".to_vec();
        b.extend_from_slice(&4u32.to_le_bytes());
        for p in [[0.0, 0.0, 0.0], [x, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]] {
            p.iter().for_each(|c: &f32| b.extend_from_slice(&c.to_le_bytes()));
        }
        b
    }

    fn hash(bytes: &[u8]) -> [u8; 32] {
        pe_core::Hash::plain(bytes).0
    }

    /// Each blob is built once and kept by how it is read; past the budget the least recently used goes, never the
    /// build just made, and a build let go is built again, the same, when asked for.
    #[test]
    fn a_build_is_kept_until_the_budget_lets_the_least_recently_used_go() {
        let blobs: Vec<Vec<u8>> = (1..=3).map(|k| hull(k as f32)).collect();
        let len = blobs[0].len() as u64;
        let cache = BlobShapes::with_budget(2 * len);
        let get = |k: usize, read| cache.get(read, hash(&blobs[k]), &blobs[k]);
        assert!(matches!(get(0, Read::Hull), Built::Hull(p) if p[1] == [1.0, 0.0, 0.0]));
        assert!(matches!(get(0, Read::Mesh), Built::Bad), "a hull blob read as a mesh, kept apart");
        assert_eq!(cache.kept(), (2, 2 * len));
        let _ = get(0, Read::Hull);
        let _ = get(1, Read::Hull);
        // The mesh reading of blob 0 was the least recently used.
        assert_eq!(cache.kept(), (2, 2 * len));
        let held = cache.0.lock().unwrap().builds.keys().copied().collect::<Vec<_>>();
        assert!(held.contains(&(Read::Hull, hash(&blobs[0]))) && held.contains(&(Read::Hull, hash(&blobs[1]))));
        let _ = get(2, Read::Hull);
        let held = cache.0.lock().unwrap().builds.keys().copied().collect::<Vec<_>>();
        assert!(!held.contains(&(Read::Hull, hash(&blobs[0]))), "blob 0 went: {held:?}");
        assert!(matches!(get(0, Read::Hull), Built::Hull(p) if p[1] == [1.0, 0.0, 0.0]), "built again, the same");
        let alone = BlobShapes::with_budget(1);
        assert!(matches!(alone.get(Read::Hull, hash(&blobs[2]), &blobs[2]), Built::Hull(_)));
        assert_eq!(alone.kept(), (1, len), "the build just made stays, over the budget");
    }
}
