//! The build service's machine side (logic.md 6.2 steps 3-4, 6.2a; M1's n = 2 path, @coordinator-opus;
//! split with @engine-architect, design; api.md 7.1 with @api-eng).
//!
//! - **[`Builder`]** builds a source archive with pe-logic's `build_archive` and describes the module through the
//!   runner's [`ModuleInbox`]: its metered hash and its components under V's ids, as the runner would bind them. It
//!   keeps each result by V, the archive's hash, so a check, the author's commit and the attestation build a version
//!   once (api.md 7.1). One build at a time: callers queue on one lock. The order api.md 7.1 gives (attesting builds
//!   first, then the API's) comes with pe-api's `Builds` port, its second caller.
//! - **[`Attester`]** makes one pass over a world's pending commits (`World::pending`). For each commit this machine
//!   hasn't handled, it builds the version, checks the build against the commit, and loads the module into the runner
//!   through its inbox, since every machine must hold the module at F, the author's included. If this member is one
//!   of the commit's voters and isn't counted yet, the pass also gives the `AttestBuild` to submit.
//!   **The author's own pass loads but attests nothing:** its commit already counts as its attestation, and pe-world
//!   refuses a voter's second attestation as `Invalid` (`op_attest`). Don't "fix" that into a second attestation.
//! - **[`toolchain_and_sdk`]** stands in for the toolchain and SDK hashes until @iter-bench's 6.2b slice B defines
//!   them.
//!
//! A pass and a build block their thread, and a build takes seconds, so neither runs on the simulation thread. The
//! thread that calls them needs [`ModuleInbox::thread_stack`] of native stack: describing and loading a module read
//! its manifest, which runs it.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use pe_logic::build::{BuildError, Toolchain};
use pe_replica::logic::{Described, ModuleInbox};
use pe_seams::{Hash, MemberKey, Version};
use pe_world::edit::{Actor, Op, Transaction};
use pe_world::script::{AssetId, tag_desc};
use pe_world::{BlobRef, PendingCommit};

/// Where the build service builds (logic.md 4.2).
#[derive(Clone, Debug)]
pub struct BuildConfig {
    /// The pinned toolchain, by absolute path (pe-logic's `Toolchain`).
    pub toolchain: Toolchain,
    /// pe-sdk's source folder: `pe_logic::build::engine_sdk()` for the dev engine.
    pub sdk: PathBuf,
    /// The service's own folder. `work` under it is `build_archive`'s work folder, so keep this folder's path short:
    /// logic.md 4.1a's paths are at most 128 bytes below it. `target` is the cargo target folder, kept between builds.
    pub dir: PathBuf,
}

/// A version this machine built (the machine's side of api.md 7.1's `Built`).
#[derive(Clone, Debug, PartialEq)]
pub struct Built {
    /// The module as the build gave it, for the runner's inbox.
    pub wasm: Arc<[u8]>,
    /// Its metered hash (what `CommitScript::author_module` names) and its components under V's ids.
    pub described: Described,
    /// How long the build took: api.md 7.1 applies the author's 15 s margin (Q4 (b)) to it.
    pub build_ms: u32,
}

/// The build service (module doc).
pub struct Builder {
    config: BuildConfig,
    inbox: ModuleInbox,
    ids: (Hash, Hash),
    /// Each version built, by V. Held for the whole of a build: one build at a time.
    built: Mutex<BTreeMap<Hash, Built>>,
}

impl Builder {
    /// A builder that describes modules with `inbox`'s runner, which decides their hashes.
    pub fn new(config: BuildConfig, inbox: ModuleInbox) -> Result<Builder, BuildError> {
        let ids = toolchain_and_sdk(&config.toolchain, &config.sdk)?;
        Ok(Builder { config, inbox, ids, built: Mutex::new(BTreeMap::new()) })
    }

    /// This machine's toolchain and SDK hashes, as a `CommitScript` carries them ([`toolchain_and_sdk`]).
    pub fn ids(&self) -> (Hash, Hash) {
        self.ids
    }

