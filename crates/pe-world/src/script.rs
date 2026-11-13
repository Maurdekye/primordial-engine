//! Scripts in the world (world-model.md 7.9, 9.2a; slice D): the tag, component ids from the version hash, the modules
//! table, `Register` and `CommitScript` with re-keying, the engine components a script may name, and the logic stages'
//! hooks (`World::logic_stage`, `World::after_logic`). pe-world never depends on pe-logic: the runner reaches the
//! world through plain types and closures.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use pe_core::commit::Hasher;
use pe_core::{EntryRef, Frame, Hash, MemberKey, ObjectId};
use pe_seams::{Reason, Version};
use pe_sim::{ComponentDesc, ComponentId, FieldDesc, RowVersion, SimStore};

use crate::apply::Footprint;
use crate::components::{BlobRef, Body, Light, SimSize, ids};
use crate::world::{TOMBSTONE_SECONDS, World};

pub use crate::source::{
    MAX_PATH, MAX_SEGMENT, MAX_SEGMENTS, MAX_SOURCE_BYTES, SOURCE_MAGIC, SourceError, check_source, pack_source,
    path_ok, source_files,
};

/// A script's stable asset id (logic.md 6.1), the modules table's key.
pub type AssetId = [u8; 32];

/// The name every script version's tag carries (7.9). An SDK name is a Rust identifier and has no `.`.
pub const TAG_NAME: &str = "pe.tag";

/// The most rows one commit's re-key moves (7.9) [I, to measure: the frame-time spike of moving them in one step].
pub const MAX_REKEY_ROWS: u32 = 32_768;

/// Script component ids start here: everything below is engine space (3.2).
pub const SCRIPT_ID_FLOOR: u128 = 1 << 32;

/// One script asset's modules row (5.4, 6.6).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModuleRow {
    /// The commit that wrote it.
    pub version: Version,
    /// The source archive (logic.md 4.1a); its hash is the version hash V.
    pub source: BlobRef,
    pub toolchain: Hash,
    pub sdk: Hash,
    pub author_module: Hash,
    /// The version's component ids, strictly ascending, its tag among them.
    pub components: Vec<ComponentId>,
    pub activate_at: Frame,
    /// The frame and kind (pe-logic's `Fault::code()`, 1 to 13) of the fault that stopped it, until its next commit.
    pub faulted: Option<(Frame, u8)>,
}

impl ModuleRow {
    /// The version hash V.
    pub fn v(&self) -> Hash {
        Hash(self.source.hash)
    }

    /// 6.6's value bytes.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&self.version.entry.0.0);
        b.extend_from_slice(&self.version.op.to_le_bytes());
        b.extend_from_slice(&self.source.hash);
        b.extend_from_slice(&self.source.len.to_le_bytes());
        b.extend_from_slice(&self.toolchain.0);
        b.extend_from_slice(&self.sdk.0);
        b.extend_from_slice(&self.author_module.0);
        b.extend_from_slice(&(self.components.len() as u32).to_le_bytes());
        for c in &self.components {
            b.extend_from_slice(&c.0.to_le_bytes());
        }
        b.extend_from_slice(&self.activate_at.0.to_le_bytes());
        match self.faulted {
            None => b.push(0),
            Some((f, k)) => {
                b.push(1);
                b.extend_from_slice(&f.0.to_le_bytes());
                b.push(k);
            }
        }
        b
    }

    /// Strict decoding of 6.6's value bytes.
    pub fn from_bytes(b: &[u8]) -> Option<ModuleRow> {
        let mut r = Rd(b);
        let entry = EntryRef(Hash(r.arr()?));
        let op = u32::from_le_bytes(r.arr()?);
        let source = BlobRef { hash: r.arr()?, len: u64::from_le_bytes(r.arr()?) };
        let toolchain = Hash(r.arr()?);
        let sdk = Hash(r.arr()?);
        let author_module = Hash(r.arr()?);
        let n = u32::from_le_bytes(r.arr()?) as usize;
        if n > r.0.len() / 16 {
            return None;
        }
        let mut components = Vec::with_capacity(n);
        for _ in 0..n {
            let c = ComponentId(u128::from_le_bytes(r.arr()?));
            if components.last().is_some_and(|l: &ComponentId| *l >= c) {
                return None;
            }
            components.push(c);
        }
        let activate_at = Frame(u64::from_le_bytes(r.arr()?));
        let faulted = match r.arr::<1>()?[0] {
            0 => None,
            1 => {
                let f = Frame(u64::from_le_bytes(r.arr()?));
                let k = r.arr::<1>()?[0];
                if !(1..=13).contains(&k) {
                    return None;
                }
                Some((f, k))
            }
            _ => return None,
        };
        if !r.0.is_empty() {
            return None;
        }
        Some(ModuleRow {
            version: Version { entry, op },
            source,
            toolchain,
            sdk,
            author_module,
            components,
            activate_at,
            faulted,
        })
    }
}

