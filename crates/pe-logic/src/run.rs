//! The batch runner (docs/spec/logic.md 3.3, 3.5, 3.6): a world's scripts, run as systems over the store.
//!
//! A frame's logic is two stages, `pre_physics` then `post_physics` (pe-seams `Stage::ORDER`). In each, scripts run
//! in the order of their keys, and each script's systems of that stage in manifest order (3.5). One system call:
//! 1. the bound objects are the store's objects that have every bound component, in id order;
//! 2. they are split into batches that fit the SDK's area, and each batch is copied in (ids, then one column per
//!    bound component) and run through `pe_run`;
//! 3. after the last batch, the rows the guest changed in its write columns are validated, then written, then the
//!    commands are applied in buffer order.
//!
//! Host functions read the store as it was at the start of the system call, so nothing of a call is applied until
//! all its batches have run. A fault in any batch discards the whole call: no row and no command of it is applied,
//! and the script runs no more systems this frame (3.6). The fault is reported in [`StageReport::faults`]; keeping
//! it as world state is the caller's (the `faulted` set it passes in).
//!
//! The runner keeps no state from one frame to the next that affects results: instances are reset at the start of
//! each frame's `pre_physics` stage, and the counts behind created ids restart there. So a frame replayed after a
//! rollback runs exactly as it did the first time, provided the caller runs both stages of every frame, in order.

use std::collections::{BTreeMap, BTreeSet};

use pe_core::rng::{Rng, mix64};
use pe_core::{Frame, Hash, LineId, ObjectId};
use pe_sdk::layout::{
    Access, BATCH_ALIGN, BatchHeader, CMD_LEN_AT, COMMAND_HEADER, Command, FieldKind, HEADER_BYTES, Stage,
};
use pe_sim::{ComponentDesc, ComponentId, FieldDesc, Registry, SimStore};

use crate::manifest::{self, Manifest};
use crate::runtime::{Fault, Instance, NoServices, Script, Services};

/// A script asset's key: its stable asset id, the same for every version (the `modules` table's row, 6.1), never a
/// version's hash. Scripts run in key order within a stage (3.5), a fault names the script by it, and the spawn mark
/// names it, so a new version can still destroy what an earlier one spawned (3.3).
pub type ScriptKey = Hash;

/// How the runner runs calls. Part of a world's identity, like the engine's [`crate::Settings`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RunSettings {
    /// Fuel for one `pe_run` call (one batch). [I: a placeholder until the fuel classes of 5.2 are set.]
    pub fuel_per_call: u64,
    /// Skip copying in a column the instance's area already holds unchanged (3.3a (c)). Nothing a script can see
    /// depends on it; the switch is for tests and measurements.
    pub skip_unchanged: bool,
}

impl Default for RunSettings {
    fn default() -> Self {
        RunSettings { fuel_per_call: 50_000_000, skip_unchanged: true }
    }
}

/// The store's description of a component a script declares, under the id the world model assigned it.
pub fn component_desc(c: &manifest::Component, id: ComponentId) -> Result<ComponentDesc, pe_core::Error> {
    let fields = c.fields.iter().map(|f| FieldDesc::new(&f.name, sim_kind(f.kind), f.offset as usize, f.count));
    ComponentDesc::new(id, &c.name, fields.collect())
}

/// The spawn mark: the store's description of the row the runner puts on every object a script creates, naming the
/// script. A script may destroy only objects whose mark names it (3.3), so the world keeps this component out of
/// every edit's reach: no edit may write or remove it [I: WS-world reserves the id].
pub fn spawn_mark_desc(id: ComponentId) -> ComponentDesc {
    ComponentDesc::new(id, "Spawned", vec![FieldDesc::new("script", pe_sim::FieldKind::Hash, 0, 1)])
        .expect("one 32-byte field")
}

fn sim_kind(k: FieldKind) -> pe_sim::FieldKind {
    use pe_sim::FieldKind as S;
    match k {
        FieldKind::U8 => S::U8,
        FieldKind::U16 => S::U16,
        FieldKind::U32 => S::U32,
        FieldKind::U64 => S::U64,
        FieldKind::I8 => S::I8,
        FieldKind::I16 => S::I16,
        FieldKind::I32 => S::I32,
        FieldKind::I64 => S::I64,
        FieldKind::F32 => S::F32,
        FieldKind::F64 => S::F64,
        FieldKind::Bool => S::Bool,
        FieldKind::ObjectId => S::ObjectRef,
        FieldKind::Pos => S::Pos,
    }
}

/// Why a script could not be bound to a world.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BindError(pub String);

impl std::fmt::Display for BindError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "bind: {}", self.0)
    }
}

impl std::error::Error for BindError {}

