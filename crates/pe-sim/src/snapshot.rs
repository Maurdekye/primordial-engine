//! Snapshot, restore, serialization and the state hash (design 6.1, 9.1, 9.5).
//!
//! * A [`Snapshot`] shares the store's copy-on-write buffers, so taking one costs one reference per
//!   column. The cost moves to the first write of each column afterwards, which copies that column once
//!   (the "memcpy of columns" of 6.1, paid only for columns that change).
//! * [`SimStore::restore`] puts the snapshot's buffers back: also one reference per column.
//! * [`Snapshot::to_bytes`] is the canonical serialization for state transfer and keyframes on disk;
//!   [`Snapshot::from_bytes`] validates everything it reads (sorted ids, schema, sizes, values), because the
//!   bytes may come from another peer.
//! * [`SimStore::state_hash`] hashes the canonical content, so it equals [`Snapshot::state_hash`] of a
//!   snapshot of the same state and of its serialized round trip. Empty columns are skipped, so a column
//!   that was emptied and one that never existed hash alike.

use std::sync::Arc;

use pe_core::hash::Hasher;
use pe_core::{Error, Frame, ObjectId};

use crate::column::Column;
use crate::registry::{ComponentId, Registry};
use crate::store::SimStore;

const MAGIC: &[u8; 8] = b"PESNAP01";
const HASH_SEED: u64 = 0x5045_5354_4154_4531; // "PESTATE1"

#[derive(Clone, Debug)]
struct SnapColumn {
    id: ComponentId,
    schema_hash: u64,
    col: Column,
}

/// A copy of a store's state at one frame.
#[derive(Clone, Debug)]
pub struct Snapshot {
    frame: Frame,
    seed: u64,
    objects: Arc<Vec<ObjectId>>,
    counters: Arc<Vec<ObjectId>>,
    columns: Vec<SnapColumn>,
}

fn hash_state<'a>(
    frame: Frame,
    seed: u64,
    objects: &[ObjectId],
    counters: &[ObjectId],
    columns: impl Iterator<Item = (ComponentId, u64, &'a Column)>,
) -> u64 {
    let mut h = Hasher::new(HASH_SEED);
    h.write_u64(frame.0);
    h.write_u64(seed);
    h.write_u64(objects.len() as u64);
    h.write(bytemuck::cast_slice(objects));
    h.write_u64(counters.len() as u64);
    h.write(bytemuck::cast_slice(counters));
    for (id, schema, col) in columns {
        if col.len() == 0 {
            continue;
        }
        h.write_u32(id.0);
        h.write_u64(schema);
        h.write_u64(col.len() as u64);
        h.write(bytemuck::cast_slice(&col.ids));
        h.write(col.bytes());
    }
    h.finish()
}

impl SimStore {
    /// A snapshot of the current state. O(columns): buffers are shared, not copied.
    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            frame: self.frame,
            seed: self.seed,
            objects: Arc::clone(&self.objects),
            counters: Arc::clone(&self.counters),
            columns: self
                .columns
                .iter()
                .filter(|(_, c)| c.len() > 0)
                .map(|(&id, col)| SnapColumn {
                    id,
                    schema_hash: self.registry.get(id).expect("column of a registered component").schema_hash(),
                    col: col.clone(),
                })
                .collect(),
        }
    }

    /// Restores a snapshot of this world. Fails, leaving the store unchanged, if a component's schema
    /// differs from the registry's.
    pub fn restore(&mut self, s: &Snapshot) -> Result<(), Error> {
        for c in &s.columns {
            match self.registry.get(c.id) {
                Some(d) if d.schema_hash() == c.schema_hash => {}
                _ => return Err(Error::SchemaMismatch { component: c.id.0 }),
            }
        }
        self.frame = s.frame;
        self.seed = s.seed;
        self.objects = Arc::clone(&s.objects);
        self.counters = Arc::clone(&s.counters);
        self.columns = s.columns.iter().map(|c| (c.id, c.col.clone())).collect();
        Ok(())
    }

    /// The state hash (9.1): frame, seed, objects, counters and every non-empty column with its schema.
    pub fn state_hash(&self) -> u64 {
        let reg = &self.registry;
        hash_state(
            self.frame,
            self.seed,
            &self.objects,
            &self.counters,
            self.columns.iter().map(|(&id, col)| (id, reg.get(id).map_or(0, |d| d.schema_hash()), col)),
        )
    }
}

impl Snapshot {
    pub fn frame(&self) -> Frame {
        self.frame
    }

    pub fn state_hash(&self) -> u64 {
        hash_state(
            self.frame,
            self.seed,
            &self.objects,
            &self.counters,
            self.columns.iter().map(|c| (c.id, c.schema_hash, &c.col)),
        )
    }

    /// Bytes of row data held (ids excluded): what a deep copy of this snapshot would copy.
    pub fn data_bytes(&self) -> usize {
        self.columns.iter().map(|c| c.col.bytes().len()).sum()
    }

