//! pe-logic's wasmtime runner as a [`ScriptRunner`]: the `logic` feature (@coordinator-opus).
//!
//! - **Modules come ready** (increment 1): [`LogicRunner::insert`] takes a module as the build service gives it, and
//!   pe-logic validates, meters and compiles it. The runner keys it by the hash of its metered bytes, which a row's
//!   `author_module` names (logic.md 6.2 step 3).
//! - **Modules loaded on another thread** (logic.md 6.2 steps 3-4: a member builds a pending version and holds its
//!   module before it attests): a [`ModuleInbox`], taken from the runner before the replica owns it, loads a module on
//!   the thread that calls it, so the sim thread never compiles a built module: loading the 18 KB tally takes about
//!   5 ms and a 264 KB module 50-84 ms [V logic.md 4.2, 5.2a], against a 13.9 ms frame at 72 Hz. The runner takes
//!   what has arrived at the start of a frame, its `LogicPrePhysics` stage, so both stages of a frame see the same
//!   modules.
//! - **Binding** (logic.md 3.2, 6.2; world-model.md 7.9): a row binds its module under the ids its version V gives
//!   (`pe_world::script::script_component_id` over each of the manifest's own components, the tag `tag_desc(V)`),
//!   every one of which must be among the row's ids, and under the engine components of 3.2's table. A script stays
//!   bound while its row names the same module, version and ids. A row whose module would not bind for a reason of the
//!   module's own is not tried again until the runner is given another module.
//! - **The thread** that steps a replica with this runner needs [`LogicRunner::thread_stack`] of native stack:
//!   wasmtime checks its own stack limit against the stack it entered on (pe-logic `Settings::thread_stack`).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, MutexGuard};

use bytes::Bytes;
use pe_core::Hash;
use pe_logic::manifest::{ENGINE_PREFIX, Manifest};
use pe_logic::run::{self, EngineComponent, Loaded, Logic, RunSettings, StageCtx};
use pe_logic::runtime::{self, LoadError, Script, Settings};
use pe_sdk::layout::Stage as SdkStage;
use pe_seams::Stage;
use pe_sim::{ComponentId, Registry, SimStore};
use pe_world::script::{AssetId, ENGINE_COMPONENTS, script_component_id, tag_desc};

use super::{ScriptEvent, ScriptRow, ScriptRunner, StageOutcome, StageWorld};

/// A module the runner holds.
struct Held {
    /// The bytes as the build service gave them: each binding loads them again, since a bound script owns its
    /// compiled module.
    wasm: Vec<u8>,
    metered: Bytes,
    manifest: Manifest,
    /// The module loaded at [`LogicRunner::insert`] or [`ModuleInbox::load`], for its first binding.
    spare: Option<Script>,
}

impl Held {
    /// Loads `wasm` as the build service gives it: validated, metered and compiled, and its manifest read, which runs
    /// the module (so the calling thread needs `Settings::thread_stack` of native stack). With its hash.
    fn load(engine: &wasmtime::Engine, settings: Settings, wasm: &[u8]) -> Result<(Hash, Held), LoadError> {
        let script = Script::load(engine, settings, wasm)?;
        let manifest = run::read_manifest(&script).map_err(|e| LoadError::Compile(e.0))?;
        let hash = script.module_hash();
        let metered = Bytes::from(script.metered.clone());
        Ok((hash, Held { wasm: wasm.to_vec(), metered, manifest, spare: Some(script) }))
    }
}

/// Modules loaded through a [`ModuleInbox`], in their order of arrival, until the runner takes them.
type Arrived = Arc<Mutex<Vec<(Hash, Held)>>>;

fn lock(arrived: &Arrived) -> MutexGuard<'_, Vec<(Hash, Held)>> {
    // A thread that panicked while holding the lock left the list whole: every change to it is one push or one take.
    arrived.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Loads modules for a [`LogicRunner`] on another thread, such as pe-app's build thread (module doc). Clone it for
/// each thread that loads; every clone feeds the same runner.
#[derive(Clone)]
pub struct ModuleInbox {
    engine: wasmtime::Engine,
    settings: Settings,
    arrived: Arrived,
}

impl ModuleInbox {
    /// Loads a module as [`LogicRunner::insert`] does, but on the calling thread, which needs
    /// [`ModuleInbox::thread_stack`] of native stack. The runner takes it at the start of its next frame. Returns the
    /// hash of its metered bytes, the `author_module` a row names it by. A module that won't load leaves nothing.
    pub fn load(&self, wasm: &[u8]) -> Result<Hash, LoadError> {
        let (hash, held) = Held::load(&self.engine, self.settings, wasm)?;
        lock(&self.arrived).push((hash, held));
        Ok(hash)
    }

