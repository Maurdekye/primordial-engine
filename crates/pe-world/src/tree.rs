//! The hash tree (world-model.md 6): object records and leaves, the world tables, the config hash, the canonical
//! path-compressed 16-way trie per domain, and the S2 [`Tree`] calls on a world.
//!
//! The byte-level definitions are 6.6's; every node's hash is `pe_seams::Node::hash`, the one definition. This first
//! version computes from the rows and tables each time a tree is built: no hash is cached, so [`Tree::verify`] has
//! nothing to compare and always passes. The incremental leaf cache (6.4) comes later, measured first.

use pe_core::commit::Hasher;
use pe_core::coords::PosRow;
use pe_core::{Hash, LineId, ObjectId, Pos};
use pe_seams::{DomainId, Frame, LeafKey, Mismatch, Nibbles, Node, Record, Tree, TreePath};
use pe_sim::{ComponentDesc, ComponentId, FieldKind, RowVersion, WorldTransform};

use std::sync::Arc;

use crate::world::World;

/// The world tables, in their order in the tables root (6.6): the `LeafKey::Table` index is the position.
pub mod table {
    pub const REGISTRY: u8 = 0;
    pub const LINES: u8 = 1;
    pub const MODULES: u8 = 2;
    pub const GOVERNANCE: u8 = 3;
    pub const CLAIMS: u8 = 4;
    pub const COOLDOWNS: u8 = 5;
    pub const TOMBSTONES: u8 = 6;
    pub const HAND_PACKS: u8 = 7;
    pub const PUSH_WINDOWS: u8 = 8;
    pub const COUNT: usize = 9;
}

/// One table: its entries as (key bytes, value bytes), in key order.
type Entries = Vec<(Vec<u8>, Vec<u8>)>;

fn lp(out: &mut Vec<u8>, b: &[u8]) {
    out.extend_from_slice(&(b.len() as u64).to_le_bytes());
    out.extend_from_slice(b);
}

fn id_be(id: ObjectId) -> [u8; 16] {
    id.to_u128().to_be_bytes()
}

/// The field kind a frozen 6.6 code names, if any.
pub(crate) fn kind_of_code(c: u8) -> Option<FieldKind> {
    const ALL: [FieldKind; 18] = [
        FieldKind::U8,
        FieldKind::U16,
        FieldKind::U32,
        FieldKind::U64,
        FieldKind::I8,
        FieldKind::I16,
        FieldKind::I32,
        FieldKind::I64,
        FieldKind::F32,
        FieldKind::F64,
        FieldKind::Bool,
        FieldKind::ObjectRef,
        FieldKind::BlobRef,
        FieldKind::SimBlobRef,
        FieldKind::Pos,
        FieldKind::MemberKey,
        FieldKind::EntryRef,
        FieldKind::Hash,
    ];
    ALL.into_iter().find(|&k| kind_code(k) == c)
}

pub(crate) fn kind_code(k: FieldKind) -> u8 {
    match k {
        FieldKind::U8 => 0,
        FieldKind::U16 => 1,
        FieldKind::U32 => 2,
        FieldKind::U64 => 3,
        FieldKind::I8 => 4,
        FieldKind::I16 => 5,
        FieldKind::I32 => 6,
        FieldKind::I64 => 7,
        FieldKind::F32 => 8,
        FieldKind::F64 => 9,
        FieldKind::Bool => 10,
        FieldKind::ObjectRef => 11,
        FieldKind::BlobRef => 12,
        FieldKind::SimBlobRef => 13,
        FieldKind::Pos => 14,
        FieldKind::MemberKey => 15,
        FieldKind::EntryRef => 16,
        FieldKind::Hash => 17,
    }
}

/// A field's kind byte (6.6, 3.6): the kind's code in the low five bits, its colour space's code (0 for none) in the
/// high three, so a field without a colour space has the byte it always had.
pub(crate) fn field_byte(f: &pe_sim::FieldDesc) -> u8 {
    f.kind_byte(kind_code(f.kind))
}

/// The kind and colour space of a [`field_byte`]; `None` for an unknown kind or space.
pub(crate) fn field_of_byte(b: u8) -> Option<(FieldKind, Option<pe_sim::ColorSpace>)> {
    let (k, space) = pe_sim::FieldDesc::split_kind_byte(b)?;
    Some((kind_of_code(k)?, space))
}

