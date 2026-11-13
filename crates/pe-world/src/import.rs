//! Import (world-model.md 6.5, repair rung 2 and a joiner): a world patched from records, each checked against the
//! agreed root before anything is applied, with the absent leaves removed. The byte formats are 6.6's.

use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;

use pe_core::{EntryRef, Frame, Hash, LineId, MemberKey, ObjectId, Pos};
use pe_seams::{CutImport, DomainId, ImportError, LeafKey, Nibbles, Node, Record, SimRate};
use pe_sim::{ComponentDesc, ComponentId, FieldKind, RowVersion, WorldTransform};

use crate::components::ids;
use crate::input::{HandPose, InputSettings, Pack};
use crate::script::AssetId;
use crate::tree::{schema_hash, table, table_hash, table_record};
use crate::world::{Claim, Tombstone, World};

/// A reader over a record's bytes; every read is checked, so a short or malformed record is an error, never a panic.
struct Rd<'a>(&'a [u8]);

impl<'a> Rd<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        if self.0.len() < n {
            return None;
        }
        let (a, b) = self.0.split_at(n);
        self.0 = b;
        Some(a)
    }
    fn arr<const N: usize>(&mut self) -> Option<[u8; N]> {
        self.take(N).map(|b| b.try_into().expect("N bytes"))
    }
    fn u8(&mut self) -> Option<u8> {
        self.take(1).map(|b| b[0])
    }
    fn u16(&mut self) -> Option<u16> {
        self.arr().map(u16::from_le_bytes)
    }
    fn u32(&mut self) -> Option<u32> {
        self.arr().map(u32::from_le_bytes)
    }
    fn u64(&mut self) -> Option<u64> {
        self.arr().map(u64::from_le_bytes)
    }
    fn u128(&mut self) -> Option<u128> {
        self.arr().map(u128::from_le_bytes)
    }
    fn f32(&mut self) -> Option<f32> {
        self.arr().map(f32::from_le_bytes)
    }
    fn lp(&mut self) -> Option<&'a [u8]> {
        let n = usize::try_from(self.u64()?).ok()?;
        self.take(n)
    }
    fn pos(&mut self) -> Option<Pos> {
        let (p, n) = Pos::decode(self.0).ok()?;
        self.take(n)?;
        Some(p)
    }
    fn id(&mut self) -> Option<ObjectId> {
        Some(ObjectId::from_u128(self.u128()?))
    }
    fn opt_id(&mut self) -> Option<Option<ObjectId>> {
        match self.u8()? {
            0 => Some(None),
            1 => Some(Some(self.id()?)),
            _ => None,
        }
    }
    fn end(&self) -> Option<()> {
        self.0.is_empty().then_some(())
    }
}

fn read_node(r: &mut Rd) -> Option<pe_sim::Node> {
    let [parent, first_child, last_child, prev_sibling, next_sibling] = [(); 5].map(|_| r.id());
    let n = pe_sim::Node {
        parent: parent?,
        first_child: first_child?,
        last_child: last_child?,
        prev_sibling: prev_sibling?,
        next_sibling: next_sibling?,
        creator: MemberKey(r.arr()?),
        version_entry: r.arr()?,
        structural_entry: r.arr()?,
        version_op: r.u32()?,
        structural_op: r.u32()?,
        born_on: r.u32()?,
        _pad: 0,
    };
    (r.u32()? == 0).then_some(n)
}

fn read_version(r: &mut Rd) -> Option<RowVersion> {
    Some(RowVersion { entry: r.arr()?, op: r.u32()?, tag: r.arr()?, _pad: 0 })
}

/// Canonical row bytes back to stored ones: each `Pos` field's canonical encoding back to its 48-byte row.
fn stored_row(d: &ComponentDesc, canonical: &[u8]) -> Option<Vec<u8>> {
    let mut fields: Vec<_> = d.fields().iter().filter(|f| f.kind == FieldKind::Pos).collect();
    fields.sort_by_key(|f| f.offset);
    let mut out = Vec::with_capacity(d.size());
    let mut r = Rd(canonical);
    for f in fields {
        let gap = (f.offset as usize).checked_sub(out.len())?;
        out.extend_from_slice(r.take(gap)?);
        for _ in 0..f.count {
            out.extend_from_slice(bytemuck::bytes_of(&r.pos()?.to_row()));
        }
    }
    let rest = d.size().checked_sub(out.len())?;
    out.extend_from_slice(r.take(rest)?);
    r.end()?;
    Some(out)
}

