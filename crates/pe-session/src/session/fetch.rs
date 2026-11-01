//! Fetch repair: requests for missing chain ranges, with backoff, and their answers.

use super::*;

impl WorldSim {
    /// Ask `holder` for `author`'s entries `from..=to`. The reply may be lean when this member holds
    /// the entry below the range (a wrong rebuild fails verification and falls back).
    pub(super) fn want_from(&mut self, holder: MemberId, author: MemberId, from: u32, to: u32) {
        let below = self.lean_wire && (from <= 1 || self.by_author.contains_key(&(author, from - 1)));
        self.want_from_held(holder, author, from, to, if below { HELD_BELOW } else { 0 });
    }

    /// The same, with what this member holds around the range stated ([`Msg::Fetch`]'s `held`).
    pub(super) fn want_from_held(&mut self, holder: MemberId, author: MemberId, from: u32, to: u32, held: u8) {
        if holder != self.me && from <= to {
            self.wants.push((holder, author, from, to, held));
        }
    }

    pub(super) fn want_all(&mut self, author: MemberId, from: u32, to: u32) {
        let known: Vec<MemberId> = self.known.iter().copied().collect();
        for j in known {
            self.want_from(j, author, from, to);
        }
    }

    /// The answer to a fetch of `author`'s `from..=to`: one run of the entries held there, in a row,
    /// each naming the one before. Lean if the requester holds the entry below ([`HELD_BELOW`]);
    /// unsigned if it holds a later entry naming the top ([`HELD_ABOVE`]: the hole fill, option 2);
    /// otherwise extended up to the first entry held signed on itself, which covers the rest.
    pub fn on_fetch(&self, author: MemberId, from: u32, to: u32, held: u8) -> Vec<Msg> {
        // At most 16 entries per reply (review R3-9): the requester asks again for the rest.
        // Saturating (review P23-14): a request near u32::MAX must not overflow.
        let mut run: Vec<SignedEntry> = Vec::new();
        for s in from..=to.min(from.saturating_add(15)) {
            let Some(e) = self.by_author.get(&(author, s)).and_then(|id| self.entries.get(id)) else { break };
            if run.last().is_some_and(|p| e.prev != p.id()) {
                break;
            }
            run.push(SignedEntry { entry: e.clone(), sig: self.sigs.get(&e.id()).copied().unwrap_or([0; 64]) });
            // O2-1: a view-change or admin entry ends the answer, signed on itself; the requester asks
            // again for the rest, as it does past 16 entries.
            if e.payload.signs_itself() && self.run_wire {
                break;
            }
        }
        let Some(top) = run.last() else { return vec![] };
        // Without runs on the wire (A/B, option 1's form): every entry on its own, signed on itself.
        if !self.run_wire {
            let lean = held & HELD_BELOW != 0;
            return run
                .into_iter()
                .enumerate()
                .map(|(i, e)| if i == 0 && lean { Msg::Run { lean: true, heads: false, signed: true, entries: vec![e] } } else { Msg::Entry(e) })
                .collect();
        }
        let signed = held & HELD_ABOVE == 0 || top.entry.payload.signs_itself();
        if signed && !self.sigs.contains_key(&top.id()) {
            let Some(cover) = self.run_of(top.id()) else { return vec![] };
            if run.len() + cover.len() - 1 > Self::RUN_MAX {
                return vec![];
            }
            run.pop();
            run.extend(cover);
        }
        let lean = held & HELD_BELOW != 0;
        if run.len() == 1 && signed && !lean {
            return vec![Msg::Entry(run.pop().expect("one"))];
        }
        vec![Msg::Run { lean, heads: false, signed, entries: run }]
    }

    /// Fetch requests from gap and conflict detection: (who to ask, request), rate-limited.
    pub fn take_fetches(&mut self, now_ms: u64) -> Vec<(MemberId, Msg)> {
        let mut out = Vec::new();
        for (to, author, from, upto, held) in std::mem::take(&mut self.wants) {
            // Review R3-9: the same range is re-asked with exponential backoff, 60 ms doubling up to
            // 2 s, so a congested link is not flooded with repeats of requests whose replies are
            // still queued (a fixed 60 ms was shorter than a congested round trip).
            let wait = match self.fetch_asked.get(&(to, author)).copied() {
                Some((f, t, w)) if f == from => {
                    if now_ms < t + w {
                        continue;
                    }
                    (w * 2).min(2000)
                }
                _ => 60,
            };
            self.fetch_asked.insert((to, author), (from, now_ms, wait));
            out.push((to, Msg::Fetch { author, from, to: upto, held }));
            self.stats.fetches_sent += 1;
        }
        let (values, ids) = self.vcm.take_fetch_wants();
        for (to, value) in values {
            out.push((to, Msg::FetchValue { value }));
        }
        let mut asked_now = BTreeSet::new();
        for (to, id, back) in ids {
            if self.fixes_off & fix::VC4R4 == 0 {
                // Review VC4R-4: the same 60 ms doubling up to 2 s as range fetches, per (holder, id),
                // for as long as the entry is missing.
                if self.entries.contains_key(&id) || !asked_now.insert((to, id)) {
                    continue;
                }
                let wait = match self.entry_backoff.get(&(to, id)).copied() {
                    Some((t, w)) if now_ms < t + w => continue,
                    Some((_, w)) => (w * 2).min(2000),
                    None => 60,
                };
                self.entry_backoff.insert((to, id), (now_ms, wait));
            }
            out.push((to, Msg::FetchEntry { id, back }));
        }
        out
    }
}
