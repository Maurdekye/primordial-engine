//! A cut the live history matches takes the live root, and a snapshot takes the transfer its take_root serialized
//! (replica.md 5.4). The same roots, transfers and check hashes come out as when every root is re-stepped and every
//! transfer serialized again (`ReplicaConfig::reuse_live` off), over a run shaped like a two-member session.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use pe_replica::mock::MockClock;
use pe_replica::{PeWorld, ReplicaConfig, ReplicaImpl};
use pe_seams::test_util::{EntryBuilder, TestKeys};
use pe_seams::{
    ChainHead, Class, Cut, CutRef, DomainId, EntrySource, FinalCut, Finalized, Frame, Hash, HostTime, LineId, ObjectId,
    Pos, Replica, SignedEntry,
};
use pe_world::{BlobStore, Input, World, perm};

/// Agreement's stand-in: entries by frame, in a fixed order; nothing is trimmed.
#[derive(Default)]
struct Source {
    entries: BTreeMap<Frame, Vec<SignedEntry>>,
    changed: Option<Frame>,
}

impl Source {
    fn add(&mut self, e: SignedEntry) {
        let f = e.entry.stamp;
        let at = self.entries.entry(f).or_default();
        at.push(e);
        at.sort_by_key(|e| (e.entry.chain, e.entry.seq));
        self.changed = Some(self.changed.map_or(f, |c| c.min(f)));
    }
}

impl EntrySource for Source {
    fn final_cut(&self) -> Option<&FinalCut> {
        None
    }
    fn entries_at(&self, frame: Frame) -> Option<&[SignedEntry]> {
        Some(self.entries.get(&frame).map_or(&[][..], Vec::as_slice))
    }
    fn kept_from(&self) -> Frame {
        Frame(0)
    }
    fn keep_final_from(&mut self, _cv: Frame, _floor: Frame) {}
    fn take_changed_from(&mut self) -> Option<Frame> {
        self.changed.take()
    }
}

const K: u64 = 8;
const CLOCK: MockClock = MockClock { rate: pe_seams::SimRate::DEFAULT, start: HostTime(1_234_567_891) };

fn keys() -> TestKeys {
    TestKeys::new(5, 2)
}

/// The M1 ball pit with `balls` balls, both test keys editors, and its first five balls.
fn pit(balls: usize) -> (World, Vec<ObjectId>) {
    let keys = keys();
    let mut world = pe_world::preset::ball_pit_with(7, BlobStore::default(), Pos::ORIGIN, balls);
    world.set_member(keys.key(0), perm::EDIT);
    world.set_member(keys.key(1), perm::EDIT);
    let ids = (0..5).map(|i| pe_world::preset::find(&world, &format!("Ball {i}")).expect("a ball")).collect();
    (world, ids)
}

fn replica_of(world: World, reuse_live: bool) -> ReplicaImpl<PeWorld> {
    replica_on_grid(world, reuse_live, K)
}

/// pe-app's cut grid: a cut every 100 ms, 6 frames at 60 Hz.
const K_APP: u64 = 6;

fn replica_on_grid(world: World, reuse_live: bool, cut_every: u64) -> ReplicaImpl<PeWorld> {
    let mut config = ReplicaConfig::new(pe_seams::SimRate::DEFAULT, Duration::from_millis(133), Arc::new(CLOCK));
    config.cut_every = cut_every;
    config.reuse_live = reuse_live;
    ReplicaImpl::new(PeWorld::new(world, DomainId(0)), config).expect("a replica")
}

fn push(b: &mut EntryBuilder, at: u64, object: ObjectId, impulse: [f32; 3]) -> SignedEntry {
    b.next(Class::Input, Frame(at), Bytes::from(Input::Push { object, impulse, at: None }.encode()))
}