/// A component description's bytes (6.6): `lp(name) ‖ row size ‖ field count ‖ per field, in offset order: lp(name)
/// ‖ kind byte ([`field_byte`]) ‖ offset ‖ count ‖ lp(unit)`. pe-sim keeps fields sorted by offset.
pub(crate) fn desc_bytes(d: &ComponentDesc) -> Vec<u8> {
    let mut b = Vec::new();
    lp(&mut b, d.name().as_bytes());
    b.extend_from_slice(&(d.size() as u32).to_le_bytes());
    b.extend_from_slice(&(d.fields().len() as u32).to_le_bytes());
    for f in d.fields() {
        lp(&mut b, f.name.as_bytes());
        b.push(field_byte(f));
        b.extend_from_slice(&f.offset.to_le_bytes());
        b.extend_from_slice(&f.count.to_le_bytes());
        lp(&mut b, f.unit.as_bytes());
    }
    b
}

/// The schema hash of a component description (6.6): `H("pe.schema" ‖ description bytes)`.
pub fn schema_hash(d: &ComponentDesc) -> Hash {
    Hash::of("pe.schema", &desc_bytes(d))
}

/// A row's canonical bytes (6.3): the stored bytes, each `Pos` field replaced by its canonical encoding (4.5).
fn canonical_row(d: &ComponentDesc, row: &[u8]) -> Vec<u8> {
    if !d.fields().iter().any(|f| f.kind == FieldKind::Pos) {
        return row.to_vec();
    }
    let mut out = Vec::with_capacity(row.len());
    let mut at = 0usize;
    let mut fields: Vec<_> = d.fields().iter().filter(|f| f.kind == FieldKind::Pos).collect();
    fields.sort_by_key(|f| f.offset);
    for f in fields {
        let start = f.offset as usize;
        out.extend_from_slice(&row[at..start]);
        for k in 0..f.count as usize {
            let o = start + k * size_of::<PosRow>();
            let raw = &row[o..o + size_of::<PosRow>()];
            let pos = bytemuck::try_pod_read_unaligned::<PosRow>(raw).ok().and_then(|r| Pos::from_row(r).ok());
            match pos {
                Some(p) => p.encode(&mut out),
                // A stored row passed validation, so this cannot happen; the raw bytes keep the hash total.
                None => out.extend_from_slice(raw),
            }
        }
        at = start + f.count as usize * size_of::<PosRow>();
    }
    out.extend_from_slice(&row[at..]);
    out
}

/// A `Node` record's bytes (6.6): the five links, creator, the two version entries, the two ops, `born_on`, then 4
/// zero bytes.
fn node_bytes(out: &mut Vec<u8>, n: &pe_sim::Node) {
    for l in [n.parent, n.first_child, n.last_child, n.prev_sibling, n.next_sibling] {
        out.extend_from_slice(&l.to_u128().to_le_bytes());
    }
    out.extend_from_slice(&n.creator.0);
    out.extend_from_slice(&n.version_entry);
    out.extend_from_slice(&n.structural_entry);
    for v in [n.version_op, n.structural_op, n.born_on, 0] {
        out.extend_from_slice(&v.to_le_bytes());
    }
}

/// A row version's bytes (6.6): entry, op, tag (52 bytes; pe-sim's padding is not part of it).
fn version_bytes(out: &mut Vec<u8>, v: &RowVersion) {
    out.extend_from_slice(&v.entry);
    out.extend_from_slice(&v.op.to_le_bytes());
    out.extend_from_slice(&v.tag);
}

fn world_pose_bytes(out: &mut Vec<u8>, w: &Option<WorldTransform>) {
    match w {
        None => out.push(0),
        Some(w) => {
            out.push(1);
            w.pos.encode(out);
            for x in w.rot.iter().chain(&w.scale) {
                out.extend_from_slice(&x.to_le_bytes());
            }
        }
    }
}

