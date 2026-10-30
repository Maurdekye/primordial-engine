//! R14 cross-world transfers: facts, attestations, certificates and P2-5 provenance.

use super::*;

// ---- R14 proofs (reviews NEW-3..NEW-6) -------------------------------------------------------------

/// Largest linear speed (m/s) and spin (rad/s) an arriving object may carry (review NEW-21).
pub const MAX_ARRIVAL_SPEED: f32 = 200.0;
pub const MAX_ARRIVAL_SPIN: f32 = 2000.0;
/// Furthest deadline, in frames after its arrival, that a destination accepts (round 3, low):
/// it bounds how long a transfer stays open and its id must be kept. World config in production.
pub const MAX_DEADLINE_FRAMES: u64 = 3600;

/// P25-3: the source's certified departure carried by an `Arrive`, as one export of a fork proof.
/// The spike's certificates have no epochs yet (P2-4): epoch 0.
pub(super) fn arrival_export(src: u32, depart: EntryId, record: &ObjRecord, dest: u32, deadline: u64, chain: u64, cert: &[(MemberId, u64)]) -> Export {
    Export::Departed { src, epoch: 0, depart, record: *record, dest, deadline, chain, cert: cert.to_vec() }
}

/// A transfer's id: derived from the source world and its Depart entry, so nobody can squat it.
pub fn transfer_id(src_world: u32, depart: EntryId) -> u64 {
    splitmix64(((src_world as u64) << 40) ^ depart)
}

/// "World `src` confirmed departure `depart` of `record`, bound for world `dest` by `deadline`
/// (a frame of `dest`), with provenance `chain` (its [`chain_hash`])." Covers every field of the
/// record (review R3-8) and the chain (P2-5), so a relayer can neither strip nor swap the chain.
pub fn fact_departed(src: u32, depart: EntryId, record: &ObjRecord, dest: u32, deadline: u64, chain: u64) -> u64 {
    fnv_u64s([
        1,
        record.via,
        chain,
        src as u64,
        depart,
        record.global,
        record.kind as u64,
        record.state.hash(),
        record.src_world as u64,
        record.src_frame,
        dest as u64,
        deadline,
    ])
}

/// "World `dst` confirmed transfer `transfer` as accepted / rejected."
pub fn fact_verdict(dst: u32, transfer: u64, accepted: bool) -> u64 {
    fnv_u64s([2, dst as u64, transfer, accepted as u64])
}

/// Spike model of member `member` of world `world` signing `fact`. Production: an Ed25519
/// signature by the member's key, checked against the other world's member keys (D46).
pub fn member_sig(world: u32, member: MemberId, fact: u64) -> u64 {
    splitmix64(fact ^ splitmix64((((world as u64) << 32) | member as u64) ^ 0x5157_4E41_5455_5245))
}

/// True if `cert` holds valid signatures over `fact` from a strict majority of `members`.
pub fn cert_ok(members: &[MemberId], world: u32, fact: u64, cert: &[(MemberId, u64)]) -> bool {
    let signers: BTreeSet<MemberId> =
        cert.iter().filter(|(m, sig)| members.contains(m) && *sig == member_sig(world, *m, fact)).map(|(m, _)| *m).collect();
    !members.is_empty() && signers.len() * 2 > members.len()
}

impl WorldSim {
    // ---- R14 attestations ------------------------------------------------------------------------

    /// Sign, as chain entries, the cross-world facts this replica has confirmed: a pending
    /// departure (for the destination) and an arrival's outcome (for the source). A relay needs a
    /// strict majority of them (reviews NEW-3, NEW-4).
    pub fn attest_due(&mut self) -> Vec<SignedEntry> {
        let out = self.attest_due_entries();
        self.signed_all(out)
    }

    fn attest_due_entries(&mut self) -> Vec<Entry> {
        if self.dirty_from.is_some() || !self.members.contains(&self.me) {
            return vec![];
        }
        let mut facts = Vec::new();
        for esc in self.t.escrow.values() {
            if let (EscrowStatus::Pending, Some(rec)) = (esc.status, esc.record) {
                if esc.depart_frame < self.conf {
                    let chain = self.chain_hash_of(rec.global);
                    facts.push(fact_departed(self.world_id, esc.depart, &rec, esc.dest, esc.deadline, chain));
                }
            }
        }
        // P2-5: a committed transfer is the hop the destination's next departure needs. P25-4: each
        // commit is queued once, when its frame is confirmed.
        self.confirm_effects();
        facts.append(&mut self.commits_due);
        for (t, a) in &self.t.arrivals {
            let (frame, accepted) = match a {
                Arrival::Accepted { frame, .. } => (*frame, true),
                Arrival::Rejected { frame, .. } => (*frame, false),
            };
            if frame < self.conf {
                facts.push(fact_verdict(self.world_id, *t, accepted));
            }
        }
        let mut out = Vec::new();
        for fact in facts {
            if self.attested.insert(fact) {
                let e = Entry {
                    member: self.me,
                    seq: self.my_seq + 1,
                    prev: self.my_last,
                    stamp: self.frame.max(self.now),
                    payload: Payload::Attest { fact, sig: member_sig(self.world_id, self.me, fact) },
                    carried: None,
                };
                out.push(self.publish(e));
            }
        }
        out
    }

