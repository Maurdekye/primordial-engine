//! Scenes, keyframes and state hashes, frame application and stepping, rollback.

use super::*;

// ---- scenes --------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
pub enum SceneKind {
    /// The phase-1 pile: ground plus side*side*layers boxes/spheres/capsules.
    Pile { side: usize, layers: usize },
    /// A walled pit of small balls plus one kinematic hand per listed member.
    BallPit { balls: usize, hands: usize },
    /// No bodies: a log-only world, for membership measurements at scale (P2-4).
    Empty,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Obj {
    /// The object's body in the physics world: [`object_id`] of its table index.
    pub id: ObjectId,
    pub global: u64,
    pub kind: u8,
}

pub(super) const BALL_R: f32 = 0.06;
pub const CLAIM_COOLDOWN: u64 = 10;
pub(super) const HAND_R: f32 = 0.09;
/// Dead-reckoning speed cap for a hand without an entry (m/s).
pub(super) const HAND_COAST_VMAX: f32 = 3.0;

/// The physics id of object-table entry `i` (S2). Scene statics, which are not objects, are
/// creator 1.
pub fn object_id(i: usize) -> ObjectId {
    ObjectId::new(0, i as u32 + 1)
}

fn static_id(k: u32) -> ObjectId {
    ObjectId::new(1, k + 1)
}

/// The shape of an object of `kind` (as the spike's `add_shape`).
pub(super) fn shape_desc(kind: u8) -> ShapeDesc {
    match kind {
        0 => ShapeDesc::new(Geometry::Box { half_extents: [0.4, 0.4, 0.4] }).with_density(1.0),
        1 => ShapeDesc::new(Geometry::Sphere { center: [0.0; 3], radius: 0.45 }).with_density(1.0),
        2 => ShapeDesc::new(Geometry::Capsule { center1: [-0.3, 0.0, 0.0], center2: [0.3, 0.0, 0.0], radius: 0.25 }).with_density(1.0),
        3 => {
            let mut s = ShapeDesc::new(Geometry::Sphere { center: [0.0; 3], radius: BALL_R }).with_density(0.2).with_friction(0.6);
            s.rolling_resistance = 0.2;
            s
        }
        _ => ShapeDesc::new(Geometry::Sphere { center: [0.0; 3], radius: HAND_R }).with_density(1.0),
    }
}

fn static_box(world: &mut PhysicsWorld, k: u32, p: [f32; 3], half_extents: [f32; 3]) {
    let d = BodyDesc::new(BodyKind::Static, p.map(f64::from)).with_shape(ShapeDesc::new(Geometry::Box { half_extents }));
    world.add_body(static_id(k), &d).expect("scene static");
}

/// Builds the scene. `world_tag` makes global object ids unique across worlds (R14). Bodies are
/// created in the spike's order with the spike's settings, so the physics is the same.
pub fn build_scene(kind: SceneKind, world_tag: u32) -> (PhysicsWorld, Vec<Option<Obj>>) {
    let g = |i: usize| ((world_tag as u64) << 32) | i as u64;
    let mut world = PhysicsWorld::new(PhysicsConfig::engine()).expect("engine physics config");
    let mut objs = Vec::new();
    let add = |world: &mut PhysicsWorld, objs: &mut Vec<Option<Obj>>, d: BodyDesc, kind: u8| {
        let i = objs.len();
        world.add_body(object_id(i), &d).expect("scene body");
        objs.push(Some(Obj { id: object_id(i), global: g(i), kind }));
    };
    match kind {
        SceneKind::Pile { side, layers } => {
            static_box(&mut world, 0, [0.0, -1.0, 0.0], [40.0, 1.0, 40.0]);
            for layer in 0..layers {
                for ix in 0..side {
                    for iz in 0..side {
                        let n = objs.len();
                        // Small deterministic jitter so the pile topples (in f32, as the spike).
                        let jitter = (n as f32 * 0.618_034).fract() * 0.2 - 0.1;
                        let p = [(ix as f32 - side as f32 * 0.5) * 1.1 + jitter, 0.5 + layer as f32 * 1.2, (iz as f32 - side as f32 * 0.5) * 1.1 - jitter];
                        let k = (n % 3) as u8;
                        add(&mut world, &mut objs, BodyDesc::new(BodyKind::Dynamic, p.map(f64::from)).with_shape(shape_desc(k)), k);
                    }
                }
            }
        }
        SceneKind::BallPit { balls, hands } => {
            static_box(&mut world, 0, [0.0, -0.5, 0.0], [2.0, 0.5, 2.0]);
            let (w, h) = (1.2f32, 0.6f32);
            static_box(&mut world, 1, [w + 0.1, h, 0.0], [0.1, h, w + 0.2]);
            static_box(&mut world, 2, [-w - 0.1, h, 0.0], [0.1, h, w + 0.2]);
            static_box(&mut world, 3, [0.0, h, w + 0.1], [w + 0.2, h, 0.1]);
            static_box(&mut world, 4, [0.0, h, -w - 0.1], [w + 0.2, h, 0.1]);
            let per_row = 16usize;
            let spacing = 2.0 * w / per_row as f32;
            for i in 0..balls {
                let (ix, iz, iy) = (i % per_row, (i / per_row) % per_row, i / (per_row * per_row));
                let jitter = (i as f32 * 0.618_034).fract() * 0.02 - 0.01;
                let p = [
                    -w + spacing * (ix as f32 + 0.5) + jitter,
                    BALL_R + 0.01 + iy as f32 * (2.0 * BALL_R + 0.01),
                    -w + spacing * (iz as f32 + 0.5) - jitter,
                ];
                let mut d = BodyDesc::new(BodyKind::Dynamic, p.map(f64::from)).with_shape(shape_desc(3));
                d.linear_damping = 0.1;
                d.angular_damping = 0.3;
                add(&mut world, &mut objs, d, 3);
            }
            for k in 0..hands {
                let d = BodyDesc::new(BodyKind::Kinematic, [(-0.8 + 1.6 * k as f32) as f64, 1.5, 0.0]).with_shape(shape_desc(4));
                add(&mut world, &mut objs, d, 4);
            }
        }
        SceneKind::Empty => {}
    }
    (world, objs)
}

impl WorldSim {
    // ---- snapshots -------------------------------------------------------------------------------

