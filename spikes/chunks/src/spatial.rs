//! Spatial indexes for the relevance query, the user's octree suggestion against alternatives:
//!
//! * [`ChunkOctree`] with `loose = false`: a classic (tight) octree rooted at every level-0 chunk;
//!   an object sinks to the deepest node that fully contains its box.
//! * [`ChunkOctree`] with `loose = true`: a loose octree (k = 2) rooted at every chunk; an object
//!   goes to the deepest node at least twice its radius, chosen from its centre in O(depth).
//! * [`HashGrid`]: a hashed multi-level grid (an implicit loose octree): one sparse map per level,
//!   objects filed at the level whose cell equals their relevance distance, so a query reads the
//!   3x3x3 neighbourhood per occupied level. `BTreeMap` (ordered) or `HashMap` storage.
//!
//! Objects larger than a chunk go to a shared [`HashGrid`] in the octrees too (an octree rooted at
//! a chunk cannot hold them).
//!
//! **Relevance** (R16 addendum 2): object `o` is simulated for a player at `p` when
//! `o.r >= min_r` (not too small for that player's scale) and `|p - o| <= ratio * o.r`
//! (its angular size is large enough). Results are returned sorted by id: the structures are
//! caches, never sim state, so their internal layout may differ between peers and is never
//! rolled back.

use std::collections::{BTreeMap, HashMap};

use crate::coords::{cell_size, level_for_extent, CellKey, WorldPos};

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Entry {
    pub id: u32,
    pub pos: WorldPos,
    pub r: f64,
}

#[derive(Clone, Copy, Debug)]
pub struct Query {
    pub center: WorldPos,
    pub ratio: f64,
    pub min_r: f64,
}

impl Query {
    pub fn accepts(&self, e: &Entry) -> bool {
        e.r >= self.min_r && self.center.dist2(&e.pos) <= (self.ratio * e.r) * (self.ratio * e.r)
    }
}

#[derive(Default, Clone, Copy, Debug)]
pub struct QueryStats {
    pub nodes: u64,
    pub tested: u64,
}

pub trait Spatial {
    fn name(&self) -> String;
    fn insert(&mut self, e: Entry);
    /// Move object `id` (radius unchanged) to `pos`.
    fn update(&mut self, id: u32, pos: WorldPos);
    fn remove(&mut self, id: u32);
    /// Appends matching ids (unsorted).
    fn query(&self, q: &Query, out: &mut Vec<u32>, st: &mut QueryStats);
    fn mem_bytes(&self) -> usize;
    fn max_depth(&self) -> i32 {
        0
    }
}

pub fn query_sorted(s: &dyn Spatial, q: &Query, st: &mut QueryStats) -> Vec<u32> {
    let mut v = Vec::new();
    s.query(q, &mut v, st);
    v.sort_unstable();
    v
}

pub fn brute_force(all: &[Entry], q: &Query) -> Vec<u32> {
    let mut v: Vec<u32> = all.iter().filter(|e| q.accepts(e)).map(|e| e.id).collect();
    v.sort_unstable();
    v
}

const NONE: u32 = u32::MAX;

#[derive(Clone, Copy)]
struct Loc {
    a: u32,
    idx: u32,
}

// ---- hashed multi-level grid ---------------------------------------------------------------------

pub trait CellMap: Default {
    const NAME: &'static str;
    fn get(&self, k: &CellKey) -> Option<&Vec<Entry>>;
    fn get_mut(&mut self, k: &CellKey) -> Option<&mut Vec<Entry>>;
    fn get_or_insert(&mut self, k: CellKey) -> &mut Vec<Entry>;
    fn remove(&mut self, k: &CellKey);
    fn len(&self) -> usize;
    fn vec_bytes(&self) -> usize;
    /// Bytes per map slot beyond key + value (node or bucket overhead), an estimate.
    const SLOT_OVERHEAD: usize;
}

#[derive(Default)]
pub struct BTreeCells(BTreeMap<CellKey, Vec<Entry>>);
#[derive(Default)]
pub struct HashCells(HashMap<CellKey, Vec<Entry>>);

impl CellMap for BTreeCells {
    const NAME: &'static str = "hashed-grid/btree";
    const SLOT_OVERHEAD: usize = 16;
    fn get(&self, k: &CellKey) -> Option<&Vec<Entry>> {
        self.0.get(k)
    }
    fn get_mut(&mut self, k: &CellKey) -> Option<&mut Vec<Entry>> {
        self.0.get_mut(k)
    }
    fn get_or_insert(&mut self, k: CellKey) -> &mut Vec<Entry> {
        self.0.entry(k).or_default()
    }
    fn remove(&mut self, k: &CellKey) {
        self.0.remove(k);
    }
    fn len(&self) -> usize {
        self.0.len()
    }
    fn vec_bytes(&self) -> usize {
        self.0.values().map(|v| v.capacity() * size_of::<Entry>()).sum()
    }
}