/// A script's manifest, read from a fresh instance.
pub fn read_manifest(script: &Script) -> Result<Manifest, BindError> {
    let mut i = script.instantiate(Box::new(NoServices)).map_err(|e| BindError(format!("instantiate: {e}")))?;
    let (packed, _) =
        i.call::<(), i64>("pe_manifest", (), 1 << 40).map_err(|f| BindError(format!("pe_manifest: {f:?}")))?;
    let (ptr, len) = ((packed as u64 & 0xffff_ffff) as usize, (packed as u64 >> 32) as usize);
    let bytes = i.memory().get(ptr..ptr.saturating_add(len)).ok_or(BindError("manifest outside memory".into()))?;
    manifest::decode(bytes).map_err(|e| BindError(e.to_string()))
}

/// One bound component of a system.
#[derive(Clone, Copy, Debug)]
struct Bound {
    id: ComponentId,
    access: Access,
    size: usize,
}

/// A system, resolved against the world's components, with its batch layout. The layout is the script's: every
/// component any of its systems binds has one offset in the area, and every batch has room for the same number of
/// rows (a smaller last batch uses the same offsets). So a column stays where it was from one call to the next, and
/// the copy-in skip (3.3a (c)) works across systems.
#[derive(Clone, Debug)]
struct Plan {
    index: u32,
    name: String,
    stage: Stage,
    creates: bool,
    emits: usize,
    bound: Vec<Bound>,
    reads_other: Vec<(ComponentId, usize)>,
    rows: usize,
    ids_at: u32,
    columns: Vec<u32>,
    cmd_at: u32,
    cmd_cap: u32,
}

fn align(x: usize) -> usize {
    x.div_ceil(BATCH_ALIGN) * BATCH_ALIGN
}

/// The largest batch that fits the area with a command buffer of a quarter of it, and its offsets.
fn layout(cap: usize, sizes: &[usize]) -> Option<(usize, u32, Vec<u32>, u32, u32)> {
    let cmd_cap = (cap / 4) / 8 * 8;
    let at = |rows: usize| {
        let ids = align(HEADER_BYTES);
        let mut end = ids + 16 * rows;
        let mut cols = Vec::with_capacity(sizes.len());
        for s in sizes {
            let c = align(end);
            cols.push(c as u32);
            end = c + s * rows;
        }
        let cmd = align(end);
        (cmd + cmd_cap <= cap).then_some((rows, ids as u32, cols, cmd as u32, cmd_cap as u32))
    };
    let per_row = 16 + sizes.iter().sum::<usize>();
    let room = cap.saturating_sub(cmd_cap + align(HEADER_BYTES) + BATCH_ALIGN * (sizes.len() + 1));
    let mut rows = room / per_row;
    while rows > 0 {
        if let Some(l) = at(rows) {
            return Some(l);
        }
        rows -= 1;
    }
    None
}

/// A column the area holds that equals the store's rows for the ids the area holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Resident {
    at: u32,
    id: ComponentId,
    bytes: usize,
}

/// A script bound to a world: its module, its manifest resolved against the world's components, and its live
/// instance.
pub struct Loaded {
    key: ScriptKey,
    script: Script,
    manifest: Manifest,
    components: Vec<ComponentId>,
    plans: Vec<Plan>,
    inst: Instance,
    /// What the area holds from the last accepted call this stage, for the copy-in skip.
    resident: Vec<Resident>,
    resident_ids: Vec<ObjectId>,
}

impl Loaded {
    /// Bind `script` (whose manifest is `manifest`, from [`read_manifest`]) with its components under `ids`, in the
    /// manifest's order. Every one must already be in `registry` with the description [`component_desc`] gives.
    pub fn bind(
        key: ScriptKey,
        script: Script,
        manifest: Manifest,
        ids: &[ComponentId],
        registry: &Registry,
    ) -> Result<Loaded, BindError> {
        if ids.len() != manifest.components.len() {
            return Err(BindError(format!("{} ids for {} components", ids.len(), manifest.components.len())));
        }
        for (c, &id) in manifest.components.iter().zip(ids) {
            let want = component_desc(c, id).map_err(|e| BindError(format!("component {}: {e}", c.name)))?;
            if registry.get(id) != Some(&want) {
                return Err(BindError(format!("component {} is not registered as declared", c.name)));
            }
        }
        let resolve = |name: &str| {
            let k = manifest.components.iter().position(|c| c.name == name);
            k.map(|k| (ids[k], manifest.components[k].size as usize))
                .ok_or_else(|| BindError(format!("component {name} is not one the script declares")))
        };
        let cap = manifest.batch_cap as usize;
        // The script's columns: every component a system binds, in the manifest's order.
        let used: Vec<usize> = (0..manifest.components.len())
            .filter(|&k| {
                manifest.systems.iter().any(|s| s.bound.iter().any(|(n, _)| *n == manifest.components[k].name))
            })
            .collect();
        let sizes: Vec<usize> = used.iter().map(|&k| manifest.components[k].size as usize).collect();
        let (rows, ids_at, at, cmd_at, cmd_cap) = layout(cap, &sizes)
            .ok_or_else(|| BindError("one row of every bound component does not fit the batch area".into()))?;
        let column_of =
            |id: ComponentId| at[used.iter().position(|&k| ids[k] == id).expect("bound components are used")];
        let mut plans = Vec::new();
        for (index, s) in manifest.systems.iter().enumerate() {
            let mut bound = Vec::new();
            for (name, access) in &s.bound {
                let (id, size) = resolve(name)?;
                if bound.iter().any(|b: &Bound| b.id == id) {
                    return Err(BindError(format!("system {}: {name} is bound twice", s.name)));
                }
                bound.push(Bound { id, access: *access, size });
            }
            if bound.is_empty() {
                return Err(BindError(format!("system {} binds no component", s.name)));
            }
            let reads_other = s.reads_other.iter().map(|n| resolve(n)).collect::<Result<Vec<_>, _>>()?;
            let columns = bound.iter().map(|b| column_of(b.id)).collect();
            plans.push(Plan {
                index: index as u32,
                name: s.name.clone(),
                stage: s.stage,
                creates: s.creates,
                emits: s.emits.len(),
                bound,
                reads_other,
                rows,
                ids_at,
                columns,
                cmd_at,
                cmd_cap,
            });
        }
        let inst = script.instantiate(Box::new(NoServices)).map_err(|e| BindError(format!("instantiate: {e}")))?;
        let end = (manifest.batch_addr as usize).checked_add(cap);
        if !(manifest.batch_addr as usize).is_multiple_of(BATCH_ALIGN) || end.is_none_or(|e| e > inst.memory().len()) {
            return Err(BindError("the batch area is misaligned or outside the module's memory".into()));
        }
        Ok(Loaded {
            key,
            script,
            manifest,
            components: ids.to_vec(),
            plans,
            inst,
            resident: Vec::new(),
            resident_ids: Vec::new(),
        })
    }

