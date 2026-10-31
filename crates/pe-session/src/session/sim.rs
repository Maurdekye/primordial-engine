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
    pub body: BodyId,
    pub global: u64,
    pub kind: u8,
}

pub(super) const BALL_R: f32 = 0.06;
pub const CLAIM_COOLDOWN: u64 = 10;
pub(super) const HAND_R: f32 = 0.09;
/// Dead-reckoning speed cap for a hand without an entry (m/s).
pub(super) const HAND_COAST_VMAX: f32 = 3.0;

pub(super) fn add_shape(world: &mut World, body: BodyId, kind: u8) {
    let mut sd = default_shape_def();
    sd.density = 1.0;
    match kind {
        0 => {
            let h = make_box_hull(0.4, 0.4, 0.4);
            create_hull_shape(world, body, &sd, &h.base);
        }
        1 => {
            let s = Sphere { center: Vec3 { x: 0.0, y: 0.0, z: 0.0 }, radius: 0.45 };
            create_sphere_shape(world, body, &sd, &s);
        }
        2 => {
            let c = Capsule {
                center1: Vec3 { x: -0.3, y: 0.0, z: 0.0 },
                center2: Vec3 { x: 0.3, y: 0.0, z: 0.0 },
                radius: 0.25,
            };
            create_capsule_shape(world, body, &sd, &c);
        }
        3 => {
            sd.density = 0.2;
            sd.base_material.rolling_resistance = 0.2;
            sd.base_material.friction = 0.6;
            let s = Sphere { center: Vec3 { x: 0.0, y: 0.0, z: 0.0 }, radius: BALL_R };
            create_sphere_shape(world, body, &sd, &s);
        }
        _ => {
            let s = Sphere { center: Vec3 { x: 0.0, y: 0.0, z: 0.0 }, radius: HAND_R };
            create_sphere_shape(world, body, &sd, &s);
        }
    }
}

pub(super) fn static_box(world: &mut World, p: Pos, hx: f32, hy: f32, hz: f32) {
    let mut d = default_body_def();
    d.type_ = BodyType::Static;
    d.position = p;
    let b = create_body(world, &d);
    let hull = make_box_hull(hx, hy, hz);
    create_hull_shape(world, b, &default_shape_def(), &hull.base);
}

/// Builds the scene. `world_tag` makes global object ids unique across worlds (R14).
pub fn build_scene(kind: SceneKind, world_tag: u32) -> (World, Vec<Option<Obj>>) {
    let g = |i: usize| ((world_tag as u64) << 32) | i as u64;
    match kind {
        SceneKind::Pile { side, layers } => {
            let s = crate::build_pile_scene(side, layers);
            let objs = s.bodies.iter().enumerate().map(|(i, &b)| Some(Obj { body: b, global: g(i), kind: (i % 3) as u8 })).collect();
            (s.world, objs)
        }
        SceneKind::BallPit { balls, hands } => {
            let mut world = World::new(&crate::engine_world_def());
            static_box(&mut world, Pos { x: 0.0, y: -0.5, z: 0.0 }, 2.0, 0.5, 2.0);
            let (w, h) = (1.2f32, 0.6f32);
            static_box(&mut world, Pos { x: w + 0.1, y: h, z: 0.0 }, 0.1, h, w + 0.2);
            static_box(&mut world, Pos { x: -w - 0.1, y: h, z: 0.0 }, 0.1, h, w + 0.2);
            static_box(&mut world, Pos { x: 0.0, y: h, z: w + 0.1 }, w + 0.2, h, 0.1);
            static_box(&mut world, Pos { x: 0.0, y: h, z: -w - 0.1 }, w + 0.2, h, 0.1);
            let mut objs = Vec::with_capacity(balls + hands);
            let per_row = 16usize;
            let spacing = 2.0 * w / per_row as f32;
            for i in 0..balls {
                let (ix, iz, iy) = (i % per_row, (i / per_row) % per_row, i / (per_row * per_row));
                let jitter = (i as f32 * 0.618_034).fract() * 0.02 - 0.01;
                let mut d = default_body_def();
                d.type_ = BodyType::Dynamic;
                d.linear_damping = 0.1;
                d.angular_damping = 0.3;
                d.position = Pos {
                    x: -w + spacing * (ix as f32 + 0.5) + jitter,
                    y: BALL_R + 0.01 + iy as f32 * (2.0 * BALL_R + 0.01),
                    z: -w + spacing * (iz as f32 + 0.5) - jitter,
                };
                let b = create_body(&mut world, &d);
                add_shape(&mut world, b, 3);
                objs.push(Some(Obj { body: b, global: g(i), kind: 3 }));
            }
            for k in 0..hands {
                let mut d = default_body_def();
                d.type_ = BodyType::Kinematic;
                d.position = Pos { x: -0.8 + 1.6 * k as f32, y: 1.5, z: 0.0 };
                let b = create_body(&mut world, &d);
                add_shape(&mut world, b, 4);
                objs.push(Some(Obj { body: b, global: g(balls + k), kind: 4 }));
            }
            (world, objs)
        }
        SceneKind::Empty => (World::new(&crate::engine_world_def()), Vec::new()),
    }
}