/// One decoded object record.
struct ObjectRecord {
    id: ObjectId,
    node: pe_sim::Node,
    rows: Vec<(ComponentId, Vec<u8>, RowVersion)>,
}

fn decode_object(world: &World, bytes: &[u8]) -> Option<ObjectRecord> {
    let mut r = Rd(bytes);
    let id = r.id()?;
    let node = read_node(&mut r)?;
    let n = r.u32()?;
    let mut rows = Vec::new();
    let mut last: Option<ComponentId> = None;
    for _ in 0..n {
        let c = ComponentId(r.u128()?);
        if c == ComponentId::NODE || last.is_some_and(|l| l >= c) {
            return None;
        }
        last = Some(c);
        let d = world.store.registry().get(c)?;
        if r.arr::<32>()? != schema_hash(d).0 || r.u16()? != 0 {
            return None;
        }
        let v = read_version(&mut r)?;
        rows.push((c, stored_row(d, r.lp()?)?, v));
    }
    r.end()?;
    Some(ObjectRecord { id, node, rows })
}

/// The leaf a record proves, checked up its proof to `target` (6.5): the root node, the domain, then the trie's inner
/// nodes, each naming the next. Returns the domain node.
fn check_object(target: Hash, domain: DomainId, id: ObjectId, bytes: &[u8], proof: &[Node]) -> Option<Node> {
    let [world, dom, inner @ ..] = proof else { return None };
    let Node::World { domains, .. } = world else { return None };
    if world.hash()? != target {
        return None;
    }
    let Node::Domain { domain: d, objects, .. } = dom else { return None };
    if *d != domain || domains.iter().find(|x| x.0 == domain)?.1 != dom.hash()? {
        return None;
    }
    let nibbles = Nibbles::of_id(id);
    let mut child = Node::Leaf { id, object_hash: Hash::of("pe.obj", bytes) }.hash()?;
    for node in inner.iter().rev() {
        let Node::Inner { prefix, children } = node else { return None };
        if !nibbles.starts_with(prefix) {
            return None;
        }
        let n = nibbles.as_slice()[prefix.len()];
        if children.iter().find(|c| c.0 == n)?.1 != child {
            return None;
        }
        child = node.hash()?;
    }
    (child == *objects).then(|| dom.clone())
}

/// The config record (S2 v1.3's `LeafKey::Config`): its proof is `[Node::World]`, and it checks as
/// `H("pe.config", bytes) == World.config`.
fn check_config(target: Hash, bytes: &[u8], proof: &[Node]) -> Option<()> {
    let [world] = proof else { return None };
    let Node::World { config, .. } = world else { return None };
    (world.hash()? == target && *config == Hash::of("pe.config", bytes)).then_some(())
}

/// The world settings a config record carries (6.6's config bytes, in order).
struct Config {
    seed: u64,
    line: LineId,
    m1_marker: Option<LineId>,
    agree_settings: Vec<u8>,
    rate: SimRate,
    depth_limit: u32,
    gravity: [f32; 3],
    origin: Pos,
    input_settings: InputSettings,
}

/// Decodes 6.6's config bytes exactly: every item present, nothing after the last, and each value one the world's own
/// setters accept, so the decoded config re-encodes to the same bytes.
fn decode_config(bytes: &[u8]) -> Option<Config> {
    let mut r = Rd(bytes);
    let seed = r.u64()?;
    let line = LineId(Hash(r.arr()?));
    let m1_marker = match r.u8()? {
        0 => None,
        1 => Some(LineId(Hash(r.arr()?))),
        _ => return None,
    };
    let agree_settings = r.lp()?.to_vec();
    let rate = SimRate::new(r.u32()?)?;
    let depth_limit = r.u32()?;
    if !(1..=crate::world::DEPTH_LIMIT_MAX).contains(&depth_limit) {
        return None;
    }
    let gravity = [r.f32()?, r.f32()?, r.f32()?];
    if !gravity.iter().all(|g| g.is_finite()) {
        return None;
    }
    let origin = r.pos()?;
    let input_settings = InputSettings {
        claim_ms: r.u32()?,
        cooldown_ms: r.u32()?,
        push_budget: r.u32()?,
        reach_mm: r.u32()?,
        slack_mm: r.u32()?,
    };
    if !input_settings.valid() {
        return None;
    }
    r.end()?;
    Some(Config { seed, line, m1_marker, agree_settings, rate, depth_limit, gravity, origin, input_settings })
}

