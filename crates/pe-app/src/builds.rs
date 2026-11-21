//! The build service's machine side (logic.md 6.2 steps 3-4, 6.2a, 6.2b; M1's n = 2 path, @coordinator-opus; split with @engine-architect, design; api.md 7.1 with @api-eng).
//!
//! - **[`Builder`]** is the machine's one build service. It builds a source archive with pe-logic, loads the module
//!   with the runner's engine and settings, and describes it through the runner's [`ModuleInbox`]: its metered hash and
//!   its components under V's ids, as the runner would bind them. It keeps each result by V, the archive's hash, so a
//!   check, the author's commit and the attestation build a version once (api.md 7.1). One build runs at a time, in
//!   api.md 7.1's order: the attester's builds first, then pe-api's through the [`Builds`] port (`code_check` and the
//!   `code` op), first in, first out. A queued API build whose deadline passes leaves unbuilt; a started one finishes
//!   into the cache, so a retry answers at once.
//!   - The attester's build of a commit is pe-logic's `build_for_row`: the module must be the commit's, and a failure
//!     is retried as logic.md 6.2b says (`Local` twice, the last from clean; `Mismatch` once, from clean) before this
//!     machine reports it. A failure that means 6.2b's prevention has a gap (`Failed::wants_bundle`) leaves a bug
//!     bundle in the service's `bugs` folder, saved while the build's output is still the failed build's.
//!   - An API build is the author's: past the author's time margin (`AUTHOR_MAX_BUILD`, 15 s; Q4 (b)) it is refused,
//!     cached or not. The memory margin waits for pe-logic's measure of a build's peak. rustc's errors and its
//!     warnings about the script's own source come back as diagnostics, from pe-logic's parse of rustc's JSON.
//! - **[`Attester`]** makes one pass over a world's pending commits (`World::pending`) and its active versions
//!   (`World::modules`). For each commit or version this machine hasn't handled, it builds and checks the version and
//!   loads the module into the runner through its inbox, since every machine must hold the module at F, the author's
//!   included (logic.md 6.2 item 3). An active version needs no attestation, so it is only loaded: it is one this
//!   machine never saw pending, above all the author's own, which the author's prediction (logic.md 6.2a's option
//!   (i)) activates at the end of its commit's frame, so that no snapshot shows it pending there. Before
//!   @item:primordial-engine/m1-blocker-binding-an-activated-script-to-an-obj the pass read pending commits only, and
//!   the author's runner never held its own module. If this member is one of the commit's voters and
//!   isn't counted yet, the pass gives the `AttestBuild` to submit, or, for a build that failed after its retries, the
//!   `BuildFailed` (6.2a item 2). A module that built as the commit's but won't describe here, for a reason of this
//!   machine's, is only logged: the member holds its vote, as for a module the runner won't load.
//!   **The author's own pass loads but posts nothing:** its commit already counts as its attestation, and pe-world
//!   refuses a voter's second attestation, or a failure after it, as `Invalid`. Don't "fix" that into a post.
//! - **[`toolchain_and_sdk`]** gives the hashes a commit carries: the build identity B (logic.md 6.2b) and pe-sdk's
//!   source hash.
//!
//! A pass and a build block their thread, and a build takes seconds, so neither runs on the simulation thread. The
//! thread that calls them needs [`ModuleInbox::thread_stack`] of native stack: describing and loading a module read
//! its manifest, which runs it.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use pe_api::builds::{BuildRefused, Builds, Margin};
use pe_api::queue::{EditQueue, Receipt, Ticket};
use pe_logic::build::{AUTHOR_MAX_BUILD, BuildError, FailReason, Failed, RustcMessage, Toolchain};
use pe_logic::meter::RefusalKind;
use pe_logic::runtime::{LoadError, Script};
use pe_replica::logic::{Described, ModuleInbox};
use pe_seams::world_api::{Diagnostic, DiagnosticKind, Severity};
use pe_seams::{BuildFailure, Frame, Hash, MemberKey, Version};
use pe_world::edit::{Actor, Op, Transaction};
use pe_world::script::{AssetId, tag_desc};
use pe_world::{BlobRef, ModuleRow, PendingCommit};

/// Where the build service builds (logic.md 4.2), and the author's margin.
#[derive(Clone, Debug)]
pub struct BuildConfig {
    /// The pinned toolchain, by absolute path (pe-logic's `Toolchain`).
    pub toolchain: Toolchain,
    /// pe-sdk's source folder: [`sdk`] in pe-app.
    pub sdk: PathBuf,
    /// The service's own folder ([`build_dir`]). `work` under it is the work folder, so keep this folder's path short:
    /// logic.md 4.1a's paths are at most 128 bytes below it. `target` is the cargo target folder, kept between builds.
    pub dir: PathBuf,
    /// The author's time margin on an API build ([`Builds`]): `AUTHOR_MAX_BUILD`, other than in tests.
    pub margin: Duration,
}

impl BuildConfig {
    /// A configuration with the author's margin, `pe_logic::build::AUTHOR_MAX_BUILD`.
    pub fn new(toolchain: Toolchain, sdk: PathBuf, dir: PathBuf) -> BuildConfig {
        BuildConfig { toolchain, sdk, dir, margin: AUTHOR_MAX_BUILD }
    }

    fn work(&self) -> PathBuf {
        self.dir.join("work")
    }

    fn target(&self) -> PathBuf {
        self.dir.join("target")
    }

    /// Where bug bundles go (logic.md 6.2b): local only, for the user to hand to whoever chases the gap.
    fn bugs(&self) -> PathBuf {
        self.dir.join("bugs")
    }
}

/// The build folder of the machine whose identity seed is the file `identity`: `<its name>.b` beside it, so two
/// instances on one PC, each with its own `--identity`, never share a work folder (@engine-architect's Low 1 on
/// ed51a6ac).
pub fn build_dir(identity: &Path) -> PathBuf {
    let mut name = identity.file_name().map_or_else(|| "identity".into(), |n| n.to_os_string());
    name.push(".b");
    identity.with_file_name(name)
}

/// Where this process found pe-sdk's source folder ([`sdk`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SdkFrom {
    /// The `PE_SDK` variable.
    Env,
    /// A `pe-sdk` folder beside the executable: a pe-app copied to a machine without the engine's sources, such as the
    /// M1 demo's second PC (@coordinator-opus).
    BesideExe,
    /// The source tree pe-app was compiled from, `pe_logic::build::engine_sdk()`: the dev engine's.
    SourceTree,
}

impl SdkFrom {
    fn why(self) -> &'static str {
        match self {
            SdkFrom::Env => "from PE_SDK",
            SdkFrom::BesideExe => "beside the executable",
            SdkFrom::SourceTree => "the source tree this build was compiled from",
        }
    }
}

