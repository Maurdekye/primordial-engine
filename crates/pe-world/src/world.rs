//! One world's state (world-model.md 5.1): pe-sim's store of objects and component columns, the world tables, and the
//! machine's blob store.
//!
//! This first slice has the trusted construction calls (genesis and tests) and the published snapshot (`read`).
//! Entries reach the state through `apply` (section 7), which comes next and uses these same calls.

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

use pe_core::{EntryRef, Frame, Hash, MemberKey, ObjectId};
use pe_sim::{Component, Node, SimStore};

use crate::components::{BlobRef, engine_registry};
use crate::read::WorldSnapshot;

/// The default hierarchy depth limit (3.3): an object may be at most this many levels below its root.
pub const DEPTH_LIMIT: u32 = 256;
/// The largest depth limit a world may set (3.3).
pub const DEPTH_LIMIT_MAX: u32 = 4096;

/// A row's or an object's version (7.3): the edit that last wrote it and the op's index in its transaction.
/// [`Version::GENESIS`] (all zero) means "never written by an edit".
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Version {
    pub entry: EntryRef,
    pub op: u32,
}

impl Version {
    pub const GENESIS: Version = Version { entry: EntryRef(Hash::ZERO), op: 0 };
}

/// An exclusive hold (7.2): `hand`, owned by `member`, holds the object since `since`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Claim {
    pub hand: ObjectId,
    pub member: MemberKey,
    pub since: Frame,
}

/// Why a world operation was refused. Nothing was changed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WorldError {
    /// The object does not exist (or the id is `ObjectId::NONE`).
    UnknownObject(ObjectId),
    /// An object with this id already exists (2.1: an id collision is a refusal).
    Duplicate(ObjectId),
    /// The object would sit deeper than the world's depth limit (3.3).
    TooDeep { object: ObjectId, limit: u32 },
    /// pe-sim refused the write (an invalid value, an unknown component, a cycle).
    Store(pe_core::Error),
}

impl From<pe_core::Error> for WorldError {
    fn from(e: pe_core::Error) -> Self {
        WorldError::Store(e)
    }
}

/// The machine's blobs (3.4), keyed by hash and shared by every world and snapshot on the machine. Bytes are
/// checked against their reference on the way in, so a stored blob always matches its name.
#[derive(Clone, Debug, Default)]
pub struct BlobStore(Arc<RwLock<Blobs>>);

type Blobs = BTreeMap<[u8; 32], Arc<[u8]>>;

impl BlobStore {
    /// Stores `bytes` and returns their reference.
    pub fn put(&self, bytes: &[u8]) -> BlobRef {
        let r = BlobRef::of(bytes);
        let mut m = self.0.write().unwrap_or_else(|e| e.into_inner());
        m.entry(r.hash).or_insert_with(|| Arc::from(bytes));
        r
    }

    /// Stores bytes that arrived for `r`, if they match it. Returns whether they did.
    pub fn put_checked(&self, r: &BlobRef, bytes: &[u8]) -> bool {
        if r.is_none() || BlobRef::of(bytes) != *r {
            return false;
        }
        self.put(bytes);
        true
    }

    /// The bytes of `r`, if this machine has them. `None` for [`BlobRef::NONE`].
    pub fn get(&self, r: &BlobRef) -> Option<Arc<[u8]>> {
        if r.is_none() {
            return None;
        }
        let m = self.0.read().unwrap_or_else(|e| e.into_inner());
        m.get(&r.hash).filter(|b| b.len() as u64 == r.len).cloned()
    }
}

/// One world.
#[derive(Clone, Debug)]
pub struct World {
    pub(crate) store: SimStore,
    pub(crate) claims: Arc<BTreeMap<ObjectId, Claim>>,
    pub(crate) blobs: BlobStore,
    pub(crate) depth_limit: u32,
}

impl World {
    /// An empty world at frame 0 with the engine's registry (3.2), using the machine's blob store.
    pub fn new(seed: u64, blobs: BlobStore) -> World {
        World {
            store: SimStore::new(Arc::new(engine_registry()), seed),
            claims: Arc::new(BTreeMap::new()),
            blobs,
            depth_limit: DEPTH_LIMIT,
        }
    }

