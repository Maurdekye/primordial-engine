//! The glue between S2 and S3 that pe-app's simulation tick runs (pe-seams ruling 3; docs/spec/app.md 3,
//! @engine-architect), meant to be the loop the harness's properties check too. pe-harness's `SeamNode` still runs its
//! own copy of it (seamnode.rs), which places no vote holds, until it moves to this driver (@harness-eng).
//!
//! It is written against the seam traits only ([`Replica`] and [`Agreement`]), so it does not depend on how either
//! is built. Messages, local input, S3's `tick` and pe-diverge stay outside it, in pe-app and in the harness. One
//! [`Driver::drive`] call, once per tick:
//! 1. **advances** the replica to `present` (every frame below it is simulated, with the entries held so far);
//! 2. **holds:** reconciles the replica's [`Replica::vote_holds`] with S3's `hold_votes_from` and
//!    `release_votes_from` (S2 v1.7), so S3 holds exactly the frames the replica lists now. It is level-triggered: a
//!    partial advance, a rollback that removed a commit, or a rebuilt replica never leaves a hold behind. No replica
//!    lists a hold today: since F3's C4 the author of a code commit votes on the old code's state instead
//!    (replica.md 5.4). The reconciliation stays for a version whose module a machine lacks at n >= 3 (runner.rs,
//!    replica.md 8.2), which is not built yet (@design-reviewer's nit on C4);
//! 3. **votes:** each cut S3 wants a root for gets `root_at_cut`, then `vote` with the caller's self-test verdict.
//!    A cut is voted on once; a revote after a repair is pe-diverge's, through the caller. A cut at or past a hold
//!    waits, unasked, until the hold is released;
//! 4. **finalizes:** when S3's final cut changed, or was not ready last time, it goes to `finalize`.
//!
//! The driver keeps which cuts it voted on, which final cut it last finalized, and which holds it placed. The first
//! two are loop state, not simulation state: dropping them costs at most one repeated vote or `finalize`, which both
//! seams allow. The holds are not disposable: S3 counts them, so a driver must live as long as the agreement it
//! drives, as pe-app's simulation thread keeps both together.

use std::collections::BTreeSet;

use pe_seams::{Advance, Agreement, CutRef, Finalized, Frame, Hash, Replica, RootError, SelfTest};

/// What one [`Driver::drive`] call did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DriveReport {
    pub advance: Advance,
    /// The cuts voted on, with the root sent.
    pub votes: Vec<(CutRef, Hash)>,
    /// The cuts S3 wanted a root for that the replica could not give one for yet.
    pub no_root: Vec<(CutRef, RootError)>,
    /// The cuts S3 wanted a root for that wait for a vote hold: at or past the earliest frame the replica holds.
    pub held_back: Vec<CutRef>,
    /// Each final cut passed to `finalize`, with what the replica found.
    pub finalized: Vec<(CutRef, Finalized)>,
}

/// The loop's memory between ticks.
#[derive(Clone, Debug, Default)]
pub struct Driver {
    /// Cuts voted on and not yet final.
    voted: BTreeSet<CutRef>,
    /// The final cut last finalized with a verdict (not `NotReady`).
    finalized: Option<CutRef>,
    /// The vote holds this driver placed with S3, one per frame: the replica's `vote_holds()` at the last call.
    held: BTreeSet<Frame>,
}

impl Driver {
    pub fn new() -> Driver {
        Driver::default()
    }

    /// One tick's S2/S3 glue, in the order of this module's doc. `self_test` is the verdict to publish with this
    /// tick's votes: `NotRun` unless pe-diverge asked for a self-test (divergence.md 5, step 2).
    pub fn drive(
        &mut self,
        replica: &mut dyn Replica,
        agree: &mut dyn Agreement,
        present: Frame,
        self_test: SelfTest,
    ) -> DriveReport {
        let mut report = DriveReport { advance: replica.advance(present, agree), ..DriveReport::default() };

        // Vote holds, after the advance and before any vote: S3 holds exactly the frames the replica lists now.
        let want: BTreeSet<Frame> = replica.vote_holds().into_iter().collect();
        for &f in want.difference(&self.held) {
            agree.hold_votes_from(f);
        }
        for &f in self.held.difference(&want) {
            agree.release_votes_from(f);
        }
        self.held = want;
        let hold = self.held.first().copied();

        for cut in agree.wants_roots() {
            if self.voted.contains(&cut.at) {
                continue;
            }
            // A cut at or past a hold is not asked yet, and not marked: S3 need not leave it out of `wants_roots`,
            // and a vote refused for the hold would otherwise never be asked again once the hold is released.
            if hold.is_some_and(|h| cut.at.frame >= h) {
                report.held_back.push(cut.at);
                continue;
            }
            match replica.root_at_cut(&cut, agree) {
                Ok(root) => {
                    // A first vote is cast, or not votable (the cut went final meanwhile); either way this cut is
                    // not asked again. A revote after a repair is pe-diverge's, through its actions.
                    let _ = agree.vote(&cut.at, root, self_test);
                    self.voted.insert(cut.at);
                    report.votes.push((cut.at, root));
                }
                Err(e) => report.no_root.push((cut.at, e)),
            }
        }

        if let Some(fc) = agree.final_cut().cloned()
            && self.finalized != Some(fc.cut.at)
        {
            let verdict = replica.finalize(&fc, agree);
            if verdict != Finalized::NotReady {
                self.finalized = Some(fc.cut.at);
                let height = fc.cut.at.height;
                self.voted.retain(|c| c.height > height);
            }
            report.finalized.push((fc.cut.at, verdict));
        }
        report
    }
}
