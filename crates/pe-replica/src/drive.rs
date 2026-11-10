//! The glue between S2 and S3 that pe-app's simulation tick and the harness's node driver both run (pe-seams ruling 3;
//! docs/spec/app.md 3, @engine-architect): so the loop that ships is the loop the properties check.
//!
//! It is written against the seam traits only ([`Replica`] and [`Agreement`]), so it does not depend on how either
//! is built. Messages, local input, S3's `tick` and pe-diverge stay outside it, in pe-app and in the harness. One
//! [`Driver::drive`] call, once per tick:
//! 1. **advances** the replica to `present` (every frame below it is simulated, with the entries held so far);
//! 2. **votes:** each cut S3 wants a root for gets `root_at_cut`, then `vote` with the caller's self-test verdict.
//!    A cut is voted on once; a revote after a repair is pe-diverge's, through the caller;
//! 3. **finalizes:** when S3's final cut changed, or was not ready last time, it goes to `finalize`.
//!
//! The driver keeps only which cuts it voted on and which final cut it last finalized. That is loop state, not
//! simulation state: dropping it costs at most one repeated vote or `finalize`, which both seams allow.

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

        for cut in agree.wants_roots() {
            if self.voted.contains(&cut.at) {
                continue;
            }
            match replica.root_at_cut(&cut, agree) {
                Ok(root) => {
                    agree.vote(&cut.at, root, self_test);
                    self.voted.insert(cut.at);
                    report.votes.push((cut.at, root));
                }
                Err(e) => report.no_root.push((cut.at, e)),
            }
        }

        if let Some(fc) = agree.final_cut().cloned() {
            if self.finalized != Some(fc.cut.at) {
                let verdict = replica.finalize(&fc, agree);
                if verdict != Finalized::NotReady {
                    self.finalized = Some(fc.cut.at);
                    let height = fc.cut.at.height;
                    self.voted.retain(|c| c.height > height);
                }
                report.finalized.push((fc.cut.at, verdict));
            }
        }
        report
    }
}