    pub fn key(&self) -> ScriptKey {
        self.key
    }

    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }

    /// Back to the instance's state after instantiation (5.1), or a fresh instance if its memory grew.
    fn reset(&mut self) {
        if !self.inst.reset() {
            self.inst = self.script.instantiate(Box::new(NoServices)).expect("instantiated once at bind");
        }
        self.resident.clear();
        self.resident_ids.clear();
    }
}

/// An event a system emitted, for the events stage.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Event {
    pub script: ScriptKey,
    pub system: String,
    /// The event type's index in the system's `emits` list.
    pub index: u32,
    /// The object whose row emitted it.
    pub object: ObjectId,
    pub bytes: Vec<u8>,
}

/// A fault (3.6): the call's writes and commands were discarded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FaultReport {
    pub script: ScriptKey,
    pub system: String,
    pub frame: Frame,
    pub fault: Fault,
}

/// What one stage did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StageReport {
    /// Rows systems changed, in the order they were written.
    pub written: Vec<(ComponentId, ObjectId)>,
    /// Objects created, in order, each with its spawner (the bound object whose row created it): the world sizes a
    /// new object from its spawner's world scale (world-model.md 3.5).
    pub created: Vec<(ObjectId, ObjectId)>,
    /// Objects destroyed by command (each with what it spawned beneath it), in order.
    pub destroyed: Vec<ObjectId>,
    /// Objects that were beneath a destroyed one but not spawned by its script: made roots in place.
    pub orphaned: Vec<ObjectId>,
    /// Destroys that did not happen (a hold or a lock on the subtree, 3.3): not faults; the object stays.
    pub refused: Vec<(ObjectId, Refused)>,
    pub events: Vec<Event>,
    /// Faults, in order. Every one except [`Fault::HostStack`] is world state: the caller records it.
    pub faults: Vec<FaultReport>,
    /// `pe_run` calls made, and the fuel they used.
    pub calls: u32,
    pub fuel: u64,
    /// Column bytes copied in, and skipped because the area held them unchanged.
    pub copied_in: u64,
    pub skipped_in: u64,
    /// Batches run, and the host's merge-walk steps over sorted column ids to find the bound objects and their rows:
    /// host work fuel does not meter, so it is linear in the bound columns' lengths per call, whatever batch size the
    /// script declares (shader-reviewer, 569cf551).
    pub batches: u32,
    pub join_steps: u64,
    /// Times the floating-point environment had changed at a guest entry and was restored (5.2b; N11). Local,
    /// not world state.
    pub fp_resets: u32,
}

impl StageReport {
    /// Append what one applied call did.
    fn absorb(&mut self, part: StageReport) {
        self.written.extend(part.written);
        self.created.extend(part.created);
        self.destroyed.extend(part.destroyed);
        self.orphaned.extend(part.orphaned);
        self.refused.extend(part.refused);
        self.events.extend(part.events);
    }
}

/// Why a destroy did not happen.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refused {
    /// The world's guard said no for this object of the subtree: held, or locked.
    Guarded(ObjectId),
    /// An object to keep could not be made a root in place (the store's transform limits).
    Orphan(ObjectId, String),
    /// The world's own destroy refused it.
    World(String),
}