/// An object's record (6.6) in `store`, from its rows alone; `None` if it does not exist.
fn record_in(store: &pe_sim::SimStore, id: ObjectId) -> Option<Vec<u8>> {
    if !store.exists(id) {
        return None;
    }
    let mut b = Vec::new();
    b.extend_from_slice(&id.to_u128().to_le_bytes());
    node_bytes(&mut b, &store.node(id));
    let reg = store.registry();
    let mut rows: Vec<(&ComponentDesc, &[u8], RowVersion)> = Vec::new();
    for d in reg.iter() {
        if d.id() == ComponentId::NODE {
            continue;
        }
        if let Some(row) = store.get_raw(d.id(), id) {
            rows.push((d, row, store.row_version(d.id(), id).unwrap_or(RowVersion::NONE)));
        }
    }
    rows.sort_by_key(|(d, ..)| d.id());
    b.extend_from_slice(&(rows.len() as u32).to_le_bytes());
    for (d, row, v) in rows {
        b.extend_from_slice(&d.id().0.to_le_bytes());
        b.extend_from_slice(&schema_hash(d).0);
        // One instance per component in M1 (3.2's instances come with their slice).
        b.extend_from_slice(&0u16.to_le_bytes());
        version_bytes(&mut b, &v);
        lp(&mut b, &canonical_row(d, row));
    }
    Some(b)
}

/// An object's leaf hash in `store`, from its rows alone.
fn leaf_in(store: &pe_sim::SimStore, id: ObjectId) -> Option<Hash> {
    record_in(store, id).map(|r| Hash::of("pe.obj", &r))
}

impl World {
    /// An object's record (6.6), the preimage of its leaf hash; `None` if it does not exist.
    pub fn object_record(&self, id: ObjectId) -> Option<Vec<u8>> {
        record_in(&self.store, id)
    }

    /// An object's leaf hash (6.3): `H("pe.obj" ‖ record)`. The cached one (6.4) if no write touched the object since
    /// the last [`World::refresh_tree_cache`], else computed from its rows.
    pub fn object_hash(&self, id: ObjectId) -> Option<Hash> {
        match self.store.cached_hash(id) {
            Some(h) => Some(Hash(h)),
            None => leaf_in(&self.store, id),
        }
    }

    /// Stores the leaf hash of every object written since the last refresh (6.4), so later roots rehash only the
    /// objects written after it. Roots are equal with or without it; it only saves work. pe-replica's call, at cuts.
    pub fn refresh_tree_cache(&mut self) {
        self.store.refresh_hashes(|s, id| leaf_in(s, id).expect("a live object has a record").0);
    }

    /// Rehashes every object from its rows alone and compares it with its cached leaf (@diverge-eng's R-H3): the
    /// first object whose cached hash differs, as a mismatch at its objects path.
    pub fn verify_tree_cache(&self, domain: DomainId) -> Result<(), Mismatch> {
        for &id in self.store.objects() {
            let Some(cached) = self.store.cached_hash(id) else { continue };
            let computed = leaf_in(&self.store, id).expect("a live object has a record");
            if computed.0 != cached {
                let path = TreePath::Objects { domain, prefix: Nibbles::of_id(id) };
                return Err(Mismatch { path, cached: Hash(cached), computed });
            }
        }
        Ok(())
    }

