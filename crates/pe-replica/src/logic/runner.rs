//! pe-logic's wasmtime runner as a [`ScriptRunner`]: the `logic` feature (@coordinator-opus mba546f07b57f).
//!
//! - **Modules come ready** (increment 1): [`LogicRunner::insert`] takes a module as the build service gives it, and
//!   pe-logic validates, meters and compiles it. The runner keys it by the hash of its metered bytes, which a row's
//!   `author_module` names (logic.md 6.2 step 3).
//! - **Modules loaded on another thread** (logic.md 6.2 steps 3-4: a member builds a pending version and holds its
//!   module before it attests): a [`ModuleInbox`], taken from the runner before the replica owns it, loads a module on
//!   the thread that calls it, so the sim thread never compiles a built module: loading the 18 KB tally takes about
//!   5 ms and a 264 KB module 50-84 ms [V logic.md 4.2, 5.2a], against a 13.9 ms frame at 72 Hz. The runner takes
//!   what has arrived at the start of a frame, its `LogicPrePhysics` stage, so both stages of a frame see the same
//!   modules. When a module arrives is this machine's timing, not the world's: a take can land inside a re-simulation,
//!   so one advance can step some frames without the module and later ones with it. That is harmless only because a
//!   replica that stepped frames at or past an activation F without the module holds its votes from F, and steps
//!   again from F once it holds it (S2 v1.7's `vote_holds`; at n = 2 every member holds the module at F, since it
//!   attests after loading it; @engine-architect's Low 1 on 1dd85502).
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
use pe_sim::{ComponentDesc, ComponentId, Registry, SimStore};
use pe_world::script::{AssetId, ENGINE_COMPONENTS, script_component_id, tag_desc};

use super::{ScriptEvent, ScriptRow, ScriptRunner, StageOutcome, StageWorld};

/// A module the runner holds.
struct Held {
    metered: Bytes,
    manifest: Manifest,
    /// The module as [`LogicRunner::insert`] or [`ModuleInbox::load`] compiled it. Each binding takes a clone, which
    /// shares the compiled code, so a re-binding (a rollback across the script's activation, or its row leaving and
    /// coming back) never compiles on the sim thread. Re-binding the 9.5 KB lamp took a median 12.1 ms when it
    /// loaded the module again and takes 0.15 ms with a clone [V release, 40 re-bindings each, under load]. The cost
    /// is memory: every module held keeps its compiled code, where it kept the wasm before (an M1 session holds a
    /// few). A clone carries the settings the module was loaded with, which are the runner's, fixed at
    /// [`LogicRunner::new`]: a runner whose settings could change would load again rather than clone (@iter-bench,
    /// m752c4d0b714b).
    script: Script,
}

impl Held {
    /// Loads `wasm` as the build service gives it: validated, metered and compiled, and its manifest read, which runs
    /// the module (so the calling thread needs `Settings::thread_stack` of native stack). With its hash.
    fn load(engine: &wasmtime::Engine, settings: Settings, wasm: &[u8]) -> Result<(Hash, Held), LoadError> {
        Held::of(Script::load(engine, settings, wasm)?)
    }

    /// A module pe-logic has loaded, with its manifest read (which runs it) and its hash. A manifest that won't read
    /// is the module's own fault, the same everywhere, so it is refused (the API's `Manifest` kind), never a
    /// `LoadError::Compile`, which pe-logic keeps for this machine's own faults.
    fn of(script: Script) -> Result<(Hash, Held), LoadError> {
        let manifest = run::read_manifest(&script).map_err(|e| refused(format!("its manifest: {}", e.0)))?;
        let hash = script.module_hash();
        let metered = Bytes::from(script.metered.clone());
        Ok((hash, Held { metered, manifest, script }))
    }
}

