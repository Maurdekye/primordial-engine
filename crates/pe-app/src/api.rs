//! pe-api's ports in pe-app (app.md 3.1; api.md 3 and 9): [`ApiWorld`], pe-api's `WorldSource` over what the
//! simulation thread publishes, and [`Ports`], what the simulation thread feeds it through ([`crate::sim::Sim::with_api`]).
//!
//! - **Snapshots.** `latest()` and `at(f)` come from the snapshots the simulation thread keeps by frame for the last
//!   2 s ([`KEEP`]), each taken with a new view. The kept ones above an advance's `rolled_back_to` are dropped, and all
//!   of them on a repair, so none belongs to a timeline the replica replaced. `final_()` is the replica's agreed
//!   snapshot, the world at `Cv`, exact and never lagging; before the first agreed cut, the frame-0 snapshot.
//! - **Edits.** The simulation thread drains pe-api's `EditQueue` one edit a frame (`Sim`'s step 2). Each edit entry a
//!   view's change records name is decoded once into an index that `edit()` answers from, kept as long as pe-api's
//!   `ChangeLog` holds its entry: at most [`EDITS_MAX`] entries and [`EDIT_BYTES_MAX`] bytes of payloads.
//! - **The rest:** `draft_for` from the member's chain as step 2 last left it; `put_blobs` into the machine's blob
//!   store; `member_name` a short form of the key; `head_of` none in M1, which has no head object; `session()` from
//!   the session's facts, the members whose links are up and the last advance.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::io;
use std::process::ExitStatus;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use pe_api::api::{Api, NotRunning, SessionFacts, WorldSource};
use pe_api::builds::Builds;
use pe_api::changes::{CAPACITY, ChangeLog};
use pe_api::queue::EditQueue;
use pe_mcp::host::{self, HostOpts};
use pe_seams::world_api::{self, Link, MemberId, WorldApi};
use pe_seams::{ChainId, EntryRef, Frame, LineId, MemberKey, Replica, SimRate};
use pe_world::edit::Transaction;
use pe_world::script::AssetId;
use pe_world::{Draft, WorldSnapshot};

/// How long the kept snapshots serve `at(f)`: pe-api's retention (api.md 9.1).
pub const KEEP: Duration = Duration::from_secs(2);
/// The edit index's caps (app.md 3.1): `ChangeLog`'s own entry count, and payload bytes [I].
pub const EDITS_MAX: usize = CAPACITY;
pub const EDIT_BYTES_MAX: usize = 64 << 20;
/// The edit queue's length: a longer one answers `rate_limited` (api.md 9.2). At one edit a frame it drains in about
/// a second at 60 Hz.
pub const QUEUE: usize = 64;

/// What only pe-app knows about the session, fixed for its length (api.md 5.1).
#[derive(Clone, Debug, PartialEq)]
pub struct Facts {
    /// The world's name, from the invite or the local configuration.
    pub world_name: String,
    pub line: LineId,
    pub rate: SimRate,
    /// This machine's member, if it plays (it is always among `here`).
    pub me: Option<MemberKey>,
}

/// The replica's present world as pe-world's snapshot (S2 v1.7's `world_snapshot`), if it is a pe-world replica.
pub fn snapshot_of(replica: &dyn Replica) -> Option<Arc<WorldSnapshot>> {
    replica.world_snapshot()?.downcast::<WorldSnapshot>().ok()
}

/// The replica's world at `Cv` as pe-world's snapshot (S2 v1.7's `agreed_snapshot`), if there is one.
pub fn agreed_of(replica: &dyn Replica) -> Option<Arc<WorldSnapshot>> {
    replica.agreed_snapshot()?.1.downcast::<WorldSnapshot>().ok()
}