struct Rd<'a>(&'a [u8]);

impl Rd<'_> {
    fn arr<const N: usize>(&mut self) -> Option<[u8; N]> {
        let (h, t) = self.0.split_first_chunk::<N>()?;
        self.0 = t;
        Some(*h)
    }
}

/// The one id derivation for a script's component (3.2, 7.9): `u128::from_le_bytes(BLAKE3("pe.component" ‖ V ‖
/// lp(name) ‖ schema hash)[..16])`, where the schema hash is 6.6's hash of the description, which has no id in it.
/// `None` if the fields do not make a valid description. `Register`'s check and pe-replica's binding both call it.
pub fn script_component_id(v: &Hash, name: &str, fields: Vec<FieldDesc>) -> Option<ComponentId> {
    let d = ComponentDesc::new(ComponentId(0), name, fields).ok()?;
    let mut h = Hasher::new("pe.component");
    h.write_hash(v).write_len_prefixed(name.as_bytes()).write_hash(&crate::tree::schema_hash(&d));
    let id = u128::from_le_bytes(h.finish().0[..16].try_into().expect("16 bytes"));
    Some(ComponentId(id))
}

/// Version `v`'s tag (7.9): zero size, named [`TAG_NAME`].
pub fn tag_desc(v: &Hash) -> ComponentDesc {
    let id = script_component_id(v, TAG_NAME, Vec::new()).expect("an empty description is valid");
    ComponentDesc::new(id, TAG_NAME, Vec::new()).expect("an empty description is valid")
}

/// An engine component a script may name in its manifest (`pe.X`), and whether it may write it (3.2's table).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EngineComponent {
    pub name: &'static str,
    pub id: ComponentId,
    pub scripts_may_write: bool,
}

/// The engine components a script may name (3.2): read any, write only those marked.
pub const ENGINE_COMPONENTS: [EngineComponent; 10] = [
    EngineComponent { name: "pe.Transform", id: ComponentId::TRANSFORM, scripts_may_write: true },
    EngineComponent { name: "pe.Light", id: ids::LIGHT, scripts_may_write: true },
    EngineComponent { name: "pe.Appearance", id: ids::APPEARANCE, scripts_may_write: true },
    EngineComponent { name: "pe.Name", id: ids::NAME, scripts_may_write: false },
    EngineComponent { name: "pe.Shape", id: ids::SHAPE, scripts_may_write: false },
    EngineComponent { name: "pe.Body", id: ids::BODY, scripts_may_write: false },
    EngineComponent { name: "pe.BodyState", id: ids::BODY_STATE, scripts_may_write: false },
    EngineComponent { name: "pe.Hand", id: ids::HAND, scripts_may_write: false },
    EngineComponent { name: "pe.SimSize", id: ids::SIM_SIZE, scripts_may_write: false },
    EngineComponent { name: "pe.Protection", id: ids::PROTECTION, scripts_may_write: false },
];