/// The world's own destroy of an object and its subtree.
pub type WorldDestroy = dyn FnMut(&mut SimStore, ObjectId) -> Result<(), pe_core::Error>;

/// What the world gives one stage besides the store. Only std, pe-core and pe-sim types, so the world can drive the
/// runner through a closure without depending on pe-logic.
pub struct StageCtx<'a> {
    pub line: LineId,
    pub rate_hz: u32,
    pub stage: Stage,
    /// Scripts marked faulted in the world's state (3.6): they do not run.
    pub faulted: &'a BTreeSet<ScriptKey>,
    /// True for an object a script may not destroy now: held, or locked (3.3). None: no object is.
    pub guard: Option<&'a dyn Fn(ObjectId) -> bool>,
    /// The world's destroy (tombstones and the like). None: `SimStore::destroy`. **Its effects must live in the
    /// store:** the runner undoes a refused destroy, and every command of a call that faults, by restoring the store
    /// (3.3, 3.6), and cannot undo anything kept elsewhere.
    pub destroy: Option<&'a mut WorldDestroy>,
}

impl<'a> StageCtx<'a> {
    pub fn new(line: LineId, rate_hz: u32, stage: Stage, faulted: &'a BTreeSet<ScriptKey>) -> StageCtx<'a> {
        StageCtx { line, rate_hz, stage, faulted, guard: None, destroy: None }
    }
}

/// The host functions over the store as it was at the start of a system call (3.4).
struct StoreServices {
    store: SimStore,
    reads: Vec<(ComponentId, usize)>,
    seed: u64,
    frame: u64,
    /// The script's key and the system's index, folded.
    system_key: u64,
}

fn object_of(bytes: [u8; 16]) -> ObjectId {
    // The store's layout: `hi` then `lo`, each little-endian (pe-core `ObjectId` is repr(C) of two u64).
    let hi = u64::from_le_bytes(bytes[..8].try_into().unwrap());
    let lo = u64::from_le_bytes(bytes[8..].try_into().unwrap());
    ObjectId::from_u128(((hi as u128) << 64) | lo as u128)
}

fn bytes_of(id: ObjectId) -> [u8; 16] {
    let v = id.to_u128();
    let mut b = [0u8; 16];
    b[..8].copy_from_slice(&((v >> 64) as u64).to_le_bytes());
    b[8..].copy_from_slice(&(v as u64).to_le_bytes());
    b
}

impl Services for StoreServices {
    fn get(&mut self, component: u32, object: [u8; 16], dst: &mut [u8]) -> bool {
        let Some(&(c, size)) = self.reads.get(component as usize) else { return false };
        match self.store.get_raw(c, object_of(object)) {
            Some(row) if dst.len() >= size => {
                dst[..size].copy_from_slice(row);
                true
            }
            _ => false,
        }
    }

    fn query_near(&mut self, _: u32, _: f32, _: &mut [u8], _: u32) -> u32 {
        // Needs positions (Transform), which v0 scripts cannot bind yet: none found. When it is written, it must
        // check `cap * record size <= dst.len()` first, as `get` checks the row size (shader-reviewer).
        0
    }

    fn rand(&mut self, object: [u8; 16], stream: u32) -> u64 {
        let o = object_of(object).to_u128();
        let k = mix64(mix64(mix64(self.system_key ^ (o >> 64) as u64) ^ o as u64) ^ stream as u64);
        Rng::for_frame(self.seed, k, self.frame).next_u64()
    }
}

fn fold_key(key: &ScriptKey, system: u32) -> u64 {
    let mut k = mix64(system as u64);
    for w in key.0.chunks(8) {
        k = mix64(k ^ u64::from_le_bytes(w.try_into().unwrap()));
    }
    k
}

/// A command, parsed and checked against the system's declaration and the store at the start of the call.
enum Cmd {
    Create { spawner: ObjectId, component: ComponentId, row: Vec<u8> },
    Destroy(ObjectId),
    Emit { index: u32, object: ObjectId, bytes: Vec<u8> },
}

/// A world's scripts.
pub struct Logic {
    scripts: Vec<Loaded>,
    settings: RunSettings,
    spawn_mark: ComponentId,
    frame: Option<Frame>,
    /// Objects created per spawner this frame (pe-core `ObjectId::spawned`'s `n`).
    spawns: BTreeMap<ObjectId, u32>,
    /// Scripts that faulted this frame: they run no more systems until the next frame's check of `faulted`.
    faulted_now: BTreeSet<ScriptKey>,
}

impl Logic {
    /// A world's logic. `spawn_mark` is the id under which `registry` holds [`spawn_mark_desc`].
    pub fn new(settings: RunSettings, spawn_mark: ComponentId, registry: &Registry) -> Result<Logic, BindError> {
        if registry.get(spawn_mark) != Some(&spawn_mark_desc(spawn_mark)) {
            return Err(BindError("the spawn mark is not registered".into()));
        }
        Ok(Logic {
            scripts: Vec::new(),
            settings,
            spawn_mark,
            frame: None,
            spawns: BTreeMap::new(),
            faulted_now: BTreeSet::new(),
        })
    }