/// pe-api's ports over one simulation: the shared state [`ApiWorld`] reads, the edit queue and the change log, and
/// the machine's build service once it has started.
#[derive(Clone)]
pub struct Ports {
    pub shared: Arc<ApiShared>,
    pub queue: Arc<EditQueue>,
    pub log: Arc<ChangeLog>,
    /// The build port (api.md 7.1): pe-app's `Builder`, without which `code_check` answers `unavailable`.
    pub builds: Option<Arc<dyn Builds>>,
    /// The capture port (api.md 8): pe-app's renderer, without which `view_capture` answers `capture_unavailable`.
    pub captures: Option<Arc<dyn pe_api::capture::Captures>>,
}

impl Ports {
    /// Ports for a session whose frame-0 world is `genesis` (final by genesis until the first agreed cut). `wake`
    /// wakes the simulation thread when an edit is queued (app.md 3, step 7).
    pub fn new(genesis: Arc<WorldSnapshot>, facts: Facts, wake: impl Fn() + Send + Sync + 'static) -> Ports {
        let queue = Arc::new(EditQueue::new(QUEUE, wake));
        let log = Arc::new(ChangeLog::new());
        queue.track_into(log.clone());
        Ports { shared: Arc::new(ApiShared::new(genesis, facts)), queue, log, builds: None, captures: None }
    }

    /// These ports with the machine's build service, which the WorldApi builds through (api.md 7.1).
    pub fn with_builds(mut self, builds: Arc<dyn Builds>) -> Ports {
        self.builds = Some(builds);
        self
    }

    /// These ports with the machine's renderer, which `view_capture` draws through (api.md 8).
    pub fn with_captures(mut self, captures: Arc<dyn pe_api::capture::Captures>) -> Ports {
        self.captures = Some(captures);
        self
    }

    /// The WorldApi over these ports, which pe-mcp serves (api.md 6.1).
    pub fn api(&self) -> Arc<dyn WorldApi> {
        let mut api = Api::new(ApiWorld(self.shared.clone()), self.queue.clone(), self.log.clone());
        if let Some(b) = &self.builds {
            api = api.with_builds(b.clone());
        }
        if let Some(c) = &self.captures {
            api = api.with_captures(c.clone());
        }
        Arc::new(api)
    }
}

/// What the simulation thread writes and pe-api's handlers read (app.md 3.1). Handlers hold the lock for one read.
pub struct ApiShared {
    facts: Facts,
    /// The kept snapshots' retention in frames, at the world's rate.
    keep_frames: u64,
    inner: Mutex<Inner>,
    /// Signalled at each [`ApiShared::keep`], for a thread that follows the newest snapshot (the build thread).
    fresh: Condvar,
}

struct Inner {
    latest: Arc<WorldSnapshot>,
    /// How many snapshots [`ApiShared::keep`] has had.
    published: u64,
    agreed: Arc<WorldSnapshot>,
    kept: BTreeMap<u64, Arc<WorldSnapshot>>,
    /// The member's chain and the seq its next entry gets.
    draft: Option<(ChainId, u64)>,
    edits: EditIndex,
    here: BTreeSet<MemberKey>,
    caught_up: bool,
    /// The scripts this machine can't run, as the attester last found them (`Attester::not_running`).
    scripts_not_running: Vec<(AssetId, Frame, String)>,
}

