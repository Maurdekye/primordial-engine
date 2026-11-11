//! `SeamNode`: one node over the real seams, S3's [`Agreement`] plus S2's [`Replica`] (DESIGN.md 12, M0 step 2).
//!
//! The drive loop here is a copy of pe-replica's `Driver::drive`, kept only until that lands on main (ws/replica,
//! @vdw-physics). Each tick: the agreement ticks, the replica advances to the present (`p + 1`), every cut in
//! `wants_roots` gets the replica's root and a vote, and each new `final_cut()` goes to `finalize` (retried while
//! `NotReady`).
//!
//! The runner and the checkers still read the harness's local trace ([`Ev`]); this node maps the seam's events onto
//! it until the trace is retyped to `TraceEvent`:
//! - `CutFinal` becomes `Final`, with the cut's hash over its final entries, so P1 compares cut contents;
//! - `Voided` keeps its deciding cut (P4); `Paused` stays `Paused` (P8);
//! - the recorder adds `ReplicaRoot` at each voted cut, `Floor` at each finalize and `RolledBack` from `Advance`.
//!
//! Node ids are the members' places in key order, from 1, as [`KvSeamReplica`] numbers them, so P2's offline replay
//! over [`KvReplica`](crate::kv::KvReplica) sees the same authors. An entry stamped `s` is the replay's frame `s + 1`:
//! the replay counts frames stepped, and the cut at frame `f` covers the entries stamped below `f`.

use std::collections::{BTreeMap, BTreeSet};

use pe_seams::test_util::{EntryBuilder, TestKeys};
use pe_seams::{
    AgreeEvent, Agreement, ChainId, Class, Cut, CutRef, FaultKind, Finalized, Frame, FrameClock, Hasher, HostTime,
    Lane, MemberKey, Replica, SelfTest, SignedEntry, SimRate,
};

use crate::clock::SessionClock;
use crate::kv::{KvEntry, Op, Template};
use crate::kvrep::{KvSeamReplica, payload};
use crate::lockstep::LockstepAgreement;
use crate::net::NodeId;
use crate::sim::{Fault, NodeUnderTest, Out};
use crate::trace::{EntryRef as TraceEntry, Ev, Frame as TFrame};

/// An agreement's entry framing, read back: the entry a message carries, if it carries one.
pub type EntryDecoder = fn(&[u8]) -> Option<SignedEntry>;

pub struct SeamNode<A: Agreement> {
    pub agree: A,
    pub replica: KvSeamReplica,
    clock: SessionClock,
    builder: EntryBuilder,
    /// Member keys in node-id order: node `i` is `keys[i - 1]`.
    keys: Vec<MemberKey>,
    last_final: CutRef,
    finalized_height: u64,
    log: BTreeMap<TFrame, Vec<KvEntry>>,
    events: Vec<Ev>,
    self_test: SelfTest,
    /// The agreement's entry framing (its test-util codec), so the recorder knows which entries this member holds:
    /// its own and every valid one delivered to it. `None` turns the hold checks of P15 off.
    decode: Option<EntryDecoder>,
    held: BTreeSet<(ChainId, u64)>,
    held_ids: BTreeSet<pe_seams::EntryRef>,
    voided: BTreeSet<pe_seams::EntryRef>,
    /// Frames below the final frame whose `entries_at` P15 has checked.
    checked_to: u64,
    /// The highest `kept_from` seen: it never goes back.
    kept_seen: u64,
    /// Cut frames already recorded as `Damaged`.
    damaged: BTreeSet<u64>,
    seen_height: u64,
}

/// The node id of member `key`: its place in key order, from 1.
pub fn node_of(keys: &[MemberKey], key: &MemberKey) -> NodeId {
    keys.binary_search(key).map_or(0, |i| i as NodeId + 1)
}

/// A short stand-in for an entry's chain position in the local trace: the id's first eight bytes.
pub fn trace_seq(e: &pe_seams::EntryRef) -> u64 {
    u64::from_le_bytes(e.0.0[..8].try_into().unwrap_or_default())
}