    /// The config bytes (6.6).
    pub fn config_bytes(&self) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&self.store.seed().to_le_bytes());
        b.extend_from_slice(&self.line.0.0);
        match &self.m1_marker {
            None => b.push(0),
            Some(l) => {
                b.push(1);
                b.extend_from_slice(&l.0.0);
            }
        }
        lp(&mut b, &self.agree_settings);
        b.extend_from_slice(&self.rate.hz().to_le_bytes());
        b.extend_from_slice(&self.depth_limit.to_le_bytes());
        for g in self.gravity {
            b.extend_from_slice(&g.to_le_bytes());
        }
        self.origin.encode(&mut b);
        let s = self.input_settings;
        for v in [s.claim_ms, s.cooldown_ms, s.push_budget, s.reach_mm, s.slack_mm] {
            b.extend_from_slice(&v.to_le_bytes());
        }
        b
    }

    /// The world's line id (6.6, config item 2): all zero until the world has one.
    pub fn line(&self) -> LineId {
        self.line
    }

    /// Continues this world on `line` (F3's n = 2 branch, world-model.md 2.3; @diverge-eng's design v5, L3): the
    /// state stays as it is, and only the config record's line id, so the root, changes. Objects keep the ids they
    /// were made with on the old line. Trusted: called by the replica at the branch's frame, on every machine that
    /// continues the world on the new line, never by an edit.
    pub fn switch_line(&mut self, line: LineId) {
        self.line = line;
    }

    /// Sets pe-agree's settings record (6.6, config item 4): opaque bytes pe-world never parses. Set by the creator at
    /// genesis; later only by an agreed entry at a frame.
    pub fn set_agree_settings(&mut self, bytes: &[u8]) {
        self.agree_settings = Arc::from(bytes);
    }

    /// pe-agree's settings record, as set (empty until then).
    pub fn agree_settings(&self) -> &[u8] {
        &self.agree_settings
    }

    /// The entries of world table `index` (6.6), in key order. An unknown index has none.
    pub fn table_entries(&self, index: u8) -> Vec<(Vec<u8>, Vec<u8>)> {
        let mut e: Entries = Vec::new();
        match index {
            table::REGISTRY => {
                for d in self.store.registry().iter() {
                    let desc = desc_bytes(d);
                    // The registering script asset, zero for an engine component (6.6, slice D).
                    let mut v = self.script_ids.get(&d.id()).copied().unwrap_or([0; 32]).to_vec();
                    v.extend_from_slice(&Hash::of("pe.schema", &desc).0);
                    v.extend_from_slice(&desc);
                    e.push((d.id().0.to_be_bytes().to_vec(), v));
                }
            }
            table::MODULES => {
                // Each asset's current row and pending commit, in asset order (logic.md 6.2a).
                let assets: std::collections::BTreeSet<_> = self.modules.keys().chain(self.pending.keys()).collect();
                for a in assets {
                    let v = crate::script::modules_value(self.modules.get(a), self.pending.get(a));
                    e.push((a.to_vec(), v));
                }
            }
            table::GOVERNANCE => {
                for (k, p) in self.members.iter() {
                    e.push((k.0.to_vec(), p.to_le_bytes().to_vec()));
                }
            }
            table::CLAIMS => {
                for (o, c) in self.claims.iter() {
                    let mut v = c.hand.to_u128().to_le_bytes().to_vec();
                    v.extend_from_slice(&c.member.0);
                    v.extend_from_slice(&c.since.0.to_le_bytes());
                    v.extend_from_slice(&c.until.0.to_le_bytes());
                    e.push((id_be(*o).to_vec(), v));
                }
            }
            table::COOLDOWNS => {
                for ((m, o), u) in self.inputs.cooldowns.iter() {
                    let mut k = m.0.to_vec();
                    k.extend_from_slice(&id_be(*o));
                    e.push((k, u.0.to_le_bytes().to_vec()));
                }
            }
            table::TOMBSTONES => {
                for (id, t) in self.tombstones.iter() {
                    let mut v = t.by.0.0.to_vec();
                    v.extend_from_slice(&t.frame.0.to_le_bytes());
                    v.extend_from_slice(&t.expires.0.to_le_bytes());
                    match t.parent {
                        None => v.push(0),
                        Some(p) => {
                            v.push(1);
                            v.extend_from_slice(&p.to_u128().to_le_bytes());
                        }
                    }
                    v.extend_from_slice(&t.place.to_le_bytes());
                    world_pose_bytes(&mut v, &t.world);
                    node_bytes(&mut v, &t.node);
                    let mut rows: Vec<_> = t.rows.iter().collect();
                    rows.sort_by_key(|(c, ..)| *c);
                    v.extend_from_slice(&(rows.len() as u32).to_le_bytes());
                    for (c, bytes, rv) in rows {
                        v.extend_from_slice(&c.0.to_le_bytes());
                        version_bytes(&mut v, rv);
                        match self.store.registry().get(*c) {
                            Some(d) => lp(&mut v, &canonical_row(d, bytes)),
                            None => lp(&mut v, bytes),
                        }
                    }
                    e.push((id_be(*id).to_vec(), v));
                }
            }
            table::HAND_PACKS => {
                for (hand, p) in self.inputs.packs.iter() {
                    let mut v = p.stamp.0.to_le_bytes().to_vec();
                    v.push(p.hold);
                    v.extend_from_slice(&(p.poses.len() as u32).to_le_bytes());
                    for q in &p.poses {
                        q.pos.encode(&mut v);
                        for x in q.rot.iter().chain([&q.grip]) {
                            v.extend_from_slice(&x.to_le_bytes());
                        }
                        v.push(u8::from(q.engaged));
                    }
                    e.push((id_be(*hand).to_vec(), v));
                }
                // Player bodies' packs (8.2), under 17-byte keys: none in a world without bodies, so its table is
                // the one it always had. Then the whole table in key order.
                if !self.inputs.body_packs.is_empty() || !self.inputs.moves.is_empty() {
                    let key = |id: ObjectId, tag: u8| {
                        let mut k = id_be(id).to_vec();
                        k.push(tag);
                        k
                    };
                    for (hand, p) in self.inputs.body_packs.iter() {
                        e.push((key(*hand, crate::player::KEY_BODY_HAND), crate::player::encode_body_pack(p)));
                    }
                    for (body, p) in self.inputs.moves.iter() {
                        e.push((key(*body, crate::player::KEY_MOVE), crate::player::encode_move_pack(p)));
                    }
                    e.sort_unstable_by(|a, b| a.0.cmp(&b.0));
                }
            }
            table::PUSH_WINDOWS => {
                for (m, q) in self.inputs.pushes.iter() {
                    let mut v = (q.len() as u32).to_le_bytes().to_vec();
                    for (f, n) in q {
                        v.extend_from_slice(&f.0.to_le_bytes());
                        v.extend_from_slice(&n.to_bits().to_le_bytes());
                    }
                    e.push((m.0.to_vec(), v));
                }
            }
            // The branch record (F3's n = 2 branch); empty for a world that never branched, as in all of M1's sessions.
            table::LINES => e = self.branch.entries(),
            _ => {}
        }
        e.sort();
        e
    }

    /// The S2 tree of this world as one domain (6.2), computed from the rows and tables now.
    pub fn tree(&self, domain: DomainId) -> WorldTree<'_> {
        WorldTree::new(self, domain)
    }

    /// The world root of this world as one domain (6.2).
    pub fn root_hash(&self, domain: DomainId) -> Hash {
        self.tree(domain).root
    }
}

