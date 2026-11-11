//! The S2/S3 glue (src/drive.rs): the order of one call, one vote per cut with the caller's self-test, roots not yet
//! available, and one `finalize` per final cut (again after `NotReady`). With fakes that log every call; no physics.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pe_replica::{DriveReport, Driver};
use pe_seams::{
    Advance, AgreeEvent, AgreeStats, Agreement, Cut, CutImport, CutRef, DomainId, Entry, EntryRef, EntrySource, Fault,
    FinalCut, Finalized, Frame, FrameClock, FrameView, GroupSel, Hash, HostTime, ImportError, Lane, LineId, MemberKey,
    Outcome, Outgoing, PeerId, PhysicsImage, Pos, Replica, RigTransform, RootError, SelfTest, Signature, SignedEntry,
    SubmitError, Tree, ViewCounters,
};

type Log = Arc<Mutex<Vec<String>>>;

fn note(log: &Log, s: impl Into<String>) {
    log.lock().unwrap().push(s.into());
}

fn cut(height: u64, frame: u64) -> Cut {
    Cut { at: CutRef { line: LineId::default(), height, frame: Frame(frame) }, heads: Vec::new() }
}

fn final_cut(height: u64, frame: u64) -> FinalCut {
    FinalCut { cut: cut(height, frame), root: Hash([9; 32]), voters: Vec::new() }
}

struct Agree {
    log: Log,
    final_cut: Option<FinalCut>,
    wants: Vec<Cut>,
    none: Vec<SignedEntry>,
}

impl EntrySource for Agree {
    fn final_cut(&self) -> Option<&FinalCut> {
        self.final_cut.as_ref()
    }
    fn entries_at(&self, _: Frame) -> Option<&[SignedEntry]> {
        Some(&self.none)
    }
    fn kept_from(&self) -> Frame {
        Frame(0)
    }
    fn keep_final_from(&mut self, _: Frame, _: Frame) {}
    fn take_changed_from(&mut self) -> Option<Frame> {
        None
    }
}

impl Agreement for Agree {
    fn line(&self) -> LineId {
        LineId::default()
    }
    fn submit(&mut self, entry: Entry) -> Result<SignedEntry, SubmitError> {
        note(&self.log, "agree.submit");
        Ok(SignedEntry { entry, sig: Signature([0; 64]) })
    }
    fn receive(&mut self, _: PeerId, lane: Lane, _: &[u8], _: HostTime) -> Result<(), Fault> {
        note(&self.log, format!("agree.receive {lane:?}"));
        Ok(())
    }
    fn tick(&mut self, _: HostTime, _: &dyn FrameClock) -> Vec<Outgoing> {
        note(&self.log, "agree.tick");
        Vec::new()
    }
    fn wants_roots(&self) -> Vec<Cut> {
        self.wants.clone()
    }
    fn vote(&mut self, cut: &CutRef, _: Hash, self_test: SelfTest) {
        note(&self.log, format!("agree.vote h{} {self_test:?}", cut.height));
    }
    fn hold_votes_from(&mut self, _: Frame) {}
    fn release_votes_from(&mut self, _: Frame) {}
    fn final_entries(&self, _: &CutRef, _: &CutRef) -> Option<Vec<SignedEntry>> {
        None
    }
    fn members(&self) -> Vec<MemberKey> {
        Vec::new()
    }
    fn stats(&self) -> AgreeStats {
        AgreeStats::default()
    }
    fn events(&mut self) -> Vec<AgreeEvent> {
        note(&self.log, "agree.events");
        Vec::new()
    }
}

/// A replica that has simulated every frame below the last `present` it was advanced to. `pending` roots answer
/// `RootError::Pending` once each, as a replica whose budget ran out does (S2 v1.1).
struct Rep {
    log: Log,
    present: u64,
    verdicts: VecDeque<Finalized>,
    pending: Vec<u64>,
    view: Arc<FrameView>,
}

impl Replica for Rep {
    fn domain(&self) -> DomainId {
        DomainId(0)
    }
    fn frame(&self) -> Frame {
        Frame(self.present)
    }
    fn set_budget(&mut self, _: Duration) {}
    fn advance(&mut self, present: Frame, _: &mut dyn EntrySource) -> Advance {
        note(&self.log, format!("replica.advance {}", present.0));
        let stepped = present.0.saturating_sub(self.present);
        self.present = self.present.max(present.0);
        Advance { rolled_back_to: None, resimulated: 0, stepped, caught_up: true }
    }
    fn outcome(&self, _: &EntryRef) -> Option<(Frame, Outcome)> {
        None
    }
    fn root_at_cut(&mut self, cut: &Cut, _: &mut dyn EntrySource) -> Result<Hash, RootError> {
        note(&self.log, format!("replica.root_at_cut h{}", cut.at.height));
        if cut.at.frame.0 > self.present {
            return Err(RootError::NotYetSimulated);
        }
        if let Some(i) = self.pending.iter().position(|&h| h == cut.at.height) {
            self.pending.remove(i);
            return Err(RootError::Pending);
        }
        Ok(Hash([cut.at.height as u8; 32]))
    }
    fn finalize(&mut self, cut: &FinalCut, _: &mut dyn EntrySource) -> Finalized {
        note(&self.log, format!("replica.finalize h{}", cut.cut.at.height));
        self.verdicts.pop_front().unwrap_or(Finalized::Agreed)
    }
    fn replay_to(&mut self, _: &Cut, _: &mut dyn EntrySource) -> Result<Hash, RootError> {
        Err(RootError::NotKept)
    }
    fn tree(&self, _: &Hash) -> Option<Box<dyn Tree + '_>> {
        None
    }
    fn physics_image(&self, _: &Hash) -> Option<PhysicsImage> {
        None
    }
    fn import_at_cut(&mut self, _: &GroupSel, _: CutImport, _: &mut dyn EntrySource) -> Result<Hash, ImportError> {
        Err(ImportError::NotKept)
    }
    fn view(&self) -> Arc<FrameView> {
        self.view.clone()
    }
}

