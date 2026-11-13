//! The repair lane's messages (divergence.md 11.3), on S4's `Lane::Repair`.
//!
//! Unsigned: every record a reply carries is checked against the quorum-agreed root by the receiving replica's
//! `import_at_cut`, with its proof, before anything is applied. Decoding never panics and never allocates more than
//! the input could hold: any byte string is either a message or a [`DecodeError`] (plan 4.1).

use bytes::Bytes;
use pe_seams::{
    CutRef, DomainId, Frame, Hash, LeafKey, LineId, Nibbles, Node, ObjectId, PhysicsImage, Record, TreePath,
};

const VERSION: u8 = 1;
/// Most items in one message.
pub const MAX_ITEMS: usize = 4096;
/// Most children of one trie node: one per nibble.
pub const MAX_CHILDREN: usize = 16;
/// Largest record.
pub const MAX_RECORD: usize = 16 << 20;
/// Largest physics image (about 2.5 kB per body before compression [V file 02]: room for 25,000 bodies).
pub const MAX_IMAGE: usize = 64 << 20;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Msg {
    NodeRequest {
        cut: CutRef,
        root: Hash,
        paths: Vec<TreePath>,
    },
    NodeReply {
        cut: CutRef,
        root: Hash,
        nodes: Vec<(TreePath, Node)>,
    },
    RecordRequest {
        cut: CutRef,
        root: Hash,
        keys: Vec<LeafKey>,
    },
    RecordReply {
        cut: CutRef,
        root: Hash,
        records: Vec<Record>,
    },
    PhysicsRequest {
        cut: CutRef,
        root: Hash,
    },
    PhysicsReply {
        cut: CutRef,
        root: Hash,
        image: PhysicsImage,
    },
    /// The sender does not hold this root (released, or never held).
    NotHeld {
        cut: CutRef,
        root: Hash,
    },
    /// The sender holds the root but is over its serving budget for this peer: ask the same source again after
    /// `retry_after_us` microseconds. Never a fault, but a source that keeps answering it is rotated (divergence.md
    /// 4.2, step 2).
    Busy {
        cut: CutRef,
        root: Hash,
        retry_after_us: u64,
    },
}

/// Why a message was not encoded: something in it is over a limit, which the receiver's decoder would refuse.
/// Nothing is truncated: the sender learns it, and sends less.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EncodeError {
    TooMany,
    TooLong,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DecodeError {
    Version,
    Tag,
    Truncated,
    TooMany,
    TooLong,
    Trailing,
}

impl Msg {
    /// The cut and root every message names.
    pub fn head(&self) -> (CutRef, Hash) {
        match self {
            Msg::NodeRequest { cut, root, .. }
            | Msg::NodeReply { cut, root, .. }
            | Msg::RecordRequest { cut, root, .. }
            | Msg::RecordReply { cut, root, .. }
            | Msg::PhysicsRequest { cut, root }
            | Msg::PhysicsReply { cut, root, .. }
            | Msg::NotHeld { cut, root }
            | Msg::Busy { cut, root, .. } => (*cut, *root),
        }
    }