    /// The native stack a thread that calls [`ModuleInbox::load`] needs: reading a module's manifest runs it.
    pub fn thread_stack(&self) -> usize {
        self.settings.thread_stack()
    }
}

/// What a row binds.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Binding {
    module: Hash,
    version: Hash,
    components: Vec<ComponentId>,
}

impl Binding {
    fn of(row: &ScriptRow) -> Binding {
        Binding { module: row.module, version: row.version, components: row.components.clone() }
    }
}

/// pe-logic's runner (module doc).
pub struct LogicRunner {
    engine: wasmtime::Engine,
    settings: Settings,
    run: RunSettings,
    held: BTreeMap<Hash, Held>,
    /// What its [`ModuleInbox`]es have loaded since the last frame's start.
    arrived: Arrived,
    logic: Logic,
    /// The binding of each script in `logic`.
    bound: BTreeMap<AssetId, Binding>,
    /// Rows whose module would not bind for a reason of its own, not tried again until another module comes.
    refused: BTreeMap<AssetId, Binding>,
}

impl LogicRunner {
    /// A runner with these settings, which are part of its identity.
    pub fn new(settings: Settings, run: RunSettings) -> Result<LogicRunner, String> {
        let engine = runtime::engine(&settings).map_err(|e| e.to_string())?;
        Ok(LogicRunner {
            engine,
            settings,
            run,
            held: BTreeMap::new(),
            arrived: Arrived::default(),
            logic: Logic::new(run),
            bound: BTreeMap::new(),
            refused: BTreeMap::new(),
        })
    }

    /// A handle that loads modules for this runner on another thread (module doc). Take it before the replica owns the
    /// runner.
    pub fn inbox(&self) -> ModuleInbox {
        ModuleInbox { engine: self.engine.clone(), settings: self.settings, arrived: Arc::clone(&self.arrived) }
    }

    /// A runner with pe-logic's default settings (`Settings::default()`, `RunSettings::default()`).
    pub fn with_defaults() -> Result<LogicRunner, String> {
        LogicRunner::new(Settings::default(), RunSettings::default())
    }

    /// The native stack a thread that steps a replica with this runner needs (module doc).
    pub fn thread_stack(&self) -> usize {
        self.settings.thread_stack()
    }

    /// Takes a module as the build service gives it: validated, metered and compiled. Returns the hash of its metered
    /// bytes, the `author_module` a row names it by.
    pub fn insert(&mut self, wasm: &[u8]) -> Result<Hash, LoadError> {
        let (hash, held) = Held::load(&self.engine, self.settings, wasm)?;
        self.hold(hash, held);
        Ok(hash)
    }

    fn hold(&mut self, hash: Hash, held: Held) {
        self.held.insert(hash, held);
        self.refused.clear();
    }

    /// Holds what the inboxes have loaded since the last take.
    fn take_arrived(&mut self) {
        let arrived = std::mem::take(&mut *lock(&self.arrived));
        for (hash, held) in arrived {
            self.hold(hash, held);
        }
    }

    /// `row`'s module bound under its ids, or why not. `Err((reason, of_the_module))`: `of_the_module` is true when
    /// the reason is the module's own, so the same module, version and ids would fail again whatever the world holds.
    fn bind(&mut self, row: &ScriptRow, registry: &Registry) -> Result<Loaded, (String, bool)> {
        let held = self.held.get_mut(&row.module).ok_or_else(|| ("the module is not held".to_string(), false))?;
        let tag = tag_desc(&row.version).id();
        let mut own = Vec::new();
        for c in held.manifest.components.iter().filter(|c| !c.name.starts_with(ENGINE_PREFIX)) {
            let fields = run::component_desc(c, ComponentId(0)).map_err(|e| (e.to_string(), true))?.fields().to_vec();
            let id = script_component_id(&row.version, &c.name, fields)
                .ok_or_else(|| (format!("component {}: no id", c.name), true))?;
            // The world's own check of what bind checks too: until the world registers the version, it doesn't bind.
            let want = run::component_desc(c, id).map_err(|e| (e.to_string(), true))?;
            if registry.get(id) != Some(&want) {
                return Err((format!("component {} is not registered under this version", c.name), false));
            }
            own.push(id);
        }
        if registry.get(tag).is_none() {
            return Err(("the version's tag is not registered".to_string(), false));
        }
        if let Some(id) = own.iter().chain([&tag]).find(|id| row.components.binary_search(id).is_err()) {
            return Err((format!("id {:?} is not among the row's", id), false));
        }
        let script = match held.spare.take() {
            Some(s) => s,
            None => Script::load(&self.engine, self.settings, &held.wasm).map_err(|e| (e.to_string(), true))?,
        };
        debug_assert_eq!(script.module_hash(), row.module, "a held module is keyed by its metered bytes' hash");
        let engine: Vec<EngineComponent<'_>> = ENGINE_COMPONENTS
            .iter()
            .map(|e| EngineComponent { name: e.name, id: e.id, scripts_may_write: e.scripts_may_write })
            .collect();
        Loaded::bind(Hash(row.asset), script, held.manifest.clone(), &own, tag, &engine, registry)
            .map_err(|e| (e.0, true))
    }

