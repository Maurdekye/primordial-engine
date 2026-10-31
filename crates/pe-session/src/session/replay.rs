//! The offline reference: re-simulating a world from a final log.

use super::*;

// ---- offline reference ---------------------------------------------------------------------------

/// Re-simulate a world from scratch with a complete, final log and effective frames: the
/// single-world reference used to check that sharing a process with other worlds, rollbacks and
/// throttling changed nothing. `keys` are the members' public keys: certificates in the log are
/// checked against them, as the replicas did (S3c).
pub fn replay(world_id: u32, scene: SceneKind, cfg: SimConfig, keys: &PublicKeys, members: &[MemberId], log: &[(Entry, u64)], frames: u64) -> BTreeMap<u64, u64> {
    replay_sim(world_id, scene, cfg, keys, members, log, frames).hashes
}

pub fn replay_sim(world_id: u32, scene: SceneKind, cfg: SimConfig, keys: &PublicKeys, members: &[MemberId], log: &[(Entry, u64)], frames: u64) -> WorldSim {
    replay_sim_with(world_id, scene, cfg, keys, members, &BTreeMap::new(), log, frames)
}

/// As [`replay_sim`], for a world that trades objects with others (R14 certificates).
/// Its epoch book is empty, so a world whose log holds R14 arrivals needs [`replay_sim_like`]
/// instead (review P23-12), or its arrivals' provenance fails here and not live.
pub fn replay_sim_with(
    world_id: u32,
    scene: SceneKind,
    cfg: SimConfig,
    keys: &PublicKeys,
    members: &[MemberId],
    peers_of: &BTreeMap<u32, Vec<MemberId>>,
    log: &[(Entry, u64)],
    frames: u64,
) -> WorldSim {
    let mut s = WorldSim::new(world_id, u32::MAX, members, scene, cfg, KeyRing::new(None, keys.clone(), [0; 32]));
    s.peers_of = peers_of.clone();
    s.run_log(log, frames)
}

/// As [`replay_sim_with`], with `like`'s cross-world config: route list, epoch certificates and
/// whether provenance is checked (P2-5).
pub fn replay_sim_like(like: &WorldSim, scene: SceneKind, members: &[MemberId], log: &[(Entry, u64)], frames: u64) -> WorldSim {
    let mut s = WorldSim::new(like.world_id, u32::MAX, members, scene, like.cfg, KeyRing::new(None, like.keys.public.clone(), like.keys.session));
    s.peers_of = like.peers_of.clone();
    s.epochs = like.epochs.clone();
    s.provenance = like.provenance;
    s.import_policy = like.import_policy.clone();
    s.hand_owners = like.hand_owners.clone();
    s.run_log(log, frames)
}

impl WorldSim {
    /// The members' public keys this replica checks signatures against.
    pub fn public_keys(&self) -> &PublicKeys {
        &self.keys.public
    }

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

#[cfg(test)]
mod round3_tests {
    //! Review VCR-11 (P2-4 round 3): encodings that must be refused, never read in part.
    use super::*;

    #[test]
    pub(super) fn large_polka_round_trips() {
        // Review VC6R-2: a capped precommit carries up to MAX_POLKA prevote ids. A polka of 300 would
        // have panicked the old encoder (asserted at 64); now it round-trips, and a decode claiming
        // more than MAX_POLKA is refused.
        let polka: Vec<EntryId> = (1..=300).map(EntryId::fake).collect();
        let sig = test_fact_sig(1, 1, vc_fact(0, 0, 9));
        let e = test_sign(1, Entry { member: 1, seq: 1, prev: EntryId::NONE, stamp: 5, payload: Payload::Precommit { epoch: 0, round: 0, value: 9, boundary: 7, polka, sig }, carried: None });
        assert_eq!(decode_msg(&encode_msg(&Msg::Entry(e.clone()))), Some(Msg::Entry(e)));
        // A precommit payload whose polka count exceeds MAX_POLKA is refused (never read in part).
        let mut w = Wr(Vec::new());
        w.u8(1); // Msg::Entry
        w.u32(1); w.u32(1); w.id(EntryId::NONE); w.u64(5); // member, seq, prev, stamp
        w.u8(13); w.u32(0); w.u32(0); w.u64(9); w.u64(7); // Precommit epoch, round, value, boundary
        w.u32(MAX_POLKA as u32 + 1);
        for i in 0..=MAX_POLKA as u64 { w.id(EntryId::fake(i)) }
        w.sig(&[0; 64]);
        w.u8(0); // carried: none
        w.sig(&[0; 64]); // the entry's signature
        assert_eq!(decode_msg(&w.0), None);
    }