/// The cut at `frame` over `entries`: per author, the last entry stamped below it.
fn cut_at(entries: &[SignedEntry], height: u64, frame: u64) -> Cut {
    let mut heads: BTreeMap<_, ChainHead> = BTreeMap::new();
    for e in entries.iter().filter(|e| e.entry.stamp.0 < frame) {
        let h = ChainHead { chain: e.entry.chain, author: e.entry.author, seq: e.entry.seq, entry: e.id() };
        if heads.get(&h.chain).is_none_or(|old| old.seq < h.seq) {
            heads.insert(h.chain, h);
        }
    }
    Cut { at: CutRef { line: LineId::origin(), height, frame: Frame(frame) }, heads: heads.into_values().collect() }
}

/// Two replicas of one world, one taking matched roots from the live history and kept transfers (`on`), one
/// re-stepping and serializing everything (`off`), each with its own copy of the entries.
struct Pair {
    on: ReplicaImpl<PeWorld>,
    off: ReplicaImpl<PeWorld>,
    s_on: Source,
    s_off: Source,
}

impl Pair {
    fn on_grid(world: World, cut_every: u64) -> Pair {
        Pair {
            on: replica_on_grid(world.clone(), true, cut_every),
            off: replica_on_grid(world, false, cut_every),
            s_on: Source::default(),
            s_off: Source::default(),
        }
    }

    /// Steps both live histories one frame at a time, as pe-app's ticks do, to `present`.
    fn step_to(&mut self, present: u64) {
        while self.on.frame().0 < present {
            let next = self.on.frame().0 + 1;
            self.advance(next);
        }
    }

    /// `on`'s roots taken from the live history so far, and the frames it re-stepped.
    fn reuse(&self) -> (u64, u64) {
        let s = self.on.scratch();
        (s.reused, s.frames)
    }

    fn add(&mut self, e: &SignedEntry) {
        self.s_on.add(e.clone());
        self.s_off.add(e.clone());
    }

    fn advance(&mut self, present: u64) {
        self.on.advance(Frame(present), &mut self.s_on);
        self.off.advance(Frame(present), &mut self.s_off);
        assert_eq!((self.on.frame(), self.off.frame()), (Frame(present), Frame(present)));
        assert_eq!(self.on.view().frame, self.off.view().frame);
    }

    /// Both replicas' root at `cut`, which must be the same, with the same physics image: the transfer's bytes and
    /// its check hash. Shown short on a mismatch, since a transfer is tens of kB.
    fn root(&mut self, cut: &Cut, what: &str) -> Hash {
        let on = self.on.root_at_cut(cut, &mut self.s_on).unwrap_or_else(|e| panic!("{what}: on {e:?}"));
        let off = self.off.root_at_cut(cut, &mut self.s_off).unwrap_or_else(|e| panic!("{what}: off {e:?}"));
        assert_eq!(on, off, "{what}: the root");
        self.same_image(&on, what);
        on
    }

    fn same_image(&self, root: &Hash, what: &str) {
        let (a, b) = (self.on.physics_image(root), self.off.physics_image(root));
        let (a, b) = (a.unwrap_or_else(|| panic!("{what}: on holds no image")), b.expect("off holds the image"));
        assert_eq!((a.domain, a.frame, a.global), (b.domain, b.frame, b.global), "{what}: the image's check hash");
        assert!(a.bytes == b.bytes, "{what}: the transfer's bytes ({} and {} bytes)", a.bytes.len(), b.bytes.len());
    }

    fn finalize(&mut self, cut: &Cut, root: Hash, what: &str) {
        let fin = FinalCut { cut: cut.clone(), root, voters: vec![] };
        let on = self.on.finalize(&fin, &mut self.s_on);
        let off = self.off.finalize(&fin, &mut self.s_off);
        assert!(matches!(on, Finalized::Agreed), "{what}: on {on:?}");
        assert!(matches!(off, Finalized::Agreed), "{what}: off {off:?}");
        self.same_image(&root, what);
    }
}

