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

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use pe_core::EntryRef;
use pe_seams::SubmitError;
use pe_seams::world_api::{
    ApiError, ApplyOpts, At, AtName, Caller, Changes, CodeCheck, ConnectionId, Cursor, Diagnostics, ErrorCode, Filter,
    Frame, HeldAction, MemberId, Mode, ObjectId, Outcome, ProducerKind, Query, QueryResult, Refusal, RefusalCode,
    Scope, SessionEvent, Status, Transaction, TxId, Wait,
};
use pe_world::edit::Actor;
use pe_world::{Draft, WorldSnapshot};

use crate::builds::{BuildRefused, Builds, archive_of, diagnostics_of, margin_diagnostic, refusal_diagnostics};
use crate::changes::{ChangeLog, FeedItem, Kept, Owner};
use crate::edit::{Unmapped, WorldEdit, to_world_built};
use crate::outcome::{dry_run, map_refusal};
use crate::ports::{Snapshot, SnapshotSource};
use crate::query::{Budget, EXAMINE_MAX, pick, run, select_in};
use crate::queue::{EditQueue, Receipt, Submission};
use crate::rate::{Bucket, Rate};
use crate::resolve::{Resolved, resolve_tx};
use crate::stream::StreamError;
use crate::versions::VersionTable;
use crate::watch::{Describe, mine};
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
    /// The transaction of an edit entry this machine holds (its payload, decoded by pe-world's codec), for `watch`
    /// to say what a change did and who produced it. `None` if the entry is unknown or no longer held. Shared, not
    /// copied: `history` may ask for thousands while the simulation thread shares the source's lock.
    fn edit(&self, entry: EntryRef) -> Option<Arc<pe_world::edit::Transaction>>;
    /// A member's display name, for `watch`'s `by`.
    fn member_name(&self, member: MemberId) -> Option<String>;
    /// What only pe-app knows about the session (api.md 5.1). Members, roles and scale are world state, read
    /// from the snapshot.
    fn session(&self) -> SessionFacts;
}

/// The session as pe-app alone knows it (api.md 5.1; agreed with @engine-architect).
#[derive(Clone, Debug, PartialEq)]
pub struct SessionFacts {
    /// The world's name, from the invite or the local configuration (not world state in M1).
    pub world_name: String,
    pub line: pe_core::LineId,
    /// The members connected now (from agreement and pe-net), each at most once.
    pub here: Vec<MemberId>,
    pub link: pe_seams::world_api::Link,
    /// False while a joiner's import or a catch-up runs: `apply` then refuses live changes with `not_ready`.
    pub caught_up: bool,
    /// The rate pe-app runs at, the world's setting.
    pub tick_hz: u32,
    /// The owning user's local setting, off by default: agents acting for this machine's member may move it
    /// (locomotion, a teleport, a bodiless member's view position). A client setting, not world state (coordinator; api.md 10.6).
    pub agents_move_user: bool,
}

/// A refusal with only a code, a message and what to do next.
fn refusal_of(code: RefusalCode, message: &str, next: Option<&str>) -> Refusal {
    Refusal {
        code,
        message: message.into(),
        op: None,
        object: None,
        field: None,
        read: None,
        now: None,
        winner: None,
        next: next.map(Into::into),
        details: None,
    }
}

/// A live change refused before it is checked or queued.
fn refused_outcome(snap: &WorldSnapshot, f: Refusal) -> Outcome {
    Outcome {
        dry_run: false,
        status: Status::Refused,
        repaired_at: None,
        is_final: false,
        change: None,
        frame: None,
        final_frame: snap.final_frame().0,
        created: Default::default(),
        changed: vec![],
        count: 0,
        refusal: Some(f),
        diff: None,
        resent_as: None,
        held: None,
        activation: None,
        scripts: Default::default(),
    }
}

/// Whether `o` is part of "the user" for the member `key` in `snap` (api.md 10.6; coordinator): `o`,
/// or something under it, is the user's head or one of the hands whose `Hand` row it owns. That is the chain above
/// each tracked node: the tracked nodes themselves, their avatar root and all of its ancestors. The head's own
/// descendants and the root's other children are not part of it. Costs the head's depth, then `o`'s subtree, never
/// a scan of the world (@chunks-reviewer's Low on fab13945).
fn is_user(
    snap: &WorldSnapshot,
    o: pe_core::ObjectId,
    head: Option<pe_core::ObjectId>,
    key: pe_core::MemberKey,
) -> bool {
    if head.is_some_and(|h| std::iter::successors(Some(h), |x| snap.parent(*x)).any(|a| a == o)) {
        return true;
    }
    let owns = |x: pe_core::ObjectId| {
        snap.row_tag(x, "Hand").is_some()
            && snap.fields(x, "Hand").is_some_and(|f| {
                f.iter().any(|(n, v)| n == "owner" && matches!(v, pe_world::Value::Member(k) if *k == key))
            })
    };
    let mut stack = vec![o];
    while let Some(x) = stack.pop() {
        if owns(x) {
            return true;
        }
        stack.extend(snap.children(x));
    }
    false
}

