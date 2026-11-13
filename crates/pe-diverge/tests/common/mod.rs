//! A mock S2 v1.2 replica and a small in-process network for pe-diverge's tests.
//!
//! The mock's state is objects, tables and one physics blob under a real S2 v1 tree: a world root over the tables root
//! and one domain, whose objects sit in a path-compressed nibble trie, every node hashed with `Node::hash`. Leaf,
//! table and physics hashes are cached, as a real replica caches them. Every object's record holds everything a step
//! reads (a counter and a wake frame). A sleeping object keeps its record and cached hash until it wakes, so a damaged
//! record with a stale cache stays invisible to roots until something rehashes it. Bound bodies need the physics
//! image to be imported. Every frame is kept, so an imported record can be checked against an honest peer afterwards.
//!
//! Like pe-replica: only `finalize` moves `Cv` and the floor; `replay_to` verifies `Cv`, replays and re-simulates to
//! the present without moving either; `import_at_cut` checks every record's proof against the target root before
//! changing anything, imports from empty when it holds no root of its own at the cut, and never rehashes anything else.
//! Replays and imports can be made to answer `Pending` for some frames, and a trim at or above the floor makes roots
//! from a frame on `Damaged`.

#![allow(dead_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use pe_diverge::msg::Msg;
use pe_diverge::{Action, Config, Diverge};
use pe_seams::{
    Advance, Cut, CutImport, CutRef, DivergeEvent, DomainId, EntryRef, EntrySource, FinalCut, Finalized, Frame,
    FrameClock, FrameView, GroupSel, Hash, HostTime, ImportError, LeafKey, LineId, MemberKey, Mismatch, Nibbles, Node,
    ObjectId, Outcome, PhysicsImage, Pos, Record, Replica, RigTransform, RootError, SelfTest, Signature, SignedEntry,
    Tree, TreePath, ViewCounters,
};

pub const NS_PER_FRAME: i64 = 16_666_667;
pub const DOMAIN: DomainId = DomainId(0);
/// The world's config bytes, and their hash as the world node names it (S2 v1.3: `H("pe.config", bytes)`).
const CONFIG_BYTES: &[u8] = b"mock config";
fn config_hash() -> Hash {
    let mut h = pe_seams::Hasher::new("pe.config");
    h.write(CONFIG_BYTES);
    h.finish()
}

/// Spread small object numbers over the whole id space, so the trie branches near its top (an odd multiplier is a
/// bijection on u128).
pub fn spread(n: u128) -> u128 {
    n.wrapping_mul(0x9e37_79b9_7f4a_7c15_f39c_c060_5ced_c835)
}
pub fn oid(n: u128) -> ObjectId {
    ObjectId::from_u128(spread(n))
}
pub fn obj(n: u128) -> LeafKey {
    LeafKey::Object(oid(n))
}

fn obj_hash(id: u128, b: &[u8]) -> Hash {
    let mut v = id.to_le_bytes().to_vec();
    v.extend_from_slice(b);
    Hash::of("mock.obj", &v)
}
fn table_hash(t: u8, b: &[u8]) -> Hash {
    let mut v = vec![t];
    v.extend_from_slice(b);
    Hash::of("mock.table", &v)
}
fn phys_hash(b: &[u8]) -> Hash {
    Hash::of("mock.phys", b)
}

fn nib(id: u128, i: usize) -> u8 {
    ((id >> (124 - 4 * i)) & 0xf) as u8
}

