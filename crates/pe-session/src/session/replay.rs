//! The offline reference: re-simulating a world from a final log.

use super::*;

// ---- offline reference ---------------------------------------------------------------------------

/// Re-simulate a world from scratch with a complete, final log and effective frames: the
/// single-world reference used to check that sharing a process with other worlds, rollbacks and
/// throttling changed nothing.
pub fn replay(world_id: u32, scene: SceneKind, cfg: SimConfig, members: &[MemberId], log: &[(Entry, u64)], frames: u64) -> BTreeMap<u64, u64> {
    replay_sim(world_id, scene, cfg, members, log, frames).hashes
}

pub fn replay_sim(world_id: u32, scene: SceneKind, cfg: SimConfig, members: &[MemberId], log: &[(Entry, u64)], frames: u64) -> WorldSim {
    replay_sim_with(world_id, scene, cfg, members, &BTreeMap::new(), log, frames)
}

/// As [`replay_sim`], for a world that trades objects with others (R14 certificates).
pub fn replay_sim_with(
    world_id: u32,
    scene: SceneKind,
    cfg: SimConfig,
    members: &[MemberId],
    peers_of: &BTreeMap<u32, Vec<MemberId>>,
    log: &[(Entry, u64)],
    frames: u64,
) -> WorldSim {
    let mut s = WorldSim::new(world_id, u32::MAX, members, scene, cfg, KeyRing::none());
    s.peers_of = peers_of.clone();
    s.run_log(log, frames)
}

/// As [`replay_sim_with`], with `like`'s cross-world config: route list, epoch certificates and
/// whether provenance is checked (P2-5).
pub fn replay_sim_like(like: &WorldSim, scene: SceneKind, members: &[MemberId], log: &[(Entry, u64)], frames: u64) -> WorldSim {
    let mut s = WorldSim::new(like.world_id, u32::MAX, members, scene, like.cfg, KeyRing::none());
    s.peers_of = like.peers_of.clone();
    s.epochs = like.epochs.clone();
    s.provenance = like.provenance;
    s.import_policy = like.import_policy.clone();
    s.hand_owners = like.hand_owners.clone();
    s.run_log(log, frames)
}

impl WorldSim {
    /// Offline reference: load a final log with its effective frames and simulate `frames`.
    pub(super) fn run_log(mut self, log: &[(Entry, u64)], frames: u64) -> WorldSim {
        for (e, eff) in log {
            let id = e.id();
            self.by_author.insert((e.member, e.seq), id);
            self.entries.insert(id, e.clone());
            self.eff.insert(id, *eff);
            self.at_frame.entry(*eff).or_default().insert(id);
        }
        self.conf = u64::MAX / 2;
        while self.frame < frames {
            self.step_frame(true);
        }
        self
    }
}

/// The log this member would hand to [`replay`]: every non-void entry with its effective frame.
pub fn final_log(s: &WorldSim) -> Vec<(Entry, u64)> {
    s.eff.iter().map(|(id, f)| (s.entries[id].clone(), *f)).collect()
}

pub fn digest(h: &BTreeMap<u64, u64>, frames: u64) -> u64 {
    fnv_u64s((0..frames).map(|f| h.get(&f).copied().unwrap_or(0)))
}
