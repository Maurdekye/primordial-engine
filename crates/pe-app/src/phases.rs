//! Where the simulation thread's time goes (@coordinator-opus, after the first two-machine run): totals
//! for each step of the tick (app.md 3) and, inside step 4, for each call pe-replica's driver makes, through
//! [`Timed`], a replica wrapper that times. Apart from the coalesced pose rollbacks it carries ([`crate::coalesce`]),
//! it changes nothing the replica or agreement does. The numbers are wall
//! time on this machine, so they compare phases with each other, never one machine's run with another's.

use std::any::Any;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use pe_seams::{
    Advance, Cut, CutImport, DomainId, EntryRef, EntrySource, FinalCut, Finalized, Frame, FrameView, GroupSel, Hash,
    ImportError, Outcome, PhysicsImage, Replica, RootError, RunnerIdentity, Tree,
};

/// Totals over a run, in nanoseconds unless named otherwise. Steps 1 to 6 are the tick's (app.md 3); `advance_ns`,
/// `roots_ns` and `finalize_ns` are the parts of step 4 spent in those replica calls, and the rest of `drive_ns` is the
/// driver's own work (vote holds, votes into agreement).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PhaseTimes {
    /// Step 1: messages into agreement, the network's events and the hook's tick.
    pub ingest_ns: u64,
    /// Step 2: the local hands and edits into agreement.
    pub inputs_ns: u64,
    /// Step 3: agreement's tick and handing its messages to the network.
    pub agree_ns: u64,
    /// Step 4, the whole driver call.
    pub drive_ns: u64,
    /// The replica's `advance`: new frames stepped and frames simulated again after a rollback.
    pub advance_ns: u64,
    /// `root_at_cut`, the roots this member votes with (the lazy hashing, world-model.md 6), and how many calls.
    pub roots_ns: u64,
    pub roots: u64,
    /// `finalize` on final cuts.
    pub finalize_ns: u64,
    /// Step 5: S3's events and the confirmed-cut log.
    pub events_ns: u64,
    /// Step 5's pe-diverge (`Diverge::step`: its events, repairs, self-test and the Repair lane's sends). Step 1's
    /// Repair-lane messages to pe-diverge stay in `ingest_ns`.
    pub diverge_ns: u64,
    /// Step 6: the view, pe-api's feed and the publish.
    pub publish_ns: u64,
    /// The ticks from frame 0 on, end to end, and the longest one.
    pub tick_ns: u64,
    pub longest_tick_ns: u64,
    /// Step 7: the thread's wait for the next frame or a wake.
    pub idle_ns: u64,
    /// What `advance` did: frames stepped new, frames simulated again below the previous present, and the calls that
    /// rolled back.
    pub stepped: u64,
    pub resimulated: u64,
    pub rollbacks: u64,
}

/// Nanoseconds since `start`, saturating.
pub fn since(start: Instant) -> u64 {
    nanos(start.elapsed())
}

pub fn nanos(d: Duration) -> u64 {
    u64::try_from(d.as_nanos()).unwrap_or(u64::MAX)
}

/// A replica that times `advance`, `root_at_cut` and `finalize` into `times` and passes every call through unchanged,
/// the provided methods included, so the driver sees the replica it wraps. With `log`, each cut the driver finalizes
/// is first recorded in `--check-replay`'s final log (crate::replay), while agreement still keeps the entries: the
/// finalize moves the replica's `Cv`, below which agreement trims.
pub struct Timed<'a> {
    pub inner: &'a mut dyn Replica,
    pub times: &'a mut PhaseTimes,
    pub log: Option<&'a mut crate::replay::ReplayLog>,
    /// The coalesced pose rollbacks ([`crate::coalesce`]): `advance` reads its source through them. The one place
    /// `Timed` changes what the replica sees: when it re-simulates, never what it steps.
    pub coalesce: Option<&'a mut crate::coalesce::Coalesce>,
}

