//! `query` over one snapshot (api.md 5.2): references and paths, selectors, paging, field projection and inlined
//! children. Selector resolution here is also what `apply` uses to expand a `where` into explicit ids (api.md 5.3).

use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use pe_seams::world_api::{
    ApiError, At, AtName, Cursor, ErrorCode, ObjRef, ObjectId, ObjectView, Query, QueryResult, Selector, Version,
};

use crate::ports::{ObjInfo, Snapshot, SnapshotSource};

pub const LIMIT_DEFAULT: u32 = 50;
pub const LIMIT_MAX: u32 = 256;
/// The most objects one result may inline as children, all levels together.
pub const INLINE_MAX: usize = 2_000;

/// What a reference names in one snapshot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Named {
    One(ObjectId),
    /// A path that matches several objects (names need not be unique).
    Many(Vec<ObjectId>),
    None,
    /// `"me"`: the player the caller acts for, not an object of the world.
    Me,
}

/// How many objects one call (a query) or one change (an apply's resolution) may visit: every node a path walk,
/// a subtree walk or a selector's candidate scan touches is charged (design-reviewer M3). Walks stop early once it is
/// spent, and the caller refuses with `too_large`; a partial result is never used.
pub struct Budget {
    used: Cell<usize>,
    limit: usize,
}

/// The objects one query or one change may visit (provisional [I]; about 0.15 s in release at 150 ns an object).
pub const EXAMINE_MAX: usize = 1_000_000;

impl Budget {
    pub fn new(limit: usize) -> Budget {
        Budget { used: Cell::new(0), limit }
    }
    pub fn unlimited() -> Budget {
        Budget::new(usize::MAX)
    }
    /// Charges `n` visits; false once the budget is spent.
    pub fn charge(&self, n: usize) -> bool {
        self.used.set(self.used.get().saturating_add(n));
        !self.over()
    }
    pub fn over(&self) -> bool {
        self.used.get() > self.limit
    }
    pub fn used(&self) -> usize {
        self.used.get()
    }
    pub fn limit(&self) -> usize {
        self.limit
    }
}

impl Default for Budget {
    fn default() -> Self {
        Budget::new(EXAMINE_MAX)
    }
}

fn kids(snap: &dyn Snapshot, p: Option<ObjectId>, b: &Budget) -> Vec<ObjectId> {
    let k = match p {
        None => snap.roots(),
        Some(id) => snap.children(id),
    };
    b.charge(k.len());
    k
}

/// Appends `id`'s descendants in pre-order. Iterative with an explicit stack, so a deep hierarchy can't overflow the
/// worker's stack (a stack overflow aborts the process; it is not a panic). `expanded` skips subtrees already walked,
/// so overlapping walks stay linear. Charges each node visited, and stops once the budget is spent.
fn descendants(
    snap: &dyn Snapshot,
    id: ObjectId,
    expanded: &mut BTreeSet<ObjectId>,
    out: &mut Vec<ObjectId>,
    b: &Budget,
) {
    let mut stack: Vec<ObjectId> = kids(snap, Some(id), b).into_iter().rev().collect();
    while let Some(c) = stack.pop() {
        if b.over() {
            return;
        }
        if !expanded.insert(c) {
            continue;
        }
        out.push(c);
        stack.extend(kids(snap, Some(c), b).into_iter().rev());
    }
}

/// Every object a path names: `/A/B` by names from the roots; `*` is any one level, `**` any depth (zero or more).
pub fn walk_path(snap: &dyn Snapshot, path: &str) -> Vec<ObjectId> {
    walk_path_in(snap, path, &Budget::unlimited())
}