/// A table's record (6.6): `count u64 ‖ lp(key) ‖ lp(value) …`, in key order. Its hash adds the context and index.
pub fn table_record(entries: &[(Vec<u8>, Vec<u8>)]) -> Vec<u8> {
    let mut b = (entries.len() as u64).to_le_bytes().to_vec();
    for (k, v) in entries {
        lp(&mut b, k);
        lp(&mut b, v);
    }
    b
}

/// A table's hash (6.6): `H("pe.table" ‖ index u8 ‖ record)`.
pub fn table_hash(index: u8, record: &[u8]) -> Hash {
    let mut h = Hasher::new("pe.table");
    h.write_u8(index).write(record);
    h.finish()
}

/// One node of the canonical trie, with its hash.
enum Trie {
    Leaf { id: ObjectId, object_hash: Hash, hash: Hash },
    Inner { prefix: Nibbles, children: Vec<(u8, Trie)>, hash: Hash },
}

impl Trie {
    fn hash(&self) -> Hash {
        match self {
            Trie::Leaf { hash, .. } | Trie::Inner { hash, .. } => *hash,
        }
    }

    fn node(&self) -> Node {
        match self {
            Trie::Leaf { id, object_hash, .. } => Node::Leaf { id: *id, object_hash: *object_hash },
            Trie::Inner { prefix, children, .. } => {
                Node::Inner { prefix: prefix.clone(), children: children.iter().map(|(n, c)| (*n, c.hash())).collect() }
            }
        }
    }

    /// The canonical trie over `items` (sorted by id, at least one), all sharing their first `depth` nibbles (6.2):
    /// a leaf alone, otherwise an inner node at the depth where the first and last ids first differ.
    fn build(items: &[(ObjectId, Nibbles, Hash)], depth: usize) -> Trie {
        if let [(id, _, object_hash)] = items {
            let hash = Node::Leaf { id: *id, object_hash: *object_hash }.hash().expect("a leaf always hashes");
            return Trie::Leaf { id: *id, object_hash: *object_hash, hash };
        }
        let (first, last) = (items[0].1.as_slice(), items[items.len() - 1].1.as_slice());
        let d = (depth..Nibbles::MAX).find(|&d| first[d] != last[d]).expect("distinct ids differ in some nibble");
        let mut children = Vec::new();
        let mut at = 0;
        while at < items.len() {
            let n = items[at].1.as_slice()[d];
            let end = at + items[at..].iter().take_while(|x| x.1.as_slice()[d] == n).count();
            children.push((n, Trie::build(&items[at..end], d + 1)));
            at = end;
        }
        let prefix = Nibbles::new(first[..d].to_vec()).expect("a prefix of an id");
        let node =
            Node::Inner { prefix: prefix.clone(), children: children.iter().map(|(n, c)| (*n, c.hash())).collect() };
        let hash = node.hash().expect("a canonical inner node always hashes");
        Trie::Inner { prefix, children, hash }
    }