impl Replica for Timed<'_> {
    fn domain(&self) -> DomainId {
        self.inner.domain()
    }

    fn frame(&self) -> Frame {
        self.inner.frame()
    }

    fn set_budget(&mut self, simulated: Duration) {
        self.inner.set_budget(simulated)
    }

    fn advance(&mut self, present: Frame, source: &mut dyn EntrySource) -> Advance {
        let start = Instant::now();
        let a = match &mut self.coalesce {
            Some(coalesce) => {
                let stepped = self.inner.frame();
                let mut source = crate::coalesce::Coalesced { inner: source, coalesce, stepped };
                self.inner.advance(present, &mut source)
            }
            None => self.inner.advance(present, source),
        };
        self.times.advance_ns += since(start);
        self.times.stepped += a.stepped;
        self.times.resimulated += a.resimulated;
        self.times.rollbacks += u64::from(a.rolled_back_to.is_some());
        a
    }

    fn outcome(&self, entry: &EntryRef) -> Option<(Frame, Outcome)> {
        self.inner.outcome(entry)
    }

    fn root_at_cut(&mut self, cut: &Cut, source: &mut dyn EntrySource) -> Result<Hash, RootError> {
        let start = Instant::now();
        let r = self.inner.root_at_cut(cut, source);
        self.times.roots_ns += since(start);
        self.times.roots += 1;
        r
    }

    fn finalize(&mut self, cut: &FinalCut, source: &mut dyn EntrySource) -> Finalized {
        if let Some(log) = &mut self.log {
            log.record(cut, &*source);
        }
        let start = Instant::now();
        let f = self.inner.finalize(cut, source);
        self.times.finalize_ns += since(start);
        f
    }

    fn replay_to(&mut self, cut: &Cut, source: &mut dyn EntrySource) -> Result<Hash, RootError> {
        self.inner.replay_to(cut, source)
    }

    fn tree(&self, root: &Hash) -> Option<Box<dyn Tree + '_>> {
        self.inner.tree(root)
    }

    fn physics_image(&self, root: &Hash) -> Option<PhysicsImage> {
        self.inner.physics_image(root)
    }

    fn set_served(&mut self, roots: &[Hash]) {
        self.inner.set_served(roots)
    }

    fn import_at_cut(
        &mut self,
        group: &GroupSel,
        import: CutImport,
        source: &mut dyn EntrySource,
    ) -> Result<Hash, ImportError> {
        self.inner.import_at_cut(group, import, source)
    }

    fn view(&self) -> Arc<FrameView> {
        self.inner.view()
    }

    fn world_snapshot(&self) -> Option<Arc<dyn Any + Send + Sync>> {
        self.inner.world_snapshot()
    }

    fn agreed_snapshot(&self) -> Option<(Frame, Arc<dyn Any + Send + Sync>)> {
        self.inner.agreed_snapshot()
    }

    fn vote_holds(&self) -> Vec<Frame> {
        self.inner.vote_holds()
    }

    fn runner_identity(&self) -> Option<RunnerIdentity> {
        self.inner.runner_identity()
    }

    fn module(&self, hash: &Hash) -> Option<Bytes> {
        self.inner.module(hash)
    }

    fn script_notes(&self) -> pe_seams::replica::ScriptNotes {
        self.inner.script_notes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pe_replica::mock::{MockClock, MockWorld};
    use pe_replica::{ReplicaConfig, ReplicaImpl};
    use pe_seams::SimRate;

    /// `Timed` passes the provided methods through as well (S2's wrapper rule): here `set_served`, a donor's served
    /// roots (S2 v1.9), whose default would let a resync's snapshot be released while the slow member fetches it.
    #[test]
    fn timed_passes_set_served_through() {
        let rate = SimRate::DEFAULT;
        let config = ReplicaConfig::new(rate, Duration::from_millis(100), Arc::new(MockClock::new(rate)));
        let mut inner = ReplicaImpl::new(MockWorld::new(DomainId(0), rate), config).expect("a replica");
        let mut times = PhaseTimes::default();
        let served = [Hash::of("pe-app.test", b"served")];
        Timed { inner: &mut inner, times: &mut times, log: None, coalesce: None }.set_served(&served);
        assert_eq!(inner.served(), served);
        assert_eq!(times, PhaseTimes::default(), "nothing to time");
    }
}