    pub(super) fn save(&mut self) -> Keyframe {
        let t0 = now_us();
        let image = self.world.save_image().expect("a usable physics world");
        self.stats.saves += 1;
        self.stats.save_us += now_us() - t0;
        Keyframe { image, t: self.t.clone() }
    }

    pub(super) fn restore(&mut self, k: &Keyframe, frame: u64) {
        let t0 = now_us();
        self.world.restore_image(&k.image).expect("restore failed");
        self.t = k.t.clone();
        self.frame = frame;
        self.stats.restore_us += now_us() - t0;
    }

    /// Bytes of world image the ring holds now (the memory side of the snapshot knob).
    pub fn ring_bytes(&self) -> u64 {
        self.ring.values().map(|k| k.image.world_bytes().len() as u64).sum()
    }

    /// What the ring would hold as one full base image plus block deltas: (full bytes, bytes if
    /// every later image were stored as a delta against the base, bytes if each were stored as a
    /// delta against the previous image), with 64-byte blocks.
    pub fn ring_delta_bytes(&self) -> (u64, u64, u64) {
        let imgs: Vec<&[u8]> = self.ring.values().map(|k| k.image.world_bytes()).collect();
        let Some(base) = imgs.first() else { return (0, 0, 0) };
        let full = imgs.iter().map(|i| i.len() as u64).sum();
        let vs_base = base.len() as u64 + imgs[1..].iter().map(|i| block_delta(base, i, 64)).sum::<u64>();
        let vs_prev = base.len() as u64 + imgs.windows(2).map(|w| block_delta(w[0], w[1], 64)).sum::<u64>();
        (full, vs_base, vs_prev)
    }

    /// The memory/CPU knob (design 6.7): any value, any time. Takes effect at the next keyframe
    /// decision and trim; never changes a result.
    pub fn set_snap(&mut self, p: SnapPolicy) {
        self.cfg.snap = p;
    }

    pub(super) fn wants_keyframe(&self, f: u64) -> bool {
        if self.ring.contains_key(&f) {
            return false;
        }
        if self.ring.is_empty() {
            return true;
        }
        match self.cfg.snap {
            SnapPolicy::Every(k) => f % k == 0,
            // During a re-simulation, skip images the thinning would drop on arrival.
            SnapPolicy::Log(k) => f % k == 0 && f % log_grid(k, self.resim_to.max(self.frame) - f) == 0,
            SnapPolicy::Base(k) => f % k == 0 && f > self.conf && self.ring.range(self.conf + 1..).next().is_none(),
            SnapPolicy::Window(k) => f % k == 0 && self.resim_to.max(self.frame) - f < 4 * k,
        }
    }

