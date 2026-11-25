//! The repair lane's messages (divergence.md 11.3), on S4's `Lane::Repair`.
//!
//! Unsigned: every record a reply carries is checked against the quorum-agreed root by the receiving replica's
//! `import_at_cut`, with its proof, before anything is applied. Decoding never panics and never allocates more than
//! the input could hold: any byte string is either a message or a [`DecodeError`] (plan 4.1).

use bytes::Bytes;
use pe_seams::{
    CutRef, DomainId, Frame, Hash, LeafKey, LineId, Nibbles, Node, ObjectId, PhysicsImage, Record, TreePath,
};

/// 2: a record reply carries the inline blobs its records' rows reference (inline blobs phase 3b). 3: blobs up to
/// [`MAX_BLOB`]; what does not fit a reply is named in it and pulled: blobs by hash, records over the reply budget in
/// numbered pieces (divergence.md 4.2, step 3; @coordinator-opus).
const VERSION: u8 = 3;
/// Most items in one message.
pub const MAX_ITEMS: usize = 4096;
/// Most children of one trie node: one per nibble.
pub const MAX_CHILDREN: usize = 16;
/// Largest record a reply carries whole. A source sends whole only records within its reply budget (1 MiB by
/// default), and names a longer one ([`Large`]) for the requester to pull in pieces.
pub const MAX_RECORD: usize = 16 << 20;
/// Longest record a name may claim: `R_max` (divergence.md 4.2). A piece can't be checked until the whole record is
/// assembled, so the claimed length is what the requester would buffer; pe-world refuses any edit that would take a
/// record past the same number (world-model.md 6.6, @coordinator-opus).
pub const R_MAX: usize = 256 << 20;
/// One piece of a pulled record: piece `i` is the record's bytes `[i * PIECE, (i + 1) * PIECE)`, the last one shorter.
pub const PIECE: usize = 64 << 10;
/// Largest blob: an inline blob (1 to 256 bytes) or a carried simulation blob (257 bytes to 960 KiB, a `SimBlobRef`
/// field's mesh or hull; world-model.md 3.4). pe-world's `inline::MAX_CARRIED_BLOB`, which pe-diverge (sans-IO, over
/// pe-seams only) doesn't depend on; a test keeps the two equal.
pub const MAX_BLOB: usize = 960 * 1024;
/// Most bytes the blobs one record reply names may claim in all. The names are the source's unchecked claim until the
/// blobs arrive and hash, and the requester pulls what they name.
pub const MAX_NAMED: usize = R_MAX;
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
        /// Records too long to carry: each named with its length and proof, pulled with [`Msg::PieceRequest`].
        large: Vec<Large>,
        /// Blobs these records' rows reference, as far as the sender has them (S2 v1.8 `Tree::export_blobs`), and as
        /// many as fit the reply. Unchecked here: the importing replica checks each against the rows it imports.
        blobs: Vec<Bytes>,
        /// The rest of those blobs, named, to be pulled with [`Msg::BlobRequest`].
        named: Vec<Named>,
    },
    /// Blobs a record reply named: each with the record that references it, which the source checks.
    BlobRequest {
        cut: CutRef,
        root: Hash,
        wants: Vec<(LeafKey, Hash)>,
    },
    BlobReply {
        cut: CutRef,
        root: Hash,
        blobs: Vec<Bytes>,
    },
    /// Pieces `first..first + count` of a record a reply named as [`Large`].
    PieceRequest {
        cut: CutRef,
        root: Hash,
        key: LeafKey,
        first: u32,
        count: u32,
    },
    /// Pieces `first..` of a record, in order: each [`PIECE`] long but the record's last.
    PieceReply {
        cut: CutRef,
        root: Hash,
        key: LeafKey,
        first: u32,
        pieces: Vec<Bytes>,
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

/// A record a reply names instead of carrying: its bytes are pulled in pieces, and checked against the target root,
/// with this proof, only once assembled (by the importing replica).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Large {
    pub key: LeafKey,
    /// The record's length: claimed, at most [`R_MAX`].
    pub len: usize,
    pub proof: Vec<Node>,
}