impl ApiShared {
    fn new(genesis: Arc<WorldSnapshot>, facts: Facts) -> ApiShared {
        let keep_frames = (KEEP.as_secs_f64() * f64::from(facts.rate.hz())).ceil() as u64;
        let mut kept = BTreeMap::new();
        kept.insert(genesis.frame().0, genesis.clone());
        let inner = Inner {
            latest: genesis.clone(),
            published: 0,
            agreed: genesis,
            kept,
            draft: None,
            edits: EditIndex::default(),
            here: BTreeSet::new(),
            // Until the first advance says otherwise, so pe-api refuses live changes with `not_ready` meanwhile
            // (@api-eng's Low on slice 1).
            caught_up: false,
            scripts_not_running: Vec::new(),
        };
        ApiShared { facts, keep_frames, inner: Mutex::new(inner), fresh: Condvar::new() }
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// A new view's snapshot: the newest, kept by its frame; those older than [`KEEP`] behind it go.
    pub fn keep(&self, snap: Arc<WorldSnapshot>) {
        let f = snap.frame().0;
        let mut g = self.lock();
        g.kept.retain(|&k, _| k + self.keep_frames >= f);
        g.kept.insert(f, snap.clone());
        g.latest = snap;
        g.published += 1;
        drop(g);
        self.fresh.notify_all();
    }

    /// The newest snapshot, and how many [`ApiShared::keep`] has had, once that count is past `seen` or `timeout` has
    /// passed: the build thread's wait (builds.rs). It returns at once when `seen` is behind.
    pub fn wait_newer(&self, seen: u64, timeout: Duration) -> (u64, Arc<WorldSnapshot>) {
        let g = self.lock();
        let wait = self.fresh.wait_timeout_while(g, timeout, |g| g.published <= seen);
        let (g, _) = wait.unwrap_or_else(|e| e.into_inner());
        (g.published, g.latest.clone())
    }

    /// The replica's world at `Cv`, as `final_()` reads it: the build thread's final state.
    pub fn agreed_snapshot(&self) -> Arc<WorldSnapshot> {
        self.lock().agreed.clone()
    }

    /// The replica rolled back to the state at frame `k`: every kept snapshot above it belongs to the timeline it
    /// replaced. The one at `k` is the state it restored, so it stays.
    pub fn rolled_back(&self, k: Frame) {
        self.lock().kept.retain(|&f, _| f <= k.0);
    }

    /// A repair replaced the replica's state: no kept snapshot is of it. `latest()` keeps the last published one, which
    /// the view keeps too, until the next view.
    pub fn repaired(&self) {
        self.lock().kept.clear();
    }

    /// The replica's world at `Cv` (`final_()`).
    pub fn agreed(&self, snap: Arc<WorldSnapshot>) {
        self.lock().agreed = snap;
    }

    /// The member's chain and the seq its next entry gets, as step 2 left them.
    pub fn draft(&self, chain: ChainId, next: u64) {
        self.lock().draft = Some((chain, next));
    }

    pub fn caught_up(&self, caught_up: bool) {
        self.lock().caught_up = caught_up;
    }

    /// The scripts this machine can't run, for session_info (S6's `scripts_not_running`): the attester's thread
    /// sets them after each pass (builds.rs).
    pub fn scripts_not_running(&self, list: Vec<(AssetId, Frame, String)>) {
        self.lock().scripts_not_running = list;
    }

    /// A member's link came up or went down (pe-net's `LinkUp`, `LinkDown`).
    pub fn link(&self, member: MemberKey, up: bool) {
        let mut g = self.lock();
        if up {
            g.here.insert(member);
        } else {
            g.here.remove(&member);
        }
    }

    /// Whether `entry`'s transaction is already indexed.
    pub fn has_edit(&self, entry: &EntryRef) -> bool {
        self.lock().edits.by_entry.contains_key(entry)
    }

    /// An edit entry's transaction, with its payload's length: this machine's own when step 2 submits it, anyone's
    /// when a view's change record names it.
    pub fn index_edit(&self, entry: EntryRef, tx: Arc<Transaction>, bytes: usize) {
        self.lock().edits.insert(entry, tx, bytes);
    }
}

/// The transactions of the edit entries the views named, oldest dropped first past either cap.
#[derive(Default)]
struct EditIndex {
    by_entry: HashMap<EntryRef, Arc<Transaction>>,
    order: VecDeque<(EntryRef, usize)>,
    bytes: usize,
}

impl EditIndex {
    fn insert(&mut self, entry: EntryRef, tx: Arc<Transaction>, bytes: usize) {
        if self.by_entry.insert(entry, tx).is_some() {
            return;
        }
        self.order.push_back((entry, bytes));
        self.bytes += bytes;
        while self.order.len() > EDITS_MAX || self.bytes > EDIT_BYTES_MAX {
            let Some((old, n)) = self.order.pop_front() else { break };
            self.by_entry.remove(&old);
            self.bytes -= n;
        }
    }
}

/// pe-api's `WorldSource` over [`ApiShared`] (app.md 3.1).
pub struct ApiWorld(pub Arc<ApiShared>);

impl WorldSource for ApiWorld {
    fn latest(&self) -> Arc<WorldSnapshot> {
        self.0.lock().latest.clone()
    }