/// What a logic stage's runner reported (9.2a), in plain types.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LogicReport {
    /// Objects created, in order, each with its spawner.
    pub created: Vec<(ObjectId, ObjectId)>,
    /// Every object the surviving destroys removed, in order.
    pub removed: Vec<ObjectId>,
    /// Untagged objects beneath a destroyed one, made roots in place.
    pub orphaned: Vec<ObjectId>,
    /// Rows written, in order.
    pub written: Vec<(ComponentId, ObjectId)>,
    /// Faulting assets with their fault's code, in order.
    pub faults: Vec<(AssetId, u8)>,
}

/// What a re-key does or would do (7.9): the objects it moves rows on, the row count, the losses and the objects
/// whose losses block the commit.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RekeyReport {
    pub objects: Vec<ObjectId>,
    pub rows: u32,
    pub losses: Vec<Loss>,
    pub blocked: Vec<ObjectId>,
}

/// One kind of data a re-key loses: a component's field zeroed or dropped, or a whole component dropped.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Loss {
    pub component: String,
    /// `None`: the whole component.
    pub field: Option<String>,
    pub zeroed: bool,
}

/// One row a re-key moves, live or held in a tombstone.
pub(crate) struct Move {
    object: ObjectId,
    from: ComponentId,
    to: Option<(ComponentId, Vec<u8>)>,
    /// The row is in the object's tombstone (7.5), not the store: `Undelete` must bring back the new version's rows.
    stone: bool,
}

/// Maps `row` of `old` onto `new` (7.9): a field with the same name, kind, count and unit copies; anything else starts
/// at zero. Returns the new row and the losses: a non-zero field zeroed or dropped. Padding is never a loss.
fn map_row(old: &ComponentDesc, new: &ComponentDesc, row: &[u8]) -> (Vec<u8>, Vec<Loss>) {
    let mut out = vec![0u8; new.size()];
    let mut losses = Vec::new();
    for f in old.fields().iter().filter(|f| !f.name.starts_with('_')) {
        let at = f.offset as usize;
        let len = f.kind.size() as usize * f.count as usize;
        let bytes = &row[at..at + len];
        let same = new.fields().iter().find(|g| g.name == f.name);
        match same {
            Some(g) if g.kind == f.kind && g.count == f.count && g.unit == f.unit => {
                out[g.offset as usize..g.offset as usize + len].copy_from_slice(bytes);
            }
            Some(_) | None if bytes.iter().all(|&b| b == 0) => {}
            Some(_) => losses.push(Loss { component: old.name().into(), field: Some(f.name.clone()), zeroed: true }),
            None => losses.push(Loss { component: old.name().into(), field: Some(f.name.clone()), zeroed: false }),
        }
    }
    (out, losses)
}

impl World {
    /// The modules table (5.4): each script asset's row.
    pub fn modules(&self) -> &BTreeMap<AssetId, ModuleRow> {
        &self.modules
    }

    /// The asset that registered script component `c`, if it is one.
    pub fn script_asset(&self, c: ComponentId) -> Option<AssetId> {
        self.script_ids.get(&c).copied()
    }

    /// Whether `c` is a script's tag.
    pub(crate) fn is_tag(&self, c: ComponentId) -> bool {
        self.script_ids.contains_key(&c) && self.store.registry().get(c).is_some_and(|d| d.name() == TAG_NAME)
    }

    /// `Stale` if `c` is a script component whose asset's current version does not list it (7.9): an author who
    /// resolved names before a commit they had not received.
    pub(crate) fn superseded(&self, c: ComponentId) -> Result<(), Reason> {
        let Some(a) = self.script_ids.get(&c) else { return Ok(()) };
        match self.modules.get(a) {
            Some(m) if m.components.contains(&c) => Ok(()),
            m => {
                let current = m.map_or(Version { entry: EntryRef(Hash::ZERO), op: 0 }, |m| m.version);
                Err(Reason::Stale { precondition: crate::edit::OWN_CHECK, current, winner: current.entry })
            }
        }
    }