/// pe-sdk's folder from the first of: `env` (the `PE_SDK` variable; empty counts as unset, a relative path is taken
/// from the working folder), a `pe-sdk` folder beside `exe`, and the source tree pe-app was compiled from. The first
/// one present is the answer, and it must hold pe-sdk's `Cargo.toml`: one that doesn't is refused by name, never passed
/// over for the next, so a botched copy can't quietly build against another pe-sdk (@engine-architect).
/// The folder's place reaches neither a module (builds remap it to `/pe-sdk`) nor B (which hashes its files' contents).
pub fn locate_sdk(env: Option<OsString>, exe: Option<&Path>) -> Result<(PathBuf, SdkFrom), String> {
    let beside = exe.and_then(Path::parent).map(|d| d.join("pe-sdk")).filter(|d| d.exists());
    let (dir, from) = if let Some(v) = env.filter(|v| !v.is_empty()) {
        let p = PathBuf::from(v);
        (std::path::absolute(&p).unwrap_or(p), SdkFrom::Env)
    } else if let Some(dir) = beside {
        (dir, SdkFrom::BesideExe)
    } else {
        (pe_logic::build::engine_sdk(), SdkFrom::SourceTree)
    };
    if dir.join("Cargo.toml").is_file() {
        Ok((dir, from))
    } else {
        Err(format!("pe-sdk at {} ({}) holds no Cargo.toml", dir.display(), from.why()))
    }
}

/// This process's pe-sdk ([`locate_sdk`] with `PE_SDK` and the running executable), found once, so the build id and
/// the build service can't use two different folders. An error leaves this machine without a build service, as a
/// missing toolchain does.
pub fn sdk() -> &'static Result<(PathBuf, SdkFrom), String> {
    static SDK: OnceLock<Result<(PathBuf, SdkFrom), String>> = OnceLock::new();
    SDK.get_or_init(|| locate_sdk(std::env::var_os("PE_SDK"), std::env::current_exe().ok().as_deref()))
}

/// The console's start line for [`sdk`]: which pe-sdk this process builds scripts against, and why that one, an info
/// line for stdout like `session: ...`; or, as `Err`, the warning that it has none, for stderr, where pe-app's
/// `pe-app: ...` warnings go (@engine-architect's nit).
pub fn sdk_line() -> Result<String, String> {
    match sdk() {
        Ok((dir, from)) => Ok(format!("scripts: pe-sdk at {} ({})", dir.display(), from.why())),
        Err(e) => Err(format!("pe-app: scripts: no pe-sdk, so this machine builds no script: {e}")),
    }
}

/// A version this machine built (the machine's side of api.md 7.1's `Built`).
#[derive(Clone)]
pub struct Built {
    /// The module, loaded with the runner's engine and settings, for the runner's inbox.
    pub script: Script,
    /// Its metered hash (what `CommitScript::author_module` names), its components under V's ids and its systems.
    pub described: Described,
    /// How long the build took (for the attester's build of a commit, its retries included): api.md 7.1 applies the
    /// author's margin to it.
    pub build_ms: u32,
    /// rustc's warnings about the script's own source (pe-logic's `Built.warnings`). An attester's build doesn't keep
    /// them: a version the attester built first answers an API build with none.
    pub warnings: Vec<RustcMessage>,
}

impl std::fmt::Debug for Built {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Built")
            .field("module", &self.described.module)
            .field("build_ms", &self.build_ms)
            .finish_non_exhaustive()
    }
}

/// Why a [`Builder::build`] failed: pe-logic's build, or the built module's load.
#[derive(Debug)]
pub enum BuildFail {
    Build(BuildError),
    Load(LoadError),
}

impl std::fmt::Display for BuildFail {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BuildFail::Build(e) => e.fmt(f),
            BuildFail::Load(e) => write!(f, "the built module won't load: {e}"),
        }
    }
}

impl BuildFail {
    /// The failure as api.md 7.1's port answers it: the author's own error as diagnostics (rustc's messages, or
    /// pe-logic's refusal), and this machine's as `Unavailable`, never as the author's error.
    fn refused(self) -> BuildRefused {
        match self {
            BuildFail::Build(e) => match (e.reason(), &e) {
                (FailReason::Compile, BuildError::Rustc { text, diagnostics, .. }) => {
                    BuildRefused::Failed(rustc_diagnostics(diagnostics, text))
                }
                (FailReason::Refused, _) => {
                    BuildRefused::Failed(vec![diagnostic(DiagnosticKind::Manifest, e.to_string())])
                }
                _ => BuildRefused::Unavailable(e.to_string()),
            },
            BuildFail::Load(LoadError::Refused(r)) => {
                let kind = match r.kind {
                    RefusalKind::Import => DiagnosticKind::Import,
                    RefusalKind::Feature => DiagnosticKind::Feature,
                    RefusalKind::ReservedName => DiagnosticKind::ReservedName,
                    RefusalKind::Malformed | RefusalKind::Abi => DiagnosticKind::Manifest,
                };
                BuildRefused::Failed(vec![diagnostic(kind, r.message)])
            }
            BuildFail::Load(e) => BuildRefused::Unavailable(e.to_string()),
        }
    }
}

fn diagnostic(kind: DiagnosticKind, message: String) -> Diagnostic {
    Diagnostic { kind, severity: Severity::Error, message, file: None, line: None, column: None, code: None }
}

/// The most of rustc's own output a diagnostic carries when none of its lines is in the short format.
const RAW_MAX: usize = 4096;

/// rustc's errors and warnings (api.md 7.1), from pe-logic's parse of rustc's JSON (`BuildError::Rustc`'s
/// `diagnostics`, pe-logic v11), one diagnostic each, at the first primary span. Files are as pe-logic gives them:
/// crate-relative for the script (`src/lib.rs`), `/pe-sdk/...` for pe-sdk, never this machine's folders. Notes, helps
/// and rustc's summary lines (no place, `aborting due to ...`) are left out. When no error is left (cargo failed first,
/// a linker's error), the output itself (`text`), trimmed to [`RAW_MAX`] bytes, is the one diagnostic, so the author
/// still sees why (@api-eng's Medium on 3b8bf923).
fn rustc_diagnostics(diagnostics: &[RustcMessage], text: &str) -> Vec<Diagnostic> {
    let parsed: Vec<Diagnostic> = diagnostics.iter().filter_map(rustc_diagnostic).collect();
    if parsed.iter().any(|d| d.severity == Severity::Error) {
        return parsed;
    }
    let raw = text.trim();
    let mut end = raw.len().min(RAW_MAX);
    while !raw.is_char_boundary(end) {
        end -= 1;
    }
    let mut out = vec![diagnostic(DiagnosticKind::Rustc, raw[..end].to_string())];
    out.extend(parsed);
    out
}

/// One of rustc's messages as a diagnostic: an error (an internal compiler error included) or a warning with a place,
/// or an error with none; anything else is `None`.
fn rustc_diagnostic(m: &RustcMessage) -> Option<Diagnostic> {
    let severity = if m.level.starts_with("error") {
        Severity::Error
    } else if m.level == "warning" {
        Severity::Warning
    } else {
        return None;
    };
    let at = m.spans.first();
    if at.is_none() && (severity == Severity::Warning || m.message.starts_with("aborting due to")) {
        return None;
    }
    Some(Diagnostic {
        kind: DiagnosticKind::Rustc,
        severity,
        message: m.message.clone(),
        file: at.map(|s| s.file.clone()),
        line: at.map(|s| s.line),
        column: at.map(|s| s.column),
        code: m.code.clone(),
    })
}