/// The first of `ops` that would move the user (`is_user`, api.md 10.6): on an object of that set, a write, add or
/// removal of its `Transform`, or of a `Body` or `Shape` (which move it through physics), a re-parent, or a delete
/// (pe-world destroys the whole subtree, so deleting an ancestor deletes the user too; world-model.md 7.5).
fn moves_user(
    snap: &WorldSnapshot,
    ops: &[pe_world::edit::Op],
    is_user: impl Fn(pe_core::ObjectId) -> bool,
) -> Option<usize> {
    use pe_world::edit::{Op, Target};
    let movers: Vec<_> =
        ["Transform", "Body", "Shape"].iter().filter_map(|n| snap.component(n).map(|d| d.id())).collect();
    let target = |op: &Op| match op {
        Op::SetField { id: Target::Existing(o), component, .. }
        | Op::RemoveRow { id: Target::Existing(o), component, .. }
            if movers.contains(component) =>
        {
            Some(*o)
        }
        Op::SetRow { id: Target::Existing(o), row } if movers.contains(&row.component) => Some(*o),
        Op::Reparent { id: Target::Existing(o), .. } | Op::Delete { id: Target::Existing(o) } => Some(*o),
        _ => None,
    };
    ops.iter().position(|op| target(op).is_some_and(&is_user))
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
    /// Shared with the change log, which reads the newest snapshot when it prepares a re-send (api.md 9.4).
    src: Arc<S>,
    queue: Arc<EditQueue>,
    log: Arc<ChangeLog>,
    /// Each connection's table of versions seen, behind its own lock, so one connection's apply never waits on
    /// another's query.
    versions: Mutex<HashMap<ConnectionId, Arc<Mutex<VersionTable>>>>,
    rates: Mutex<HashMap<String, Bucket>>,
    rate: Rate,
    /// The machine's build service (api.md 7.1); `None` answers `unavailable`.
    builds: Option<Arc<dyn Builds>>,
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

/// The producer label pe-world records for this caller's changes (api.md 4.5).
fn actor(c: &Caller) -> Actor {
    match c.producer.kind {
        ProducerKind::Hand => Actor::Person,
        ProducerKind::Agent => Actor::Agent {
            harness: c.producer.harness.clone().unwrap_or_default(),
            principal: c.member.into(),
            requested_by: c.producer.requested_by.map(Into::into),
        },
    }
}

fn forbidden(what: &str) -> ApiError {
    ApiError::new(ErrorCode::Forbidden, format!("This token may not {what}."))
}

impl<S: WorldSource + 'static> Api<S> {
    /// The API over `src`, queueing live changes on `queue`, which the simulation thread drains, and learning
    /// their fates from `log`, which pe-app feeds from each publication's change record.
    pub fn new(src: S, queue: Arc<EditQueue>, log: Arc<ChangeLog>) -> Self {
        queue.track_into(log.clone());
        let src = Arc::new(src);
        let latest = src.clone();
        log.read_with(move || latest.latest());
        Api {
            src,
            queue,
            log,
            versions: Mutex::new(HashMap::new()),
            rates: Mutex::new(HashMap::new()),
            rate: Rate::TX_DEFAULT,
            builds: None,
        }
    }

    /// One transaction from this principal's bucket, or `rate_limited`.
    fn take_rate(&self, c: &Caller) -> Result<(), ApiError> {
        let mut r = self.rates.lock().unwrap_or_else(|e| e.into_inner());
        let b = r.entry(c.principal.clone()).or_insert_with(|| Bucket::new(self.rate, Instant::now()));
        b.take(Instant::now()).map_err(rate_limited)
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
        let views = Views { src: &*self.src, me: self.src.head_of(c.member) };
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
        self.take_rate(c)?;
        let snap = self.base(tx.base)?;
        let view = WorldView::new(snap.clone(), self.src.head_of(c.member));
        let table = self.table(c.connection);
        let table = table.lock().unwrap_or_else(|e| e.into_inner());
        let lookup = |id: ObjectId| table.get(&id).cloned();
        let outcome_refused = |f: Box<pe_seams::world_api::Refusal>| Outcome {
            dry_run: opts.mode == Mode::DryRun,
            status: pe_seams::world_api::Status::Refused,
            repaired_at: None,
            is_final: false,
            change: None,
            frame: None,
            final_frame: snap.final_frame().0,
            created: Default::default(),
            changed: vec![],
            count: 0,
            refusal: Some(*f),
            diff: None,
            resent_as: None,
            held: None,
            activation: None,
            scripts: Default::default(),
        };
        let r = resolve_tx(&view, &tx, &lookup);
        drop(table);
        let r = match r {
            Ok(r) => r,
            Err(f) => return Ok(outcome_refused(f)),
        };
        // A `code` op is compiled once, here, before any entry exists (api.md 7); what can be refused without a build
        // is refused first.
        match crate::edit::precheck_scripts(&r, &snap) {
            Ok(()) => {}
            Err(Unmapped::Refused(f)) => return Ok(outcome_refused(f)),
            Err(Unmapped::Failed(err)) => return Err(err),
        }
        let built = match r.ops.iter().position(|o| matches!(o, crate::resolve::ROp::Code { .. })) {
            None => None,
            Some(i) => {
                let crate::resolve::ROp::Code { source, .. } = &r.ops[i] else { unreachable!() };
                let op = r.op_of.get(i).copied().unwrap_or(i as u32);
                match self.build_for_commit(source, op)? {
                    Ok(b) => Some(b),
                    Err(f) => return Ok(outcome_refused(f)),
                }
            }
        };
        let e = match to_world_built(&r, actor(c), &snap, self.src.head_of(c.member), built.as_ref()) {
            Ok(e) => e,
            Err(Unmapped::Refused(f)) => return Ok(outcome_refused(f)),
            Err(Unmapped::Failed(err)) => return Err(err),
        };
        self.submit(c, &snap, &r, e, opts)
    }

    /// The rest of `apply` (and `revert`) once the change is pe-world's payload: the dry run, then for a live change
    /// the blobs, the queue, the receipt and the change log (api.md 9.2 steps 3-6).
    fn submit(
        &self,
        c: &Caller,
        snap: &Arc<WorldSnapshot>,
        r: &Resolved,
        e: WorldEdit,
        opts: &ApplyOpts,
    ) -> Result<Outcome, ApiError> {
        // A live change can't go ahead yet; a dry run still answers.
        // - A `confirm` token (C11) would hold each change for its owner's approval; that isn't built (coordinator, past M1), so it fails closed rather than apply without the approval it promises.
        // - Spec 5.1: a joiner watches first and acts once caught up.
        // An agent doesn't move the user it acts for unless that user switched it on locally (api.md 10.6): refused
        // in a dry run too, since the change itself is what isn't allowed. The setting is read only on a hit.
        if c.producer.kind == ProducerKind::Agent
            && let Some(i) = {
                let head = self.src.head_of(c.member).map(pe_core::ObjectId::from);
                moves_user(snap, &e.tx.ops, |o| is_user(snap, o, head, c.member.into()))
            }
            && !self.src.session().agents_move_user
        {
            let mut f = refusal_of(
                RefusalCode::NotPermitted,
                "This change would move or delete the user you act for (their head, hands, avatar root or what \
                 holds it), and their \"let agents move me\" setting (agents_move_user) is off.",
                Some(
                    "Leave their avatar where it is, or ask them to switch on \"let agents move me\" in their own \
                     settings.",
                ),
            );
            f.op = e.op_of.get(i).copied();
            return Ok(Outcome { dry_run: opts.mode == Mode::DryRun, ..refused_outcome(snap, f) });
        }
        let blocked = if opts.mode == Mode::DryRun {
            None
        } else if c.confirm {
            Some((
                RefusalCode::NotPermitted,
                "Confirmation isn't available yet; ask the owner to issue a token without it.",
                None,
            ))
        } else if !self.src.session().caught_up {
            Some((
                RefusalCode::NotReady,
                "This machine is still catching up with the world; it can change it once caught up.",
                Some("Wait for watch's caught_up session event (or session's caught_up), then apply again."),
            ))
        } else {
            None
        };
        if let Some((code, message, next)) = blocked {
            return Ok(refused_outcome(snap, refusal_of(code, message, next)));
        }
        let draft = self.src.draft_for(c.member);
        let mut checked = dry_run(snap, r, &e, &draft);
        if let Some(f) = checked.refusal.as_mut() {
            self.with_winner(f);
        }
        if opts.mode == Mode::DryRun {
            return Ok(checked);
        }
        // Live. Refused against the base: answered now, never queued.
        if checked.status == Status::Refused {
            return Ok(Outcome { dry_run: false, ..checked });
        }
        self.src.put_blobs(&e.blobs);
        let owner = Owner {
            principal: c.principal.as_str().into(),
            op_of: e.op_of.clone().into(),
            refs: r.refs.iter().map(|(k, v)| (k.clone(), *v)).collect(),
        };
        // Kept for the re-send backstop (api.md 9.4): shared, with its slots read from the schema now.
        let kept = Kept {
            tx: Arc::new(e.tx.clone()),
            slots: Arc::new(crate::resend::slots(&e.tx, snap.schema())),
            line: draft.line,
            author: draft.author,
            first: Instant::now(),
        };
        let ticket = self
            .queue
            .submit_kept(Submission { tx: e.tx.clone(), base: snap.frame().0 }, owner, kept)
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
                    // As watch reports it: a final change is final, as a final refusal is.
                    is_final: fate.is_final || fate.status == Status::Final,
                    frame: fate.frame.or(Some(frame)),
                    // As of the answer, not of the read it was checked against: a final change's is past its frame.
                    final_frame: self.src.latest().final_frame().0.max(queued.final_frame),
                    ..queued
                };
                crate::watch::backstop(&mut o, fate.resent_as, fate.note);
                match &fate.outcome {
                    None => {}
                    Some(pe_seams::Outcome::Applied { created, .. }) => {
                        // pe-world creates in new-index order (a copy's block depth-first, as resolve numbered it).
                        // A revert reserves no new indices; its Undeletes may still report restored ids.
                        if !r.refs.is_empty() && created.len() != r.new_count as usize {
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
                        // The script it committed: the asset whose pending commit (or row, once active) is this
                        // entry's, so a re-sent entry's derived id is found as well (api.md 7, 9.4).
                        let commit = e.tx.ops.iter().find_map(|o| match o {
                            pe_world::edit::Op::CommitScript { script, .. } => Some(*script),
                            _ => None,
                        });
                        if let (Some(script), Some(entry)) = (commit, fate.step.as_ref().map(|s| s.entry)) {
                            let found = [Some(&now), then.as_ref()].into_iter().flatten().find_map(|s| {
                                let p = s.pending().iter().map(|(a, p)| (a, p.version));
                                let m = s.modules().iter().map(|(a, m)| (a, m.version));
                                p.chain(m).find(|(_, v)| v.entry == entry).map(|(a, v)| (*a, v))
                            });
                            if let Some((a, v)) = found {
                                o.scripts.insert(
                                    crate::outcome::script_key(r, &script),
                                    pe_seams::world_api::ScriptVersion {
                                        id: crate::resolve::asset_text(&a),
                                        version: crate::world::version_text(&v),
                                    },
                                );
                            }
                        }
                    }
                    Some(pe_seams::Outcome::Refused { op, reason }) => {
                        o.count = 0;
                        // A re-sent change's refusal names the re-sent entry's ops (the later-word rule may have
                        // dropped some), mapped back to the API's through its owner.
                        let carrier = fate
                            .resent_as
                            .and(fate.step.as_ref())
                            .and_then(|s| self.src.edit(s.entry))
                            .zip(fate.owner.as_ref());
                        o.refusal = Some(match carrier {
                            Some((tx, owner)) => {
                                let tx = pe_world::edit::Transaction::clone(&tx);
                                let e = WorldEdit { tx, blobs: vec![], op_of: owner.op_of.to_vec() };
                                map_refusal(snap, &e, *op, reason)
                            }
                            None => map_refusal(snap, &e, *op, reason),
                        });
                        if let Some(f) = o.refusal.as_mut() {
                            self.with_winner(f);
                        }
                    }
                    Some(pe_seams::Outcome::Unknown { repaired_at }) => {
                        o.status = Status::Unknown;
                        o.repaired_at = Some(repaired_at.frame.0);
                    }
                    Some(pe_seams::Outcome::Dropped { .. }) => {
                        o.count = 0;
                        o.status = Status::Dropped;
                    }
                }
                o
            }
            // Agreement's reasons (engine-architect): not this member's next entry is pe-app's bug; a
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
                    // S3 v1.7 (@api-eng's mapping, api.md 9.4): this machine's frame clock stepped back; retryable.
                    SubmitError::BelowOwnStamp => (
                        RefusalCode::NotReady,
                        "This machine's clock is catching up; try again.".to_string(),
                        Some("Apply again in a moment.".to_string()),
                    ),
                    // S3 v1.7: this member has left the session; nothing more is accepted from it.
                    SubmitError::AfterLeave => (
                        RefusalCode::Invalid,
                        "This member has left the session; it can't make changes any more.".to_string(),
                        None,
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

    /// `history` (api.md 5.5): the changes stepped here, newest first, filtered and paged.
    pub fn history(
        &self,
        c: &Caller,
        q: &pe_seams::world_api::HistoryQuery,
    ) -> Result<pe_seams::world_api::History, ApiError> {
        use crate::history::{LIMIT_DEFAULT, LIMIT_MAX, cursor_of, key, parse_cursor, summary};
        if !c.scopes.contains(&Scope::Read) {
            return Err(forbidden("read the world"));
        }
        let after = q.cursor.as_ref().map(parse_cursor).transpose()?;
        let limit = q.limit.unwrap_or(LIMIT_DEFAULT).clamp(1, LIMIT_MAX) as usize;
        let latest = self.src.latest();
        let select: Option<HashSet<ObjectId>> = match &q.select {
            None => None,
            Some(s) => {
                let view = WorldView::new(latest.clone(), self.src.head_of(c.member));
                let b = Budget::new(EXAMINE_MAX);
                let ids = select_in(&view, s, &b);
                if b.over() {
                    return Err(ApiError::arg("where", "This selector examines too many objects; narrow it."));
                }
                Some(ids.into_iter().collect())
            }
        };
        let mut fates = self.log.stepped();
        fates.sort_by_key(|f| std::cmp::Reverse(key(f.step.as_ref().expect("stepped"))));
        let mut entries = vec![];
        let mut more = None;
        for f in fates {
            let step = f.step.as_ref().expect("stepped");
            if after.is_some_and(|a| key(step) >= a) {
                continue;
            }
            let frame = step.frame.0;
            if q.from_frame.is_some_and(|x| frame < x) || q.to_frame.is_some_and(|x| frame > x) {
                continue;
            }
            if q.member.is_some_and(|m| MemberId::from(step.author) != m) {
                continue;
            }
            if q.mine && !f.owner.as_ref().is_some_and(|o| *o.principal == *c.principal) {
                continue;
            }
            let tx = self.src.edit(step.entry);
            let producer = tx.as_deref().map(|t| crate::watch::producer(&t.actor));
            if let Some(k) = q.producer
                && producer.as_ref().is_none_or(|p| p.kind != k)
            {
                continue;
            }
            if let Some(sel) = &select
                && !crate::watch::touched_ids(tx.as_deref(), &step.outcome).iter().any(|i| sel.contains(i))
            {
                continue;
            }
            if entries.len() == limit {
                more = Some(());
                break;
            }
            let snap = self.src.at(frame).unwrap_or_else(|| latest.clone());
            entries.push((
                step.clone(),
                pe_seams::world_api::HistoryEntry {
                    change: f.change,
                    by: pe_seams::world_api::MemberBrief {
                        member: step.author.into(),
                        name: self.src.member_name(step.author.into()).unwrap_or_default(),
                    },
                    producer: producer.unwrap_or(pe_seams::world_api::Producer {
                        kind: ProducerKind::Hand,
                        harness: None,
                        principal: None,
                        requested_by: None,
                    }),
                    label: tx.as_deref().and_then(|t| t.label.clone()),
                    frame,
                    is_final: f.is_final || f.status == Status::Final,
                    status: f.status,
                    repaired_at: match &step.outcome {
                        pe_seams::Outcome::Unknown { repaired_at } => Some(repaired_at.frame.0),
                        _ => None,
                    },
                    summary: summary(tx.as_deref(), &step.outcome, &snap),
                    activation: self.log.activation_of(f.change).map(|a| self.note(&a)),
                },
            ));
        }
        let more = more.and_then(|_| entries.last().map(|(s, _)| cursor_of(s)));
        Ok(pe_seams::world_api::History { entries: entries.into_iter().map(|(_, e)| e).collect(), more })
    }

    /// `revert` (api.md 5.5): the inverse pe-world recorded for `change`, applied as a new change of this caller's,
    /// labelled "Revert: <label>", with its preconditions on the versions the change left (`if_version: "any"`
    /// drops them). A dry run previews it.
    pub fn revert(&self, c: &Caller, change: TxId, opts: &ApplyOpts) -> Result<Outcome, ApiError> {
        if !c.scopes.contains(&Scope::Edit) {
            return Err(forbidden("change the world"));
        }
        let fate = self
            .log
            .get(change)
            .ok_or_else(|| ApiError::arg("change", "This change isn't in this session's history on this machine."))?;
        let inverse = match (fate.status, fate.outcome.as_ref()) {
            (Status::Dropped, _) => {
                return Err(ApiError::arg("change", "This change was dropped; nothing of it remains to revert."));
            }
            (_, Some(pe_seams::Outcome::Applied { inverse: Some(b), .. })) => b.clone(),
            (_, Some(pe_seams::Outcome::Applied { inverse: None, .. })) => {
                return Err(ApiError::arg("change", "This change isn't an edit, so it can't be reverted."));
            }
            _ => return Err(ApiError::arg("change", "This change didn't apply, so there is nothing to revert.")),
        };
        let mut tx = pe_world::edit::Transaction::decode(&inverse)
            .ok_or_else(|| ApiError::new(ErrorCode::Internal, "The change's recorded inverse couldn't be read."))?;
        self.take_rate(c)?;
        let was = fate.step.as_ref().and_then(|s| self.src.edit(s.entry)).and_then(|t| t.label.clone());
        let mut label = match was {
            Some(l) => format!("Revert: {l}"),
            None => "Revert".to_string(),
        };
        if label.len() > pe_world::edit::MAX_LABEL {
            let mut cut = pe_world::edit::MAX_LABEL;
            while !label.is_char_boundary(cut) {
                cut -= 1;
            }
            label.truncate(cut);
        }
        tx.label = Some(label.clone());
        tx.actor = actor(c);
        if opts.if_version.as_ref().is_some_and(|v| v.0 == "any") {
            tx.preconditions.clear();
        }
        let n = tx.ops.len() as u32;
        let r = Resolved {
            label,
            ops: vec![],
            preconditions: vec![],
            refs: Default::default(),
            script_refs: Default::default(),
            new_count: 0,
            count: u64::from(n),
            op_of: vec![],
        };
        let e = WorldEdit { tx, blobs: vec![], op_of: (0..n).collect() };
        self.submit(c, &self.src.latest(), &r, e, opts)
    }

    /// `schema` (api.md 5.6), from the newest snapshot's registry.
    pub fn schema(
        &self,
        c: &Caller,
        q: &pe_seams::world_api::SchemaQuery,
    ) -> Result<pe_seams::world_api::Schema, ApiError> {
        if !c.scopes.contains(&Scope::Read) {
            return Err(forbidden("read the world"));
        }
        crate::schema::schema(&self.src.latest(), q)
    }

    /// `watch` (api.md 5.4): the change stream after `cursor`, described for this caller, waiting up to `wait` (at
    /// most 30 s) on the caller's thread for something to report. `motion` and `final_only` are not built yet and
    /// are refused as invalid rather than ignored [I].
    pub fn watch(&self, c: &Caller, f: &Filter, cursor: Option<&Cursor>, wait: Duration) -> Result<Changes, ApiError> {
        if !c.scopes.contains(&Scope::Read) {
            return Err(forbidden("read the world"));
        }
        if f.motion.is_some() {
            return Err(ApiError::arg("motion", "Sampled motion isn't available yet; query the poses instead."));
        }
        if f.final_only {
            return Err(ApiError::arg(
                "final_only",
                "final_only isn't available yet; `mine` reports when a change is final.",
            ));
        }
        let limit = f.limit.unwrap_or(WATCH_LIMIT).clamp(1, WATCH_LIMIT_MAX) as usize;
        let wait = wait.min(WATCH_WAIT_MAX);
        let me = self.src.head_of(c.member);
        let select: Option<HashSet<ObjectId>> = match &f.select {
            None => None,
            Some(s) => {
                let view = WorldView::new(self.src.latest(), me);
                let b = Budget::new(EXAMINE_MAX);
                let ids = select_in(&view, s, &b);
                if b.over() {
                    return Err(ApiError::arg("where", "This selector examines too many objects; narrow it."));
                }
                Some(ids.into_iter().collect())
            }
        };
        let changes_on = f.changes != Some(false);
        let session_on = f.session != Some(false);
        let is_mine = |o: Option<&Owner>, t: TxId| o.is_some_and(|o| *o.principal == *c.principal) || f.of.contains(&t);
        let snap_at = |frame: Option<Frame>| frame.and_then(|fr| self.src.at(fr)).unwrap_or_else(|| self.src.latest());
        let name = |m: MemberId| self.src.member_name(m).unwrap_or_default();
        let describe = |item: &Arc<FeedItem>| -> Option<Vec<Said>> {
            let mut out = vec![];
            match item.as_ref() {
                FeedItem::Submitted { change, frame, owner } => {
                    if is_mine(owner.as_ref(), *change) {
                        let snap = self.src.latest();
                        let o = mine(*change, (Status::Queued, false), None, Some(*frame), owner.as_ref(), None, &snap);
                        out.push(Said::Mine(Box::new(o)));
                    }
                }
                FeedItem::Stepped { step, undid, status, is_final, owner } => {
                    let change = TxId::from(step.entry);
                    let own = is_mine(owner.as_ref(), change);
                    let applied = matches!(step.outcome, pe_seams::Outcome::Applied { .. });
                    if !(own || changes_on && (applied || undid.is_some())) {
                        return None;
                    }
                    let snap = snap_at(Some(step.frame.0));
                    let tx = self.src.edit(step.entry);
                    if changes_on {
                        // A retraction says what it undid: the earlier step that applied (@chunks-reviewer's M2).
                        let shown = undid.as_ref().unwrap_or(step);
                        let d = Describe { filter: f, select: select.as_ref(), snap: snap.clone(), me };
                        if let Some(ch) = d.change(shown, undid.is_some(), tx.as_deref(), name(step.author.into())) {
                            out.push(Said::Change(Box::new(ch)));
                        }
                    }
                    if own {
                        out.push(Said::Mine(Box::new(mine(
                            change,
                            (*status, *is_final),
                            Some(step),
                            None,
                            owner.as_ref(),
                            tx,
                            &snap,
                        ))));
                    }
                }
                FeedItem::Settled { change, status, is_final, step, owner, resent_as, note } => {
                    let own = is_mine(owner.as_ref(), *change);
                    let undone = *status == Status::Dropped
                        && step.as_ref().is_some_and(|s| matches!(s.outcome, pe_seams::Outcome::Applied { .. }));
                    if !(own || changes_on && undone) {
                        return None;
                    }
                    let snap = snap_at(step.as_ref().map(|s| s.frame.0));
                    let tx = step.as_ref().and_then(|s| self.src.edit(s.entry));
                    if changes_on
                        && undone
                        && let Some(s) = step
                    {
                        let d = Describe { filter: f, select: select.as_ref(), snap: snap.clone(), me };
                        if let Some(ch) = d.change(s, true, tx.as_deref(), name(s.author.into())) {
                            out.push(Said::Change(Box::new(ch)));
                        }
                    }
                    if own {
                        let mut o = mine(*change, (*status, *is_final), step.as_ref(), None, owner.as_ref(), tx, &snap);
                        crate::watch::backstop(&mut o, *resent_as, *note);
                        out.push(Said::Mine(Box::new(o)));
                    }
                }
                FeedItem::Final { frame } => {
                    if session_on {
                        out.push(Said::Session(SessionEvent::Final { frame: *frame }));
                    }
                }
                FeedItem::Session(e) => {
                    if session_on {
                        out.push(Said::Session(e.clone()));
                    }
                }
                FeedItem::Activation { change, owner, activation } => {
                    // The second report of a code change (api.md 5.3.2): final, with its version's fate.
                    if is_mine(owner.as_ref(), *change) {
                        let snap = self.src.latest();
                        let frame = self.log.get(*change).and_then(|f| f.frame);
                        let mut o = mine(*change, (Status::Final, true), None, frame, owner.as_ref(), None, &snap);
                        o.activation = Some(self.note(activation));
                        out.push(Said::Mine(Box::new(o)));
                    }
                }
                FeedItem::HeldDiscarded { handle, counts } => {
                    let theirs = counts.iter().find(|(p, _)| **p == *c.principal);
                    if let (true, Some((_, count))) = (session_on, theirs) {
                        out.push(Said::Session(SessionEvent::HeldDiscarded { handle: handle.clone(), count: *count }));
                    }
                }
            }
            (!out.is_empty()).then_some(out)
        };
        let read = self.log.feed().read(cursor, describe, limit, wait).map_err(|e| match e {
            StreamError::Invalid => ApiError::arg("cursor", "Not a cursor of this session's change stream; call watch without one for a fresh cursor."),
            StreamError::Expired { fresh } => {
                let mut e = ApiError::new(
                    ErrorCode::CursorExpired,
                    "The changes after this cursor are no longer kept; query the world again, then watch from the fresh cursor.",
                );
                e.field = Some("cursor".into());
                e.details = Some(serde_json::json!({ "cursor": fresh.0 }));
                e
            }
        })?;
        let mut out = Changes {
            cursor: read.cursor,
            changes: vec![],
            mine: vec![],
            session: vec![],
            motion: vec![],
            more: read.more,
        };
        for said in read.items.into_iter().flatten() {
            match said {
                Said::Change(ch) => out.changes.push(*ch),
                Said::Mine(o) => out.mine.push(*o),
                Said::Session(s) => out.session.push(s),
            }
        }
        // Every version a watch result carries enters the connection's table (spec 5.4, 4.2).
        let table = self.table(c.connection);
        let mut table = table.lock().unwrap_or_else(|e| e.into_inner());
        for ch in out.changes.iter().filter(|ch| !ch.retracted) {
            for iv in ch.created.iter().chain(&ch.reparented).filter(|iv| !iv.version.0.is_empty()) {
                table.saw(iv.id, iv.version.clone(), ch.frame);
            }
        }
        for o in &out.mine {
            for iv in o.created.values().filter(|iv| !iv.version.0.is_empty()) {
                table.saw(iv.id, iv.version.clone(), o.frame.unwrap_or(o.final_frame));
            }
        }
        Ok(out)
    }
}

/// Role names from pe-world's permission bits (world-model.md 5.4; M1 stores bits, not names, @world-model): an admin, a guest (may edit), or a watcher.
pub fn role_of(perms: u32) -> (&'static str, Vec<String>) {
    if perms & pe_world::perm::ADMIN != 0 {
        ("admin", vec!["edit".into(), "admin".into()])
    } else if perms & pe_world::perm::EDIT != 0 {
        ("guest", vec!["edit".into()])
    } else {
        ("watcher", vec![])
    }
}

impl<S: WorldSource + 'static> Api<S> {
    /// A stale refusal's winner (api.md 5.3.2: the winner's change, member, producer and value), from the change log
    /// and the winning entry, once this machine has stepped it; the refusal is left as it was otherwise.
    fn with_winner(&self, f: &mut Refusal) {
        let Some(change) =
            f.details.as_ref().and_then(|d| d.get("winner_change")).and_then(|v| v.as_str()).and_then(TxId::parse)
        else {
            return;
        };
        let Some(step) = self.log.get(change).and_then(|fate| fate.step) else { return };
        // Without the winning entry its producer is unknown here: no winner rather than a guess.
        let Some(tx) = self.src.edit(step.entry) else { return };
        // The field's value, only while the object's newest version is still the winner's: then it is the value the
        // winner wrote, not a later change's or a prediction's.
        let latest = self.src.latest();
        let value = match (&f.object, f.field.as_deref().and_then(|k| k.split_once('.'))) {
            (Some(o), Some((comp, field)))
                if latest.info(o.id.into()).is_some_and(|i| i.version.entry == step.entry) =>
            {
                WorldView::new(latest, None)
                    .fields(o.id, comp, &pe_seams::world_api::ObjRef("parent".into()))
                    .and_then(|mut m| m.remove(field))
            }
            _ => None,
        };
        f.winner = Some(pe_seams::world_api::Winner {
            change,
            frame: step.frame.0,
            by: pe_seams::world_api::MemberBrief {
                member: step.author.into(),
                name: self.src.member_name(step.author.into()).unwrap_or_default(),
            },
            producer: crate::watch::producer(&tx.actor),
            label: tx.label.clone(),
            value,
        });
    }

    /// `session` (api.md 5.1): pe-app's facts, and the world state (frame, members and their roles, scale) from the
    /// newest snapshot. Every member of the world is listed, each marked whether it is here, and then anyone here
    /// whose admission hasn't stepped yet, as a watcher (@chunks-reviewer's Low on 930cd094); `kept` is whether an
    /// admin is here.
    pub fn session(&self, c: &Caller) -> pe_seams::world_api::SessionInfo {
        use pe_seams::world_api::{Limits, Me, MemberInfo, SessionInfo, WorldBrief};
        let facts = self.src.session();
        let snap = self.src.latest();
        let role = |m: MemberId| role_of(snap.perms(m.into()).unwrap_or(0));
        let name = |m: MemberId| self.src.member_name(m).unwrap_or_default();
        let mut members: Vec<MemberInfo> = snap
            .members()
            .map(|(k, p)| {
                let m: MemberId = k.into();
                MemberInfo { member: m, name: name(m), role: role_of(p).0.into(), here: facts.here.contains(&m) }
            })
            .collect();
        for m in &facts.here {
            if snap.perms((*m).into()).is_none() {
                members.push(MemberInfo { member: *m, name: name(*m), role: role_of(0).0.into(), here: true });
            }
        }
        let kept = members.iter().any(|m| m.here && m.role == "admin");
        let scale = self
            .src
            .head_of(c.member)
            .and_then(|h| snap.world_transform(h.into()))
            .map_or(1.0, |t| f64::from(t.scale[0]));
        let (r, may) = role(c.member);
        let scopes: Vec<Scope> = c.scopes.iter().copied().collect();
        SessionInfo {
            world: WorldBrief { line: facts.line.into(), name: facts.world_name },
            me: Me {
                member: c.member,
                name: name(c.member),
                principal: c.principal.clone(),
                producer: c.producer.clone(),
                role: r.into(),
                may,
                scopes,
                scale,
            },
            members,
            frame: snap.frame().0,
            final_frame: snap.final_frame().0,
            tick_hz: facts.tick_hz,
            link: facts.link,
            caught_up: facts.caught_up,
            health: crate::changes::session_health(self.log.health()),
            kept,
            limits: Limits {
                tx_per_s: self.rate.per_s as u32,
                tx_burst: self.rate.burst as u32,
                ops_per_tx: crate::resolve::OPS_MAX as u32,
                objects_per_tx: crate::resolve::NEW_MAX,
                payload_bytes_per_tx: pe_world::edit::MAX_ROW_BYTES as u32,
                query_limit_max: crate::query::LIMIT_MAX,
                watch_wait_ms_max: WATCH_WAIT_MAX.as_millis() as u32,
                capture_size_max: [1920, 1080],
            },
            held: self.log.held_for(&c.principal),
        }
    }

    /// `held` (api.md 9.4): applies or discards the caller's own changes in a held batch. Applied changes go to the
    /// queue again in order, through the backstop's preparation, with a fresh bound; each answers as `apply` with
    /// `wait: "local"` would, under its original id.
    /// An activation record as the API says it (api.md 7): objects capped, failures by member name and reason name.
    fn note(&self, a: &pe_seams::Activation) -> pe_seams::world_api::ActivationNote {
        use pe_seams::world_api::{ActivationNote, BuildFailedBy, MemberBrief, VoidWhy};
        use pe_seams::{ActivationOutcome, VoidedWhy};
        const REKEYED_SHOWN: usize = 64;
        match &a.outcome {
            ActivationOutcome::Activated { rekeyed } => ActivationNote::Activated {
                frame: a.frame.0,
                rekeyed: rekeyed.iter().take(REKEYED_SHOWN).map(|o| (*o).into()).collect(),
                more: rekeyed.len().saturating_sub(REKEYED_SHOWN) as u32,
            },
            ActivationOutcome::Voided { why, objects, more, failed } => ActivationNote::Voided {
                frame: a.frame.0,
                why: match why {
                    VoidedWhy::Unattested => VoidWhy::Unattested,
                    VoidedWhy::RekeyBlockedAtActivation => VoidWhy::RekeyBlocked,
                    VoidedWhy::RekeyTooLarge => VoidWhy::RekeyTooLarge,
                    VoidedWhy::AuthorRevokedAtActivation => VoidWhy::AuthorRevoked,
                    VoidedWhy::QuorumUnreachable => VoidWhy::QuorumUnreachable,
                },
                failed: failed
                    .iter()
                    .map(|(k, r)| BuildFailedBy {
                        member: MemberBrief {
                            member: (*k).into(),
                            name: self.src.member_name((*k).into()).unwrap_or_default(),
                        },
                        reason: r.name().to_string(),
                    })
                    .collect(),
                // Already capped by pe-world (REKEY_BLOCKED_NAMED, 16, with `more`): no second cap here.
                blocked: objects.iter().map(|o| (*o).into()).collect(),
                more: *more,
            },
        }
    }

    /// The machine's build service, which pe-app starts before it creates the ports (api.md 7.1).
    pub fn with_builds(mut self, b: Arc<dyn Builds>) -> Self {
        self.builds = Some(b);
        self
    }

    /// The author's build of a `code` op's source (api.md 7): a failure or a margin refuses the op as `compile`
    /// with its diagnostics, and nothing is submitted; no service, or a wait past the deadline, fails the call.
    fn build_for_commit(
        &self,
        source: &str,
        op: u32,
    ) -> Result<Result<crate::builds::Built, Box<pe_seams::world_api::Refusal>>, ApiError> {
        let Some(b) = &self.builds else {
            return Err(ApiError::new(
                ErrorCode::Unavailable,
                "This machine has no build service, so it can't commit scripts.",
            ));
        };
        let archive =
            archive_of(source).map_err(|e| ApiError::arg("source", format!("The source can't be packed: {e}.")))?;
        let compile = |d: Vec<pe_seams::world_api::Diagnostic>| {
            let mut f = refusal_of(
                RefusalCode::Compile,
                "The script didn't build, so nothing was submitted.",
                Some("Fix what the diagnostics say (code_check runs the same build), then apply again."),
            );
            f.op = Some(op);
            f.details = Some(serde_json::json!({ "diagnostics": d }));
            Box::new(f)
        };
        match b.build(&archive, Instant::now() + CODE_CHECK_WAIT) {
            Ok(built) => Ok(Ok(built)),
            Err(BuildRefused::Failed(d)) => Ok(Err(compile(d))),
            Err(BuildRefused::Margin { which, measured }) => Ok(Err(compile(vec![margin_diagnostic(which, measured)]))),
            Err(BuildRefused::Timeout) => Err(ApiError::new(
                ErrorCode::Timeout,
                "The build waited too long behind other builds; try again (a build that started finishes and is kept).",
            )),
            Err(BuildRefused::Unavailable(why)) => {
                Err(ApiError::new(ErrorCode::Unavailable, format!("The build service can't build now: {why}.")))
            }
        }
    }

    /// `code_check` (api.md 7, 7.1): builds and validates one script's source as its author's machine would for a
    /// commit, through the build port, and changes nothing. It takes a token from the caller's bucket, as a build
    /// is the costliest call [I].
    pub fn code_check(&self, c: &Caller, r: &CodeCheck, deadline: Instant) -> Result<Diagnostics, ApiError> {
        if !c.scopes.contains(&Scope::Edit) {
            return Err(forbidden("check code"));
        }
        let Some(b) = &self.builds else {
            return Err(ApiError::new(
                ErrorCode::Unavailable,
                "This machine has no build service, so code can't be checked here.",
            ));
        };
        let archive =
            archive_of(&r.source).map_err(|e| ApiError::arg("source", format!("The source can't be packed: {e}.")))?;
        self.take_rate(c)?;
        match b.build(&archive, deadline) {
            Ok(built) => Ok(diagnostics_of(&built)),
            Err(BuildRefused::Failed(d)) => Ok(refusal_diagnostics(d)),
            Err(BuildRefused::Margin { which, measured }) => {
                Ok(refusal_diagnostics(vec![margin_diagnostic(which, measured)]))
            }
            Err(BuildRefused::Timeout) => Err(ApiError::new(
                ErrorCode::Timeout,
                "The build waited too long behind other builds; try again (a build that started finishes and is kept).",
            )),
            Err(BuildRefused::Unavailable(why)) => {
                Err(ApiError::new(ErrorCode::Unavailable, format!("The build service can't build now: {why}.")))
            }
        }
    }

    pub fn held(&self, c: &Caller, handle: &str, action: HeldAction) -> Result<Vec<Outcome>, ApiError> {
        if !c.scopes.contains(&Scope::Edit) {
            return Err(forbidden("change the world"));
        }
        let apply = action == HeldAction::Apply;
        let Some(changes) = self.log.held(&c.principal, handle, apply) else {
            return Err(ApiError::arg("handle", format!("No held batch {handle} with changes of yours.")));
        };
        if !apply {
            return Ok(vec![]);
        }
        let deadline = Instant::now() + Duration::from_millis(2_000);
        let snap = self.src.latest();
        Ok(changes
            .into_iter()
            .map(|t| {
                let f = self.log.wait(t, Wait::Local, deadline);
                let (status, is_final) = f.as_ref().map_or((Status::Queued, false), |f| (f.status, f.is_final));
                let step = f.as_ref().and_then(|f| f.step.clone());
                let tx = step.as_ref().and_then(|s| self.src.edit(s.entry));
                let owner = f.as_ref().and_then(|f| f.owner.clone());
                let frame = f.as_ref().and_then(|f| f.frame);
                let mut o = mine(t, (status, is_final), step.as_ref(), frame, owner.as_ref(), tx, &snap);
                if let Some(f) = &f {
                    crate::watch::backstop(&mut o, f.resent_as, f.note);
                }
                o
            })
            .collect())
    }
}