    /// The node covering `prefix` (6.5): the shallowest one whose own prefix starts with it, with the nodes above
    /// it on the way down.
    fn covering<'t>(&'t self, prefix: &Nibbles, path: &mut Vec<&'t Trie>) -> Option<&'t Trie> {
        match self {
            Trie::Leaf { id, .. } => Nibbles::of_id(*id).starts_with(prefix).then_some(self),
            Trie::Inner { prefix: p, children, .. } => {
                if p.starts_with(prefix) {
                    return Some(self);
                }
                if !prefix.starts_with(p) {
                    return None;
                }
                let n = prefix.as_slice()[p.len()];
                let (_, child) = children.iter().find(|(c, _)| *c == n)?;
                path.push(self);
                child.covering(prefix, path)
            }
        }
    }
}

/// pe-world's S2 tree on one world state (6.2, 6.5): built from the rows and tables when made.
pub struct WorldTree<'a> {
    world: &'a World,
    domain: DomainId,
    trie: Option<Trie>,
    tables: Vec<Hash>,
    config: Hash,
    root: Hash,
}

impl<'a> WorldTree<'a> {
    fn new(world: &'a World, domain: DomainId) -> WorldTree<'a> {
        let items: Vec<(ObjectId, Nibbles, Hash)> = world
            .store
            .objects()
            .iter()
            .map(|&id| (id, Nibbles::of_id(id), world.object_hash(id).expect("a live object has a record")))
            .collect();
        let trie = (!items.is_empty()).then(|| Trie::build(&items, 0));
        let tables: Vec<Hash> =
            (0..table::COUNT as u8).map(|i| table_hash(i, &table_record(&world.table_entries(i)))).collect();
        let config = Hash::of("pe.config", &world.config_bytes());
        let mut t = WorldTree { world, domain, trie, tables, config, root: Hash::ZERO };
        t.root = t.world_node().hash().expect("one domain is in order");
        t
    }

    fn objects_root(&self) -> Hash {
        self.trie.as_ref().map_or_else(|| Node::Empty.hash().expect("empty hashes"), Trie::hash)
    }

    fn domain_node(&self) -> Node {
        Node::Domain { domain: self.domain, objects: self.objects_root(), physics: self.world.physics_global }
    }

    fn tables_node(&self) -> Node {
        Node::Tables(self.tables.clone())
    }

    fn world_node(&self) -> Node {
        Node::World {
            frame: self.world.frame(),
            config: self.config,
            tables: self.tables_node().hash().expect("tables hash"),
            domains: vec![(self.domain, self.domain_node().hash().expect("a domain hashes"))],
        }
    }

    fn objects_node(&self, prefix: &Nibbles) -> Node {
        let mut path = Vec::new();
        match self.trie.as_ref().and_then(|t| t.covering(prefix, &mut path)) {
            Some(n) => n.node(),
            None => Node::Empty,
        }
    }
}

impl Tree for WorldTree<'_> {
    fn root(&self) -> Hash {
        self.root
    }

    fn frame(&self) -> Frame {
        self.world.frame()
    }

    fn node(&self, path: &TreePath) -> Option<Node> {
        match path {
            TreePath::World => Some(self.world_node()),
            TreePath::Tables => Some(self.tables_node()),
            TreePath::Domain(d) => (*d == self.domain).then(|| self.domain_node()),
            TreePath::Objects { domain, prefix } => (*domain == self.domain).then(|| self.objects_node(prefix)),
        }
    }