/// One build at a time, in api.md 7.1's order: the attester's builds first, then the API's, first in, first out.
#[derive(Default)]
struct Gate {
    state: Mutex<GateState>,
    turn: Condvar,
}

#[derive(Default)]
struct GateState {
    busy: bool,
    /// The attester's builds waiting: they go before any API build.
    attesting: usize,
    /// The API builds waiting, by ticket, in their order of arrival.
    api: VecDeque<u64>,
    next: u64,
}

/// A turn at the build service: its build runs while it is held.
struct Turn<'a>(&'a Gate);

impl Drop for Turn<'_> {
    fn drop(&mut self) {
        self.0.lock().busy = false;
        self.0.turn.notify_all();
    }
}

impl Gate {
    fn lock(&self) -> MutexGuard<'_, GateState> {
        // Every change to the state is whole within one lock, so a thread that panicked left it consistent.
        self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The attester's turn: after the build in progress, and before any API build.
    fn attester(&self) -> Turn<'_> {
        let mut g = self.lock();
        g.attesting += 1;
        while g.busy {
            g = self.turn.wait(g).unwrap_or_else(|poisoned| poisoned.into_inner());
        }
        g.attesting -= 1;
        g.busy = true;
        Turn(self)
    }

    /// An API build's turn: after every attester build waiting and the API builds that came before it. `None` once
    /// `deadline` passes first, and the build leaves the queue.
    fn api(&self, deadline: Instant) -> Option<Turn<'_>> {
        let mut g = self.lock();
        let me = g.next;
        g.next += 1;
        g.api.push_back(me);
        loop {
            if !g.busy && g.attesting == 0 && g.api.front() == Some(&me) {
                g.api.pop_front();
                g.busy = true;
                return Some(Turn(self));
            }
            let now = Instant::now();
            if now >= deadline {
                g.api.retain(|&t| t != me);
                drop(g);
                self.turn.notify_all();
                return None;
            }
            g = self.turn.wait_timeout(g, deadline - now).unwrap_or_else(|poisoned| poisoned.into_inner()).0;
        }
    }
}

/// The build service (module doc).
pub struct Builder {
    config: BuildConfig,
    inbox: ModuleInbox,
    ids: (Hash, Hash),
    /// Each version built, by V ([`Cache`]).
    built: Mutex<Cache<Built>>,
    gate: Gate,
}

/// How many versions the build cache keeps.
const CACHED: usize = 16;

/// The versions built, by V: at most [`CACHED`], the least recently used dropped first. Each holds a compiled module,
/// and an agent iterating with `code_check` builds a new V each time, so the cache can't keep every one; a dropped
/// version is built again when asked (@engine-architect's Low 2 on 6700586b).
struct Cache<T> {
    by_v: BTreeMap<Hash, T>,
    /// The cached versions, least recently used first.
    used: VecDeque<Hash>,
}

impl<T> Default for Cache<T> {
    fn default() -> Self {
        Cache { by_v: BTreeMap::new(), used: VecDeque::new() }
    }
}

impl<T: Clone> Cache<T> {
    fn get(&mut self, v: &Hash) -> Option<T> {
        let b = self.by_v.get(v).cloned();
        if b.is_some() {
            self.touch(*v);
        }
        b
    }

    fn insert(&mut self, v: Hash, b: T) {
        self.by_v.insert(v, b);
        self.touch(v);
        while self.by_v.len() > CACHED {
            let Some(old) = self.used.pop_front() else { break };
            self.by_v.remove(&old);
        }
    }

    fn touch(&mut self, v: Hash) {
        self.used.retain(|u| *u != v);
        self.used.push_back(v);
    }
}

impl Builder {
    /// A builder that loads and describes modules with `inbox`'s runner, which decides their hashes.
    pub fn new(config: BuildConfig, inbox: ModuleInbox) -> Result<Builder, BuildError> {
        let ids = toolchain_and_sdk(&config.toolchain, &config.sdk)?;
        Ok(Builder { config, inbox, ids, built: Mutex::new(Cache::default()), gate: Gate::default() })
    }

    /// This machine's toolchain and SDK hashes, as a `CommitScript` carries them ([`toolchain_and_sdk`]).
    pub fn ids(&self) -> (Hash, Hash) {
        self.ids
    }

    /// The version `archive` holds (V is its hash), built as the build service builds it, or what an earlier build of
    /// it gave. A failure is not kept: the next call builds again. Blocks for the build, which takes seconds, and goes
    /// before any API build.
    pub fn build(&self, archive: &[u8]) -> Result<Built, BuildFail> {
        let v = version_of(archive);
        if let Some(b) = self.cached(&v) {
            return Ok(b);
        }
        let _turn = self.gate.attester();
        self.build_in_turn(archive, v)
    }

    /// The attester's build of commit `p`'s version (logic.md 6.2 step 3, 6.2b): pe-logic's `build_for_row`, whose
    /// module must be the commit's `author_module`, with its retries, and whose components, the tag among them, must be
    /// the commit's. A build this machine already has with that module answers at once. `attested_by_another`: whether
    /// a member other than this one has built the version (the author, or another attester), which decides whether a
    /// failure is bundled ([`RowFail::Failed`]).
    pub fn build_for_row(
        &self,
        archive: &[u8],
        p: &PendingCommit,
        attested_by_another: bool,
    ) -> Result<Built, RowFail> {
        self.build_checked(archive, p.author_module, |b| components_match(b, p), attested_by_another)
    }

    /// The build of active version `row`, which this machine never saw pending (the author's own under its
    /// prediction, module doc): the same build and checks as a pending commit's, by a member other than this one.
    pub fn build_for_active(&self, archive: &[u8], row: &ModuleRow) -> Result<Built, RowFail> {
        self.build_checked(archive, row.author_module, |b| active_components_match(b, row), true)
    }

    /// pe-logic's `build_for_row` of `archive`, whose module must be `author_module` and whose components, with the
    /// version's tag, must be the record's (`matches`).
    fn build_checked(
        &self,
        archive: &[u8],
        author_module: Hash,
        matches: impl Fn(&Built) -> bool,
        attested_by_another: bool,
    ) -> Result<Built, RowFail> {
        let v = version_of(archive);
        let theirs = |b: &Built| b.described.module == author_module;
        if let Some(b) = self.cached(&v).filter(theirs).filter(|b| matches(b)) {
            return Ok(b);
        }
        let _turn = self.gate.attester();
        // From here to the return, this turn holds the build's output, so a bug bundle sees the failed build's.
        let fail = |failed: Failed| self.failed(failed, archive, author_module, attested_by_another);
        if let Some(b) = self.cached(&v).filter(theirs) {
            return if matches(&b) { Ok(b) } else { Err(fail(components_differ(true))) };
        }
        let (c, inbox) = (&self.config, &self.inbox);
        let started = Instant::now();
        let script = pe_logic::build::build_for_row(
            &c.toolchain,
            archive,
            &c.sdk,
            &c.work(),
            &c.target(),
            true,
            inbox.engine(),
            inbox.settings(),
            author_module,
        )
        .map_err(fail)?;
        let build_ms = millis(started.elapsed());
        let described = inbox.describe_script(&script, &v).map_err(|e| match describe_failed(e) {
            Ok(failed) => fail(failed),
            Err(why) => RowFail::Held(why),
        })?;
        let b = Built { script, described, build_ms, warnings: Vec::new() };
        self.keep(v, &b);
        if matches(&b) { Ok(b) } else { Err(fail(components_differ(false))) }
    }

