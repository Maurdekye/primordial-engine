//! The S5 overflow journal (F3, slow-member-design.md S5 Part 1; @world-chunks).
//!
//! A receiver keeps every checked entry of another author beyond the final cut in a durable journal, written through
//! the storage seam ([`crate::store`]), in a log of its own (`journal/` beside `safety/`). That lets it hold a far
//! longer stretch of a faster author's chain than memory allows, survive a restart with it, and tell the author what
//! it durably holds.
//!
//! - **What goes in:** entries that passed pe-agree's checks (signature, line, member, shape, in `Node::hold`'s
//!   order), keyed (author, chain, seq), de-duplicated by [`EntryRef`]. Two entries at one seq (a fork) are both
//!   kept: the log judges them, not the journal.
//! - **Hot memory:** the index (key, id, size, Lsn) is always in memory, the bodies only up to a cap: per author the
//!   lowest seqs first, since they are admitted next, and a total. A body leaves memory only once durable. A cold
//!   body comes back through an asynchronous [`Job::Load`], so no disk read lands on the frame loop.
//! - **The ack:** per chain, the highest seq s such that every seq from the final head + 1 to s has a durable entry
//!   ([`Journal::acks`]). A hole holds it. S2's `Progress` heads become min(linked head, ack) (@world-chunks's
//!   change in `send_progress`), released under the same hold as votes.
//! - **Final entries** (at or below the final cut) leave the index at once. A compaction ([`Job::Compact`]) rewrites
//!   the live entries when dead bytes pass `compact_ratio` times the live ones, at most every `compact_every_ns`.
//! - **A full disk, or an author over its disk cap:** [`Offer::Paused`]. The caller defers the entry in memory as
//!   `defer` does today, never refusing an honest one; acks stop advancing, so the author's `backlog()` grows and it
//!   thins (S1). The journal takes entries again once the writer's retry succeeds.
//! - **Restart:** [`Journal::recover`] rebuilds the index from the log and hands the entries back. They passed every
//!   check before they were written, and the frame check catches disk damage, so the caller skips only the
//!   signature check and reruns the rest (line, membership in the current epoch, shape, budget): an author may have
//!   left, or the epoch changed, since. Each entry's id is recomputed from its decoded bytes.
//! - **Grip edges keep their poses** by construction: whole signed entries are kept, never thinned or dropped.
//!
//! Sans-IO, like the node: the journal emits numbered [`Job`]s ([`Journal::take_jobs`]) and learns their outcome
//! from [`Journal::on_report`]. Its job numbers are its own log's, separate from the safety log's.

use std::collections::BTreeMap;

use pe_log::ChainKey;
use pe_seams::{EntryRef, MemberKey, SignedEntry};

use crate::store::{Done, Job, Keep, Lsn, Record, Report};

/// The journal's record kind: one `SignedEntry::encode`.
pub const KIND_ENTRY: u16 = 16;

#[derive(Clone, Debug)]
pub struct JournalConfig {
    /// Entry bodies kept in memory per author, lowest seqs first.
    pub hot_per_author: usize,
    /// And in all.
    pub hot_total: usize,
    /// Bytes journalled per author past which it pauses for that author (the disk cap). [I] 65 s at 120 Hz's budget
    /// (480/s) of ~120 B hand entries is about 3.7 MB.
    pub max_per_author: u64,
    /// Compact when dead bytes pass this many times the live ones...
    pub compact_ratio: u64,
    /// ...and at least this many bytes...
    pub compact_min_bytes: u64,
    /// ...at most this often.
    pub compact_every_ns: i64,
    /// Cold entries asked for in one load.
    pub load_batch: usize,
}

impl Default for JournalConfig {
    fn default() -> JournalConfig {
        JournalConfig {
            hot_per_author: 256 << 10,
            hot_total: 2 << 20,
            max_per_author: 16 << 20,
            compact_ratio: 2,
            compact_min_bytes: 1 << 20,
            compact_every_ns: 10_000_000_000,
            load_batch: 256,
        }
    }
}

/// What [`Journal::offer`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Offer {
    /// Journalled: durable once its job is.
    Taken,
    /// Already held under this id.
    Duplicate,
    /// At or below the chain's final head: nothing to keep.
    Final,
    /// The disk is full or the author is over its cap: defer it in memory, as today.
    Paused,
}

