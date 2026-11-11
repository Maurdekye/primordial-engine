//! The world a machine opens (app.md 4, step 5), on pe-world through pe-replica's [`PeWorld`]. This slice opens a new,
//! empty world from the origin. A preset (the M1 ball pit), an opened save and a joiner's import come later.

use std::sync::Arc;
use std::time::Duration;

use pe_replica::{PeWorld, ReplicaConfig, ReplicaImpl};
use pe_seams::{DomainId, FrameClock, Replica, SimRate};
use pe_world::{BlobStore, World};

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
    let mut config = ReplicaConfig::new(rate, cut_interval);
    config.clock = Some(clock);
    let replica = ReplicaImpl::new(PeWorld::new(world, DomainId(0)), config).map_err(|e| format!("{e:?}"))?;
    Ok(Box::new(replica))
}
