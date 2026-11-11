//! The core of WorldApi over pe-world (api.md 5.2, 5.3, 9.2): `query` and `apply` for one caller, composed from the
//! pieces beside it. pe-app supplies a `WorldSource` (its published pe-world frames, each caller's head, and the
//! coordinates the signer will use); `Api` keeps the per-connection versions seen and the per-principal rate.
//!
//! - `query` picks the snapshot (`at`, or a cursor's frame), reads it through `WorldView` for this caller, and
//!   records every version it reported in the connection's table, at the snapshot's frame (spec 4.2).
//! - `apply` checks the `edit` scope and the rate, resolves against the base snapshot with the connection's
//!   versions as defaults, maps to pe-world's payload and runs pe-world's own edit path on it (step 3). A dry run
//!   stops there. A live change refused there is answered at once and never queued; otherwise its blobs go into
//!   the store, it goes on the edit queue (step 4), and `apply` waits, within the call's timeout, for the receipt:
//!   the entry id and frame agreement gave it, then for its fate in the change log (step 6): `accepted` or
//!   `refused` when stepped here, `final`, `dropped` or a final refusal when asked to wait for finality (a final cut
//!   decides, never the frame alone). Created ids come from pe-world's applied outcome (in new-index order,
//!   checked against the count resolve reserved), versions from the newest snapshot or, failing that, the fate's
//!   frame. Out of time, it is `queued`, with its id once known. [I] If even the receipt is late the caller gets no
//!   id; `watch` reporting a caller's own changes covers that (to build).
//!
//! The rest of the trait (session, schema, watch, history, revert, capture, code_check) composes later over this,
//! with the session, change-record, capture and logic ports (spec 3).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use pe_seams::SubmitError;
use pe_seams::world_api::{
    ApiError, ApplyOpts, At, AtName, Caller, ConnectionId, ErrorCode, Frame, MemberId, Mode, ObjectId, Outcome,
    ProducerKind, Query, QueryResult, Refusal, RefusalCode, Scope, Status, Transaction, Wait,
};
use pe_world::edit::Actor;
use pe_world::{Draft, WorldSnapshot};

use crate::changes::ChangeLog;
use crate::edit::{Unmapped, to_world};
use crate::outcome::{dry_run, map_refusal};
use crate::ports::{Snapshot, SnapshotSource};
use crate::query::{pick, run};
use crate::queue::{EditQueue, Receipt, Submission};
use crate::rate::{Bucket, Rate};
use crate::resolve::resolve_tx;
use crate::versions::VersionTable;
use crate::world::WorldView;

/// What pe-app gives the API: its published pe-world frames and, per member, what `"me"` and a draft need.
pub trait WorldSource: Send + Sync {
    fn latest(&self) -> Arc<WorldSnapshot>;
    fn final_(&self) -> Arc<WorldSnapshot>;
    /// A frame still retained, or `None`.
    fn at(&self, frame: Frame) -> Option<Arc<WorldSnapshot>>;
    /// The object that is this member's head, for `"me"`.
    fn head_of(&self, member: MemberId) -> Option<ObjectId>;
    /// The coordinates the signer will use for this member's next entry (exact created ids when they match).
    fn draft_for(&self, member: MemberId) -> Draft;
    /// Puts a change's blob bytes into the machine's blob store, before the change is queued.
    fn put_blobs(&self, blobs: &[Vec<u8>]);
}

/// The source's frames as one caller sees them, for the query pieces.
struct Views<'a> {
    src: &'a dyn WorldSource,
    me: Option<ObjectId>,
}

impl SnapshotSource for Views<'_> {
    fn latest(&self) -> Arc<dyn Snapshot> {
        Arc::new(WorldView::new(self.src.latest(), self.me))
    }
    fn final_(&self) -> Arc<dyn Snapshot> {
        Arc::new(WorldView::new(self.src.final_(), self.me))
    }
    fn at(&self, frame: Frame) -> Option<Arc<dyn Snapshot>> {
        Some(Arc::new(WorldView::new(self.src.at(frame)?, self.me)))
    }
}