    /// Add a bound script. Scripts run in key order; a key already present is replaced.
    pub fn insert(&mut self, s: Loaded) {
        match self.scripts.binary_search_by_key(&s.key, |l| l.key) {
            Ok(i) => self.scripts[i] = s,
            Err(i) => self.scripts.insert(i, s),
        }
    }

    pub fn remove(&mut self, key: ScriptKey) -> Option<Loaded> {
        let i = self.scripts.binary_search_by_key(&key, |l| l.key).ok()?;
        Some(self.scripts.remove(i))
    }

    pub fn scripts(&self) -> impl Iterator<Item = &Loaded> {
        self.scripts.iter()
    }

    /// Run one logic stage of the store's current frame. `pre_physics` starts the frame: every instance is reset
    /// and the created-id counts restart. Scripts in `faulted` (world state the caller keeps, 3.6) do not run.
    /// The thread must have [`crate::Settings::thread_stack`] of native stack.
    pub fn run_stage(&mut self, store: &mut SimStore, ctx: &mut StageCtx<'_>) -> StageReport {
        let (stage, rate_hz) = (ctx.stage, ctx.rate_hz);
        let frame = store.frame();
        if stage == Stage::PrePhysics || self.frame != Some(frame) {
            for s in &mut self.scripts {
                s.reset();
            }
            self.spawns.clear();
            self.faulted_now.clear();
            self.frame = Some(frame);
        }
        let mut report = StageReport::default();
        let settings = self.settings;
        for si in 0..self.scripts.len() {
            let script = &mut self.scripts[si];
            // The area's columns are trusted only within a stage: between stages the engine's own systems run.
            script.resident.clear();
            script.resident_ids.clear();
            for pi in 0..script.plans.len() {
                let key = script.key;
                if ctx.faulted.contains(&key) || self.faulted_now.contains(&key) {
                    break;
                }
                let plan = script.plans[pi].clone();
                if plan.stage != stage {
                    continue;
                }
                let outcome = call_system(script, &plan, store, rate_hz, self.spawn_mark, settings, &mut report);
                let fault = match outcome {
                    Ok((rows, cmds)) if rows.is_empty() && cmds.is_empty() => None,
                    Ok((rows, cmds)) => {
                        // A fault while applying discards the whole call too (3.6): the store and the spawn counts go
                        // back to the copy-on-write snapshot, and the call reports nothing it did.
                        let snapshot = store.clone();
                        let creates = cmds.iter().any(|c| matches!(c, Cmd::Create { .. }));
                        let spawns = creates.then(|| self.spawns.clone());
                        let mut part = StageReport::default();
                        let mut a = Apply { spawns: &mut self.spawns, spawn_mark: self.spawn_mark, report: &mut part };
                        match a.apply(key, &plan, store, ctx, rows, cmds) {
                            Ok(()) => {
                                report.absorb(part);
                                None
                            }
                            Err(f) => {
                                *store = snapshot;
                                if let Some(s) = spawns {
                                    self.spawns = s;
                                }
                                Some(f)
                            }
                        }
                    }
                    Err(f) => Some(f),
                };
                if let Some(fault) = fault {
                    script.resident.clear();
                    script.resident_ids.clear();
                    report.faults.push(FaultReport { script: key, system: plan.name.clone(), frame, fault });
                    self.faulted_now.insert(key);
                }
            }
        }
        report
    }
}

type Rows = Vec<(ComponentId, ObjectId, Vec<u8>)>;

/// Run every batch of one system call. Returns the changed rows, validated, and the commands, checked; nothing is
/// applied yet.
fn call_system(
    script: &mut Loaded,
    plan: &Plan,
    store: &SimStore,
    rate_hz: u32,
    spawn_mark: ComponentId,
    settings: RunSettings,
    report: &mut StageReport,
) -> Result<(Rows, Vec<Cmd>), Fault> {
    let objects = bound_objects(store, &plan.bound, &mut report.join_steps);
    if objects.is_empty() {
        return Ok((Vec::new(), Vec::new()));
    }
    // Each bound object's row index in each bound column, by one merge walk per column per call; each batch takes its
    // slice (the store does not change during the call). A walk per batch from row 0 would grow with the square of
    // the objects, at a batch size the script chooses.
    let indices: Vec<Vec<usize>> =
        plan.bound.iter().map(|b| row_indices(store.column_raw(b.id).0, &objects, &mut report.join_steps)).collect();
    let registry = store.registry().clone();
    let base = script.manifest.batch_addr as usize;
    let frame = store.frame();
    let system_key = fold_key(&script.key, plan.index);
    let services = || StoreServices {
        store: store.clone(),
        reads: plan.reads_other.clone(),
        seed: store.seed(),
        frame: frame.0,
        system_key,
    };
    let mut rows: Rows = Vec::new();
    let mut cmds = Vec::new();
    for (batch_no, chunk) in objects.chunks(plan.rows).enumerate() {
        // Copy in: the header, the ids, then each column unless the area holds it unchanged.
        let mut header = BatchHeader {
            frame: frame.0,
            seed: store.seed(),
            rate_hz,
            n: chunk.len() as u32,
            ids: plan.ids_at,
            cmd: plan.cmd_at,
            cmd_cap: plan.cmd_cap,
            cmd_len: 0,
            ..BatchHeader::default()
        };
        header.columns[..plan.columns.len()].copy_from_slice(&plan.columns);
        let same_ids = settings.skip_unchanged && script.resident_ids == chunk;
        let mem = script.inst.memory_mut();
        let area = &mut mem[base..base + script.manifest.batch_cap as usize];
        area[..HEADER_BYTES].copy_from_slice(&header.to_bytes());
        for (k, &id) in chunk.iter().enumerate() {
            let at = plan.ids_at as usize + 16 * k;
            area[at..at + 16].copy_from_slice(&bytes_of(id));
        }
        // Where this batch's rows sit in each bound column: batches are consecutive slices of `objects`.
        let lo = batch_no * plan.rows;
        let places: Vec<Place> = indices.iter().map(|ix| place(&ix[lo..lo + chunk.len()])).collect();
        report.batches += 1;
        for ((b, &at), pl) in plan.bound.iter().zip(&plan.columns).zip(&places) {
            let bytes = b.size * chunk.len();
            let held = Resident { at, id: b.id, bytes };
            if same_ids && script.resident.contains(&held) {
                report.skipped_in += bytes as u64;
                continue;
            }
            let col = store.column_raw(b.id).1;
            let at = at as usize;
            match pl {
                // The chunk's rows are one run of the column: one copy.
                Place::Run(first) => area[at..at + bytes].copy_from_slice(&col[first * b.size..][..bytes]),
                Place::Rows(ix) => {
                    for (k, &i) in ix.iter().enumerate() {
                        area[at + b.size * k..][..b.size].copy_from_slice(&col[i * b.size..][..b.size]);
                    }
                }
            }
            report.copied_in += bytes as u64;
        }
        script.resident.clear();
        script.resident_ids.clear();

        if !pe_core::fpenv::check_and_reset() {
            report.fp_resets += 1;
        }
        script.inst.set_services(Box::new(services()));
        let ran = script.inst.run_system(plan.index, batch_no as u32, settings.fuel_per_call);
        script.inst.set_services(Box::new(NoServices));
        report.calls += 1;
        report.fuel += match ran {
            Ok(used) => used,
            Err(Fault::Fuel { budget }) => budget,
            Err(_) => 0,
        };
        ran?;

        // Read back: the changed rows of the write columns, each validated, and the commands.
        let area = &script.inst.memory()[base..base + script.manifest.batch_cap as usize];
        for ((b, &at), pl) in plan.bound.iter().zip(&plan.columns).zip(&places) {
            if b.access != Access::Write {
                continue;
            }
            let desc = registry.get(b.id).expect("bound at bind");
            let col = store.column_raw(b.id).1;
            let (at, n) = (at as usize, chunk.len());
            let old_row = |k: usize| -> &[u8] {
                let i = match pl {
                    Place::Run(first) => first + k,
                    Place::Rows(ix) => ix[k],
                };
                &col[i * b.size..][..b.size]
            };
            // Rows whose bytes may have changed: with a run, whole 256-byte blocks that compare equal are skipped
            // (3.3a: one memcmp per unchanged block); otherwise every row is compared.
            let mut changed = Vec::new();
            match pl {
                Place::Run(first) => {
                    let (new, old) = (&area[at..at + n * b.size], &col[first * b.size..][..n * b.size]);
                    let mut k = 0;
                    for start in (0..new.len()).step_by(WRITE_BLOCK) {
                        let end = (start + WRITE_BLOCK).min(new.len());
                        if new[start..end] == old[start..end] {
                            continue;
                        }
                        k = k.max(start / b.size);
                        while k < n && k * b.size < end {
                            changed.push(k);
                            k += 1;
                        }
                    }
                }
                Place::Rows(_) => changed.extend(0..n),
            }
            for k in changed {
                let new = &area[at + b.size * k..][..b.size];
                if new == old_row(k) {
                    continue;
                }
                if let Some(field) = desc.invalid_field(new) {
                    return Err(Fault::InvalidRow { component: desc.name().into(), field: field.into() });
                }
                rows.push((b.id, chunk[k], new.to_vec()));
            }
        }
        let len = u32::from_le_bytes(area[CMD_LEN_AT..CMD_LEN_AT + 4].try_into().unwrap());
        if len > plan.cmd_cap || !len.is_multiple_of(8) {
            return Err(Fault::BadCommand(format!("command length {len} of {}", plan.cmd_cap)));
        }
        let buf = &area[plan.cmd_at as usize..][..len as usize];
        parse_commands(script, plan, store, spawn_mark, chunk, buf, &mut cmds)?;

        // The write columns, compared row by row above, now equal the store for these ids once the call's rows are
        // written (all or none of them). The read columns are not trusted: nothing stops a script from writing
        // there in safe code (shader-reviewer, logic.md 3.3a (c)), so they are copied in again.
        script.resident_ids = chunk.to_vec();
        script.resident = plan
            .bound
            .iter()
            .zip(&plan.columns)
            .filter(|(b, _)| b.access == Access::Write)
            .map(|(b, &at)| Resident { at, id: b.id, bytes: b.size * chunk.len() })
            .collect();
    }
    Ok((rows, cmds))
}

/// The write-back's comparison block (logic.md 3.3a, measured: 256 B blocks against 0.62-0.68 ms by row).
const WRITE_BLOCK: usize = 256;

/// Where a batch's objects sit in one column: one run starting at a row index, or each row's index.
enum Place<'a> {
    Run(usize),
    Rows(&'a [usize]),
}

/// A batch's row indices in one column (strictly increasing): one run if they are contiguous.
fn place(ix: &[usize]) -> Place<'_> {
    match ix.first() {
        Some(&first) if ix.last() == Some(&(first + ix.len() - 1)) => Place::Run(first),
        _ => Place::Rows(ix),
    }
}

