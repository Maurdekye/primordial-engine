//! Stand-ins that let pe-app run a world on one machine before pe-agree (S3) and pe-replica's replica (S2) land: an
//! agreement with one member that never cuts, and the replica of an empty world. Both go once those crates land.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use pe_seams::{
    Advance, AgreeEvent, AgreeStats, Agreement, Cut, CutImport, CutRef, DomainId, Entry, EntryRef, EntrySource, Fault,
    FinalCut, Finalized, Frame, FrameClock, FrameView, GroupSel, Hash, HostTime, ImportError, Lane, LineId, MemberKey,
    Outcome, Outgoing, PeerId, PhysicsImage, Pos, Replica, RigTransform, RootError, SelfTest, Signature, SignedEntry,
    SimRate, SubmitError, Tree, ViewCounters,
};

/// Agreement with one member and no peers. Its entries are final as soon as they are held, so it never makes a cut
/// and never asks for a root.
pub struct SoloAgreement {
    line: LineId,
    entries: BTreeMap<Frame, Vec<SignedEntry>>,
    changed_from: Option<Frame>,
}

impl SoloAgreement {
    pub fn new(line: LineId) -> SoloAgreement {
        SoloAgreement { line, entries: BTreeMap::new(), changed_from: None }
    }
}

impl EntrySource for SoloAgreement {
    fn final_cut(&self) -> Option<&FinalCut> {
        None
    }

    fn entries_at(&self, frame: Frame) -> Option<&[SignedEntry]> {
        Some(self.entries.get(&frame).map_or(&[], Vec::as_slice))
    }

    /// The stand-in keeps every entry (S3 v1.2): it never trims.
    fn kept_from(&self) -> Frame {
        Frame(0)
    }

    fn keep_final_from(&mut self, _cv: Frame, _floor: Frame) {}

    fn take_changed_from(&mut self) -> Option<Frame> {
        self.changed_from.take()
    }
}

impl Agreement for SoloAgreement {
    fn line(&self) -> LineId {
        self.line
    }

    /// Kept for the replica, unsigned: nothing leaves this machine, and nothing here checks a signature.
    fn submit(&mut self, entry: Entry) -> Result<SignedEntry, SubmitError> {
        let signed = SignedEntry { entry, sig: Signature([0; 64]) };
        let frame = signed.entry.stamp;
        self.entries.entry(frame).or_default().push(signed.clone());
        self.changed_from = Some(self.changed_from.map_or(frame, |c| c.min(frame)));
        Ok(signed)
    }

    fn receive(&mut self, _: PeerId, _: Lane, _: &[u8], _: HostTime) -> Result<(), Fault> {
        Ok(())
    }

    fn tick(&mut self, _: HostTime, _: &dyn FrameClock) -> Vec<Outgoing> {
        Vec::new()
    }

    fn wants_roots(&self) -> Vec<Cut> {
        Vec::new()
    }

    fn vote(&mut self, _: &CutRef, _: Hash, _: SelfTest) {}

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
        Vec::new()
    }
}

/// The replica of an empty world: it steps frames within its budget and publishes a view with no objects once it
/// reaches the present. It holds no state to hash, so it has no roots: every cut is `NotKept`.
pub struct EmptyReplica {
    clock: Arc<dyn FrameClock>,
    rate: SimRate,
    frame: Frame,
    budget_frames: u64,
    view: Arc<FrameView>,
}

impl EmptyReplica {
    /// A world whose next frame to step is `start`; its view starts there.
    pub fn new(clock: Arc<dyn FrameClock>, start: Frame) -> EmptyReplica {
        let rate = SimRate::new(clock.rate_hz()).unwrap_or_default();
        let view = view_at(&*clock, start);
        EmptyReplica { clock, rate, frame: start, budget_frames: 2, view }
    }
}

fn view_at(clock: &dyn FrameClock, frame: Frame) -> Arc<FrameView> {
    Arc::new(FrameView {
        frame,
        domain: DomainId(0),
        sim_time: clock.host_time_of(frame),
        final_frame: None,
        local_rig: RigTransform { pos: Pos::ORIGIN, rotation: [0.0, 0.0, 0.0, 1.0], scale: 1.0 },
        objects: Vec::new(),
        effects: Vec::new(),
        retracted: Vec::new(),
        counters: ViewCounters::default(),
        changes: Vec::new(),
    })
}

impl Replica for EmptyReplica {
    fn domain(&self) -> DomainId {
        DomainId(0)
    }

    fn frame(&self) -> Frame {
        self.frame
    }

    fn set_budget(&mut self, simulated: Duration) {
        self.budget_frames = self.rate.frames(simulated).max(2);
    }

    fn advance(&mut self, present: Frame, _: &mut dyn EntrySource) -> Advance {
        let to = present.0.min(self.frame.0.saturating_add(self.budget_frames));
        let stepped = to.saturating_sub(self.frame.0);
        self.frame = Frame(self.frame.0.max(to));
        let caught_up = self.frame >= present;
        if caught_up && stepped > 0 {
            self.view = view_at(&*self.clock, self.frame);
        }
        Advance { rolled_back_to: None, resimulated: 0, stepped, caught_up }
    }

    fn outcome(&self, _: &EntryRef) -> Option<(Frame, Outcome)> {
        None
    }

    fn root_at_cut(&mut self, _: &Cut, _: &mut dyn EntrySource) -> Result<Hash, RootError> {
        Err(RootError::NotKept)
    }

    fn finalize(&mut self, _: &FinalCut, _: &mut dyn EntrySource) -> Finalized {
        Finalized::NotReady
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