    /// Makes the bound scripts exactly `scripts`' rows that bind; returns the rows that don't.
    fn rebind(&mut self, scripts: &[ScriptRow], registry: &Registry) -> Vec<AssetId> {
        let gone: Vec<AssetId> =
            self.bound.keys().filter(|a| !scripts.iter().any(|r| r.asset == **a)).copied().collect();
        for a in gone {
            self.bound.remove(&a);
            self.logic.remove(Hash(a));
        }
        let mut missing = Vec::new();
        for row in scripts {
            let want = Binding::of(row);
            if self.bound.get(&row.asset) == Some(&want) {
                continue;
            }
            if self.bound.remove(&row.asset).is_some() {
                self.logic.remove(Hash(row.asset));
            }
            if self.refused.get(&row.asset) == Some(&want) {
                missing.push(row.asset);
                continue;
            }
            match self.bind(row, registry) {
                Ok(loaded) => {
                    self.logic.insert(loaded);
                    self.bound.insert(row.asset, want);
                }
                Err((_, of_the_module)) => {
                    if of_the_module {
                        self.refused.insert(row.asset, want);
                    }
                    missing.push(row.asset);
                }
            }
        }
        missing
    }

    /// The declared name of event `index` of `system` in the bound script `asset`.
    fn event_name(&self, asset: Hash, system: &str, index: u32) -> Option<String> {
        let s = self.logic.scripts().find(|l| l.key() == asset)?;
        let sys = s.manifest().systems.iter().find(|s| s.name == system)?;
        sys.emits.get(index as usize).map(|(name, _)| name.clone())
    }
}

impl ScriptRunner for LogicRunner {
    /// pe-logic's bytes: its result version, wasmtime's, the pass's and the ABI's, and every setting that can change a
    /// result (S2's `RunnerIdentity`: pe-logic produces them).
    fn identity(&self) -> Bytes {
        Bytes::from(pe_logic::identity(&self.settings, &self.run))
    }

    /// A module an inbox loads counts from the start of the runner's next frame.
    fn holds(&self, module: &Hash) -> bool {
        self.held.contains_key(module)
    }

    fn module(&self, module: &Hash) -> Option<Bytes> {
        self.held.get(module).map(|h| h.metered.clone())
    }

    fn run_stage(
        &mut self,
        stage: Stage,
        scripts: &[ScriptRow],
        store: &mut SimStore,
        world: &StageWorld<'_>,
    ) -> StageOutcome {
        let sdk_stage = match stage {
            Stage::LogicPrePhysics => {
                self.take_arrived();
                SdkStage::PrePhysics
            }
            Stage::LogicPostPhysics => SdkStage::PostPhysics,
            _ => return StageOutcome::default(),
        };
        let registry = store.registry().clone();
        let missing: Vec<AssetId> =
            self.rebind(scripts, &registry).into_iter().filter(|a| !world.faulted.contains(a)).collect();
        let faulted: BTreeSet<Hash> = world.faulted.iter().map(|a| Hash(*a)).collect();
        let mut ctx = StageCtx::new(world.line, world.rate_hz, sdk_stage, &faulted);
        ctx.guard = Some(world.guard);
        ctx.validate = Some(world.check);
        let r = self.logic.run_stage(store, &mut ctx);
        let mut out = StageOutcome { missing, fp_resets: r.fp_resets, ..StageOutcome::default() };
        for f in &r.faults {
            match f.fault.code() {
                Some(code) => out.report.faults.push((f.script.0, code)),
                None => out.host_faults += 1,
            }
        }
        out.report.created = r.created;
        out.report.removed = r.removed;
        out.report.orphaned = r.orphaned;
        out.report.written = r.written;
        for e in r.events {
            let name = self.event_name(e.script, &e.system, e.index);
            debug_assert!(name.is_some(), "an event names a system and an index its script declares");
            out.events.push(ScriptEvent {
                asset: e.script.0,
                system: e.system,
                name: name.unwrap_or_default(),
                object: e.object,
                bytes: e.bytes,
            });
        }
        out
    }
}