    /// Registers `components` for asset `asset` at version `v` (7.9's `Register`): every id derived from `v`, none
    /// in engine space, exactly one tag; the same description and asset again is a no-op, anything else under a
    /// registered id is a collision. `Invalid` otherwise, with nothing changed.
    pub(crate) fn register_script(
        &mut self,
        asset: &AssetId,
        v: &Hash,
        components: &[ComponentDesc],
    ) -> Result<(), Reason> {
        let invalid = || Reason::Invalid { field: None };
        if components.iter().filter(|d| d.name() == TAG_NAME).count() != 1 {
            return Err(invalid());
        }
        for d in components {
            let want = script_component_id(v, d.name(), d.fields().to_vec()).ok_or_else(invalid)?;
            // No `.` outside the tag, so no script component can pose as an engine one (`pe.X`, @chunks-reviewer's
            // code Low 1).
            let posing = d.name() != TAG_NAME && d.name().contains('.');
            if d.id() != want || d.id().0 < SCRIPT_ID_FLOOR || posing || (d.name() == TAG_NAME && d.size() != 0) {
                return Err(invalid());
            }
            match (self.script_ids.get(&d.id()), self.store.registry().get(d.id())) {
                (Some(a), Some(have)) if a == asset && have == d => {}
                (None, None) => {}
                _ => return Err(invalid()),
            }
        }
        for d in components {
            self.store.register(d.clone()).map_err(|_| invalid())?;
            Arc::make_mut(&mut self.script_ids).insert(d.id(), *asset);
        }
        Ok(())
    }

    /// What re-keying `old` (the base version's ids) onto `new` (the new version's descriptions) would do for
    /// `committer` (7.9), and the moves. Pure: a dry run (pe-api) calls it to show the cost before the commit.
    pub(crate) fn rekey_plan(
        &self,
        old: &[ComponentId],
        new: &[ComponentDesc],
        committer: MemberKey,
    ) -> (RekeyReport, Vec<Move>) {
        let mut report = RekeyReport::default();
        let mut moves = Vec::new();
        let mut objects = BTreeSet::new();
        let mut blocked = BTreeSet::new();
        let mut losses = BTreeSet::new();
        let reg = self.store.registry();
        let map = |od: &ComponentDesc, row: &[u8]| match new.iter().find(|d| d.name() == od.name()) {
            Some(nd) => {
                let (b, l) = map_row(od, nd, row);
                (Some((nd.id(), b)), l)
            }
            None if row.iter().all(|&b| b == 0) => (None, Vec::new()),
            None => (None, vec![Loss { component: od.name().into(), field: None, zeroed: false }]),
        };
        for &o in old {
            let Some(od) = reg.get(o) else { continue };
            let (ids, bytes) = self.store.column_raw(o);
            for (k, &obj) in ids.iter().enumerate() {
                let (to, lost) = map(od, &bytes[k * od.size()..(k + 1) * od.size()]);
                if !lost.is_empty() && !self.committer_may_edit(committer, obj) {
                    blocked.insert(obj);
                }
                losses.extend(lost);
                objects.insert(obj);
                report.rows = report.rows.saturating_add(1);
                moves.push(Move { object: obj, from: o, to, stone: false });
            }
        }
        // Tombstones hold rows too (@chunks-reviewer's code M1): left on the old ids, an `Undelete` would bring back
        // rows no system binds and no later commit re-keys. A loss there is judged by the protection the object had.
        for (&obj, stone) in self.tombstones.iter() {
            for (c, row, _) in &stone.rows {
                let Some(od) = old.contains(c).then(|| reg.get(*c)).flatten() else { continue };
                let (to, lost) = map(od, row);
                if !lost.is_empty() && self.stone_protected_by(obj).is_some_and(|o| o != committer) {
                    blocked.insert(obj);
                }
                losses.extend(lost);
                objects.insert(obj);
                report.rows = report.rows.saturating_add(1);
                moves.push(Move { object: obj, from: *c, to, stone: true });
            }
        }
        report.objects = objects.into_iter().collect();
        report.blocked = blocked.into_iter().collect();
        report.losses = losses.into_iter().collect();
        (report, moves)
    }

    /// Whether `who` could edit `obj` for 7.9's loss rule: not held by another member, and not protected (7.8) by
    /// another owner.
    fn committer_may_edit(&self, who: MemberKey, obj: ObjectId) -> bool {
        self.claims.get(&obj).is_none_or(|c| c.member == who) && self.protected_by(obj).is_none_or(|o| o == who)
    }

