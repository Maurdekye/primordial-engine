//! The build service's machine side (src/builds.rs): a voter builds a pending version, checks it against the commit,
//! loads it into its runner and attests once; the author loads and attests nothing; a build that fails after its
//! retries is reported with `BuildFailed`, and one this machine can't judge is only logged; a commit that is voided or
//! already counted gets nothing. pe-api's build port answers as the author's build. With pe-logic's lamp, built for
//! real.

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use bytes::Bytes;
use pe_api::builds::{BuildRefused, Builds, Margin};
use pe_app::api::{Facts, Ports};
use pe_app::builds::{
    Attest, Attester, BuildConfig, Builder, Built, Failure, attestation, build_failed, spawn_attester,
};
use pe_logic::build::Toolchain;
use pe_replica::logic::{LogicRunner, ModuleInbox};
use pe_seams::test_util::{EntryBuilder, TestKeys};
use pe_seams::world_api::{
    Caller, CodeCheck, ConnectionId, DiagnosticKind, MemberId, Producer, ProducerKind, Scope, Severity,
};
use pe_seams::{BuildFailure, Class, Frame, Hash, LineId, Outcome, SimRate};
use pe_world::edit::{Actor, AssetTarget, Op, Transaction};
use pe_world::script::tag_desc;
use pe_world::{BlobRef, BlobStore, World, perm};

/// The lamp's asset: the id A's commit creates as `New(0)`, A's first entry (world-model.md 7.9).
fn lamp() -> [u8; 32] {
    static L: OnceLock<[u8; 32]> = OnceLock::new();
    *L.get_or_init(|| {
        let e = EntryBuilder::new(keys(), 0).next(Class::Edit, Frame(0), Bytes::new()).entry;
        pe_world::script::asset_created(e.line, e.author, e.chain, e.seq, 0)
    })
}

fn keys() -> &'static TestKeys {
    static K: OnceLock<TestKeys> = OnceLock::new();
    K.get_or_init(|| TestKeys::new(5, 2))
}

fn lamp_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../pe-logic/tests/scripts/lamp")
}

/// The lamp's source archive, as its author commits it (`pe_logic::build::pack`).
fn archive() -> &'static [u8] {
    static A: OnceLock<Vec<u8>> = OnceLock::new();
    A.get_or_init(|| pe_logic::build::pack(&lamp_dir()).unwrap_or_else(|e| panic!("{e}")))
}

fn v() -> Hash {
    Hash(BlobRef::of(archive()).hash)
}

/// A source that doesn't compile: rustc's error is at line 1.
fn broken() -> Vec<u8> {
    pe_world::script::pack_source(&[("src/lib.rs", b"pub fn broken( {}\n".as_slice())]).expect("an archive")
}

/// A build folder of the test's own: tests run at once, and a build rewrites its work folder.
fn config(test: &str) -> BuildConfig {
    BuildConfig::new(
        Toolchain::current().unwrap_or_else(|e| panic!("{e}")),
        pe_logic::build::engine_sdk(),
        PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("b").join(test),
    )
}

/// A builder over a fresh runner's inbox, and that inbox.
fn builder(test: &str) -> (Builder, ModuleInbox) {
    let inbox = LogicRunner::with_defaults().expect("a runner").inbox();
    (Builder::new(config(test), inbox.clone()).expect("a builder"), inbox)
}

/// Runs `f` on a thread with the stack describing and loading a module need.
fn big_stack(f: impl FnOnce() + Send + 'static) {
    let stack = pe_logic::Settings::default().thread_stack();
    std::thread::Builder::new().stack_size(stack).spawn(f).unwrap().join().unwrap();
}

fn tx(ops: Vec<Op>) -> Transaction {
    Transaction { inline: vec![], actor: Actor::Person, label: None, preconditions: vec![], ops, blobs: vec![] }
}