    pub fn encode(&self) -> Result<Bytes, EncodeError> {
        let mut w = vec![VERSION];
        let (cut, root) = self.head();
        w.push(match self {
            Msg::NodeRequest { .. } => 0,
            Msg::NodeReply { .. } => 1,
            Msg::RecordRequest { .. } => 2,
            Msg::RecordReply { .. } => 3,
            Msg::PhysicsRequest { .. } => 4,
            Msg::PhysicsReply { .. } => 5,
            Msg::NotHeld { .. } => 6,
            Msg::Busy { .. } => 7,
        });
        put_cut(&mut w, &cut);
        w.extend_from_slice(&root.0);
        match self {
            Msg::NodeRequest { paths, .. } => {
                put_count(&mut w, paths.len())?;
                for p in paths {
                    put_path(&mut w, p);
                }
            }
            Msg::NodeReply { nodes, .. } => {
                put_count(&mut w, nodes.len())?;
                for (p, n) in nodes {
                    put_path(&mut w, p);
                    put_node(&mut w, n)?;
                }
            }
            Msg::RecordRequest { keys, .. } => {
                put_count(&mut w, keys.len())?;
                for k in keys {
                    put_key(&mut w, k);
                }
            }
            Msg::RecordReply { records, .. } => {
                put_count(&mut w, records.len())?;
                for r in records {
                    put_record(&mut w, r)?;
                }
            }
            Msg::PhysicsReply { image, .. } => {
                if image.bytes.len() > MAX_IMAGE {
                    return Err(EncodeError::TooLong);
                }
                w.extend_from_slice(&image.domain.0.to_le_bytes());
                w.extend_from_slice(&image.frame.0.to_le_bytes());
                w.extend_from_slice(&image.global.0);
                put_bytes(&mut w, &image.bytes);
            }
            Msg::Busy { retry_after_us, .. } => w.extend_from_slice(&retry_after_us.to_le_bytes()),
            Msg::PhysicsRequest { .. } | Msg::NotHeld { .. } => {}
        }
        Ok(Bytes::from(w))
    }

    pub fn decode(bytes: &[u8]) -> Result<Msg, DecodeError> {
        let mut r = Reader { b: bytes };
        if r.u8()? != VERSION {
            return Err(DecodeError::Version);
        }
        let tag = r.u8()?;
        let cut = r.cut()?;
        let root = r.hash()?;
        let msg = match tag {
            0 => {
                let n = r.count(1)?;
                let mut paths = Vec::with_capacity(n);
                for _ in 0..n {
                    paths.push(r.path()?);
                }
                Msg::NodeRequest { cut, root, paths }
            }
            1 => {
                let n = r.count(2)?;
                let mut nodes = Vec::with_capacity(n);
                for _ in 0..n {
                    nodes.push((r.path()?, r.node()?));
                }
                Msg::NodeReply { cut, root, nodes }
            }
            2 => {
                let n = r.count(1)?; // the smallest key, `Config`, is one byte
                let mut keys = Vec::with_capacity(n);
                for _ in 0..n {
                    keys.push(r.key()?);
                }
                Msg::RecordRequest { cut, root, keys }
            }
            3 => {
                let n = r.count(1 + 4 + 2)?; // key, bytes length, proof count
                let mut records = Vec::with_capacity(n);
                for _ in 0..n {
                    records.push(r.record()?);
                }
                Msg::RecordReply { cut, root, records }
            }
            4 => Msg::PhysicsRequest { cut, root },
            5 => {
                let domain = DomainId(r.u64()?);
                let frame = Frame(r.u64()?);
                let global = r.hash()?;
                let bytes = r.bytes(MAX_IMAGE)?;
                Msg::PhysicsReply { cut, root, image: PhysicsImage { domain, frame, global, bytes } }
            }
            6 => Msg::NotHeld { cut, root },
            7 => Msg::Busy { cut, root, retry_after_us: r.u64()? },
            _ => return Err(DecodeError::Tag),
        };
        if !r.b.is_empty() {
            return Err(DecodeError::Trailing);
        }
        Ok(msg)
    }
}

// ---- writing: the ladder never builds more than the limits allow; anything over a limit is refused -------------

fn put_count(w: &mut Vec<u8>, n: usize) -> Result<(), EncodeError> {
    if n > MAX_ITEMS {
        return Err(EncodeError::TooMany);
    }
    w.extend_from_slice(&(n as u16).to_le_bytes());
    Ok(())
}

pub(crate) fn put_cut(w: &mut Vec<u8>, c: &CutRef) {
    w.extend_from_slice(&c.line.0.0);
    w.extend_from_slice(&c.height.to_le_bytes());
    w.extend_from_slice(&c.frame.0.to_le_bytes());
}