    /// `failed` as [`RowFail::Failed`], with a bug bundle when it wants one (6.2b). Call it in the attester's turn,
    /// while the build's output is still the failed build's: the bundle copies its module and dep-info.
    fn failed(&self, failed: Failed, archive: &[u8], author_module: Hash, attested_by_another: bool) -> RowFail {
        let c = &self.config;
        let bundle = failed.wants_bundle(attested_by_another).then(|| {
            pe_logic::build::save_bug_bundle(
                &c.toolchain,
                &c.bugs(),
                archive,
                author_module,
                &failed,
                &c.sdk,
                &c.target(),
            )
            .map_err(|e| e.to_string())
        });
        RowFail::Failed { failed, bundle }
    }

    /// A build in a turn the caller holds; one that finished while the caller waited for it answers.
    fn build_in_turn(&self, archive: &[u8], v: Hash) -> Result<Built, BuildFail> {
        if let Some(b) = self.cached(&v) {
            return Ok(b);
        }
        let c = &self.config;
        let started = Instant::now();
        let out = pe_logic::build::build_archive_reporting(&c.toolchain, archive, &c.sdk, &c.work(), &c.target(), true)
            .map_err(BuildFail::Build)?;
        let build_ms = millis(started.elapsed());
        let script = Script::load(self.inbox.engine(), self.inbox.settings(), &out.wasm).map_err(BuildFail::Load)?;
        let described = self.inbox.describe_script(&script, &v).map_err(BuildFail::Load)?;
        let b = Built { script, described, build_ms, warnings: out.warnings };
        self.keep(v, &b);
        Ok(b)
    }

    fn cached(&self, v: &Hash) -> Option<Built> {
        self.lock_built().get(v)
    }

    fn keep(&self, v: Hash, b: &Built) {
        self.lock_built().insert(v, b.clone());
    }

    fn lock_built(&self) -> MutexGuard<'_, Cache<Built>> {
        // Every change to the cache is whole within one lock.
        self.built.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl Builds for Builder {
    /// An author's build (api.md 7.1): after the attester's builds and the API builds before it, and refused past the
    /// author's time margin, cached or not. rustc's warnings about the script's own source come back with it.
    fn build(&self, archive: &[u8], deadline: Instant) -> Result<pe_api::builds::Built, BuildRefused> {
        let v = version_of(archive);
        let b = match self.cached(&v) {
            Some(b) => b,
            None => {
                let _turn = self.gate.api(deadline).ok_or(BuildRefused::Timeout)?;
                // Describing the module reads its manifest, which runs it, so the build runs on a thread with the
                // inbox's stack, never on the API worker's (pe-mcp's blocking pool has 2 MiB, and a guest may use up
                // to its wasm stack; @engine-architect's Medium on 6700586b).
                let built = std::thread::scope(|s| {
                    let job = std::thread::Builder::new()
                        .name("pe-build".into())
                        .stack_size(self.inbox.thread_stack())
                        .spawn_scoped(s, || self.build_in_turn(archive, v));
                    match job {
                        Ok(h) => h.join().map_err(|_| BuildRefused::Unavailable("the build thread panicked".into())),
                        Err(e) => Err(BuildRefused::Unavailable(format!("the build thread did not start: {e}"))),
                    }
                })?;
                built.map_err(BuildFail::refused)?
            }
        };
        if u128::from(b.build_ms) > self.config.margin.as_millis() {
            return Err(BuildRefused::Margin { which: Margin::Time, measured: u64::from(b.build_ms) });
        }
        let (toolchain, sdk) = self.ids;
        // The version's tag with the module's own components: pe-api registers exactly these under the commit, and
        // pe-world refuses a `Register` without exactly one tag (world-model.md 7.9), so without it every live `code`
        // op was refused `Invalid` (@iter-bench's c-to-F probe, 2026-11-17).
        let mut components = b.described.components;
        components.push(tag_desc(&v));
        Ok(pe_api::builds::Built {
            module: b.described.module,
            toolchain,
            sdk,
            components,
            systems: b.described.systems,
            warnings: b.warnings.iter().filter_map(rustc_diagnostic).collect(),
            build_ms: b.build_ms,
            peak_bytes: None,
        })
    }
}

/// What a built module that won't describe means (logic.md 6.2a item 2). A manifest that won't read, or a component
/// that won't take an id, is the module's own fault, the same on every machine: `Refused` (@engine-architect's Low 1 on
/// 6700586b). Anything else is this machine's own, and describing again in this process is unlikely to clear it, so
/// it's no `BuildFailed`, which would cost the member the session if the version activates (Q2a): the member holds
/// its vote, and `Err` says why (@vdw-physics, @iter-bench).
fn describe_failed(e: LoadError) -> Result<Failed, String> {
    match e {
        LoadError::Refused(_) => {
            let detail = format!("the built module won't describe: {e}");
            Ok(Failed { reason: FailReason::Refused, module: None, attempts: 1, detail })
        }
        e => Err(format!("the built module won't describe here: {e}")),
    }
}

/// Whether `b`'s components, with the version's tag, are commit `p`'s.
fn components_match(b: &Built, p: &PendingCommit) -> bool {
    let mut ids: Vec<_> = b.described.components.iter().map(|c| c.id()).collect();
    ids.push(tag_desc(&p.v()).id());
    ids.sort();
    ids == p.components
}

/// Whether `b`'s components, with the version's tag, are active version `row`'s.
fn active_components_match(b: &Built, row: &ModuleRow) -> bool {
    let mut ids: Vec<_> = b.described.components.iter().map(|c| c.id()).collect();
    ids.push(tag_desc(&row.v()).id());
    ids.sort();
    ids == row.components
}

/// Components that differ from the commit's, with the module the author's. One module under one V declares the same
/// components on every machine, so this is the commit's own fault or the same module describing differently on two
/// machines: `Refused`, the same everywhere (6.2a item 2; @iter-bench's Low 2 on builds-port), and bundled when another
/// member built the version, since in M1, with every member trusted, the second is the likelier (@engine-architect's
/// Low 1 on app-b-r0). `cached`: the module came from the cache, so nothing was built in this turn and a bug bundle's
/// dep-info is the last build's, maybe another version's; the detail, which the bundle's report carries, says so
/// (@engine-architect's nit on app-b-r1).
fn components_differ(cached: bool) -> Failed {
    let mut detail = "the module's components differ from the commit's".to_string();
    if cached {
        detail.push_str(
            " (the module came from this machine's build cache: nothing was built for this check, so a bug bundle's              deps/ are the last build's, maybe another version's)",
        );
    }
    Failed { reason: FailReason::Refused, module: None, attempts: 1, detail }
}

fn version_of(archive: &[u8]) -> Hash {
    Hash(BlobRef::of(archive).hash)
}

fn millis(d: Duration) -> u32 {
    u32::try_from(d.as_millis()).unwrap_or(u32::MAX)
}

/// The toolchain and SDK hashes a `CommitScript` carries and an attester compares with its own (logic.md 6.2 step 3,
/// D75): the build identity B (`pe_logic::build::build_identity`: the toolchain's release and wasm32 std, pe-sdk, the
/// service's data and versions), the same B pe-app's session `BUILD` carries, and pe-sdk's sources packed as a
/// script's are (`pe_logic::build::pack`). B covers pe-sdk already; its own hash names it apart in a commit. Two
/// machines with one toolchain and one engine build give the same pair. The module's hash is the comparison that
/// decides; these refuse a mismatched build early.
pub fn toolchain_and_sdk(toolchain: &Toolchain, sdk: &Path) -> Result<(Hash, Hash), BuildError> {
    let b = pe_logic::build::build_identity(toolchain, sdk)?;
    Ok((b, Hash::of("pe.app.sdk", &pe_logic::build::pack(sdk)?)))
}

/// The transaction that attests this machine's build of version `v` of `asset` into `module`: `Op::AttestBuild`, the
/// only op of its transaction (logic.md 6.2a). The member's machine makes it for the member, so its actor is the
/// person's; pe-api's machine lane (api.md 7.1) is how it reaches the edit queue.
pub fn attestation(asset: AssetId, v: Hash, module: Hash) -> Transaction {
    machine_entry(Op::AttestBuild { script: asset, version: v, module })
}

/// The transaction that reports this machine's failed build of version `v` of `asset` (logic.md 6.2a item 2):
/// `Op::BuildFailed` alone, with its reason, this machine's toolchain and SDK hashes (`ids`) and, for a `Mismatch`,
/// the module hash it built. Never compiler output, which can carry local paths. Made and sent as [`attestation`] is.
pub fn build_failed(
    asset: AssetId,
    v: Hash,
    reason: BuildFailure,
    ids: (Hash, Hash),
    module: Option<Hash>,
) -> Transaction {
    machine_entry(Op::BuildFailed { script: asset, version: v, reason, toolchain: ids.0, sdk: ids.1, module })
}

fn machine_entry(op: Op) -> Transaction {
    Transaction {
        inline: vec![],
        actor: Actor::Person,
        label: None,
        preconditions: Vec::new(),
        ops: vec![op],
        blobs: Vec::new(),
    }
}

/// An attestation to submit, with the commit it attests: its edit queue receipt goes back to [`Attester::posted`].
#[derive(Clone, Debug, PartialEq)]
pub struct Attest {
    pub asset: AssetId,
    /// The commit: its entry and op.
    pub version: Version,
    pub tx: Transaction,
}

/// A `BuildFailed` to submit, with the commit it reports on (logic.md 6.2a item 2).
#[derive(Clone, Debug, PartialEq)]
pub struct Failure {
    pub asset: AssetId,
    /// The commit: its entry and op.
    pub version: Version,
    pub reason: BuildFailure,
    pub tx: Transaction,
}

/// What one [`Attester::pass`] did.
#[derive(Debug, Default)]
pub struct Pass {
    /// The versions whose modules it loaded into the runner, as (asset, V).
    pub loaded: Vec<(AssetId, Hash)>,
    /// The attestations to submit.
    pub attest: Vec<Attest>,
    /// The failures to report: this voter's builds that failed after their retries.
    pub failures: Vec<Failure>,
    /// What went wrong, as (asset, V, why), for this machine's log: every failure, reported or not. The text can name
    /// local paths, so it never leaves the machine.
    pub failed: Vec<(AssetId, Hash, String)>,
}

/// One machine's attester (module doc).
pub struct Attester {
    me: MemberKey,
    /// The commits handled, by asset and commit (its entry and op): a version committed again after a void is a new
    /// commit, built from the cache and attested again.
    done: BTreeSet<(AssetId, Version)>,
    /// This machine's attestations whose fate isn't known yet, with the stamp each got.
    posted: BTreeMap<(AssetId, Version), Frame>,
    /// The versions, by asset and V, whose modules it loaded into the runner, and why the others it handled failed:
    /// what [`Attester::not_running`] reports.
    loaded: BTreeSet<(AssetId, Hash)>,
    failed: BTreeMap<(AssetId, Hash), String>,
}

impl Attester {
    /// The attester of this machine's member `me`.
    pub fn new(me: MemberKey) -> Attester {
        Attester {
            me,
            done: BTreeSet::new(),
            posted: BTreeMap::new(),
            loaded: BTreeSet::new(),
            failed: BTreeMap::new(),
        }
    }