/// `walk_path`, charging every node it visits to `b`.
pub fn walk_path_in(snap: &dyn Snapshot, path: &str, b: &Budget) -> Vec<ObjectId> {
    let segs: Vec<&str> = path.trim_start_matches('/').split('/').filter(|s| !s.is_empty()).collect();
    // The current set holds "parents to look under"; None stands for the virtual root above the roots.
    let mut level: Vec<Option<ObjectId>> = vec![None];
    for seg in segs {
        let mut next: Vec<Option<ObjectId>> = vec![];
        let mut seen = BTreeSet::new();
        let mut expanded = BTreeSet::new();
        let mut push = |id: ObjectId, next: &mut Vec<Option<ObjectId>>| {
            if seen.insert(id) {
                next.push(Some(id));
            }
        };
        for p in &level {
            if b.over() {
                return vec![];
            }
            match seg {
                "*" => kids(snap, *p, b).into_iter().for_each(|c| push(c, &mut next)),
                "**" => {
                    // Zero levels: the parents themselves stay candidates for the next segment. Each subtree is
                    // walked once per segment (`expanded`), so `/**/**` stays linear.
                    let mut all = vec![];
                    match p {
                        Some(id) => {
                            all.push(*id);
                            if expanded.insert(*id) {
                                descendants(snap, *id, &mut expanded, &mut all, b);
                            }
                        }
                        None => {
                            next.push(None); // zero levels: the virtual root stays, so `/**/X` finds a root X
                            for c in kids(snap, None, b) {
                                if expanded.insert(c) {
                                    all.push(c);
                                    descendants(snap, c, &mut expanded, &mut all, b);
                                }
                            }
                        }
                    }
                    all.into_iter().for_each(|c| push(c, &mut next));
                }
                name => kids(snap, *p, b)
                    .into_iter()
                    .filter(|id| snap.info(*id).is_some_and(|i| i.name == name))
                    .for_each(|c| push(c, &mut next)),
            }
        }
        level = next;
    }
    if b.over() {
        return vec![];
    }
    level.into_iter().flatten().collect()
}

/// Resolves an id, a path or `"me"`. `$ref`s belong to a transaction and resolve there.
pub fn resolve(snap: &dyn Snapshot, r: &ObjRef) -> Named {
    resolve_in(snap, r, &Budget::unlimited())
}

/// `resolve`, charging a path's walk to `b`. When `b` is spent the answer is meaningless; the caller refuses.
pub fn resolve_in(snap: &dyn Snapshot, r: &ObjRef, b: &Budget) -> Named {
    let s = r.0.as_str();
    if s == "me" {
        return Named::Me;
    }
    if let Some(id) = ObjectId::parse(s) {
        b.charge(1);
        return if snap.info(id).is_some() { Named::One(id) } else { Named::None };
    }
    if s.starts_with('/') {
        let hits = walk_path_in(snap, s, b);
        return match hits.len() {
            0 => Named::None,
            1 => Named::One(hits[0]),
            _ => Named::Many(hits),
        };
    }
    Named::None
}

fn refs_to_ids(snap: &dyn Snapshot, r: &ObjRef, b: &Budget) -> Vec<ObjectId> {
    match resolve_in(snap, r, b) {
        Named::One(id) => vec![id],
        Named::Many(v) => v,
        Named::None | Named::Me => vec![],
    }
}

/// A selector with its references, paths and subtrees resolved once against one snapshot, so testing a candidate
/// is a set lookup or a field compare, never a path walk per candidate. Sets keep their walk order too, so the
/// candidates need no second walk.
enum Compiled<'a> {
    /// Id, Ids and Path: exactly these objects, in the order named.
    Set(BTreeSet<ObjectId>, Vec<ObjectId>),
    Name(&'a str),
    /// Lower-cased once.
    NameLike(String),
    Has(&'a str),
    /// `of` resolved to an id when it was a path (the port still resolves `"me"` and ids).
    Near(ObjRef, f64),
    /// The parents, in the order named.
    ChildOf(BTreeSet<ObjectId>, Vec<ObjectId>),
    /// Every object strictly under the named ones, in walk order.
    Under(BTreeSet<ObjectId>, Vec<ObjectId>),
    All(Vec<Compiled<'a>>),
}

fn subtree_below(snap: &dyn Snapshot, roots: &[ObjectId], b: &Budget) -> Vec<ObjectId> {
    let mut expanded = BTreeSet::new();
    let mut out = vec![];
    for r in roots {
        descendants(snap, *r, &mut expanded, &mut out, b);
    }
    out
}

fn ordered(v: Vec<ObjectId>) -> (BTreeSet<ObjectId>, Vec<ObjectId>) {
    let mut set = BTreeSet::new();
    let v: Vec<ObjectId> = v.into_iter().filter(|id| set.insert(*id)).collect();
    (set, v)
}

fn compile<'a>(snap: &dyn Snapshot, s: &'a Selector, b: &Budget) -> Compiled<'a> {
    match s {
        Selector::Id(r) => {
            let (s, v) = ordered(refs_to_ids(snap, r, b));
            Compiled::Set(s, v)
        }
        Selector::Ids(rs) => {
            let mut all = vec![];
            for r in rs {
                if b.over() {
                    break;
                }
                all.extend(refs_to_ids(snap, r, b));
            }
            let (s, v) = ordered(all);
            Compiled::Set(s, v)
        }
        Selector::Path(p) => {
            let (s, v) = ordered(walk_path_in(snap, p, b));
            Compiled::Set(s, v)
        }
        Selector::Name(n) => Compiled::Name(n),
        Selector::NameLike(n) => Compiled::NameLike(n.to_lowercase()),
        Selector::Has(t) => Compiled::Has(t),
        Selector::Near(n) => {
            let of = match resolve_in(snap, &n.of, b) {
                Named::One(id) if n.of.0.starts_with('/') => ObjRef(id.to_string()),
                _ => n.of.clone(),
            };
            Compiled::Near(of, n.within_m)
        }
        Selector::ChildOf(r) => {
            let (s, v) = ordered(refs_to_ids(snap, r, b));
            Compiled::ChildOf(s, v)
        }
        Selector::Under(r) => {
            let (s, v) = ordered(subtree_below(snap, &refs_to_ids(snap, r, b), b));
            Compiled::Under(s, v)
        }
        Selector::All(v) => Compiled::All(v.iter().map(|s| compile(snap, s, b)).collect()),
    }
}