/// A 24-ball pit to frame 214. A pushes four balls every 30 frames; B pushes a fifth at every cut's frame less 6, and
/// each push reaches the replicas 4 frames after its stamp, so the live history rolls back at every cut. At each cut
/// frame f, 6 frames after the live history passes it: a vote on the cut that leaves out B's push at f - 6 (re-stepped,
/// from a held root where the histories match), then one on the cut with every entry (the live history's). Finality
/// keeps up to frame 80, stalls from 88 to 152, and then catches up at 158 in order: there some cuts have lost their
/// vote's snapshot and the ring's keyframe to the thinning, and take the keyframe and transfer kept for their frame.
/// The last final cut leaves out B's last push, so the floor is re-based over it. Every root, every physics image and
/// the live roots after are the same with the reuse as without it, and every matched cut took the live root.
#[test]
fn the_live_root_and_the_kept_transfer_give_what_a_re_step_gives() {
    let (to, lag, late) = (200, 6, 4);
    let (world, ids) = pit(24);
    let keys = keys();
    let (mut a, mut b) = (EntryBuilder::new(&keys, 0), EntryBuilder::new(&keys, 1));
    let mut pair = Pair {
        on: replica_of(world.clone(), true),
        off: replica_of(world, false),
        s_on: Source::default(),
        s_off: Source::default(),
    };
    let mut known = Vec::new();
    for f in (5..to + 2 * K).step_by(30) {
        for (i, &object) in ids[..4].iter().enumerate() {
            let e = push(&mut a, f, object, [0.02 * (i as f32 - 1.5), 0.03, 0.01]);
            pair.add(&e);
            known.push(e);
        }
    }
    let remote: Vec<SignedEntry> =
        (16..=to).step_by(K as usize).map(|f| push(&mut b, f - 6, ids[4], [0.0, 0.02, 0.0])).collect();
    let b_chain = remote[0].entry.chain;
    let mut arrived = 0;
    let mut stalled: Vec<(Cut, Hash)> = Vec::new();
    let mut last_late = None;
    for (n, f) in (16..=to).step_by(K as usize).enumerate() {
        while pair.on.frame().0 < f + lag {
            let present = pair.on.frame().0 + 1;
            while remote.get(arrived).is_some_and(|e| e.entry.stamp.0 + late <= present) {
                pair.add(&remote[arrived]);
                known.push(remote[arrived].clone());
                arrived += 1;
            }
            pair.advance(present);
        }
        let h = 2 * n as u64 + 2;
        let before_late: Vec<SignedEntry> =
            known.iter().filter(|e| e.entry.chain != b_chain || e.entry.stamp.0 < f - 6).cloned().collect();
        let late_cut = cut_at(&before_late, h, f);
        let late_root = pair.root(&late_cut, &format!("the cut at {f} without B's push at {}", f - 6));
        let all = cut_at(&known, h + 1, f);
        let root = pair.root(&all, &format!("the full cut at {f}"));
        assert_eq!(pair.on.live_root(Frame(f)), Some(root), "the full cut's root is the live one at {f}");
        assert_eq!(pair.off.live_root(Frame(f)), Some(root));
        match f {
            ..=80 => pair.finalize(&all, root, &format!("the final cut at {f}")),
            88..=144 => stalled.push((all, root)),
            152 => {
                stalled.push((all, root));
                for (cut, root) in std::mem::take(&mut stalled) {
                    let what = format!("the stalled final cut at {}, from 158", cut.at.frame.0);
                    pair.finalize(&cut, root, &what);
                }
            }
            _ if f < to => pair.finalize(&all, root, &format!("the final cut at {f}")),
            // The last: a final cut without B's push at f - 6, which the live history stepped, so the floor is
            // re-based over it.
            _ => last_late = Some((late_cut, late_root)),
        }
    }
    let (late_cut, late_root) = last_late.expect("the last cut");
    pair.finalize(&late_cut, late_root, "the last final cut, without B's last push");
    for present in to + lag + 1..=to + lag + 2 * K {
        pair.advance(present);
    }
    let end = to + 2 * K;
    assert_eq!(pair.on.live_root(Frame(end)), pair.off.live_root(Frame(end)), "the live root at {end}");
    assert!(pair.on.live_root(Frame(end)).is_some(), "re-simulated to {end}");
    assert_eq!(pair.on.held_count(), pair.off.held_count(), "the snapshots held");
    assert_eq!((pair.on.faults(), pair.off.faults()), (0, 0));
    let (on, off) = (pair.on.scratch(), pair.off.scratch());
    assert_eq!(off.reused, 0, "off re-steps every root");
    // 24 votes on the full cuts and 23 final cuts: every matched cut, the stalled ones included.
    assert!(on.reused >= 47, "on took every matched root from the live history: {on:?}");
    assert!(on.frames < off.frames, "and re-stepped fewer frames: {on:?} against {off:?}");
}