    pub fn frame(&self) -> Frame {
        self.store.frame()
    }

    pub fn set_frame(&mut self, f: Frame) {
        self.store.set_frame(f);
    }

    /// The object store, read-only.
    pub fn store(&self) -> &SimStore {
        &self.store
    }

    pub fn blobs(&self) -> &BlobStore {
        &self.blobs
    }

    pub fn depth_limit(&self) -> u32 {
        self.depth_limit
    }

    /// Sets the depth limit, clamped to `1..=DEPTH_LIMIT_MAX`. Lowering it moves and refuses nothing already deeper
    /// (3.3).
    pub fn set_depth_limit(&mut self, limit: u32) {
        self.depth_limit = limit.clamp(1, DEPTH_LIMIT_MAX);
    }

    /// Creates an object under `parent` (last among its children), or as a root. Trusted: genesis, tests, and
    /// `apply` once it has checked the entry. The object and structural versions both start at `version`.
    pub fn create(
        &mut self,
        id: ObjectId,
        parent: Option<ObjectId>,
        creator: MemberKey,
        version: Version,
    ) -> Result<(), WorldError> {
        if id.is_none() {
            return Err(WorldError::UnknownObject(id));
        }
        if self.store.exists(id) {
            return Err(WorldError::Duplicate(id));
        }
        if let Some(p) = parent {
            if !self.store.exists(p) {
                return Err(WorldError::UnknownObject(p));
            }
            if depth(&self.store, p) >= self.depth_limit {
                return Err(WorldError::TooDeep { object: id, limit: self.depth_limit });
            }
        }
        self.store.create(id)?;
        if let Some(p) = parent
            && let Err(e) = self.store.set_parent(id, Some(p), None)
        {
            // Unreachable for a fresh root under an existing parent; undo so a refusal changes nothing.
            let _ = self.store.destroy(id);
            return Err(e.into());
        }
        let record = Node {
            creator,
            version_entry: version.entry.0.0,
            version_op: version.op,
            structural_entry: version.entry.0.0,
            structural_op: version.op,
            ..Node::ROOT
        };
        self.store.set_node_record(id, &record);
        Ok(())
    }

    /// Inserts or replaces one component row (validated by pe-sim). Trusted, like [`World::create`].
    pub fn insert<T: Component>(&mut self, id: ObjectId, row: T) -> Result<(), WorldError> {
        if !self.store.exists(id) {
            return Err(WorldError::UnknownObject(id));
        }
        Ok(self.store.insert(id, row)?)
    }

    /// Records that `claim.hand` holds `object`, replacing any hold on it.
    pub fn set_claim(&mut self, object: ObjectId, claim: Claim) -> Result<(), WorldError> {
        if !self.store.exists(object) {
            return Err(WorldError::UnknownObject(object));
        }
        Arc::make_mut(&mut self.claims).insert(object, claim);
        Ok(())
    }

    pub fn clear_claim(&mut self, object: ObjectId) {
        if self.claims.contains_key(&object) {
            Arc::make_mut(&mut self.claims).remove(&object);
        }
    }

    /// The published view of this frame (S6 reads it, api.md 9.1): structural sharing, so it costs a reference per
    /// column and table, and its state never changes while held (only a blob arriving in the shared [`BlobStore`] can
    /// turn a `None` name or blob into `Some`). `final_frame` is the newest final frame the caller knows.
    pub fn snapshot(&self, final_frame: Frame) -> Arc<WorldSnapshot> {
        Arc::new(WorldSnapshot {
            store: self.store.clone(),
            claims: self.claims.clone(),
            blobs: self.blobs.clone(),
            final_frame,
        })
    }
}

/// How many levels `id` is below its root (a root is at 0). Iterative, and bounded past the largest limit, so a
/// corrupt parent chain cannot loop or overflow a stack (3.3).
pub(crate) fn depth(store: &SimStore, id: ObjectId) -> u32 {
    let mut d = 0;
    let mut at = id;
    while let Some(p) = store.parent(at) {
        d += 1;
        if d > DEPTH_LIMIT_MAX {
            break;
        }
        at = p;
    }
    d
}
