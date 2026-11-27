//! The world a machine opens (app.md 4, step 5), on pe-world through pe-replica's [`PeWorld`]: a new, empty world from
//! the origin ([`open_new`]), or M1's ball pit with the local member admitted ([`open_ball_pit`]). An opened save and a
//! joiner's import come later. Each is under [`FaultWorld`], which injects `--inject-fault-at`'s fault when this
//! process armed one ([`crate::fault`]).

use std::sync::{Arc, OnceLock};

use pe_replica::{CostProbe, PeWorld, ReplicaImpl, ScriptRunner, WorldModel};
use pe_seams::{DomainId, Frame, FrameClock, MemberKey, Offset, Pos, Replica, SimRate};
use pe_world::{BlobStore, World, perm, preset};

use crate::fault::{Fault, FaultWorld};

/// The member key this machine plays as until pe-net-crypto's identity lands (app.md 4, step 2). A fixed key is enough
/// for a world alone: the ball pit admits it as its admin, and the view marks its hands as the local player's.
// TODO(pe-net-crypto): the member key from this machine's identity file.
pub const DEV_MEMBER: MemberKey = MemberKey([0x1d; 32]);

/// A new, empty world at frame 0 under pe-replica's replica, at `rate`. The replica takes the frame clock, so the
/// view's `sim_time` is on the shared time base (S5). `seed` and `rate` are world settings; they come from the world
/// once configuration names it (app.md 4, step 1). The cut grid is this build's consensus setting, as in a session
/// ([`crate::genesis::replica_config`]). `runner` runs the world's scripts (pe-replica's `logic`): pe-app owns it,
/// since the build service hands it its modules.
pub fn open_new(
    seed: u64,
    rate: SimRate,
    clock: Arc<dyn FrameClock>,
    runner: Box<dyn ScriptRunner>,
) -> Result<Box<dyn Replica>, String> {
    let mut world = World::new(seed, BlobStore::default());
    world.set_rate(rate);
    let config = crate::genesis::replica_config(&crate::genesis::settings(), rate, clock);
    let replica =
        ReplicaImpl::new(FaultWorld::armed(PeWorld::new(world, DomainId(0))), config).map_err(|e| format!("{e:?}"))?;
    Ok(Box::new(probed(replica.with_runner(runner))))
}

/// The replay costs' probe (pe_replica::costs) this process gives every replica it opens, once [`arm_cost_probe`]
/// made it: this machine's own counters and timers, never agreed state. `SimStats::replay_costs` reads it.
static COST_PROBE: OnceLock<CostProbe> = OnceLock::new();

/// Make this process's replay-cost probe (main does, unless `PE_REPLAY_COSTS=0`): every replica opened from now on
/// measures into it.
pub fn arm_cost_probe() -> CostProbe {
    COST_PROBE.get_or_init(pe_replica::costs::probe).clone()
}

/// This process's replay-cost probe, if armed.
pub fn cost_probe() -> Option<CostProbe> {
    COST_PROBE.get().cloned()
}

/// `replica`, measuring into this process's replay-cost probe if one is armed.
pub fn probed<W: WorldModel>(replica: ReplicaImpl<W>) -> ReplicaImpl<W> {
    match cost_probe() {
        Some(p) => replica.with_cost_probe(p),
        None => replica,
    }
}

/// Where the `i`-th admitted member's hands start, before its first hand input moves them: at chest height over the
/// pit's near edge, 0.6 m apart along it. In VR the display draws the local hands at the headset's late-latched
/// poses wherever this is.
fn hand_spawn(i: usize) -> Pos {
    Pos::ORIGIN.checked_add(Offset([0.6 * i as f64, 1.2, 1.5])).unwrap_or(Pos::ORIGIN)
}

