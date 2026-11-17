//! The build service's machine side (src/builds.rs): a voter builds a pending version, checks it against the commit,
//! loads it into its runner and attests once; the author loads and attests nothing; a build that differs, or a commit
//! that is voided or already counted, gets no attestation. With pe-logic's lamp, built for real.

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use bytes::Bytes;
use pe_app::api::{Facts, Ports};
use pe_app::builds::{Attest, Attester, BuildConfig, Builder, Built, attestation, spawn_attester};
use pe_logic::build::Toolchain;
use pe_replica::logic::{LogicRunner, ModuleInbox};
use pe_seams::test_util::{EntryBuilder, TestKeys};
use pe_seams::{Class, Frame, Hash, LineId, Outcome, SimRate};
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

/// The lamp's source archive, as its author commits it (`pe_logic::build::pack`).
fn archive() -> &'static [u8] {
    static A: OnceLock<Vec<u8>> = OnceLock::new();
    A.get_or_init(|| {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../pe-logic/tests/scripts/lamp");
        pe_logic::build::pack(&dir).unwrap_or_else(|e| panic!("{e}"))
    })
}

fn v() -> Hash {
    Hash(BlobRef::of(archive()).hash)
}

/// A build folder of the test's own: tests run at once, and a build rewrites its work folder.
fn config(test: &str) -> BuildConfig {
    BuildConfig {
        toolchain: Toolchain::current().unwrap_or_else(|e| panic!("{e}")),
        sdk: pe_logic::build::engine_sdk(),
        dir: PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("b").join(test),
    }
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
    Transaction { actor: Actor::Person, label: None, preconditions: vec![], ops, blobs: vec![] }
}

/// A world of two editors, A (key 0) and B (key 1), where A has committed the lamp at frame 0 as its build gave it,
/// with the toolchain, SDK and module hashes `ids` and `module`: pending, A's attestation the commit's own.
fn committed(built: &Built, ids: (Hash, Hash), module: Hash) -> World {
    let mut w = World::new(1, BlobStore::default());
    for k in 0..2 {
        w.set_member(keys().key(k), perm::EDIT);
    }
    let mut components = vec![tag_desc(&v())];
    components.extend(built.described.components.iter().cloned());
    let commit = tx(vec![
        Op::Register { components },
        Op::commit_script(AssetTarget::New(0), None, Bytes::from_static(archive()), ids.0, ids.1, module, 0),
    ]);
    let mut a = EntryBuilder::new(keys(), 0);
    let entry = a.next(Class::Edit, Frame(0), Bytes::from(commit.encode()));
    let (outcome, _) = w.apply_edit(Frame(0), &entry);
    assert!(matches!(outcome, Outcome::Applied { .. }), "the commit: {outcome:?}");
    assert_eq!(w.pending()[&lamp()].attested.len(), 1, "A's own");
    w
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
        let pass = b.pass(w.pending(), blob_of(&w), &builder, &inbox);
        assert!(pass.failed.is_empty(), "{:?}", pass.failed);
        assert_eq!(pass.loaded, [(lamp(), v())]);
        let tx = attestation(lamp(), v(), built.described.module);
        assert_eq!(pass.attest, [Attest { asset: lamp(), version: w.pending()[&lamp()].version, tx }]);
        let again = b.pass(w.pending(), blob_of(&w), &builder, &inbox);
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
        let pass = Attester::new(keys().key(0)).pass(w.pending(), blob_of(&w), &builder, &inbox);
        assert_eq!((pass.loaded, pass.attest.len(), pass.failed.len()), (vec![(lamp(), v())], 0, 0));
    });
}

/// A commit whose module, or whose toolchain and SDK, differ from this machine's build is neither loaded nor
/// attested, and the pass says why.
#[test]
fn a_build_that_differs_from_the_commit_is_not_attested() {
    big_stack(|| {
        let (builder, inbox) = builder("differs");
        let built = builder.build(archive()).expect("the lamp builds");
        let cases = [
            ((builder.ids(), Hash([9; 32])), "the module differs"),
            (((Hash([7; 32]), builder.ids().1), built.described.module), "another toolchain or SDK"),
        ];
        for ((ids, module), why) in cases {
            let w = committed(&built, ids, module);
            let pass = Attester::new(keys().key(1)).pass(w.pending(), blob_of(&w), &builder, &inbox);
            assert!(pass.loaded.is_empty() && pass.attest.is_empty(), "{pass:?}");
            assert!(
                matches!(&pass.failed[..], [(a, f, w)] if *a == lamp() && *f == v() && w.contains(why)),
                "{pass:?}"
            );
        }
    });
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
        let pass = Attester::new(keys().key(1)).pass(w.pending(), blob_of(&w), &builder, &inbox);
        assert_eq!((pass.loaded, pass.attest.len()), (vec![(lamp(), v())], 0));

        let mut w = committed(&built, builder.ids(), module);
        let deadline = w.pending()[&lamp()].deadline;
        for f in 0..=deadline.0 {
            let _ = w.end_frame(Frame(f));
        }
        assert!(w.pending()[&lamp()].void.is_some(), "unattested at D");
        let pass = Attester::new(keys().key(1)).pass(w.pending(), blob_of(&w), &builder, &inbox);
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
        assert_eq!(b.pass(w.pending(), blob_of(&w), &builder, &inbox).attest.len(), 1);
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

/// The attester's thread over pe-app's ports: from the newest snapshot (the frame-0 one here, with A's commit
/// pending), B's thread builds, loads, and submits exactly one attestation through pe-api's edit queue. Stopping it
/// ends the thread.
#[test]
fn the_attesters_thread_submits_through_the_edit_queue() {
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