/// The row index of each of `objects` (sorted, every one with a row here) in a column whose sorted ids are `ids`: one
/// merge walk, its steps added to `steps`. The check is one compare per row, kept in release: a broken invariant
/// would otherwise copy another object's row.
fn row_indices(ids: &[ObjectId], objects: &[ObjectId], steps: &mut u64) -> Vec<usize> {
    let mut ix = Vec::with_capacity(objects.len());
    let mut i = 0;
    for &id in objects {
        while ids[i] < id {
            i += 1;
        }
        assert_eq!(ids[i], id, "bound objects have every bound row");
        ix.push(i);
        i += 1;
    }
    *steps += i as u64;
    ix
}

/// The store's objects with every bound component, in id order: the smallest bound column's ids, filtered by a merge
/// walk through each other bound column (N12).
fn bound_objects(store: &SimStore, bound: &[Bound], steps: &mut u64) -> Vec<ObjectId> {
    let Some(first) = bound.iter().min_by_key(|b| store.count(b.id)) else { return Vec::new() };
    let mut out: Vec<ObjectId> = store.column_raw(first.id).0.to_vec();
    *steps += out.len() as u64;
    for b in bound {
        if b.id == first.id || out.is_empty() {
            continue;
        }
        let ids = store.column_raw(b.id).0;
        let mut j = 0;
        out.retain(|&id| {
            while j < ids.len() && ids[j] < id {
                j += 1;
            }
            j < ids.len() && ids[j] == id
        });
        *steps += j as u64;
    }
    out
}