    /// A certificate for `fact` from this world's members, once a strict majority has signed it.
    pub fn cert(&self, fact: u64) -> Option<Vec<(MemberId, u64)>> {
        let members: Vec<MemberId> = self.members.iter().copied().collect();
        let mut c: BTreeMap<MemberId, u64> = BTreeMap::new();
        for (id, e) in &self.entries {
            if let Payload::Attest { fact: f, sig } = e.payload {
                if f == fact && !self.is_void(*id) && members.contains(&e.member) {
                    c.insert(e.member, sig);
                }
            }
        }
        let c: Vec<(MemberId, u64)> = c.into_iter().collect();
        cert_ok(&members, self.world_id, fact, &c).then_some(c)
    }

    // ---- P2-5 provenance ---------------------------------------------------------------------------

    /// Act on the cross-world effects of frames that are now confirmed and simulated in their final
    /// form: queue their commit facts for attestation, and keep their fork proofs.
    pub(super) fn confirm_effects(&mut self) {
        if self.dirty_from.is_some() {
            return;
        }
        let upto = self.conf.min(self.frame);
        while let Some(e) = self.frame_effects.first_entry() {
            if *e.key() >= upto {
                break;
            }
            let (commits, forks) = e.remove();
            self.commits_due.extend(commits);
            for p in forks {
                let k = p.key();
                if !self.fork_proofs.contains_key(&k) {
                    self.ev(format!(
                        "P2-5 fork proof: world {} exported global id {:#x} twice from its entry {}: transfers {} and {}",
                        k.0, k.1, k.2, k.3, k.4
                    ));
                    self.fork_proofs.insert(k, p);
                }
            }
        }
    }

    /// What this world knows of global id `g`. An object born here that has no entry yet is here,
    /// with an empty chain.
    pub fn seen_of(&self, g: u64) -> Option<Seen> {
        self.t.seen.get(&g).cloned().or_else(|| {
            (birth_world(g) == self.world_id && (g & 0xFFFF_FFFF) < self.genesis_objects)
                .then(|| Seen::Here { via: 0, from: self.world_id, from_via: 0, chain: Vec::new(), chash: chain_hash(&[]), settled: true, entry: None })
        })
    }

    /// The provenance chain of an object here: what its next departure's `Arrive` carries.
    /// The cached hash of [`Self::chain_of`] (what a departure certificate covers).
    pub fn chain_hash_of(&self, g: u64) -> u64 {
        match self.t.seen.get(&g) {
            Some(Seen::Here { chash, .. }) => *chash,
            _ => chain_hash(&self.chain_of(g)),
        }
    }

    pub fn chain_of(&self, g: u64) -> Vec<Hop> {
        match self.seen_of(g) {
            Some(Seen::Here { chain, .. }) => chain,
            _ => Vec::new(),
        }
    }

    /// The hop a departure of local object `obj` must carry: the certified commit that brought it
    /// here, from a `Provenance` entry in this world's log, if this world does not hold it yet.
    pub fn pending_hop(&self, obj: u32) -> Option<Hop> {
        let g = self.live(obj)?.global;
        let here = self.seen_of(g)?;
        if !matches!(here, Seen::Here { settled: false, .. }) {
            return None;
        }
        // Anyone may publish a Provenance entry: take one that proves the hop.
        self.entries.iter().find_map(|(id, e)| match &e.payload {
            Payload::Provenance { hop } if !self.is_void(*id) && self.hop_in_ok(g, &here, hop) => Some(hop.clone()),
            _ => None,
        })
    }

    /// `hop` is the certified commit of the transfer that brought object `g` here.
    pub(super) fn hop_in_ok(&self, g: u64, here: &Seen, hop: &Hop) -> bool {
        let Seen::Here { via, from, from_via, .. } = here else { return false };
        hop.src == *from
            && hop.dest == self.world_id
            && hop.transfer == *via
            && hop.via == *from_via
            && self.epochs.hop_ok(hop, fact_committed(hop.src, hop.transfer, g, hop.dest, hop.via))
    }
}
