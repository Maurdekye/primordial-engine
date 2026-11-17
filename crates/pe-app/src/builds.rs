//! The build service's machine side (logic.md 6.2 steps 3-4, 6.2a, 6.2b; M1's n = 2 path, @coordinator-opus
//! m303e80fdcd44; split with @engine-architect m668e8458cf86, design m7855d4a97816; api.md 7.1 with @api-eng).
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
//!     machine reports it.
//!   - An API build is the author's: past the author's time margin (`AUTHOR_MAX_BUILD`, 15 s; Q4 (b)) it is refused,
//!     cached or not. The memory margin waits for pe-logic's measure of a build's peak.
//! - **[`Attester`]** makes one pass over a world's pending commits (`World::pending`). For each commit this machine
//!   hasn't handled, it builds and checks the version and loads the module into the runner through its inbox, since
//!   every machine must hold the module at F, the author's included. If this member is one of the commit's voters and
//!   isn't counted yet, the pass gives the `AttestBuild` to submit, or, for a build that failed after its retries, the
//!   `BuildFailed` (6.2a item 2).
//!   **The author's own pass loads but posts nothing:** its commit already counts as its attestation, and pe-world
//!   refuses a voter's second attestation, or a failure after it, as `Invalid`. Don't "fix" that into a post.
//! - **[`toolchain_and_sdk`]** stands in for the toolchain and SDK hashes until @iter-bench's 6.2b slice B defines
//!   them.
//!
//! A pass and a build block their thread, and a build takes seconds, so neither runs on the simulation thread. The
//! thread that calls them needs [`ModuleInbox::thread_stack`] of native stack: describing and loading a module read
//! its manifest, which runs it.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use pe_api::builds::{BuildRefused, Builds, Margin};
use pe_api::queue::{EditQueue, Receipt, Ticket};
use pe_logic::build::{AUTHOR_MAX_BUILD, BuildError, FailReason, Failed, Toolchain};
use pe_logic::meter::RefusalKind;
use pe_logic::runtime::{LoadError, Script};
use pe_replica::logic::{Described, ModuleInbox};
use pe_seams::world_api::{Diagnostic, DiagnosticKind, Severity};
use pe_seams::{BuildFailure, Frame, Hash, MemberKey, Version};
use pe_world::edit::{Actor, Op, Transaction};
use pe_world::script::{AssetId, tag_desc};
use pe_world::{BlobRef, PendingCommit};

/// Where the build service builds (logic.md 4.2), and the author's margin.
#[derive(Clone, Debug)]
pub struct BuildConfig {
    /// The pinned toolchain, by absolute path (pe-logic's `Toolchain`).
    pub toolchain: Toolchain,
    /// pe-sdk's source folder: `pe_logic::build::engine_sdk()` for the dev engine.
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