impl CellMap for HashCells {
    const NAME: &'static str = "hashed-grid/hashmap";
    const SLOT_OVERHEAD: usize = 8;
    fn get(&self, k: &CellKey) -> Option<&Vec<Entry>> {
        self.0.get(k)
    }
    fn get_mut(&mut self, k: &CellKey) -> Option<&mut Vec<Entry>> {
        self.0.get_mut(k)
    }
    fn get_or_insert(&mut self, k: CellKey) -> &mut Vec<Entry> {
        self.0.entry(k).or_default()
    }
    fn remove(&mut self, k: &CellKey) {
        self.0.remove(k);
    }
    fn len(&self) -> usize {
        self.0.len()
    }
    fn vec_bytes(&self) -> usize {
        // A hashbrown table keeps ~1/8 slack plus one control byte per slot.
        self.0.values().map(|v| v.capacity() * size_of::<Entry>()).sum::<usize>()
            + self.0.capacity() / 8 * (size_of::<CellKey>() + size_of::<Vec<Entry>>())
    }
}

/// Objects are filed at the smallest level whose cell is at least `file_ratio * r` wide, keyed by
/// the cell of their centre.
pub struct HashGrid<M: CellMap> {
    file_ratio: f64,
    cells: M,
    /// occupied level -> object count
    levels: BTreeMap<i32, u32>,
    loc: Vec<(CellKey, u32)>,
}

impl<M: CellMap> HashGrid<M> {
    pub fn new(file_ratio: f64) -> Self {
        HashGrid { file_ratio, cells: M::default(), levels: BTreeMap::new(), loc: Vec::new() }
    }

    fn level_of(&self, r: f64) -> i32 {
        level_for_extent(self.file_ratio * r)
    }

    fn put(&mut self, e: Entry, key: CellKey) {
        let v = self.cells.get_or_insert(key);
        v.push(e);
        let idx = (v.len() - 1) as u32;
        let id = e.id as usize;
        if self.loc.len() <= id {
            self.loc.resize(id + 1, (key, NONE));
        }
        self.loc[id] = (key, idx);
    }

    fn take(&mut self, id: u32) -> Entry {
        let (key, idx) = self.loc[id as usize];
        assert!(idx != NONE, "unknown id {id}");
        let v = self.cells.get_mut(&key).expect("cell");
        let e = v.swap_remove(idx as usize);
        if (idx as usize) < v.len() {
            let moved = v[idx as usize].id;
            self.loc[moved as usize].1 = idx;
        }
        if v.is_empty() {
            self.cells.remove(&key);
        }
        self.loc[id as usize].1 = NONE;
        e
    }
}

impl<M: CellMap> Spatial for HashGrid<M> {
    fn name(&self) -> String {
        M::NAME.to_string()
    }

    fn insert(&mut self, e: Entry) {
        let level = self.level_of(e.r);
        *self.levels.entry(level).or_default() += 1;
        let key = e.pos.cell(level);
        self.put(e, key);
    }

    fn update(&mut self, id: u32, pos: WorldPos) {
        let (key, idx) = self.loc[id as usize];
        let new_key = pos.cell(key.level);
        if new_key == key {
            self.cells.get_mut(&key).expect("cell")[idx as usize].pos = pos;
            return;
        }
        let mut e = self.take(id);
        e.pos = pos;
        self.put(e, new_key);
    }

    fn remove(&mut self, id: u32) {
        let level = self.loc[id as usize].0.level;
        self.take(id);
        let n = self.levels.get_mut(&level).expect("level");
        *n -= 1;
        if *n == 0 {
            self.levels.remove(&level);
        }
    }

    fn query(&self, q: &Query, out: &mut Vec<u32>, st: &mut QueryStats) {
        for (&level, _) in &self.levels {
            let s = cell_size(level);
            let r_max = s / self.file_ratio;
            if r_max < q.min_r {
                continue;
            }
            let reach = q.ratio * r_max;
            let n = (reach / s).ceil() as i128;
            let c0 = q.center.cell(level).c;
            for dx in -n..=n {
                for dy in -n..=n {
                    for dz in -n..=n {
                        let key = CellKey { level, c: [c0[0] + dx, c0[1] + dy, c0[2] + dz] };
                        st.nodes += 1;
                        let Some(v) = self.cells.get(&key) else { continue };
                        for e in v {
                            st.tested += 1;
                            if q.accepts(e) {
                                out.push(e.id);
                            }
                        }
                    }
                }
            }
        }
    }