fn put_nibbles(w: &mut Vec<u8>, n: &Nibbles) {
    w.push(n.len() as u8);
    w.extend_from_slice(n.as_slice());
}

fn put_path(w: &mut Vec<u8>, p: &TreePath) {
    match p {
        TreePath::World => w.push(0),
        TreePath::Tables => w.push(1),
        TreePath::Domain(d) => {
            w.push(2);
            w.extend_from_slice(&d.0.to_le_bytes());
        }
        TreePath::Objects { domain, prefix } => {
            w.push(3);
            w.extend_from_slice(&domain.0.to_le_bytes());
            put_nibbles(w, prefix);
        }
    }
}

pub(crate) fn put_bytes(w: &mut Vec<u8>, b: &[u8]) {
    w.extend_from_slice(&(b.len() as u32).to_le_bytes());
    w.extend_from_slice(b);
}

pub(crate) fn put_key(w: &mut Vec<u8>, k: &LeafKey) {
    match k {
        LeafKey::Object(id) => {
            w.push(0);
            w.extend_from_slice(&id.to_u128().to_le_bytes());
        }
        LeafKey::Table(t) => {
            w.push(1);
            w.push(*t);
        }
        LeafKey::Physics(d) => {
            w.push(2);
            w.extend_from_slice(&d.0.to_le_bytes());
        }
        LeafKey::Config => w.push(3),
    }
}

fn put_hashes(w: &mut Vec<u8>, hs: &[Hash]) -> Result<(), EncodeError> {
    put_count(w, hs.len())?;
    for h in hs {
        w.extend_from_slice(&h.0);
    }
    Ok(())
}

fn put_node(w: &mut Vec<u8>, node: &Node) -> Result<(), EncodeError> {
    match node {
        Node::World { frame, config, tables, domains } => {
            w.push(0);
            w.extend_from_slice(&frame.0.to_le_bytes());
            w.extend_from_slice(&config.0);
            w.extend_from_slice(&tables.0);
            put_count(w, domains.len())?;
            for (d, h) in domains {
                w.extend_from_slice(&d.0.to_le_bytes());
                w.extend_from_slice(&h.0);
            }
        }
        Node::Tables(hs) => {
            w.push(1);
            put_hashes(w, hs)?;
        }
        Node::Domain { domain, objects, physics } => {
            w.push(2);
            w.extend_from_slice(&domain.0.to_le_bytes());
            w.extend_from_slice(&objects.0);
            w.extend_from_slice(&physics.0);
        }
        Node::Inner { prefix, children } => {
            if children.len() > MAX_CHILDREN {
                return Err(EncodeError::TooMany);
            }
            w.push(3);
            put_nibbles(w, prefix);
            w.push(children.len() as u8);
            for (n, h) in children {
                w.push(*n);
                w.extend_from_slice(&h.0);
            }
        }
        Node::Leaf { id, object_hash } => {
            w.push(4);
            w.extend_from_slice(&id.to_u128().to_le_bytes());
            w.extend_from_slice(&object_hash.0);
        }
        Node::Empty => w.push(5),
    }
    Ok(())
}

pub(crate) fn put_record(w: &mut Vec<u8>, r: &Record) -> Result<(), EncodeError> {
    if r.bytes.len() > MAX_RECORD {
        return Err(EncodeError::TooLong);
    }
    put_key(w, &r.key);
    put_bytes(w, &r.bytes);
    put_count(w, r.proof.len())?;
    for n in &r.proof {
        put_node(w, n)?;
    }
    Ok(())
}

/// The encoded size of a node, for splitting replies by bytes.
pub fn node_size(n: &Node) -> usize {
    1 + match n {
        Node::World { domains, .. } => 8 + 64 + 2 + 40 * domains.len(),
        Node::Tables(hs) => 2 + 32 * hs.len(),
        Node::Domain { .. } => 8 + 64,
        Node::Inner { prefix, children } => 1 + prefix.len() + 1 + 33 * children.len(),
        Node::Leaf { .. } => 16 + 32,
        Node::Empty => 0,
    }
}