    /// The owner whose protection (7.8) covered a deleted object: its tombstone's own `Protection` row, else its
    /// nearest ancestor's, live or tombstoned.
    fn stone_protected_by(&self, obj: ObjectId) -> Option<MemberKey> {
        let mut at = obj;
        for _ in 0..=crate::world::DEPTH_LIMIT_MAX {
            let Some(stone) = self.tombstones.get(&at) else { return self.protected_by(at) };
            if let Some((_, row, _)) = stone.rows.iter().find(|(c, _, _)| *c == ids::PROTECTION) {
                return Some(MemberKey(row[..32].try_into().ok()?));
            }
            at = stone.parent?;
        }
        None
    }

    /// Applies `moves` as op `op` of entry `entry`: each moved row takes the commit as its row version and keeps its
    /// instance tag. Live objects are touched; a tombstone's rows are rewritten in place, in component-id order.
    pub(crate) fn apply_rekey(&mut self, moves: Vec<Move>, entry: EntryRef, op: u32) -> Result<(), Reason> {
        let v = RowVersion { entry: entry.0.0, op, tag: [0; 16], _pad: 0 };
        for m in moves {
            if m.stone {
                let stone =
                    Arc::make_mut(&mut self.tombstones).get_mut(&m.object).ok_or(Reason::Invalid { field: None })?;
                let k = stone.rows.iter().position(|(c, _, _)| *c == m.from).ok_or(Reason::Invalid { field: None })?;
                let (_, _, rv) = stone.rows.remove(k);
                if let Some((to, bytes)) = m.to {
                    stone.rows.push((to, bytes, RowVersion { tag: rv.tag, ..v }));
                    stone.rows.sort_by_key(|(c, _, _)| *c);
                }
                continue;
            }
            let tag = self.store.row_version(m.from, m.object).map_or([0; 16], |r| r.tag);
            self.store.remove_raw(m.from, m.object).map_err(|_| Reason::Invalid { field: None })?;
            if let Some((to, bytes)) = m.to {
                self.store.insert_raw(to, m.object, &bytes).map_err(|_| Reason::Invalid { field: None })?;
                self.store.set_row_version(to, m.object, RowVersion { tag, ..v });
            }
            self.touch(m.object, Version { entry, op }, false);
        }
        Ok(())
    }

    /// The per-row edit checks for a row a script writes to an engine component (7.9), with no author: the
    /// component must be marked writable, a hand carries no script's writes, a held object's pose is the holder's,
    /// and the row must be valid as an edit's would be. pe-replica passes the returned closure to the runner, which
    /// faults the call on an `Err`. The sets it needs are taken now, so it borrows nothing of the world.
    pub fn script_row_checker(&self) -> impl Fn(ComponentId, ObjectId, &[u8]) -> Result<(), String> + use<> {
        let held: BTreeSet<ObjectId> = self.claims.keys().copied().collect();
        let hands: BTreeSet<ObjectId> = self.store.column_raw(ids::HAND).0.iter().copied().collect();
        move |c, obj, bytes| {
            let e = ENGINE_COMPONENTS.iter().find(|e| e.id == c).ok_or("not an engine component a script names")?;
            if !e.scripts_may_write {
                return Err(format!("{} is not written by scripts", e.name));
            }
            if hands.contains(&obj) {
                return Err("a hand takes no script's writes".into());
            }
            if c == ComponentId::TRANSFORM && held.contains(&obj) {
                return Err("held".into());
            }
            if c == ids::LIGHT {
                let l: Light = bytemuck::try_pod_read_unaligned(bytes).map_err(|_| "Light size")?;
                if let Some(f) = l.invalid_angle() {
                    return Err(format!("Light field {f}"));
                }
            }
            Ok(())
        }
    }