fn fault_code(k: FaultKind) -> u16 {
    match k {
        FaultKind::Malformed => 1,
        FaultKind::TooLarge => 2,
        FaultKind::BadSignature => 3,
        FaultKind::WrongLine => 4,
        FaultKind::NotMember => 5,
        FaultKind::Equivocation => 6,
        FaultKind::RateExceeded => 7,
        FaultKind::Protocol => 8,
    }
}

impl SeamNode<LockstepAgreement> {
    /// Node `id` (from 1, in key order) of a lockstep session over `keys`, cutting every `cadence_ns`.
    pub fn lockstep(
        keys: &TestKeys,
        id: NodeId,
        template: &Template,
        objects: &[u128],
        rate: SimRate,
        cadence_ns: i64,
    ) -> Self {
        let sorted = keys.sorted_keys();
        let key = sorted[(id as usize).saturating_sub(1)];
        let i = (0..keys.len()).find(|&j| keys.key(j) == key).unwrap_or(0);
        let agree = LockstepAgreement::new(keys, i, cadence_ns);
        let genesis = agree.genesis();
        SeamNode {
            agree,
            replica: KvSeamReplica::new(sorted.clone(), template, objects),
            clock: SessionClock { rate, start: HostTime(0) },
            builder: EntryBuilder::new(keys, i),
            keys: sorted,
            last_final: genesis,
            finalized_height: 0,
            log: BTreeMap::new(),
            events: Vec::new(),
            self_test: SelfTest::NotRun,
            decode: Some(LockstepAgreement::decode_entry_message),
            held: BTreeSet::new(),
            held_ids: BTreeSet::new(),
            voided: BTreeSet::new(),
            checked_to: 0,
            kept_seen: 0,
            damaged: BTreeSet::new(),
            seen_height: 0,
        }
    }
}

impl<A: Agreement> SeamNode<A> {
    fn frame_at(&self, now: i64) -> u64 {
        self.clock.frame_at(HostTime(now)).map_or(0, |f| f.0)
    }

    /// The (chain, seq) positions `cut` names that this member does not hold, by the recorder's count.
    fn lacking(&self, cut: &Cut) -> Vec<(ChainId, u64)> {
        if self.decode.is_none() {
            return Vec::new();
        }
        cut.heads
            .iter()
            .flat_map(|h| (1..=h.seq).map(move |s| (h.chain, s)))
            .filter(|k| !self.held.contains(k))
            .collect()
    }

    fn breach_if_lacking(&mut self, rule: &'static str, cut: &Cut) {
        let missing = self.lacking(cut);
        if !missing.is_empty() {
            let detail = format!("cut {}/{} names {} entries not held", cut.at.height, cut.at.frame.0, missing.len());
            self.events.push(Ev::Breach { rule, detail });
        }
    }

    /// P15's check of `entries_at` below the final frame: only entries the final cut covers, none it voided. A frame
    /// no longer kept (S3 v1.2) holds nothing to check.
    fn check_applied(&mut self) {
        let Some(fc) = self.agree.final_cut().cloned() else { return };
        let to = fc.cut.at.frame.0;
        for f in self.checked_to..to {
            for e in self.agree.entries_at(Frame(f)).into_iter().flatten() {
                let bad = if self.voided.contains(&e.id()) {
                    Some("a voided entry")
                } else if !fc.cut.includes(&e.entry.chain, e.entry.seq) {
                    Some("an entry the final cut does not cover")
                } else {
                    None
                };
                if let Some(what) = bad {
                    let detail = format!("frame {f} below final frame {to} applies {what}");
                    self.events.push(Ev::Breach { rule: "entries_at", detail });
                }
            }
        }
        self.checked_to = self.checked_to.max(to);
    }