    /// The active versions whose module this machine hasn't loaded into its runner, as (asset, the first frame the
    /// version runs in: the row's `activate_at`, F + 1, why): the scripts this machine can't run (S6's
    /// `scripts_not_running`, with no object; @coordinator-opus). Why is the first line of the build's failure, never a local path, or that the module isn't
    /// built here yet. A script whose module is loaded but won't bind isn't here: the runner knows that one (logged by
    /// pe-replica's runner only as a count, `ReplicaImpl::script_counts`).
    pub fn not_running(&self, active: &BTreeMap<AssetId, ModuleRow>) -> Vec<(AssetId, Frame, String)> {
        let mut out = Vec::new();
        for (&asset, row) in active {
            let key = (asset, row.v());
            if self.loaded.contains(&key) {
                continue;
            }
            let why = match self.failed.get(&key) {
                Some(f) => format!("its build here failed: {}", f.lines().next().unwrap_or_default()),
                None => "its module isn't built on this machine yet".to_string(),
            };
            out.push((asset, row.activate_at, why));
        }
        out
    }

    /// Records the stamp the edit queue gave this machine's attestation of `asset`'s commit `version` (its receipt),
    /// so that [`Attester::reposts`] can tell when its fate is final.
    pub fn posted(&mut self, asset: AssetId, version: Version, stamp: Frame) {
        self.posted.insert((asset, version), stamp);
    }

    /// The attestations to post again (api.md 7.1; @design-reviewer). The rule is state-based. Once
    /// `final_frame` has passed an attestation's stamp (S5: an entry stamped below it is final), `agreed`, the final
    /// snapshot's pending commits, has decided it. If its commit is still pending there, not voided, and this member
    /// still isn't counted, the attestation was voided or dropped, whatever the cause. It's posted again while the
    /// commit's deadline D is ahead of `now`. At n = 2 (q = 2), one late-voided attestation would otherwise leave a
    /// good commit `Unattested` at D. An attestation is forgotten once decided: counted, activated, voided, replaced or
    /// past D, or posted again, until the new post's receipt records it.
    pub fn reposts(
        &mut self,
        now: Frame,
        final_frame: Frame,
        agreed: &BTreeMap<AssetId, PendingCommit>,
    ) -> Vec<Attest> {
        let (me, mut again) = (self.me, Vec::new());
        self.posted.retain(|&(asset, version), stamp| {
            if stamp.0 >= final_frame.0 {
                return true;
            }
            let p = agreed.get(&asset).filter(|p| p.version == version && p.void.is_none());
            if let Some(p) = p.filter(|p| !p.attested.iter().any(|(k, _)| *k == me) && now.0 < p.deadline.0) {
                again.push(Attest { asset, version, tx: attestation(asset, p.v(), p.author_module) });
            }
            false
        });
        again
    }