    /// Hash of the full world image plus tables: used at confirmed check frames (review H4).
    /// The check hash at this frame (design 9.1, P22R-1): BLAKE3 over pe-physics's full-state check
    /// hash (canonical whichever way the state was reached: review NEW-7, tests/physics_check_hash.rs)
    /// and every engine table.
    pub fn check_hash(&mut self) -> CheckHash {
        let physics = self.world.check_hash().expect("a usable physics world");
        let mut h = blake3::Hasher::new();
        h.update(b"pe-check");
        h.update(&physics.0);
        for w in self.tables_words() {
            h.update(&w.to_le_bytes());
        }
        CheckHash(*h.finalize().as_bytes())
    }

    pub(super) fn tables_hash(&self) -> u64 {
        fnv_u64s(self.tables_words())
    }

    /// Every engine table, canonically, as 64-bit words.
    pub(super) fn tables_words(&self) -> Vec<u64> {
        let mut v = vec![self.frame];
        for (i, o) in self.t.objs.iter().enumerate() {
            if let Some(o) = o {
                v.extend([i as u64, o.global, o.kind as u64]);
            }
        }
        for (o, c) in &self.t.claims {
            v.extend([*o as u64, c.member as u64, c.until]);
        }
        for (o, (m, u)) in &self.t.cooldown {
            v.extend([*o as u64, *m as u64, *u, 7]);
        }
        for (t, e) in &self.t.escrow {
            v.extend([*t, e.obj as u64, e.status as u64, e.depart_frame, e.record.map_or(0, |r| r.state.hash())]);
        }
        for (t, a) in &self.t.arrivals {
            match a {
                Arrival::Accepted { obj, frame } => v.extend([*t, 1, *obj as u64, *frame]),
                Arrival::Rejected { frame, until } => v.extend([*t, 2, *frame, *until]),
            }
        }
        for (g, s) in &self.t.seen {
            s.hash_into(*g, &mut v);
        }
        v.push(self.t.portal_open as u64);
        if self.cfg.hand_predict > 0 {
            for (o, n) in &self.t.hand_idle {
                v.extend([*o as u64, *n, 11]);
            }
        }
        v
    }

    /// Per-frame hash: Box3D body transforms + velocities, plus every engine table.
    pub fn state_hash(&self) -> u64 {
        self.world.box3d_hash().expect("a usable physics world") ^ self.tables_hash().rotate_left(29)
    }

    pub fn obj_state(&self, obj: u32) -> Option<BodyState> {
        self.t.objs.get(obj as usize).copied().flatten().map(|o| BodyState::read(&self.world, o.id))
    }

    // ---- simulation ------------------------------------------------------------------------------

    /// Sorted (entry id, entry) for frame `f`: soonest-wins order with the per-frame tie rank.
    pub(super) fn frame_entries(&self, f: u64) -> Vec<(EntryId, Entry)> {
        let mut v: Vec<(EntryId, Entry)> = self
            .at_frame
            .get(&f)
            .map(|s| s.iter().filter(|id| !self.is_void(**id)).map(|id| (*id, self.entries[id].clone())).collect())
            .unwrap_or_default();
        let seed = self.cfg.seed;
        v.sort_by_key(|(_, e)| (tie_rank(seed, f, e.member), e.member, e.seq));
        v
    }