/// The API as S6's trait, which pe-mcp and pe-app serve (`Arc<dyn WorldApi>`). `capture` and `code_check` answer
/// that they aren't available yet (view.capture with @ballpit-viz; the logic check with WS-logic).
impl<S: WorldSource + 'static> pe_seams::world_api::WorldApi for Api<S> {
    fn session(&self, c: &Caller) -> pe_seams::world_api::SessionInfo {
        Api::session(self, c)
    }
    fn schema(
        &self,
        c: &Caller,
        q: &pe_seams::world_api::SchemaQuery,
    ) -> Result<pe_seams::world_api::Schema, ApiError> {
        Api::schema(self, c, q)
    }
    fn query(&self, c: &Caller, q: &Query) -> Result<QueryResult, ApiError> {
        Api::query(self, c, q)
    }
    fn apply(&self, c: &Caller, tx: Transaction, opts: &ApplyOpts) -> Result<Outcome, ApiError> {
        Api::apply(self, c, tx, opts)
    }
    fn watch(&self, c: &Caller, f: &Filter, cursor: Option<&Cursor>, wait: Duration) -> Result<Changes, ApiError> {
        Api::watch(self, c, f, cursor, wait)
    }
    fn history(
        &self,
        c: &Caller,
        q: &pe_seams::world_api::HistoryQuery,
    ) -> Result<pe_seams::world_api::History, ApiError> {
        Api::history(self, c, q)
    }
    fn revert(&self, c: &Caller, change: TxId, opts: &ApplyOpts) -> Result<Outcome, ApiError> {
        Api::revert(self, c, change, opts)
    }
    fn capture(
        &self,
        _c: &Caller,
        _r: &pe_seams::world_api::CaptureRequest,
        _deadline: Instant,
    ) -> Result<pe_seams::world_api::Capture, ApiError> {
        Err(ApiError::new(ErrorCode::CaptureUnavailable, "view_capture isn't available on this machine yet."))
    }
    fn code_check(&self, c: &Caller, r: &CodeCheck) -> Result<Diagnostics, ApiError> {
        Api::code_check(self, c, r, Instant::now() + CODE_CHECK_WAIT)
    }
    fn held(&self, c: &Caller, handle: &str, action: HeldAction) -> Result<Vec<Outcome>, ApiError> {
        Api::held(self, c, handle, action)
    }
}