/// A world of two editors, A (key 0) and B (key 1), where A has committed `source` at frame 0 with its tag and the
/// components `built` describes (none without), and the toolchain, SDK and module hashes `ids` and `module`: pending,
/// A's attestation the commit's own.
fn commit_world(source: &[u8], built: Option<&Built>, ids: (Hash, Hash), module: Hash) -> World {
    let mut w = World::new(1, BlobStore::default());
    for k in 0..2 {
        w.set_member(keys().key(k), perm::EDIT);
    }
    let mut descs = vec![tag_desc(&Hash(BlobRef::of(source).hash))];
    descs.extend(built.iter().flat_map(|b| b.described.components.iter().cloned()));
    let commit = tx(vec![
        Op::Register { components: descs },
        Op::commit_script(AssetTarget::New(0), None, Bytes::copy_from_slice(source), ids.0, ids.1, module, 0),
    ]);
    let mut a = EntryBuilder::new(keys(), 0);
    let entry = a.next(Class::Edit, Frame(0), Bytes::from(commit.encode()));
    let (outcome, _) = w.apply_edit(Frame(0), &entry);
    assert!(matches!(outcome, Outcome::Applied { .. }), "the commit: {outcome:?}");
    assert_eq!(w.pending()[&lamp()].attested.len(), 1, "A's own");
    w
}

/// The lamp committed as its build gave it, with `ids` and `module`.
fn committed(built: &Built, ids: (Hash, Hash), module: Hash) -> World {
    commit_world(archive(), Some(built), ids, module)
}

/// The bug bundle's folder a pass's log line names.
fn bundle_in(why: &str) -> PathBuf {
    let at = why.split("bug bundle (local, see report.txt): ").nth(1).unwrap_or_else(|| panic!("no bundle: {why}"));
    PathBuf::from(at.lines().next().unwrap())
}

fn blob_of(w: &World) -> impl Fn(&BlobRef) -> Option<Arc<[u8]>> + '_ {
    |r| w.blobs().get(r)
}

/// B, a voter, builds the version, finds it the author's, loads it into its runner and gives one `AttestBuild` with
/// the commit's V and module. A second pass does nothing.
#[test]
fn a_voter_builds_checks_loads_and_attests_once() {
    big_stack(|| {
        let (builder, inbox) = builder("voter");
        let built = builder.build(archive()).expect("the lamp builds");
        let w = committed(&built, builder.ids(), built.described.module);
        let mut b = Attester::new(keys().key(1));
        let pass = b.pass(w.pending(), w.modules(), blob_of(&w), &builder, &inbox);
        assert!(pass.failed.is_empty() && pass.failures.is_empty(), "{pass:?}");
        assert_eq!(pass.loaded, [(lamp(), v())]);
        let tx = attestation(lamp(), v(), built.described.module);
        assert_eq!(pass.attest, [Attest { asset: lamp(), version: w.pending()[&lamp()].version, tx }]);
        let again = b.pass(w.pending(), w.modules(), blob_of(&w), &builder, &inbox);
        assert!(again.loaded.is_empty() && again.attest.is_empty() && again.failed.is_empty(), "{again:?}");
    });
}

/// The author loads its own module, since it too must hold it at F, but gives no attestation: its commit counts as
/// one, and pe-world refuses a second as `Invalid`.
#[test]
fn the_author_loads_but_does_not_attest() {
    big_stack(|| {
        let (builder, inbox) = builder("author");
        let built = builder.build(archive()).expect("the lamp builds");
        let w = committed(&built, builder.ids(), built.described.module);
        let pass = Attester::new(keys().key(0)).pass(w.pending(), w.modules(), blob_of(&w), &builder, &inbox);
        assert_eq!((pass.loaded, pass.attest.len(), pass.failed.len()), (vec![(lamp(), v())], 0, 0));
        assert!(pass.failures.is_empty());
    });
}

/// `w` after the author's prediction of its own pending lamp commit (logic.md 6.2a's option (i)): activated at the end
/// of its frame 0, which removes the pending record, as on the author's machine before any snapshot is published.
fn predicted(mut w: World) -> World {
    let _ = w.end_frame(Frame(0));
    assert!(matches!(w.activate_for_prediction(lamp(), Frame(0)), Some(Ok(_))), "the prediction activates it");
    assert!(w.pending().is_empty() && w.modules().contains_key(&lamp()), "active, no longer pending");
    w
}

