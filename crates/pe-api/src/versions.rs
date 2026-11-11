//! The per-connection table of versions seen (spec 4.2): the default `if_version` of an op that names an object is
//! the version this connection last saw for it. Bounded; the oldest entries are dropped first, and a dropped object
//! falls back to the base snapshot's version.

use std::collections::{HashMap, VecDeque};

use pe_seams::world_api::{ObjectId, Version};

pub const DEFAULT_CAPACITY: usize = 100_000;

pub struct VersionTable {
    cap: usize,
    /// The version seen, the frame it was seen at, and the stamp of its latest insertion.
    map: HashMap<ObjectId, (Version, u64, u64)>,
    /// Insertion order; entries whose stamp no longer matches the map are stale and skipped.
    order: VecDeque<(ObjectId, u64)>,
    stamp: u64,
}

impl VersionTable {
    pub fn new(cap: usize) -> Self {
        assert!(cap > 0);
        VersionTable { cap, map: HashMap::new(), order: VecDeque::new(), stamp: 0 }
    }

    /// Records the version of `id` seen at `frame` (from a query, a watch or an apply result; an apply result's frame
    /// is the frame its change takes effect in). A sighting from an older frame than the one stored is ignored: a
    /// query at `"final"` or at an older retained frame, run after an apply, must not put back the version before
    /// the agent's own change, or its next default `if_version` would fail against that change (spec 4.2). At the
    /// same frame the later sighting wins.
    pub fn saw(&mut self, id: ObjectId, v: Version, frame: u64) {
        if self.map.get(&id).is_some_and(|(_, f, _)| *f > frame) {
            return;
        }
        self.stamp += 1;
        self.map.insert(id, (v, frame, self.stamp));
        self.order.push_back((id, self.stamp));
        while self.map.len() > self.cap {
            let (old, s) = self.order.pop_front().expect("order covers the map");
            if self.map.get(&old).is_some_and(|(_, _, cur)| *cur == s) {
                self.map.remove(&old);
            }
        }
        // Keep the order queue from growing without bound under repeated sightings of the same ids.
        if self.order.len() > 2 * self.cap {
            let map = &self.map;
            self.order.retain(|(id, s)| map.get(id).is_some_and(|(_, _, cur)| cur == s));
        }
    }

    /// Forgets an object (deleted).
    pub fn forget(&mut self, id: &ObjectId) {
        self.map.remove(id);
    }

    pub fn get(&self, id: &ObjectId) -> Option<&Version> {
        self.map.get(id).map(|(v, _, _)| v)
    }

    /// The default precondition for an op on `id`: what this connection saw, else `base` (the base snapshot's).
    pub fn default_for(&self, id: &ObjectId, base: impl FnOnce() -> Option<Version>) -> Option<Version> {
        self.get(id).cloned().or_else(base)
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

impl Default for VersionTable {
    fn default() -> Self {
        Self::new(DEFAULT_CAPACITY)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(n: u8) -> ObjectId {
        ObjectId([n; 16])
    }
    fn v(s: &str) -> Version {
        Version(s.into())
    }

    #[test]
    fn newer_sightings_replace_older() {
        let mut t = VersionTable::new(4);
        t.saw(id(1), v("v:a"), 10);
        t.saw(id(1), v("v:b"), 11);
        assert_eq!(t.get(&id(1)), Some(&v("v:b")));
        assert_eq!(t.len(), 1);
    }

    #[test]
    fn the_oldest_is_dropped_first_and_falls_back_to_base() {
        let mut t = VersionTable::new(2);
        t.saw(id(1), v("v:1"), 1);
        t.saw(id(2), v("v:2"), 1);
        t.saw(id(1), v("v:1b"), 2); // id 1 is now the newest
        t.saw(id(3), v("v:3"), 2); // drops id 2, the oldest
        assert_eq!(t.get(&id(2)), None);
        assert_eq!(t.get(&id(1)), Some(&v("v:1b")));
        assert_eq!(t.default_for(&id(2), || Some(v("v:base"))), Some(v("v:base")));
        assert_eq!(t.default_for(&id(3), || Some(v("v:base"))), Some(v("v:3")));
    }

    #[test]
    fn the_order_queue_stays_bounded() {
        let mut t = VersionTable::new(3);
        for i in 0..10_000u32 {
            t.saw(id((i % 3) as u8), v(&format!("v:{i}")), i as u64);
        }
        assert_eq!(t.len(), 3);
        assert!(t.order.len() <= 6);
    }

    #[test]
    fn an_older_frame_does_not_put_back_an_older_version() {
        let mut t = VersionTable::new(4);
        t.saw(id(1), v("v:before"), 100); // a query at latest
        t.saw(id(1), v("v:mine"), 101); // the agent's own apply, in effect at frame 101
        t.saw(id(1), v("v:before"), 98); // then a query at "final", three frames behind
        assert_eq!(t.default_for(&id(1), || None), Some(v("v:mine")));
        t.saw(id(1), v("v:later"), 101); // the same frame: the later sighting wins
        assert_eq!(t.get(&id(1)), Some(&v("v:later")));
    }

    #[test]
    fn forgetting_a_deleted_object() {
        let mut t = VersionTable::new(3);
        t.saw(id(1), v("v:1"), 1);
        t.forget(&id(1));
        assert!(t.is_empty());
    }
}