#[derive(Clone, Debug)]
enum Body {
    /// In memory, not yet taken by the caller. Boxed: most slots are cold or taken.
    Hot(Box<SignedEntry>),
    /// On disk only.
    Cold,
    /// Being loaded by this job.
    Loading(u64),
    /// Handed to the caller ([`Journal::next`]), which holds it now; still durable here until final.
    Taken,
}

#[derive(Clone, Debug)]
struct Slot {
    id: EntryRef,
    size: u64,
    /// Where it is in the log, once known.
    lsn: Option<Lsn>,
    /// The append job that wrote it (none after a restart).
    job: Option<u64>,
    durable: bool,
    body: Body,
}

/// Where a slot is: (chain, seq, id).
type Key = (ChainKey, u64, EntryRef);

/// What a job in flight was for.
#[derive(Clone, Debug)]
enum Pending {
    Append(Key),
    /// The keys kept, and the dead bytes when it was asked for.
    Compact(Vec<Key>, u64),
    Load(Vec<Key>),
}

pub struct Journal {
    cfg: JournalConfig,
    next_job: u64,
    jobs: Vec<Job>,
    in_flight: BTreeMap<u64, Pending>,
    chains: BTreeMap<ChainKey, BTreeMap<u64, Vec<Slot>>>,
    final_seq: BTreeMap<ChainKey, u64>,
    hot_by_author: BTreeMap<MemberKey, usize>,
    hot_total: usize,
    by_author: BTreeMap<MemberKey, u64>,
    live: u64,
    dead: u64,
    last_compact_ns: i64,
    full: bool,
}

/// Counts, for tests and the status line.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct JournalStats {
    pub entries: usize,
    pub hot_bytes: usize,
    pub live_bytes: u64,
    pub dead_bytes: u64,
    pub in_flight: usize,
    pub paused: bool,
}

fn body_size(b: &Body, size: u64) -> usize {
    if matches!(b, Body::Hot(_)) { size as usize } else { 0 }
}

impl Journal {
    /// A journal over an empty log, whose first job is number `first_job`.
    pub fn new(cfg: JournalConfig, first_job: u64) -> Journal {
        Journal {
            cfg,
            next_job: first_job,
            jobs: Vec::new(),
            in_flight: BTreeMap::new(),
            chains: BTreeMap::new(),
            final_seq: BTreeMap::new(),
            hot_by_author: BTreeMap::new(),
            hot_total: 0,
            by_author: BTreeMap::new(),
            live: 0,
            dead: 0,
            last_compact_ns: i64::MIN,
            full: false,
        }
    }

    /// Rebuild the journal from its log's recovered records. Returns the entries, in log order, for the caller to
    /// recheck (all but the signature; see the module documentation) and the number of records that didn't decode.
    /// The caller calls [`Journal::forget`] for each entry its checks refuse.
    pub fn recover(cfg: JournalConfig, first_job: u64, records: &[Record]) -> (Journal, Vec<SignedEntry>, usize) {
        let mut j = Journal::new(cfg, first_job);
        let mut out = Vec::new();
        let mut bad = 0;
        for r in records {
            let e = match (r.kind, SignedEntry::decode(&r.payload)) {
                (KIND_ENTRY, Ok(e)) => e,
                _ => {
                    bad += 1;
                    continue;
                }
            };
            let id = e.id();
            let key = ChainKey::of(&e);
            let size = r.payload.len() as u64;
            let slots = j.chains.entry(key).or_default().entry(e.entry.seq).or_default();
            if slots.iter().any(|s| s.id == id) {
                continue;
            }
            slots.push(Slot {
                id,
                size,
                lsn: Some(r.lsn),
                job: None,
                durable: true,
                body: Body::Hot(Box::new(e.clone())),
            });
            *j.hot_by_author.entry(key.author).or_default() += size as usize;
            j.hot_total += size as usize;
            *j.by_author.entry(key.author).or_default() += size;
            j.live += size;
            out.push(e);
        }
        j.enforce_hot();
        (j, out, bad)
    }

    fn job(&mut self, job: Job, p: Pending) -> u64 {
        let n = self.next_job;
        debug_assert_eq!(job.n(), n);
        self.next_job += 1;
        self.jobs.push(job);
        self.in_flight.insert(n, p);
        n
    }