/// A blob a reply names instead of carrying.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Named {
    /// The record whose rows reference it.
    pub key: LeafKey,
    pub hash: Hash,
    /// Its length: claimed, at most [`MAX_BLOB`]. The requester sizes its requests by it.
    pub len: usize,
}

/// The number of pieces a record of `len` bytes is pulled in.
pub fn pieces_of(len: usize) -> usize {
    len.div_ceil(PIECE)
}

/// The length of piece `i` of a record of `len` bytes (0 past its last).
pub fn piece_len(len: usize, i: usize) -> usize {
    len.saturating_sub(i.saturating_mul(PIECE)).min(PIECE)
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
            | Msg::BlobRequest { cut, root, .. }
            | Msg::BlobReply { cut, root, .. }
            | Msg::PieceRequest { cut, root, .. }
            | Msg::PieceReply { cut, root, .. }
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
            Msg::BlobRequest { .. } => 8,
            Msg::BlobReply { .. } => 9,
            Msg::PieceRequest { .. } => 10,
            Msg::PieceReply { .. } => 11,
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
            Msg::RecordReply { records, large, blobs, named, .. } => {
                put_count(&mut w, records.len())?;
                for r in records {
                    put_record(&mut w, r)?;
                }
                put_count(&mut w, large.len())?;
                for l in large {
                    if l.len == 0 || l.len > R_MAX {
                        return Err(EncodeError::TooLong);
                    }
                    put_key(&mut w, &l.key);
                    w.extend_from_slice(&(l.len as u32).to_le_bytes());
                    put_count(&mut w, l.proof.len())?;
                    for n in &l.proof {
                        put_node(&mut w, n)?;
                    }
                }
                put_blobs(&mut w, blobs, MAX_BLOB)?;
                put_count(&mut w, named.len())?;
                if named.iter().map(|n| n.len).sum::<usize>() > MAX_NAMED {
                    return Err(EncodeError::TooLong);
                }
                for n in named {
                    if n.len == 0 || n.len > MAX_BLOB {
                        return Err(EncodeError::TooLong);
                    }
                    put_key(&mut w, &n.key);
                    w.extend_from_slice(&n.hash.0);
                    w.extend_from_slice(&(n.len as u32).to_le_bytes());
                }
            }
            Msg::BlobRequest { wants, .. } => {
                put_count(&mut w, wants.len())?;
                for (k, h) in wants {
                    put_key(&mut w, k);
                    w.extend_from_slice(&h.0);
                }
            }
            Msg::BlobReply { blobs, .. } => put_blobs(&mut w, blobs, MAX_BLOB)?,
            Msg::PieceRequest { key, first, count, .. } => {
                if *count as usize > MAX_ITEMS {
                    return Err(EncodeError::TooMany);
                }
                put_key(&mut w, key);
                w.extend_from_slice(&first.to_le_bytes());
                w.extend_from_slice(&count.to_le_bytes());
            }
            Msg::PieceReply { key, first, pieces, .. } => {
                put_key(&mut w, key);
                w.extend_from_slice(&first.to_le_bytes());
                put_blobs(&mut w, pieces, PIECE)?;
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
                let n = r.count(1 + 4 + 2)?; // key, length, proof count
                let mut large = Vec::with_capacity(n);
                for _ in 0..n {
                    let key = r.key()?;
                    let len = r.u32()?;
                    if len == 0 || len > R_MAX {
                        return Err(DecodeError::TooLong);
                    }
                    let m = r.count(1)?;
                    let mut proof = Vec::with_capacity(m);
                    for _ in 0..m {
                        proof.push(r.node()?);
                    }
                    large.push(Large { key, len, proof });
                }
                let blobs = r.blobs(MAX_BLOB)?;
                let n = r.count(1 + 32 + 4)?; // key, hash, length
                let mut named = Vec::with_capacity(n);
                let mut total = 0usize;
                for _ in 0..n {
                    let (key, hash, len) = (r.key()?, r.hash()?, r.u32()?);
                    total += len;
                    if len == 0 || len > MAX_BLOB || total > MAX_NAMED {
                        return Err(DecodeError::TooLong);
                    }
                    named.push(Named { key, hash, len });
                }
                Msg::RecordReply { cut, root, records, large, blobs, named }
            }
            8 => {
                let n = r.count(1 + 32)?; // key, hash
                let mut wants = Vec::with_capacity(n);
                for _ in 0..n {
                    wants.push((r.key()?, r.hash()?));
                }
                Msg::BlobRequest { cut, root, wants }
            }
            9 => Msg::BlobReply { cut, root, blobs: r.blobs(MAX_BLOB)? },
            10 => {
                let (key, first, count) = (r.key()?, r.u32()? as u32, r.u32()? as u32);
                if count as usize > MAX_ITEMS {
                    return Err(DecodeError::TooMany);
                }
                Msg::PieceRequest { cut, root, key, first, count }
            }
            11 => Msg::PieceReply { cut, root, key: r.key()?, first: r.u32()? as u32, pieces: r.blobs(PIECE)? },
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

/// A count, then each blob with its length, each at most `max` bytes.
fn put_blobs(w: &mut Vec<u8>, blobs: &[Bytes], max: usize) -> Result<(), EncodeError> {
    put_count(w, blobs.len())?;
    for b in blobs {
        if b.len() > max {
            return Err(EncodeError::TooLong);
        }
        put_bytes(w, b);
    }
    Ok(())
}

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

/// A blob's (or a piece's) encoded size in a reply: its length prefix and its bytes.
pub fn blob_size(b: &[u8]) -> usize {
    4 + b.len()
}

/// The encoded size of a [`Large`] name with this proof.
pub fn large_size(proof: &[Node]) -> usize {
    17 + 4 + 2 + proof.iter().map(node_size).sum::<usize>()
}

/// The encoded size of a [`Named`] blob.
pub const NAMED_SIZE: usize = 17 + 32 + 4;

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
    /// A count, then each blob with its length, each at most `max` bytes.
    fn blobs(&mut self, max: usize) -> Result<Vec<Bytes>, DecodeError> {
        let n = self.count(4)?; // a blob's length
        let mut blobs = Vec::with_capacity(n);
        for _ in 0..n {
            blobs.push(self.bytes(max)?);
        }
        Ok(blobs)
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
                large: vec![Large {
                    key: LeafKey::Table(4),
                    len: 5 << 20,
                    proof: vec![Node::Tables(vec![Hash([2; 32]), Hash([3; 32])])],
                }],
                blobs: vec![Bytes::from_static(b"a name"), Bytes::from(vec![7; 300])],
                named: vec![
                    Named { key: LeafKey::Object(ObjectId::from_u128(5)), hash: Hash([5; 32]), len: MAX_BLOB },
                    Named { key: LeafKey::Table(4), hash: Hash([6; 32]), len: 257 },
                ],
            },
            Msg::BlobRequest {
                cut: cut(),
                root: Hash([11; 32]),
                wants: vec![
                    (LeafKey::Object(ObjectId::from_u128(5)), Hash([5; 32])),
                    (LeafKey::Table(4), Hash([6; 32])),
                ],
            },
            Msg::BlobReply { cut: cut(), root: Hash([12; 32]), blobs: vec![Bytes::from(vec![1; 257])] },
            Msg::PieceRequest { cut: cut(), root: Hash([13; 32]), key: LeafKey::Table(4), first: 3, count: 16 },
            Msg::PieceReply {
                cut: cut(),
                root: Hash([14; 32]),
                key: LeafKey::Table(4),
                first: 3,
                pieces: vec![Bytes::from(vec![2; 40]), Bytes::from_static(b"end")],
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
        // At the limits: a blob at the carried class's cap, a piece at its length, and a record named at `R_MAX`.
        let at_cap = [
            Msg::BlobReply { cut: cut(), root: Hash::ZERO, blobs: vec![Bytes::from(vec![8; MAX_BLOB])] },
            Msg::PieceReply {
                cut: cut(),
                root: Hash::ZERO,
                key: LeafKey::Config,
                first: 0,
                pieces: vec![Bytes::from(vec![3; PIECE])],
            },
            Msg::RecordReply {
                cut: cut(),
                root: Hash::ZERO,
                records: vec![],
                large: vec![Large { key: LeafKey::Config, len: R_MAX, proof: vec![] }],
                blobs: vec![Bytes::from(vec![8; MAX_BLOB])],
                named: vec![],
            },
        ];
        for m in at_cap {
            assert_eq!(Msg::decode(&m.encode().unwrap()), Ok(m));
        }
    }

    #[test]
    fn pieces_cover_a_record_exactly() {
        assert_eq!((pieces_of(1), piece_len(1, 0), piece_len(1, 1)), (1, 1, 0));
        assert_eq!((pieces_of(PIECE), piece_len(PIECE, 0)), (1, PIECE));
        let len = 3 * PIECE + 5;
        assert_eq!(pieces_of(len), 4);
        assert_eq!((0..4).map(|i| piece_len(len, i)).sum::<usize>(), len);
        assert_eq!(piece_len(len, 3), 5);
        assert_eq!(pieces_of(R_MAX), R_MAX / PIECE);
        assert!(pieces_of(R_MAX) <= MAX_ITEMS, "a whole record fits one piece request's count");
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
        let reply = |large: Vec<Large>, blobs: Vec<Bytes>, named: Vec<Named>| Msg::RecordReply {
            cut: cut(),
            root: Hash::ZERO,
            records: vec![],
            large,
            blobs,
            named,
        };
        assert_eq!(reply(vec![], vec![Bytes::from(vec![0; MAX_BLOB + 1])], vec![]).encode(), Err(EncodeError::TooLong));
        let big = Large { key: LeafKey::Config, len: R_MAX + 1, proof: vec![] };
        assert_eq!(reply(vec![big], vec![], vec![]).encode(), Err(EncodeError::TooLong));
        let named = |i: u8, len: usize| Named { key: LeafKey::Config, hash: Hash([i; 32]), len };
        assert_eq!(reply(vec![], vec![], vec![named(1, MAX_BLOB + 1)]).encode(), Err(EncodeError::TooLong));
        // Each name within the class, but more than `MAX_NAMED` claimed in all.
        let over: Vec<Named> = (0..=MAX_NAMED / MAX_BLOB)
            .map(|i| Named { key: LeafKey::Config, hash: Hash::plain(&i.to_le_bytes()), len: MAX_BLOB })
            .collect();
        assert_eq!(reply(vec![], vec![], over).encode(), Err(EncodeError::TooLong));
        let piece = Msg::PieceReply {
            cut: cut(),
            root: Hash::ZERO,
            key: LeafKey::Config,
            first: 0,
            pieces: vec![Bytes::from(vec![0; PIECE + 1])],
        };
        assert_eq!(piece.encode(), Err(EncodeError::TooLong));
        let count = MAX_ITEMS as u32 + 1;
        let wide = Msg::PieceRequest { cut: cut(), root: Hash::ZERO, key: LeafKey::Config, first: 0, count };
        assert_eq!(wide.encode(), Err(EncodeError::TooMany));
    }

    /// The decoder refuses what the encoder would: here, claims past the limits written by a sender that skipped
    /// its own checks.
    #[test]
    fn a_claim_past_the_limits_is_refused_by_the_decoder() {
        let reply = |large: Vec<Large>, named: Vec<Named>| {
            Msg::RecordReply { cut: cut(), root: Hash::ZERO, records: vec![], large, blobs: vec![], named }
                .encode()
                .unwrap()
                .to_vec()
        };
        // A large record of length 1, its length field patched to `R_MAX + 1`: the bytes after the header are
        // version, tag, cut (48), root (32), records count (2), large count (2), key (Config: 1).
        let mut b = reply(vec![Large { key: LeafKey::Config, len: 1, proof: vec![] }], vec![]);
        let at = 2 + 48 + 32 + 2 + 2 + 1;
        b[at..at + 4].copy_from_slice(&(R_MAX as u32 + 1).to_le_bytes());
        assert_eq!(Msg::decode(&b), Err(DecodeError::TooLong));
        // A named blob of length 1, its length (the last four bytes) patched to `MAX_BLOB + 1`.
        let mut b = reply(vec![], vec![Named { key: LeafKey::Config, hash: Hash::ZERO, len: 1 }]);
        let at = b.len() - 4;
        b[at..].copy_from_slice(&(MAX_BLOB as u32 + 1).to_le_bytes());
        assert_eq!(Msg::decode(&b), Err(DecodeError::TooLong));
        // A name of length 0 claims nothing to pull.
        let mut b = reply(vec![], vec![Named { key: LeafKey::Config, hash: Hash::ZERO, len: 1 }]);
        let at = b.len() - 4;
        b[at..].copy_from_slice(&0u32.to_le_bytes());
        assert_eq!(Msg::decode(&b), Err(DecodeError::TooLong));
        // A piece longer than `PIECE`.
        let ok = Msg::PieceReply {
            cut: cut(),
            root: Hash::ZERO,
            key: LeafKey::Config,
            first: 0,
            pieces: vec![Bytes::from_static(b"x")],
        };
        let mut b = ok.encode().unwrap().to_vec();
        let at = b.len() - 5;
        b[at..at + 4].copy_from_slice(&((PIECE as u32) + 1).to_le_bytes());
        b.truncate(at + 4);
        b.extend(std::iter::repeat_n(0u8, PIECE + 1));
        assert_eq!(Msg::decode(&b), Err(DecodeError::TooLong));
    }

    #[test]
    fn a_blob_longer_than_the_carried_class_is_refused_by_the_decoder() {
        let ok = Msg::RecordReply {
            cut: cut(),
            root: Hash::ZERO,
            records: vec![],
            large: vec![],
            blobs: vec![Bytes::from_static(b"x")],
            named: vec![],
        };
        let mut b = ok.encode().unwrap().to_vec();
        // The one blob's length field is the 7 bytes before the end (then its byte and the names' count): make it
        // 960 KiB + 1 and append the bytes.
        let at = b.len() - 7;
        b[at..at + 4].copy_from_slice(&((MAX_BLOB as u32) + 1).to_le_bytes());
        b.truncate(at + 4);
        b.extend(std::iter::repeat_n(0u8, MAX_BLOB + 1));
        b.extend_from_slice(&0u16.to_le_bytes());
        assert_eq!(Msg::decode(&b), Err(DecodeError::TooLong));
    }

    #[test]
    fn the_largest_blob_is_pe_world_s_carried_class() {
        assert_eq!(MAX_BLOB, pe_world::inline::MAX_CARRIED_BLOB);
    }

    /// pe-world refuses an edit that would take a record past the same `R_max` (world-model.md 6.6), so no honest
    /// record is refused here.
    #[test]
    fn r_max_is_pe_world_s_record_cap() {
        assert_eq!(R_MAX, pe_world::tree::MAX_RECORD_BYTES);
    }

    #[test]
    fn a_huge_count_is_refused_without_allocating() {
        let mut b = vec![VERSION, 2];
        b.extend_from_slice(&[0; 32 + 16 + 32]);
        b.extend_from_slice(&u16::MAX.to_le_bytes());
        assert!(Msg::decode(&b).is_err());
    }
}