/// The encoded size of a path.
pub fn path_size(p: &TreePath) -> usize {
    match p {
        TreePath::World | TreePath::Tables => 1,
        TreePath::Domain(_) => 9,
        TreePath::Objects { prefix, .. } => 10 + prefix.len(),
    }
}

/// The encoded size of a record, for splitting replies by bytes.
pub fn record_size(r: &Record) -> usize {
    17 + 4 + r.bytes.len() + 2 + r.proof.iter().map(node_size).sum::<usize>()
}

// ---- reading: every read checks what is left first -----------------------------------------------------------------

pub(crate) struct Reader<'a> {
    pub(crate) b: &'a [u8],
}

impl<'a> Reader<'a> {
    pub(crate) fn take(&mut self, n: usize) -> Result<&'a [u8], DecodeError> {
        if self.b.len() < n {
            return Err(DecodeError::Truncated);
        }
        let (head, rest) = self.b.split_at(n);
        self.b = rest;
        Ok(head)
    }
    pub(crate) fn u8(&mut self) -> Result<u8, DecodeError> {
        Ok(self.take(1)?[0])
    }
    fn u16(&mut self) -> Result<usize, DecodeError> {
        let s = self.take(2)?;
        Ok(u16::from_le_bytes([s[0], s[1]]) as usize)
    }
    pub(crate) fn u32(&mut self) -> Result<usize, DecodeError> {
        let s = self.take(4)?;
        Ok(u32::from_le_bytes([s[0], s[1], s[2], s[3]]) as usize)
    }
    pub(crate) fn u64(&mut self) -> Result<u64, DecodeError> {
        let mut a = [0u8; 8];
        a.copy_from_slice(self.take(8)?);
        Ok(u64::from_le_bytes(a))
    }
    pub(crate) fn u128(&mut self) -> Result<u128, DecodeError> {
        let mut a = [0u8; 16];
        a.copy_from_slice(self.take(16)?);
        Ok(u128::from_le_bytes(a))
    }
    pub(crate) fn hash(&mut self) -> Result<Hash, DecodeError> {
        let mut a = [0u8; 32];
        a.copy_from_slice(self.take(32)?);
        Ok(Hash(a))
    }
    pub(crate) fn cut(&mut self) -> Result<CutRef, DecodeError> {
        Ok(CutRef { line: LineId(self.hash()?), height: self.u64()?, frame: Frame(self.u64()?) })
    }
    /// A count of items each at least `min` bytes long, refused if the input cannot hold them.
    pub(crate) fn count(&mut self, min: usize) -> Result<usize, DecodeError> {
        let n = self.u16()?;
        if n > MAX_ITEMS {
            return Err(DecodeError::TooMany);
        }
        if n.saturating_mul(min) > self.b.len() {
            return Err(DecodeError::Truncated);
        }
        Ok(n)
    }
    pub(crate) fn bytes(&mut self, max: usize) -> Result<Bytes, DecodeError> {
        let n = self.u32()?;
        if n > max {
            return Err(DecodeError::TooLong);
        }
        Ok(Bytes::copy_from_slice(self.take(n)?))
    }
    fn nibbles(&mut self) -> Result<Nibbles, DecodeError> {
        let n = self.u8()? as usize;
        if n > Nibbles::MAX {
            return Err(DecodeError::TooLong);
        }
        Nibbles::new(self.take(n)?.to_vec()).ok_or(DecodeError::Tag)
    }
    fn path(&mut self) -> Result<TreePath, DecodeError> {
        match self.u8()? {
            0 => Ok(TreePath::World),
            1 => Ok(TreePath::Tables),
            2 => Ok(TreePath::Domain(DomainId(self.u64()?))),
            3 => Ok(TreePath::Objects { domain: DomainId(self.u64()?), prefix: self.nibbles()? }),
            _ => Err(DecodeError::Tag),
        }
    }
    pub(crate) fn key(&mut self) -> Result<LeafKey, DecodeError> {
        match self.u8()? {
            0 => Ok(LeafKey::Object(ObjectId::from_u128(self.u128()?))),
            1 => Ok(LeafKey::Table(self.u8()?)),
            2 => Ok(LeafKey::Physics(DomainId(self.u64()?))),
            3 => Ok(LeafKey::Config),
            _ => Err(DecodeError::Tag),
        }
    }
    fn node(&mut self) -> Result<Node, DecodeError> {
        match self.u8()? {
            0 => {
                let frame = Frame(self.u64()?);
                let config = self.hash()?;
                let tables = self.hash()?;
                let n = self.count(40)?;
                let mut domains = Vec::with_capacity(n);
                for _ in 0..n {
                    domains.push((DomainId(self.u64()?), self.hash()?));
                }
                Ok(Node::World { frame, config, tables, domains })
            }
            1 => {
                let n = self.count(32)?;
                let mut hs = Vec::with_capacity(n);
                for _ in 0..n {
                    hs.push(self.hash()?);
                }
                Ok(Node::Tables(hs))
            }
            2 => Ok(Node::Domain { domain: DomainId(self.u64()?), objects: self.hash()?, physics: self.hash()? }),
            3 => {
                let prefix = self.nibbles()?;
                let n = self.u8()? as usize;
                if n > MAX_CHILDREN {
                    return Err(DecodeError::TooMany);
                }
                let mut children = Vec::with_capacity(n);
                for _ in 0..n {
                    children.push((self.u8()?, self.hash()?));
                }
                Ok(Node::Inner { prefix, children })
            }
            4 => Ok(Node::Leaf { id: ObjectId::from_u128(self.u128()?), object_hash: self.hash()? }),
            5 => Ok(Node::Empty),
            _ => Err(DecodeError::Tag),
        }
    }
    pub(crate) fn record(&mut self) -> Result<Record, DecodeError> {
        let key = self.key()?;
        let bytes = self.bytes(MAX_RECORD)?;
        let n = self.count(1)?;
        let mut proof = Vec::with_capacity(n);
        for _ in 0..n {
            proof.push(self.node()?);
        }
        Ok(Record { key, bytes, proof })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cut() -> CutRef {
        CutRef { line: LineId(Hash([9; 32])), height: 3, frame: Frame(99) }
    }

    fn nib(v: &[u8]) -> Nibbles {
        Nibbles::new(v.to_vec()).unwrap()
    }

    fn samples() -> Vec<Msg> {
        let d = DomainId(0);
        vec![
            Msg::NodeRequest {
                cut: cut(),
                root: Hash([1; 32]),
                paths: vec![
                    TreePath::World,
                    TreePath::Tables,
                    TreePath::Domain(d),
                    TreePath::Objects { domain: d, prefix: nib(&[3, 4]) },
                ],
            },
            Msg::NodeReply {
                cut: cut(),
                root: Hash([2; 32]),
                nodes: vec![
                    (
                        TreePath::World,
                        Node::World {
                            frame: Frame(99),
                            config: Hash([1; 32]),
                            tables: Hash([2; 32]),
                            domains: vec![(d, Hash([3; 32]))],
                        },
                    ),
                    (TreePath::Tables, Node::Tables(vec![Hash([4; 32]), Hash([5; 32])])),
                    (TreePath::Domain(d), Node::Domain { domain: d, objects: Hash([6; 32]), physics: Hash([7; 32]) }),
                    (
                        TreePath::Objects { domain: d, prefix: nib(&[]) },
                        Node::Inner { prefix: nib(&[0, 0, 1]), children: vec![(0, Hash([5; 32])), (9, Hash([6; 32]))] },
                    ),
                    (
                        TreePath::Objects { domain: d, prefix: nib(&[9, 1]) },
                        Node::Leaf { id: ObjectId::from_u128(77), object_hash: Hash([7; 32]) },
                    ),
                    (TreePath::Objects { domain: d, prefix: nib(&[2]) }, Node::Empty),
                ],
            },
            Msg::RecordRequest {
                cut: cut(),
                root: Hash([3; 32]),
                keys: vec![
                    LeafKey::Object(ObjectId::from_u128(5)),
                    LeafKey::Table(1),
                    LeafKey::Object(ObjectId::from_u128(u128::MAX)),
                    LeafKey::Config,
                ],
            },
            Msg::RecordReply {
                cut: cut(),
                root: Hash([4; 32]),
                records: vec![Record {
                    key: LeafKey::Object(ObjectId::from_u128(5)),
                    bytes: Bytes::from_static(&[9, 9, 9]),
                    proof: vec![Node::Inner { prefix: nib(&[]), children: vec![(5, Hash([1; 32]))] }],
                }],
            },
            Msg::PhysicsRequest { cut: cut(), root: Hash([5; 32]) },
            Msg::PhysicsReply {
                cut: cut(),
                root: Hash([6; 32]),
                image: PhysicsImage {
                    domain: DomainId(0),
                    frame: Frame(99),
                    global: Hash([7; 32]),
                    bytes: Bytes::from_static(b"img"),
                },
            },
            Msg::NotHeld { cut: cut(), root: Hash([8; 32]) },
            Msg::Busy { cut: cut(), root: Hash([9; 32]), retry_after_us: 12_345 },
            // One key of one byte: the decoder's size check must allow it (found by the pacing test).
            Msg::RecordRequest { cut: cut(), root: Hash([10; 32]), keys: vec![LeafKey::Config] },
        ]
    }

    #[test]
    fn every_message_round_trips() {
        for m in samples() {
            assert_eq!(Msg::decode(&m.encode().unwrap()), Ok(m));
        }
    }

    #[test]
    fn arbitrary_bytes_never_panic() {
        let mut x: u64 = 0x9e37_79b9_7f4a_7c15;
        for valid in samples().iter().map(|m| m.encode().unwrap().to_vec()) {
            for n in 0..valid.len() {
                let _ = Msg::decode(&valid[..n]);
            }
            for _ in 0..3000 {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                let mut b = valid.clone();
                let i = (x as usize) % b.len();
                b[i] = (x >> 32) as u8;
                let _ = Msg::decode(&b);
            }
        }
        for _ in 0..10_000 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let len = (x >> 40) as usize % 300;
            let noise: Vec<u8> = (0..len).map(|k| (x.rotate_left(k as u32) & 0xff) as u8).collect();
            let _ = Msg::decode(&noise);
        }
    }

    #[test]
    fn over_limit_messages_are_refused_not_truncated() {
        let wide = Msg::NodeReply {
            cut: cut(),
            root: Hash::ZERO,
            nodes: vec![(
                TreePath::World,
                Node::Inner { prefix: nib(&[]), children: (0..=MAX_CHILDREN as u8).map(|n| (n, Hash::ZERO)).collect() },
            )],
        };
        assert_eq!(wide.encode(), Err(EncodeError::TooMany));
        let many = Msg::RecordRequest { cut: cut(), root: Hash::ZERO, keys: vec![LeafKey::Table(1); MAX_ITEMS + 1] };
        assert_eq!(many.encode(), Err(EncodeError::TooMany));
    }

    #[test]
    fn a_huge_count_is_refused_without_allocating() {
        let mut b = vec![VERSION, 2];
        b.extend_from_slice(&[0; 32 + 16 + 32]);
        b.extend_from_slice(&u16::MAX.to_le_bytes());
        assert!(Msg::decode(&b).is_err());
    }
}