    pub(super) fn live(&self, obj: u32) -> Option<Obj> {
        self.t.objs.get(obj as usize).copied().flatten()
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn apply(
        &mut self,
        id: EntryId,
        e: &Entry,
        f: u64,
        hands_set: &mut BTreeSet<u32>,
        carry: &mut Vec<(u32, BodyState)>,
        record_effects: bool,
    ) -> Outcome {
        match &e.payload {
            Payload::Touch { obj, impulse, grab } => {
                // Review HO-1: an object a member owns (a hand) is touched by no one else: refused,
                // so no claim is taken that would block the owner.
                if self.not_owner(*obj, e.member) {
                    return Outcome::RejectedNotOwner;
                }
                // Review HO-1, round 2: a touch carries the touched object's resulting state and no
                // other; a carried state for another object would overwrite it unclaimed (a member's
                // hand included).
                if self.fixes_off & fix::HO1 == 0 && e.carried.is_some_and(|(cobj, _)| cobj != *obj) {
                    return Outcome::RejectedInvalid;
                }
                let Some(o) = self.live(*obj) else { return Outcome::RejectedGone };
                // D72 (8.4): only grabs claim, and only a grab can be refused. Every other touch
                // applies, on a held object too; world logic never claims.
                let spike = self.cfg.claim_rule == ClaimRule::Spike;
                let claims = (*grab || spike) && e.member != LOGIC;
                if claims {
                    if let Some(c) = self.t.claims.get(obj) {
                        if c.member != e.member && c.until > f {
                            return Outcome::RejectedClaimed { by: c.member, until: c.until };
                        }
                    }
                    if let Some(&(m, u)) = self.t.cooldown.get(obj) {
                        if m == e.member && u > f && !self.t.claims.contains_key(obj) {
                            return Outcome::RejectedCooldown { until: u };
                        }
                    }
                }
                // pe-physics refuses a non-finite or absurd impulse (limits::MAX_PUSH): invalid.
                if self.world.push(o.id, Push::Impulse(*impulse)).is_err() {
                    return Outcome::RejectedInvalid;
                }
                if claims {
                    // Takes the claim, or renews the holder's: held while the grab keeps renewing it,
                    // it lapses CLAIM_FRAMES after the last renewal, then the holder's cooldown starts.
                    // (The spike rule renewed only on a grab.)
                    let held = self.t.claims.get(obj).is_some_and(|c| c.member == e.member && c.until > f);
                    if *grab || !held {
                        self.t.claims.insert(*obj, Claim { member: e.member, until: f + CLAIM_FRAMES });
                    }
                }
                if let Some((cobj, st)) = e.carried {
                    if self.eff.get(&id) == Some(&e.stamp) {
                        carry.push((cobj, st));
                    }
                }
                Outcome::Accepted
            }
            Payload::Hand { obj, target_mm, active, span } => {
                // A member moves only its own hands; the author is the signed chain's member.
                if self.hand_owners.get(obj) != Some(&e.member) {
                    return Outcome::RejectedNotOwner;
                }
                let Some(o) = self.live(*obj) else { return Outcome::RejectedGone };
                let p = if *active {
                    [target_mm[0] as f32 * 0.001, target_mm[1] as f32 * 0.001, target_mm[2] as f32 * 0.001]
                } else {
                    [0.0, 1.5, 0.0]
                };
                let dt = self.cfg.dt * (*span).max(1) as f32;
                let target = Pose { position: p.map(f64::from), rotation: [0.0, 0.0, 0.0, 1.0] };
                if self.world.set_target(o.id, target, dt, true).is_err() {
                    return Outcome::RejectedInvalid;
                }
                hands_set.insert(*obj);
                if *active {
                    self.t.hand_idle.insert(*obj, 0);
                } else {
                    self.t.hand_idle.remove(obj);
                }
                Outcome::Accepted
            }
            Payload::Depart { obj, dest, deadline, hop } => {
                // Review HO-1: nobody carries another member's hand through a portal.
                if self.not_owner(*obj, e.member) {
                    return Outcome::RejectedNotOwner;
                }
                let transfer = &transfer_id(self.world_id, id);
                if self.t.escrow.contains_key(transfer) {
                    return Outcome::RejectedDuplicate;
                }
                let gone = Escrow {
                    obj: *obj,
                    record: None,
                    status: EscrowStatus::DepartRejected,
                    depart_frame: f,
                    depart: id,
                    dest: *dest,
                    deadline: *deadline,
                };
                // Reviews R3-4, R3-6: objects leave only for worlds on the agreed route list, so no
                // member can send one to a world it controls alone, or to a world nobody knows.
                if !self.peers_of.contains_key(dest) {
                    self.t.escrow.insert(*transfer, gone);
                    return Outcome::RejectedNoRoute;
                }
                let Some(o) = self.live(*obj) else {
                    self.t.escrow.insert(*transfer, gone);
                    return Outcome::RejectedGone;
                };
                if let Some(c) = self.t.claims.get(obj) {
                    if c.member != e.member && c.until > f {
                        self.t.escrow.insert(*transfer, gone);
                        return Outcome::RejectedClaimed { by: c.member, until: c.until };
                    }
                }
                // P2-5: an object that arrived here leaves again only with the certified hop in, so
                // the next destination can check its whole way from birth.
                let mut via = 0;
                if self.provenance {
                    let here = self.seen_of(o.global);
                    match here.clone() {
                        Some(Seen::Here { settled: true, via: v, .. }) => via = v,
                        Some(Seen::Here { settled: false, via: v, from, from_via, mut chain, entry, .. }) => {
                            let Some(h) = hop.as_ref().filter(|h| self.hop_in_ok(o.global, here.as_ref().unwrap(), h)) else {
                                self.t.escrow.insert(*transfer, gone);
                                return Outcome::RejectedUnsettled;
                            };
                            chain.push(h.clone());
                            via = v;
                            let chash = chain_hash(&chain);
                            self.t.seen.insert(o.global, Seen::Here { via, from, from_via, chain, chash, settled: true, entry });
                        }
                        _ => {
                            self.t.escrow.insert(*transfer, gone);
                            return Outcome::RejectedUnsettled;
                        }
                    }
                }
                let record =
                    ObjRecord { global: o.global, kind: o.kind, state: BodyState::read(&self.world, o.id), src_world: self.world_id, src_frame: f, via };
                self.world.remove_body(o.id).expect("a bound body");
                self.t.objs[*obj as usize] = None;
                self.t.claims.remove(obj);
                self.t.escrow.insert(*transfer, Escrow { record: Some(record), status: EscrowStatus::Pending, ..gone });
                Outcome::Accepted
            }
            Payload::Arrive { transfer, record, depart, deadline, cert, chain } => {
                let src = record.src_world;
                // Reviews R3-4, R3-5: only a world on the agreed route list can vouch for an object.
                let Some(members) = self.peers_of.get(&src).cloned() else { return Outcome::RejectedNoRoute };
                let chash = chain_hash(chain);
                let fact = fact_departed(src, *depart, record, self.world_id, *deadline, chash);
                if *transfer != transfer_id(src, *depart) || !cert_ok(&self.keys.public, &members, src, fact, cert) {
                    return Outcome::RejectedNoProof;
                }
                // Review NEW-21: even a certified record is untrusted input (a bug or a compromised
                // majority in the source world). Refuse non-finite or absurd states.
                let st = record.state;
                let finite = st.0.iter().all(|&b| (b >> 23) & 0xFF != 0xFF);
                let (q, v, w) = (st.rot(), st.lin(), st.ang());
                let qn = q[0] * q[0] + q[1] * q[1] + q[2] * q[2] + q[3] * q[3];
                let sane = finite
                    && (0.99..=1.01).contains(&qn)
                    && v[0] * v[0] + v[1] * v[1] + v[2] * v[2] <= MAX_ARRIVAL_SPEED * MAX_ARRIVAL_SPEED
                    && w[0] * w[0] + w[1] * w[1] + w[2] * w[2] <= MAX_ARRIVAL_SPIN * MAX_ARRIVAL_SPIN;
                if !sane {
                    if !self.t.arrivals.contains_key(transfer) {
                        self.t.arrivals.insert(*transfer, Arrival::Rejected { frame: f, until: *deadline });
                    }
                    return Outcome::RejectedInvalid;
                }
                if self.t.arrivals.contains_key(transfer) {
                    return Outcome::RejectedDuplicate;
                }
                // P2-5: the source's majority vouches for the departure, not for how the object got
                // there. Check the chain back to its birth world against each hop's epoch
                // certificates, and that it passes through this world's own last exit of the
                // object. A refusal is certified like any other, and returns the object to the
                // source (whose majority signed this chain).
                if self.provenance {
                    if check_chain(&self.epochs, record.global, src, record.via, chain).is_err() {
                        self.t.arrivals.insert(*transfer, Arrival::Rejected { frame: f, until: *deadline });
                        return Outcome::RejectedNoProvenance;
                    }
                    // P25-1: identity is proven; content and birth are the policy's call.
                    if !self.import_policy.allows(record.global, src, record.kind, chain) {
                        self.t.arrivals.insert(*transfer, Arrival::Rejected { frame: f, until: *deadline });
                        return Outcome::RejectedPolicy;
                    }
                    let known = self.seen_of(record.global);
                    let arrival = arrival_export(src, *depart, record, self.world_id, *deadline, chash, cert);
                    if let Some(p) = known.as_ref().and_then(|k| fork_proof(k, record.global, chain, &arrival)) {
                        if record_effects {
                            self.frame_effects.entry(f).or_default().1.push(p);
                        }
                    }
                    match known {
                        Some(Seen::Here { .. }) => {
                            self.t.arrivals.insert(*transfer, Arrival::Rejected { frame: f, until: *deadline });
                            return Outcome::RejectedLiveId;
                        }
                        Some(Seen::Left { transfer: out, .. }) if !chain.iter().any(|h| h.transfer == out && h.src == self.world_id) => {
                            self.t.arrivals.insert(*transfer, Arrival::Rejected { frame: f, until: *deadline });
                            return Outcome::RejectedFork;
                        }
                        _ => {}
                    }
                }
                // Review R3-6: past the deadline the arrival is refused, and that certified refusal
                // returns the object to the source. Nobody can hold it in limbo by never relaying:
                // anyone may relay late, and late means refused.
                if f > *deadline {
                    self.t.arrivals.insert(*transfer, Arrival::Rejected { frame: f, until: *deadline });
                    return Outcome::RejectedLate;
                }
                // A deadline far ahead would keep the transfer open, and its id in this table, for
                // as long as it likes (round 3, low).
                if *deadline > f.saturating_add(MAX_DEADLINE_FRAMES) {
                    self.t.arrivals.insert(*transfer, Arrival::Rejected { frame: f, until: *deadline });
                    return Outcome::RejectedDeadline;
                }
                if !self.t.portal_open {
                    self.t.arrivals.insert(*transfer, Arrival::Rejected { frame: f, until: *deadline });
                    return Outcome::RejectedPortalClosed;
                }
                // Review R3-5: never a second live copy of one global id in a world.
                if self.t.objs.iter().flatten().any(|o| o.global == record.global) {
                    self.t.arrivals.insert(*transfer, Arrival::Rejected { frame: f, until: *deadline });
                    return Outcome::RejectedLiveId;
                }
                let obj = self.spawn(record, [0.0, 2.0, 0.0]);
                self.t.arrivals.insert(*transfer, Arrival::Accepted { obj, frame: f });
                let entry = Some(Box::new(arrival_export(src, *depart, record, self.world_id, *deadline, chash, cert)));
                let here = Seen::Here { via: *transfer, from: src, from_via: record.via, chain: chain.clone(), chash, settled: false, entry };
                self.t.seen.insert(record.global, here);
                self.watched.insert(obj);
                Outcome::Accepted
            }
            Payload::Verdict { transfer, accepted, cert } => {
                let Some(esc) = self.t.escrow.get(transfer).copied() else { return Outcome::RejectedGone };
                if esc.status != EscrowStatus::Pending {
                    return Outcome::RejectedDuplicate;
                }
                let members = self.peers_of.get(&esc.dest).cloned().unwrap_or_default();
                if !cert_ok(&self.keys.public, &members, esc.dest, fact_verdict(esc.dest, *transfer, *accepted), cert) {
                    return Outcome::RejectedNoProof;
                }
                if *accepted {
                    let rec = esc.record.expect("pending escrow holds its record");
                    let (from, from_via, entry) = match self.seen_of(rec.global) {
                        Some(Seen::Here { from, from_via, entry, .. }) => (from, from_via, entry),
                        _ => (self.world_id, 0, None),
                    };
                    self.t.seen.insert(rec.global, Seen::Left { transfer: *transfer, dest: esc.dest, via: rec.via, frame: f, from, from_via, entry });
                    if record_effects {
                        self.frame_effects.entry(f).or_default().0.push(fact_committed(self.world_id, *transfer, rec.global, esc.dest, rec.via));
                    }
                    self.t.escrow.insert(*transfer, Escrow { status: EscrowStatus::Committed, record: None, ..esc });
                } else {
                    let rec = esc.record.expect("pending escrow holds its record");
                    let back = self.spawn(&rec, rec.state.pos());
                    debug_assert_eq!(back as usize, self.t.objs.len() - 1);
                    self.t.escrow.insert(*transfer, Escrow { status: EscrowStatus::Returned, ..esc });
                }
                Outcome::Accepted
            }
            Payload::Portal { open } => {
                self.t.portal_open = *open;
                Outcome::Accepted
            }
            // Protocol entries never reach the sim (set_eff schedules only perturbations).
            Payload::Ack { .. }
            | Payload::Vote { .. }
            | Payload::Attest { .. }
            | Payload::Provenance { .. }
            | Payload::Propose { .. }
            | Payload::Prevote { .. }
            | Payload::Precommit { .. }
            | Payload::Kick { .. }
            | Payload::SetRole { .. }
            | Payload::DefineRole { .. } => Outcome::Accepted,
        }
    }

    /// Recreate an object from a record at `at`, keeping its rotation and velocities.
    pub(super) fn spawn(&mut self, r: &ObjRecord, at: [f32; 3]) -> u32 {
        let i = self.t.objs.len();
        let d = BodyDesc::new(BodyKind::Dynamic, at.map(f64::from))
            .with_rotation(r.state.rot())
            .with_velocity(r.state.lin(), r.state.ang())
            .with_shape(shape_desc(r.kind));
        // The arrival checks (NEW-21) keep the record within pe-physics's limits.
        self.world.add_body(object_id(i), &d).expect("a checked record");
        self.t.objs.push(Some(Obj { id: object_id(i), global: r.global, kind: r.kind }));
        i as u32
    }

    /// Step one frame: keyframe, entries in canonical order, hands, Box3D step, carried states.
    pub(super) fn step_frame(&mut self, record: bool) {
        let f = self.frame;
        if record {
            // A re-simulation of this frame replaces its cross-world effects (P25-3, P25-4).
            self.frame_effects.remove(&f);
        }
        if record && self.wants_keyframe(f) {
            let k = self.save();
            self.ring.insert(f, k);
            self.stats.max_ring_bytes = self.stats.max_ring_bytes.max(self.ring_bytes());
        }
        // Round 3 (low): forget transfers that can no longer change. A rejected arrival goes once
        // its deadline has passed (any replay is then late, so rejected again, consistently) and
        // MAX_DEADLINE_FRAMES after it arrived (by then it is confirmed and attested: the sim never
        // runs more than the speculation limit past the confirmed frame). A resolved escrow goes
        // MAX_DEADLINE_FRAMES after its departure: a replayed verdict then finds nothing and does
        // nothing. Accepted arrivals stay: forgetting one would let a late replay of its Arrive be
        // certified "rejected" and refund an object that did arrive.
        self.t.arrivals
            .retain(|_, a| !matches!(a, Arrival::Rejected { frame, until } if *until < f && frame.saturating_add(MAX_DEADLINE_FRAMES) < f));
        self.t.escrow.retain(|_, e| e.status == EscrowStatus::Pending || e.depart_frame.saturating_add(MAX_DEADLINE_FRAMES) >= f);
        let mut hands_set = BTreeSet::new();
        let mut carry = Vec::new();
        for (id, e) in self.frame_entries(f) {
            let o = self.apply(id, &e, f, &mut hands_set, &mut carry, record);
            if record {
                self.outcomes.insert(id, (f, o));
            }
        }
        // Hands without a target this frame hold still, or, with `hand_predict`, an active hand
        // keeps its last commanded velocity for up to that many frames (dead reckoning). Both are
        // deterministic rules of the sim: a speculative frame that lacks the author's entry
        // predicts with them, and the entry's arrival replaces the prediction by rollback.
        let hands: Vec<(u32, Obj)> =
            self.t.objs.iter().enumerate().filter_map(|(i, o)| o.filter(|o| o.kind == 4).map(|o| (i as u32, o))).collect();
        for (i, o) in hands {
            if hands_set.contains(&i) {
                continue;
            }
            let coast = match self.t.hand_idle.get_mut(&i) {
                Some(n) if self.cfg.hand_predict > 0 => {
                    *n += 1;
                    *n <= self.cfg.hand_predict
                }
                _ => false,
            };
            let (v, w) = self.world.velocity(o.id).expect("a bound body");
            if !coast {
                self.world.set_velocity(o.id, [0.0; 3], w).expect("a bound hand");
            } else {
                // Never coast faster than a hand moves: a jump (a teleport-like target) would
                // otherwise be extrapolated as a 40 m/s flight.
                let sp = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
                if sp > HAND_COAST_VMAX {
                    let k = HAND_COAST_VMAX / sp;
                    self.world.set_velocity(o.id, v.map(|c| c * k), w).expect("a bound hand");
                }
            }
        }
        let t0 = now_us();
        self.world.step(self.cfg.dt).expect("a usable physics world");
        if record {
            self.stats.step_us += now_us() - t0;
            self.stats.steps += 1;
        }
        // User ruling 09:30Z: the touched object takes the carried resulting state. A no-op when
        // this peer is on the author's world line; otherwise it pulls the object onto it.
        for (obj, st) in carry {
            if let Some(o) = self.live(obj) {
                if BodyState::read(&self.world, o.id) != st && st.write(&mut self.world, o.id).is_ok() {
                    if record {
                        self.stats.overwrites += 1;
                    }
                }
            }
        }
        // Expire claims; their holders enter a short cooldown on that object.
        let next = f + 1;
        let expired: Vec<(u32, MemberId)> = self.t.claims.iter().filter(|(_, c)| c.until <= next).map(|(o, c)| (*o, c.member)).collect();
        for (o, m) in expired {
            self.t.claims.remove(&o);
            self.t.cooldown.insert(o, (m, next + CLAIM_COOLDOWN));
        }
        self.t.cooldown.retain(|_, (_, u)| *u > next);
        self.frame = next;
        if record {
            self.hashes.insert(f, self.state_hash());
            for &obj in &self.watched.clone() {
                if let Some(s) = self.obj_state(obj) {
                    self.obj_hist.insert((f, obj), s.hash());
                }
            }
            if f % self.cfg.check_every == 0 {
                let h = self.check_hash();
                self.check_hashes.insert(f, h);
                // Review L2: a release-build finite check at every check frame. NaN payload bits differ
                // across ISAs, so a non-finite state is a divergence waiting to happen: report it.
                let bad = self.t.objs.iter().flatten().find(|o| {
                    let st = BodyState::read(&self.world, o.id);
                    st.0.iter().any(|&b| (b >> 23) & 0xFF == 0xFF)
                });
                if let Some(o) = bad {
                    self.stats.nonfinite += 1;
                    let g = o.global;
                    self.ev(format!("NON-FINITE body state at frame {f} (object global id {g:#x})"));
                }
            }
            if self.track {
                let p: Vec<[f32; 3]> = self
                    .t
                    .objs
                    .iter()
                    .map(|o| o.map_or([f32::NAN; 3], |o| {
                        self.world.pose(o.id).expect("a bound body").position.map(|c| c as f32)
                    }))
                    .collect();
                self.pos_hist.insert(f, p);
            }
        }
    }

    /// Roll back if needed, then simulate forward to `target` (exclusive) unless the speculation
    /// limit is reached. Returns the number of new frames stepped.
    pub fn advance(&mut self, target: u64) -> u64 {
        let t_adv = now_us();
        let n = self.advance_inner(target);
        let us = now_us() - t_adv;
        self.stats.advance_us += us;
        self.stats.max_advance_us = self.stats.max_advance_us.max(us);
        n
    }

    pub(super) fn advance_inner(&mut self, target: u64) -> u64 {
        if let Some(from) = self.dirty_from.take() {
            let now = self.frame;
            self.resim_to = now;
            let (&kf, _) = self.ring.range(..=from).next_back().expect("keyframe at or before rollback frame");
            let k = self.ring[&kf].clone();
            for f in self.ring.range(kf + 1..).map(|(f, _)| *f).collect::<Vec<_>>() {
                self.ring.remove(&f);
            }
            self.restore(&k, kf);
            self.stats.rollbacks += 1;
            self.stats.max_depth = self.stats.max_depth.max(now - from);
            let t0 = now_us();
            while self.frame < now {
                self.step_frame(true);
                self.stats.resim_frames += 1;
            }
            self.stats.resim_us += now_us() - t0;
            self.evaluate_checks();
        }
        self.trim_ring();
        let mut n = 0;
        while self.frame < target {
            if self.frame >= self.conf + self.cfg.window {
                self.stats.paused_frames += 1;
                break;
            }
            self.step_frame(true);
            n += 1;
        }
        n
    }

    /// Drop keyframes this member can never need again: everything before the newest keyframe at
    /// or before min(confirmed frame, simulated frame), once no re-simulation is pending.
    pub(super) fn trim_ring(&mut self) {
        if self.dirty_from.is_some() {
            return;
        }
        let fin = self.conf.min(self.frame);
        let base = match self.ring.range(..=fin).next_back() {
            Some((&kf, _)) => kf,
            None => return,
        };
        let now = self.frame;
        let drop: Vec<u64> = self
            .ring
            .keys()
            .copied()
            .filter(|&f| {
                f < base
                    || (f > base
                        && match self.cfg.snap {
                            SnapPolicy::Every(k) => f % k != 0,
                            SnapPolicy::Log(k) => f % log_grid(k, now - f) != 0,
                            // Keep only the first candidate after the confirmed frame.
                            SnapPolicy::Base(_) => self.ring.range(base + 1..f).next().is_some(),
                            SnapPolicy::Window(k) => f % k != 0 || now - f >= 4 * k,
                        })
            })
            .collect();
        for f in drop {
            self.ring.remove(&f);
        }
    }

    pub fn ring_len(&self) -> usize {
        self.ring.len()
    }

    /// Bytes of rollback snapshot data held: every keyframe's world image plus its object table
    /// (ballpit-viz; `ring_bytes` counts the world images only).
    pub fn ring_bytes_with_objs(&self) -> usize {
        self.ring.values().map(|k| k.image.world_bytes().len() + k.t.objs.len() * std::mem::size_of::<Option<Obj>>()).sum()
    }

    pub fn caught_up(&self) -> bool {
        self.dirty_from.is_none()
    }

    /// Entries this member holds whose effective frame is confirmed (R14 relays wait for this).
    pub fn is_confirmed(&self, id: EntryId) -> bool {
        self.eff.get(&id).is_some_and(|&f| f < self.conf)
    }

    pub fn all_entries(&self) -> Vec<Entry> {
        self.entries.values().cloned().collect()
    }

    pub fn void_ids(&self) -> BTreeSet<EntryId> {
        self.violations.union(&self.disputed).copied().collect()
    }

    pub fn members_evicted(&self) -> &BTreeMap<MemberId, (u32, u64)> {
        self.membership().removed()
    }
}
