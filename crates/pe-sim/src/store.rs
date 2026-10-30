use std::collections::BTreeMap;
use std::sync::Arc;

use pe_core::{Error, Frame, MemberId, ObjectId};

use crate::column::Column;
use crate::hierarchy::Node;
use crate::registry::{Component, ComponentDesc, ComponentId, Registry};
use crate::transform::Transform;

/// The sim state of one world (design 16, 6.1): its frame, its objects, the next object counter of every
/// creator, and one sorted column per component.
///
/// Everything here is rolled back, hashed and sync-checked. The registry is configuration: it is shared,
/// not snapshotted.
///
/// **Writes through [`SimStore::get_mut`] and [`SimStore::column_mut`] are not validated.** Inserts
/// are. Sim code must not write NaN (4.3 rule 6); [`SimStore::check_values`] finds any that got in, and
/// runs at check frames (4.3 rule 9).
#[derive(Clone, Debug)]
pub struct SimStore {
    pub(crate) registry: Arc<Registry>,
    pub(crate) frame: Frame,
    pub(crate) seed: u64,
    /// Live objects, sorted.
    pub(crate) objects: Arc<Vec<ObjectId>>,
    /// Per creator, sorted by creator: `ObjectId { creator, counter: last counter allocated }`.
    pub(crate) counters: Arc<Vec<ObjectId>>,
    pub(crate) columns: BTreeMap<ComponentId, Column>,
}

impl SimStore {
    /// An empty world at frame 0. `seed` is the session seed that [`TickCtx::rng`](crate::TickCtx::rng)
    /// streams derive from.
    pub fn new(registry: Arc<Registry>, seed: u64) -> Self {
        SimStore {
            registry,
            frame: Frame::ZERO,
            seed,
            objects: Arc::new(Vec::new()),
            counters: Arc::new(Vec::new()),
            columns: BTreeMap::new(),
        }
    }

    /// A copy that shares no buffer with this store or its snapshots (tools and benchmarks; a plain
    /// `clone` shares buffers copy-on-write, which is equivalent and cheaper).
    pub fn deep_clone(&self) -> SimStore {
        let mut c = self.clone();
        c.objects = Arc::new(self.objects.to_vec());
        c.counters = Arc::new(self.counters.to_vec());
        for col in c.columns.values_mut() {
            col.ids = Arc::new(col.ids.to_vec());
            col.data = Arc::new(col.data.to_vec());
        }
        c
    }

    pub fn registry(&self) -> &Arc<Registry> {
        &self.registry
    }

    pub fn frame(&self) -> Frame {
        self.frame
    }

    /// Sets the frame counter. The schedule advances it; a state transfer or a scene load may set it.
    pub fn set_frame(&mut self, f: Frame) {
        self.frame = f;
    }

    pub fn seed(&self) -> u64 {
        self.seed
    }

    // ---- objects ---------------------------------------------------------------------------------

    /// Live objects, in id order.
    pub fn objects(&self) -> &[ObjectId] {
        &self.objects
    }

    pub fn exists(&self, id: ObjectId) -> bool {
        self.objects.binary_search(&id).is_ok()
    }

    /// The last counter `creator` has used (0 if none).
    pub fn last_counter(&self, creator: MemberId) -> u32 {
        match self.counters.binary_search_by_key(&creator, |c| c.creator) {
            Ok(i) => self.counters[i].counter,
            Err(_) => 0,
        }
    }

    fn bump_counter(&mut self, id: ObjectId) {
        let counters = Arc::make_mut(&mut self.counters);
        match counters.binary_search_by_key(&id.creator, |c| c.creator) {
            Ok(i) => counters[i].counter = counters[i].counter.max(id.counter),
            Err(i) => counters.insert(i, id),
        }
    }

    /// Creates an object with the next id of `creator`, as a root with no components but its [`Node`].
    pub fn create(&mut self, creator: MemberId) -> Result<ObjectId, Error> {
        let next = self.last_counter(creator).checked_add(1).ok_or(Error::CountersExhausted { creator })?;
        let id = ObjectId::new(creator, next);
        self.create_with_id(id)?;
        Ok(id)
    }

