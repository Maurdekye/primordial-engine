//! render.md 14 test 6's run (@coordinator-opus, option (b)): several pe-replica members over the
//! harness's network, each a `ReplicaImpl<PeWorld>` opened from M1's ball pit, with a push scenario, recording every
//! member's frame views over time. A run is a pure function of its [`BallPit`]: the same seed and delay give the same
//! views on any machine. @ballpit-viz's adapter feeds each member's views to its own `DisplayLayer`.
//!
//! - **Members** are [`SeamNode`]s over the lockstep mock (S3) and `ReplicaImpl<PeWorld>` (S2), all admitted at frame
//!   0 with edit permission, each with its hands at its own spawn point.
//! - **Two kinds of push:** a lift is an edit that puts one ball above the pile, from which it drops and scatters
//!   others. A hand push is two inputs (PeWorld's, @vdw-physics): the member moves its first hand above the ball's
//!   place in the genesis world and holds it there, then pushes the ball with an impulse at its centre. The two are
//!   independent checks of two inputs: the push names the ball and acts at its centre (`at: None`), so it does not
//!   depend on where the hand is, and after lifts the ball may be far from its genesis place. pe-world has no reach
//!   rule for pushes today (only a push's `at` must lie on the object, and `None` skips that); if one lands, these
//!   pushes would be refused for it, and the run's rise checks fail.
//! - **Views:** pe-replica's view publish, with `ReplicaConfig::local` set to each member's key, so a member's views
//!   mark its own hands `OWN_HAND` (and what they hold `HELD_LOCAL`), and objects a re-simulation moved `CORRECTED`.
//! - **Checked:** P1, P3, P4, P6, P8 and P15 from the trace, and P2 against a straight replay of the final log
//!   through a fresh member's replica ([`crate::s2replay`]).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pe_replica::logic::LogicRunner;
use pe_replica::{PeWorld, ReplicaConfig, ReplicaImpl};
use pe_seams::test_util::TestKeys;
use pe_seams::{Class, DomainId, Frame, FrameView, HostTime, MemberKey, ObjectId, Pos, SimRate};
use pe_sim::{Component, Transform};
use pe_world::components::Hand;
use pe_world::edit::{Actor, Op, Row, Target, Transaction};
use pe_world::{BlobStore, HandPose, Input, World, perm, preset};

use crate::clock::{MS, Nanos, SEC, SessionClock};
use crate::net::{LinkCfg, NodeId};
use crate::props::{self, Violation};
use crate::rng::{self, Purpose};
use crate::s2replay;
use crate::seamnode::{FinalLog, FinalSink, SeamNode, ViewSink};
use crate::sim::{Run, Scenario, run as run_scenario};
use crate::trace::Ev;

/// The run's simulation rate.
pub const HZ: u32 = 60;
/// The lockstep mock's cut cadence, and so the replicas' cut interval (6 frames at 60 Hz).
pub const CADENCE_NS: i64 = 100_000_000;
/// The draw stream of the seeded pushes.
const PUSH_STREAM: u64 = 0xBA11_9175;
/// The draw stream of the seeded hand pushes.
const HAND_STREAM: u64 = 0x4A9D_9175;
/// How long before its push a member moves its hand into place.
pub const HAND_LEAD: Nanos = 200 * MS;
/// How many frames a hand holds its place after its input's one pose (a second at 60 Hz).
pub const HAND_HOLD: u8 = 60;
/// How far above a ball's genesis place its pusher's hand goes, in metres.
pub const HAND_ABOVE: f64 = 0.25;
/// A hand push's upward impulse, in newton seconds: about 2.2 m/s on an 18 g ball, which then rises about 0.25 m.
/// Upward, because the pile is packed: a level push meets a neighbour or a wall at once, and the ball barely moves.
pub const HAND_IMPULSE: f64 = 0.04;
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

