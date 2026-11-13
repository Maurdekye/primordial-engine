//! The world a machine opens (app.md 4, step 5), on pe-world through pe-replica's [`PeWorld`]: a new, empty world from
//! the origin ([`open_new`]), or M1's ball pit with the local member admitted ([`open_ball_pit`]). An opened save and a
//! joiner's import come later.

use std::sync::Arc;
use std::time::Duration;

use pe_replica::{PeWorld, ReplicaConfig, ReplicaImpl};
use pe_seams::{DomainId, Frame, FrameClock, MemberKey, Offset, Pos, Replica, SimRate};
use pe_world::{BlobStore, World, perm, preset};

/// The member key this machine plays as until pe-net-crypto's identity lands (app.md 4, step 2). A fixed key is enough
/// for a world alone: the ball pit admits it as its admin, and the view marks its hands as the local player's.
// TODO(pe-net-crypto): the member key from this machine's identity file.
pub const DEV_MEMBER: MemberKey = MemberKey([0x1d; 32]);

/// A new, empty world at frame 0 under pe-replica's replica, at `rate`. The replica takes the frame clock, so the
/// view's `sim_time` is on the shared time base (S5). `seed`, `rate` and `cut_interval` are world settings; they come
/// from the world once configuration names it (app.md 4, step 1).
pub fn open_new(
    seed: u64,
    rate: SimRate,
    cut_interval: Duration,
    clock: Arc<dyn FrameClock>,
) -> Result<Box<dyn Replica>, String> {
    let mut world = World::new(seed, BlobStore::default());
    world.set_rate(rate);
    let config = ReplicaConfig::new(rate, cut_interval, clock);
    let replica = ReplicaImpl::new(PeWorld::new(world, DomainId(0)), config).map_err(|e| format!("{e:?}"))?;
    Ok(Box::new(replica))
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
    cut_interval: Duration,
    clock: Arc<dyn FrameClock>,
) -> Result<Box<dyn Replica>, String> {
    ball_pit(preset::ball_pit(seed, BlobStore::default(), Pos::ORIGIN), &[me], me, rate, cut_interval, clock)
}

/// A ball pit `world` with every one of `members` admitted at frame 0 as an admin, in key order so that every machine
/// builds the same state (pe-agree M1's membership is fixed at the line's start), and `me`'s hands marked as the
/// local player's.
pub(crate) fn ball_pit(
    mut world: World,
    members: &[MemberKey],
    me: MemberKey,
    rate: SimRate,
    cut_interval: Duration,
    clock: Arc<dyn FrameClock>,
) -> Result<Box<dyn Replica>, String> {
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
            .admit(preset::m1_line(), *m, perm::ADMIN | perm::EDIT, Frame(0), hand_spawn(i))
            .map_err(|e| format!("admitting member {i}: {e:?}"))?;
    }
    let mut config = ReplicaConfig::new(rate, cut_interval, clock);
    config.local = Some(me);
    let replica = ReplicaImpl::new(PeWorld::new(world, DomainId(0)), config).map_err(|e| format!("{e:?}"))?;
    Ok(Box::new(replica))
}