    /// Journal a checked entry.
    pub fn offer(&mut self, e: &SignedEntry) -> Offer {
        let key = ChainKey::of(e);
        let seq = e.entry.seq;
        if self.final_seq.get(&key).is_some_and(|f| seq <= *f) {
            return Offer::Final;
        }
        let id = e.id();
        if self.chains.get(&key).and_then(|c| c.get(&seq)).is_some_and(|s| s.iter().any(|s| s.id == id)) {
            return Offer::Duplicate;
        }
        let bytes = e.encode();
        let size = bytes.len() as u64;
        if self.full || self.by_author.get(&key.author).copied().unwrap_or(0) + size > self.cfg.max_per_author {
            return Offer::Paused;
        }
        let n = self.next_job;
        self.job(Job::Append { n, kind: KIND_ENTRY, bytes }, Pending::Append((key, seq, id)));
        self.chains.entry(key).or_default().entry(seq).or_default().push(Slot {
            id,
            size,
            lsn: None,
            job: Some(n),
            durable: false,
            body: Body::Hot(Box::new(e.clone())),
        });
        *self.hot_by_author.entry(key.author).or_default() += size as usize;
        self.hot_total += size as usize;
        *self.by_author.entry(key.author).or_default() += size;
        self.live += size;
        Offer::Taken
    }

    /// The jobs to hand to the writer, in order.
    pub fn take_jobs(&mut self) -> Vec<Job> {
        std::mem::take(&mut self.jobs)
    }

    fn slot_mut(&mut self, k: &Key) -> Option<&mut Slot> {
        self.chains.get_mut(&k.0)?.get_mut(&k.1)?.iter_mut().find(|s| s.id == k.2)
    }

    /// Learn what the writer did.
    pub fn on_report(&mut self, r: &Report) {
        match r {
            Report::Durable { done, .. } => {
                for d in done {
                    self.on_done(d);
                }
                self.full = false;
                self.enforce_hot();
            }
            Report::Loaded { n, records } => self.on_loaded(*n, records),
            Report::Full { .. } => self.full = true,
            Report::Failed(_) => self.full = true,
        }
    }

    fn on_done(&mut self, d: &Done) {
        match self.in_flight.remove(&d.n) {
            Some(Pending::Append(k)) => {
                if let Some(s) = self.slot_mut(&k) {
                    // A compaction later in the same report corrects the Lsn of an entry it moved.
                    s.lsn = d.lsns.first().copied();
                    s.durable = true;
                }
            }
            Some(Pending::Compact(keys, dead_then)) => {
                // An empty keep set leaves nothing to move, but the compaction did drop every dead record: without
                // this, `dead` never falls and an empty checkpoint follows every `compact_every_ns`.
                let mut moved = keys.is_empty();
                for (k, lsn) in keys.iter().zip(&d.lsns) {
                    if let Some(s) = self.slot_mut(k) {
                        moved |= s.lsn != Some(*lsn);
                        s.lsn = Some(*lsn);
                        // Durable where it now is: copied by the compaction, or (a skipped one) synced before it.
                        s.durable = true;
                        s.job = None;
                    }
                }
                // Entries finalized since it was asked for were in its keep set: they are still dead. A compaction the
                // writer skipped (no room) freed nothing.
                if moved {
                    self.dead = self.dead.saturating_sub(dead_then);
                }
            }
            Some(Pending::Load(_)) | None => {}
        }
    }

    fn on_loaded(&mut self, n: u64, records: &[Record]) {
        let Some(Pending::Load(keys)) = self.in_flight.remove(&n) else { return };
        let by_lsn: BTreeMap<Lsn, &Record> = records.iter().map(|r| (r.lsn, r)).collect();
        for k in keys {
            let Some(slot) = self.slot_mut(&k) else { continue };
            if !matches!(slot.body, Body::Loading(m) if m == n) {
                continue;
            }
            let e = slot
                .lsn
                .and_then(|l| by_lsn.get(&l))
                .and_then(|r| SignedEntry::decode(&r.payload).ok())
                .filter(|e| e.id() == k.2);
            match e {
                Some(e) => {
                    slot.body = Body::Hot(Box::new(e));
                    let size = slot.size as usize;
                    *self.hot_by_author.entry(k.0.author).or_default() += size;
                    self.hot_total += size;
                }
                // Gone or not what was asked for: ask again later.
                None => slot.body = Body::Cold,
            }
        }
    }