impl Compiled<'_> {
    fn matches(&self, snap: &dyn Snapshot, id: ObjectId, info: &ObjInfo) -> bool {
        match self {
            Compiled::Set(ids, _) | Compiled::Under(ids, _) => ids.contains(&id),
            Compiled::Name(n) => info.name == *n,
            Compiled::NameLike(n) => info.name.to_lowercase().contains(n.as_str()),
            Compiled::Has(t) => info.types.iter().any(|x| x == t || x.split('#').next() == Some(*t)),
            Compiled::Near(of, within) => snap.distance(id, of).is_some_and(|d| d <= *within),
            Compiled::ChildOf(parents, _) => info.parent.is_some_and(|p| parents.contains(&p)),
            Compiled::All(v) => v.iter().all(|c| c.matches(snap, id, info)),
        }
    }

    /// A narrow candidate set, in a stable order, where this selector gives one.
    /// Charged by the caller, once, by its length.
    fn candidates(&self, snap: &dyn Snapshot) -> Option<Vec<ObjectId>> {
        match self {
            Compiled::Set(_, v) | Compiled::Under(_, v) => Some(v.clone()),
            Compiled::ChildOf(_, ps) => Some(ps.iter().flat_map(|p| snap.children(*p)).collect()),
            // The narrowest member of an `all` that has one.
            Compiled::All(cs) => cs.iter().filter_map(|c| c.candidates(snap)).min_by_key(|v| v.len()),
            _ => None,
        }
    }
}

/// Whether `id` satisfies the selector. Resolves the selector's references for this one call, so an `under` costs
/// its whole subtree each time; to test many objects use `select`, and a caller testing objects one by one (a watch
/// filter per changed object) should keep one compiled selector instead.
pub fn matches(snap: &dyn Snapshot, id: ObjectId, s: &Selector) -> bool {
    let b = Budget::unlimited();
    snap.info(id).is_some_and(|info| compile(snap, s, &b).matches(snap, id, &info))
}

/// The objects a selector selects, in a stable order: a narrow candidate set where the selector gives one, else
/// every object, then the full predicate. `None` selects the roots (api.md 5.2). The selector's references, paths
/// and subtrees are resolved once per call. Unbounded; callers serving a request use `select_in`.
pub fn select(snap: &dyn Snapshot, s: Option<&Selector>) -> Vec<ObjectId> {
    match s {
        None => snap.roots(),
        Some(s) => select_in(snap, s, &Budget::unlimited()),
    }
}

/// `select`, charging every walk and every candidate examined to `b`. When `b` is spent the result is meaningless
/// (empty or partial); the caller refuses with `too_large`.
pub fn select_in(snap: &dyn Snapshot, s: &Selector, b: &Budget) -> Vec<ObjectId> {
    let c = compile(snap, s, b);
    if b.over() {
        return vec![];
    }
    let candidates = c.candidates(snap).unwrap_or_else(|| snap.all());
    if !b.charge(candidates.len()) {
        return vec![];
    }
    candidates.into_iter().filter(|id| snap.info(*id).is_some_and(|info| c.matches(snap, *id, &info))).collect()
}

fn page_cursor(frame: u64, offset: usize) -> Cursor {
    Cursor(format!("q:{frame}.{offset}"))
}