fn spawned_by(store: &SimStore, mark: ComponentId, id: ObjectId) -> Option<ScriptKey> {
    store.get_raw(mark, id).map(|r| Hash(r.try_into().expect("32 bytes")))
}

fn parse_commands(
    script: &Loaded,
    plan: &Plan,
    store: &SimStore,
    spawn_mark: ComponentId,
    chunk: &[ObjectId],
    mut buf: &[u8],
    out: &mut Vec<Cmd>,
) -> Result<(), Fault> {
    let bad = |m: String| Err(Fault::BadCommand(m));
    let u32_at = |b: &[u8], at: usize| b.get(at..at + 4).map(|w| u32::from_le_bytes(w.try_into().unwrap()));
    while !buf.is_empty() {
        let (Some(kind), Some(len)) = (u32_at(buf, 0), u32_at(buf, 4)) else { return bad("truncated record".into()) };
        let rec = (COMMAND_HEADER + len as usize).div_ceil(8) * 8;
        if rec > buf.len() {
            return bad("a record runs past the buffer".into());
        }
        let p = &buf[COMMAND_HEADER..COMMAND_HEADER + len as usize];
        match kind {
            k if k == Command::Create as u32 => {
                if !plan.creates {
                    return bad(format!("system {} does not declare creates", plan.name));
                }
                let (Some(row_index), Some(name_len)) = (u32_at(p, 0), u32_at(p, 4)) else {
                    return bad("create: truncated".into());
                };
                let Some(&spawner) = chunk.get(row_index as usize) else { return bad("create: no such row".into()) };
                let name = p.get(8..8 + name_len as usize).ok_or(Fault::BadCommand("create: name".into()))?;
                let k = script.manifest.components.iter().position(|c| c.name.as_bytes() == name);
                let Some(k) = k else { return bad("create: not a component the script declares".into()) };
                let component = script.components[k];
                let desc = store.registry().get(component).expect("bound at bind");
                let row = &p[8 + name_len as usize..];
                if row.len() != desc.size() {
                    return bad(format!("create: {} bytes for {}", row.len(), desc.name()));
                }
                if let Some(field) = desc.invalid_field(row) {
                    return Err(Fault::InvalidRow { component: desc.name().into(), field: field.into() });
                }
                out.push(Cmd::Create { spawner, component, row: row.to_vec() });
            }
            k if k == Command::Destroy as u32 => {
                if !plan.creates {
                    return bad(format!("system {} does not declare creates", plan.name));
                }
                let Ok(id) = <[u8; 16]>::try_from(p) else { return bad("destroy: payload".into()) };
                let id = object_of(id);
                // A script destroys only what it spawned (3.3; coordinator-opus mfe83e04db541). An object already
                // gone is left to the apply step, where it does nothing.
                if store.exists(id) && spawned_by(store, spawn_mark, id) != Some(script.key) {
                    return bad("destroy: the script did not spawn the object".into());
                }
                out.push(Cmd::Destroy(id));
            }
            k if k == Command::Emit as u32 => {
                let (Some(row_index), Some(index)) = (u32_at(p, 0), u32_at(p, 4)) else {
                    return bad("emit: truncated".into());
                };
                let Some(&object) = chunk.get(row_index as usize) else { return bad("emit: no such row".into()) };
                if index as usize >= plan.emits {
                    return bad(format!("emit: event {index} is not declared"));
                }
                out.push(Cmd::Emit { index, object, bytes: p[8..].to_vec() });
            }
            k => return bad(format!("unknown command kind {k}")),
        }
        buf = &buf[rec..];
    }
    Ok(())
}