    /// Creates an object with a given id (scene content, a state transfer, R14 arrivals). The creator's
    /// counter moves past it, so [`SimStore::create`] never reissues it.
    pub fn create_with_id(&mut self, id: ObjectId) -> Result<(), Error> {
        if id.is_none() {
            return Err(Error::UnknownObject(id));
        }
        let at = match self.objects.binary_search(&id) {
            Ok(_) => return Err(Error::DuplicateObject(id)),
            Err(at) => at,
        };
        Arc::make_mut(&mut self.objects).insert(at, id);
        self.bump_counter(id);
        self.put_row(ComponentId::NODE, id, bytemuck::bytes_of(&Node::ROOT));
        Ok(())
    }

    /// Destroys an object and its whole subtree. Every component row of every destroyed object goes.
    pub fn destroy(&mut self, id: ObjectId) -> Result<(), Error> {
        if !self.exists(id) {
            return Err(Error::UnknownObject(id));
        }
        let doomed = self.subtree(id);
        self.unlink(id);
        let objects = Arc::make_mut(&mut self.objects);
        for &d in &doomed {
            if let Ok(i) = objects.binary_search(&d) {
                objects.remove(i);
            }
        }
        for col in self.columns.values_mut() {
            for &d in &doomed {
                if let Ok(i) = col.find(d) {
                    col.remove(i);
                }
            }
        }
        Ok(())
    }

    // ---- columns ---------------------------------------------------------------------------------

    fn desc(&self, c: ComponentId) -> Result<&ComponentDesc, Error> {
        self.registry.get(c).ok_or(Error::UnknownComponent(c.0))
    }

    /// Inserts or overwrites a row without checks (the caller has validated it).
    pub(crate) fn put_row(&mut self, c: ComponentId, id: ObjectId, row: &[u8]) {
        let size = row.len();
        let col = self.columns.entry(c).or_insert_with(|| Column::new(size));
        match col.find(id) {
            Ok(i) => col.row_mut(i).copy_from_slice(row),
            Err(i) => col.insert(i, id, row),
        }
    }

    /// Removes a row without checks.
    pub(crate) fn drop_row(&mut self, c: ComponentId, id: ObjectId) -> bool {
        let Some(col) = self.columns.get_mut(&c) else { return false };
        match col.find(id) {
            Ok(i) => {
                col.remove(i);
                true
            }
            Err(_) => false,
        }
    }

    pub(crate) fn row(&self, c: ComponentId, id: ObjectId) -> Option<&[u8]> {
        let col = self.columns.get(&c)?;
        col.find(id).ok().map(|i| col.row(i))
    }

    pub(crate) fn row_mut(&mut self, c: ComponentId, id: ObjectId) -> Option<&mut [u8]> {
        let col = self.columns.get_mut(&c)?;
        let i = col.find(id).ok()?;
        Some(col.row_mut(i))
    }

    /// Inserts or overwrites a component row given as raw little-endian bytes (logic modules, MCP, scene
    /// loading). Validated: the object exists, the size matches and the values are legal; a `Transform` is
    /// also checked against its limits (see [`crate::transform`]). A data edit: the row is stored as given,
    /// so the result does not depend on the order rows are inserted in. (Whole states travel as snapshots.)
    pub fn insert_raw(&mut self, c: ComponentId, id: ObjectId, row: &[u8]) -> Result<(), Error> {
        if c == ComponentId::NODE {
            return Err(Error::Reserved { component: c.0 });
        }
        let desc = self.desc(c)?;
        if row.len() != desc.size() {
            return Err(Error::WrongSize { component: c.0, expected: desc.size(), got: row.len() });
        }
        if let Some(field) = desc.invalid_field(row) {
            return Err(Error::InvalidValue { component: c.0, object: id, field: field.to_string() });
        }
        if !self.exists(id) {
            return Err(Error::UnknownObject(id));
        }
        if c == ComponentId::TRANSFORM {
            return self.insert_transform(id, bytemuck::pod_read_unaligned::<Transform>(row));
        }
        self.put_row(c, id, row);
        Ok(())
    }

    pub fn get_raw(&self, c: ComponentId, id: ObjectId) -> Option<&[u8]> {
        self.row(c, id)
    }