    /// The version `archive` holds (V is its hash), built as the build service builds it, or what an earlier build of
    /// it gave. A failure is not kept: the next call builds again. Blocks for the build, which takes seconds.
    pub fn build(&self, archive: &[u8]) -> Result<Built, String> {
        let v = Hash(BlobRef::of(archive).hash);
        let mut built = self.built.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(b) = built.get(&v) {
            return Ok(b.clone());
        }
        let started = Instant::now();
        let (work, target) = (self.config.dir.join("work"), self.config.dir.join("target"));
        let wasm =
            pe_logic::build::build_archive(&self.config.toolchain, archive, &self.config.sdk, &work, &target, true)
                .map_err(|e| e.to_string())?;
        let build_ms = u32::try_from(started.elapsed().as_millis()).unwrap_or(u32::MAX);
        let described = self.inbox.describe(&wasm, &v).map_err(|e| format!("the built module won't load: {e}"))?;
        let b = Built { wasm: wasm.into(), described, build_ms };
        built.insert(v, b.clone());
        Ok(b)
    }
}

/// The toolchain and SDK hashes a `CommitScript` carries and an attester compares with its own (logic.md 6.2 step 3,
/// D75). A stand-in until @iter-bench's 6.2b slice B defines them, which replaces this body and keeps the signature
/// (@engine-architect): the toolchain's `rustc -vV` with pe-logic's result version, and pe-sdk's sources
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
    Transaction {
        actor: Actor::Person,
        label: None,
        preconditions: Vec::new(),
        ops: vec![Op::AttestBuild { script: asset, version: v, module }],
        blobs: Vec::new(),
    }
}

/// What one [`Attester::pass`] did.
#[derive(Debug, Default)]
pub struct Pass {
    /// The versions whose modules it loaded into the runner, as (asset, V).
    pub loaded: Vec<(AssetId, Hash)>,
    /// The attestations to submit, one transaction each.
    pub attest: Vec<Transaction>,
    /// The commits this machine couldn't build, or whose build differs from the author's, as (asset, V, why).
    pub failed: Vec<(AssetId, Hash, String)>,
}

/// One machine's attester (module doc).
pub struct Attester {
    me: MemberKey,
    /// The commits handled, by asset and commit (its entry and op): a version committed again after a void is a new
    /// commit, built from the cache and attested again.
    done: BTreeSet<(AssetId, Version)>,
}

impl Attester {
    /// The attester of this machine's member `me`.
    pub fn new(me: MemberKey) -> Attester {
        Attester { me, done: BTreeSet::new() }
    }

    /// One pass over `pending`, a world's pending commits (`World::pending`), in asset order (module doc). `blob` gives
    /// a commit's source archive from the world's store. A commit whose archive isn't there yet is tried again at the
    /// next pass; a voided one is skipped. `inbox` is the runner's.
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
            let loaded = check(p, builder, &archive)
                .and_then(|built| inbox.load(&built.wasm).map_err(|e| format!("the module won't load: {e}")));
            if let Err(why) = loaded {
                // TODO(BuildFailed): post `BuildFailed` once @world-model's op lands, and only after the local retries
                // of logic.md 6.2b (`Local` twice, `Mismatch` once from a clean build), since under the
                // build-asymmetry rulings a post can cost this member its session (@engine-architect).
                // Until then a failure is only reported, and the commit voids at D.
                pass.failed.push((asset, v, why));
                continue;
            }
            pass.loaded.push((asset, v));
            let counted = p.attested.iter().any(|(k, _)| *k == self.me);
            if p.voters.contains(&self.me) && !counted {
                pass.attest.push(attestation(asset, v, p.author_module));
            }
        }
        pass
    }
}

/// This machine's build of `p`'s version, checked against the commit (logic.md 6.2 step 3): the same toolchain and
/// SDK, the same module, and the same components, the tag among them.
fn check(p: &PendingCommit, builder: &Builder, archive: &[u8]) -> Result<Built, String> {
    if (p.toolchain, p.sdk) != builder.ids() {
        return Err("the commit names another toolchain or SDK".into());
    }
    let built = builder.build(archive)?;
    if built.described.module != p.author_module {
        return Err(format!(
            "the module differs from the author's: {:?} here, {:?} committed",
            built.described.module, p.author_module
        ));
    }
    let mut ids: Vec<_> = built.described.components.iter().map(|c| c.id()).collect();
    ids.push(tag_desc(&p.v()).id());
    ids.sort();
    if ids != p.components {
        return Err("the module's components differ from the commit's".into());
    }
    Ok(built)
}