/// Applying one call's rows and commands.
struct Apply<'r> {
    spawns: &'r mut BTreeMap<ObjectId, u32>,
    spawn_mark: ComponentId,
    report: &'r mut StageReport,
}

impl Apply<'_> {
    /// Write a call's rows, then apply its commands in order.
    fn apply(
        &mut self,
        key: ScriptKey,
        plan: &Plan,
        store: &mut SimStore,
        ctx: &mut StageCtx<'_>,
        rows: Rows,
        cmds: Vec<Cmd>,
    ) -> Result<(), Fault> {
        let frame = store.frame();
        for (c, id, row) in rows {
            store.insert_raw(c, id, &row).map_err(|e| Fault::Other(format!("write-back: {e}")))?;
            self.report.written.push((c, id));
        }
        for cmd in cmds {
            match cmd {
                Cmd::Create { spawner, component, row } => {
                    let n = self.spawns.entry(spawner).or_insert(0);
                    let id = ObjectId::spawned(ctx.line, spawner, frame, *n);
                    *n += 1;
                    let other = |e: pe_core::Error| Fault::Other(format!("create: {e}"));
                    store.create(id).map_err(other)?;
                    store.insert_raw(self.spawn_mark, id, &key.0).map_err(other)?;
                    store.insert_raw(component, id, &row).map_err(other)?;
                    self.report.created.push((id, spawner));
                }
                Cmd::Destroy(id) => self.destroy(key, store, ctx, id),
                Cmd::Emit { index, object, bytes } => {
                    let e = Event { script: key, system: plan.name.clone(), index, object, bytes };
                    self.report.events.push(e);
                }
            }
        }
        Ok(())
    }

    /// Destroy what the script spawned at and beneath `id` (3.3). Refused whole if the guard holds any object of the
    /// subtree. Objects beneath it that the script did not spawn are not taken: each topmost one becomes a root in
    /// place (the orphan rule, world-model.md 7.5), with what is beneath it.
    fn destroy(&mut self, key: ScriptKey, store: &mut SimStore, ctx: &mut StageCtx<'_>, id: ObjectId) {
        // Already gone (an earlier command this frame) is not an error: the same on every machine.
        if !store.exists(id) {
            return;
        }
        let subtree = store.subtree(id);
        if let Some(guard) = ctx.guard
            && let Some(&g) = subtree.iter().find(|&&x| guard(x))
        {
            self.report.refused.push((id, Refused::Guarded(g)));
            return;
        }
        let foreign: BTreeSet<ObjectId> =
            subtree.iter().copied().filter(|&x| spawned_by(store, self.spawn_mark, x) != Some(key)).collect();
        // The orphaning and the destroy are one step: a refusal of either undoes both (design-reviewer m6ff8571e295a).
        let snapshot = store.clone();
        let mut orphaned = Vec::new();
        let mut refusal = None;
        for &x in &subtree {
            let top = foreign.contains(&x) && store.parent(x).is_none_or(|p| !foreign.contains(&p));
            if top {
                if let Err(e) = store.set_parent_keep_world(x, None, None) {
                    refusal = Some(Refused::Orphan(x, e.to_string()));
                    break;
                }
                orphaned.push(x);
            }
        }
        if refusal.is_none() {
            let done = match ctx.destroy.as_mut() {
                Some(d) => d(store, id),
                None => store.destroy(id),
            };
            refusal = done.err().map(|e| Refused::World(e.to_string()));
        }
        match refusal {
            None => {
                self.report.orphaned.extend(orphaned);
                self.report.destroyed.push(id);
            }
            Some(r) => {
                *store = snapshot;
                self.report.refused.push((id, r));
            }
        }
    }
}
