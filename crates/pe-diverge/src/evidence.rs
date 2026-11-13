//! What a fault bundle needs to replay offline (divergence.md 6): `Cv`'s cut and state, so the bundle imports into an
//! empty replica, and the entries from `Cv` up to the cut that diverged.
//!
//! The ladder captures it when rung 2 first runs after `Cv` moved. `Cv`'s state and the entries from it are still
//! kept then: the replica holds `Cv` until a later `finalize` agrees (S2), and S3 keeps the entries from `Cv` up
//! (S3 v1.2). The import that cures moves `Cv` past them, so the embedding could not fetch them afterwards.

use bytes::Bytes;
use pe_seams::{
    ChainHead, ChainId, Cut, DomainId, EntryRef, Frame, Hash, LeafKey, MemberKey, Nibbles, Node, PhysicsImage, Record,
    Tree, TreePath,
};

use crate::msg::{DecodeError, EncodeError, MAX_IMAGE, Reader, put_bytes, put_cut, put_record};

/// The format of [`CvState`]'s bytes.
pub const STATE_VERSION: u8 = 1;

/// `Cv`, and the whole state there: what `import_at_cut` takes on an empty replica.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CvState {
    /// `Cv`'s cut, with its heads.
    pub cv: Cut,
    /// `Cv`'s root: every record checks against it.
    pub root: Hash,
    /// The world's rate: a replica is built at it before the import (S2 v1.4, `RateMismatch`).
    pub rate_hz: u32,
    /// Every record at `Cv`, each with its proof: the config, the tables, the objects.
    pub records: Vec<Record>,
    /// The physics image at `Cv`, for bound bodies.
    pub physics: Option<PhysicsImage>,
}

impl CvState {
    pub fn encode(&self) -> Result<Bytes, EncodeError> {
        let mut w = vec![STATE_VERSION];
        put_cut(&mut w, &self.cv.at);
        put_u32(&mut w, self.cv.heads.len())?;
        for h in &self.cv.heads {
            w.extend_from_slice(&h.chain.0.0);
            w.extend_from_slice(&h.author.0);
            w.extend_from_slice(&h.seq.to_le_bytes());
            w.extend_from_slice(&h.entry.0.0);
        }
        w.extend_from_slice(&self.root.0);
        w.extend_from_slice(&self.rate_hz.to_le_bytes());
        put_u32(&mut w, self.records.len())?;
        for r in &self.records {
            put_record(&mut w, r)?;
        }
        match &self.physics {
            None => w.push(0),
            Some(image) => {
                if image.bytes.len() > MAX_IMAGE {
                    return Err(EncodeError::TooLong);
                }
                w.push(1);
                w.extend_from_slice(&image.domain.0.to_le_bytes());
                w.extend_from_slice(&image.frame.0.to_le_bytes());
                w.extend_from_slice(&image.global.0);
                put_bytes(&mut w, &image.bytes);
            }
        }
        Ok(Bytes::from(w))
    }

    /// Never panics, and never allocates more than the input could hold.
    pub fn decode(b: &[u8]) -> Result<CvState, DecodeError> {
        let mut r = Reader { b };
        if r.u8()? != STATE_VERSION {
            return Err(DecodeError::Version);
        }
        let at = r.cut()?;
        let n = bounded(&mut r, 32 + 32 + 8 + 32)?;
        let mut heads = Vec::with_capacity(n);
        for _ in 0..n {
            heads.push(ChainHead {
                chain: ChainId(r.hash()?),
                author: MemberKey(r.hash()?.0),
                seq: r.u64()?,
                entry: EntryRef(r.hash()?),
            });
        }
        let root = r.hash()?;
        let rate_hz = r.u32()? as u32;
        let n = bounded(&mut r, 1)?; // a record is at least its key's one byte
        let mut records = Vec::with_capacity(n);
        for _ in 0..n {
            records.push(r.record()?);
        }
        let physics = match r.u8()? {
            0 => None,
            1 => Some(PhysicsImage {
                domain: DomainId(r.u64()?),
                frame: Frame(r.u64()?),
                global: r.hash()?,
                bytes: r.bytes(MAX_IMAGE)?,
            }),
            _ => return Err(DecodeError::Tag),
        };
        if !r.b.is_empty() {
            return Err(DecodeError::Trailing);
        }
        Ok(CvState { cv: Cut { at, heads }, root, rate_hz, records, physics })
    }
}