fn check_table(target: Hash, index: u8, bytes: &[u8], proof: &[Node]) -> Option<()> {
    let [world, tables] = proof else { return None };
    let Node::World { tables: t, .. } = world else { return None };
    if world.hash()? != target || tables.hash()? != *t {
        return None;
    }
    let Node::Tables(hashes) = tables else { return None };
    (*hashes.get(index as usize)? == table_hash(index, bytes)).then_some(())
}

/// A table record's entries: `count u64 ‖ lp(key) ‖ lp(value) …`.
fn mismatch() -> ImportError {
    ImportError::RootMismatch { got: Hash::ZERO }
}

/// A registry of exactly `descs` (the built-ins among them, as every world registers them).
fn registry_of(descs: impl IntoIterator<Item = pe_sim::ComponentDesc>) -> Result<pe_sim::Registry, ImportError> {
    let mut r = pe_sim::Registry::new();
    for d in descs {
        r.register_engine(d).map_err(|_| mismatch())?;
    }
    Ok(r)
}

/// A registry table record's entries (6.6): each `asset ‖ schema hash ‖ description` under its id, checked strictly
/// (the schema hash and the re-encoded description must match).
fn decode_registry(bytes: &[u8]) -> Option<Vec<(AssetId, pe_sim::ComponentDesc)>> {
    let mut out = Vec::new();
    for (k, v) in table_entries(bytes)? {
        let id = ComponentId(u128::from_be_bytes(k.try_into().ok()?));
        let mut r = Rd(v);
        let asset: AssetId = r.arr()?;
        let schema: [u8; 32] = r.arr()?;
        let desc = r.0;
        let name = std::str::from_utf8(r.lp()?).ok()?.to_string();
        let size = r.u32()?;
        let n = r.u32()? as usize;
        if n > desc.len() {
            return None;
        }
        let mut fields = Vec::with_capacity(n);
        for _ in 0..n {
            let fname = std::str::from_utf8(r.lp()?).ok()?.to_string();
            let kind = crate::tree::kind_of_code(r.arr::<1>()?[0])?;
            let offset = r.u32()?;
            let count = r.u32()?;
            let unit = std::str::from_utf8(r.lp()?).ok()?.to_string();
            fields.push(pe_sim::FieldDesc::new(&fname, kind, offset as usize, count).with_unit(&unit));
        }
        r.end()?;
        let d = pe_sim::ComponentDesc::new(id, &name, fields).ok()?;
        if d.size() != size as usize || crate::tree::desc_bytes(&d) != desc || Hash::of("pe.schema", desc).0 != schema {
            return None;
        }
        out.push((asset, d));
    }
    Some(out)
}

fn table_entries(bytes: &[u8]) -> Option<Vec<(&[u8], &[u8])>> {
    let mut r = Rd(bytes);
    let n = r.u64()?;
    let mut out = Vec::new();
    for _ in 0..n {
        out.push((r.lp()?, r.lp()?));
    }
    r.end()?;
    out.windows(2).all(|w| w[0].0 < w[1].0).then_some(out)
}

fn key_id(k: &[u8]) -> Option<ObjectId> {
    Some(ObjectId::from_u128(u128::from_be_bytes(k.try_into().ok()?)))
}

fn key_member(k: &[u8]) -> Option<MemberKey> {
    Some(MemberKey(k.try_into().ok()?))
}