    /// Removes a component row. Returns whether there was one.
    pub fn remove_raw(&mut self, c: ComponentId, id: ObjectId) -> Result<bool, Error> {
        if c == ComponentId::NODE {
            return Err(Error::Reserved { component: c.0 });
        }
        self.desc(c)?;
        if c == ComponentId::TRANSFORM {
            return self.remove_transform(id);
        }
        Ok(self.drop_row(c, id))
    }

    /// Inserts or overwrites a typed component (validated, like [`SimStore::insert_raw`]).
    pub fn insert<T: Component>(&mut self, id: ObjectId, value: T) -> Result<(), Error> {
        self.insert_raw(T::ID, id, bytemuck::bytes_of(&value))
    }

    pub fn get<T: Component>(&self, id: ObjectId) -> Option<&T> {
        self.row(T::ID, id).map(bytemuck::from_bytes)
    }

    /// Writable access, not validated (see the type docs). The hierarchy's [`Node`] is not writable.
    pub fn get_mut<T: Component>(&mut self, id: ObjectId) -> Option<&mut T> {
        assert!(T::ID != ComponentId::NODE, "Node is managed by the hierarchy API");
        self.row_mut(T::ID, id).map(bytemuck::from_bytes_mut)
    }

    pub fn remove<T: Component>(&mut self, id: ObjectId) -> Result<bool, Error> {
        self.remove_raw(T::ID, id)
    }

    pub fn has<T: Component>(&self, id: ObjectId) -> bool {
        self.row(T::ID, id).is_some()
    }

    /// The ids and rows of a component, in id order.
    pub fn column<T: Component>(&self) -> (&[ObjectId], &[T]) {
        match self.columns.get(&T::ID) {
            Some(col) => (&col.ids, bytemuck::cast_slice(col.bytes())),
            None => (&[], &[]),
        }
    }

    /// The ids and writable rows of a component, in id order (not validated). Copies the column first if a
    /// snapshot shares it.
    pub fn column_mut<T: Component>(&mut self) -> (&[ObjectId], &mut [T]) {
        assert!(T::ID != ComponentId::NODE, "Node is managed by the hierarchy API");
        match self.columns.get_mut(&T::ID) {
            Some(col) => {
                let n = col.ids.len() * col.size;
                // Disjoint fields: the ids are read, the data buffer is written.
                let ids: &[ObjectId] = &col.ids;
                let data: &mut Vec<u64> = Arc::make_mut(&mut col.data);
                let bytes = &mut bytemuck::cast_slice_mut::<u64, u8>(data.as_mut_slice())[..n];
                (ids, bytemuck::cast_slice_mut(bytes))
            }
            None => (&[], &mut []),
        }
    }

    pub fn iter<T: Component>(&self) -> impl Iterator<Item = (ObjectId, &T)> {
        let (ids, rows) = self.column::<T>();
        ids.iter().copied().zip(rows.iter())
    }

    /// Rows in a component's column.
    pub fn count(&self, c: ComponentId) -> usize {
        self.columns.get(&c).map_or(0, Column::len)
    }

    /// Checks every row of every column for NaN, infinities and bad bools, and every `Transform` against its
    /// limits (4.3 rules 6 and 9). Returns the first offender in (component, object) order.
    pub fn check_values(&self) -> Result<(), Error> {
        for (&c, col) in &self.columns {
            let desc = self.desc(c)?;
            for (i, &id) in col.ids.iter().enumerate() {
                if let Some(field) = desc.invalid_field(col.row(i)) {
                    return Err(Error::InvalidValue { component: c.0, object: id, field: field.to_string() });
                }
            }
        }
        let flags = self.transform_frame_flags();
        for ((id, t), has_frame) in self.iter::<Transform>().zip(flags) {
            if let Err(field) = t.validate(has_frame) {
                return Err(Error::InvalidValue { component: ComponentId::TRANSFORM.0, object: id, field: field.to_string() });
            }
        }
        Ok(())
    }

    /// [`SimStore::check_values`], then [`SimStore::state_hash`]. Sync checks use this (review P21-5): a NaN
    /// is a deterministic error on every peer, never a hash of platform-specific NaN bits.
    pub fn checked_state_hash(&self) -> Result<u64, Error> {
        self.check_values()?;
        Ok(self.state_hash())
    }
}