fn put_u32(w: &mut Vec<u8>, n: usize) -> Result<(), EncodeError> {
    let n = u32::try_from(n).map_err(|_| EncodeError::TooMany)?;
    w.extend_from_slice(&n.to_le_bytes());
    Ok(())
}

/// A u32 count of items each at least `min` bytes long, refused if the input cannot hold them.
fn bounded(r: &mut Reader<'_>, min: usize) -> Result<usize, DecodeError> {
    let n = r.u32()?;
    if n.saturating_mul(min) > r.b.len() {
        return Err(DecodeError::Truncated);
    }
    Ok(n)
}

/// Every leaf of `tree` that has a record: the config, each table, and each domain's objects. Physics has none; it
/// travels as the whole image (S2 `Tree::export`). `None` if a
/// node is missing or not the kind its path names: a tree the replica holds is whole, so that is no state to save.
pub(crate) fn all_keys(tree: &dyn Tree) -> Option<Vec<LeafKey>> {
    let Some(Node::World { domains, .. }) = tree.node(&TreePath::World) else {
        return None;
    };
    let mut keys = vec![LeafKey::Config];
    let Some(Node::Tables(tables)) = tree.node(&TreePath::Tables) else {
        return None;
    };
    for i in 0..tables.len() {
        keys.push(LeafKey::Table(u8::try_from(i).ok()?));
    }
    for (d, _) in &domains {
        objects(tree, *d, Nibbles::default(), &mut keys)?;
    }
    Some(keys)
}

/// The objects under `prefix`, depth first in nibble order. The depth is at most 32 nibbles.
fn objects(tree: &dyn Tree, domain: DomainId, prefix: Nibbles, keys: &mut Vec<LeafKey>) -> Option<()> {
    match tree.node(&TreePath::Objects { domain, prefix })? {
        Node::Inner { prefix, children } => {
            for (n, _) in children {
                objects(tree, domain, prefix.child(n)?, keys)?;
            }
        }
        Node::Leaf { id, .. } => keys.push(LeafKey::Object(id)),
        Node::Empty => {}
        _ => return None,
    }
    Some(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use pe_seams::{CutRef, LineId, ObjectId};

    #[test]
    fn a_state_round_trips_and_no_prefix_of_it_panics() {
        let s = CvState {
            cv: Cut {
                at: CutRef { line: LineId(Hash([1; 32])), height: 3, frame: Frame(24) },
                heads: vec![ChainHead {
                    chain: ChainId(Hash([2; 32])),
                    author: MemberKey([3; 32]),
                    seq: 7,
                    entry: EntryRef(Hash([4; 32])),
                }],
            },
            root: Hash([5; 32]),
            rate_hz: 60,
            records: vec![
                Record { key: LeafKey::Config, bytes: Bytes::from_static(b"cfg"), proof: vec![Node::Empty] },
                Record {
                    key: LeafKey::Object(ObjectId::from_u128(9)),
                    bytes: Bytes::from_static(&[1, 2]),
                    proof: vec![],
                },
            ],
            physics: Some(PhysicsImage {
                domain: DomainId(1),
                frame: Frame(24),
                global: Hash([6; 32]),
                bytes: Bytes::from_static(&[7; 5]),
            }),
        };
        let e = s.encode().unwrap();
        assert_eq!(CvState::decode(&e), Ok(s.clone()));
        for n in 0..e.len() {
            assert!(CvState::decode(&e[..n]).is_err());
        }
        let mut long = e.to_vec();
        long.push(0);
        assert_eq!(CvState::decode(&long), Err(DecodeError::Trailing));
        let none = CvState { physics: None, records: Vec::new(), ..s };
        assert_eq!(CvState::decode(&none.encode().unwrap()), Ok(none));
    }
}
