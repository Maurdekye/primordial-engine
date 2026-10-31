//! Fetch repair: requests for missing chain ranges, with backoff, and their answers.

use super::*;

impl WorldSim {
    pub(super) fn want_from(&mut self, holder: MemberId, author: MemberId, from: u32, to: u32) {
        if holder != self.me && from <= to {
            self.wants.push((holder, author, from, to));
        }
    }

    pub(super) fn want_all(&mut self, author: MemberId, from: u32, to: u32) {
        let known: Vec<MemberId> = self.known.iter().copied().collect();
        for j in known {
            self.want_from(j, author, from, to);
        }
    }

    pub fn on_fetch(&self, author: MemberId, from: u32, to: u32) -> Vec<Msg> {
        // At most 16 entries per reply (review R3-9): the requester asks again for the rest.
        (from..=to.min(from + 15))
            .filter_map(|s| self.by_author.get(&(author, s)).and_then(|id| self.signed_entry(*id)))
            .map(Msg::Entry)
            .collect()
    }

    /// Fetch requests from gap and conflict detection: (who to ask, request), rate-limited.
    pub fn take_fetches(&mut self, now_ms: u64) -> Vec<(MemberId, Msg)> {
        let mut out = Vec::new();
        for (to, author, from, upto) in std::mem::take(&mut self.wants) {
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
            out.push((to, Msg::Fetch { author, from, to: upto }));
        }
        let (values, ids) = self.vcm.take_fetch_wants();
        for (to, value) in values {
            out.push((to, Msg::FetchValue { value }));
        }
        for (to, id) in ids {
            out.push((to, Msg::FetchEntry { id }));
        }
        out
    }
}