    /// Runs one logic stage (9.2a): keeps the stage-start state, then calls `f` with the store, the guard (objects a
    /// script may not remove: held ones), the faulted assets and the row check. pe-replica builds pe-logic's
    /// `StageCtx` from them and returns the runner's report through `f`.
    pub fn logic_stage<R>(
        &mut self,
        f: impl FnOnce(
            &mut SimStore,
            &dyn Fn(ObjectId) -> bool,
            &BTreeSet<AssetId>,
            &dyn Fn(ComponentId, ObjectId, &[u8]) -> Result<(), String>,
        ) -> R,
    ) -> R {
        self.stage_start = Some(self.store.clone());
        let held: BTreeSet<ObjectId> = self.claims.keys().copied().collect();
        let guard = move |o: ObjectId| held.contains(&o);
        let faulted: BTreeSet<AssetId> =
            self.modules.iter().filter(|(_, m)| m.faulted.is_some()).map(|(a, _)| *a).collect();
        let check = self.script_row_checker();
        f(&mut self.store, &guard, &faulted, &check)
    }

    /// Records what a logic stage did (9.2a): tombstones for the removed objects from the stage-start state, `SimSize`
    /// for the created ones, the faulted marks, and the physics effects for pe-replica.
    pub fn after_logic(&mut self, frame: Frame, report: &LogicReport) -> Footprint {
        // Without `logic_stage` first there is no stage-start state and the removed objects would leave no tombstones:
        // a wiring slip, loud in debug builds (@chunks-reviewer's code Low 2).
        debug_assert!(self.stage_start.is_some(), "after_logic without logic_stage");
        let start = self.stage_start.take().unwrap_or_else(|| self.store.clone());
        let mut fp = Footprint::default();
        let expires =
            Frame(frame.0.saturating_add(self.rate.frames(std::time::Duration::from_secs(TOMBSTONE_SECONDS))));
        let removed: BTreeSet<ObjectId> = report.removed.iter().copied().collect();
        let mut places: BTreeMap<ObjectId, u32> = BTreeMap::new();
        let mut stones = Vec::new();
        for &d in &report.removed {
            // An object created and removed in this stage has no stage-start record and leaves no tombstone.
            if !start.exists(d) {
                continue;
            }
            let up = start.parent(d);
            let place = match up {
                Some(p) if !removed.contains(&p) => start.children(p).position(|c| c == d).unwrap_or(0) as u32,
                Some(p) => {
                    let n = places.entry(p).or_insert(0);
                    *n += 1;
                    *n - 1
                }
                None => 0,
            };
            stones.push((d, crate::apply::tombstone_of(&start, d, EntryRef(Hash::ZERO), frame, expires, up, place)));
        }
        if !stones.is_empty() {
            let t = Arc::make_mut(&mut self.tombstones);
            for (d, s) in stones {
                t.insert(d, s);
            }
        }
        if self.claims.keys().any(|o| removed.contains(o)) {
            Arc::make_mut(&mut self.claims).retain(|o, _| !removed.contains(o));
        }
        for &(id, spawner) in &report.created {
            if !self.store.exists(id) {
                continue;
            }
            let scale = self
                .store
                .world_transform(spawner)
                .map_or(1.0, |t| t.scale.iter().fold(0.0_f32, |m, s| m.max(s.abs())) as f64);
            let metres = if scale.is_finite() && scale > 0.0 { scale } else { 1.0 };
            let _ = self.store.insert(id, SimSize { metres });
            fp.created.push(id);
        }
        for &(asset, code) in &report.faults {
            if let Some(m) = self.modules.get(&asset)
                && m.faulted.is_none()
                && (1..=13).contains(&code)
            {
                Arc::make_mut(&mut self.modules).get_mut(&asset).expect("present").faulted = Some((frame, code));
            }
        }
        fp.removed.extend(report.removed.iter().filter(|o| start.has::<Body>(**o)).copied());
        for &o in &report.orphaned {
            fp.reparented.push((o, true));
        }
        for &(c, o) in &report.written {
            if c == ComponentId::TRANSFORM && self.store.has::<Body>(o) && !fp.rows.contains(&(o, c)) {
                fp.rows.push((o, c));
            }
        }
        fp
    }
}