    /// The crate folder `build_archive` unpacks an archive into, which rustc's paths are relative to.
    fn crate_dir(&self) -> PathBuf {
        self.work().join("script")
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
    fn refused(self, crate_dir: &Path) -> BuildRefused {
        match self {
            BuildFail::Build(e) => match (e.reason(), &e) {
                (FailReason::Compile, BuildError::Rustc { text, .. }) => {
                    BuildRefused::Failed(rustc_diagnostics(text, crate_dir))
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

/// rustc's short-format messages (`src\lib.rs:3:5: error[E0425]: cannot find value ...`, the format pe-logic asks
/// for), one diagnostic each (api.md 7.1; @iter-bench m2704bc958fb8), from `BuildError::Rustc`'s `text`: each
/// diagnostic's short rendering, then cargo's output (pe-logic v11). Lines that aren't one are left out. The file is
/// made relative to `crate_dir`, with `/`. When no line is one (another format, a linker's error), the output itself,
/// trimmed to [`RAW_MAX`] bytes, is the one diagnostic, so the author still sees why (@api-eng's Medium on 3b8bf923).
fn rustc_diagnostics(out: &str, crate_dir: &Path) -> Vec<Diagnostic> {
    let crate_dir = crate_dir.to_string_lossy().replace('\\', "/");
    let parsed: Vec<Diagnostic> = out.lines().filter_map(|l| short_message(l, &crate_dir)).collect();
    if !parsed.is_empty() {
        return parsed;
    }
    let raw = out.trim();
    let mut end = raw.len().min(RAW_MAX);
    while !raw.is_char_boundary(end) {
        end -= 1;
    }
    vec![diagnostic(DiagnosticKind::Rustc, raw[..end].to_string())]
}

fn short_message(line: &str, crate_dir: &str) -> Option<Diagnostic> {
    // The level is the first tag in the line: a warning's own text can hold ": error" (@api-eng's Low 1).
    let levels = [(": error", Severity::Error), (": warning", Severity::Warning)];
    let (i, tag, severity) =
        levels.into_iter().filter_map(|(tag, s)| line.find(tag).map(|i| (i, tag, s))).min_by_key(|&(i, ..)| i)?;
    let (at, rest) = (&line[..i], &line[i + tag.len()..]);
    // Split from the right, so a drive letter's colon stays in the path.
    let mut parts = at.rsplitn(3, ':');
    let (column, number, path) = (parts.next()?, parts.next()?, parts.next()?);
    let (number, column) = (number.parse::<u32>().ok()?, column.parse::<u32>().ok()?);
    let (code, message) = match rest.strip_prefix('[') {
        Some(r) => {
            let end = r.find(']')?;
            (Some(r[..end].to_string()), &r[end + 1..])
        }
        None => (None, rest),
    };
    // Relative to the crate when it lies in it: the build folder's own path may hold a `src/` (@api-eng's Low 2).
    let path = path.replace('\\', "/");
    let file = path.strip_prefix(crate_dir).map_or_else(|| path.clone(), |p| p.trim_start_matches('/').to_string());
    Some(Diagnostic {
        kind: DiagnosticKind::Rustc,
        severity,
        message: message.trim_start_matches(':').trim().to_string(),
        file: Some(file),
        line: Some(number),
        column: Some(column),
        code,
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

    /// The attester's build of a commit's version (logic.md 6.2 step 3, 6.2b): pe-logic's `build_for_row`, whose module
    /// must be `author_module`, with its retries. A build this machine already has with that module answers at once.
    pub fn build_for_row(&self, archive: &[u8], author_module: Hash) -> Result<Built, Failed> {
        let v = version_of(archive);
        let theirs = |b: &Built| b.described.module == author_module;
        if let Some(b) = self.cached(&v).filter(theirs) {
            return Ok(b);
        }
        let _turn = self.gate.attester();
        if let Some(b) = self.cached(&v).filter(theirs) {
            return Ok(b);
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
        )?;
        let build_ms = millis(started.elapsed());
        // A manifest that won't read, or a component that won't take an id, is the module's own fault, the same on
        // every machine (`Refused`); anything else is this machine's (@engine-architect's Low 1 on 6700586b).
        let described = inbox.describe_script(&script, &v).map_err(|e| Failed {
            reason: if matches!(e, LoadError::Refused(_)) { FailReason::Refused } else { FailReason::Local },
            module: None,
            attempts: 1,
            detail: format!("the built module won't describe: {e}"),
        })?;
        let b = Built { script, described, build_ms };
        self.keep(v, &b);
        Ok(b)
    }

    /// A build in a turn the caller holds; one that finished while the caller waited for it answers.
    fn build_in_turn(&self, archive: &[u8], v: Hash) -> Result<Built, BuildFail> {
        if let Some(b) = self.cached(&v) {
            return Ok(b);
        }
        let c = &self.config;
        let started = Instant::now();
        let wasm = pe_logic::build::build_archive(&c.toolchain, archive, &c.sdk, &c.work(), &c.target(), true)
            .map_err(BuildFail::Build)?;
        let build_ms = millis(started.elapsed());
        let script = Script::load(self.inbox.engine(), self.inbox.settings(), &wasm).map_err(BuildFail::Load)?;
        let described = self.inbox.describe_script(&script, &v).map_err(BuildFail::Load)?;
        let b = Built { script, described, build_ms };
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
    /// author's time margin, cached or not. pe-logic's build gives no warnings yet, so none are reported.
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
                built.map_err(|e| e.refused(&self.config.crate_dir()))?
            }
        };
        if u128::from(b.build_ms) > self.config.margin.as_millis() {
            return Err(BuildRefused::Margin { which: Margin::Time, measured: u64::from(b.build_ms) });
        }
        let (toolchain, sdk) = self.ids;
        Ok(pe_api::builds::Built {
            module: b.described.module,
            toolchain,
            sdk,
            components: b.described.components,
            systems: b.described.systems,
            warnings: Vec::new(),
            build_ms: b.build_ms,
            peak_bytes: None,
        })
    }
}

fn version_of(archive: &[u8]) -> Hash {
    Hash(BlobRef::of(archive).hash)
}

fn millis(d: Duration) -> u32 {
    u32::try_from(d.as_millis()).unwrap_or(u32::MAX)
}

/// The toolchain and SDK hashes a `CommitScript` carries and an attester compares with its own (logic.md 6.2 step 3,
/// D75). A stand-in until @iter-bench's 6.2b slice B defines them, which replaces this body and keeps the signature
/// (@engine-architect m668e8458cf86): the toolchain's `rustc -vV` with pe-logic's result version, and pe-sdk's sources
/// packed as a script's are (`pe_logic::build::pack`). Two machines with one toolchain and one engine build give the
/// same pair. The module's hash is the comparison that decides; these two refuse a mismatched build early.
pub fn toolchain_and_sdk(toolchain: &Toolchain, sdk: &Path) -> Result<(Hash, Hash), BuildError> {
    let out = std::process::Command::new(&toolchain.rustc)
        .arg("-vV")
        .output()
        .map_err(|e| BuildError::Toolchain(format!("{} -vV: {e}", toolchain.rustc.display())))?;
    if !out.status.success() {
        return Err(BuildError::Toolchain(format!("{} -vV: {}", toolchain.rustc.display(), out.status)));
    }
    let mut id = pe_logic::RESULT_VERSION.to_le_bytes().to_vec();
    id.extend_from_slice(&out.stdout);
    let tc = Hash::of("pe.app.toolchain.stand-in", &id);
    let sdk = Hash::of("pe.app.sdk.stand-in", &pe_logic::build::pack(sdk)?);
    Ok((tc, sdk))
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
    Transaction { actor: Actor::Person, label: None, preconditions: Vec::new(), ops: vec![op], blobs: Vec::new() }
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
}

impl Attester {
    /// The attester of this machine's member `me`.
    pub fn new(me: MemberKey) -> Attester {
        Attester { me, done: BTreeSet::new(), posted: BTreeMap::new() }
    }

    /// Records the stamp the edit queue gave this machine's attestation of `asset`'s commit `version` (its receipt),
    /// so that [`Attester::reposts`] can tell when its fate is final.
    pub fn posted(&mut self, asset: AssetId, version: Version, stamp: Frame) {
        self.posted.insert((asset, version), stamp);
    }

    /// The attestations to post again (api.md 7.1; @design-reviewer m19210137e1cb). The rule is state-based. Once
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

    /// One pass over `pending`, a world's pending commits (`World::pending`), in asset order (module doc). `blob` gives
    /// a commit's source archive from the world's store. A commit whose archive isn't there yet is tried again at the
    /// next pass; a voided one is skipped. `inbox` is the runner's. A build's retries (6.2b) happen inside the pass, so
    /// a commit is handled once: attested, reported as failed, or, when this machine can't judge it, only logged.
    pub fn pass(
        &mut self,
        pending: &BTreeMap<AssetId, PendingCommit>,
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
            match check(p, builder, &archive) {
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
                Err(Unchecked::Ids) => pass.failed.push((asset, v, "the commit names another toolchain or SDK".into())),
                Err(Unchecked::Failed(f)) => {
                    pass.failed.push((asset, v, format!("{:?} after {} build(s): {}", f.reason, f.attempts, f.detail)));
                    if voter {
                        let reason = reported(f.reason);
                        let tx = build_failed(asset, v, reason, builder.ids(), f.module);
                        pass.failures.push(Failure { asset, version: p.version, reason, tx });
                    }
                }
            }
        }
        pass
    }
}

/// Why a commit's version isn't this machine's to run.
enum Unchecked {
    /// The commit names another toolchain or SDK than this machine's. Until 6.2b slice B defines those hashes this
    /// machine reports nothing for it: its commit voids at D unless the quorum attests without this member.
    Ids,
    /// The build failed after its retries (6.2b), or its module isn't the commit's.
    Failed(Failed),
}

/// This machine's build of `p`'s version, checked against the commit (logic.md 6.2 step 3): the same toolchain and
/// SDK, the same module (pe-logic's `build_for_row`), and the same components, the tag among them.
fn check(p: &PendingCommit, builder: &Builder, archive: &[u8]) -> Result<Built, Unchecked> {
    if (p.toolchain, p.sdk) != builder.ids() {
        return Err(Unchecked::Ids);
    }
    let built = builder.build_for_row(archive, p.author_module).map_err(Unchecked::Failed)?;
    let mut ids: Vec<_> = built.described.components.iter().map(|c| c.id()).collect();
    ids.push(tag_desc(&p.v()).id());
    ids.sort();
    if ids != p.components {
        // One module under one V declares the same components on every machine. With the module the author's, a
        // different list more likely means a commit inconsistent with its own module than a fault here, but M1's
        // members are trusted: after M1 it deserves a reason of its own (@engine-architect's Low 1 on 6700586b).
        let detail = "the module's components differ from the commit's".to_string();
        return Err(Unchecked::Failed(Failed { reason: FailReason::Local, module: None, attempts: 1, detail }));
    }
    Ok(built)
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
        let (mut seen, mut unsent, mut waiting) = (0, Vec::<Post>::new(), Vec::<(Ticket, Post)>::new());
        while !flag.load(Ordering::Relaxed) {
            let (published, snap) = shared.wait_newer(seen, WAIT);
            seen = published;
            let now = snap.frame();
            let pass = attester.pass(snap.pending(), |r| snap.blob(r), &builder, &inbox);
            for (asset, v, why) in &pass.failed {
                eprintln!("pe-app: this machine's build of script {} at {v:?} failed: {why}", short(asset));
            }
            for f in &pass.failures {
                eprintln!(
                    "pe-app: reporting this machine's failed build of script {}: {:?}",
                    short(&f.asset),
                    f.reason
                );
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
                    eprintln!("pe-app: a machine entry of script {} was not submitted: {e:?}", short(post.asset()));
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

    /// rustc's short format, with a drive letter, a code or none, and a warning whose text holds ": error"; other
    /// lines are left out. A path in the crate folder is made relative to it, even when the folder's own path holds a
    /// `src` (@api-eng's Lows on 3b8bf923).
    #[test]
    fn rustcs_messages_become_diagnostics() {
        let out = "C:\\src\\b\\work\\script\\src\\lib.rs:3:5: error[E0425]: cannot find value `x` in this scope\n\
                   src\\lib.rs:1:9: error: expected `;`, found `}`\n\
                   src/lib.rs:2:1: warning: unused variable: error_count\n\
                   error: could not compile `script` (lib) due to 2 previous errors\n";
        let d = rustc_diagnostics(out, Path::new("C:\\src\\b\\work\\script"));
        let at = |d: &Diagnostic| (d.file.clone().unwrap(), d.line.unwrap(), d.column.unwrap(), d.severity);
        assert_eq!(
            d.iter().map(at).collect::<Vec<_>>(),
            [
                ("src/lib.rs".to_string(), 3, 5, Severity::Error),
                ("src/lib.rs".to_string(), 1, 9, Severity::Error),
                ("src/lib.rs".to_string(), 2, 1, Severity::Warning),
            ]
        );
        assert_eq!(
            (d[0].code.as_deref(), d[0].message.as_str()),
            (Some("E0425"), "cannot find value `x` in this scope")
        );
        assert_eq!((d[1].code.as_deref(), d[1].message.as_str()), (None, "expected `;`, found `}`"));
        assert_eq!(d[2].message, "unused variable: error_count");
    }

    /// Output with no short-format line comes back whole, trimmed and bounded, as one diagnostic (@api-eng's Medium on
    /// 3b8bf923).
    #[test]
    fn output_with_no_short_format_line_is_one_diagnostic() {
        let out = "  error: linking with `rust-lld` failed: exit code: 1\n  = note: rust-lld: undefined symbol\n";
        let d = rustc_diagnostics(out, Path::new("w/script"));
        assert_eq!(d.len(), 1);
        assert_eq!((d[0].kind, d[0].severity, d[0].file.as_deref()), (DiagnosticKind::Rustc, Severity::Error, None));
        assert!(d[0].message.starts_with("error: linking") && d[0].message.ends_with("undefined symbol"), "{d:?}");
        let long = "é".repeat(RAW_MAX);
        assert!(rustc_diagnostics(&long, Path::new("w")).pop().unwrap().message.len() <= RAW_MAX);
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