/// The author never sees its own commit pending: its prediction activates it in the commit's own frame. The pass loads
/// the module as an active version, from the author's own build, and attests nothing; a second pass does nothing.
/// Before, the author's runner never held its own module
/// (@item:primordial-engine/m1-blocker-binding-an-activated-script-to-an-obj).
#[test]
fn the_author_loads_its_own_commit_that_its_prediction_activated() {
    big_stack(|| {
        let (builder, inbox) = builder("author-predicted");
        let built = builder.build(archive()).expect("the lamp builds");
        let w = predicted(committed(&built, builder.ids(), built.described.module));
        let mut a = Attester::new(keys().key(0));
        let pass = a.pass(w.pending(), w.modules(), blob_of(&w), &builder, &inbox);
        assert!(pass.failed.is_empty() && pass.failures.is_empty(), "{pass:?}");
        assert_eq!((pass.loaded, pass.attest.len()), (vec![(lamp(), v())], 0), "loaded, not attested");
        let again = a.pass(w.pending(), w.modules(), blob_of(&w), &builder, &inbox);
        assert!(again.loaded.is_empty() && again.failed.is_empty(), "loaded once: {again:?}");
    });
}

/// A version is loaded once: a voter that handled it pending (loaded and attested) does nothing more once it is active,
/// and one that only ever sees it active loads it and attests nothing, since an active version needs no attestation.
#[test]
fn an_active_version_is_loaded_once_and_never_attested() {
    big_stack(|| {
        let (builder, inbox) = builder("active");
        let built = builder.build(archive()).expect("the lamp builds");
        let w = committed(&built, builder.ids(), built.described.module);
        let mut seen = Attester::new(keys().key(1));
        let first = seen.pass(w.pending(), w.modules(), blob_of(&w), &builder, &inbox);
        assert_eq!((first.loaded.len(), first.attest.len()), (1, 1), "pending: loaded and attested: {first:?}");
        let a = predicted(committed(&built, builder.ids(), built.described.module));
        let later = seen.pass(a.pending(), a.modules(), blob_of(&a), &builder, &inbox);
        assert!(later.loaded.is_empty() && later.attest.is_empty(), "handled while pending: {later:?}");
        let late = Attester::new(keys().key(1)).pass(a.pending(), a.modules(), blob_of(&a), &builder, &inbox);
        assert!(late.failed.is_empty() && late.failures.is_empty(), "{late:?}");
        assert_eq!((late.loaded, late.attest.len()), (vec![(lamp(), v())], 0), "active only: loaded, not attested");
    });
}

/// A script this machine can't run is reported (S6's `scripts_not_running`; @coordinator-opus): an
/// active version whose module isn't loaded here, with why, until a pass loads it. One with another build identity
/// stays reported, with that reason.
#[test]
fn an_active_version_not_loaded_here_is_reported_until_it_is() {
    big_stack(|| {
        let (builder, inbox) = builder("not-running");
        let built = builder.build(archive()).expect("the lamp builds");
        let w = predicted(committed(&built, builder.ids(), built.described.module));
        let mut a = Attester::new(keys().key(0));
        let before = a.not_running(w.modules());
        assert_eq!(before.iter().map(|n| n.0).collect::<Vec<_>>(), [lamp()], "{before:?}");
        assert!(before[0].2.contains("isn't built"), "{before:?}");
        a.pass(w.pending(), w.modules(), blob_of(&w), &builder, &inbox);
        assert!(a.not_running(w.modules()).is_empty(), "loaded");
        let other = predicted(committed(&built, (Hash([7; 32]), builder.ids().1), built.described.module));
        let mut b = Attester::new(keys().key(1));
        b.pass(other.pending(), other.modules(), blob_of(&other), &builder, &inbox);
        let why = b.not_running(other.modules());
        assert_eq!(why.iter().map(|n| n.0).collect::<Vec<_>>(), [lamp()], "{why:?}");
        assert!(why[0].2.contains("another build identity"), "{why:?}");
    });
}