struct Node {
    log: Log,
    agree: Agree,
    replica: Rep,
    driver: Driver,
}

fn node() -> Node {
    let log: Log = Arc::default();
    let view = Arc::new(FrameView {
        frame: Frame(0),
        domain: DomainId(0),
        sim_time: HostTime(0),
        final_frame: None,
        local_rig: RigTransform { pos: Pos::ORIGIN, rotation: [0.0, 0.0, 0.0, 1.0], scale: 1.0 },
        objects: Vec::new(),
        effects: Vec::new(),
        retracted: Vec::new(),
        counters: ViewCounters::default(),
        changes: Vec::new(),
    });
    Node {
        agree: Agree { log: log.clone(), final_cut: None, wants: Vec::new(), none: Vec::new() },
        replica: Rep { log: log.clone(), present: 0, verdicts: VecDeque::new(), pending: Vec::new(), view },
        driver: Driver::new(),
        log,
    }
}

impl Node {
    fn tick(&mut self, present: u64, self_test: SelfTest) -> DriveReport {
        self.log.lock().unwrap().clear();
        self.driver.drive(&mut self.replica, &mut self.agree, Frame(present), self_test)
    }

    fn log(&self) -> Vec<String> {
        self.log.lock().unwrap().clone()
    }
}

#[test]
fn one_call_advances_then_votes_then_finalizes() {
    let mut n = node();
    n.agree.wants = vec![cut(1, 8)];
    n.agree.final_cut = Some(final_cut(0, 4));

    let r = n.tick(8, SelfTest::NotRun);

    assert_eq!(
        n.log(),
        ["replica.advance 8", "replica.root_at_cut h1", "agree.vote h1 NotRun", "replica.finalize h0"],
        "advance, then the votes, then finalize; messages, input, S3's tick and its events stay outside"
    );
    assert_eq!(r.advance.stepped, 8);
    assert_eq!(r.votes, [(cut(1, 8).at, Hash([1; 32]))]);
    assert!(r.no_root.is_empty());
    assert_eq!(r.finalized, [(cut(0, 4).at, Finalized::Agreed)]);
}

#[test]
fn votes_once_per_cut_and_finalizes_once_per_final_cut() {
    let mut n = node();

    // A cut past the simulated frames has no root yet: no vote, and it is reported.
    n.agree.wants = vec![cut(1, 8)];
    let r = n.tick(4, SelfTest::NotRun);
    assert!(r.votes.is_empty());
    assert_eq!(r.no_root, [(cut(1, 8).at, RootError::NotYetSimulated)]);

    // Once simulated, it is voted on, with the caller's self-test verdict.
    let r = n.tick(8, SelfTest::Failed);
    assert_eq!(r.votes, [(cut(1, 8).at, Hash([1; 32]))]);
    assert!(n.log().contains(&"agree.vote h1 Failed".to_string()));

    // Wanted again: no second vote (a revote is pe-diverge's, through the caller).
    let r = n.tick(9, SelfTest::NotRun);
    assert!(r.votes.is_empty());
    assert!(!n.log().iter().any(|l| l.starts_with("replica.root_at_cut")));

    // The cut is final but not ready here: finalize is tried again at the next tick, and agrees.
    n.agree.wants.clear();
    n.agree.final_cut = Some(final_cut(1, 8));
    n.replica.verdicts = VecDeque::from([Finalized::NotReady]);
    let r = n.tick(10, SelfTest::NotRun);
    assert_eq!(r.finalized, [(cut(1, 8).at, Finalized::NotReady)]);
    let r = n.tick(11, SelfTest::NotRun);
    assert_eq!(r.finalized, [(cut(1, 8).at, Finalized::Agreed)]);

    // Not a third time for the same cut; a new final cut is finalized.
    let r = n.tick(12, SelfTest::NotRun);
    assert!(r.finalized.is_empty());
    n.agree.final_cut = Some(final_cut(2, 12));
    let r = n.tick(13, SelfTest::NotRun);
    assert_eq!(r.finalized, [(cut(2, 12).at, Finalized::Agreed)]);
}

#[test]
fn a_diverged_verdict_is_reported_and_not_retried_by_the_driver() {
    let mut n = node();
    let mine = Hash([7; 32]);
    n.agree.final_cut = Some(final_cut(3, 24));
    n.replica.verdicts = VecDeque::from([Finalized::Diverged { mine }]);
    let r = n.tick(24, SelfTest::NotRun);
    assert_eq!(r.finalized, [(cut(3, 24).at, Finalized::Diverged { mine })]);
    // The repair, and finalize again for the same cut after it, are pe-diverge's (S2 v1).
    let r = n.tick(25, SelfTest::NotRun);
    assert!(r.finalized.is_empty());
}

/// S2 v1.1: a replica whose budget ran out answers `Pending`; the driver asks again on the next call.
#[test]
fn a_pending_root_is_asked_for_again() {
    let mut n = node();
    n.agree.wants = vec![cut(1, 8)];
    n.replica.pending = vec![1];
    let r = n.tick(8, SelfTest::NotRun);
    assert!(r.votes.is_empty());
    assert_eq!(r.no_root, [(cut(1, 8).at, RootError::Pending)]);
    let r = n.tick(9, SelfTest::NotRun);
    assert_eq!(r.votes, [(cut(1, 8).at, Hash([1; 32]))]);
}