    /// Keep memory under the caps: drop the bodies of durable entries with the highest seqs first (they are admitted
    /// last), per author, then across authors.
    fn enforce_hot(&mut self) {
        let per = self.cfg.hot_per_author;
        let authors: Vec<MemberKey> = self.hot_by_author.iter().filter(|(_, b)| **b > per).map(|(a, _)| *a).collect();
        for a in authors {
            self.evict(Some(a), per);
        }
        if self.hot_total > self.cfg.hot_total {
            self.evict(None, self.cfg.hot_total);
        }
    }

    fn evict(&mut self, author: Option<MemberKey>, to: usize) {
        // Candidates in descending seq order: durable, hot bodies.
        let mut cands: Vec<(u64, Key, u64)> = Vec::new();
        for (ck, seqs) in &self.chains {
            if author.is_some_and(|a| a != ck.author) {
                continue;
            }
            for (seq, slots) in seqs {
                for s in slots {
                    if s.durable && s.lsn.is_some() && matches!(s.body, Body::Hot(_)) {
                        cands.push((*seq, (*ck, *seq, s.id), s.size));
                    }
                }
            }
        }
        cands.sort_by_key(|c| std::cmp::Reverse(c.0));
        for (_, k, size) in cands {
            let over = match author {
                Some(a) => self.hot_by_author.get(&a).copied().unwrap_or(0) > to,
                None => self.hot_total > to,
            };
            if !over {
                break;
            }
            if let Some(s) = self.slot_mut(&k) {
                s.body = Body::Cold;
            }
            if let Some(h) = self.hot_by_author.get_mut(&k.0.author) {
                *h = h.saturating_sub(size as usize);
            }
            self.hot_total = self.hot_total.saturating_sub(size as usize);
        }
    }