fn parse_page(c: &Cursor) -> Option<(u64, usize)> {
    let (f, o) = c.0.strip_prefix("q:")?.split_once('.')?;
    Some((f.parse().ok()?, o.parse().ok()?))
}

/// Picks the snapshot a query reads: the cursor's frame when paging (so pages are consistent), else `at`.
pub fn pick(src: &dyn SnapshotSource, at: At, cursor: Option<&Cursor>) -> Result<(Arc<dyn Snapshot>, usize), ApiError> {
    let gone = |f: u64| {
        let mut e = ApiError::new(ErrorCode::SnapshotGone, format!("frame {f} is no longer retained; query again"));
        e.field = Some(if cursor.is_some() { "cursor".into() } else { "at".into() });
        e
    };
    if let Some(c) = cursor {
        let (frame, offset) = parse_page(c).ok_or_else(|| ApiError::arg("cursor", "not a cursor from world_query"))?;
        return Ok((src.at(frame).ok_or_else(|| gone(frame))?, offset));
    }
    let snap = match at {
        At::Named(AtName::Latest) => src.latest(),
        At::Named(AtName::Final) => src.final_(),
        At::Frame(f) => src.at(f).ok_or_else(|| gone(f))?,
    };
    Ok((snap, 0))
}

/// Which components and fields to return, from `fields`.
enum Projection {
    TypesOnly,
    All,
    Some(BTreeMap<String, Option<BTreeSet<String>>>),
}

fn projection(fields: Option<&Vec<String>>) -> Projection {
    let Some(f) = fields else { return Projection::TypesOnly };
    if f.iter().any(|x| x == "*") {
        return Projection::All;
    }
    let mut m: BTreeMap<String, Option<BTreeSet<String>>> = BTreeMap::new();
    for x in f {
        match x.split_once('.') {
            None => {
                m.insert(x.clone(), None);
            }
            Some((c, field)) => {
                let e = m.entry(c.to_string()).or_insert_with(|| Some(BTreeSet::new()));
                if let Some(set) = e {
                    set.insert(field.to_string());
                }
            }
        }
    }
    Projection::Some(m)
}

struct Builder<'a> {
    snap: &'a dyn Snapshot,
    proj: Projection,
    frame: ObjRef,
    inlined: usize,
    seen: Vec<(ObjectId, Version)>,
}

impl Builder<'_> {
    fn view(&mut self, id: ObjectId, depth: u32) -> Result<Option<ObjectView>, ApiError> {
        let Some(info) = self.snap.info(id) else { return Ok(None) };
        self.seen.push((id, info.version.clone()));
        let mut components = BTreeMap::new();
        let mut types = vec![];
        match &self.proj {
            Projection::TypesOnly => types = info.types.clone(),
            Projection::All => {
                for t in &info.types {
                    if let Some(f) = self.snap.fields(id, t, &self.frame) {
                        components.insert(t.clone(), f);
                    }
                }
            }
            Projection::Some(m) => {
                for (c, want) in m {
                    if !info.types.contains(c) {
                        continue;
                    }
                    if let Some(mut f) = self.snap.fields(id, c, &self.frame) {
                        if let Some(want) = want {
                            f.retain(|k, _| want.contains(k));
                        }
                        components.insert(c.clone(), f);
                    }
                }
            }
        }
        let mut children = vec![];
        if depth > 0 {
            for c in self.snap.children(id) {
                self.inlined += 1;
                if self.inlined > INLINE_MAX {
                    return Err(ApiError::arg(
                        "children",
                        format!("more than {INLINE_MAX} objects to inline; page with child_of"),
                    ));
                }
                if let Some(v) = self.view(c, depth - 1)? {
                    children.push(v);
                }
            }
        }
        Ok(Some(ObjectView {
            id,
            name: info.name,
            parent: info.parent,
            version: info.version,
            creator: info.creator,
            held_by: info.held_by,
            protected_by: info.protected_by,
            types,
            components,
            children,
        }))
    }
}

