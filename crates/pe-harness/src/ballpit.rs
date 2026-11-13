//! render.md 14 test 6's run (@coordinator-opus, option (b)): several pe-replica members over the
//! harness's network, each a `ReplicaImpl<PeWorld>` opened from M1's ball pit, with a push scenario, recording every
//! member's frame views over time. A run is a pure function of its [`BallPit`]: the same seed and delay give the same
//! views on any machine. @ballpit-viz's adapter feeds each member's views to its own `DisplayLayer`.
//!
//! - **Members** are [`SeamNode`]s over the lockstep mock (S3) and `ReplicaImpl<PeWorld>` (S2), all admitted at frame
//!   0 with edit permission, each with its hands at its own spawn point.
//! - **Pushes are edits for now:** each lifts one ball above the pile, from which it drops and scatters others. Hand
//!   pushes replace them once PeWorld applies hand inputs (@vdw-physics).
//! - **Views:** pe-replica's view publish, with `ReplicaConfig::local` set to each member's key, so a member's views
//!   mark its own hands `OWN_HAND` (and what they hold `HELD_LOCAL`), and objects a re-simulation moved `CORRECTED`.
//! - **Checked:** P1, P3, P4, P6, P8 and P15 from the trace. P2 needs an offline replay over pe-world, not built yet.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pe_replica::{PeWorld, ReplicaConfig, ReplicaImpl};
use pe_seams::test_util::TestKeys;
use pe_seams::{Class, DomainId, Frame, FrameView, HostTime, MemberKey, ObjectId, Pos, SimRate};
use pe_sim::{Component, Transform};
use pe_world::edit::{Actor, Op, Row, Target, Transaction};
use pe_world::{BlobStore, World, perm, preset};

use crate::clock::{MS, Nanos, SEC, SessionClock};
use crate::net::{LinkCfg, NodeId};
use crate::props::{self, Violation};
use crate::rng::{self, Purpose};
use crate::seamnode::{SeamNode, ViewSink};
use crate::sim::{Run, Scenario, run as run_scenario};

/// The run's simulation rate.
pub const HZ: u32 = 60;
/// The lockstep mock's cut cadence, and so the replicas' cut interval (6 frames at 60 Hz).
pub const CADENCE_NS: i64 = 100_000_000;
/// The draw stream of the seeded pushes.
const PUSH_STREAM: u64 = 0xBA11_9175;
/// Where each member's hands spawn, relative to the pit's origin: around the pit, outside its walls. Fixed offsets,
/// not trigonometry, so every machine places them alike.
const SPAWNS: [[f64; 3]; 4] = [[1.8, 0.0, 0.0], [-1.8, 0.0, 0.0], [0.0, 0.0, 1.8], [0.0, 0.0, -1.8]];

/// One push: at host time `at`, `member` lifts ball `ball` (the preset's `Ball <ball>`) to `to`, relative to the pit's
/// root (its floor's top).
#[derive(Clone, Debug, PartialEq)]
pub struct Push {
    pub at: Nanos,
    pub member: NodeId,
    pub ball: usize,
    pub to: [f64; 3],
}

/// A ball-pit run: who, over which links, for how long, and the pushes.
#[derive(Clone, Debug)]
pub struct BallPit {
    /// The network's and the pit's seed.
    pub seed: u64,
    /// The one-way delay on every link, and its jitter (uniform in `0..=jitter_ns`).
    pub delay_ns: Nanos,
    pub jitter_ns: Nanos,
    pub members: u32,
    /// How many balls the pit has (`preset::ball_pit_with`'s count).
    pub balls: usize,
    pub end: Nanos,
    pub pushes: Vec<Push>,
}

impl BallPit {
    /// `members` members and `balls` balls for `end`, over links of `delay_ns` with a quarter of it as jitter, with a
    /// seeded push every `every` from 0.5 s until a second before the end (so every push is final before it).
    pub fn new(seed: u64, delay_ns: Nanos, members: u32, balls: usize, end: Nanos, every: Nanos) -> BallPit {
        let pushes = seeded_pushes(seed, members, balls, 500 * MS, end.saturating_sub(SEC), every);
        BallPit { seed, delay_ns, jitter_ns: delay_ns / 4, members, balls, end, pushes }
    }
}

/// A push every `every` in `from..to`, each from a member, of a ball, to a point above the pile, all drawn from
/// `seed`.
pub fn seeded_pushes(seed: u64, members: u32, balls: usize, from: Nanos, to: Nanos, every: Nanos) -> Vec<Push> {
    let d = |i: u64, k: u64, n: u64| rng::below(rng::draw(seed, PUSH_STREAM, i * 8 + k, Purpose::Schedule), n);
    let mm = |x: u64| x as f64 / 1000.0;
    (0..)
        .map(|i| (i, from + i * every.max(1)))
        .take_while(|&(_, at)| at < to)
        .map(|(i, at)| Push {
            at,
            member: d(i, 0, u64::from(members.max(1))) as NodeId + 1,
            ball: d(i, 1, balls.max(1) as u64) as usize,
            to: [mm(d(i, 2, 2001)) - 1.0, 0.9, mm(d(i, 3, 2001)) - 1.0],
        })
        .collect()
}

/// The run's member keys: node `i` is the `i`-th in key order.
pub fn keys(members: u32) -> TestKeys {
    TestKeys::new(17, members)
}