/// A refusal of the module's own making that the metering pass's kinds don't name: its manifest or its components.
fn refused(message: String) -> LoadError {
    LoadError::Refused(pe_logic::meter::Refusal { kind: pe_logic::meter::RefusalKind::Abi, message })
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

    /// Describes a module as a `CommitScript` of version `v` registers it, without holding it, on the calling thread
    /// (which needs [`ModuleInbox::thread_stack`]): for the build service's checks of a member's build against the
    /// commit (logic.md 6.2 step 3) and for the author's own commit (pe-app's `Builder`). The hash is this runner's,
    /// from the settings it meters with.
    pub fn describe(&self, wasm: &[u8], v: &Hash) -> Result<Described, LoadError> {
        let (module, held) = Held::load(&self.engine, self.settings, wasm)?;
        Described::of(module, &held.manifest, v)
    }

    /// [`ModuleInbox::load`] for a module pe-logic has already loaded with this inbox's engine and settings
    /// ([`ModuleInbox::engine`], [`ModuleInbox::settings`]), as its `build_for_row` gives it: the runner holds this one,
    /// without loading it again. A module loaded with another engine must not come here.
    pub fn load_script(&self, script: Script) -> Result<Hash, LoadError> {
        let (hash, held) = Held::of(script)?;
        lock(&self.arrived).push((hash, held));
        Ok(hash)
    }

    /// [`ModuleInbox::describe`] for a module pe-logic has already loaded with this inbox's engine and settings.
    pub fn describe_script(&self, script: &Script, v: &Hash) -> Result<Described, LoadError> {
        let (module, held) = Held::of(script.clone())?;
        Described::of(module, &held.manifest, v)
    }

    /// The engine this inbox's runner compiles with: a module loaded with it can go to [`ModuleInbox::load_script`].
    pub fn engine(&self) -> &wasmtime::Engine {
        &self.engine
    }

    /// The settings this inbox's runner meters and compiles with, which decide a module's hash.
    pub fn settings(&self) -> Settings {
        self.settings
    }

    /// The native stack a thread that calls [`ModuleInbox::load`] or [`ModuleInbox::describe`] needs: reading a
    /// module's manifest runs it.
    pub fn thread_stack(&self) -> usize {
        self.settings.thread_stack()
    }
}

/// A module as a `CommitScript` of version V registers it (logic.md 3.2, 6.2 step 3; [`ModuleInbox::describe`]).
#[derive(Clone, Debug, PartialEq)]
pub struct Described {
    /// The hash of its metered bytes: the commit's `author_module`.
    pub module: Hash,
    /// Its own components under the ids V gives them (`pe_world::script::script_component_id`), in the manifest's
    /// order. With the tag `tag_desc(V)` these are the version's components, which the commit registers.
    pub components: Vec<ComponentDesc>,
    /// Its systems' names, in the manifest's order (what pe-api's `code_check` reports).
    pub systems: Vec<String>,
}

impl Described {
    /// The description of the module `module` whose manifest is `manifest`, under version `v`. A component that
    /// won't take an id is the module's own fault, so it is refused like a manifest that won't read.
    fn of(module: Hash, manifest: &Manifest, v: &Hash) -> Result<Described, LoadError> {
        let components = own_components(manifest, v).map_err(refused)?;
        let systems = manifest.systems.iter().map(|s| s.name.clone()).collect();
        Ok(Described { module, components, systems })
    }
}

/// `manifest`'s own components (not the engine's) under the ids version `v` gives them, in the manifest's order. An
/// error is the module's own: the same module and version fail alike everywhere.
fn own_components(manifest: &Manifest, v: &Hash) -> Result<Vec<ComponentDesc>, String> {
    let own = manifest.components.iter().filter(|c| !c.name.starts_with(ENGINE_PREFIX));
    own.map(|c| {
        let fields = run::component_desc(c, ComponentId(0)).map_err(|e| e.to_string())?.fields().to_vec();
        let id = script_component_id(v, &c.name, fields).ok_or_else(|| format!("component {}: no id", c.name))?;
        run::component_desc(c, id).map_err(|e| e.to_string())
    })
    .collect()
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
    fn bind(&self, row: &ScriptRow, registry: &Registry) -> Result<Loaded, (String, bool)> {
        let held = self.held.get(&row.module).ok_or_else(|| ("the module is not held".to_string(), false))?;
        let tag = tag_desc(&row.version).id();
        let mut own = Vec::new();
        for want in own_components(&held.manifest, &row.version).map_err(|e| (e, true))? {
            // The world's own check of what bind checks too: until the world registers the version, it doesn't bind.
            if registry.get(want.id()) != Some(&want) {
                return Err((format!("component {} is not registered under this version", want.name()), false));
            }
            own.push(want.id());
        }
        if registry.get(tag).is_none() {
            return Err(("the version's tag is not registered".to_string(), false));
        }
        if let Some(id) = own.iter().chain([&tag]).find(|id| row.components.binary_search(id).is_err()) {
            return Err((format!("id {:?} is not among the row's", id), false));
        }
        let script = held.script.clone();
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