/// Runs a query over a snapshot already picked (`pick`), from `offset`. Returns the result and every
/// (object, version) it reported, for the connection's table of versions seen (api.md 4.2).
pub fn run(snap: &dyn Snapshot, q: &Query, offset: usize) -> Result<(QueryResult, Vec<(ObjectId, Version)>), ApiError> {
    let limit = q.limit.unwrap_or(LIMIT_DEFAULT);
    if limit == 0 || limit > LIMIT_MAX {
        return Err(ApiError::arg("limit", format!("between 1 and {LIMIT_MAX}")));
    }
    let budget = Budget::default();
    let ids = match q.select.as_ref() {
        None => snap.roots(),
        Some(s) => select_in(snap, s, &budget),
    };
    if budget.over() {
        return Err(ApiError::arg(
            "select",
            format!("The selector examines more than {EXAMINE_MAX} objects; narrow it (an id, a path, under)."),
        ));
    }
    if q.count_only {
        return Ok((
            QueryResult {
                frame: snap.frame(),
                final_frame: snap.final_frame(),
                count: Some(ids.len() as u64),
                objects: vec![],
                more: None,
            },
            vec![],
        ));
    }
    let mut b = Builder {
        snap,
        proj: projection(q.fields.as_ref()),
        frame: q.frame.clone().unwrap_or_else(|| ObjRef("parent".into())),
        inlined: 0,
        seen: vec![],
    };
    let end = (offset + limit as usize).min(ids.len());
    let mut objects = vec![];
    for id in ids.iter().skip(offset).take(limit as usize) {
        if let Some(v) = b.view(*id, q.children)? {
            objects.push(v);
        }
    }
    let more = (end < ids.len()).then(|| page_cursor(snap.frame(), end));
    Ok((QueryResult { frame: snap.frame(), final_frame: snap.final_frame(), count: None, objects, more }, b.seen))
}

#[cfg(test)]
pub(crate) mod mem {
    //! An in-memory snapshot for tests.
    use super::*;
    use pe_seams::world_api::{Json, MemberId};
    use serde_json::json;

    /// An object, its components' fields, and its world position.
    pub type MemObj = (ObjInfo, BTreeMap<String, BTreeMap<String, Json>>, [f64; 3]);

    #[derive(Clone, Default)]
    pub struct Mem {
        pub frame: u64,
        pub objs: Vec<MemObj>,
    }

    pub fn oid(n: u8) -> ObjectId {
        let mut b = [0u8; 16];
        b[15] = n;
        ObjectId(b)
    }

    impl Mem {
        pub fn add(&mut self, n: u8, name: &str, parent: Option<u8>, types: &[&str], at: [f64; 3]) {
            let info = ObjInfo {
                id: oid(n),
                name: name.into(),
                parent: parent.map(oid),
                version: Version(format!("v:{n:016x}")),
                creator: MemberId([1; 32]),
                held_by: None,
                protected_by: None,
                types: types.iter().map(|s| s.to_string()).collect(),
            };
            let comps = types
                .iter()
                .map(|t| (t.to_string(), BTreeMap::from([("a".to_string(), json!(n)), ("b".to_string(), json!(t))])))
                .collect();
            self.objs.push((info, comps, at));
        }
        /// The ball pit's shape: a table with two lamps, and balls at the root.
        pub fn pit(balls: u8) -> Mem {
            let mut m = Mem { frame: 100, objs: vec![] };
            m.add(1, "Table", None, &["Transform", "Shape"], [0.0, 0.0, 0.0]);
            m.add(2, "Lamp", Some(1), &["Transform", "Light"], [-0.5, 1.0, 0.0]);
            m.add(3, "Lamp", Some(1), &["Transform", "Light"], [0.5, 1.0, 0.0]);
            m.add(4, "Shade", Some(2), &["Transform"], [-0.5, 1.2, 0.0]);
            for i in 0..balls {
                m.add(10 + i, &format!("Ball {i}"), None, &["Transform", "Ball"], [i as f64, 0.0, 3.0]);
            }
            m
        }
        fn get(&self, id: ObjectId) -> Option<&MemObj> {
            self.objs.iter().find(|o| o.0.id == id)
        }
    }

    impl Snapshot for Mem {
        fn frame(&self) -> u64 {
            self.frame
        }
        fn final_frame(&self) -> u64 {
            self.frame.saturating_sub(8)
        }
        fn roots(&self) -> Vec<ObjectId> {
            self.objs.iter().filter(|o| o.0.parent.is_none()).map(|o| o.0.id).collect()
        }
        fn all(&self) -> Vec<ObjectId> {
            self.objs.iter().map(|o| o.0.id).collect()
        }
        fn info(&self, id: ObjectId) -> Option<ObjInfo> {
            self.get(id).map(|o| o.0.clone())
        }
        fn children(&self, id: ObjectId) -> Vec<ObjectId> {
            self.objs.iter().filter(|o| o.0.parent == Some(id)).map(|o| o.0.id).collect()
        }
        fn fields(&self, id: ObjectId, c: &str, _frame: &ObjRef) -> Option<BTreeMap<String, Json>> {
            self.get(id)?.1.get(c).cloned()
        }
        fn distance(&self, a: ObjectId, b: &ObjRef) -> Option<f64> {
            let pa = self.get(a)?.2;
            let pb = if b.0 == "me" {
                [0.0, 1.6, 0.0]
            } else {
                match resolve(self, b) {
                    Named::One(id) => self.get(id)?.2,
                    _ => return None,
                }
            };
            Some(((pa[0] - pb[0]).powi(2) + (pa[1] - pb[1]).powi(2) + (pa[2] - pb[2]).powi(2)).sqrt())
        }
    }