    fn final_(&self) -> Arc<WorldSnapshot> {
        self.0.lock().agreed.clone()
    }

    fn at(&self, frame: world_api::Frame) -> Option<Arc<WorldSnapshot>> {
        self.0.lock().kept.get(&frame).cloned()
    }

    /// M1 has no head object, so `"me"` is refused by name, and the rule that agents don't move their user (api.md
    /// 10.6) has nothing to protect: re-check it when heads arrive.
    fn head_of(&self, _member: MemberId) -> Option<world_api::ObjectId> {
        None
    }

    /// This machine's member: its chain and next seq as step 2 left them, so a dry run made while the edit queue is
    /// empty names the seq the edit gets (its entry goes first in its frame). Anyone else: a chain of seq 1, whose
    /// ids are only provisional, as every dry run's are.
    fn draft_for(&self, member: MemberId) -> Draft {
        let author: MemberKey = member.into();
        let line = self.0.facts.line;
        match (self.0.facts.me, self.0.lock().draft) {
            (Some(me), Some((chain, seq))) if me == author => Draft { author, line, chain, seq },
            _ => Draft { author, line, chain: ChainId::new(line, author, [0; 32]), seq: 1 },
        }
    }

    fn put_blobs(&self, blobs: &[Vec<u8>]) {
        let store = self.0.lock().latest.blob_store().clone();
        for b in blobs {
            store.put(b);
        }
    }

    fn edit(&self, entry: EntryRef) -> Option<Arc<Transaction>> {
        self.0.lock().edits.by_entry.get(&entry).cloned()
    }

    /// Until members have names: "member " and the key's first four bytes in hex.
    fn member_name(&self, member: MemberId) -> Option<String> {
        let key: MemberKey = member.into();
        Some(format!("member {:02x}{:02x}{:02x}{:02x}", key.0[0], key.0[1], key.0[2], key.0[3]))
    }