    /// The canonical serialization (little-endian).
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(64 + self.objects.len() * 8 + self.columns.iter().map(|c| c.col.len() * (8 + c.col.size) + 24).sum::<usize>());
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&self.frame.0.to_le_bytes());
        out.extend_from_slice(&self.seed.to_le_bytes());
        out.extend_from_slice(&(self.objects.len() as u64).to_le_bytes());
        out.extend_from_slice(bytemuck::cast_slice(&self.objects));
        out.extend_from_slice(&(self.counters.len() as u64).to_le_bytes());
        out.extend_from_slice(bytemuck::cast_slice(&self.counters));
        out.extend_from_slice(&(self.columns.len() as u32).to_le_bytes());
        for c in &self.columns {
            out.extend_from_slice(&c.id.0.to_le_bytes());
            out.extend_from_slice(&c.schema_hash.to_le_bytes());
            out.extend_from_slice(&(c.col.size as u32).to_le_bytes());
            out.extend_from_slice(&(c.col.len() as u64).to_le_bytes());
            out.extend_from_slice(bytemuck::cast_slice(&c.col.ids));
            out.extend_from_slice(c.col.bytes());
        }
        out
    }

    /// Parses and validates [`Snapshot::to_bytes`] output against a registry.
    pub fn from_bytes(bytes: &[u8], registry: &Registry) -> Result<Snapshot, Error> {
        let mut r = Reader { b: bytes, at: 0 };
        if r.take(8)? != MAGIC {
            return Err(Error::Corrupt("bad magic"));
        }
        let frame = Frame(r.u64()?);
        let seed = r.u64()?;
        let objects = r.ids()?;
        if !strictly_sorted(&objects) || objects.iter().any(|o| o.is_none()) {
            return Err(Error::Corrupt("object table not sorted"));
        }
        let counters = r.ids()?;
        if !counters.windows(2).all(|w| w[0].creator < w[1].creator) {
            return Err(Error::Corrupt("counters not sorted"));
        }
        for o in &objects {
            let ok = counters
                .binary_search_by_key(&o.creator, |c| c.creator)
                .is_ok_and(|i| counters[i].counter >= o.counter);
            if !ok {
                return Err(Error::Corrupt("object id beyond its creator's counter"));
            }
        }
        let n = r.u32()? as usize;
        let mut columns: Vec<SnapColumn> = Vec::with_capacity(n.min(4096));
        for _ in 0..n {
            let id = ComponentId(r.u32()?);
            let schema_hash = r.u64()?;
            let size = r.u32()? as usize;
            let desc = registry.get(id).ok_or(Error::UnknownComponent(id.0))?;
            if desc.schema_hash() != schema_hash {
                return Err(Error::SchemaMismatch { component: id.0 });
            }
            if size != desc.size() {
                return Err(Error::WrongSize { component: id.0, expected: desc.size(), got: size });
            }
            if columns.last().is_some_and(|c| c.id >= id) {
                return Err(Error::Corrupt("columns not sorted"));
            }
            let len = r.u64()? as usize;
            let ids = r.ids_n(len)?;
            if len == 0 || !strictly_sorted(&ids) || ids.iter().any(|i| objects.binary_search(i).is_err()) {
                return Err(Error::Corrupt("column ids not sorted, empty, or not live objects"));
            }
            let data = r.take(len.checked_mul(size).ok_or(Error::Corrupt("size overflow"))?)?;
            for (i, row) in data.chunks_exact(size).enumerate() {
                if let Some(f) = desc.invalid_field(row) {
                    return Err(Error::InvalidValue { component: id.0, object: ids[i], field: f.to_string() });
                }
            }
            let mut words = vec![0u64; data.len().div_ceil(8)];
            bytemuck::cast_slice_mut::<u64, u8>(&mut words)[..data.len()].copy_from_slice(data);
            columns.push(SnapColumn { id, schema_hash, col: Column { size, ids: Arc::new(ids), data: Arc::new(words) } });
        }
        if r.at != bytes.len() {
            return Err(Error::Corrupt("trailing bytes"));
        }
        // Every live object has a hierarchy node.
        let nodes = columns.iter().find(|c| c.id == ComponentId::NODE);
        if nodes.map_or(0, |c| c.col.len()) != objects.len() {
            return Err(Error::Corrupt("objects without hierarchy nodes"));
        }
        Ok(Snapshot { frame, seed, objects: Arc::new(objects), counters: Arc::new(counters), columns })
    }
}

fn strictly_sorted(ids: &[ObjectId]) -> bool {
    ids.windows(2).all(|w| w[0] < w[1])
}

struct Reader<'a> {
    b: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], Error> {
        let end = self.at.checked_add(n).filter(|&e| e <= self.b.len()).ok_or(Error::Corrupt("truncated"))?;
        let s = &self.b[self.at..end];
        self.at = end;
        Ok(s)
    }

    fn u32(&mut self) -> Result<u32, Error> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }

    fn u64(&mut self) -> Result<u64, Error> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }

    fn ids_n(&mut self, n: usize) -> Result<Vec<ObjectId>, Error> {
        let raw = self.take(n.checked_mul(8).ok_or(Error::Corrupt("size overflow"))?)?;
        Ok(raw.chunks_exact(8).map(bytemuck::pod_read_unaligned::<ObjectId>).collect())
    }

    fn ids(&mut self) -> Result<Vec<ObjectId>, Error> {
        let n = self.u64()? as usize;
        self.ids_n(n)
    }
}