    fn mem_bytes(&self) -> usize {
        self.cells.vec_bytes()
            + self.cells.len() * (size_of::<CellKey>() + size_of::<Vec<Entry>>() + M::SLOT_OVERHEAD)
            + self.loc.capacity() * size_of::<(CellKey, u32)>()
            + self.levels.len() * 32
    }

    fn max_depth(&self) -> i32 {
        self.levels.len() as i32
    }
}

// ---- chunk-rooted octree (tight or loose) --------------------------------------------------------

struct Node {
    key: CellKey,
    children: [u32; 8],
    entries: Vec<Entry>,
    /// Largest radius stored in this subtree (monotone; never lowered on removal).
    max_r: f64,
}

pub struct ChunkOctree {
    loose: bool,
    roots: BTreeMap<[i64; 3], u32>,
    nodes: Vec<Node>,
    /// Objects too large for a chunk-rooted tree.
    big: HashGrid<BTreeCells>,
    /// id -> (node, index) or (NONE, _) when the object lives in `big`.
    loc: Vec<Loc>,
    max_r_all: f64,
    deepest: i32,
}

/// Deepest level a tight octree may split to (1 um cells); loose trees stop at the object's size.
const MIN_LEVEL: i32 = -30;

impl ChunkOctree {
    /// `big_file_ratio` should equal the query ratio, so the shared big-object grid also reads
    /// only 3x3x3 cells per level.
    pub fn new(loose: bool, big_file_ratio: f64) -> Self {
        ChunkOctree {
            loose,
            roots: BTreeMap::new(),
            nodes: Vec::new(),
            big: HashGrid::new(big_file_ratio),
            loc: Vec::new(),
            max_r_all: 0.0,
            deepest: 0,
        }
    }

    fn new_node(&mut self, key: CellKey) -> u32 {
        self.nodes.push(Node { key, children: [NONE; 8], entries: Vec::new(), max_r: 0.0 });
        (self.nodes.len() - 1) as u32
    }

    fn child_index(parent: &CellKey, child: &CellKey) -> usize {
        let b = |a: usize| ((child.c[a] - (parent.c[a] << 1)) & 1) as usize;
        b(0) | (b(1) << 1) | (b(2) << 2)
    }

    /// Target level for an object: loose = deepest node >= 2r; tight = deepest node that fully
    /// contains the box [p - r, p + r] (and is >= 2r).
    fn target(&self, e: &Entry) -> i32 {
        let size_floor = level_for_extent(2.0 * e.r).max(MIN_LEVEL);
        if self.loose || size_floor > 0 {
            return size_floor;
        }
        let lo = e.pos.offset([-e.r, -e.r, -e.r]);
        let hi = e.pos.offset([e.r, e.r, e.r]);
        let mut level = 0;
        // Descend while the box stays inside one cell of the next level.
        while level - 1 >= size_floor && lo.cell(level - 1) == hi.cell(level - 1) {
            level -= 1;
        }
        // A box straddling its chunk's faces stays at the chunk root.
        level
    }

    fn place(&mut self, e: Entry) {
        let level = self.target(&e);
        if level > 0 {
            self.big.insert(e);
            self.set_loc(e.id, Loc { a: NONE, idx: 0 });
            return;
        }
        let chunk = e.pos.chunk;
        let mut n = match self.roots.get(&chunk) {
            Some(&n) => n,
            None => {
                let k = e.pos.cell(0);
                let n = self.new_node(k);
                self.roots.insert(chunk, n);
                n
            }
        };
        self.nodes[n as usize].max_r = self.nodes[n as usize].max_r.max(e.r);
        let mut l = 0;
        while l > level {
            let ck = e.pos.cell(l - 1);
            let ci = Self::child_index(&self.nodes[n as usize].key, &ck);
            let mut c = self.nodes[n as usize].children[ci];
            if c == NONE {
                c = self.new_node(ck);
                self.nodes[n as usize].children[ci] = c;
            }
            n = c;
            self.nodes[n as usize].max_r = self.nodes[n as usize].max_r.max(e.r);
            l -= 1;
        }
        self.deepest = self.deepest.min(level);
        let v = &mut self.nodes[n as usize].entries;
        v.push(e);
        let idx = (v.len() - 1) as u32;
        self.set_loc(e.id, Loc { a: n, idx });
    }