/// M1's ball pit (@world-model's disposable genesis preset), its floor's top at the origin, with `me` admitted at frame 0
/// as its admin, so the world holds `me`'s two hands (world-model.md 8). The replica marks `me`'s hands, and what they
/// hold, as the local player's in the frame view (`ReplicaConfig::local`, S5), so the display draws them from tracking.
///
/// `seed` places the balls. The rate is the world's own setting (app.md 4, step 1): the frame clock must run at it, and
/// a clock at another rate is refused rather than the world's setting overridden.
pub fn open_ball_pit(
    seed: u64,
    me: MemberKey,
    rate: SimRate,
    clock: Arc<dyn FrameClock>,
    runner: Box<dyn ScriptRunner>,
) -> Result<Box<dyn Replica>, String> {
    ball_pit(preset::ball_pit(seed, BlobStore::default(), Pos::ORIGIN), &[me], me, rate, clock, runner)
}

/// A ball pit `world` with every one of `members` admitted at frame 0 as an admin, in key order so that every machine
/// builds the same state (pe-agree M1's membership is fixed at the line's start), and `me`'s hands marked as the
/// local player's.
pub(crate) fn ball_pit(
    world: World,
    members: &[MemberKey],
    me: MemberKey,
    rate: SimRate,
    clock: Arc<dyn FrameClock>,
    runner: Box<dyn ScriptRunner>,
) -> Result<Box<dyn Replica>, String> {
    Ok(Box::new(ball_pit_replica(world, members, me, rate, clock, runner)?))
}

/// [`ball_pit`]'s replica, unboxed: a test reaches pe-replica's test-only damage through it. Over this process's armed
/// fault, if any ([`crate::fault::armed`]).
pub(crate) fn ball_pit_replica(
    world: World,
    members: &[MemberKey],
    me: MemberKey,
    rate: SimRate,
    clock: Arc<dyn FrameClock>,
    runner: Box<dyn ScriptRunner>,
) -> Result<ReplicaImpl<FaultWorld>, String> {
    ball_pit_replica_with(world, members, me, rate, clock, runner, crate::fault::armed())
}

/// [`ball_pit_replica`] with `fault` injected, whatever this process armed: a test gives one member a fault.
pub(crate) fn ball_pit_replica_with(
    world: World,
    members: &[MemberKey],
    me: MemberKey,
    rate: SimRate,
    clock: Arc<dyn FrameClock>,
    runner: Box<dyn ScriptRunner>,
    fault: Option<Arc<Fault>>,
) -> Result<ReplicaImpl<FaultWorld>, String> {
    ball_pit_replica_over(world, members, me, rate, clock, runner, |w| FaultWorld::new(w, fault))
}

/// [`ball_pit_replica_with`] over the model `wrap` makes of the world: a test slows one replica
/// ([`FaultWorld::with_slow`]).
pub(crate) fn ball_pit_replica_over(
    mut world: World,
    members: &[MemberKey],
    me: MemberKey,
    rate: SimRate,
    clock: Arc<dyn FrameClock>,
    runner: Box<dyn ScriptRunner>,
    wrap: impl FnOnce(PeWorld) -> FaultWorld,
) -> Result<ReplicaImpl<FaultWorld>, String> {
    if world.rate() != rate {
        return Err(format!("the ball pit runs at {:?}, the frame clock at {rate:?}", world.rate()));
    }
    let mut sorted = members.to_vec();
    sorted.sort();
    sorted.dedup();
    if !sorted.contains(&me) {
        return Err("the local member is not one of the world's members".into());
    }
    for (i, m) in sorted.iter().enumerate() {
        world
            .admit(preset::m1_line(), *m, perm::ADMIN | perm::EDIT | perm::AGENT, Frame(0), hand_spawn(i))
            .map_err(|e| format!("admitting member {i}: {e:?}"))?;
    }
    let mut config = crate::genesis::replica_config(&crate::genesis::settings(), rate, clock);
    config.local = Some(me);
    let replica = ReplicaImpl::new(wrap(PeWorld::new(world, DomainId(0))), config).map_err(|e| format!("{e:?}"))?;
    Ok(probed(replica.with_runner(runner)))
}