    fn session(&self) -> SessionFacts {
        let g = self.0.lock();
        let here = self.0.facts.me.iter().chain(g.here.iter()).copied().collect::<BTreeSet<_>>();
        SessionFacts {
            world_name: self.0.facts.world_name.clone(),
            line: self.0.facts.line,
            here: here.into_iter().map(MemberId::from).collect(),
            // pe-net's link statistics don't reach the simulation thread yet; a replica catching up is the one
            // state this machine knows of itself.
            link: if g.caught_up { Link::Fine } else { Link::CatchingUp },
            caught_up: g.caught_up,
            tick_hz: self.0.facts.rate.hz(),
            agents_move_user: false,
            scripts_not_running: (g.scripts_not_running.iter())
                .map(|(script, since, why)| NotRunning {
                    script: *script,
                    object: None,
                    since: *since,
                    why: why.clone(),
                })
                .collect(),
        }
    }
}

/// The MCP thread (app.md 1, 3.1): pe-mcp's transports over a WorldApi on a tokio runtime of 2 workers, whose
/// handlers call the API through `spawn_blocking` (WS-api O9). It runs until the agent pe-mcp launched exits, or until
/// [`McpThread::stop`]; either way pe-mcp revokes its tokens.
pub struct McpThread {
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<JoinHandle<io::Result<Option<ExitStatus>>>>,
    #[cfg(test)]
    handle: tokio::runtime::Handle,
}

/// How long stopping waits for the runtime's blocking calls, such as an agent's `watch` long poll (up to 30 s): they
/// hold only the API and finish by themselves, and pe-app is exiting (@api-eng's Medium on slice 2).
const MCP_SHUTDOWN: Duration = Duration::from_millis(100);

/// Start the MCP thread over `api` with pe-mcp's `opts` (its transports, the agent's command line and grant).
pub fn spawn_mcp(api: Arc<dyn WorldApi>, opts: HostOpts) -> io::Result<McpThread> {
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let rt =
        tokio::runtime::Builder::new_multi_thread().worker_threads(2).thread_name("pe-mcp").enable_all().build()?;
    #[cfg(test)]
    let handle = rt.handle().clone();
    let thread = thread::Builder::new().name("mcp".into()).spawn(move || {
        let served = rt.block_on(async move {
            let mut hosted = host::start(api, &opts).await?;
            tokio::select! {
                exited = hosted.wait() => exited,
                _ = stopped => Ok(None),
            }
        });
        rt.shutdown_timeout(MCP_SHUTDOWN);
        served
    })?;
    Ok(McpThread {
        stop: Some(stop),
        thread: Some(thread),
        #[cfg(test)]
        handle,
    })
}

impl McpThread {
    /// Stop the transports, and the agent with them if pe-mcp launched it, and wait for the thread. The agent's exit
    /// status if it had already exited; an error if the transports never started.
    pub fn stop(mut self) -> io::Result<Option<ExitStatus>> {
        if let Some(s) = self.stop.take() {
            let _ = s.send(());
        }
        match self.thread.take().map(JoinHandle::join) {
            Some(Ok(r)) => r,
            Some(Err(_)) => Err(io::Error::other("the MCP thread panicked")),
            None => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts() -> Facts {
        Facts { world_name: "Pit".into(), line: LineId(pe_seams::Hash([1; 32])), rate: SimRate::DEFAULT, me: None }
    }

    /// A world snapshot at `frame`: an empty pe-world world stepped to it.
    fn snap(frame: u64) -> Arc<WorldSnapshot> {
        let mut w = pe_world::World::new(0, pe_world::BlobStore::default());
        w.set_frame(Frame(frame));
        w.snapshot(Frame(0))
    }

    #[test]
    fn kept_snapshots_serve_two_seconds_and_a_rollback_drops_those_above_the_restored_frame() {
        let s = ApiShared::new(snap(0), facts());
        let w = ApiWorld(Arc::new(s));
        for f in [10, 60, 130, 140] {
            w.0.keep(snap(f));
        }
        let kept = |w: &ApiWorld| w.0.lock().kept.keys().copied().collect::<Vec<_>>();
        assert_eq!(kept(&w), vec![60, 130, 140], "2 s at 60 Hz reaches 120 frames behind the newest: 0 and 10 went");
        w.0.keep(snap(200));
        assert_eq!(kept(&w), vec![130, 140, 200]);
        w.0.rolled_back(Frame(140));
        assert_eq!(kept(&w), vec![130, 140], "the restored state's frame stays");
        assert!(w.at(200).is_none() && w.at(140).is_some());
        assert_eq!(w.latest().frame(), Frame(200), "latest is the last published until the next view");
        w.0.repaired();
        assert!(w.at(140).is_none());
    }

    #[test]
    fn final_is_frame_0_until_an_agreed_snapshot_comes() {
        let w = ApiWorld(Arc::new(ApiShared::new(snap(0), facts())));
        assert_eq!(w.final_().frame(), Frame(0));
        w.0.keep(snap(90));
        assert_eq!(w.final_().frame(), Frame(0), "a published view is not final");
        w.0.agreed(snap(64));
        assert_eq!(w.final_().frame(), Frame(64));
    }

    #[test]
    fn the_edit_index_drops_the_oldest_past_its_byte_cap() {
        let mut ix = EditIndex::default();
        let tx = Transaction {
            inline: vec![],
            actor: pe_world::edit::Actor::Person,
            label: None,
            preconditions: vec![],
            ops: vec![],
            blobs: vec![],
        };
        let e = |n: u8| EntryRef(pe_seams::Hash([n; 32]));
        let tx = Arc::new(tx);
        ix.insert(e(1), tx.clone(), EDIT_BYTES_MAX / 2);
        ix.insert(e(2), tx.clone(), EDIT_BYTES_MAX / 2);
        ix.insert(e(2), tx.clone(), EDIT_BYTES_MAX / 2);
        assert_eq!((ix.order.len(), ix.bytes), (2, EDIT_BYTES_MAX), "an entry indexed twice counts once");
        ix.insert(e(3), tx, 1);
        assert!(!ix.by_entry.contains_key(&e(1)) && ix.by_entry.contains_key(&e(3)));
        assert_eq!(ix.bytes, EDIT_BYTES_MAX / 2 + 1);
    }

    fn grant() -> pe_mcp::auth::Grant {
        pe_mcp::auth::Grant {
            member: MemberId::from(MemberKey([2; 32])),
            producer: world_api::Producer {
                kind: world_api::ProducerKind::Agent,
                harness: None,
                principal: None,
                requested_by: None,
            },
            confirm: false,
        }
    }

    /// The MCP thread serves its pipe until stopped, and reports transports that never started.
    #[test]
    fn the_mcp_thread_serves_until_stopped_and_reports_a_start_that_failed() {
        let ports = Ports::new(snap(0), facts(), || {});
        let mut opts = HostOpts::new(grant());
        opts.pipe = true;
        let mcp = spawn_mcp(ports.api(), opts).expect("the thread");
        assert_eq!(mcp.stop().expect("served, then stopped"), None, "no agent was launched");
        let none = spawn_mcp(ports.api(), HostOpts::new(grant())).expect("the thread");
        assert_eq!(none.stop().map_err(|e| e.kind()), Err(io::ErrorKind::InvalidInput), "no transport asked for");
    }

    /// Stopping doesn't wait out a blocking API call in flight, such as an agent's 30 s `watch` (@api-eng's Medium on
    /// slice 2).
    #[test]
    fn stopping_the_mcp_thread_does_not_wait_for_a_blocking_call() {
        let ports = Ports::new(snap(0), facts(), || {});
        let mut opts = HostOpts::new(grant());
        opts.pipe = true;
        let mcp = spawn_mcp(ports.api(), opts).expect("the thread");
        mcp.handle.spawn_blocking(|| thread::sleep(Duration::from_secs(30)));
        let started = std::time::Instant::now();
        assert_eq!(mcp.stop().expect("stopped"), None);
        assert!(started.elapsed() < Duration::from_secs(5), "stopping took {:?}", started.elapsed());
    }

    #[test]
    fn the_session_lists_this_machine_s_member_and_those_whose_links_are_up_once_each() {
        let me = MemberKey([2; 32]);
        let other = MemberKey([3; 32]);
        let w = ApiWorld(Arc::new(ApiShared::new(snap(0), Facts { me: Some(me), ..facts() })));
        assert_eq!(w.session().here, vec![MemberId::from(me)]);
        w.0.link(other, true);
        w.0.link(me, true);
        assert_eq!(w.session().here, vec![MemberId::from(me), MemberId::from(other)]);
        w.0.link(other, false);
        assert_eq!(w.session().here, vec![MemberId::from(me)]);
        w.0.draft(ChainId::new(w.0.facts.line, me, [9; 32]), 12);
        assert_eq!(w.draft_for(me.into()).seq, 12, "my chain's next seq");
        assert_eq!(w.draft_for(other.into()).seq, 1, "anyone else's is provisional");
    }
}