/// A commit whose module differs from this machine's build, after the clean rebuild 6.2b asks for, is neither loaded
/// nor attested: the voter reports `BuildFailed` with `Mismatch` and the module it built. One whose toolchain and SDK
/// differ from this machine's is only logged: this machine can't judge it.
#[test]
fn a_build_that_differs_from_the_commit_is_reported_and_not_attested() {
    big_stack(|| {
        let (builder, inbox) = builder("differs");
        let built = builder.build(archive()).expect("the lamp builds");
        let mine = built.described.module;
        let w = committed(&built, builder.ids(), Hash([9; 32]));
        let pass = Attester::new(keys().key(1)).pass(w.pending(), w.modules(), blob_of(&w), &builder, &inbox);
        assert!(pass.loaded.is_empty() && pass.attest.is_empty(), "{pass:?}");
        let tx = build_failed(lamp(), v(), BuildFailure::Mismatch, builder.ids(), Some(mine));
        let version = w.pending()[&lamp()].version;
        assert_eq!(pass.failures, [Failure { asset: lamp(), version, reason: BuildFailure::Mismatch, tx }]);
        assert!(
            matches!(&pass.failed[..], [(a, f, why)] if *a == lamp() && *f == v() && why.contains("Mismatch after 2")),
            "{pass:?}"
        );
        // A Mismatch means 6.2b's prevention has a gap: the bug bundle is saved, locally, and the log says where.
        let bundle = bundle_in(&pass.failed[0].2);
        assert!(bundle.starts_with(config("differs").dir.join("bugs")), "{}", bundle.display());
        let report = std::fs::read_to_string(bundle.join("report.txt")).expect("the bundle's report");
        assert!(report.contains("reason: Mismatch") && bundle.join("module.wasm").is_file(), "{report}");

        let w = committed(&built, (Hash([7; 32]), builder.ids().1), mine);
        let pass = Attester::new(keys().key(1)).pass(w.pending(), w.modules(), blob_of(&w), &builder, &inbox);
        assert!(pass.loaded.is_empty() && pass.attest.is_empty() && pass.failures.is_empty(), "{pass:?}");
        let why = "another build identity";
        assert!(matches!(&pass.failed[..], [(a, f, w)] if *a == lamp() && *f == v() && w.contains(why)), "{pass:?}");
    });
}

/// A source that doesn't compile is reported at once by a voter, with `Compile` and no module: rustc's error is the
/// same on every attempt and every machine, so it isn't retried (6.2b). The author, already counted, reports nothing.
#[test]
fn a_voter_reports_a_compile_error_at_once() {
    big_stack(|| {
        let (builder, inbox) = builder("compile");
        let source = broken();
        let w = commit_world(&source, None, builder.ids(), Hash([5; 32]));
        let pass = Attester::new(keys().key(1)).pass(w.pending(), w.modules(), blob_of(&w), &builder, &inbox);
        let v = Hash(BlobRef::of(&source).hash);
        let tx = build_failed(lamp(), v, BuildFailure::Compile, builder.ids(), None);
        let version = w.pending()[&lamp()].version;
        assert_eq!(pass.failures, [Failure { asset: lamp(), version, reason: BuildFailure::Compile, tx }]);
        assert!(matches!(&pass.failed[..], [(_, _, why)] if why.contains("Compile after 1")), "{pass:?}");
        // The author's commit counts as its attestation, so a compile error here is a gap too: bundled, rustc's output
        // in its report.
        let report = std::fs::read_to_string(bundle_in(&pass.failed[0].2).join("report.txt")).expect("a report");
        assert!(report.contains("reason: Compile") && report.contains("src/lib.rs"), "{report}");
        let author = Attester::new(keys().key(0)).pass(w.pending(), w.modules(), blob_of(&w), &builder, &inbox);
        assert!(author.failures.is_empty() && author.attest.is_empty(), "{author:?}");
    });
}