    /// P15's retention lines (S3 v1.2): `kept_from` never goes back, and it is truthful: `entries_at` answers for
    /// it and for the final frame's predecessor, whenever they are at or above it and below `present`. (That nothing
    /// at or above the floor is trimmed except past the time cap needs the cap, which is S3's own: pe-agree's
    /// conformance target checks it.)
    fn check_retention(&mut self, present: u64) {
        let kept = self.agree.kept_from().0;
        if kept < self.kept_seen {
            let detail = format!("went back from {} to {kept}", self.kept_seen);
            self.events.push(Ev::Breach { rule: "kept_from", detail });
        }
        self.kept_seen = self.kept_seen.max(kept);
        let last = self.agree.final_cut().map(|fc| fc.cut.at.frame.0.saturating_sub(1));
        for f in std::iter::once(kept).chain(last) {
            if f >= kept && f < present && self.agree.entries_at(Frame(f)).is_none() {
                let detail = format!("frame {f} is at or above kept_from {kept} but no longer kept");
                self.events.push(Ev::Breach { rule: "kept_from", detail });
            }
        }
    }

    /// One drive step: advance, vote on what wants a root, finalize the newest final cut.
    fn drive(&mut self, now: i64) {
        let present = Frame(self.frame_at(now) + 1);
        self.check_retention(present.0);
        let adv = self.replica.advance(present, &mut self.agree);
        if let Some(f) = adv.rolled_back_to {
            self.events.push(Ev::RolledBack { frame: f.0 });
        }
        for cut in self.agree.wants_roots() {
            self.breach_if_lacking("wants_roots", &cut);
            match self.replica.root_at_cut(&cut, &mut self.agree) {
                Ok(root) => {
                    self.events.push(Ev::ReplicaRoot { frame: cut.at.frame.0, root: root.0 });
                    self.agree.vote(&cut.at, root, self.self_test);
                }
                // S3 v1.2: no root at this cut will come; rung 2 is due (M0 runs none). Recorded once per cut.
                Err(pe_seams::RootError::Damaged) if self.damaged.insert(cut.at.frame.0) => {
                    self.events.push(Ev::Damaged { frame: cut.at.frame.0 });
                }
                Err(_) => {}
            }
        }
        if let Some(fc) = self.agree.final_cut().cloned() {
            if fc.cut.at.height < self.seen_height {
                let detail = format!("height {} after {}", fc.cut.at.height, self.seen_height);
                self.events.push(Ev::Breach { rule: "final_cut", detail });
            }
            if fc.cut.at.height > self.seen_height {
                self.seen_height = fc.cut.at.height;
                self.breach_if_lacking("final_cut", &fc.cut);
            }
        }
        if let Some(fc) = self.agree.final_cut().cloned()
            && fc.cut.at.height > self.finalized_height
        {
            match self.replica.finalize(&fc, &mut self.agree) {
                Finalized::NotReady => {}
                Finalized::Agreed | Finalized::Diverged { .. } => {
                    self.finalized_height = fc.cut.at.height;
                    self.events.push(Ev::Floor { frame: fc.cut.at.frame.0 });
                }
                // S3 v1.2: my root at the cut cannot be taken, and waiting will not change that. The floor stays, so
                // there is no floor event, and the cut is not retried; rung 2 is pe-diverge's, which M0 does not run.
                Finalized::Damaged => {
                    self.finalized_height = fc.cut.at.height;
                    if self.damaged.insert(fc.cut.at.frame.0) {
                        self.events.push(Ev::Damaged { frame: fc.cut.at.frame.0 });
                    }
                }
            }
        }
    }