    fn set_loc(&mut self, id: u32, l: Loc) {
        let id = id as usize;
        if self.loc.len() <= id {
            self.loc.resize(id + 1, Loc { a: NONE, idx: NONE });
        }
        self.loc[id] = l;
    }

    fn take(&mut self, id: u32) -> Entry {
        let l = self.loc[id as usize];
        assert!(l.idx != NONE, "unknown id {id}");
        if l.a == NONE {
            let e = self.big.take_entry(id);
            self.loc[id as usize].idx = NONE;
            return e;
        }
        let v = &mut self.nodes[l.a as usize].entries;
        let e = v.swap_remove(l.idx as usize);
        if (l.idx as usize) < v.len() {
            let moved = v[l.idx as usize].id;
            self.loc[moved as usize].idx = l.idx;
        }
        self.loc[id as usize].idx = NONE;
        e
    }

    fn visit(&self, n: u32, q: &Query, out: &mut Vec<u32>, st: &mut QueryStats) {
        let node = &self.nodes[n as usize];
        st.nodes += 1;
        if node.max_r < q.min_r {
            return;
        }
        // Loose: objects may poke out by up to half a cell; tight: by nothing, except at the
        // chunk root, which also holds boxes straddling the chunk faces (up to max_r).
        let loose = if self.loose {
            0.5
        } else if node.key.level == 0 {
            node.max_r / node.key.size()
        } else {
            0.0
        };
        let reach = q.ratio * node.max_r;
        if node.key.dist2_to(&q.center, loose) > reach * reach {
            return;
        }
        for e in &node.entries {
            st.tested += 1;
            if q.accepts(e) {
                out.push(e.id);
            }
        }
        for &c in &node.children {
            if c != NONE {
                self.visit(c, q, out, st);
            }
        }
    }
}

impl<M: CellMap> HashGrid<M> {
    fn take_entry(&mut self, id: u32) -> Entry {
        let level = self.loc[id as usize].0.level;
        let e = self.take(id);
        let n = self.levels.get_mut(&level).expect("level");
        *n -= 1;
        if *n == 0 {
            self.levels.remove(&level);
        }
        e
    }
}

impl Spatial for ChunkOctree {
    fn name(&self) -> String {
        if self.loose { "loose-octree/chunk-rooted".into() } else { "octree/chunk-rooted".into() }
    }

    fn insert(&mut self, e: Entry) {
        self.max_r_all = self.max_r_all.max(e.r.min(512.0));
        self.place(e);
    }

    fn update(&mut self, id: u32, pos: WorldPos) {
        let l = self.loc[id as usize];
        if l.a != NONE {
            // Fast path: still inside the same node, and (tight) still fully inside it.
            let node = &self.nodes[l.a as usize];
            let e = node.entries[l.idx as usize];
            let same = pos.cell(node.key.level) == node.key
                && pos.chunk == e.pos.chunk
                && (self.loose || {
                    let mut moved = e;
                    moved.pos = pos;
                    self.target(&moved) == node.key.level
                });
            if same {
                self.nodes[l.a as usize].entries[l.idx as usize].pos = pos;
                return;
            }
        }
        let mut e = self.take(id);
        e.pos = pos;
        self.place(e);
    }

    fn remove(&mut self, id: u32) {
        self.take(id);
    }

    fn query(&self, q: &Query, out: &mut Vec<u32>, st: &mut QueryStats) {
        self.big.query(q, out, st);
        // Chunk roots within the farthest relevance reach of anything a chunk can hold.
        let reach = q.ratio * self.max_r_all;
        let n = (reach / crate::coords::CHUNK).ceil() as i64 + 1;
        let c = q.center.chunk;
        if (2 * n + 1).pow(2) as usize > self.roots.len() {
            for &root in self.roots.values() {
                self.visit(root, q, out, st);
            }
            return;
        }
        for x in c[0] - n..=c[0] + n {
            for y in c[1] - n..=c[1] + n {
                for (_, &root) in self.roots.range([x, y, c[2] - n]..=[x, y, c[2] + n]) {
                    self.visit(root, q, out, st);
                }
            }
        }
    }

    fn mem_bytes(&self) -> usize {
        self.nodes.capacity() * size_of::<Node>()
            + self.nodes.iter().map(|n| n.entries.capacity() * size_of::<Entry>()).sum::<usize>()
            + self.roots.len() * (24 + 4 + 16)
            + self.loc.capacity() * size_of::<Loc>()
            + self.big.mem_bytes()
    }

    fn max_depth(&self) -> i32 {
        -self.deepest
    }
}