/// The world every member opens: the ball pit with `balls` balls, and every member admitted at frame 0 with edit
/// permission, its hands at its spawn point. `None` if the world refuses an admission.
pub fn genesis(seed: u64, balls: usize, keys: &TestKeys) -> Option<World> {
    let mut w = preset::ball_pit_with(seed, BlobStore::default(), Pos::ORIGIN, balls);
    for (i, key) in keys.sorted_keys().into_iter().enumerate() {
        let mut off = SPAWNS[i % SPAWNS.len()];
        off[0] += 0.6 * (i / SPAWNS.len()) as f64;
        let spawn = Pos::ORIGIN.checked_add(pe_seams::Offset(off)).ok()?;
        w.admit(preset::m1_line(), key, perm::EDIT, Frame(0), spawn).ok()?;
    }
    Some(w)
}

/// Member `local`'s replica over `world`, at [`HZ`], cutting every [`CADENCE_NS`], on the session's time base.
pub fn replica(world: World, local: MemberKey) -> Option<ReplicaImpl<PeWorld>> {
    let rate = SimRate::new(HZ)?;
    let clock = Arc::new(SessionClock { rate, start: HostTime(0) });
    let mut config = ReplicaConfig::new(rate, Duration::from_nanos(CADENCE_NS as u64), clock);
    config.local = Some(local);
    ReplicaImpl::new(PeWorld::new(world, DomainId(0)), config).ok()
}

/// The edit that lifts `ball` to `to`, relative to the pit's root.
pub fn push_payload(ball: ObjectId, to: [f64; 3]) -> Vec<u8> {
    let t = Transform::local(to);
    let row = Row { component: Transform::ID, instance: 0, bytes: bytemuck::bytes_of(&t).to_vec() };
    let ops = vec![Op::SetRow { id: Target::Existing(ball), row }];
    Transaction { actor: Actor::Person, label: None, preconditions: Vec::new(), ops, blobs: Vec::new() }.encode()
}

/// Each member's view of one frame.
pub type ViewsAt = Vec<(NodeId, Arc<FrameView>)>;

/// A finished run: the trace, and each member's views, in publication order, with the host time each was read at.
pub struct BallPitRun {
    pub run: Run,
    pub views: BTreeMap<NodeId, Vec<(Nanos, Arc<FrameView>)>>,
    /// The balls, by index (`Ball <i>`).
    pub balls: Vec<ObjectId>,
}

impl BallPitRun {
    /// The properties a ball-pit run must keep: P1, P3, P4, P6, P8 and P15.
    pub fn violations(&self) -> Vec<Violation> {
        let t = &self.run.trace;
        let mut out = props::p1_agreement(t);
        out.extend(props::p3_no_final_undone(t));
        out.extend(props::p4_voids(t));
        out.extend(props::p6_no_abort(t));
        out.extend(props::p8_no_pause(t));
        out.extend(props::p15_source_contract(t));
        out
    }

    /// The newest frame every member published a view of, with each member's newest view of it.
    pub fn last_common(&self) -> Option<(Frame, ViewsAt)> {
        let newest = |vs: &Vec<(Nanos, Arc<FrameView>)>| {
            let mut by: BTreeMap<Frame, Arc<FrameView>> = BTreeMap::new();
            for (_, v) in vs {
                by.insert(v.frame, Arc::clone(v));
            }
            by
        };
        let per: Vec<(NodeId, BTreeMap<Frame, Arc<FrameView>>)> =
            self.views.iter().map(|(&n, vs)| (n, newest(vs))).collect();
        let (_, first) = per.first()?;
        let f = first.keys().rev().find(|f| per.iter().all(|(_, by)| by.contains_key(f))).copied()?;
        Some((f, per.iter().filter_map(|(n, by)| by.get(&f).map(|v| (*n, Arc::clone(v)))).collect()))
    }
}

/// Runs `cfg`. `None` if the world or a replica can't be built (a bug in the preset or the replica, not the run).
pub fn run(cfg: &BallPit) -> Option<BallPitRun> {
    let keys = keys(cfg.members);
    let world = genesis(cfg.seed, cfg.balls, &keys)?;
    let balls: Vec<ObjectId> = (0..cfg.balls)
        .map(|i| preset::find(&world, &format!("{}{i}", preset::names::BALL_PREFIX)))
        .collect::<Option<_>>()?;
    let rate = SimRate::new(HZ)?;
    let mut sinks: BTreeMap<NodeId, ViewSink> = BTreeMap::new();
    let mut nodes = BTreeMap::new();
    let sorted = keys.sorted_keys();
    for id in 1..=cfg.members {
        let sink: ViewSink = Arc::new(Mutex::new(Vec::new()));
        let key = *sorted.get(id as usize - 1)?;
        let mut node = SeamNode::lockstep_over(&keys, id, replica(world.clone(), key)?, rate, CADENCE_NS)
            .record_views(sink.clone());
        for p in cfg.pushes.iter().filter(|p| p.member == id) {
            let ball = *balls.get(p.ball)?;
            node = node.submit_at(p.at as i64, Class::Edit, push_payload(ball, p.to));
        }
        sinks.insert(id, sink);
        nodes.insert(id, node);
    }
    let mut sc = Scenario::new(cfg.seed, cfg.end);
    sc.link = LinkCfg { delay_ns: cfg.delay_ns, jitter_ns: cfg.jitter_ns, ..LinkCfg::default() };
    let run = run_scenario(&sc, nodes);
    let views = sinks
        .into_iter()
        .map(|(id, s)| {
            let vs = s.lock().map(|mut v| std::mem::take(&mut *v)).unwrap_or_default();
            (id, vs.into_iter().map(|(t, v)| (t as Nanos, v)).collect())
        })
        .collect();
    Some(BallPitRun { run, views, balls })
}