    /// One pass over `pending`, a world's pending commits (`World::pending`), then `active`, its active versions
    /// (`World::modules`), each in asset order (module doc). `blob` gives a source archive from the world's store. A
    /// commit or version whose archive isn't there yet is tried again at the next pass; a voided commit is skipped.
    /// `inbox` is the runner's. A build's retries (6.2b) happen inside the pass, so each is handled once: a commit is
    /// attested, reported as failed, or, when this machine can't judge it, only logged; an active version is loaded,
    /// or its failure logged, never attested or reported (it needs no attestation).
    pub fn pass(
        &mut self,
        pending: &BTreeMap<AssetId, PendingCommit>,
        active: &BTreeMap<AssetId, ModuleRow>,
        blob: impl Fn(&BlobRef) -> Option<Arc<[u8]>>,
        builder: &Builder,
        inbox: &ModuleInbox,
    ) -> Pass {
        let mut pass = Pass::default();
        for (&asset, p) in pending {
            let key = (asset, p.version);
            if p.void.is_some() || self.done.contains(&key) {
                continue;
            }
            let Some(archive) = blob(&p.source) else { continue };
            self.done.insert(key);
            let v = p.v();
            // A voter not yet counted either way. The author's commit counts as its attestation (module doc).
            let voter = p.voters.contains(&self.me) && !p.has_attested(&self.me) && !p.has_failed(&self.me);
            // The author built it, voter or not, and so did any other member counted (@engine-architect's Low 2).
            let attested_by_another = p.author != self.me || p.attested.iter().any(|(k, _)| *k != self.me);
            match check(p, builder, &archive, attested_by_another) {
                Ok(built) => {
                    if let Err(e) = inbox.load_script(built.script) {
                        pass.failed.push((asset, v, format!("the module won't load into the runner: {e}")));
                        continue;
                    }
                    pass.loaded.push((asset, v));
                    if voter {
                        pass.attest.push(Attest {
                            asset,
                            version: p.version,
                            tx: attestation(asset, v, p.author_module),
                        });
                    }
                }
                Err(Unchecked::Ids) => pass.failed.push((asset, v, "the commit names another build identity".into())),
                Err(Unchecked::Held(why)) => pass.failed.push((asset, v, why)),
                Err(Unchecked::Failed { failed: f, bundle }) => {
                    let mut why = format!("{:?} after {} build(s): {}", f.reason, f.attempts, f.detail);
                    match bundle {
                        Some(Ok(dir)) => {
                            why.push_str(&format!("\nbug bundle (local, see report.txt): {}", dir.display()))
                        }
                        Some(Err(e)) => why.push_str(&format!("\nthe bug bundle wasn't saved: {e}")),
                        None => {}
                    }
                    pass.failed.push((asset, v, why));
                    if voter {
                        let reason = reported(f.reason);
                        let tx = build_failed(asset, v, reason, builder.ids(), f.module);
                        pass.failures.push(Failure { asset, version: p.version, reason, tx });
                    }
                }
            }
        }
        // Every active version this machine hasn't handled: logic.md 6.2 item 3 has every member build what its line
        // activates. The author never sees its own commit pending (module doc), and no machine sees one pending that
        // activated before its attester looked. A version handled while pending is not built again.
        for (&asset, row) in active {
            let key = (asset, row.version);
            if self.done.contains(&key) {
                continue;
            }
            let Some(archive) = blob(&row.source) else { continue };
            self.done.insert(key);
            let v = row.v();
            if (row.toolchain, row.sdk) != builder.ids() {
                pass.failed.push((asset, v, "the active version names another build identity".into()));
                continue;
            }
            match builder.build_for_active(&archive, row) {
                Ok(built) => match inbox.load_script(built.script) {
                    Ok(_) => pass.loaded.push((asset, v)),
                    Err(e) => pass.failed.push((asset, v, format!("the module won't load into the runner: {e}"))),
                },
                Err(RowFail::Held(why)) => pass.failed.push((asset, v, why)),
                Err(RowFail::Failed { failed: f, .. }) => {
                    let why = format!("{:?} after {} build(s): {}", f.reason, f.attempts, f.detail);
                    pass.failed.push((asset, v, format!("an active version: {why}")));
                }
            }
        }
        for &(asset, v) in &pass.loaded {
            self.loaded.insert((asset, v));
            self.failed.remove(&(asset, v));
        }
        for (asset, v, why) in &pass.failed {
            self.failed.insert((*asset, *v), why.clone());
        }
        pass
    }
}

/// Why a commit's version isn't this machine's to run.
enum Unchecked {
    /// The commit names another build identity B or SDK than this machine's. The session compares B at join, so a
    /// member's commit can't differ from this machine's; if one does, this machine can't judge it and reports
    /// nothing: the commit voids at D unless the quorum attests without this member.
    Ids,
    /// This machine's own trouble with a module that built as the commit's: logged, nothing reported.
    Held(String),
    /// The build failed after its retries (6.2b), or its module or components aren't the commit's. `bundle` is the
    /// bug bundle's folder when the failure wants one.
    Failed { failed: Failed, bundle: Option<Result<PathBuf, String>> },
}

/// Why [`Builder::build_for_row`] gave no module.
#[derive(Debug)]
pub enum RowFail {
    /// The build failed after its retries (6.2b): `BuildFailed`'s to report. `bundle` is set when the failure means
    /// prevention has a gap (`Failed::wants_bundle`): the bug bundle's folder, or why it wasn't saved.
    Failed { failed: Failed, bundle: Option<Result<PathBuf, String>> },
    /// The module built as the commit's but won't describe here, for a reason of this machine's: nothing to report.
    Held(String),
}

impl From<RowFail> for Unchecked {
    fn from(f: RowFail) -> Unchecked {
        match f {
            RowFail::Failed { failed, bundle } => Unchecked::Failed { failed, bundle },
            RowFail::Held(why) => Unchecked::Held(why),
        }
    }
}

/// This machine's build of `p`'s version, checked against the commit (logic.md 6.2 step 3): the same build identity
/// and SDK, then [`Builder::build_for_row`]: the same module and the same components, the tag among them.
fn check(p: &PendingCommit, builder: &Builder, archive: &[u8], attested_by_another: bool) -> Result<Built, Unchecked> {
    if (p.toolchain, p.sdk) != builder.ids() {
        return Err(Unchecked::Ids);
    }
    Ok(builder.build_for_row(archive, p, attested_by_another)?)
}

/// The reason a failure is reported with (`BuildFailed`'s, logic.md 6.2a item 2): pe-logic's. pe-logic's `Failed`
/// carries a module exactly for a `Mismatch` (the raw module's hash when its metering pass refused it), which is what
/// pe-world's `op_build_failed` requires.
fn reported(reason: FailReason) -> BuildFailure {
    match reason {
        FailReason::Compile => BuildFailure::Compile,
        FailReason::Refused => BuildFailure::Refused,
        FailReason::Cap => BuildFailure::Cap,
        FailReason::Mismatch => BuildFailure::Mismatch,
        FailReason::Local => BuildFailure::Local,
    }
}

/// How long the attester's thread waits for a newer snapshot before it looks at its receipts and reposts again.
const WAIT: Duration = Duration::from_millis(250);

/// A machine entry the attester's thread submits: an attestation, whose receipt the attester keeps for its reposts,
/// or a failure.
enum Post {
    Attest(Attest),
    Failure(Failure),
}

impl Post {
    fn tx(&self) -> &Transaction {
        match self {
            Post::Attest(a) => &a.tx,
            Post::Failure(f) => &f.tx,
        }
    }