    fn export(&self, keys: &[LeafKey]) -> Vec<Record> {
        let mut out = Vec::new();
        for &key in keys {
            match key {
                LeafKey::Object(id) => {
                    let Some(bytes) = self.world.object_record(id) else { continue };
                    let mut path = Vec::new();
                    let Some(trie) = &self.trie else { continue };
                    if trie.covering(&Nibbles::of_id(id), &mut path).is_none() {
                        continue;
                    }
                    let mut proof = vec![self.world_node(), self.domain_node()];
                    proof.extend(path.iter().map(|t| t.node()));
                    out.push(Record { key, bytes: bytes.into(), proof });
                }
                LeafKey::Table(i) if (i as usize) < table::COUNT => {
                    let bytes = table_record(&self.world.table_entries(i));
                    out.push(Record { key, bytes: bytes.into(), proof: vec![self.world_node(), self.tables_node()] });
                }
                LeafKey::Config => {
                    out.push(Record { key, bytes: self.world.config_bytes().into(), proof: vec![self.world_node()] });
                }
                LeafKey::Table(_) | LeafKey::Physics(_) => {}
            }
        }
        out
    }

    fn object_hashes(&self, ids: &[ObjectId]) -> Vec<(ObjectId, Option<Hash>)> {
        ids.iter().map(|&id| (id, self.world.object_hash(id))).collect()
    }

    /// Every cached leaf against its rows (6.4). The tables, the config and the inner nodes are computed fresh for
    /// each tree, so only the leaves can disagree.
    fn verify(&self) -> Result<(), Mismatch> {
        self.world.verify_tree_cache(self.domain)
    }

    /// The inline-class blobs these leaves reference (3.4; S2 v1.8): an object's rows, and every tombstone's kept
    /// rows when the tombstones table is among the keys, as far as this machine has the bytes.
    fn export_blobs(&self, keys: &[LeafKey]) -> Vec<bytes::Bytes> {
        let ids: Vec<ObjectId> =
            keys.iter().filter_map(|k| if let LeafKey::Object(id) = k { Some(*id) } else { None }).collect();
        let tombstones = keys.contains(&LeafKey::Table(table::TOMBSTONES));
        crate::inline::state_refs(self.world, &ids, tombstones)
            .keys()
            .filter_map(|r| self.world.blobs().get(r))
            .map(|b| bytes::Bytes::copy_from_slice(&b))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 6.6 freezes the field-kind numbers; pe-sim's declaration order is the same today. A reordered or extended
    /// `FieldKind` fails here, not silently in a root.
    #[test]
    fn kind_numbers_are_frozen() {
        use FieldKind::*;
        let all = [
            U8, U16, U32, U64, I8, I16, I32, I64, F32, F64, Bool, ObjectRef, BlobRef, SimBlobRef, Pos, MemberKey,
            EntryRef, Hash,
        ];
        for (i, k) in all.into_iter().enumerate() {
            assert_eq!(kind_code(k), i as u8, "{k:?}");
            assert_eq!(k as u8, i as u8, "pe-sim's FieldKind order moved: {k:?}");
        }
    }
}

#[cfg(test)]
mod unit_tests {
    use super::*;
    use crate::components::{Shape, engine_registry, ids};
    use pe_sim::{Component, FieldDesc};

    /// world-model.md 3.6: the engine's units are in the registry, and a unit is part of the hashed description.
    #[test]
    fn units_are_hashed() {
        let r = engine_registry();
        let units = |c| r.get(c).unwrap().fields().iter().map(|f| (f.name.clone(), f.unit.clone())).collect::<Vec<_>>();
        assert!(units(ids::SHAPE).contains(&("density".into(), "kg/m^3".into())));
        assert!(units(ComponentId::TRANSFORM).contains(&("pos".into(), "m".into())));
        assert!(units(ids::APPEARANCE).iter().all(|(_, u)| u.is_empty()));
        let shape = Shape::desc();
        let bare = ComponentDesc::new(
            shape.id(),
            shape.name(),
            shape.fields().iter().map(|f| FieldDesc { unit: String::new(), ..f.clone() }).collect(),
        )
        .unwrap();
        assert_ne!(schema_hash(&shape), schema_hash(&bare));
        let b = desc_bytes(&shape);
        assert!(b.windows(8 + 6).any(|w| w == [&6u64.to_le_bytes()[..], b"kg/m^3"].concat()));
    }
}