/// The ids range a nibble prefix covers.
fn range(prefix: &[u8]) -> (u128, u128) {
    let mut lo = 0u128;
    for (i, n) in prefix.iter().enumerate() {
        lo |= u128::from(*n) << (124 - 4 * i);
    }
    let rest = 128 - 4 * prefix.len() as u32;
    let mask = if rest == 128 { u128::MAX } else { (1u128 << rest) - 1 };
    (lo, lo | mask)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cell {
    pub bytes: Vec<u8>,
    pub cache: Hash,
}

impl Cell {
    fn obj(id: u128, bytes: Vec<u8>) -> Cell {
        Cell { cache: obj_hash(id, &bytes), bytes }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct St {
    pub frame: u64,
    pub objs: BTreeMap<u128, Cell>,
    pub tables: BTreeMap<u8, Cell>,
    pub phys: Cell,
}

fn val(b: &[u8]) -> u64 {
    u64::from_le_bytes(b[..8].try_into().unwrap())
}
fn wake(b: &[u8]) -> u64 {
    u64::from_le_bytes(b[8..16].try_into().unwrap())
}
fn rec(v: u64, w: u64) -> Vec<u8> {
    let mut b = v.to_le_bytes().to_vec();
    b.extend_from_slice(&w.to_le_bytes());
    b
}

impl St {
    fn new(n: u128, sleepers: &[(u128, u64)]) -> St {
        let objs = (1..=n)
            .map(|k| {
                let w = sleepers.iter().find(|s| s.0 == k).map(|s| s.1).unwrap_or(0);
                (spread(k), Cell::obj(spread(k), rec(0, w)))
            })
            .collect();
        let tables = [(0u8, b"settings: v1".to_vec()), (1u8, b"admins: a b".to_vec())]
            .into_iter()
            .map(|(t, b)| (t, Cell { cache: table_hash(t, &b), bytes: b }))
            .collect();
        let p = 0u64.to_le_bytes().to_vec();
        St { frame: 0, objs, tables, phys: Cell { cache: phys_hash(&p), bytes: p } }
    }

    /// No objects, no tables, no physics: what an import from empty starts from.
    fn empty(frame: u64) -> St {
        St {
            frame,
            objs: BTreeMap::new(),
            tables: BTreeMap::new(),
            phys: Cell { bytes: Vec::new(), cache: Hash::ZERO },
        }
    }

    fn ids_under(&self, prefix: &[u8]) -> Vec<u128> {
        let (lo, hi) = range(prefix);
        self.objs.range(lo..=hi).map(|(id, _)| *id).collect()
    }

    /// The objects-trie node covering `prefix`.
    fn obj_node(&self, prefix: &[u8]) -> Node {
        let ids = self.ids_under(prefix);
        match ids.as_slice() {
            [] => Node::Empty,
            [id] => Node::Leaf { id: ObjectId::from_u128(*id), object_hash: self.objs[id].cache },
            [first, .., last] => {
                let lcp = (0..32).take_while(|&i| nib(*first, i) == nib(*last, i)).count();
                let p: Vec<u8> = (0..lcp).map(|i| nib(*first, i)).collect();
                let mut children = Vec::new();
                for n in 0..16u8 {
                    let mut c = p.clone();
                    c.push(n);
                    let node = self.obj_node(&c);
                    if node != Node::Empty {
                        children.push((n, node.hash().unwrap()));
                    }
                }
                Node::Inner { prefix: Nibbles::new(p).unwrap(), children }
            }
        }
    }
    fn tables_node(&self) -> Node {
        Node::Tables(self.tables.values().map(|c| c.cache).collect())
    }
    fn domain_node(&self) -> Node {
        Node::Domain { domain: DOMAIN, objects: self.obj_node(&[]).hash().unwrap(), physics: self.phys.cache }
    }
    fn world_node(&self) -> Node {
        Node::World {
            frame: Frame(self.frame),
            config: config_hash(),
            tables: self.tables_node().hash().unwrap(),
            domains: vec![(DOMAIN, self.domain_node().hash().unwrap())],
        }
    }
    pub fn root(&self) -> Hash {
        self.world_node().hash().unwrap()
    }
    fn node(&self, path: &TreePath) -> Option<Node> {
        match path {
            TreePath::World => Some(self.world_node()),
            TreePath::Tables => Some(self.tables_node()),
            TreePath::Domain(d) if *d == DOMAIN => Some(self.domain_node()),
            TreePath::Objects { domain, prefix } if *domain == DOMAIN => Some(self.obj_node(prefix.as_slice())),
            _ => None,
        }
    }
    fn export(&self, keys: &[LeafKey]) -> Vec<Record> {
        // The nodes on the proofs' paths are built once per call: a whole-state export of a large world would
        // otherwise rebuild the trie for every record.
        let (world, domain, tables) = (self.world_node(), self.domain_node(), self.tables_node());
        let mut memo: BTreeMap<Vec<u8>, Node> = BTreeMap::new();
        keys.iter()
            .filter_map(|k| match k {
                LeafKey::Object(o) => {
                    let id = o.to_u128();
                    let c = self.objs.get(&id)?;
                    let mut proof = vec![world.clone(), domain.clone()];
                    let mut p = Vec::new();
                    while let Node::Inner { prefix, children } =
                        memo.entry(p.clone()).or_insert_with(|| self.obj_node(&p)).clone()
                    {
                        let depth = prefix.len();
                        p = prefix.as_slice().to_vec();
                        p.push(nib(id, depth));
                        proof.push(Node::Inner { prefix, children });
                    }
                    Some(Record { key: *k, bytes: Bytes::from(c.bytes.clone()), proof })
                }
                LeafKey::Table(t) => {
                    let c = self.tables.get(t)?;
                    Some(Record {
                        key: *k,
                        bytes: Bytes::from(c.bytes.clone()),
                        proof: vec![world.clone(), tables.clone()],
                    })
                }
                LeafKey::Config => {
                    Some(Record { key: *k, bytes: Bytes::from_static(CONFIG_BYTES), proof: vec![world.clone()] })
                }
                LeafKey::Physics(_) => None,
            })
            .collect()
    }
    /// The full rehash: the first cached hash that disagrees with its record.
    fn mismatch(&self) -> Option<Mismatch> {
        for (id, c) in &self.objs {
            let fresh = obj_hash(*id, &c.bytes);
            if fresh != c.cache {
                let prefix = Nibbles::of_id(ObjectId::from_u128(*id));
                return Some(Mismatch {
                    path: TreePath::Objects { domain: DOMAIN, prefix },
                    cached: c.cache,
                    computed: fresh,
                });
            }
        }
        for (t, c) in &self.tables {
            let fresh = table_hash(*t, &c.bytes);
            if fresh != c.cache {
                return Some(Mismatch { path: TreePath::Tables, cached: c.cache, computed: fresh });
            }
        }
        let fresh = phys_hash(&self.phys.bytes);
        (fresh != self.phys.cache).then_some(Mismatch {
            path: TreePath::Domain(DOMAIN),
            cached: self.phys.cache,
            computed: fresh,
        })
    }
}

pub struct MockTree {
    st: St,
}

impl Tree for MockTree {
    fn root(&self) -> Hash {
        self.st.root()
    }
    fn frame(&self) -> Frame {
        Frame(self.st.frame)
    }
    fn node(&self, path: &TreePath) -> Option<Node> {
        self.st.node(path)
    }
    fn export(&self, keys: &[LeafKey]) -> Vec<Record> {
        self.st.export(keys)
    }
    fn object_hashes(&self, ids: &[ObjectId]) -> Vec<(ObjectId, Option<Hash>)> {
        ids.iter().map(|id| (*id, self.st.objs.get(&id.to_u128()).map(|c| c.cache))).collect()
    }
    fn verify(&self) -> Result<(), Mismatch> {
        match self.st.mismatch() {
            Some(m) => Err(m),
            None => Ok(()),
        }
    }
}

/// Whether a record's proof leads from `root` down to its bytes, every node checked by `Node::hash` against the hash
/// its parent names.
fn proves(r: &Record, root: &Hash) -> bool {
    let Some(Node::World { tables, domains, .. }) = r.proof.first() else { return false };
    if r.proof[0].hash() != Some(*root) {
        return false;
    }
    match r.key {
        LeafKey::Table(t) => match r.proof.get(1) {
            Some(n @ Node::Tables(hs)) if r.proof.len() == 2 && n.hash() == Some(*tables) => {
                hs.get(usize::from(t)) == Some(&table_hash(t, &r.bytes))
            }
            _ => false,
        },
        LeafKey::Object(o) => {
            let id = o.to_u128();
            let Some(d @ Node::Domain { domain, objects, .. }) = r.proof.get(1) else { return false };
            if *domain != DOMAIN || domains.iter().find(|x| x.0 == DOMAIN).map(|x| x.1) != d.hash() {
                return false;
            }
            let mut named = *objects;
            for n in &r.proof[2..] {
                let Node::Inner { prefix, children } = n else { return false };
                if n.hash() != Some(named) || !Nibbles::of_id(o).starts_with(prefix) {
                    return false;
                }
                let Some(h) = children.iter().find(|c| c.0 == nib(id, prefix.len())).map(|c| c.1) else {
                    return false;
                };
                named = h;
            }
            Node::Leaf { id: o, object_hash: obj_hash(id, &r.bytes) }.hash() == Some(named)
        }
        LeafKey::Config => {
            let Some(Node::World { config, .. }) = r.proof.first() else { return false };
            r.proof.len() == 1 && {
                let mut h = pe_seams::Hasher::new("pe.config");
                h.write(&r.bytes);
                h.finish() == *config
            }
        }
        LeafKey::Physics(_) => false,
    }
}

pub struct Mock {
    pub now: u64,
    pub states: BTreeMap<u64, St>,
    roots: BTreeMap<u64, Hash>,
    pub cv: Option<u64>,
    pub floor: u64,
    /// finalize calls for a cut below the floor: pe-replica counts each as a protocol fault.
    pub below_floor: u32,
    /// A computational fault: this object steps differently here.
    pub faulty: Option<u128>,
    /// Bound bodies: importing one needs the physics image.
    pub bound: BTreeSet<u128>,
    /// Every record imported, with the frame it was imported at; and every import asked for.
    pub imported: Vec<(u64, Record)>,
    pub imports: Vec<CutImport>,
    /// Replays and imports answer `Pending` for this many frames after they start.
    pub pending_frames: u64,
    replay: Option<(CutRef, u64)>,
    replayed: Option<(CutRef, Hash)>,
    importing: Option<(CutRef, Hash, u64)>,
    /// S3 trimmed at or above the floor from this frame on: a root at any cut from it is `Damaged`.
    pub damaged_from: Option<u64>,
    /// S3's `kept_from`: a replay from a `Cv` below it is `NotKept`.
    pub kept_from: u64,
    pub finalize_calls: u64,
    /// Every import is refused with this, after it is recorded (a replica that cannot take the agreed state, 4.3).
    pub refuse: Option<ImportError>,
}

impl Mock {
    pub fn new(objects: u128, sleepers: &[(u128, u64)]) -> Mock {
        let mut m = Mock {
            now: 0,
            states: BTreeMap::new(),
            roots: BTreeMap::new(),
            cv: None,
            floor: 0,
            below_floor: 0,
            faulty: None,
            bound: (1..=objects).filter(|k| k % 10 == 0).map(spread).collect(),
            imported: Vec::new(),
            imports: Vec::new(),
            pending_frames: 0,
            replay: None,
            replayed: None,
            importing: None,
            damaged_from: None,
            kept_from: 0,
            finalize_calls: 0,
            refuse: None,
        };
        m.put(St::new(objects, sleepers));
        m
    }

    fn put(&mut self, st: St) {
        self.roots.insert(st.frame, st.root());
        self.states.insert(st.frame, st);
    }

    fn step(&self, s: &St, frame: u64) -> St {
        let mut n = s.clone();
        n.frame = frame;
        for (id, c) in n.objs.iter_mut() {
            if frame < wake(&c.bytes) {
                continue;
            }
            let extra = u64::from(self.faulty.map(spread) == Some(*id));
            *c = Cell::obj(*id, rec(val(&c.bytes).wrapping_add((*id as u64 % 7) + 1 + extra), wake(&c.bytes)));
        }
        n.phys.bytes = (val(&n.phys.bytes) + 3).to_le_bytes().to_vec();
        n.phys.cache = phys_hash(&n.phys.bytes);
        n
    }

    pub fn advance_one(&mut self) {
        let next = self.step(&self.states[&self.now], self.now + 1);
        self.now += 1;
        self.put(next);
    }

    fn restep_from(&mut self, from: u64) {
        for f in from + 1..=self.now {
            let next = self.step(&self.states[&(f - 1)], f);
            self.put(next);
        }
    }

    pub fn root_of(&self, f: u64) -> Hash {
        self.roots[&f]
    }

    fn edit(&mut self, at: u64, change: impl FnOnce(&mut St)) {
        let mut st = self.states[&at].clone();
        change(&mut st);
        self.put(st);
        self.restep_from(at);
    }

    /// Damage one object's record at `at`. With `rehash`, its cached hash follows (a fault in live state hashed as
    /// usual); without, the cache goes stale (damage in a saved state).
    pub fn corrupt(&mut self, n: u128, at: u64, rehash: bool) {
        let id = spread(n);
        self.edit(at, |st| {
            let c = st.objs.get_mut(&id).unwrap();
            c.bytes[0] ^= 0x5a;
            if rehash {
                c.cache = obj_hash(id, &c.bytes);
            }
        });
    }

    /// An object this replica alone has, from `at` on (hashed as usual).
    pub fn spawn_extra(&mut self, id: u128, at: u64) {
        self.edit(at, |st| {
            st.objs.insert(id, Cell::obj(id, rec(0, 0)));
        });
    }

    /// An object this replica alone lacks, from `at` on.
    pub fn remove(&mut self, id: u128, at: u64) {
        self.edit(at, |st| {
            st.objs.remove(&id);
        });
    }

    /// Damage a table (governance) at `at`, hashed as usual.
    pub fn corrupt_table(&mut self, t: u8, at: u64) {
        self.edit(at, |st| {
            let c = st.tables.get_mut(&t).unwrap();
            c.bytes[0] ^= 1;
            c.cache = table_hash(t, &c.bytes);
        });
    }

    /// Damage the physics blob at `at`, hashed as usual.
    pub fn corrupt_physics(&mut self, at: u64) {
        self.edit(at, |st| {
            st.phys.bytes[0] ^= 1;
            st.phys.cache = phys_hash(&st.phys.bytes);
        });
    }

    /// S3's time cap trimmed frames from `at` up (S3 v1.2) after this replica stepped them wrongly: from `at` on its
    /// state is wrong, and any root at a cut from `at` is `Damaged`.
    pub fn trim_from(&mut self, at: u64) {
        self.damaged_from = Some(at);
        self.corrupt(1, at, true);
        self.remove(spread(2), at);
    }

    fn held(&self, root: &Hash) -> Option<u64> {
        self.roots.iter().rev().find(|(_, r)| *r == root).map(|(f, _)| *f)
    }

    fn damaged_at(&self, f: u64) -> bool {
        self.damaged_from.is_some_and(|d| d <= f)
    }

    /// Restore `Cv`, verify it, and re-simulate from it to the present, which becomes the live state.
    fn run_replay(&mut self, cut: &CutRef) -> Result<Hash, RootError> {
        let cv = self.cv.ok_or(RootError::NotKept)?;
        if self.states[&cv].mismatch().is_some() || self.damaged_at(cut.frame.0) {
            return Err(RootError::Damaged);
        }
        self.restep_from(cv);
        Ok(self.root_of(cut.frame.0))
    }
}

impl Replica for Mock {
    fn domain(&self) -> DomainId {
        DOMAIN
    }
    fn frame(&self) -> Frame {
        Frame(self.now + 1)
    }
    fn set_budget(&mut self, _simulated: Duration) {}
    fn advance(&mut self, present: Frame, _source: &mut dyn EntrySource) -> Advance {
        let mut stepped = 0;
        while self.now + 1 < present.0 {
            self.advance_one();
            stepped += 1;
        }
        Advance { rolled_back_to: None, resimulated: 0, stepped, caught_up: true }
    }
    fn outcome(&self, _entry: &EntryRef) -> Option<(Frame, Outcome)> {
        None
    }
    fn root_at_cut(&mut self, cut: &Cut, _source: &mut dyn EntrySource) -> Result<Hash, RootError> {
        let f = cut.at.frame.0;
        if f > self.now {
            return Err(RootError::NotYetSimulated);
        }
        if self.damaged_at(f) {
            return Err(RootError::Damaged);
        }
        self.roots.get(&f).copied().ok_or(RootError::NotKept)
    }
    fn finalize(&mut self, cut: &FinalCut, _source: &mut dyn EntrySource) -> Finalized {
        self.finalize_calls += 1;
        let f = cut.cut.at.frame.0;
        if f < self.floor {
            // As pe-replica: a cut below the floor is a protocol fault, answered NotReady.
            self.below_floor += 1;
            return Finalized::NotReady;
        }
        if f > self.now || self.importing.is_some_and(|i| i.0 == cut.cut.at && self.now < i.2) {
            return Finalized::NotReady;
        }
        if self.damaged_at(f) {
            return Finalized::Damaged;
        }
        let mine = self.root_of(f);
        self.floor = self.floor.max(f);
        if mine == cut.root {
            self.cv = Some(f);
            Finalized::Agreed
        } else {
            Finalized::Diverged { mine }
        }
    }
    fn replay_to(&mut self, cut: &Cut, _source: &mut dyn EntrySource) -> Result<Hash, RootError> {
        if let Some((_, root)) = self.replayed.filter(|r| r.0 == cut.at) {
            return Ok(root);
        }
        match self.replay {
            Some((c, ready)) if c == cut.at => {
                if self.now < ready {
                    return Err(RootError::Pending);
                }
                self.replay = None;
            }
            _ => {
                if self.cv.is_none_or(|cv| cv < self.kept_from) {
                    return Err(RootError::NotKept);
                }
                if self.pending_frames > 0 {
                    self.replay = Some((cut.at, self.now + self.pending_frames));
                    return Err(RootError::Pending);
                }
            }
        }
        let root = self.run_replay(&cut.at)?;
        self.replayed = Some((cut.at, root));
        Ok(root)
    }
    fn tree(&self, root: &Hash) -> Option<Box<dyn Tree + '_>> {
        let f = self.held(root)?;
        Some(Box::new(MockTree { st: self.states[&f].clone() }))
    }
    fn physics_image(&self, root: &Hash) -> Option<PhysicsImage> {
        let f = self.held(root)?;
        let p = &self.states[&f].phys;
        Some(PhysicsImage {
            domain: DOMAIN,
            frame: Frame(f),
            global: phys_hash(&p.bytes),
            bytes: Bytes::from(p.bytes.clone()),
        })
    }
    fn import_at_cut(
        &mut self,
        _group: &GroupSel,
        import: CutImport,
        _source: &mut dyn EntrySource,
    ) -> Result<Hash, ImportError> {
        let at = import.cut.at;
        if let Some((c, target, ready)) = self.importing.filter(|i| i.0 == at && i.1 == import.target_root) {
            if self.now < ready {
                return Err(ImportError::Pending);
            }
            self.importing = None;
            let _ = c;
            return Ok(target);
        }
        self.imports.push(import.clone());
        if let Some(e) = &self.refuse {
            return Err(e.clone());
        }
        let f = at.frame.0;
        if f > self.now {
            return Err(ImportError::NotKept);
        }
        // No root of mine at the cut: the import is the whole state, from empty.
        let mut st = if self.damaged_at(f) { St::empty(f) } else { self.states[&f].clone() };
        for (i, r) in import.records.iter().enumerate() {
            if !proves(r, &import.target_root) {
                return Err(ImportError::BadProof(i));
            }
            if let LeafKey::Object(o) = r.key
                && self.bound.contains(&o.to_u128())
                && import.physics.is_none()
            {
                return Err(ImportError::NeedsPhysicsImage(o));
            }
        }
        if let Some(img) = &import.physics
            && phys_hash(&img.bytes) != img.global
        {
            return Err(ImportError::PhysicsCheck);
        }
        for k in &import.absent {
            match k {
                LeafKey::Object(o) => {
                    st.objs.remove(&o.to_u128());
                }
                LeafKey::Table(t) => {
                    st.tables.remove(t);
                }
                LeafKey::Physics(_) | LeafKey::Config => {}
            }
        }
        for r in &import.records {
            match r.key {
                LeafKey::Object(o) => {
                    st.objs.insert(o.to_u128(), Cell::obj(o.to_u128(), r.bytes.to_vec()));
                }
                LeafKey::Table(t) => {
                    st.tables.insert(t, Cell { cache: table_hash(t, &r.bytes), bytes: r.bytes.to_vec() });
                }
                // The mock's config is constant: the record checked, and there is nothing to set.
                LeafKey::Physics(_) | LeafKey::Config => {}
            }
        }
        if let Some(img) = &import.physics {
            st.phys = Cell { cache: img.global, bytes: img.bytes.to_vec() };
        }
        let got = st.root();
        if got != import.target_root {
            return Err(ImportError::RootMismatch { got });
        }
        for r in &import.records {
            self.imported.push((f, r.clone()));
        }
        self.put(st);
        self.restep_from(f);
        if self.damaged_at(f) {
            self.damaged_from = None;
        }
        if self.pending_frames > 0 {
            self.importing = Some((at, got, self.now + self.pending_frames));
            return Err(ImportError::Pending);
        }
        Ok(got)
    }
    fn view(&self) -> Arc<FrameView> {
        Arc::new(FrameView {
            frame: Frame(self.now),
            domain: DOMAIN,
            sim_time: HostTime(self.now as i64 * NS_PER_FRAME),
            final_frame: self.cv.map(Frame),
            local_rig: RigTransform { pos: Pos::ORIGIN, rotation: [0.0, 0.0, 0.0, 1.0], scale: 1.0 },
            objects: Vec::new(),
            effects: Vec::new(),
            retracted: Vec::new(),
            counters: ViewCounters::default(),
            changes: Vec::new(),
        })
    }
}

/// No entries: the mock steps on its own rules. It holds the newest final cut, set when the net decides one, unless
/// the peer is made to lag on finality.
#[derive(Default)]
pub struct NoEntries {
    pub final_cut: Option<FinalCut>,
}

impl EntrySource for NoEntries {
    fn final_cut(&self) -> Option<&FinalCut> {
        self.final_cut.as_ref()
    }
    fn entries_at(&self, _frame: Frame) -> Option<&[SignedEntry]> {
        Some(&[])
    }
    fn kept_from(&self) -> Frame {
        Frame(0)
    }
    fn keep_final_from(&mut self, _cv: Frame, _floor: Frame) {}
    fn take_changed_from(&mut self) -> Option<Frame> {
        None
    }
}

pub struct Clock;

impl FrameClock for Clock {
    fn rate_hz(&self) -> u32 {
        60
    }
    fn frame_at(&self, now: HostTime) -> Option<Frame> {
        Some(Frame((now.0 / NS_PER_FRAME) as u64))
    }
    fn host_time_of(&self, frame: Frame) -> HostTime {
        HostTime(frame.0 as i64 * NS_PER_FRAME)
    }
}

/// How a source lies.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lie {
    None,
    /// Flips a byte of every record it serves, keeping the proof.
    Records,
    /// Changes a child hash in every inner node it serves.
    Nodes,
    /// Answers every request `Busy`, forever, naming this wait in microseconds.
    Busy(u64),
}

pub struct Peer {
    pub key: MemberKey,
    pub d: Diverge,
    pub r: Mock,
    pub src: NoEntries,
    pub lie: Lie,
    /// Answers nothing.
    pub silent: bool,
    pub events: Vec<DivergeEvent>,
    pub actions: Vec<Action>,
    /// Repair-lane messages this peer sent.
    pub sent: Vec<Msg>,
    /// Its self-test fails (a lasting fault the guard catches).
    pub self_test_fails: bool,
    /// Its verdict for a split, once run: the split, whether it passed, and the frame it ran.
    pub verdict: Option<(CutRef, bool, u64)>,
    /// Frames before the other peers see its verdict (it sits in a slow revote).
    pub verdict_delay: u64,
    /// `RootSplit` emissions this peer misses before it sees one (it was in another view).
    pub skip_splits: u32,
}

pub struct Net {
    pub peers: Vec<Peer>,
    pub cut_every: u64,
    pub height: u64,
    pub line: LineId,
    pending_split: Option<Cut>,
}

pub fn key(i: usize) -> MemberKey {
    MemberKey(Hash::of("mock.member", &[i as u8]).0)
}

impl Net {
    pub fn new(n: usize, objects: u128, sleepers: &[(u128, u64)]) -> Net {
        Net::with_config(n, objects, sleepers, Config::default())
    }

    pub fn with_config(n: usize, objects: u128, sleepers: &[(u128, u64)], cfg: Config) -> Net {
        let peers = (0..n)
            .map(|i| Peer {
                key: key(i),
                d: Diverge::new(key(i), cfg.clone()),
                r: Mock::new(objects, sleepers),
                src: NoEntries::default(),
                lie: Lie::None,
                silent: false,
                events: Vec::new(),
                actions: Vec::new(),
                sent: Vec::new(),
                self_test_fails: false,
                verdict: None,
                verdict_delay: 0,
                skip_splits: 0,
            })
            .collect();
        Net { peers, cut_every: 8, height: 0, line: LineId::origin(), pending_split: None }
    }

    /// The cut of the root split under way, if any.
    pub fn split_cut(&self) -> Option<&Cut> {
        self.pending_split.as_ref()
    }

    pub fn now(&self) -> HostTime {
        HostTime(self.peers[0].r.now as i64 * NS_PER_FRAME)
    }

    pub fn index(&self, k: &MemberKey) -> usize {
        self.peers.iter().position(|p| p.key == *k).unwrap()
    }

    /// The agreement stand-in: the quorum root is a root at least `quorum(n)` peers hold; with none, a root split.
    fn decide(&mut self, cut: Cut) {
        let now = self.now();
        let frame = self.peers[0].r.now;
        let mut groups: BTreeMap<Hash, Vec<MemberKey>> = BTreeMap::new();
        for p in &mut self.peers {
            if let Ok(r) = p.r.root_at_cut(&cut, &mut p.src) {
                groups.entry(r).or_default().push(p.key);
            }
        }
        let q = pe_seams::quorum(self.peers.len());
        match groups.iter().find(|(_, v)| v.len() >= q) {
            Some((root, voters)) => {
                self.pending_split = None;
                let fc =
                    FinalCut { cut, root: *root, voters: voters.iter().map(|k| (*k, Signature([0; 64]))).collect() };
                for p in &mut self.peers {
                    p.src.final_cut = Some(fc.clone());
                    let f = p.r.finalize(&fc, &mut p.src);
                    p.d.on_final(&fc, f, &mut p.r, &mut p.src, now, &Clock);
                }
            }
            None => {
                // Each observer sees each voter's verdict for this split once it has arrived (S3 v1.3): its own at
                // once, another's `verdict_delay` frames after it ran.
                let shown: Vec<(MemberKey, CutRef, bool, u64, u64)> = self
                    .peers
                    .iter()
                    .filter_map(|p| p.verdict.map(|(c, ok, at)| (p.key, c, ok, at, p.verdict_delay)))
                    .collect();
                for p in &mut self.peers {
                    if p.skip_splits > 0 {
                        p.skip_splits -= 1;
                        continue;
                    }
                    let me = p.key;
                    let test = |m: &MemberKey| match shown.iter().find(|v| v.0 == *m) {
                        Some((v, c, ok, at, delay)) if *c == cut.at && (*v == me || frame >= at + delay) => {
                            if *ok {
                                SelfTest::Passed { split: *c }
                            } else {
                                SelfTest::Failed { split: *c }
                            }
                        }
                        _ => SelfTest::NotRun,
                    };
                    let roots: Vec<(Hash, Vec<(MemberKey, SelfTest)>)> =
                        groups.iter().map(|(h, v)| (*h, v.iter().map(|m| (*m, test(m))).collect())).collect();
                    p.d.on_root_split(&cut, &roots, &mut p.r, &mut p.src, now, &Clock);
                }
                self.pending_split = Some(cut);
            }
        }
    }

    pub fn step(&mut self) {
        for p in &mut self.peers {
            p.r.advance_one();
        }
        let f = self.peers[0].r.now;
        if f.is_multiple_of(self.cut_every) {
            match self.pending_split.take() {
                Some(cut) => self.decide(cut),
                None => {
                    self.height += 1;
                    let cut = Cut {
                        at: CutRef { line: self.line, height: self.height, frame: Frame(f - 2) },
                        heads: Vec::new(),
                    };
                    self.decide(cut);
                }
            }
        }
        self.deliver();
        // The driver runs a self-test pe-diverge asks for, once per split.
        let frame = self.peers[0].r.now;
        for p in &mut self.peers {
            let asked = p.actions.iter().rev().find_map(|a| match a {
                Action::RunSelfTest { cut } => Some(*cut),
                _ => None,
            });
            if let Some(c) = asked
                && p.verdict.is_none_or(|v| v.0 != c)
            {
                p.verdict = Some((c, !p.self_test_fails, frame));
            }
        }
    }

    pub fn run(&mut self, frames: u64) {
        for _ in 0..frames {
            self.step();
        }
    }

    fn deliver(&mut self) {
        let now = self.now();
        for _ in 0..64 {
            let mut batch = Vec::new();
            for p in self.peers.iter_mut() {
                for o in p.d.tick(now, &Clock, &mut p.r, &mut p.src) {
                    if let Ok(m) = Msg::decode(&o.bytes) {
                        p.sent.push(m);
                    }
                    batch.push((p.key, o));
                }
                p.events.extend(p.d.events());
                p.actions.extend(p.d.actions());
            }
            if batch.is_empty() {
                break;
            }
            for (from, o) in batch {
                let fi = self.index(&from);
                if self.peers[fi].silent {
                    continue;
                }
                let bytes = lie(self.peers[fi].lie, &o.bytes);
                let ti = self.index(&o.to);
                let p = &mut self.peers[ti];
                let _ = p.d.receive(from, &bytes, &mut p.r, &mut p.src, now, &Clock);
            }
        }
        for p in &mut self.peers {
            p.events.extend(p.d.events());
            p.actions.extend(p.d.actions());
        }
    }

    pub fn converged(&self) -> bool {
        for p in &self.peers {
            assert_eq!(p.r.below_floor, 0, "finalize was called below the replica's floor");
        }
        let f = self.peers[0].r.now;
        self.peers.iter().all(|p| p.r.states[&f] == self.peers[0].r.states[&f])
    }

    /// Every record `victim` imported equals `honest`'s record of that leaf at that frame.
    pub fn imported_only_truth(&self, victim: usize, honest: usize) -> bool {
        self.peers[victim].r.imported.iter().all(|(f, r)| {
            let st = &self.peers[honest].r.states[f];
            match r.key {
                LeafKey::Object(id) => st.objs.get(&id.to_u128()).is_some_and(|c| c.bytes == r.bytes[..]),
                LeafKey::Table(t) => st.tables.get(&t).is_some_and(|c| c.bytes == r.bytes[..]),
                LeafKey::Physics(_) => true,
                LeafKey::Config => r.bytes[..] == *CONFIG_BYTES,
            }
        })
    }
}

/// What a lying source sends instead.
fn lie(how: Lie, bytes: &[u8]) -> Vec<u8> {
    match (how, Msg::decode(bytes)) {
        (Lie::Records, Ok(Msg::RecordReply { cut, root, mut records })) => {
            for r in &mut records {
                let mut b = r.bytes.to_vec();
                if let Some(x) = b.first_mut() {
                    *x ^= 1;
                }
                r.bytes = Bytes::from(b);
            }
            Msg::RecordReply { cut, root, records }.encode().unwrap().to_vec()
        }
        (Lie::Nodes, Ok(Msg::NodeReply { cut, root, mut nodes })) => {
            for (_, n) in &mut nodes {
                match n {
                    Node::Inner { children, .. } => children[0].1.0[0] ^= 1,
                    Node::World { domains, .. } => domains[0].1.0[0] ^= 1,
                    Node::Domain { objects, .. } => objects.0[0] ^= 1,
                    Node::Leaf { object_hash, .. } => object_hash.0[0] ^= 1,
                    _ => {}
                }
            }
            Msg::NodeReply { cut, root, nodes }.encode().unwrap().to_vec()
        }
        (Lie::Busy(wait_us), Ok(m @ (Msg::NodeReply { .. } | Msg::RecordReply { .. } | Msg::PhysicsReply { .. }))) => {
            let (cut, root) = m.head();
            Msg::Busy { cut, root, retry_after_us: wait_us }.encode().unwrap().to_vec()
        }
        _ => bytes.to_vec(),
    }
}