/// A commit whose components aren't its module's is the same on every machine: a voter reports `Refused`, not `Local`
/// (6.2a item 2), and, since the author built the version, saves a bug bundle: the same module describing differently
/// on two machines would be a prevention gap.
#[test]
fn components_that_differ_from_the_modules_are_refused() {
    big_stack(|| {
        let (builder, inbox) = builder("components");
        let built = builder.build(archive()).expect("the lamp builds");
        let w = commit_world(archive(), None, builder.ids(), built.described.module);
        let pass = Attester::new(keys().key(1)).pass(w.pending(), w.modules(), blob_of(&w), &builder, &inbox);
        let tx = build_failed(lamp(), v(), BuildFailure::Refused, builder.ids(), None);
        let version = w.pending()[&lamp()].version;
        assert_eq!(pass.failures, [Failure { asset: lamp(), version, reason: BuildFailure::Refused, tx }]);
        let report = std::fs::read_to_string(bundle_in(&pass.failed[0].2).join("report.txt")).expect("a report");
        assert!(report.contains("reason: Refused") && report.contains("components differ"), "{report}");
        // The lamp came from the cache (built above), so the report says its dep-info may be another version's.
        assert!(report.contains("deps/ are the last build's"), "{report}");
    });
}

/// The toolchain hash a commit carries is the build identity B (logic.md 6.2b), the one pe-app's session compares at
/// join: the build id's scripts part is B's first 6 bytes in hex (net.rs `build_id`).
#[test]
fn the_commits_toolchain_hash_is_the_build_identity() {
    let (builder, _) = builder("identity");
    let tc = Toolchain::current().unwrap_or_else(|e| panic!("{e}"));
    let b = pe_logic::build::build_identity(&tc, &pe_logic::build::engine_sdk()).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(builder.ids().0, b);
    let hex: String = b.0[..6].iter().map(|x| format!("{x:02x}")).collect();
    let id = String::from_utf8(pe_app::net::build_id().to_vec()).unwrap();
    assert!(id.split("; ").any(|part| part == format!("scripts {hex}")), "{id}");
}

/// A voter already counted loads the module but attests nothing; a voided commit is skipped.
#[test]
fn a_counted_voter_or_a_voided_commit_gets_no_attestation() {
    big_stack(|| {
        let (builder, inbox) = builder("counted");
        let built = builder.build(archive()).expect("the lamp builds");
        let module = built.described.module;
        let mut w = committed(&built, builder.ids(), module);
        let mut b = EntryBuilder::new(keys(), 1);
        let attest = b.next(Class::Edit, Frame(0), Bytes::from(attestation(lamp(), v(), module).encode()));
        assert!(matches!(w.apply_edit(Frame(0), &attest).0, Outcome::Applied { .. }));
        let pass = Attester::new(keys().key(1)).pass(w.pending(), w.modules(), blob_of(&w), &builder, &inbox);
        assert_eq!((pass.loaded, pass.attest.len()), (vec![(lamp(), v())], 0));

        let mut w = committed(&built, builder.ids(), module);
        let deadline = w.pending()[&lamp()].deadline;
        for f in 0..=deadline.0 {
            let _ = w.end_frame(Frame(f));
        }
        assert!(w.pending()[&lamp()].void.is_some(), "unattested at D");
        let pass = Attester::new(keys().key(1)).pass(w.pending(), w.modules(), blob_of(&w), &builder, &inbox);
        assert!(pass.loaded.is_empty() && pass.attest.is_empty() && pass.failed.is_empty(), "{pass:?}");
    });
}