/// On pe-app's grid (a cut every 6 frames at 60 Hz), every vote comes 30 frames after the live history passed its
/// cut, and finality follows each vote at once. By then the ring keeps only some of those frames' keyframes (it thins
/// past 16 frames), and five more grid frames have passed. Each cut still takes the live root, with no re-step, from
/// the transfer and keyframe kept for its frame, and its root and image are a re-step's. The live history keeps
/// grid frames only from the floor up.
#[test]
fn a_vote_30_frames_late_takes_the_live_root() {
    let (to, late_by) = (150, 30);
    let (world, ids) = pit(24);
    let keys = keys();
    let mut a = EntryBuilder::new(&keys, 0);
    let mut pair = Pair::on_grid(world, K_APP);
    let mut known = Vec::new();
    for f in (5..to + late_by).step_by(30) {
        for (i, &object) in ids[..4].iter().enumerate() {
            let e = push(&mut a, f, object, [0.02 * (i as f32 - 1.5), 0.03, 0.01]);
            pair.add(&e);
            known.push(e);
        }
    }
    for (n, f) in (12..=to).step_by(K_APP as usize).enumerate() {
        pair.step_to(f + late_by);
        let (reused, frames) = pair.reuse();
        let cut = cut_at(&known, n as u64 + 1, f);
        let root = pair.root(&cut, &format!("the vote at {f}, {late_by} frames late"));
        assert_eq!(pair.on.live_root(Frame(f)), Some(root), "the live root at {f}");
        assert_eq!(pair.reuse(), (reused + 1, frames), "the vote at {f} took the live root");
        pair.finalize(&cut, root, &format!("the final cut at {f}"));
        assert!(pair.on.live_kept() <= 6, "grid frames from the floor at {f}: {}", pair.on.live_kept());
    }
    assert_eq!((pair.on.faults(), pair.off.faults()), (0, 0));
}

/// Finality stalls, so the live history keeps the transfers and keyframes of its newest 16 grid frames only, the oldest
/// going first. With the live history at 108 on pe-app's grid, the vote at 12, the 17th grid frame back, re-steps
/// from the ring, with the root and image a re-step gives; the vote at 18, the oldest kept, takes the live root.
#[test]
fn past_the_kept_grid_frames_a_late_vote_re_steps() {
    let (world, ids) = pit(24);
    let keys = keys();
    let mut a = EntryBuilder::new(&keys, 0);
    let mut pair = Pair::on_grid(world, K_APP);
    let mut known = Vec::new();
    for (i, &object) in ids[..4].iter().enumerate() {
        let e = push(&mut a, 5, object, [0.02 * (i as f32 - 1.5), 0.03, 0.01]);
        pair.add(&e);
        known.push(e);
    }
    pair.step_to(108);
    assert_eq!(pair.on.live_kept(), 16, "the grid frames 18 to 108");
    let (reused, frames) = pair.reuse();
    let root = pair.root(&cut_at(&known, 1, 12), "the vote at 12");
    assert_eq!(pair.on.live_root(Frame(12)), Some(root));
    let (after, stepped) = pair.reuse();
    assert_eq!(after, reused, "the vote at 12 is past the kept frames: re-stepped");
    assert!(stepped > frames, "re-stepped from the ring: {frames} then {stepped} frames");
    pair.root(&cut_at(&known, 2, 18), "the vote at 18");
    assert_eq!(pair.reuse().0, reused + 1, "the vote at 18 took the live root");
    assert_eq!(pair.on.live_kept(), 16, "nothing more is kept for a vote");
    assert_eq!((pair.on.faults(), pair.off.faults()), (0, 0));
}