impl World {
    /// Rung 2's and a joiner's world-model part (6.5; pe-replica's `WorldModel::import`): this world patched from
    /// `import`'s records at the cut's frame, every record checked against `import.target_root` before anything is
    /// applied, with every leaf in `import.absent` removed.
    ///
    /// Refused, with this world unchanged:
    /// - `BadProof(i)` if record `i` does not check against the target root, or does not decode (a config record
    ///   whose values a world cannot hold does not decode);
    /// - `NeedsPhysicsImage(id)` if a record brings a `Body`, replaces an object that has one, or `absent` removes one,
    ///   without the physics image;
    /// - `RateMismatch` if the checked config record names a rate other than this world's (M1 runs one rate);
    /// - `PhysicsCheck` if the image's `global` is not the target's `physics_global`;
    /// - `RootMismatch` for a registry, lines or modules table other than this world's (M1 imports none of them).
    ///
    /// `import.absent` is not proven here: removing a leaf has no record to check. The caller's comparison of the
    /// patched root with the target root is what covers it (pe-replica restores the live state on a mismatch), so that
    /// comparison must travel with every use of this call. The physics check needs a domain node, which only an object
    /// record's proof carries: an import with a physics image and no object record takes the image's `global` as
    /// given, and the root comparison covers that too (@chunks-reviewer's Lows on ce6b13f0).
    ///
    /// A `LeafKey::Config` record sets every config setting (6.6), the seed included, so a joiner imports into a
    /// bare `World::new` (engine registry, default settings). The caller (pe-replica) compares the patched root with
    /// the target, which also covers a config that was not imported.
    pub fn import(&self, domain: DomainId, import: &CutImport) -> Result<World, ImportError> {
        let target = import.target_root;
        let mut raw_objects = Vec::new();
        let mut tables: Vec<(u8, &Record)> = Vec::new();
        let mut physics_leaf = None;
        let mut config = None;
        for (i, rec) in import.records.iter().enumerate() {
            match rec.key {
                LeafKey::Object(id) => {
                    let dom =
                        check_object(target, domain, id, &rec.bytes, &rec.proof).ok_or(ImportError::BadProof(i))?;
                    if let Node::Domain { physics, .. } = dom {
                        physics_leaf = Some(physics);
                    }
                    raw_objects.push((i, id, rec));
                }
                LeafKey::Table(t) if (t as usize) < table::COUNT => {
                    check_table(target, t, &rec.bytes, &rec.proof).ok_or(ImportError::BadProof(i))?;
                    tables.push((t, rec));
                }
                LeafKey::Config => {
                    check_config(target, &rec.bytes, &rec.proof).ok_or(ImportError::BadProof(i))?;
                    let c = decode_config(&rec.bytes).ok_or(ImportError::BadProof(i))?;
                    // M1 runs a world at one rate (6.6): a checked config naming another rate is a fault to
                    // report, never a rate to adopt.
                    if c.rate != self.rate {
                        return Err(ImportError::RateMismatch { world: c.rate, mine: self.rate });
                    }
                    config = Some(c);
                }
                LeafKey::Table(_) | LeafKey::Physics(_) => return Err(ImportError::BadProof(i)),
            }
        }
        // The registry first (6.6): the union of this world's and the target's, the target's entry winning, so every
        // object record decodes as the target encoded it; pruned to the target's after the patch.
        let target_registry = match tables.iter().find(|(t, _)| *t == table::REGISTRY) {
            Some((_, rec)) => {
                let i = import.records.iter().position(|r| std::ptr::eq(r, *rec)).unwrap_or(0);
                Some(decode_registry(&rec.bytes).ok_or(ImportError::BadProof(i))?)
            }
            None => None,
        };
        let mut base = self.clone();
        if let Some(entries) = &target_registry {
            let mut merged: BTreeMap<ComponentId, pe_sim::ComponentDesc> =
                self.store.registry().iter().map(|d| (d.id(), d.clone())).collect();
            let mut assets = (*self.script_ids).clone();
            for (asset, d) in entries {
                merged.insert(d.id(), d.clone());
                if *asset == [0; 32] {
                    assets.remove(&d.id());
                } else {
                    assets.insert(d.id(), *asset);
                }
            }
            base.store.set_registry(Arc::new(registry_of(merged.into_values())?)).map_err(|_| mismatch())?;
            base.script_ids = Arc::new(assets);
        }
        let mut objects = Vec::new();
        for (i, id, rec) in raw_objects {
            let o = decode_object(&base, &rec.bytes).filter(|o| o.id == id).ok_or(ImportError::BadProof(i))?;
            objects.push((i, o));
        }
        let has_body = |w: &World, id| w.store.get_raw(ids::BODY, id).is_some();
        if import.physics.is_none() {
            for (_, o) in &objects {
                if o.rows.iter().any(|r| r.0 == ids::BODY) || has_body(self, o.id) {
                    return Err(ImportError::NeedsPhysicsImage(o.id));
                }
            }
            for k in &import.absent {
                if let LeafKey::Object(id) = k
                    && has_body(self, *id)
                {
                    return Err(ImportError::NeedsPhysicsImage(*id));
                }
            }
        }
        let mut w = base;
        if let Some(c) = config {
            w.store.set_seed(c.seed);
            w.line = c.line;
            w.m1_marker = c.m1_marker;
            w.agree_settings = c.agree_settings.into();
            w.depth_limit = c.depth_limit;
            w.gravity = c.gravity;
            w.origin = c.origin;
            w.input_settings = c.input_settings;
        }
        if let Some(img) = &import.physics {
            if physics_leaf.is_some_and(|p| p != img.global) {
                return Err(ImportError::PhysicsCheck);
            }
            w.physics_global = img.global;
        }
        for k in &import.absent {
            match k {
                LeafKey::Object(id) => {
                    w.store.import_remove(*id);
                }
                LeafKey::Table(t) => w.clear_table(*t),
                LeafKey::Physics(_) | LeafKey::Config => {}
            }
        }
        for (i, o) in &objects {
            let rows: Vec<_> = o.rows.iter().map(|(c, b, v)| (*c, b.as_slice(), *v)).collect();
            w.store.import_object(o.id, &o.node, &rows).map_err(|_| ImportError::BadProof(*i))?;
        }
        for (t, rec) in tables {
            if t == table::REGISTRY {
                continue;
            }
            let i = import.records.iter().position(|r| std::ptr::eq(r, rec)).unwrap_or(0);
            w.import_table(t, &rec.bytes).ok_or(ImportError::BadProof(i))??;
        }
        // Prune to the target's registry: a component it lacks must have no rows left, or the patched world cannot
        // be the target's (`RootMismatch`); the importer itself is untouched either way.
        if let Some(entries) = target_registry {
            let keep: BTreeMap<ComponentId, AssetId> = entries.iter().map(|(a, d)| (d.id(), *a)).collect();
            if w.store.registry().iter().any(|d| !keep.contains_key(&d.id()) && w.store.count(d.id()) > 0) {
                return Err(mismatch());
            }
            w.store
                .set_registry(Arc::new(registry_of(entries.into_iter().map(|(_, d)| d))?))
                .map_err(|_| mismatch())?;
            w.script_ids = Arc::new(keep.into_iter().filter(|(_, a)| *a != [0; 32]).collect());
        }
        w.store.set_frame(import.cut.at.frame);
        Ok(w)
    }

