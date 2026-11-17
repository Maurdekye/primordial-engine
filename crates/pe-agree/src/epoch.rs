//! What a joiner starts from (agreement.md 8): the epoch certificate that proves the reconfiguring cut E final, and the
//! state at E a new member needs that the certificate doesn't carry.
//!
//! - **The certificate** is E and its certified descendants in the ending epoch, oldest first, each with a quorum
//!   certificate of it: E's own (its root is the world's root at E, which the joiner's import must match), then each
//!   suffix block's. Some block in it has a child proposed in the very next view, so under the two-chain rule that
//!   block is final, and so is E, its ancestor. A joiner checks it against the ending epoch's members, which its
//!   invite pins ([`EpochStart::verify`]).
//! - **Not covered by the certificate, trusted from the inviter** as the world import at E is (M1): the next epoch's
//!   members and governance state, the head entries of E's chains (each is checked against the head E names) and each
//!   chain's highest final stamp. A joiner checks it is among the members. Deriving the members from E's own changes
//!   would need E's entries; it comes with the epoch-certificate fetch
//!   (@item:primordial-engine/pe-agree-a-member-that-missed-its-epoch-s-reconf).
//! - **One epoch only:** a joiner whose invite pins an epoch before the one that ended at E can't start yet (the chain
//!   of certificates from the pinned epoch to E is that fetch's too).

use pe_seams::{Cut, Frame, GovState, Hash, LineId, MemberKey, SignedEntry, Verifier, quorum};

use crate::core::{Block, Qc, epoch_domain, vote_bytes};
use pe_log::ChainKey;

/// Most blocks in a certificate: E and its suffix.
pub const MAX_CERT_BLOCKS: usize = 64;

/// A joiner's start (module doc).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EpochStart {
    /// The epoch that ended at E: the one the joiner's invite pins.
    pub epoch: u64,
    /// E, then its certified descendants in `epoch`, oldest first, each with a certificate of it.
    pub chain: Vec<(Block, Qc)>,
    /// The next epoch's members, sorted, distinct.
    pub members: Vec<MemberKey>,
    /// The next epoch's governance state.
    pub gov: GovState,
    /// The entries E's heads name, those the sender holds.
    pub heads: Vec<SignedEntry>,
    /// Each chain's highest final stamp at E (the back-stamp rule's floor).
    pub final_high: Vec<(ChainKey, Frame)>,
}

/// Why a start is refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StartError {
    /// Not a start record.
    Malformed,
    /// For another epoch than the invite pins.
    OtherEpoch { start: u64, pinned: u64 },
    /// The certificate doesn't prove E final in the pinned epoch: what failed.
    Certificate(&'static str),
    /// This member isn't among the next epoch's members.
    NotAdmitted,
    /// A head entry isn't the one E names.
    BadHead,
}

impl EpochStart {
    /// E: the first block.
    pub fn e(&self) -> Option<&Cut> {
        self.chain.first().map(|(b, _)| &b.cut)
    }

    /// E's certified root: the world's root at E.
    pub fn root(&self) -> Option<Hash> {
        self.chain.first().map(|(_, q)| q.root)
    }

    /// Check the certificate (module doc) against the ending epoch `pinned` with `members` on `line`, and that `me` is
    /// among the next epoch's members. Returns E.
    pub fn verify(
        &self,
        line: LineId,
        pinned: u64,
        members: &[MemberKey],
        me: MemberKey,
        verifier: &dyn Verifier,
    ) -> Result<&Cut, StartError> {
        use StartError::Certificate as C;
        if self.epoch != pinned {
            return Err(StartError::OtherEpoch { start: self.epoch, pinned });
        }
        if self.chain.is_empty() || self.chain.len() > MAX_CERT_BLOCKS {
            return Err(C("no blocks, or too many"));
        }
        let domain = epoch_domain(line, pinned);
        let q = quorum(members.len());
        let e = &self.chain[0].0;
        if e.cut.at.line != line {
            return Err(C("another line"));
        }
        let mut final_found = false;
        for (i, (b, qc)) in self.chain.iter().enumerate() {
            if qc.block != b.id() || qc.view != b.view || qc.view == 0 {
                return Err(C("a certificate of another block"));
            }
            if qc.votes.len() < q {
                return Err(C("fewer votes than the quorum"));
            }
            let msg = vote_bytes(domain, qc.view, qc.block, qc.root);
            for (j, (k, s)) in qc.votes.iter().enumerate() {
                if j > 0 && qc.votes[j - 1].0 >= *k {
                    return Err(C("votes not sorted and distinct"));
                }
                if members.binary_search(k).is_err() {
                    return Err(C("a vote by a non-member"));
                }
                if !verifier.verify(k, &msg, s) {
                    return Err(C("a bad vote signature"));
                }
            }
            if i > 0 {
                let p = &self.chain[i - 1].0;
                if b.parent != p.id() || b.view <= p.view {
                    return Err(C("not a chain"));
                }
                if b.cut.at.frame != e.cut.at.frame
                    || b.cut.heads != e.cut.heads
                    || b.cut.at.line != line
                    || b.cut.at.height != p.cut.at.height + 1
                {
                    return Err(C("a descendant of E that isn't a suffix cut"));
                }
                if b.view == p.view + 1 {
                    final_found = true;
                }
            }
        }
        if !final_found {
            return Err(C("no block is final under the two-chain rule"));
        }
        if self.members.binary_search(&me).is_err() {
            return Err(StartError::NotAdmitted);
        }
        if self.members.windows(2).any(|w| w[0] >= w[1]) {
            return Err(StartError::Malformed);
        }
        for h in &self.heads {
            let id = h.id();
            if !e.cut.heads.iter().any(|x| x.entry == id && x.chain == h.entry.chain && x.author == h.entry.author) {
                return Err(StartError::BadHead);
            }
            if !h.verify(verifier) {
                return Err(StartError::BadHead);
            }
        }
        Ok(&e.cut)
    }
}