/// A posted attestation that the final state still lacks, once the final frame has passed its stamp, is posted again
/// while D is ahead (it was voided or dropped, whatever the cause). One not yet decided, one counted, and one past D
/// are not.
#[test]
fn an_attestation_the_final_state_lacks_is_posted_again_while_d_is_ahead() {
    big_stack(|| {
        let (builder, inbox) = builder("repost");
        let built = builder.build(archive()).expect("the lamp builds");
        let module = built.described.module;
        let mut w = committed(&built, builder.ids(), module);
        let (version, deadline) = (w.pending()[&lamp()].version, w.pending()[&lamp()].deadline);
        let mut b = Attester::new(keys().key(1));
        assert_eq!(b.pass(w.pending(), w.modules(), blob_of(&w), &builder, &inbox).attest.len(), 1);
        b.posted(lamp(), version, Frame(3));
        assert!(b.reposts(Frame(5), Frame(3), w.pending()).is_empty(), "frame 3 isn't final yet");
        let again = Attest { asset: lamp(), version, tx: attestation(lamp(), v(), module) };
        assert_eq!(b.reposts(Frame(5), Frame(4), w.pending()), std::slice::from_ref(&again));
        assert!(b.reposts(Frame(6), Frame(5), w.pending()).is_empty(), "forgotten until the new receipt");
        b.posted(lamp(), version, Frame(4));
        assert!(b.reposts(deadline, Frame(5), w.pending()).is_empty(), "D has come");
        b.posted(lamp(), version, Frame(4));
        let mut voter = EntryBuilder::new(keys(), 1);
        let entry = voter.next(Class::Edit, Frame(4), Bytes::from(again.tx.encode()));
        assert!(matches!(w.apply_edit(Frame(4), &entry).0, Outcome::Applied { .. }));
        assert!(b.reposts(Frame(6), Frame(5), w.pending()).is_empty(), "counted");
        assert!(b.reposts(Frame(6), Frame(5), w.pending()).is_empty(), "and forgotten");
    });
}

/// The build port (api.md 7.1) answers as the author's build: the module, this machine's toolchain and SDK, the
/// components and systems, from the cache the attester shares. rustc's error comes back as its diagnostics. A build
/// past the author's time margin is refused as such, cached or not.
#[test]
fn the_build_port_answers_as_the_authors_build() {
    big_stack(|| {
        let (builder, inbox) = builder("port");
        let built = builder.build(archive()).expect("the lamp builds");
        let port: &dyn Builds = &builder;
        let deadline = Instant::now() + Duration::from_secs(300);
        let api = port.build(archive(), deadline).expect("the lamp passes");
        assert_eq!((api.module, api.toolchain, api.sdk), (built.described.module, builder.ids().0, builder.ids().1));
        let mut want = built.described.components.clone();
        want.push(tag_desc(&v()));
        assert_eq!((&api.components, api.build_ms, api.peak_bytes), (&want, built.build_ms, None));
        // What pe-api's `code` op registers under the commit is exactly these, and pe-world takes the commit.
        let mut w = World::new(1, BlobStore::default());
        w.set_member(keys().key(0), perm::EDIT);
        let (ids, module) = ((api.toolchain, api.sdk), api.module);
        let commit = tx(vec![
            Op::Register { components: api.components.clone() },
            Op::commit_script(AssetTarget::New(0), None, Bytes::copy_from_slice(archive()), ids.0, ids.1, module, 0),
        ]);
        let entry = EntryBuilder::new(keys(), 0).next(Class::Edit, Frame(0), Bytes::from(commit.encode()));
        let (outcome, _) = w.apply_edit(Frame(0), &entry);
        assert!(matches!(outcome, Outcome::Applied { .. }), "the port's commit: {outcome:?}");
        assert!(!api.systems.is_empty() && api.warnings.is_empty(), "{:?}", api.systems);

        let Err(BuildRefused::Failed(d)) = port.build(&broken(), deadline) else { panic!("rustc's error") };
        assert!(!d.is_empty() && d.iter().all(|d| d.kind == DiagnosticKind::Rustc), "{d:?}");
        assert_eq!((d[0].file.as_deref(), d[0].line), (Some("src/lib.rs"), Some(1)), "{d:?}");

        // rustc's warnings about the script's own source come back on a passing build, at their place.
        let lamp = std::fs::read_to_string(lamp_dir().join("src/lib.rs")).expect("the lamp's source");
        let line = lamp.lines().count() as u32 + 3;
        let noisy = format!(
            "{lamp}
pub fn noisy() -> u32 {{
    let unused_here = 7;
    1
}}
"
        );
        let api = port.build(&pe_api::builds::archive_of(&noisy).expect("an archive"), deadline).expect("it passes");
        let w = &api.warnings[..];
        assert!(
            matches!(w, [w] if w.severity == Severity::Warning && w.code.as_deref() == Some("unused_variables")
            && (w.file.as_deref(), w.line) == (Some("src/lib.rs"), Some(line))),
            "{w:?}"
        );

        // The same folder, now free: a builder whose margin no build meets.
        let strict = Builder::new(BuildConfig { margin: Duration::ZERO, ..config("port") }, inbox).expect("a builder");
        let refused = Builds::build(&strict, archive(), deadline);
        assert!(
            matches!(refused, Err(BuildRefused::Margin { which: Margin::Time, measured }) if measured > 0),
            "{refused:?}"
        );
    });
}