    fn asset(&self) -> &AssetId {
        match self {
            Post::Attest(a) => &a.asset,
            Post::Failure(f) => &f.asset,
        }
    }
}

/// The attester's thread ([`spawn_attester`]).
pub struct AttesterThread {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl AttesterThread {
    /// Stops the thread and waits for it, which takes as long as a build in progress.
    pub fn stop(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// Starts the attester's thread (module doc) over the snapshots pe-app publishes for pe-api (`shared`). After each
/// new one, or 250 ms (`WAIT`) without one, it makes a pass over the newest snapshot's pending commits, logs what
/// failed, and submits the attestations, the failures and the reposts (from the agreed snapshot) through pe-api's
/// machine lane (`EditQueue::submit_machine`, api.md 7.1). Each attestation's receipt stamp goes back to the attester.
/// A submission the lane has no room for waits for the next pass. An attestation that agreement refuses is treated as
/// posted at the present frame, so the state-based repost decides it again once that frame is final. A failure that
/// agreement refuses is logged and not posted again: its commit then voids at D (`Unattested`) rather than at once.
/// The thread runs on the inbox's stack.
pub fn spawn_attester(
    builder: Arc<Builder>,
    inbox: ModuleInbox,
    shared: Arc<crate::api::ApiShared>,
    queue: Arc<EditQueue>,
    me: MemberKey,
) -> io::Result<AttesterThread> {
    let stop = Arc::new(AtomicBool::new(false));
    let flag = stop.clone();
    let stack = inbox.thread_stack();
    let run = move || {
        let mut attester = Attester::new(me);
        // The scripts this machine can't run, as last published: each change is logged.
        let mut not_running: Vec<(AssetId, Frame, String)> = Vec::new();
        let (mut seen, mut unsent, mut waiting) = (0, Vec::<Post>::new(), Vec::<(Ticket, Post)>::new());
        while !flag.load(Ordering::Relaxed) {
            let (published, snap) = shared.wait_newer(seen, WAIT);
            seen = published;
            let now = snap.frame();
            let pass = attester.pass(snap.pending(), snap.modules(), |r| snap.blob(r), &builder, &inbox);
            for (asset, v, why) in &pass.failed {
                crate::err!("pe-app: this machine's build of script {} at {v:?} failed: {why}", short(asset));
            }
            for f in &pass.failures {
                crate::err!(
                    "pe-app: reporting this machine's failed build of script {}: {:?}",
                    short(&f.asset),
                    f.reason
                );
            }
            let now_not = attester.not_running(snap.modules());
            if now_not != not_running {
                for (asset, since, why) in &now_not {
                    // A newer version that fails for the same reason is logged too (@engine-architect's nit).
                    if !not_running.iter().any(|(a, s, w)| a == asset && s == since && w == why) {
                        crate::err!(
                            "pe-app: script {} doesn't run on this machine (active since frame {}): {why}",
                            short(asset),
                            since.0
                        );
                    }
                }
                for (asset, _, _) in &not_running {
                    if !now_not.iter().any(|(a, _, _)| a == asset) {
                        crate::err!("pe-app: script {} runs on this machine now", short(asset));
                    }
                }
                shared.scripts_not_running(now_not.clone());
                not_running = now_not;
            }
            let agreed = shared.agreed_snapshot();
            unsent.extend(pass.attest.into_iter().map(Post::Attest));
            unsent.extend(pass.failures.into_iter().map(Post::Failure));
            unsent.extend(attester.reposts(now, agreed.frame(), agreed.pending()).into_iter().map(Post::Attest));
            for post in std::mem::take(&mut unsent) {
                match queue.submit_machine(post.tx().clone()) {
                    Ok(t) => waiting.push((t, post)),
                    Err(_) => unsent.push(post),
                }
            }
            waiting.retain(|(t, post)| match queue.receipt(*t, Instant::now()) {
                Some(Receipt::Submitted { frame, .. }) => {
                    if let Post::Attest(a) = post {
                        attester.posted(a.asset, a.version, Frame(frame));
                    }
                    false
                }
                Some(Receipt::Refused(e)) => {
                    crate::err!("pe-app: a machine entry of script {} was not submitted: {e:?}", short(post.asset()));
                    if let Post::Attest(a) = post {
                        attester.posted(a.asset, a.version, now);
                    }
                    false
                }
                // Only a script commit is held back during a join, never a machine entry; answered as a refusal.
                Some(Receipt::Unavailable(why)) => {
                    crate::err!("pe-app: a machine entry of script {} was not submitted: {why}", short(post.asset()));
                    if let Post::Attest(a) = post {
                        attester.posted(a.asset, a.version, now);
                    }
                    false
                }
                None => true,
            });
        }
    };
    let handle = std::thread::Builder::new().name("pe-attester".into()).stack_size(stack).spawn(run)?;
    Ok(AttesterThread { stop, handle: Some(handle) })
}

/// An asset's first bytes, in hex, for a log line.
fn short(asset: &AssetId) -> String {
    asset.iter().take(4).map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::thread;

    use super::*;

    /// PE_SDK first, then a pe-sdk folder beside the executable, then the source tree; the first present must hold a
    /// Cargo.toml, and one that doesn't is refused by name rather than passed over.
    #[test]
    fn pe_sdk_comes_from_the_variable_then_beside_the_exe_then_the_source_tree() {
        // Next to the test binary, in the target folder: no working files on the system drive.
        let root =
            std::env::current_exe().unwrap().parent().unwrap().join(format!("pe-app-sdk-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let exe = root.join("pe-app.exe");
        let beside = root.join("pe-sdk");
        let tree = (pe_logic::build::engine_sdk(), SdkFrom::SourceTree);
        assert!(tree.0.join("Cargo.toml").is_file());

        // Nothing beside the executable: the source tree.
        std::fs::create_dir_all(&root).unwrap();
        assert_eq!(locate_sdk(None, Some(&exe)), Ok(tree.clone()));
        assert_eq!(locate_sdk(None, None), Ok(tree.clone()));
        // A pe-sdk folder beside it without its Cargo.toml is refused, by name, not passed over.
        std::fs::create_dir_all(&beside).unwrap();
        let e = locate_sdk(None, Some(&exe)).unwrap_err();
        assert!(e.contains(&beside.display().to_string()) && e.contains("beside the executable"), "{e}");
        std::fs::write(beside.join("Cargo.toml"), b"[package]\n").unwrap();
        assert_eq!(locate_sdk(None, Some(&exe)), Ok((beside.clone(), SdkFrom::BesideExe)));
        // An empty variable counts as unset; a set one wins over the folder beside the executable.
        assert_eq!(locate_sdk(Some("".into()), Some(&exe)), Ok((beside.clone(), SdkFrom::BesideExe)));
        // (taken through std::path::absolute, which on Windows also resolves the `..` in engine_sdk()'s path)
        let given = std::path::absolute(&tree.0).unwrap();
        assert_eq!(locate_sdk(Some(tree.0.clone().into()), Some(&exe)), Ok((given, SdkFrom::Env)));
        // A variable naming a folder without a Cargo.toml is refused, not passed over for the one beside the exe.
        let wrong = root.join("no-such-sdk");
        let e = locate_sdk(Some(wrong.clone().into()), Some(&exe)).unwrap_err();
        assert!(e.contains(&wrong.display().to_string()) && e.contains("from PE_SDK"), "{e}");
        // A relative one is taken from the working folder.
        let e = locate_sdk(Some("some-relative-sdk".into()), Some(&exe)).unwrap_err();
        let abs = std::path::absolute("some-relative-sdk").unwrap();
        assert!(e.contains(&abs.display().to_string()), "{e}");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// An API build waits for the build in progress, and leaves unbuilt once its deadline passes.
    #[test]
    fn an_api_build_leaves_the_queue_at_its_deadline() {
        let gate = Gate::default();
        let held = gate.attester();
        let t0 = Instant::now();
        assert!(gate.api(t0 + Duration::from_millis(60)).is_none(), "timed out");
        assert!(t0.elapsed() >= Duration::from_millis(60));
        assert!(gate.lock().api.is_empty(), "left the queue");
        drop(held);
        assert!(gate.api(Instant::now() + Duration::from_secs(1)).is_some(), "free again");
    }

    /// The attester's builds go before API builds that came first, and API builds go in their order of arrival.
    #[test]
    fn the_attesters_builds_go_first_then_the_apis_in_order() {
        let gate = Arc::new(Gate::default());
        let held = gate.attester();
        let (tx, rx) = mpsc::channel();
        let queued = || {
            let g = gate.lock();
            (g.api.len(), g.attesting)
        };
        let wait_for = |want: (usize, usize)| {
            let t0 = Instant::now();
            while queued() != want {
                assert!(t0.elapsed() < Duration::from_secs(10), "the waiters never queued");
                thread::yield_now();
            }
        };
        let mut threads = Vec::new();
        for (name, attester) in [("api 1", false), ("api 2", false), ("attester", true)] {
            let (gate, tx) = (gate.clone(), tx.clone());
            threads.push(thread::spawn(move || {
                let _turn =
                    if attester { Some(gate.attester()) } else { gate.api(Instant::now() + Duration::from_secs(30)) };
                tx.send(name).unwrap();
            }));
            let n = threads.len();
            wait_for((n.min(2), usize::from(n == 3)));
        }
        drop(held);
        for t in threads {
            t.join().unwrap();
        }
        assert_eq!(rx.try_iter().collect::<Vec<_>>(), ["attester", "api 1", "api 2"]);
    }

    fn msg(level: &str, code: Option<&str>, message: &str, at: Option<(u32, u32)>) -> RustcMessage {
        let spans = at.map(|(line, column)| pe_logic::build::RustcSpan { file: "src/lib.rs".into(), line, column });
        RustcMessage {
            level: level.into(),
            code: code.map(Into::into),
            message: message.into(),
            spans: spans.into_iter().collect(),
        }
    }

    /// rustc's parsed messages: errors and placed warnings become diagnostics at their first span, with their codes;
    /// notes, helps, an unplaced warning and the "aborting" summary are left out.
    #[test]
    fn rustcs_messages_become_diagnostics() {
        let ms = [
            msg("error", Some("E0425"), "cannot find value `x` in this scope", Some((3, 5))),
            msg("help", None, "a local variable with a similar name exists", Some((3, 5))),
            msg("warning", Some("unused_variables"), "unused variable: `error_count`", Some((2, 1))),
            msg("warning", None, "1 warning emitted", None),
            msg("error", None, "aborting due to 1 previous error", None),
        ];
        let d = rustc_diagnostics(&ms, "cargo's text");
        let at = |d: &Diagnostic| (d.file.clone().unwrap(), d.line.unwrap(), d.column.unwrap(), d.severity);
        assert_eq!(
            d.iter().map(at).collect::<Vec<_>>(),
            [("src/lib.rs".to_string(), 3, 5, Severity::Error), ("src/lib.rs".to_string(), 2, 1, Severity::Warning)]
        );
        assert_eq!(
            (d[0].code.as_deref(), d[0].message.as_str()),
            (Some("E0425"), "cannot find value `x` in this scope")
        );
        assert_eq!(
            (d[1].code.as_deref(), d[1].message.as_str()),
            (Some("unused_variables"), "unused variable: `error_count`")
        );
    }

    /// With no error among rustc's messages (cargo failed first, a linker's error), the output comes back whole,
    /// trimmed and bounded, as the first diagnostic (@api-eng's Medium on 3b8bf923); an ICE is an error.
    #[test]
    fn output_with_no_error_message_is_one_diagnostic() {
        let out = "  error: linking with `rust-lld` failed: exit code: 1\n  = note: rust-lld: undefined symbol\n";
        let d = rustc_diagnostics(&[], out);
        assert_eq!(d.len(), 1);
        assert_eq!((d[0].kind, d[0].severity, d[0].file.as_deref()), (DiagnosticKind::Rustc, Severity::Error, None));
        assert!(d[0].message.starts_with("error: linking") && d[0].message.ends_with("undefined symbol"), "{d:?}");
        let long = "é".repeat(RAW_MAX);
        assert!(rustc_diagnostics(&[], &long).pop().unwrap().message.len() <= RAW_MAX);
        let ice = [msg("error: internal compiler error", None, "unexpected panic", None)];
        assert_eq!(rustc_diagnostics(&ice, "text")[0].message, "unexpected panic");
    }

    /// A module that won't describe for this machine's own reason holds the vote (@engine-architect's nit on
    /// app-b-r0).
    #[test]
    fn a_module_that_wont_describe_here_holds_the_vote() {
        let held = describe_failed(LoadError::Compile("cranelift ran out of memory".into()));
        let ok = matches!(&held, Err(why) if why.contains("won't describe here") && why.contains("out of memory"));
        assert!(ok, "{held:?}");
        assert!(describe_failed(LoadError::OtherRunner).is_err());
    }

    /// The cache keeps the [`CACHED`] most recently used versions.
    #[test]
    fn the_cache_drops_the_least_recently_used_version() {
        let v = |n: u8| Hash([n; 32]);
        let mut c = Cache::default();
        for n in 0..CACHED as u8 {
            c.insert(v(n), n);
        }
        assert_eq!(c.get(&v(0)), Some(0), "used again");
        c.insert(v(200), 200);
        assert_eq!((c.get(&v(1)), c.get(&v(0)), c.get(&v(200))), (None, Some(0), Some(200)));
        assert_eq!(c.by_v.len(), CACHED);
    }

    #[test]
    fn the_build_folder_is_the_identity_files_own() {
        let dir = |p: &str| build_dir(Path::new(p));
        assert_eq!(dir("data/identity.seed"), Path::new("data/identity.seed.b"));
        assert_eq!(dir("data/alice"), Path::new("data/alice.b"));
        assert_ne!(dir("data/a.seed"), dir("data/b.seed"));
    }
}