/// One thing `watch` says about a stream item.
enum Said {
    Change(Box<pe_seams::world_api::ChangeItem>),
    Mine(Box<Outcome>),
    Session(SessionEvent),
}

/// How long `code_check` waits for its build in the queue [I]; the build itself, once started, finishes. 30 s plus a
/// build inside the 15 s margin stays under the 60 s request timeout common MCP clients default to, and a retry is
/// cheap because a started build is kept (@design-reviewer). pe-mcp runs each call on tokio's blocking
/// pool, so a waiting check holds no other tool call.
const CODE_CHECK_WAIT: Duration = Duration::from_secs(30);

/// `watch`'s items per call: the default and the most (api.md 5.4 [I]).
const WATCH_LIMIT: u32 = 100;
const WATCH_LIMIT_MAX: u32 = 1_000;
/// The longest `watch` long-polls (api.md 5.4).
const WATCH_WAIT_MAX: Duration = Duration::from_secs(30);

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
    /// A member of the world who isn't here.
    const BOB: MemberKey = MemberKey([2; 32]);
    /// Someone here whose admission hasn't stepped yet.
    const CAROL: MemberKey = MemberKey([3; 32]);

    fn id(n: u128) -> pe_core::ObjectId {
        pe_core::ObjectId::from_u128(n)
    }

    /// One published frame: a Table and a Lamp under it; Alice may edit.
    #[derive(Clone)]
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
            HEAD.with(|h| h.get()).map(|n| id(n).into())
        }
        fn draft_for(&self, member: MemberId) -> Draft {
            Draft { author: member.into(), line: LineId(Hash([7; 32])), chain: ChainId(Hash([8; 32])), seq: 1 }
        }
        fn put_blobs(&self, _: &[Vec<u8>]) {}
        fn edit(&self, entry: EntryRef) -> Option<Arc<pe_world::edit::Transaction>> {
            EDITS.with(|m| m.borrow().get(&entry).cloned().map(Arc::new))
        }
        fn member_name(&self, m: MemberId) -> Option<String> {
            (m == ALICE.into()).then(|| "Alice".to_string())
        }
        fn session(&self) -> SessionFacts {
            SessionFacts {
                world_name: "Test".into(),
                line: LineId(Hash([0; 32])),
                here: vec![ALICE.into(), CAROL.into()],
                link: pe_seams::world_api::Link::Fine,
                caught_up: !CATCHING_UP.with(|c| c.get()),
                tick_hz: 60,
                agents_move_user: MOVE_OK.with(|c| c.get()),
            }
        }
    }

    thread_local! {
        /// The edit entries the test's machine holds, by entry.
        static EDITS: std::cell::RefCell<HashMap<EntryRef, pe_world::edit::Transaction>> = Default::default();
        /// Whether the test's machine is catching up.
        static CATCHING_UP: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
        /// The caller's head (an object number), if it has one.
        static HEAD: std::cell::Cell<Option<u128>> = const { std::cell::Cell::new(None) };
        /// The owning user's "agents may move me" setting.
        static MOVE_OK: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }

    #[test]
    fn an_agent_does_not_move_its_user_unless_the_user_allows_it() {
        // The Lamp is the caller's head and the Hand its hand; the Table, their avatar root, holds them and a Cup.
        let a = api_with(|w| {
            for (n, name) in [(30u128, "Hand"), (40, "Cup")] {
                let text = w.blobs().put(name.as_bytes());
                let v = pe_seams::Version { entry: EntryRef(Hash([n as u8; 32])), op: 0 };
                w.create(id(n), Some(id(10)), ALICE, v).unwrap();
                w.insert(id(n), Transform::local([0.0, 1.0, 0.0])).unwrap();
                w.insert(id(n), Name { text }).unwrap();
            }
            w.insert(id(30), pe_world::components::Hand { owner: ALICE, ..Default::default() }).unwrap();
        });
        let c = caller(&[Scope::Read, Scope::Edit]);
        HEAD.with(|h| h.set(Some(20)));
        let dry = ApplyOpts { mode: Mode::DryRun, ..Default::default() };
        let code = |c: &Caller, ops: serde_json::Value, opts: &ApplyOpts| {
            let o = a.apply(c, tx(ops), opts).unwrap();
            (o.status, o.refusal.map(|f| (f.code, f.op)))
        };
        let refused = |op: u32| (Status::Refused, Some((RefusalCode::NotPermitted, Some(op))));
        let at = json!({"relative_to": "world_origin", "offset": [5.0, 0.0, 0.0]});
        let move_table = json!([
            {"op": "create", "parent": "/Table", "name": "Cup"},
            {"op": "set", "id": "/Table", "field": "Transform.position", "value": at}
        ]);
        assert_eq!(code(&c, move_table.clone(), &dry), refused(1));
        assert_eq!(code(&c, move_table.clone(), &ApplyOpts::default()), refused(1));
        assert!(a.queue.is_empty());
        let turn_head =
            json!([{"op": "set", "id": "/Table/Lamp", "field": "Transform.rotation_deg", "value": [0, 90, 0]}]);
        assert_eq!(code(&c, turn_head, &dry), refused(0));
        assert_eq!(code(&c, json!([{"op": "reparent", "id": "/Table/Lamp", "parent": "/Table"}]), &dry), refused(0));
        // The hand, a Body that physics would move, and deleting the avatar root or the head.
        let move_hand =
            json!([{"op": "set", "id": "/Table/Hand", "field": "Transform.position", "value": {"offset": [0, 2, 0]}}]);
        assert_eq!(code(&c, move_hand, &dry), refused(0));
        let body = json!([{"op": "add", "id": "/Table", "component": "Body", "fields": {"friction": 0.5}}]);
        assert_eq!(code(&c, body, &dry), refused(0));
        assert_eq!(code(&c, json!([{"op": "delete", "id": "/Table"}]), &dry), refused(0));
        assert_eq!(code(&c, json!([{"op": "delete", "id": "/Table/Lamp"}]), &dry), refused(0));
        // A revert that would delete the head goes through the same check.
        let made = edit_entry(13, vec![]);
        let inverse = pe_world::edit::Transaction {
            actor: pe_world::edit::Actor::Person,
            label: None,
            preconditions: vec![],
            ops: vec![pe_world::edit::Op::Delete { id: pe_world::edit::Target::Existing(id(20)) }],
            blobs: vec![],
        };
        let applied = pe_seams::Outcome::Applied {
            created: vec![id(20)],
            overwrote: vec![],
            notes: vec![],
            inverse: Some(inverse.encode().into()),
        };
        a.log.record(&[stepped(made, ALICE, 4, applied)], None);
        let o = a.revert(&c, made.into(), &dry).unwrap();
        assert_eq!(o.refusal.map(|f| f.code), Some(RefusalCode::NotPermitted));
        // Building around the user, and the avatar root's other children, are fine.
        assert_eq!(code(&c, json!([{"op": "create", "parent": "/Table", "name": "Cup"}]), &dry).0, Status::Accepted);
        let move_cup =
            json!([{"op": "set", "id": "/Table/Cup", "field": "Transform.position", "value": {"offset": [0, 2, 0]}}]);
        assert_eq!(code(&c, move_cup.clone(), &dry).0, Status::Accepted);
        assert_eq!(code(&c, json!([{"op": "delete", "id": "/Table/Cup"}]), &dry).0, Status::Accepted);
        // A person's own tools aren't an agent.
        let mut person = c.clone();
        person.producer.kind = ProducerKind::Hand;
        assert_eq!(code(&person, move_table.clone(), &dry).0, Status::Accepted);
        // The user switched it on.
        MOVE_OK.with(|m| m.set(true));
        assert_eq!(code(&c, move_table, &dry).0, Status::Accepted);
    }

    #[test]
    fn session_and_the_trait_and_confirm_tokens_fail_closed() {
        use pe_seams::world_api::WorldApi;
        let a = api();
        let c = caller(&[Scope::Read, Scope::Edit]);
        let w: &dyn WorldApi = &a;
        let s = w.session(&c);
        assert_eq!((s.world.name.as_str(), s.frame, s.tick_hz, s.caught_up, s.kept), ("Test", 4, 60, true, true));
        assert_eq!((s.me.role.as_str(), s.me.name.as_str(), s.me.scale), ("admin", "Alice", 1.0));
        // Every member, each marked whether it is here, then anyone here not yet admitted, as a watcher.
        let here: Vec<(MemberId, &str, bool)> = s.members.iter().map(|m| (m.member, m.role.as_str(), m.here)).collect();
        assert_eq!(
            here,
            vec![(ALICE.into(), "admin", true), (BOB.into(), "guest", false), (CAROL.into(), "watcher", true)]
        );
        assert_eq!(s.limits.ops_per_tx, 4096);
        assert!(w.query(&c, &Query::default()).is_ok());
        // A confirm token: live changes refused, dry runs answer.
        let mut held = c.clone();
        held.confirm = true;
        let t = || tx(json!([{"op": "create", "parent": "/Table", "name": "Cup"}]));
        let o = w.apply(&held, t(), &ApplyOpts::default()).unwrap();
        assert_eq!(o.refusal.map(|f| f.code), Some(RefusalCode::NotPermitted));
        assert!(a.queue.is_empty());
        let dry = ApplyOpts { mode: Mode::DryRun, ..Default::default() };
        assert_eq!(w.apply(&held, t(), &dry).unwrap().status, Status::Accepted);
    }

    #[test]
    fn while_catching_up_a_live_change_is_not_ready_and_a_dry_run_answers() {
        let a = api();
        let c = caller(&[Scope::Read, Scope::Edit]);
        CATCHING_UP.with(|x| x.set(true));
        let t = || tx(json!([{"op": "create", "parent": "/Table", "name": "Cup"}]));
        let o = a.apply(&c, t(), &ApplyOpts::default()).unwrap();
        assert_eq!((o.status, o.refusal.map(|f| f.code)), (Status::Refused, Some(RefusalCode::NotReady)));
        assert!(a.queue.is_empty());
        let dry = ApplyOpts { mode: Mode::DryRun, ..Default::default() };
        assert_eq!(a.apply(&c, t(), &dry).unwrap().status, Status::Accepted);
        // watch carries the session news pe-app pushes.
        let start = a.watch(&c, &Filter::default(), None, Duration::ZERO).unwrap().cursor;
        a.log.session_event(SessionEvent::CaughtUp);
        CATCHING_UP.with(|x| x.set(false));
        let w = a.watch(&c, &Filter::default(), Some(&start), Duration::ZERO).unwrap();
        assert_eq!(w.session, vec![SessionEvent::CaughtUp]);
        assert_eq!(
            a.apply(&c, t(), &ApplyOpts { wait: Wait::None, ..Default::default() }).unwrap().status,
            Status::Queued
        );
    }

    fn api() -> Api<Src> {
        api_with(|_| {})
    }

    /// [`api`], with `more` run on its world before the snapshot.
    fn api_with(more: impl FnOnce(&mut World)) -> Api<Src> {
        let mut w = World::new(1, BlobStore::default());
        w.set_member(ALICE, perm::EDIT | perm::ADMIN);
        w.set_member(BOB, perm::EDIT);
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
        more(&mut w);
        w.set_frame(CoreFrame(4));
        Api::new(Src(w.snapshot(CoreFrame(4))), Arc::new(EditQueue::new(2, || {})), Arc::new(ChangeLog::new()))
    }

    /// A build service that answers what it was given, and counts its builds.
    struct FakeBuilds(Mutex<Vec<Result<crate::builds::Built, BuildRefused>>>, std::sync::atomic::AtomicUsize);
    impl Builds for FakeBuilds {
        fn build(&self, archive: &[u8], _deadline: Instant) -> Result<crate::builds::Built, BuildRefused> {
            let files = pe_world::script::source_files(archive).expect("an archive pe-world reads");
            assert_eq!(files.iter().map(|(p, _)| *p).collect::<Vec<_>>(), vec!["src/lib.rs"]);
            self.1.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.0.lock().unwrap().remove(0)
        }
    }

    #[test]
    fn code_check_goes_through_the_build_port() {
        use crate::builds::{Built, Margin};
        use pe_seams::world_api::DiagnosticKind;
        let check = CodeCheck { source: "pub fn f() {}".into() };
        let c = caller(&[Scope::Read, Scope::Edit]);
        // No build service: unavailable; read-only callers: forbidden.
        assert_eq!(api().code_check(&c, &check, Instant::now()).unwrap_err().code, ErrorCode::Unavailable);
        let built = Built {
            module: pe_core::Hash([3; 32]),
            toolchain: pe_core::Hash([1; 32]),
            sdk: pe_core::Hash([2; 32]),
            components: vec![pe_world::script::tag_desc(&pe_core::Hash([4; 32]))],
            systems: vec!["pulse".into()],
            warnings: vec![],
            build_ms: 812,
            peak_bytes: None,
        };
        let rustc = pe_seams::world_api::Diagnostic {
            kind: DiagnosticKind::Rustc,
            severity: pe_seams::world_api::Severity::Error,
            message: "expected `;`".into(),
            file: Some("src/lib.rs".into()),
            line: Some(1),
            column: Some(9),
            code: None,
        };
        let b = Arc::new(FakeBuilds(
            Mutex::new(vec![
                Ok(built),
                Err(BuildRefused::Failed(vec![rustc.clone()])),
                Err(BuildRefused::Margin { which: Margin::Time, measured: 15_400 }),
                Err(BuildRefused::Timeout),
            ]),
            Default::default(),
        ));
        let a = api().with_builds(b.clone());
        assert_eq!(
            a.code_check(&caller(&[Scope::Read]), &check, Instant::now()).unwrap_err().code,
            ErrorCode::Forbidden
        );
        let d = a.code_check(&c, &check, Instant::now()).unwrap();
        assert!(d.ok);
        assert_eq!((d.module, d.systems, d.build_ms), (Some("03".repeat(32)), vec!["pulse".to_string()], Some(812)));
        assert!(d.components.is_empty(), "the version's tag isn't shown: {:?}", d.components);
        let d = a.code_check(&c, &check, Instant::now()).unwrap();
        assert_eq!((d.ok, d.diagnostics), (false, vec![rustc]));
        // A margin is named as such, never as a compile error.
        let d = a.code_check(&c, &check, Instant::now()).unwrap();
        assert_eq!((d.ok, d.diagnostics[0].kind), (false, DiagnosticKind::Margin));
        assert!(d.diagnostics[0].message.contains("15.4 s"), "{}", d.diagnostics[0].message);
        assert_eq!(a.code_check(&c, &check, Instant::now()).unwrap_err().code, ErrorCode::Timeout);
        assert_eq!(b.1.load(std::sync::atomic::Ordering::SeqCst), 4, "a forbidden call builds nothing");
    }

    /// A build service whose every build passes, declaring only the version's tag (world-model.md 7.9).
    struct Passes(std::sync::atomic::AtomicUsize);
    impl Builds for Passes {
        fn build(&self, archive: &[u8], _deadline: Instant) -> Result<crate::builds::Built, BuildRefused> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(crate::builds::Built {
                module: Hash([3; 32]),
                toolchain: Hash([1; 32]),
                sdk: Hash([2; 32]),
                components: vec![pe_world::script::tag_desc(&v_of_archive(archive))],
                systems: vec![],
                warnings: vec![],
                build_ms: 5,
                peak_bytes: None,
            })
        }
    }

    fn v_of_archive(archive: &[u8]) -> Hash {
        Hash(pe_world::BlobRef::of(archive).hash)
    }

    /// A world where Alice has committed a script with source `"// v1"`, active once Bob attests (both vote, so the
    /// quorum is 2) if `activate`; and that script's id.
    fn with_script(w: &mut World, activate: bool) -> [u8; 32] {
        use pe_world::edit::{Actor, AssetTarget, Op, Transaction as WTx};
        let archive = archive_of("// v1").unwrap();
        let v = v_of_archive(&archive);
        let commit = Op::commit_script(
            AssetTarget::New(0),
            None,
            archive.into(),
            Hash([1; 32]),
            Hash([2; 32]),
            Hash([3; 32]),
            4,
        );
        let ops = vec![Op::Register { components: vec![pe_world::script::tag_desc(&v)] }, commit];
        let tx = WTx { actor: Actor::Person, label: None, preconditions: vec![], ops, blobs: vec![] };
        let draft =
            pe_world::Draft { author: ALICE, line: LineId(Hash([7; 32])), chain: ChainId(Hash([9; 32])), seq: 1 };
        let (o, mut after) = w.dry_run_draft_state(CoreFrame(4), &draft, &tx);
        assert!(matches!(o, pe_seams::Outcome::Applied { .. }), "{o:?}");
        let asset = pe_world::script::asset_created(draft.line, draft.author, draft.chain, draft.seq, 0);
        if activate {
            let attest = Op::AttestBuild { script: asset, version: v, module: Hash([3; 32]) };
            let tx = WTx { actor: Actor::Person, label: None, preconditions: vec![], ops: vec![attest], blobs: vec![] };
            let bob = pe_world::Draft { author: BOB, line: draft.line, chain: ChainId(Hash([10; 32])), seq: 1 };
            let (o, attested) = after.dry_run_draft_state(CoreFrame(4), &bob, &tx);
            assert!(matches!(o, pe_seams::Outcome::Applied { .. }), "{o:?}");
            after = attested;
            let end = after.end_frame(CoreFrame(4));
            assert_eq!(end.activated.len(), 1, "{end:?}");
        }
        *w = after;
        asset
    }

    use pe_seams::world_api::{Schema, SchemaQuery};

    /// One world op as `who` on chain `chain`, applied to `w` (a test's setup).
    fn world_op(w: &mut World, who: MemberKey, chain: u8, ops: Vec<pe_world::edit::Op>) -> pe_world::Draft {
        use pe_world::edit::{Actor, Transaction as WTx};
        let tx = WTx { actor: Actor::Person, label: None, preconditions: vec![], ops, blobs: vec![] };
        let draft =
            pe_world::Draft { author: who, line: LineId(Hash([7; 32])), chain: ChainId(Hash([chain; 32])), seq: 1 };
        let (o, after) = w.dry_run_draft_state(CoreFrame(4), &draft, &tx);
        assert!(matches!(o, pe_seams::Outcome::Applied { .. }), "{o:?}");
        *w = after;
        draft
    }

    /// A script with source `source`, committed by Alice on chain `chain` and declaring `declares` (one `F32` field
    /// `speed` each) besides its tag; active once Bob attests if `activate`. Its id, and each declared component's id.
    fn script_declaring(
        w: &mut World,
        source: &str,
        chain: u8,
        declares: &[&str],
        activate: bool,
    ) -> ([u8; 32], Vec<pe_sim::ComponentId>) {
        use pe_world::edit::{AssetTarget, Op};
        let archive = archive_of(source).unwrap();
        let v = v_of_archive(&archive);
        let fields = vec![pe_sim::FieldDesc::new("speed", pe_sim::FieldKind::F32, 0, 1)];
        let mut components = vec![pe_world::script::tag_desc(&v)];
        let mut ids = vec![];
        for n in declares {
            let c = pe_world::script::script_component_id(&v, n, fields.clone()).unwrap();
            components.push(pe_sim::ComponentDesc::new(c, n, fields.clone()).unwrap());
            ids.push(c);
        }
        let commit = Op::commit_script(
            AssetTarget::New(0),
            None,
            archive.into(),
            Hash([1; 32]),
            Hash([2; 32]),
            Hash([3; 32]),
            4,
        );
        let d = world_op(w, ALICE, chain, vec![Op::Register { components }, commit]);
        let asset = pe_world::script::asset_created(d.line, d.author, d.chain, d.seq, 0);
        if activate {
            world_op(w, BOB, chain + 100, vec![Op::AttestBuild { script: asset, version: v, module: Hash([3; 32]) }]);
            assert_eq!(w.end_frame(CoreFrame(4)).activated.len(), 1);
        }
        (asset, ids)
    }

    /// Attaches `asset` (its active version's tag) to `obj`, with a row of each of `rows` (`speed` = `speed`).
    fn attach_with(w: &mut World, chain: u8, asset: [u8; 32], obj: u128, rows: &[pe_sim::ComponentId], speed: f32) {
        use pe_world::edit::{Op, Row, Target};
        let v = w.snapshot(CoreFrame(4)).modules()[&asset].v();
        let tag = pe_world::script::tag_desc(&v).id();
        let mut ops =
            vec![Op::SetRow { id: Target::Existing(id(obj)), row: Row { component: tag, instance: 0, bytes: vec![] } }];
        for c in rows {
            let row = Row { component: *c, instance: 0, bytes: speed.to_le_bytes().to_vec() };
            ops.push(Op::SetRow { id: Target::Existing(id(obj)), row });
        }
        world_op(w, ALICE, chain, ops);
    }

    /// Two active scripts that both declare `Spin` (B also a `Transform`), a pending one declaring `Glow`: A on the
    /// table with a Spin row, A and B both on the lamp; and A's and B's ids.
    fn two_spins() -> (Api<Src>, [u8; 32], [u8; 32]) {
        let (mut a, mut b) = ([0; 32], [0; 32]);
        let api = api_with(|w| {
            let (sa, ca) = script_declaring(w, "// a", 30, &["Spin"], true);
            let (sb, cb) = script_declaring(w, "// b", 40, &["Spin", "Transform"], true);
            script_declaring(w, "// c", 50, &["Glow"], false);
            attach_with(w, 60, sa, 10, &ca, 2.0);
            attach_with(w, 61, sa, 20, &ca, 3.0);
            attach_with(w, 62, sb, 20, &cb[..1], 4.0);
            (a, b) = (sa, sb);
        });
        (api, a, b)
    }

    #[test]
    fn schema_lists_only_active_script_types_with_their_script() {
        let (a, sa, sb) = two_spins();
        let c = caller(&[Scope::Read]);
        let Schema::List { types, .. } = a.schema(&c, &Default::default()).unwrap() else { panic!() };
        let named = |n: &str| types.iter().filter(|t| t.name == n).map(|t| t.script.clone()).collect::<Vec<_>>();
        let (ta, tb) = (crate::resolve::asset_text(&sa), crate::resolve::asset_text(&sb));
        let (mut spins, mut want) = (named("Spin"), vec![Some(ta.clone()), Some(tb.clone())]);
        spins.sort();
        want.sort();
        assert_eq!(spins, want);
        assert_eq!(named("Transform"), [None, Some(tb.clone())], "the engine's, then B's");
        assert!(named("Glow").is_empty(), "a pending version's types aren't shown");
        assert!(named(pe_world::script::TAG_NAME).is_empty(), "nor are tags");
        // A bare type two scripts declare is refused with both full forms; a prefix picks one.
        let q = |t: &str| SchemaQuery { type_name: Some(t.into()), ..Default::default() };
        let e = a.schema(&c, &q("Spin")).unwrap_err();
        assert!(
            e.message.contains(&format!("Spin@{ta}")) && e.message.contains(&format!("Spin@{tb}")),
            "{}",
            e.message
        );
        let Schema::Type { name, fields, .. } = a.schema(&c, &q(&format!("Spin@{}", &ta[..8]))).unwrap() else {
            panic!()
        };
        assert_eq!((name, fields.contains_key("speed")), (format!("Spin@{ta}"), true));
        // The engine's Transform and B's share a bare name in a read with no object; `@engine` names the engine's.
        assert!(a.schema(&c, &q("Transform")).unwrap_err().message.contains("Transform@engine"));
        let Schema::Type { name, .. } = a.schema(&c, &q("Transform@engine")).unwrap() else { panic!() };
        assert_eq!(name, "Transform");
        assert!(a.schema(&c, &q("Spin@a:12")).unwrap_err().message.contains("at least 4 hex digits"));
    }

    #[test]
    fn the_schema_version_moves_when_a_version_activates() {
        let c = caller(&[Scope::Read]);
        let version = |activate| {
            let a = api_with(|w| {
                script_declaring(w, "// a", 30, &["Spin"], activate);
            });
            match a.schema(&c, &Default::default()).unwrap() {
                Schema::List { version, .. } => version,
                _ => unreachable!(),
            }
        };
        assert_ne!(version(false), version(true));
    }

    #[test]
    fn a_bare_name_resolves_among_the_scripts_attached_to_the_object() {
        let (a, sa, sb) = two_spins();
        let c = caller(&[Scope::Read, Scope::Edit]);
        let dry = ApplyOpts { mode: Mode::DryRun, ..Default::default() };
        let (ta, tb) = (crate::resolve::asset_text(&sa), crate::resolve::asset_text(&sb));
        let set = |obj: &str, field: &str| {
            a.apply(&c, tx(json!([{"op": "set", "id": obj, "field": field, "value": 1.5}])), &dry).unwrap()
        };
        // The table carries only A: `Spin` is A's.
        assert_eq!(set("/Table", "Spin.speed").status, Status::Accepted);
        // The lamp carries A and B: refused, with both full forms; qualified, either is fine.
        let f = set("/Table/Lamp", "Spin.speed").refusal.unwrap();
        assert_eq!(f.code, RefusalCode::Invalid);
        assert!(
            f.message.contains(&format!("Spin@{ta}")) && f.message.contains(&format!("Spin@{tb}")),
            "{}",
            f.message
        );
        assert_eq!(set("/Table/Lamp", &format!("Spin@{}.speed", &tb[..10])).status, Status::Accepted);
        // An engine name B also declares, on an object B is attached to: ambiguous, never silently the engine's.
        let pos = |obj: &str, comp: &str| {
            let op = json!([{"op": "set", "id": obj, "field": format!("{comp}.rotation_deg"), "value": [0, 90, 0]}]);
            a.apply(&c, tx(op), &dry).unwrap()
        };
        assert_eq!(pos("/Table/Lamp", "Transform").refusal.unwrap().code, RefusalCode::Invalid);
        assert_eq!(pos("/Table/Lamp", "Transform@engine").status, Status::Accepted);
        let o = pos("/Table", "Transform");
        assert_eq!(o.status, Status::Accepted, "B isn't on the table: {:?}", o.refusal);
    }

    #[test]
    fn adding_a_scripts_row_needs_the_script_attached_first() {
        let (a, sa, _) = two_spins();
        let c = caller(&[Scope::Read, Scope::Edit]);
        let dry = ApplyOpts { mode: Mode::DryRun, ..Default::default() };
        let ta = crate::resolve::asset_text(&sa);
        let create = json!({"op": "create", "as": "$cup", "parent": "/Table", "name": "Cup"});
        let add = json!({"op": "add", "id": "$cup", "component": "Spin", "fields": {"speed": 1.0}});
        let o = a.apply(&c, tx(json!([create, add])), &dry).unwrap();
        let f = o.refusal.unwrap();
        assert_eq!((f.code, f.op), (RefusalCode::Invalid, Some(1)));
        assert!(f.message.contains("attach the script first"), "{}", f.message);
        // Attached by an earlier op of the same change, it resolves.
        let attach = json!({"op": "attach", "script": ta, "to": ["$cup"], "mode": "linked"});
        let o = a.apply(&c, tx(json!([create, attach, add])), &dry).unwrap();
        assert_ne!(o.refusal.as_ref().map(|f| f.code), Some(RefusalCode::Invalid), "{:?}", o.refusal);
    }

    #[test]
    fn query_names_script_components_on_each_object_and_has_reads_every_match() {
        let (a, sa, sb) = two_spins();
        let c = caller(&[Scope::Read]);
        let (ta, tb) = (crate::resolve::asset_text(&sa), crate::resolve::asset_text(&sb));
        let one = |name: &str, fields: Option<&str>| {
            let q = Query {
                select: Some(Selector::Name(name.into())),
                fields: fields.map(|f| vec![f.to_string()]),
                ..Default::default()
            };
            a.query(&c, &q).unwrap().objects.remove(0)
        };
        assert!(one("Table", None).types.contains(&"Spin".to_string()));
        assert_eq!(one("Table", Some("Spin")).components["Spin"]["speed"], 2.0);
        // On the lamp `Spin` is ambiguous: each shows by its full form, and a bare projection reads nothing.
        let types = one("Lamp", None).types;
        assert!(types.contains(&format!("Spin@{ta}")) && types.contains(&format!("Spin@{tb}")), "{types:?}");
        assert!(!types.iter().any(|t| t == "Spin" || t.starts_with(pe_world::script::TAG_NAME)), "{types:?}");
        // A bare projection reads every match on the lamp, keyed by full form (ruling).
        let spin = one("Lamp", Some("Spin")).components;
        let mut keys: Vec<_> = spin.keys().cloned().collect();
        keys.sort();
        let mut want = vec![format!("Spin@{ta}"), format!("Spin@{tb}")];
        want.sort();
        assert_eq!(keys, want);
        assert_eq!(
            (&spin[&format!("Spin@{ta}")]["speed"], &spin[&format!("Spin@{tb}")]["speed"]),
            (&json!(3.0), &json!(4.0))
        );
        // A field list applies to each match.
        let q = Query {
            select: Some(Selector::Name("Lamp".into())),
            fields: Some(vec!["Spin.speed".into()]),
            ..Default::default()
        };
        let lamp = a.query(&c, &q).unwrap().objects.remove(0).components;
        assert_eq!(lamp.len(), 2, "{lamp:?}");
        let all = one("Lamp", Some("*")).components;
        assert_eq!(all[&format!("Spin@{ta}")]["speed"], 3.0);
        assert_eq!(all[&format!("Spin@{tb}")]["speed"], 4.0);
        // A projection is keyed as `types` names it: by the full form where the bare name is ambiguous, bare where it
        // isn't, whatever was asked.
        let t = one("Table", Some(&format!("Spin@{}.speed", &ta[..8]))).components;
        assert_eq!(t.keys().collect::<Vec<_>>(), ["Spin"]);
        let q = Query {
            select: Some(Selector::Name("Table".into())),
            fields: Some(vec!["Transform.rotation_deg".into(), "Transform@engine.position".into()]),
            ..Default::default()
        };
        let t = a.query(&c, &q).unwrap().objects.remove(0).components;
        assert_eq!((t.len(), t["Transform"].len()), (1, 2), "{t:?}");
        let keys: Vec<_> = one("Lamp", Some(&format!("Spin@{}", &tb[..6]))).components.into_keys().collect();
        assert_eq!(keys, [format!("Spin@{tb}")]);
        // Two requests that meet at one full form answer once, with both field lists.
        let q = Query {
            select: Some(Selector::Name("Lamp".into())),
            fields: Some(vec!["Spin.speed".into(), format!("Spin@{}.nothing", &ta[..8])]),
            ..Default::default()
        };
        let lamp = a.query(&c, &q).unwrap().objects.remove(0).components;
        assert_eq!(lamp.len(), 2, "{lamp:?}");
        assert_eq!(lamp[&format!("Spin@{ta}")]["speed"], 3.0, "{lamp:?}");
        let has = |s: &str| {
            let q = Query { select: Some(Selector::Has(s.into())), ..Default::default() };
            let mut n: Vec<String> = a.query(&c, &q).unwrap().objects.into_iter().map(|o| o.name).collect();
            n.sort();
            n
        };
        assert_eq!(has("Spin"), ["Lamp", "Table"]);
        assert_eq!(has(&format!("Spin@{}", &tb[..8])), ["Lamp"]);
        assert_eq!(has(&format!("Spin@{}", &ta[..8])), ["Lamp", "Table"]);
        assert_eq!(
            has(&format!("Spin@{}", ta[..8].to_ascii_uppercase().replacen("A:", "a:", 1))),
            ["Lamp", "Table"],
            "uppercase hex"
        );
        assert!(has("Glow").is_empty());
    }

    #[test]
    fn history_and_watch_name_an_attach_by_its_script() {
        use pe_world::edit::{Op, Row, Target, Transaction as WTx};
        let (a, sa, _) = two_spins();
        let snap = a.src.latest();
        let tag = pe_world::script::tag_desc(&snap.modules()[&sa].v()).id();
        let lamp = Target::Existing(id(20));
        let ops = vec![
            Op::SetRow { id: lamp, row: Row { component: tag, instance: 0, bytes: vec![] } },
            Op::RemoveRow { id: lamp, component: tag, instance: 0, tag: 0 },
        ];
        let tx = WTx { actor: pe_world::edit::Actor::Person, label: None, preconditions: vec![], ops, blobs: vec![] };
        let applied = pe_seams::Outcome::Applied { created: vec![], overwrote: vec![], notes: vec![], inverse: None };
        let hex: String = sa[..4].iter().map(|b| format!("{b:02x}")).collect();
        let s = crate::history::summary(Some(&tx), &applied, &snap);
        assert!(
            s.contains(&format!("ttached script {hex}… to")) && s.contains(&format!("detached script {hex}… from")),
            "{s}"
        );
        assert!(!s.contains(pe_world::script::TAG_NAME), "{s}");
    }

    #[test]
    fn a_code_op_builds_once_and_commits_a_new_script_by_a_derived_id() {
        let c = caller(&[Scope::Read, Scope::Edit]);
        let dry = ApplyOpts { mode: Mode::DryRun, ..Default::default() };
        let code = || tx(json!([{"op": "code", "id": "$spin", "source": "pub fn spin() {}"}]));
        // No build service: the call fails, nothing is refused as the author's.
        assert_eq!(api().apply(&c, code(), &dry).unwrap_err().code, ErrorCode::Unavailable);
        let b = Arc::new(Passes(Default::default()));
        let a = api().with_builds(b.clone());
        let o = a.apply(&c, code(), &dry).unwrap();
        assert_eq!(o.status, Status::Accepted, "{:?}", o.refusal);
        let d = a.src.draft_for(ALICE.into());
        let want = pe_world::script::asset_created(d.line, d.author, d.chain, d.seq, 0);
        assert_eq!(o.scripts["$spin"].id, crate::resolve::asset_text(&want));
        assert!(o.created.is_empty(), "a script is not an object");
        assert_eq!(b.0.load(std::sync::atomic::Ordering::SeqCst), 1);
        // A new script can't be attached until it activates.
        let o = a
            .apply(&c, tx(json!([{"op": "code", "id": "$s", "source": "pub fn s() {}", "attach": ["/Table"]}])), &dry)
            .unwrap();
        let f = o.refusal.unwrap();
        assert_eq!((f.code, f.op), (RefusalCode::NotReady, Some(0)));
        assert!(f.message.starts_with("Nothing was submitted."), "{}", f.message);
        // Refused before any build, as is a code op on an unknown script.
        let unknown = crate::resolve::asset_text(&[9; 32]);
        let o = a.apply(&c, tx(json!([{"op": "code", "id": unknown, "source": "// x"}])), &dry).unwrap();
        assert_eq!(o.refusal.unwrap().code, RefusalCode::NotFound);
        assert_eq!(b.0.load(std::sync::atomic::Ordering::SeqCst), 1);
        // A failed build refuses the op as `compile`, with the diagnostics.
        let rustc = pe_seams::world_api::Diagnostic {
            kind: pe_seams::world_api::DiagnosticKind::Rustc,
            severity: pe_seams::world_api::Severity::Error,
            message: "expected `;`".into(),
            file: Some("src/lib.rs".into()),
            line: Some(1),
            column: Some(9),
            code: None,
        };
        let fails = FakeBuilds(Mutex::new(vec![Err(BuildRefused::Failed(vec![rustc]))]), Default::default());
        let o = api().with_builds(Arc::new(fails)).apply(&c, code(), &dry).unwrap();
        let f = o.refusal.unwrap();
        assert_eq!((o.status, f.code, f.op), (Status::Refused, RefusalCode::Compile, Some(0)));
        assert_eq!(f.details.unwrap()["diagnostics"][0]["line"], 1);
    }

    #[test]
    fn an_active_script_is_committed_on_its_version_and_attached_by_its_tag() {
        let c = caller(&[Scope::Read, Scope::Edit]);
        let dry = ApplyOpts { mode: Mode::DryRun, ..Default::default() };
        let mut asset = [0; 32];
        let a = api_with(|w| asset = with_script(w, true)).with_builds(Arc::new(Passes(Default::default())));
        let id = crate::resolve::asset_text(&asset);
        // A new version, attached to the table in the same change: the current version's tag.
        let o =
            a.apply(&c, tx(json!([{"op": "code", "id": id, "source": "// v2", "attach": ["/Table"]}])), &dry).unwrap();
        assert_eq!(o.status, Status::Accepted, "{:?}", o.refusal);
        assert!(o.scripts.contains_key(&id));
        // On a stale base, `stale_version`, with the current version.
        let old = crate::world::version_text(&pe_seams::Version { entry: EntryRef(Hash([5; 32])), op: 1 });
        let stale = json!([{"op": "code", "id": id, "source": "// v3", "if_version": old}]);
        let f = a.apply(&c, tx(stale), &dry).unwrap().refusal.unwrap();
        assert_eq!(f.code, RefusalCode::StaleVersion, "{f:?}");
        let attach = json!([{"op": "attach", "script": id, "to": ["/Table", "/Table/Lamp"], "mode": "linked"}]);
        let o = a.apply(&c, tx(attach), &dry).unwrap();
        assert_eq!((o.status, o.count), (Status::Accepted, 2), "{:?}", o.refusal);
        // Not attached: nothing to detach.
        let detach = json!([{"op": "detach", "script": id, "from": ["/Table"]}]);
        assert_eq!(a.apply(&c, tx(detach), &dry).unwrap().refusal.unwrap().code, RefusalCode::NotFound);
        // An unknown script.
        let other = crate::resolve::asset_text(&[9; 32]);
        let o = a.apply(&c, tx(json!([{"op": "code", "id": other, "source": "// x"}])), &dry).unwrap();
        assert_eq!(o.refusal.unwrap().code, RefusalCode::NotFound);
    }

    #[test]
    fn a_pending_first_version_is_attached_only_after_it_activates() {
        let c = caller(&[Scope::Read, Scope::Edit]);
        let dry = ApplyOpts { mode: Mode::DryRun, ..Default::default() };
        let mut asset = [0; 32];
        let a = api_with(|w| asset = with_script(w, false));
        let id = crate::resolve::asset_text(&asset);
        let attach = json!([{"op": "attach", "script": id, "to": ["/Table"], "mode": "linked"}]);
        let f = a.apply(&c, tx(attach), &dry).unwrap().refusal.unwrap();
        assert_eq!((f.code, f.op), (RefusalCode::NotReady, Some(0)));
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
        assert_eq!((f.status, f.is_final), (Status::Refused, false), "final once the final frame passes it");
        a.log.record(&[], Some(7));
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
        a.log.record(&[], Some(6));
        assert_eq!(a.log.get(entry.into()).unwrap().status, Status::Final);
    }

    // ---- watch (api.md 5.4) ----

    fn edit_entry(e: u8, ops: Vec<pe_world::edit::Op>) -> EntryRef {
        let entry = EntryRef(Hash([e; 32]));
        let t = pe_world::edit::Transaction {
            actor: pe_world::edit::Actor::Agent { harness: "test".into(), principal: ALICE, requested_by: None },
            label: Some("t".into()),
            preconditions: vec![],
            ops,
            blobs: vec![],
        };
        EDITS.with(|m| m.borrow_mut().insert(entry, t));
        entry
    }

    fn stepped(entry: EntryRef, author: MemberKey, frame: u64, outcome: pe_seams::Outcome) -> pe_seams::StepRecord {
        pe_seams::StepRecord { entry, author, class: pe_seams::Class::Edit, frame: CoreFrame(frame), outcome }
    }

    fn applied(created: Vec<pe_core::ObjectId>) -> pe_seams::Outcome {
        pe_seams::Outcome::Applied { created, overwrote: vec![], notes: vec![], inverse: None }
    }

    /// A stale refusal names its winner: the change, its member, producer and label, from the change log and the
    /// winning entry (api.md 5.3.2).
    #[test]
    fn a_stale_refusal_names_the_winner() {
        let a = api();
        let c = caller(&[Scope::Read, Scope::Edit]);
        let won = edit_entry(20, vec![]);
        a.log.record(&[stepped(won, ALICE, 4, applied(vec![]))], None);
        let old = crate::world::version_text(&pe_seams::Version { entry: EntryRef(Hash([5; 32])), op: 0 });
        let dry = ApplyOpts { mode: Mode::DryRun, ..Default::default() };
        let del = json!([{"op": "delete", "id": "/Table/Lamp", "if_version": old}]);
        let f = a.apply(&c, tx(del), &dry).unwrap().refusal.unwrap();
        assert_eq!(f.code, RefusalCode::StaleVersion, "{f:?}");
        let w = f.winner.expect("the winner");
        assert_eq!((w.change, w.frame, w.by.member), (won.into(), 4, ALICE.into()));
        assert_eq!((w.producer.kind, w.label.as_deref()), (ProducerKind::Agent, Some("t")));
        // A winning entry this machine doesn't hold: no winner, rather than a guessed producer.
        EDITS.with(|m| m.borrow_mut().remove(&won));
        let a = api();
        a.log.record(&[stepped(EntryRef(Hash([20; 32])), ALICE, 4, applied(vec![]))], None);
        let del = json!([{"op": "delete", "id": "/Table/Lamp", "if_version": old}]);
        let f = a.apply(&c, tx(del), &dry).unwrap().refusal.unwrap();
        assert_eq!((f.code, f.winner), (RefusalCode::StaleVersion, None));
    }

    #[test]
    fn watch_follows_a_callers_own_change_from_queued_to_final() {
        let a = api();
        let c = caller(&[Scope::Read, Scope::Edit]);
        let f = Filter::default();
        let first = a.watch(&c, &f, None, Duration::ZERO).unwrap();
        assert!(first.changes.is_empty() && first.mine.is_empty(), "no cursor: now, and nothing");
        let none = ApplyOpts { wait: Wait::None, ..Default::default() };
        a.apply(&c, tx(json!([{"op": "create", "parent": "/Table", "name": "Cup", "as": "$cup"}])), &none).unwrap();
        let chain = pe_core::ChainId(Hash([1; 32]));
        let entry = edit_entry(
            4,
            vec![pe_world::edit::Op::Create {
                new: 0,
                parent: pe_world::edit::Target::Existing(id(10)),
                index: None,
                rows: vec![],
            }],
        );
        for (t, _) in a.queue.drain(4) {
            a.queue.settle(t, Receipt::Submitted { change: entry.into(), frame: 4, chain, seq: 3 });
        }
        a.log.record(&[stepped(entry, ALICE, 4, applied(vec![id(10)]))], None);
        let at = pe_seams::CutRef { line: LineId(Hash([0; 32])), height: 1, frame: CoreFrame(6) };
        let head = pe_seams::ChainHead { chain, author: ALICE, seq: 3, entry };
        a.log.confirm_cut(&pe_seams::Cut { at, heads: vec![head] }, &[]);
        a.log.record(&[], Some(6));
        let w = a.watch(&c, &f, Some(&first.cursor), Duration::ZERO).unwrap();
        let st: Vec<_> = w.mine.iter().map(|o| (o.status, o.change)).collect();
        let ch = Some(TxId::from(entry));
        assert_eq!(st, vec![(Status::Queued, ch), (Status::Accepted, ch), (Status::Final, ch)]);
        assert_eq!(w.mine[1].created["$cup"].id, id(10).into(), "the caller's own names for what it created");
        assert_eq!(w.changes.len(), 1);
        let item = &w.changes[0];
        assert_eq!((item.by.name.as_str(), item.label.as_deref(), item.frame), ("Alice", Some("t"), 4));
        assert_eq!((item.producer.kind, item.producer.harness.as_deref()), (ProducerKind::Agent, Some("test")));
        assert_eq!(item.created.iter().map(|c| c.id).collect::<Vec<_>>(), vec![id(10).into()]);
        // Read to the end: nothing more, and the cursor stays good.
        let again = a.watch(&c, &f, Some(&w.cursor), Duration::ZERO).unwrap();
        assert!(again.changes.is_empty() && again.mine.is_empty());
        // Another caller sees the change, but not as its own.
        let mut other = caller(&[Scope::Read]);
        other.principal = "someone-else".into();
        let o = a.watch(&other, &f, Some(&first.cursor), Duration::ZERO).unwrap();
        assert_eq!((o.changes.len(), o.mine.len()), (1, 0));
    }

    /// One of this caller's changes, submitted and stepped (applied) at frame 4 as `entry`, chain 1 seq 3; and a
    /// cursor from before it.
    fn own_change(a: &Api<Src>, c: &Caller, e: u8) -> (EntryRef, Cursor) {
        let first = a.watch(c, &Filter::default(), None, Duration::ZERO).unwrap().cursor;
        let none = ApplyOpts { wait: Wait::None, ..Default::default() };
        a.apply(c, tx(json!([{"op": "create", "parent": "/Table", "name": "Cup"}])), &none).unwrap();
        let chain = pe_core::ChainId(Hash([1; 32]));
        let entry = edit_entry(
            e,
            vec![pe_world::edit::Op::Create {
                new: 0,
                parent: pe_world::edit::Target::Existing(id(10)),
                index: None,
                rows: vec![],
            }],
        );
        for (t, _) in a.queue.drain(4) {
            a.queue.settle(t, Receipt::Submitted { change: entry.into(), frame: 4, chain, seq: 3 });
        }
        a.log.record(&[stepped(entry, ALICE, 4, applied(vec![id(10)]))], None);
        (entry, first)
    }

    // api.md 9.4: a change of this caller's void as late is re-sent ahead of fresh changes; `mine` tells it once,
    // final, under its own id with `resent_as`, while `changes` shows the retraction and the re-sent entry.
    #[test]
    fn a_late_void_is_re_sent_and_watch_tells_it_once() {
        let a = api();
        let c = caller(&[Scope::Read, Scope::Edit]);
        let (entry, first) = own_change(&a, &c, 4);
        a.log.confirm_cut(&cut_at(6, entry), &[(entry, pe_seams::VoidReason::Late)]);
        let got = a.queue.drain(4);
        assert_eq!(got.len(), 1, "the re-send, taken first");
        let chain = pe_core::ChainId(Hash([1; 32]));
        let re = edit_entry(
            5,
            vec![pe_world::edit::Op::Create {
                new: 0,
                parent: pe_world::edit::Target::Existing(id(10)),
                index: None,
                rows: vec![],
            }],
        );
        a.queue.settle(got[0].0, Receipt::Submitted { change: re.into(), frame: 7, chain, seq: 4 });
        a.log.record(&[stepped(re, ALICE, 7, applied(vec![id(12)]))], None);
        let at = pe_seams::CutRef { line: LineId(Hash([0; 32])), height: 2, frame: CoreFrame(8) };
        let head = pe_seams::ChainHead { chain, author: ALICE, seq: 4, entry: re };
        a.log.confirm_cut(&pe_seams::Cut { at, heads: vec![head] }, &[]);
        a.log.record(&[], Some(8));
        let w = a.watch(&c, &Filter::default(), Some(&first), Duration::ZERO).unwrap();
        let st: Vec<_> = w.mine.iter().map(|o| (o.status, o.change, o.resent_as)).collect();
        let (ch, rs) = (Some(TxId::from(entry)), Some(TxId::from(re)));
        assert_eq!(st, vec![(Status::Queued, ch, None), (Status::Accepted, ch, None), (Status::Final, ch, rs)]);
        assert_eq!(w.mine[2].frame, Some(7), "the re-sent entry's frame");
        let items: Vec<_> = w.changes.iter().map(|i| i.change).collect();
        assert_eq!(items, vec![TxId::from(entry), TxId::from(entry), TxId::from(re)], "applied, retracted, re-sent");
        assert!(a.session(&c).held.is_empty());
        let f = a.log.get(TxId::from(entry)).unwrap();
        assert_eq!((f.status, f.resent_as), (Status::Final, rs));
    }

    fn cut_at(frame: u64, entry: EntryRef) -> pe_seams::Cut {
        let at = pe_seams::CutRef { line: LineId(Hash([0; 32])), height: 1, frame: CoreFrame(frame) };
        let head = pe_seams::ChainHead { chain: pe_core::ChainId(Hash([1; 32])), author: ALICE, seq: 3, entry };
        pe_seams::Cut { at, heads: vec![head] }
    }

    // Coordinator: health from the view, in session and on watch; after a repair, each affected change
    // of the caller's resolves once, explicitly, as unknown with repaired_at.
    #[test]
    fn watch_tells_the_health_and_a_repaired_change_once_as_unknown() {
        let a = api();
        let c = caller(&[Scope::Read, Scope::Edit]);
        let (entry, first) = own_change(&a, &c, 4);
        assert_eq!(a.session(&c).health, pe_seams::world_api::SessionHealth::Agreed);
        let repairing = pe_seams::Health::Repairing { since: CoreFrame(3) };
        a.log.publish(&[], Some(2), repairing);
        assert_eq!(a.session(&c).health, pe_seams::world_api::SessionHealth::Repairing { since: 3 });
        // The quorum's cut includes it, but the final frame waits: no verdict is told while repairing.
        a.log.confirm_cut(&cut_at(6, entry), &[]);
        let cut = pe_seams::CutRef { line: LineId(Hash([0; 32])), height: 2, frame: CoreFrame(6) };
        let unknown = pe_seams::Outcome::Unknown { repaired_at: cut };
        a.log.publish(&[stepped(entry, ALICE, 4, unknown)], Some(6), pe_seams::Health::Agreed);
        a.log.publish(&[], Some(9), pe_seams::Health::Agreed);
        let w = a.watch(&c, &Filter::default(), Some(&first), Duration::ZERO).unwrap();
        let st: Vec<_> = w.mine.iter().map(|o| (o.status, o.is_final, o.repaired_at)).collect();
        assert_eq!(
            st,
            vec![(Status::Queued, false, None), (Status::Accepted, false, None), (Status::Unknown, true, Some(6))]
        );
        use pe_seams::world_api::SessionHealth as H;
        assert_eq!(
            w.session,
            vec![
                // Repairing comes before the rest of its view's news.
                SessionEvent::HealthChanged { health: H::Repairing { since: 3 } },
                SessionEvent::Final { frame: 2 },
                SessionEvent::HealthChanged { health: H::Agreed },
                SessionEvent::Final { frame: 6 },
                SessionEvent::Final { frame: 9 },
            ]
        );
        assert_eq!(a.session(&c).health, H::Agreed);
    }

    // Coordinator: the replica's Dropped and the cut's void say the same thing; whichever comes first
    // is told, once, with one retraction.
    #[test]
    fn a_void_and_the_replicas_dropped_record_are_told_once() {
        for void_first in [true, false] {
            let a = api();
            let c = caller(&[Scope::Read, Scope::Edit]);
            let (entry, first) = own_change(&a, &c, 4);
            // Back-stamped, not late: the re-send backstop leaves it alone (api.md 9.4).
            let dropped = pe_seams::Outcome::Dropped { reason: pe_seams::VoidReason::BackStamped };
            let void = || a.log.confirm_cut(&cut_at(6, entry), &[(entry, pe_seams::VoidReason::BackStamped)]);
            let record = || a.log.record(&[stepped(entry, ALICE, 4, dropped.clone())], Some(6));
            if void_first {
                void();
                record();
            } else {
                record();
                void();
            }
            let w = a.watch(&c, &Filter::default(), Some(&first), Duration::ZERO).unwrap();
            let st: Vec<_> = w.mine.iter().map(|o| (o.status, o.is_final)).collect();
            let want = vec![(Status::Queued, false), (Status::Accepted, false), (Status::Dropped, true)];
            assert_eq!(st, want, "void first: {void_first}");
            let retracted: Vec<_> = w.changes.iter().map(|ch| ch.retracted).collect();
            assert_eq!(retracted, vec![false, true], "void first: {void_first}");
        }
    }

    #[test]
    fn watch_reports_others_changes_narrowed_by_where_and_retractions() {
        let a = api();
        let c = caller(&[Scope::Read]);
        let start = a.watch(&c, &Filter::default(), None, Duration::ZERO).unwrap().cursor;
        let bob = MemberKey([2; 32]);
        let lamp = edit_entry(
            5,
            vec![pe_world::edit::Op::Reparent {
                id: pe_world::edit::Target::Existing(id(20)),
                parent: pe_world::edit::Target::Root,
                index: None,
                keep_world: true,
            }],
        );
        let table = edit_entry(6, vec![pe_world::edit::Op::Delete { id: pe_world::edit::Target::Existing(id(10)) }]);
        let refused = pe_seams::Outcome::Refused { op: Some(0), reason: pe_seams::Reason::NoPermission };
        a.log.record(
            &[
                stepped(lamp, bob, 4, applied(vec![])),
                stepped(table, bob, 4, applied(vec![])),
                stepped(EntryRef(Hash([7; 32])), bob, 4, refused.clone()),
            ],
            Some(3),
        );
        // A rollback re-steps the lamp's change, which no longer applies.
        a.log.record(&[stepped(lamp, bob, 4, refused)], None);
        let all = a.watch(&c, &Filter::default(), Some(&start), Duration::ZERO).unwrap();
        // The refused entry changed nothing; the retraction is a new item, the first stands.
        assert_eq!(all.changes.len(), 3);
        assert_eq!(all.changes[0].reparented[0].id, id(20).into());
        assert_eq!(all.changes[1].deleted, vec![id(10).into()]);
        assert!(!all.changes[0].retracted && all.changes[2].retracted);
        assert_eq!(all.changes[2].change, TxId::from(lamp));
        assert_eq!(all.session, vec![SessionEvent::Final { frame: 3 }]);
        assert!(all.mine.is_empty());
        // `where` narrows to the Lamp.
        let lamp_only = Filter { select: Some(Selector::Name("Lamp".into())), ..Default::default() };
        let w = a.watch(&c, &lamp_only, Some(&start), Duration::ZERO).unwrap();
        assert_eq!(w.changes.iter().map(|c| c.change).collect::<Vec<_>>(), vec![lamp.into(), lamp.into()]);
        // `changes: false` and `session: false` leave nothing for a stranger.
        let quiet = Filter { changes: Some(false), session: Some(false), ..Default::default() };
        let q = a.watch(&c, &quiet, Some(&start), Duration::ZERO).unwrap();
        assert!(q.changes.is_empty() && q.session.is_empty() && q.mine.is_empty());
        assert_eq!(q.cursor, all.cursor, "the cursor passes everything scanned");
    }

    // chunks-reviewer's M1 and M2 on 8642afdc.
    #[test]
    fn a_re_step_that_changes_nothing_is_not_repeated_and_a_retraction_says_what_it_undid() {
        let a = api();
        let c = caller(&[Scope::Read]);
        let start = a.watch(&c, &Filter::default(), None, Duration::ZERO).unwrap().cursor;
        let bob = MemberKey([2; 32]);
        let made = edit_entry(
            8,
            vec![
                pe_world::edit::Op::Create {
                    new: 0,
                    parent: pe_world::edit::Target::Existing(id(10)),
                    index: None,
                    rows: vec![],
                },
                pe_world::edit::Op::Reparent {
                    id: pe_world::edit::Target::New(0),
                    parent: pe_world::edit::Target::Root,
                    index: None,
                    keep_world: false,
                },
            ],
        );
        a.log.record(&[stepped(made, bob, 4, applied(vec![id(30)]))], None);
        // A rollback re-steps frame 4 with the same result, then a later one no longer applies it.
        a.log.record(&[stepped(made, bob, 4, applied(vec![id(30)]))], None);
        let refused = pe_seams::Outcome::Refused { op: Some(0), reason: pe_seams::Reason::NoPermission };
        a.log.record(&[stepped(made, bob, 4, refused)], None);
        let w = a.watch(&c, &Filter::default(), Some(&start), Duration::ZERO).unwrap();
        assert_eq!(w.changes.len(), 2, "the identical re-step is not reported again");
        let undo = &w.changes[1];
        assert!(undo.retracted);
        assert_eq!(undo.created.iter().map(|c| c.id).collect::<Vec<_>>(), vec![id(30).into()]);
        assert_eq!(undo.reparented.iter().map(|c| c.id).collect::<Vec<_>>(), vec![id(30).into()], "New(0) resolves");
    }

    #[test]
    fn watch_long_polls_and_refuses_bad_cursors_and_unbuilt_options() {
        let a = Arc::new(api());
        let c = caller(&[Scope::Read]);
        let f = Filter::default();
        let start = a.watch(&c, &f, None, Duration::ZERO).unwrap().cursor;
        let log = a.log.clone();
        let h = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            log.record(&[], Some(9));
        });
        let t0 = Instant::now();
        let w = a.watch(&c, &f, Some(&start), Duration::from_secs(5)).unwrap();
        h.join().unwrap();
        assert_eq!(w.session, vec![SessionEvent::Final { frame: 9 }]);
        assert!(t0.elapsed() < Duration::from_secs(4), "woken by the record, not the deadline");
        let e = a.watch(&c, &f, Some(&Cursor("c:0.0".into())), Duration::ZERO).unwrap_err();
        assert_eq!((e.code, e.field.as_deref()), (ErrorCode::InvalidArgument, Some("cursor")));
        let e = a.watch(
            &c,
            &Filter { motion: Some(pe_seams::world_api::Motion { every_ms: None }), ..f.clone() },
            None,
            Duration::ZERO,
        );
        assert_eq!(e.unwrap_err().field.as_deref(), Some("motion"));
        let e = a.watch(&c, &Filter { final_only: true, ..f.clone() }, None, Duration::ZERO);
        assert_eq!(e.unwrap_err().field.as_deref(), Some("final_only"));
        assert_eq!(a.watch(&caller(&[]), &f, None, Duration::ZERO).unwrap_err().code, ErrorCode::Forbidden);
    }

    #[test]
    fn a_code_change_reports_its_versions_void_with_who_could_not_build_it() {
        use pe_seams::world_api::{ActivationNote, VoidWhy};
        use pe_seams::{Activation, ActivationKey, ActivationOutcome, BuildFailure, VoidedWhy};
        let a = api();
        let c = caller(&[Scope::Read]);
        let commit = EntryRef(pe_core::Hash([7; 32]));
        let f = Filter { of: vec![TxId::from(commit)], ..Filter::default() };
        let start = a.watch(&c, &f, None, Duration::ZERO).unwrap().cursor;
        let bob = pe_core::MemberKey([2; 32]);
        a.log.activations(
            &[Activation {
                key: ActivationKey { asset: [1; 32], version: pe_seams::Version { entry: commit, op: 0 } },
                frame: CoreFrame(5),
                outcome: ActivationOutcome::Voided {
                    why: VoidedWhy::QuorumUnreachable,
                    objects: vec![],
                    more: 0,
                    failed: vec![(bob, BuildFailure::Compile)],
                },
            }],
            &[],
        );
        a.log.record(&[], Some(9));
        let w = a.watch(&c, &f, Some(&start), Duration::ZERO).unwrap();
        let o = w.mine.iter().find(|o| o.change == Some(TxId::from(commit))).expect("told");
        assert_eq!((o.status, o.is_final), (Status::Final, true));
        let Some(ActivationNote::Voided { frame: 5, why: VoidWhy::QuorumUnreachable, failed, .. }) = &o.activation
        else {
            panic!("{:?}", o.activation)
        };
        assert_eq!((failed[0].member.member, failed[0].reason.as_str()), (bob.into(), "compile error"));
    }

    #[test]
    fn an_expired_cursor_is_refused_with_a_fresh_one() {
        let src = api().src;
        let a = Api::new(
            Src::clone(&src),
            Arc::new(EditQueue::new(2, || {})),
            Arc::new(ChangeLog::with_retention(crate::stream::Retention {
                max_items: 2,
                max_age: Duration::from_secs(60),
            })),
        );
        let c = caller(&[Scope::Read]);
        let start = a.watch(&c, &Filter::default(), None, Duration::ZERO).unwrap().cursor;
        for f in 1..=3 {
            a.log.record(&[], Some(f));
        }
        let e = a.watch(&c, &Filter::default(), Some(&start), Duration::ZERO).unwrap_err();
        assert_eq!(e.code, ErrorCode::CursorExpired);
        let fresh = Cursor(e.details.unwrap()["cursor"].as_str().unwrap().to_string());
        assert!(a.watch(&c, &Filter::default(), Some(&fresh), Duration::ZERO).is_ok());
    }

    // ---- history and revert (api.md 5.5) ----

    #[test]
    fn history_lists_changes_newest_first_with_summaries_filters_and_pages() {
        let a = api();
        let c = caller(&[Scope::Read]);
        let bob = MemberKey([2; 32]);
        let del = edit_entry(11, vec![pe_world::edit::Op::Delete { id: pe_world::edit::Target::Existing(id(20)) }]);
        let mk = edit_entry(
            12,
            vec![pe_world::edit::Op::Create {
                new: 0,
                parent: pe_world::edit::Target::Existing(id(10)),
                index: None,
                rows: vec![],
            }],
        );
        a.log.record(&[stepped(del, ALICE, 4, applied(vec![]))], None);
        a.log.record(&[stepped(mk, bob, 5, applied(vec![id(20)]))], None);
        let h = a.history(&c, &Default::default()).unwrap();
        let order: Vec<_> = h.entries.iter().map(|e| e.change).collect();
        assert_eq!(order, vec![TxId::from(mk), TxId::from(del)], "newest first");
        assert_eq!(h.entries[1].summary, "Deleted Lamp.");
        assert_eq!(h.entries[0].summary, "Created Lamp under Table.");
        assert_eq!((h.entries[1].by.name.as_str(), h.entries[1].status), ("Alice", Status::Accepted));
        // Filters: member, frame range, objects.
        let alice = pe_seams::world_api::HistoryQuery { member: Some(ALICE.into()), ..Default::default() };
        assert_eq!(a.history(&c, &alice).unwrap().entries.len(), 1);
        let late = pe_seams::world_api::HistoryQuery { from_frame: Some(5), ..Default::default() };
        assert_eq!(a.history(&c, &late).unwrap().entries[0].change, TxId::from(mk));
        let table =
            pe_seams::world_api::HistoryQuery { select: Some(Selector::Name("Table".into())), ..Default::default() };
        assert_eq!(a.history(&c, &table).unwrap().entries.len(), 1, "only the create names the Table");
        // Paging.
        let one = pe_seams::world_api::HistoryQuery { limit: Some(1), ..Default::default() };
        let p1 = a.history(&c, &one).unwrap();
        assert_eq!(p1.entries.len(), 1);
        let next = pe_seams::world_api::HistoryQuery { limit: Some(1), cursor: p1.more.clone(), ..Default::default() };
        let p2 = a.history(&c, &next).unwrap();
        assert_eq!((p2.entries[0].change, p2.more), (TxId::from(del), None));
        let bad = pe_seams::world_api::HistoryQuery { cursor: Some(Cursor("nope".into())), ..Default::default() };
        assert_eq!(a.history(&c, &bad).unwrap_err().field.as_deref(), Some("cursor"));
    }

    #[test]
    fn revert_applies_the_recorded_inverse_as_a_new_labelled_change() {
        let a = api();
        let c = caller(&[Scope::Read, Scope::Edit]);
        // A change that created the Lamp; its inverse deletes it, provided the Lamp is as the change left it.
        let made = edit_entry(
            13,
            vec![pe_world::edit::Op::Create {
                new: 0,
                parent: pe_world::edit::Target::Existing(id(10)),
                index: None,
                rows: vec![],
            }],
        );
        let lamp_now = pe_seams::Version { entry: EntryRef(Hash([20; 32])), op: 0 };
        let inverse = |is: pe_seams::Version| pe_world::edit::Transaction {
            actor: pe_world::edit::Actor::Person,
            label: None,
            preconditions: vec![pe_world::edit::Precondition::ObjectVersion { id: id(20), is }],
            ops: vec![pe_world::edit::Op::Delete { id: pe_world::edit::Target::Existing(id(20)) }],
            blobs: vec![],
        };
        let with = |inv: pe_world::edit::Transaction| pe_seams::Outcome::Applied {
            created: vec![id(20)],
            overwrote: vec![],
            notes: vec![],
            inverse: Some(inv.encode().into()),
        };
        a.log.record(&[stepped(made, ALICE, 4, with(inverse(lamp_now)))], None);
        let dry = ApplyOpts { mode: Mode::DryRun, ..Default::default() };
        let o = a.revert(&c, made.into(), &dry).unwrap();
        assert_eq!((o.status, o.dry_run), (Status::Accepted, true));
        // Live: queued as this caller's change, labelled.
        let none = ApplyOpts { wait: Wait::None, ..Default::default() };
        assert_eq!(a.revert(&c, made.into(), &none).unwrap().status, Status::Queued);
        let (_, sub) = a.queue.drain(1).pop().unwrap();
        assert_eq!(sub.tx.label.as_deref(), Some("Revert: t"));
        assert!(matches!(sub.tx.actor, pe_world::edit::Actor::Agent { .. }));
        // Someone changed the Lamp since: refused as stale, unless if_version is "any".
        let other = edit_entry(14, vec![]);
        a.log.record(
            &[stepped(other, ALICE, 4, with(inverse(pe_seams::Version { entry: EntryRef(Hash([99; 32])), op: 0 })))],
            None,
        );
        let o = a.revert(&c, other.into(), &dry).unwrap();
        assert_eq!(o.refusal.map(|f| f.code), Some(RefusalCode::StaleVersion));
        let any = ApplyOpts { if_version: Some(pe_seams::world_api::Version("any".into())), ..dry.clone() };
        assert_eq!(a.revert(&c, other.into(), &any).unwrap().status, Status::Accepted);
        // Nothing to revert: unknown, or refused.
        assert_eq!(a.revert(&c, TxId([7; 32]), &dry).unwrap_err().field.as_deref(), Some("change"));
        let refused = edit_entry(15, vec![]);
        let no = pe_seams::Outcome::Refused { op: None, reason: pe_seams::Reason::NoPermission };
        a.log.record(&[stepped(refused, ALICE, 4, no)], None);
        assert_eq!(a.revert(&c, refused.into(), &dry).unwrap_err().code, ErrorCode::InvalidArgument);
        assert_eq!(a.revert(&caller(&[Scope::Read]), made.into(), &dry).unwrap_err().code, ErrorCode::Forbidden);
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