    /// The author's entries not yet taken, in (chain, seq) order, up to `max`: the caller admits them as its budget
    /// allows and holds them from then on. Within a chain it stops at the first cold one, asking for a load of the cold
    /// ones from there (unless a compaction or a load for them is in flight); they come in a later call.
    pub fn next(&mut self, author: MemberKey, max: usize) -> Vec<SignedEntry> {
        let mut out = Vec::new();
        let mut cold: Vec<Key> = Vec::new();
        let compacting = self.in_flight.values().any(|p| matches!(p, Pending::Compact(..)));
        'chains: for (ck, seqs) in self.chains.iter_mut().filter(|(ck, _)| ck.author == author) {
            // Per chain: chains are checked independently (a cut names one head per chain), so a cold entry in an old
            // chain's tail doesn't hold back a newer chain after a rechain (@world-chunks).
            let mut blocked = false;
            for (seq, slots) in seqs.iter_mut() {
                for s in slots.iter_mut() {
                    if out.len() >= max {
                        break 'chains;
                    }
                    match &s.body {
                        Body::Taken => {}
                        Body::Hot(e) if !blocked => {
                            out.push((**e).clone());
                            let size = s.size as usize;
                            s.body = Body::Taken;
                            if let Some(h) = self.hot_by_author.get_mut(&author) {
                                *h = h.saturating_sub(size);
                            }
                            self.hot_total = self.hot_total.saturating_sub(size);
                        }
                        Body::Hot(_) => {}
                        Body::Loading(_) => blocked = true,
                        Body::Cold => {
                            blocked = true;
                            if !compacting && s.lsn.is_some() && cold.len() < self.cfg.load_batch {
                                cold.push((*ck, *seq, s.id));
                            }
                        }
                    }
                }
            }
        }
        if !cold.is_empty() {
            let n = self.next_job;
            let lsns: Vec<Lsn> = cold.iter().filter_map(|k| self.slot_mut(k).and_then(|s| s.lsn)).collect();
            for k in &cold {
                if let Some(s) = self.slot_mut(k) {
                    s.body = Body::Loading(n);
                }
            }
            self.job(Job::Load { n, lsns }, Pending::Load(cold));
        }
        out
    }

    /// The final cut moved: drop every entry at or below each chain's final seq. Then compact if it pays.
    pub fn finalize(&mut self, heads: impl IntoIterator<Item = (ChainKey, u64)>, now_ns: i64) {
        for (ck, f) in heads {
            let fs = self.final_seq.entry(ck).or_default();
            if f <= *fs {
                continue;
            }
            *fs = f;
            let Some(seqs) = self.chains.get_mut(&ck) else { continue };
            let keep = seqs.split_off(&(f + 1));
            let gone = std::mem::replace(seqs, keep);
            for s in gone.into_values().flatten() {
                self.live = self.live.saturating_sub(s.size);
                self.dead += s.size;
                if let Some(b) = self.by_author.get_mut(&ck.author) {
                    *b = b.saturating_sub(s.size);
                }
                let hot = body_size(&s.body, s.size);
                if let Some(h) = self.hot_by_author.get_mut(&ck.author) {
                    *h = h.saturating_sub(hot);
                }
                self.hot_total = self.hot_total.saturating_sub(hot);
            }
            if seqs.is_empty() {
                self.chains.remove(&ck);
            }
        }
        self.maybe_compact(now_ns);
    }

    fn maybe_compact(&mut self, now_ns: i64) {
        // Not while a load or another compaction is in flight: their Lsns would be stale.
        let busy = self.in_flight.values().any(|p| !matches!(p, Pending::Append(_)));
        // Nor on a full disk: it couldn't fit, and loads wait while one is in flight.
        if busy
            || self.full
            || self.dead < self.cfg.compact_min_bytes
            || self.dead < self.live.saturating_mul(self.cfg.compact_ratio)
            || now_ns.saturating_sub(self.last_compact_ns) < self.cfg.compact_every_ns
        {
            return;
        }
        // Every live entry, by Lsn if known, else by the append job that writes it (it runs before this job, so the
        // writer knows its Lsn by then).
        let mut keys = Vec::new();
        let mut keep = Vec::new();
        for (ck, seqs) in &self.chains {
            for (seq, slots) in seqs {
                for s in slots {
                    let k = match (s.lsn, s.job) {
                        (Some(l), _) if s.durable => Keep::Lsn(l),
                        (_, Some(j)) => Keep::Job(j),
                        (Some(l), None) => Keep::Lsn(l),
                        (None, None) => continue,
                    };
                    keys.push((*ck, *seq, s.id));
                    keep.push(k);
                }
            }
        }
        self.last_compact_ns = now_ns;
        let n = self.next_job;
        let dead = self.dead;
        self.job(Job::Compact { n, keep }, Pending::Compact(keys, dead));
    }

    /// Drop one entry (the caller's checks refused it after a restart).
    pub fn forget(&mut self, key: ChainKey, seq: u64, id: EntryRef) {
        let Some(seqs) = self.chains.get_mut(&key) else { return };
        let Some(slots) = seqs.get_mut(&seq) else { return };
        let Some(i) = slots.iter().position(|s| s.id == id) else { return };
        let s = slots.remove(i);
        if slots.is_empty() {
            seqs.remove(&seq);
        }
        self.live = self.live.saturating_sub(s.size);
        self.dead += s.size;
        if let Some(b) = self.by_author.get_mut(&key.author) {
            *b = b.saturating_sub(s.size);
        }
        let hot = body_size(&s.body, s.size);
        if let Some(h) = self.hot_by_author.get_mut(&key.author) {
            *h = h.saturating_sub(hot);
        }
        self.hot_total = self.hot_total.saturating_sub(hot);
    }

    /// Per chain: the highest seq through which every seq after the final head has a durable entry. Chains with
    /// nothing durable past their final head are absent.
    pub fn acks(&self) -> BTreeMap<ChainKey, u64> {
        let mut out = BTreeMap::new();
        for (ck, seqs) in &self.chains {
            let mut want = self.final_seq.get(ck).copied().unwrap_or(0) + 1;
            for (seq, slots) in seqs {
                if *seq != want || !slots.iter().any(|s| s.durable) {
                    break;
                }
                want += 1;
            }
            if want > self.final_seq.get(ck).copied().unwrap_or(0) + 1 {
                out.insert(*ck, want - 1);
            }
        }
        out
    }

    pub fn paused(&self) -> bool {
        self.full
    }

    pub fn stats(&self) -> JournalStats {
        JournalStats {
            entries: self.chains.values().flat_map(|c| c.values()).map(Vec::len).sum(),
            hot_bytes: self.hot_total,
            live_bytes: self.live,
            dead_bytes: self.dead,
            in_flight: self.in_flight.len(),
            paused: self.full,
        }
    }
}
