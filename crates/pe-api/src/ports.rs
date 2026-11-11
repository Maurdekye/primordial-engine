//! The ports pe-api reads and writes through (api.md section 3, 11). pe-app implements them over pe-world's snapshot,
//! the simulation thread's edit queue, pe-render's capture and pe-logic's check; tests use in-memory stand-ins.
//!
//! The snapshot port is deliberately narrow: what `query`, selector resolution and the default `if_version` need,
//! read from one published frame. Field values come out as the JSON forms of api.md 4.4, already converted by the
//! adapter (pe-world's rows and the schema stay on that side), with positions in the frame the caller names.

use std::collections::BTreeMap;
use std::sync::Arc;

use pe_seams::world_api::{Frame, Json, MemberId, ObjRef, ObjectId, Version};

/// One object as `query` reports it, without its component values.
#[derive(Clone, Debug, PartialEq)]
pub struct ObjInfo {
    pub id: ObjectId,
    pub name: String,
    pub parent: Option<ObjectId>,
    pub version: Version,
    pub creator: MemberId,
    pub held_by: Option<MemberId>,
    pub protected_by: Option<MemberId>,
    /// Component type names, instances after the first as `Type#n` (api.md 4.1).
    pub types: Vec<String>,
}

/// One published frame of the world, read-only (api.md 9.1). Implementations must be cheap to hold for one call
/// (structural sharing) and must give the same answers for as long as they are held.
pub trait Snapshot: Send + Sync {
    fn frame(&self) -> Frame;
    fn final_frame(&self) -> Frame;
    /// Root objects, in a stable order.
    fn roots(&self) -> Vec<ObjectId>;
    /// Every object, in a stable order (the order paging follows).
    fn all(&self) -> Vec<ObjectId>;
    fn info(&self, id: ObjectId) -> Option<ObjInfo>;
    /// Children in their order.
    fn children(&self, id: ObjectId) -> Vec<ObjectId>;
    /// One component's fields as JSON, positions given relative to `frame` (`"parent"`, an object, `"me"`,
    /// `"world_origin"`; a root's default is its exact `"pos"`, api.md 4.4).
    fn fields(&self, id: ObjectId, component: &str, frame: &ObjRef) -> Option<BTreeMap<String, Json>>;
    /// Distance in metres between two objects' world positions, or from the caller's own head for `"me"`
    /// (`near`, api.md 5.2). None when either is unknown.
    fn distance(&self, a: ObjectId, b: &ObjRef) -> Option<f64>;
}

/// Where snapshots come from: the newest published frame, the newest final one, or one still retained.
pub trait SnapshotSource: Send + Sync {
    fn latest(&self) -> Arc<dyn Snapshot>;
    fn final_(&self) -> Arc<dyn Snapshot>;
    fn at(&self, frame: Frame) -> Option<Arc<dyn Snapshot>>;
}