pub struct Api<S: WorldSource> {
    src: S,
    queue: Arc<EditQueue>,
    log: Arc<ChangeLog>,
    /// Each connection's table of versions seen, behind its own lock, so one connection's apply never waits on
    /// another's query.
    versions: Mutex<HashMap<ConnectionId, Arc<Mutex<VersionTable>>>>,
    rates: Mutex<HashMap<String, Bucket>>,
    rate: Rate,
}

fn rate_limited(wait: Duration) -> ApiError {
    let mut e = ApiError::new(ErrorCode::RateLimited, "Too many changes; wait and try again.");
    e.details = Some(serde_json::json!({ "retry_after_ms": wait.as_millis() as u64 }));
    e
}

/// The longest an `apply` waits for its receipt (api.md 5.3: default 2,000 ms, at most 10,000).
fn timeout(opts: &ApplyOpts) -> Duration {
    Duration::from_millis(u64::from(opts.timeout_ms.unwrap_or(2_000).min(10_000)))
}

fn forbidden(what: &str) -> ApiError {
    ApiError::new(ErrorCode::Forbidden, format!("This token may not {what}."))
}

impl<S: WorldSource> Api<S> {
    /// The API over `src`, queueing live changes on `queue`, which the simulation thread drains, and learning
    /// their fates from `log`, which pe-app feeds from each publication's change record.
    pub fn new(src: S, queue: Arc<EditQueue>, log: Arc<ChangeLog>) -> Self {
        queue.track_into(log.clone());
        Api {
            src,
            queue,
            log,
            versions: Mutex::new(HashMap::new()),
            rates: Mutex::new(HashMap::new()),
            rate: Rate::TX_DEFAULT,
        }
    }

    fn table(&self, c: ConnectionId) -> Arc<Mutex<VersionTable>> {
        let mut t = self.versions.lock().unwrap_or_else(|e| e.into_inner());
        t.entry(c).or_default().clone()
    }

    /// Forgets a closed connection's table.
    pub fn close(&self, c: ConnectionId) {
        self.versions.lock().unwrap_or_else(|e| e.into_inner()).remove(&c);
    }

    fn base(&self, at: At) -> Result<Arc<WorldSnapshot>, ApiError> {
        match at {
            At::Named(AtName::Latest) => Ok(self.src.latest()),
            At::Named(AtName::Final) => Ok(self.src.final_()),
            At::Frame(f) => self.src.at(f).ok_or_else(|| {
                let mut e = ApiError::new(ErrorCode::SnapshotGone, format!("frame {f} is no longer retained"));
                e.field = Some("base".into());
                e
            }),
        }
    }

    pub fn query(&self, c: &Caller, q: &Query) -> Result<QueryResult, ApiError> {
        if !c.scopes.contains(&Scope::Read) {
            return Err(forbidden("read the world"));
        }
        let views = Views { src: &self.src, me: self.src.head_of(c.member) };
        let (snap, offset) = pick(&views, q.at, q.cursor.as_ref())?;
        let (res, seen) = run(snap.as_ref(), q, offset)?;
        let table = self.table(c.connection);
        let mut table = table.lock().unwrap_or_else(|e| e.into_inner());
        for (id, v) in seen {
            table.saw(id, v, res.frame);
        }
        Ok(res)
    }