/// pe-api's `code_check` through pe-app's ports builds with this machine's build service: the lamp's source checks out
/// with its module and systems.
#[test]
fn code_check_builds_through_the_ports_build_service() {
    big_stack(|| {
        let (builder, _) = builder("check");
        let me = keys().key(1);
        let facts = Facts { world_name: "test".into(), line: LineId::origin(), rate: SimRate::DEFAULT, me: Some(me) };
        let w = World::new(1, BlobStore::default());
        let builder = Arc::new(builder);
        let ports = Ports::new(w.snapshot(Frame(0)), facts, || {}).with_builds(builder.clone());
        let caller = Caller {
            connection: ConnectionId(1),
            principal: "test".into(),
            member: MemberId::from(me),
            scopes: [Scope::Read, Scope::Edit].into(),
            producer: Producer { kind: ProducerKind::Agent, harness: None, principal: None, requested_by: None },
            confirm: false,
        };
        let source = std::fs::read_to_string(lamp_dir().join("src/lib.rs")).expect("the lamp's source");
        let d = ports.api().code_check(&caller, &CodeCheck { source: source.clone() }).expect("an answer");
        assert!(d.ok, "{d:?}");
        let archive = pe_api::builds::archive_of(&source).expect("an archive");
        let module = builder.build(&archive).expect("cached").described.module;
        assert_eq!((d.module.clone(), d.systems.is_empty()), (Some(module.to_string()), false), "{d:?}");
    });
}

/// The attester's thread over pe-app's ports: from the newest snapshot (the frame-0 one here, with A's commit
/// pending), B's thread builds, loads, and submits exactly one attestation through pe-api's machine lane. Stopping it
/// ends the thread.
#[test]
fn the_attesters_thread_submits_through_the_machine_lane() {
    big_stack(|| {
        let (builder, inbox) = builder("thread");
        let built = builder.build(archive()).expect("the lamp builds");
        let w = committed(&built, builder.ids(), built.described.module);
        let me = keys().key(1);
        let facts = Facts { world_name: "test".into(), line: LineId::origin(), rate: SimRate::DEFAULT, me: Some(me) };
        let ports = Ports::new(w.snapshot(Frame(0)), facts, || {});
        let thread = spawn_attester(Arc::new(builder), inbox, ports.shared.clone(), ports.queue.clone(), me)
            .expect("the attester's thread");
        let deadline = Instant::now() + Duration::from_secs(60);
        let mut drained = Vec::new();
        while drained.is_empty() && Instant::now() < deadline {
            drained = ports.queue.drain(1);
            std::thread::sleep(Duration::from_millis(20));
        }
        let txs: Vec<_> = drained.iter().map(|(_, sub)| sub.tx.clone()).collect();
        assert_eq!(txs, [attestation(lamp(), v(), built.described.module)]);
        std::thread::sleep(Duration::from_millis(600));
        assert!(ports.queue.drain(1).is_empty(), "submitted once");
        thread.stop();
    });
}