/// One hand push: at `at - HAND_LEAD`, `member` moves its first hand [`HAND_ABOVE`] above ball `ball`'s place in the
/// genesis world and holds it [`HAND_HOLD`] frames; at `at` it pushes the ball with `impulse` (N·s, world frame) at
/// its centre.
#[derive(Clone, Debug, PartialEq)]
pub struct HandPush {
    pub at: Nanos,
    pub member: NodeId,
    pub ball: usize,
    pub impulse: [f32; 3],
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
    pub hand_pushes: Vec<HandPush>,
}

impl BallPit {
    /// `members` members and `balls` balls for `end`, over links of `delay_ns` with a quarter of it as jitter, with a
    /// seeded lift every `every` from 0.5 s until a second before the end (so every push is final before it), and a
    /// seeded hand push every `every` between them.
    pub fn new(seed: u64, delay_ns: Nanos, members: u32, balls: usize, end: Nanos, every: Nanos) -> BallPit {
        let to = end.saturating_sub(SEC);
        let pushes = seeded_pushes(seed, members, balls, 500 * MS, to, every);
        let hand_pushes = seeded_hand_pushes(seed, members, balls, 500 * MS + every / 2, to, every);
        BallPit { seed, delay_ns, jitter_ns: delay_ns / 4, members, balls, end, pushes, hand_pushes }
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

/// A hand push every `every` in `from..to`, each from a member, of a ball, with an upward impulse of [`HAND_IMPULSE`]
/// plus a level one of a quarter of that in a seeded direction (it scatters the ball's neighbours as it lands), all
/// drawn from `seed`. The direction is normalized with `sqrt`, correctly rounded everywhere.
pub fn seeded_hand_pushes(
    seed: u64,
    members: u32,
    balls: usize,
    from: Nanos,
    to: Nanos,
    every: Nanos,
) -> Vec<HandPush> {
    let d = |i: u64, k: u64, n: u64| rng::below(rng::draw(seed, HAND_STREAM, i * 8 + k, Purpose::Schedule), n);
    (0..)
        .map(|i| (i, from + i * every.max(1)))
        .take_while(|&(_, at)| at < to)
        .map(|(i, at)| {
            let (x, z) = (d(i, 2, 2001) as f64 - 1000.0, d(i, 3, 2001) as f64 - 1000.0);
            let n = (x * x + z * z).sqrt();
            let (x, z) = if n < 1.0 { (1.0, 0.0) } else { (x / n, z / n) };
            let s = HAND_IMPULSE;
            HandPush {
                at,
                member: d(i, 0, u64::from(members.max(1))) as NodeId + 1,
                ball: d(i, 1, balls.max(1) as u64) as usize,
                impulse: [(s * 0.25 * x) as f32, s as f32, (s * 0.25 * z) as f32],
            }
        })
        .collect()
}

/// Member `key`'s first hand in `world` (its lowest id), if it has one.
pub fn first_hand(world: &World, key: MemberKey) -> Option<ObjectId> {
    world.store().iter::<Hand>().filter(|(_, h)| h.owner == key).map(|(id, _)| id).min()
}

/// Where a hand push on `ball` puts its pusher's hand: [`HAND_ABOVE`] above the ball's place in `world`.
pub fn hand_target(world: &World, ball: ObjectId) -> Option<Pos> {
    world.store().world_transform(ball)?.pos.checked_add(pe_seams::Offset([0.0, HAND_ABOVE, 0.0])).ok()
}

/// The input that moves `hand` to `to` and holds it there [`HAND_HOLD`] frames.
pub fn hand_payload(hand: ObjectId, to: Pos) -> Vec<u8> {
    let pose = HandPose { pos: to, rot: Transform::IDENTITY.rot, grip: 0.0, engaged: false };
    Input::Hand { hand, poses: vec![pose], hold: HAND_HOLD }.encode()
}

/// The input that claims `ball` for `hand` (world-model.md 7.2): refused `OutOfReach` unless the hand's target is within
/// the world's reach (100 mm, plus 50 mm slack, by default) of the ball's bounding sphere when it applies.
pub fn grab_payload(ball: ObjectId, hand: ObjectId) -> Vec<u8> {
    Input::Grab { object: ball, hand: Some(hand) }.encode()
}

/// The input that pushes `ball` with `impulse` at its centre.
pub fn shove_payload(ball: ObjectId, impulse: [f32; 3]) -> Vec<u8> {
    Input::Push { object: ball, impulse, at: None }.encode()
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

/// Member `local`'s replica over `world`, at [`HZ`], cutting every [`CADENCE_NS`], on the session's time base, with
/// pe-logic's runner for the world's scripts, as pe-app builds it. A thread that steps it through a script needs the
/// runner's `thread_stack()`.
pub fn replica(world: World, local: MemberKey) -> Option<ReplicaImpl<PeWorld>> {
    let rate = SimRate::new(HZ)?;
    let clock = Arc::new(SessionClock { rate, start: HostTime(0) });
    let mut config = ReplicaConfig::new(rate, Duration::from_nanos(CADENCE_NS as u64), clock);
    config.local = Some(local);
    let runner = LogicRunner::with_defaults().ok()?;
    let replica = ReplicaImpl::new(PeWorld::new(world, DomainId(0)), config).ok()?;
    Some(replica.with_runner(Box::new(runner)))
}

/// The edit that lifts `ball` to `to`, relative to the pit's root.
pub fn push_payload(ball: ObjectId, to: [f64; 3]) -> Vec<u8> {
    let t = Transform::local(to);
    let row = Row { component: Transform::ID, instance: 0, bytes: bytemuck::bytes_of(&t).to_vec() };
    let ops = vec![Op::SetRow { id: Target::Existing(ball), row }];
    Transaction { inline: vec![], actor: Actor::Person, label: None, preconditions: Vec::new(), ops, blobs: Vec::new() }
        .encode()
}

/// Each member's view of one frame.
pub type ViewsAt = Vec<(NodeId, Arc<FrameView>)>;

/// A finished run: the trace, and each member's views, in publication order, with the host time each was read at.
pub struct BallPitRun {
    pub run: Run,
    pub views: BTreeMap<NodeId, Vec<(Nanos, Arc<FrameView>)>>,
    /// The balls, by index (`Ball <i>`).
    pub balls: Vec<ObjectId>,
    /// What each member saw become final: P2's replay input.
    pub finals: BTreeMap<NodeId, FinalLog>,
    /// The world every member opened, and a member's key: P2's replay opens a fresh replica from them.
    pub genesis: World,
    pub replay_as: MemberKey,
}

impl BallPitRun {
    /// The properties a ball-pit run must keep: P1, P2, P3, P4, P6, P8, P15 and P16.
    pub fn violations(&self) -> Vec<Violation> {
        let t = &self.run.trace;
        let mut out = props::p1_agreement(t);
        out.extend(props::p2_live_equals_replay(t, &self.replay()));
        out.extend(props::p3_no_final_undone(t));
        out.extend(props::p4_voids(t));
        out.extend(props::p6_no_abort(t));
        out.extend(props::p8_no_pause(t));
        out.extend(props::p15_source_contract(t));
        out.extend(self.p16());
        out
    }

    /// P16 (`changes`): each member's change records against its own final log, then final verdicts across members,
    /// over the longest final log.
    pub fn p16(&self) -> Vec<Violation> {
        let mut out = Vec::new();
        let mut verdicts = BTreeMap::new();
        for (&n, vs) in &self.views {
            let log = self.finals.get(&n).map_or(&[][..], |l| l.entries.as_slice());
            let (ver, vio) = crate::changes::p16_member(n, vs, log);
            out.extend(vio);
            verdicts.insert(n, ver);
        }
        let longest = self.finals.values().map(|l| l.entries.as_slice()).max_by_key(|e| e.len()).unwrap_or(&[]);
        let end = self.views.values().filter_map(|vs| vs.last().map(|x| x.0)).max().unwrap_or(0);
        out.extend(crate::changes::p16_agree(end, &verdicts, longest));
        out
    }

    /// P2's replay: the honest members' longest final log, stepped straight through by a fresh replica over the
    /// genesis world, with its root at every frame an honest member saw final. Empty if the replica can't be built.
    pub fn replay(&self) -> BTreeMap<(crate::trace::Line, crate::trace::Frame), crate::trace::Hash> {
        let t = &self.run.trace;
        let frames: BTreeSet<u64> = t
            .records
            .iter()
            .filter(|r| t.honest.contains(&r.node))
            .filter_map(|r| match r.ev {
                Ev::Final { frame, .. } => Some(frame),
                _ => None,
            })
            .collect();
        let logs: Vec<FinalLog> =
            self.finals.iter().filter(|(n, _)| t.honest.contains(n)).map(|(_, l)| l.clone()).collect();
        replica(self.genesis.clone(), self.replay_as)
            .map(|r| s2replay::replay(r, &s2replay::reference(&logs), &frames))
            .unwrap_or_default()
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

/// A scenario's own inputs for one member, beyond `cfg`'s pushes: given the member, the genesis world, the balls and
/// the member's node, the node with its inputs scheduled (`SeamNode::submit_at`).
pub type Script<'a> = &'a dyn Fn(NodeId, &World, &[ObjectId], Member) -> Member;

/// A ball-pit member's node.
pub type Member = SeamNode<crate::lockstep::LockstepAgreement, ReplicaImpl<PeWorld>>;

/// Runs `cfg`. `None` if the world or a replica can't be built (a bug in the preset or the replica, not the run).
pub fn run(cfg: &BallPit) -> Option<BallPitRun> {
    run_with(cfg, &|_, _, _, node| node)
}

/// Runs `cfg` with `script`'s inputs added to each member's.
pub fn run_with(cfg: &BallPit, script: Script<'_>) -> Option<BallPitRun> {
    let keys = keys(cfg.members);
    let world = genesis(cfg.seed, cfg.balls, &keys)?;
    let balls: Vec<ObjectId> = (0..cfg.balls)
        .map(|i| preset::find(&world, &format!("{}{i}", preset::names::BALL_PREFIX)))
        .collect::<Option<_>>()?;
    let rate = SimRate::new(HZ)?;
    let mut sinks: BTreeMap<NodeId, ViewSink> = BTreeMap::new();
    let mut final_sinks: BTreeMap<NodeId, FinalSink> = BTreeMap::new();
    let mut nodes = BTreeMap::new();
    let sorted = keys.sorted_keys();
    for id in 1..=cfg.members {
        let sink: ViewSink = Arc::new(Mutex::new(Vec::new()));
        let key = *sorted.get(id as usize - 1)?;
        let mut node = SeamNode::lockstep_over(&keys, id, replica(world.clone(), key)?, rate, CADENCE_NS)
            .record_views(sink.clone());
        let fs = FinalSink::default();
        node = node.record_final(fs.clone());
        final_sinks.insert(id, fs);
        for p in cfg.pushes.iter().filter(|p| p.member == id) {
            let ball = *balls.get(p.ball)?;
            node = node.submit_at(p.at as i64, Class::Edit, push_payload(ball, p.to));
        }
        let hand = first_hand(&world, key)?;
        for p in cfg.hand_pushes.iter().filter(|p| p.member == id) {
            let ball = *balls.get(p.ball)?;
            let to = hand_target(&world, ball)?;
            node = node
                .submit_at(p.at.saturating_sub(HAND_LEAD) as i64, Class::Input, hand_payload(hand, to))
                .submit_at(p.at as i64, Class::Input, shove_payload(ball, p.impulse));
        }
        node = script(id, &world, &balls, node);
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
    let finals = final_sinks
        .into_iter()
        .map(|(id, s)| (id, s.lock().map(|mut l| std::mem::take(&mut *l)).unwrap_or_default()))
        .collect();
    Some(BallPitRun { run, views, balls, finals, genesis: world, replay_as: *sorted.first()? })
}