    /// A large snapshot with O(1) lookups, for cost and depth tests (Mem is linear per lookup).
    pub struct Big {
        parent: Vec<Option<u32>>,
        kids: Vec<Vec<u32>>,
    }

    pub fn bid(i: u32) -> ObjectId {
        let mut b = [0u8; 16];
        b[12..].copy_from_slice(&(i + 1).to_be_bytes());
        ObjectId(b)
    }
    pub fn bix(id: ObjectId) -> Option<usize> {
        let i = u32::from_be_bytes(id.0[12..].try_into().unwrap()) as usize;
        (id.0[..12] == [0; 12] && i >= 1).then(|| i - 1)
    }

    impl Big {
        pub fn new(parent: Vec<Option<u32>>) -> Big {
            let mut kids = vec![vec![]; parent.len()];
            for (i, p) in parent.iter().enumerate() {
                if let Some(p) = p {
                    kids[*p as usize].push(i as u32);
                }
            }
            Big { parent, kids }
        }
        /// `roots` roots with `per` children each.
        pub fn wide(roots: u32, per: u32) -> Big {
            let mut parent = vec![];
            for r in 0..roots {
                let at = parent.len() as u32;
                assert_eq!(at, r * (per + 1));
                parent.push(None);
                parent.extend((0..per).map(|_| Some(at)));
            }
            Big::new(parent)
        }
        /// One chain `n` deep.
        pub fn chain(n: u32) -> Big {
            Big::new((0..n).map(|i| i.checked_sub(1)).collect())
        }
    }

    impl Snapshot for Big {
        fn frame(&self) -> u64 {
            1
        }
        fn final_frame(&self) -> u64 {
            1
        }
        fn roots(&self) -> Vec<ObjectId> {
            (0..self.parent.len() as u32).filter(|i| self.parent[*i as usize].is_none()).map(bid).collect()
        }
        fn all(&self) -> Vec<ObjectId> {
            (0..self.parent.len() as u32).map(bid).collect()
        }
        fn info(&self, id: ObjectId) -> Option<ObjInfo> {
            let i = bix(id).filter(|i| *i < self.parent.len())?;
            Some(ObjInfo {
                id,
                name: if self.parent[i].is_none() { "Root".into() } else { "Ball".into() },
                parent: self.parent[i].map(bid),
                version: Version(format!("v:{i}")),
                creator: MemberId([1; 32]),
                held_by: None,
                protected_by: None,
                types: vec!["Transform".into()],
            })
        }
        fn children(&self, id: ObjectId) -> Vec<ObjectId> {
            bix(id).and_then(|i| self.kids.get(i)).map(|k| k.iter().copied().map(bid).collect()).unwrap_or_default()
        }
        fn fields(&self, _: ObjectId, _: &str, _: &ObjRef) -> Option<BTreeMap<String, Json>> {
            None
        }
        fn distance(&self, _: ObjectId, _: &ObjRef) -> Option<f64> {
            None
        }
    }

    pub struct Src(pub Vec<Arc<Mem>>);