    /// The cut's hash over its final entries since the previous final cut, which also extends the final log.
    fn absorb_final(&mut self, cut: CutRef) -> pe_seams::Hash {
        let mut h = Hasher::new("pe.harness.cut");
        h.write_u64(cut.height).write_u64(cut.frame.0);
        let entries = self.agree.final_entries(&self.last_final, &cut).unwrap_or_default();
        for e in &entries {
            h.write_hash(&e.id().0);
            let author = node_of(&self.keys, &e.entry.author);
            let ops = crate::mock::decode(&e.entry.payload).map(|(_, _, ops)| ops).unwrap_or_default();
            let row = self.log.entry(e.entry.stamp.0 + 1).or_default();
            row.extend(ops.into_iter().map(|op| KvEntry { author, op }));
        }
        self.log.entry(cut.frame.0).or_default();
        self.last_final = cut;
        h.finish()
    }
}

impl<A: Agreement> NodeUnderTest for SeamNode<A> {
    fn input(&mut self, now: i64, op: Op) {
        let stamp = Frame(self.frame_at(now));
        let mut b = self.builder.clone();
        let entry = b.next_unsigned(Class::Edit, stamp, payload(&[op]));
        if let Ok(signed) = self.agree.submit(entry) {
            self.held.insert((signed.entry.chain, signed.entry.seq));
            self.held_ids.insert(signed.id());
            self.builder = b;
        }
    }

    fn receive(&mut self, now: i64, from: NodeId, bytes: &[u8]) -> Result<(), Fault> {
        let Some(&peer) = (from as usize).checked_sub(1).and_then(|i| self.keys.get(i)) else {
            return Err(Fault { author: from, reason: fault_code(FaultKind::NotMember) });
        };
        let keys = &self.keys;
        let r = self
            .agree
            .receive(peer, Lane::Action, bytes, HostTime(now))
            .map_err(|f| Fault { author: f.author.map_or(from, |a| node_of(keys, &a)), reason: fault_code(f.kind) });
        if r.is_ok()
            && let Some(e) = self.decode.and_then(|d| d(bytes))
        {
            self.held.insert((e.entry.chain, e.entry.seq));
            self.held_ids.insert(e.id());
        }
        r
    }

    fn tick(&mut self, now: i64) -> Vec<Out> {
        let outs = self.agree.tick(HostTime(now), &self.clock);
        self.drive(now);
        outs.into_iter()
            .map(|o| Out { to: node_of(&self.keys, &o.to), lane: o.lane, bytes: o.bytes.to_vec() })
            .collect()
    }

    fn drain(&mut self) -> Vec<Ev> {
        let mut out = Vec::new();
        for e in self.agree.events() {
            match e {
                AgreeEvent::CutFinal { cut, root, .. } => {
                    let hash = self.absorb_final(cut);
                    out.push(Ev::Final { line: 0, height: cut.height, frame: cut.frame.0, cut: hash.0, root: root.0 });
                }
                AgreeEvent::Voided { entry, author, reason, cut } => out.push({
                    self.voided.insert(entry);
                    if self.decode.is_some() && !self.held_ids.contains(&entry) {
                        // A verdict on an entry this member does not hold (S3 v1.1, per member).
                        let detail = format!("void of an entry not held, under cut {}", cut.height);
                        self.events.push(Ev::Breach { rule: "verdict", detail });
                    }
                    Ev::Voided {
                        entry: TraceEntry { author: node_of(&self.keys, &author), seq: trace_seq(&entry) },
                        by: Some((0, cut.height)),
                        reason: reason as u16,
                    }
                }),
                AgreeEvent::Refused { entry, author, kind } => out.push(Ev::Refused {
                    entry: TraceEntry { author: node_of(&self.keys, &author), seq: trace_seq(&entry) },
                    reason: fault_code(kind),
                }),
                AgreeEvent::Paused { at } => out.push(Ev::Paused { frame: at.0 }),
                // Faults reach the trace from `receive`'s result; the rest is not in the local trace yet.
                _ => {}
            }
        }
        self.check_applied();
        out.append(&mut self.events);
        out
    }

    fn final_entries(&self) -> Vec<(TFrame, Vec<KvEntry>)> {
        self.log.iter().map(|(&f, e)| (f, e.clone())).collect()
    }
}