    #[test]
    pub(super) fn signed_join_and_branch_fetch_round_trip() {
        // Review VC3R-4 and VC3R-7 (round 4): the join's signature and the fetch's depth travel.
        for m in [
            join_msg(&TEST_SESSION, 5, vec![], 900),
            join_msg(&TEST_SESSION, 5, vec![attest(Rel::Friend, 5, 1, 1)], u64::MAX),
            Msg::FetchEntry { id: EntryId::fake(77), back: 0 },
            Msg::FetchEntry { id: EntryId::fake(77), back: 15 },
        ] {
            assert_eq!(decode_msg(&encode_msg(&m)), Some(m));
        }
        assert_ne!(join_sig(1, 5), join_sig(2, 5));
        assert_ne!(join_sig(1, 5), join_sig(1, 6));
        // Review VC4R-6: another session of the same world, or another expiry, is another signature.
        let (a, b) = (&TEST_SESSION, &[7u8; 32]);
        assert_ne!(join_sig_in(a, 5, 900), join_sig_in(b, 5, 900));
        assert_ne!(join_sig_in(a, 5, 900), join_sig_in(a, 5, 901));
        // Only the joiner's key over this session and expiry verifies.
        let keys = PublicKeys::test();
        assert!(keys.join_ok(5, a, 900, &join_sig_in(a, 5, 900)));
        assert!(!keys.join_ok(5, b, 900, &join_sig_in(a, 5, 900)));
        assert!(!keys.join_ok(5, a, 901, &join_sig_in(a, 5, 900)));
        assert!(!keys.join_ok(6, a, 900, &join_sig_in(a, 5, 900)));
        assert!(!keys.join_ok(5, a, 900, &join_sig(1, 5)));
        // A change that adds a member carries its request, in the extended layout.
        let c = Change { epoch: 2, add: vec![5], add_roles: vec![ADMIN], add_attests: vec![vec![]], add_reqs: vec![(join_sig_in(a, 5, 900), 900)], boundary: 7, ..Default::default() };
        let e = test_sign(1, Entry { member: 1, seq: 1, prev: EntryId::NONE, stamp: 5, payload: Payload::Propose { epoch: 2, round: 0, valid_round: NO_ROUND, change: c }, carried: None });
        assert_eq!(decode_msg(&encode_msg(&Msg::Entry(e.clone()))), Some(Msg::Entry(e)));
    }

    pub(super) fn join_bytes(n: u32) -> Vec<u8> {
        let mut w = Wr(Vec::new());
        w.u8(7);
        w.u32(5);
        w.sig(&join_sig(1, 5));
        w.u64(u64::MAX);
        w.u32(n);
        for _ in 0..n {
            w.u8(Rel::Friend.tag());
            w.u32(1);
            w.sig(&[2; 64]);
            w.sig(&[3; 64]);
        }
        w.0
    }

    #[test]
    pub(super) fn join_with_too_many_attests_is_refused() {
        let ok = decode_msg(&join_bytes(MAX_ATTESTS as u32));
        assert!(matches!(ok, Some(Msg::Join { ref attests, .. }) if attests.len() == MAX_ATTESTS));
        assert_eq!(decode_msg(&join_bytes(MAX_ATTESTS as u32 + 1)), None);
        // Inside a batch, a refused join refuses the batch rather than shifting what follows.
        let mut w = Wr(Vec::new());
        w.u8(4);
        w.u32(2);
        let mut b = w.0;
        b.extend(join_bytes(MAX_ATTESTS as u32 + 1));
        b.extend(encode_msg(&Msg::FetchValue { value: 9 }));
        assert_eq!(decode_msg(&b), None);
    }

    #[test]
    pub(super) fn wide_role_ids_are_refused() {
        let e = Entry { member: 1, seq: 1, prev: EntryId::NONE, stamp: 5, payload: Payload::SetRole { epoch: 0, member: 2, role: 7 }, carried: None };
        let mut bytes = encode_msg(&Msg::Entry(test_sign(1, e.clone())));
        assert_eq!(decode_msg(&bytes), Some(Msg::Entry(test_sign(1, e))));
        // The role is the u32 after tag 18, epoch and member: set its high half.
        let at = bytes.iter().position(|&b| b == 18).unwrap() + 1 + 4 + 4;
        bytes[at + 2] = 1;
        assert_eq!(decode_msg(&bytes), None);
    }

    #[test]
    #[should_panic(expected = "does not fit")]
    pub(super) fn an_epoch_at_the_flag_bit_is_not_encoded() {
        let c = Change { epoch: CHANGE_EXT, ..Default::default() };
        let _ = c.value();
    }

    #[test]
    pub(super) fn change_attests_round_trip() {
        let c = Change { epoch: 3, add: vec![5], add_roles: vec![1], add_attests: vec![vec![attest(Rel::Friend, 5, 1, 1)]], boundary: 9, ..Default::default() };
        let mut w = Wr(Vec::new());
        w.change(&c);
        assert_eq!(Rd::new(&w.0).change(), Some(c));
    }
}