    impl SnapshotSource for Src {
        fn latest(&self) -> Arc<dyn Snapshot> {
            self.0.last().unwrap().clone()
        }
        fn final_(&self) -> Arc<dyn Snapshot> {
            self.0.first().unwrap().clone()
        }
        fn at(&self, frame: u64) -> Option<Arc<dyn Snapshot>> {
            self.0.iter().find(|m| m.frame == frame).map(|m| m.clone() as Arc<dyn Snapshot>)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::mem::*;
    use super::*;
    use pe_seams::world_api::Near;

    fn r(s: &str) -> ObjRef {
        ObjRef(s.into())
    }

    fn names(m: &Mem, ids: &[ObjectId]) -> Vec<String> {
        ids.iter().map(|id| m.info(*id).unwrap().name).collect()
    }

    #[test]
    fn paths_resolve_by_name_with_globs() {
        let m = Mem::pit(2);
        assert_eq!(resolve(&m, &r("/Table")), Named::One(oid(1)));
        assert_eq!(resolve(&m, &r("/Table/Lamp")), Named::Many(vec![oid(2), oid(3)]));
        assert_eq!(resolve(&m, &r("/Nope")), Named::None);
        assert_eq!(resolve(&m, &r("me")), Named::Me);
        assert_eq!(names(&m, &walk_path(&m, "/Table/*")), ["Lamp", "Lamp"]);
        assert_eq!(names(&m, &walk_path(&m, "/**/Shade")), ["Shade"]);
        assert_eq!(walk_path(&m, "/Table/**").len(), 4); // the table itself (zero levels), two lamps and the shade
        assert_eq!(resolve(&m, &r(&oid(4).to_string())), Named::One(oid(4)));
        assert_eq!(resolve(&m, &r(&oid(99).to_string())), Named::None);
    }

    #[test]
    fn selectors() {
        let m = Mem::pit(3);
        let sel = |s: Selector| names(&m, &select(&m, Some(&s)));
        assert_eq!(sel(Selector::Has("Ball".into())).len(), 3);
        assert_eq!(sel(Selector::NameLike("LAMP".into())), ["Lamp", "Lamp"]);
        assert_eq!(sel(Selector::ChildOf(r("/Table"))), ["Lamp", "Lamp"]);
        assert_eq!(sel(Selector::Under(r("/Table"))), ["Lamp", "Shade", "Lamp"]);
        assert_eq!(sel(Selector::All(vec![Selector::Has("Ball".into()), Selector::Name("Ball 1".into())])), ["Ball 1"]);
        assert_eq!(sel(Selector::Near(Near { of: r("/Table"), within_m: 1.2 })), ["Table", "Lamp", "Lamp"]);
        assert_eq!(sel(Selector::Near(Near { of: r("me"), within_m: 1.0 })), ["Lamp", "Lamp", "Shade"]);
        assert_eq!(names(&m, &select(&m, None)), ["Table", "Ball 0", "Ball 1", "Ball 2"]);
    }

    #[test]
    fn paging_follows_one_frame_and_projection_trims_fields() {
        let m = Arc::new(Mem::pit(5));
        let src = Src(vec![m.clone()]);
        let q = Query {
            select: Some(Selector::Has("Ball".into())),
            fields: Some(vec!["Ball.a".into()]),
            limit: Some(2),
            ..Default::default()
        };
        let (snap, off) = pick(&src, At::default(), None).unwrap();
        let (r1, seen) = run(&*snap, &q, off).unwrap();
        assert_eq!(r1.objects.len(), 2);
        assert_eq!(seen.len(), 2);
        assert_eq!(r1.objects[0].components["Ball"].keys().collect::<Vec<_>>(), ["a"]);
        assert!(r1.objects[0].types.is_empty());
        let c = r1.more.clone().unwrap();
        let (snap, off) = pick(&src, At::default(), Some(&c)).unwrap();
        let (r2, _) = run(&*snap, &q, off).unwrap();
        let (snap, off) = pick(&src, At::default(), r2.more.as_ref()).unwrap();
        let (r3, _) = run(&*snap, &q, off).unwrap();
        assert_eq!(r3.objects.len(), 1);
        assert!(r3.more.is_none());
        // A page's frame that has left retention is snapshot_gone.
        let gone = pick(&Src(vec![Arc::new(Mem::pit(1))]), At::default(), Some(&Cursor("q:7.2".into())));
        assert_eq!(gone.err().unwrap().code, ErrorCode::SnapshotGone);
        assert_eq!(
            pick(&src, At::default(), Some(&Cursor("zz".into()))).err().unwrap().code,
            ErrorCode::InvalidArgument
        );
    }

    #[test]
    fn default_projection_lists_types_and_children_inline() {
        let m = Mem::pit(0);
        let q = Query { select: Some(Selector::Id(r("/Table"))), children: 2, ..Default::default() };
        let (res, seen) = run(&m, &q, 0).unwrap();
        let t = &res.objects[0];
        assert_eq!(t.types, ["Transform", "Shape"]);
        assert!(t.components.is_empty());
        assert_eq!(t.children.len(), 2);
        assert_eq!(t.children[0].children[0].name, "Shade");
        assert_eq!(seen.len(), 4);
        let all =
            Query { select: Some(Selector::Id(r("/Table"))), fields: Some(vec!["*".into()]), ..Default::default() };
        assert_eq!(run(&m, &all, 0).unwrap().0.objects[0].components.len(), 2);
    }

    // Review Q1: a selector's paths and refs are resolved once per query, not per candidate. Before the fix, `/**`
    // over 100,000 objects walked the whole tree once per candidate (about 10^10 steps).
    #[test]
    fn selectors_over_a_large_world_resolve_once() {
        let b = Big::wide(100, 999);
        let t = std::time::Instant::now();
        assert_eq!(select(&b, Some(&Selector::Path("/**".into()))).len(), 100_000);
        assert_eq!(select(&b, Some(&Selector::Path("/**/**".into()))).len(), 100_000);
        assert_eq!(select(&b, Some(&Selector::Path("/*/Ball".into()))).len(), 99_900);
        let inside = Selector::All(vec![Selector::Has("Transform".into()), Selector::Path("/**/Ball".into())]);
        assert_eq!(select(&b, Some(&inside)).len(), 99_900);
        let many_parents = Selector::ChildOf(ObjRef("/Root".into())); // the path names all 100 roots
        assert_eq!(select(&b, Some(&many_parents)).len(), 99_900);
        let under = Selector::All(vec![Selector::Name("Ball".into()), Selector::Under(ObjRef("/Root".into()))]);
        assert_eq!(select(&b, Some(&under)).len(), 99_900);
        let ids = Selector::Ids((0..5_000).map(|i| ObjRef(bid(i * 20).to_string())).collect());
        assert_eq!(select(&b, Some(&ids)).len(), 5_000);
        let el = t.elapsed();
        assert!(el.as_secs() < 20, "selectors over 100,000 objects took {el:?}");
    }

    // design-reviewer M3 gap: path walks are charged too. 200 `/**` globs in one `ids` list over 100,000 objects
    // took 15.7 s in release before; now the query is refused once 1,000,000 visits are spent.
    #[test]
    fn path_walks_are_charged_to_the_budget() {
        let b = Big::wide(1, 99_999);
        let t = std::time::Instant::now();
        let ids = Selector::Ids((0..200).map(|_| ObjRef("/**/zzz".into())).collect());
        let q = Query { select: Some(ids), ..Default::default() };
        let e = run(&b, &q, 0).unwrap_err();
        assert_eq!(e.field.as_deref(), Some("select"));
        assert!(t.elapsed().as_secs() < 10, "took {:?}", t.elapsed());
        // Within the budget it runs: four globs of about 200,000 visits each.
        let four = Selector::Ids((0..4).map(|_| ObjRef("/**/zzz".into())).collect());
        assert_eq!(run(&b, &Query { select: Some(four), ..Default::default() }, 0).unwrap().0.objects.len(), 0);
        // `/**/X` includes zero levels below the virtual root, so it finds a root named X.
        assert_eq!(walk_path(&b, "/**/Root"), [bid(0)]);
    }

    // Review Q2: walks of the hierarchy are iterative. Before the fix, a chain this deep overflowed the test
    // thread's stack in `descendants`, which aborts the process.
    #[test]
    fn a_very_deep_hierarchy_does_not_overflow_the_stack() {
        let b = Big::chain(300_000);
        assert_eq!(select(&b, Some(&Selector::Path("/**".into()))).len(), 300_000);
        assert_eq!(select(&b, Some(&Selector::Under(ObjRef(bid(0).to_string())))).len(), 299_999);
        let deep = Selector::All(vec![Selector::Under(ObjRef(bid(1_000).to_string())), Selector::Name("Ball".into())]);
        assert_eq!(select(&b, Some(&deep)).len(), 300_000 - 1_001);
        assert!(matches(&b, bid(299_999), &Selector::Under(ObjRef(bid(0).to_string()))));
    }

    #[test]
    fn limits_and_count() {
        let m = Mem::pit(4);
        let bad = Query { limit: Some(257), ..Default::default() };
        assert_eq!(run(&m, &bad, 0).err().unwrap().field.as_deref(), Some("limit"));
        let c = Query { select: Some(Selector::Has("Ball".into())), count_only: true, ..Default::default() };
        let (res, seen) = run(&m, &c, 0).unwrap();
        assert_eq!((res.count, res.objects.len(), seen.len()), (Some(4), 0, 0));
    }
}