    pub fn apply(&self, c: &Caller, tx: Transaction, opts: &ApplyOpts) -> Result<Outcome, ApiError> {
        if !c.scopes.contains(&Scope::Edit) {
            return Err(forbidden("change the world"));
        }
        {
            let mut r = self.rates.lock().unwrap_or_else(|e| e.into_inner());
            let b = r.entry(c.principal.clone()).or_insert_with(|| Bucket::new(self.rate, Instant::now()));
            if let Err(wait) = b.take(Instant::now()) {
                return Err(rate_limited(wait));
            }
        }
        let snap = self.base(tx.base)?;
        let view = WorldView::new(snap.clone(), self.src.head_of(c.member));
        let table = self.table(c.connection);
        let table = table.lock().unwrap_or_else(|e| e.into_inner());
        let lookup = |id: ObjectId| table.get(&id).cloned();
        let outcome_refused = |f: Box<pe_seams::world_api::Refusal>| Outcome {
            dry_run: opts.mode == Mode::DryRun,
            status: pe_seams::world_api::Status::Refused,
            is_final: false,
            change: None,
            frame: None,
            final_frame: snap.final_frame().0,
            created: Default::default(),
            changed: vec![],
            count: 0,
            refusal: Some(*f),
            diff: None,
        };
        let r = resolve_tx(&view, &tx, &lookup);
        drop(table);
        let r = match r {
            Ok(r) => r,
            Err(f) => return Ok(outcome_refused(f)),
        };
        let actor = match c.producer.kind {
            ProducerKind::Hand => Actor::Person,
            ProducerKind::Agent => Actor::Agent {
                harness: c.producer.harness.clone().unwrap_or_default(),
                principal: c.member.into(),
                requested_by: c.producer.requested_by.map(Into::into),
            },
        };
        let e = match to_world(&r, actor, &snap) {
            Ok(e) => e,
            Err(Unmapped::Refused(f)) => return Ok(outcome_refused(f)),
            Err(Unmapped::Failed(err)) => return Err(err),
        };
        let checked = dry_run(&snap, &r, &e, &self.src.draft_for(c.member));
        if opts.mode == Mode::DryRun {
            return Ok(checked);
        }
        // Live. Refused against the base: answered now, never queued.
        if checked.status == Status::Refused {
            return Ok(Outcome { dry_run: false, ..checked });
        }
        self.src.put_blobs(&e.blobs);
        let ticket = self
            .queue
            .submit(Submission { tx: e.tx.clone(), base: snap.frame().0 })
            .map_err(|f| rate_limited(f.retry_after))?;
        // The dry run's created ids and versions were provisional; the real ones come with the change record.
        let queued = Outcome { dry_run: false, status: Status::Queued, created: Default::default(), ..checked };
        if opts.wait == Wait::None {
            return Ok(queued);
        }
        let deadline = Instant::now() + timeout(opts);
        Ok(match self.queue.receipt(ticket, deadline) {
            Some(Receipt::Submitted { change, frame, .. }) => {
                let queued = Outcome { change: Some(change), frame: Some(frame), ..queued };
                let Some(fate) = self.log.wait(change, opts.wait, deadline) else { return Ok(queued) };
                let mut o = Outcome {
                    status: fate.status,
                    is_final: fate.is_final,
                    frame: fate.frame.or(Some(frame)),
                    ..queued
                };
                match &fate.outcome {
                    None => {}
                    Some(pe_seams::Outcome::Applied { created, .. }) => {
                        // pe-world creates in new-index order (a copy's block depth-first, as resolve numbered it).
                        if created.len() != r.new_count as usize {
                            return Err(ApiError::new(
                                ErrorCode::Internal,
                                format!(
                                    "The world created {} objects where {} were expected.",
                                    created.len(),
                                    r.new_count
                                ),
                            ));
                        }
                        let now = self.src.latest();
                        let then = fate.frame.and_then(|f| self.src.at(f));
                        for (name, k) in &r.refs {
                            if let Some(id) = created.get(*k as usize)
                                && let Some(info) = now.info(*id).or_else(|| then.as_ref().and_then(|s| s.info(*id)))
                            {
                                let v = crate::world::version_text(&info.version);
                                o.created.insert(
                                    name.clone(),
                                    pe_seams::world_api::IdVersion { id: (*id).into(), version: v },
                                );
                            }
                        }
                    }
                    Some(pe_seams::Outcome::Refused { op, reason }) => {
                        o.count = 0;
                        o.refusal = Some(map_refusal(&snap, &e, *op, reason));
                    }
                }
                o
            }
            // Agreement's reasons (engine-architect m67f17c97f4b9): not this member's next entry is pe-app's bug; a
            // malformed entry is an encoding bug, invalid; too late means this machine is far behind, retryable.
            Some(Receipt::Refused(SubmitError::NotNext)) => {
                return Err(ApiError::new(ErrorCode::Internal, "The change wasn't this machine's next entry."));
            }
            Some(Receipt::Refused(why)) => {
                let (code, message, next) = match why {
                    SubmitError::TooLate => (
                        RefusalCode::NotReady,
                        "This machine is too far behind to submit; try again when it has caught up.".to_string(),
                        Some("Wait for session_info's caught_up, then apply again.".to_string()),
                    ),
                    other => {
                        (RefusalCode::Invalid, format!("The change couldn't be encoded as an entry ({other:?})."), None)
                    }
                };
                Outcome {
                    status: Status::Refused,
                    refusal: Some(Refusal {
                        code,
                        message,
                        op: None,
                        object: None,
                        field: None,
                        read: None,
                        now: None,
                        winner: None,
                        next,
                        details: None,
                    }),
                    ..queued
                }
            }
            // Still waiting for the simulation thread: queued, with no id yet; watch reports it.
            None => queued,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pe_core::{ChainId, EntryRef, Frame as CoreFrame, Hash, LineId, MemberKey, Offset, Pos};
    use pe_seams::world_api::{Producer, RefusalCode, Selector, Status};
    use pe_sim::Transform;
    use pe_world::components::Name;
    use pe_world::{BlobStore, World, perm};
    use serde_json::json;
    use std::collections::BTreeSet;

    const ALICE: MemberKey = MemberKey([1; 32]);

    fn id(n: u128) -> pe_core::ObjectId {
        pe_core::ObjectId::from_u128(n)
    }

    /// One published frame: a Table and a Lamp under it; Alice may edit.
    struct Src(Arc<WorldSnapshot>);

    impl WorldSource for Src {
        fn latest(&self) -> Arc<WorldSnapshot> {
            self.0.clone()
        }
        fn final_(&self) -> Arc<WorldSnapshot> {
            self.0.clone()
        }
        fn at(&self, frame: Frame) -> Option<Arc<WorldSnapshot>> {
            (frame == self.0.frame().0).then(|| self.0.clone())
        }
        fn head_of(&self, _: MemberId) -> Option<ObjectId> {
            None
        }
        fn draft_for(&self, member: MemberId) -> Draft {
            Draft { author: member.into(), line: LineId(Hash([7; 32])), chain: ChainId(Hash([8; 32])), seq: 1 }
        }
        fn put_blobs(&self, _: &[Vec<u8>]) {}
    }

    fn api() -> Api<Src> {
        let mut w = World::new(1, BlobStore::default());
        w.set_member(ALICE, perm::EDIT);
        for (n, name, parent) in [(10u128, "Table", None), (20, "Lamp", Some(10))] {
            let text = w.blobs().put(name.as_bytes());
            let v = pe_seams::Version { entry: EntryRef(Hash([n as u8; 32])), op: 0 };
            w.create(id(n), parent.map(id), ALICE, v).unwrap();
            let t = match parent {
                None => Transform::at(Pos::ORIGIN.checked_add(Offset([1.0, 0.0, 0.0])).unwrap()),
                Some(_) => Transform::local([0.0, 1.0, 0.0]),
            };
            w.insert(id(n), t).unwrap();
            w.insert(id(n), Name { text }).unwrap();
        }
        w.set_frame(CoreFrame(4));
        Api::new(Src(w.snapshot(CoreFrame(4))), Arc::new(EditQueue::new(2, || {})), Arc::new(ChangeLog::new()))
    }

    fn caller(scopes: &[Scope]) -> Caller {
        Caller {
            connection: ConnectionId(1),
            principal: "claude".into(),
            member: ALICE.into(),
            scopes: scopes.iter().copied().collect::<BTreeSet<_>>(),
            producer: Producer {
                kind: ProducerKind::Agent,
                harness: Some("test".into()),
                principal: None,
                requested_by: None,
            },
            confirm: false,
        }
    }

    fn tx(ops: serde_json::Value) -> Transaction {
        serde_json::from_value(json!({ "label": "t", "ops": ops })).unwrap()
    }

    #[test]
    fn query_reads_pe_world_and_records_versions() {
        let a = api();
        let c = caller(&[Scope::Read]);
        let q = Query { select: Some(Selector::Name("Lamp".into())), ..Default::default() };
        let r = a.query(&c, &q).unwrap();
        assert_eq!(r.objects.len(), 1);
        assert_eq!(r.objects[0].name, "Lamp");
        let t = a.table(c.connection);
        assert!(t.lock().unwrap().get(&id(20).into()).is_some());
    }

    #[test]
    fn apply_dry_runs_and_refuses_what_it_should() {
        let a = api();
        let c = caller(&[Scope::Read, Scope::Edit]);
        let dry = ApplyOpts { mode: Mode::DryRun, ..Default::default() };
        let o =
            a.apply(&c, tx(json!([{"op": "create", "as": "$cup", "parent": "/Table", "name": "Cup"}])), &dry).unwrap();
        assert!(o.dry_run);
        assert_eq!(o.status, Status::Accepted, "{:?}", o.refusal);
        assert!(o.created.contains_key("$cup"));
        // A resolve refusal comes back as a refused outcome, not an error.
        let o = a.apply(&c, tx(json!([{"op": "delete", "id": "/Nope"}])), &dry).unwrap();
        assert_eq!(o.refusal.map(|f| f.code), Some(RefusalCode::NotFound));
        // A read-only token can't apply at all.
        assert_eq!(a.apply(&caller(&[Scope::Read]), tx(json!([])), &dry).unwrap_err().code, ErrorCode::Forbidden);
    }

    #[test]
    fn a_live_change_is_checked_queued_and_given_its_entry() {
        let a = Arc::new(api());
        let c = caller(&[Scope::Edit]);
        // Refused against the base: answered at once, never queued.
        let o = a.apply(&c, tx(json!([{"op": "delete", "id": "/Nope"}])), &ApplyOpts::default()).unwrap();
        assert_eq!((o.status, o.dry_run), (Status::Refused, false));
        assert!(a.queue.is_empty());
        // Without waiting: queued, no id yet.
        let none = ApplyOpts { wait: Wait::None, ..Default::default() };
        let o = a.apply(&c, tx(json!([{"op": "create", "parent": "/Table", "name": "Cup"}])), &none).unwrap();
        assert_eq!((o.status, o.change, o.dry_run), (Status::Queued, None, false));
        assert!(o.created.is_empty(), "provisional ids aren't reported for a live change");
        // Waiting: the simulation thread drains it, settles the entry agreement gave it, steps it, and the
        // publication's change record reaches the log.
        let (q, log) = (a.queue.clone(), a.log.clone());
        let entry = EntryRef(Hash([3; 32]));
        let sim = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(5);
            let mut settled = 0;
            while settled < 2 && Instant::now() < deadline {
                for (t, s) in q.drain(4) {
                    assert_eq!(s.base, 4);
                    q.settle(
                        t,
                        Receipt::Submitted {
                            change: entry.into(),
                            frame: 5,
                            chain: pe_core::ChainId(Hash([1; 32])),
                            seq: 1,
                        },
                    );
                    let outcome = pe_seams::Outcome::Applied {
                        created: vec![id(99)],
                        overwrote: vec![],
                        notes: vec![],
                        inverse: None,
                    };
                    let step = pe_seams::StepRecord {
                        entry,
                        author: ALICE,
                        class: pe_seams::Class::Edit,
                        frame: CoreFrame(5),
                        outcome,
                    };
                    log.record(&[step], Some(2));
                    settled += 1;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        });
        let o = a
            .apply(&c, tx(json!([{"op": "create", "parent": "/Table", "name": "Mug"}])), &ApplyOpts::default())
            .unwrap();
        sim.join().unwrap();
        assert_eq!((o.status, o.change, o.frame), (Status::Accepted, Some(entry.into()), Some(5)));
        // Refused when stepped: the refusal is mapped like a dry run's.
        let step = pe_seams::StepRecord {
            entry,
            author: ALICE,
            class: pe_seams::Class::Edit,
            frame: CoreFrame(6),
            outcome: pe_seams::Outcome::Refused { op: Some(0), reason: pe_seams::Reason::NoPermission },
        };
        a.log.record(&[step], Some(6));
        let at = pe_seams::CutRef { line: LineId(Hash([0; 32])), height: 1, frame: CoreFrame(7) };
        let head = pe_seams::ChainHead { chain: pe_core::ChainId(Hash([1; 32])), author: ALICE, seq: 1, entry };
        a.log.confirm_cut(&pe_seams::Cut { at, heads: vec![head] }, &[]);
        let f = a.log.get(entry.into()).unwrap();
        assert_eq!((f.status, f.is_final), (Status::Refused, true));
        // A full queue is rate_limited with a retry time.
        a.apply(&c, tx(json!([])), &none).unwrap();
        a.apply(&c, tx(json!([])), &none).unwrap();
        let e = a.apply(&c, tx(json!([])), &none).unwrap_err();
        assert_eq!(e.code, ErrorCode::RateLimited);
        assert!(e.details.unwrap()["retry_after_ms"].as_u64().is_some());
    }

    // chunks-reviewer's Low on c322084e: a change nobody waits for is still decided by the final cuts.
    #[test]
    fn a_change_applied_without_waiting_is_still_decided_by_the_cut() {
        let a = api();
        let c = caller(&[Scope::Edit]);
        let none = ApplyOpts { wait: Wait::None, ..Default::default() };
        a.apply(&c, tx(json!([{"op": "create", "parent": "/Table", "name": "Cup"}])), &none).unwrap();
        let entry = EntryRef(Hash([4; 32]));
        let chain = pe_core::ChainId(Hash([1; 32]));
        for (t, _) in a.queue.drain(4) {
            a.queue.settle(t, Receipt::Submitted { change: entry.into(), frame: 5, chain, seq: 3 });
        }
        let outcome =
            pe_seams::Outcome::Applied { created: vec![id(99)], overwrote: vec![], notes: vec![], inverse: None };
        let step =
            pe_seams::StepRecord { entry, author: ALICE, class: pe_seams::Class::Edit, frame: CoreFrame(5), outcome };
        a.log.record(&[step], None);
        let at = pe_seams::CutRef { line: LineId(Hash([0; 32])), height: 1, frame: CoreFrame(6) };
        let head = pe_seams::ChainHead { chain, author: ALICE, seq: 3, entry };
        a.log.confirm_cut(&pe_seams::Cut { at, heads: vec![head] }, &[]);
        assert_eq!(a.log.get(entry.into()).unwrap().status, Status::Final);
    }

    #[test]
    fn the_rate_limit_applies_per_principal() {
        let a = api();
        let c = caller(&[Scope::Edit]);
        let dry = ApplyOpts { mode: Mode::DryRun, ..Default::default() };
        let limited = (0..60)
            .filter(|_| matches!(a.apply(&c, tx(json!([])), &dry), Err(e) if e.code == ErrorCode::RateLimited))
            .count();
        assert!(limited > 0, "a burst of 60 passes a 40-transaction bucket");
    }
}