    fn clear_table(&mut self, t: u8) {
        match t {
            table::GOVERNANCE => self.members = Arc::default(),
            table::CLAIMS => self.claims = Arc::default(),
            table::COOLDOWNS => Arc::make_mut(&mut self.inputs).cooldowns.clear(),
            table::TOMBSTONES => self.tombstones = Arc::default(),
            table::HAND_PACKS => Arc::make_mut(&mut self.inputs).packs.clear(),
            table::PUSH_WINDOWS => Arc::make_mut(&mut self.inputs).pushes.clear(),
            _ => {}
        }
    }

    /// Replaces table `t` from its record. `None` if the record does not decode; `Some(Err)` for the lines table,
    /// which M1 cannot import unless it equals this world's. The registry is applied by `import` itself.
    fn import_table(&mut self, t: u8, bytes: &[u8]) -> Option<Result<(), ImportError>> {
        let entries = table_entries(bytes)?;
        match t {
            table::LINES => {
                if bytes != table_record(&self.table_entries(t)).as_slice() {
                    return Some(Err(ImportError::RootMismatch { got: Hash::ZERO }));
                }
            }
            table::MODULES => {
                let (mut m, mut p) = (BTreeMap::new(), BTreeMap::new());
                for (k, v) in entries {
                    let a = <AssetId>::try_from(k).ok()?;
                    let (current, pending) = crate::script::modules_value_from(v)?;
                    if let Some(c) = current {
                        m.insert(a, c);
                    }
                    if let Some(c) = pending {
                        p.insert(a, c);
                    }
                }
                self.modules = Arc::new(m);
                self.pending = Arc::new(p);
            }
            table::GOVERNANCE => {
                let mut m = BTreeMap::new();
                for (k, v) in entries {
                    m.insert(key_member(k)?, u32::from_le_bytes(v.try_into().ok()?));
                }
                self.members = Arc::new(m);
            }
            table::CLAIMS => {
                let mut m = BTreeMap::new();
                for (k, v) in entries {
                    let mut r = Rd(v);
                    let c = Claim {
                        hand: r.id()?,
                        member: MemberKey(r.arr()?),
                        since: Frame(r.u64()?),
                        until: Frame(r.u64()?),
                    };
                    r.end()?;
                    m.insert(key_id(k)?, c);
                }
                self.claims = Arc::new(m);
            }
            table::COOLDOWNS => {
                let mut m = BTreeMap::new();
                for (k, v) in entries {
                    if k.len() != 48 {
                        return None;
                    }
                    let until = Frame(u64::from_le_bytes(v.try_into().ok()?));
                    m.insert((key_member(&k[..32])?, key_id(&k[32..])?), until);
                }
                Arc::make_mut(&mut self.inputs).cooldowns = m;
            }
            table::TOMBSTONES => {
                let mut m = BTreeMap::new();
                for (k, v) in entries {
                    m.insert(key_id(k)?, self.read_tombstone(v)?);
                }
                self.tombstones = Arc::new(m);
            }
            table::HAND_PACKS => {
                let mut m = BTreeMap::new();
                for (k, v) in entries {
                    let mut r = Rd(v);
                    let stamp = Frame(r.u64()?);
                    let hold = r.u8()?;
                    let n = r.u32()?;
                    let mut poses = Vec::new();
                    for _ in 0..n {
                        let pos = r.pos()?;
                        let rot = [r.f32()?, r.f32()?, r.f32()?, r.f32()?];
                        let grip = r.f32()?;
                        let engaged = match r.u8()? {
                            0 => false,
                            1 => true,
                            _ => return None,
                        };
                        poses.push(HandPose { pos, rot, grip, engaged });
                    }
                    r.end()?;
                    m.insert(key_id(k)?, Pack { stamp, poses, hold });
                }
                Arc::make_mut(&mut self.inputs).packs = m;
            }
            table::PUSH_WINDOWS => {
                let mut m = BTreeMap::new();
                for (k, v) in entries {
                    let mut r = Rd(v);
                    let n = r.u32()?;
                    let mut q = VecDeque::new();
                    for _ in 0..n {
                        q.push_back((Frame(r.u64()?), f64::from_bits(r.u64()?)));
                    }
                    r.end()?;
                    m.insert(key_member(k)?, q);
                }
                Arc::make_mut(&mut self.inputs).pushes = m;
            }
            _ => return None,
        }
        Some(Ok(()))
    }

    fn read_tombstone(&self, v: &[u8]) -> Option<Tombstone> {
        let mut r = Rd(v);
        let by = EntryRef(Hash(r.arr()?));
        let frame = Frame(r.u64()?);
        let expires = Frame(r.u64()?);
        let parent = r.opt_id()?;
        let place = r.u32()?;
        let world = match r.u8()? {
            0 => None,
            1 => {
                let pos = r.pos()?;
                let rot = [r.f32()?, r.f32()?, r.f32()?, r.f32()?];
                let scale = [r.f32()?, r.f32()?, r.f32()?];
                Some(WorldTransform { pos, rot, scale })
            }
            _ => return None,
        };
        let node = read_node(&mut r)?;
        let n = r.u32()?;
        let mut rows = Vec::new();
        for _ in 0..n {
            let c = ComponentId(r.u128()?);
            let version = read_version(&mut r)?;
            let d = self.store.registry().get(c)?;
            rows.push((c, stored_row(d, r.lp()?)?, version));
        }
        r.end()?;
        Some(Tombstone { by, frame, expires, parent, place, world, node, rows })
    }
}