impl WorldSim {
    // ---- snapshots -------------------------------------------------------------------------------

    pub(super) fn save(&mut self) -> Keyframe {
        let t0 = now_us();
        let mut buf = RecBuffer::new();
        serialize_world(&self.world, &mut buf, &mut self.registry);
        if self.registry.entries.len() != self.slot_count {
            self.slots = self.registry.to_slots();
            self.slot_count = self.registry.entries.len();
        }
        self.stats.saves += 1;
        self.stats.save_us += now_us() - t0;
        Keyframe { image: buf.data, t: self.t.clone() }
    }

    pub(super) fn restore(&mut self, k: &Keyframe, frame: u64) {
        let t0 = now_us();
        assert!(deserialize_into_shell(&k.image, &mut self.world, &mut self.slots), "restore failed");
        self.t = k.t.clone();
        self.frame = frame;
        self.stats.restore_us += now_us() - t0;
    }

    /// Bytes of world image the ring holds now (the memory side of the snapshot knob).
    pub fn ring_bytes(&self) -> u64 {
        self.ring.values().map(|k| k.image.len() as u64).sum()
    }

    /// What the ring would hold as one full base image plus block deltas: (full bytes, bytes if
    /// every later image were stored as a delta against the base, bytes if each were stored as a
    /// delta against the previous image), with 64-byte blocks.
    pub fn ring_delta_bytes(&self) -> (u64, u64, u64) {
        let imgs: Vec<&[u8]> = self.ring.values().map(|k| k.image.as_slice()).collect();
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
    pub fn image_hash(&mut self) -> u64 {
        // A check-hash serialization is not a keyframe: keep it out of the snapshot counters.
        let (n, us) = (self.stats.saves, self.stats.save_us);
        let k = self.save();
        (self.stats.saves, self.stats.save_us) = (n, us);
        // Review NEW-7: after a destroy, a world that stepped continuously and one restored from a
        // keyframe hold the same physics but different image bytes. Hash the image of a scratch
        // world restored from this one instead, so both paths hash alike.
        let scratch = self.scratch.get_or_insert_with(|| World::new(&crate::engine_world_def()));
        assert!(deserialize_into_shell(&k.image, scratch, &mut self.slots), "canonical restore failed");
        let mut buf = RecBuffer::new();
        serialize_world(scratch, &mut buf, &mut self.registry);
        fnv_bytes(&buf.data) ^ self.tables_hash().rotate_left(17)
    }

    pub(super) fn tables_hash(&self) -> u64 {
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
        fnv_u64s(v)
    }

    /// Per-frame hash: Box3D body transforms + velocities, plus every engine table.
    pub fn state_hash(&self) -> u64 {
        hash_world_state(&self.world) ^ self.tables_hash().rotate_left(29)
    }

    pub fn obj_state(&self, obj: u32) -> Option<BodyState> {
        self.t.objs.get(obj as usize).copied().flatten().map(|o| BodyState::read(&self.world, o.body))
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
                let Some(o) = self.live(*obj) else { return Outcome::RejectedGone };
                if e.member != LOGIC {
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
                let imp = Vec3 { x: impulse[0], y: impulse[1], z: impulse[2] };
                body_apply_linear_impulse_to_center(&mut self.world, o.body, imp, true);
                if e.member != LOGIC {
                    match self.t.claims.get(obj).copied() {
                        Some(c) if c.member == e.member && c.until > f => {
                            if *grab {
                                self.t.claims.insert(*obj, Claim { member: e.member, until: f + CLAIM_FRAMES });
                            }
                        }
                        _ => {
                            self.t.claims.insert(*obj, Claim { member: e.member, until: f + CLAIM_FRAMES });
                        }
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
                    Pos { x: target_mm[0] as f32 * 0.001, y: target_mm[1] as f32 * 0.001, z: target_mm[2] as f32 * 0.001 }
                } else {
                    Pos { x: 0.0, y: 1.5, z: 0.0 }
                };
                let dt = self.cfg.dt * (*span).max(1) as f32;
                body_set_target_transform(&mut self.world, o.body, Transform { p, q: QUAT_IDENTITY }, dt, true);
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
                    ObjRecord { global: o.global, kind: o.kind, state: BodyState::read(&self.world, o.body), src_world: self.world_id, src_frame: f, via };
                destroy_body(&mut self.world, o.body);
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
                if *transfer != transfer_id(src, *depart) || !cert_ok(&members, src, fact, cert) {
                    return Outcome::RejectedNoProof;
                }
                // Review NEW-21: even a certified record is untrusted input (a bug or a compromised
                // majority in the source world). Refuse non-finite or absurd states.
                let st = record.state;
                let finite = st.0.iter().all(|&b| (b >> 23) & 0xFF != 0xFF);
                let (q, v, w) = (st.rot(), st.lin(), st.ang());
                let qn = q.v.x * q.v.x + q.v.y * q.v.y + q.v.z * q.v.z + q.s * q.s;
                let sane = finite
                    && (0.99..=1.01).contains(&qn)
                    && v.x * v.x + v.y * v.y + v.z * v.z <= MAX_ARRIVAL_SPEED * MAX_ARRIVAL_SPEED
                    && w.x * w.x + w.y * w.y + w.z * w.z <= MAX_ARRIVAL_SPIN * MAX_ARRIVAL_SPIN;
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
                let obj = self.spawn(record, Pos { x: 0.0, y: 2.0, z: 0.0 });
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
                if !cert_ok(&members, esc.dest, fact_verdict(esc.dest, *transfer, *accepted), cert) {
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
    pub(super) fn spawn(&mut self, r: &ObjRecord, at: Pos) -> u32 {
        let mut d = default_body_def();
        d.type_ = BodyType::Dynamic;
        d.position = at;
        d.rotation = r.state.rot();
        d.linear_velocity = r.state.lin();
        d.angular_velocity = r.state.ang();
        let b = create_body(&mut self.world, &d);
        add_shape(&mut self.world, b, r.kind);
        self.t.objs.push(Some(Obj { body: b, global: r.global, kind: r.kind }));
        (self.t.objs.len() - 1) as u32
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
            if !coast {
                body_set_linear_velocity(&mut self.world, o.body, Vec3 { x: 0.0, y: 0.0, z: 0.0 });
            } else {
                // Never coast faster than a hand moves: a jump (a teleport-like target) would
                // otherwise be extrapolated as a 40 m/s flight.
                let v = body_get_linear_velocity(&self.world, o.body);
                let sp = (v.x * v.x + v.y * v.y + v.z * v.z).sqrt();
                if sp > HAND_COAST_VMAX {
                    let k = HAND_COAST_VMAX / sp;
                    body_set_linear_velocity(&mut self.world, o.body, Vec3 { x: v.x * k, y: v.y * k, z: v.z * k });
                }
            }
        }
        let t0 = now_us();
        self.world.step(self.cfg.dt, SUBSTEPS);
        if record {
            self.stats.step_us += now_us() - t0;
            self.stats.steps += 1;
        }
        // User ruling 09:30Z: the touched object takes the carried resulting state. A no-op when
        // this peer is on the author's world line; otherwise it pulls the object onto it.
        for (obj, st) in carry {
            if let Some(o) = self.live(obj) {
                if BodyState::read(&self.world, o.body) != st {
                    st.write(&mut self.world, o.body);
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
                let h = self.image_hash();
                self.check_hashes.insert(f, h);
                // Review L2: a release-build finite check at every check frame. NaN payload bits differ
                // across ISAs, so a non-finite state is a divergence waiting to happen: report it.
                let bad = self.t.objs.iter().flatten().find(|o| {
                    let st = BodyState::read(&self.world, o.body);
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
                        let x = body_get_transform(&self.world, o.body).p;
                        [x.x as f32, x.y as f32, x.z as f32]
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
        self.ring.values().map(|k| k.image.len() + k.t.objs.len() * std::mem::size_of::<Option<Obj>>()).sum()
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
        &self.evicted
    }
}
