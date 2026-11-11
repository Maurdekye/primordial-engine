//! A small world model for pe-replica's tests and the harness, until pe-world exists: objects that are spheres or
//! boxes, created, removed, teleported and pushed by edit entries in a tiny test codec. It implements
//! [`WorldModel`] the way replica.md 8.1 asks of pe-world: every entry is all or nothing, refused entries change
//! nothing, and the root covers every row, the physics rows and the frame.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use pe_seams::{
    Class, DomainId, Frame, FrameClock, Hash, Hasher, HostTime, ImportError, LeafKey, Mismatch, Node, ObjectId, Offset,
    Outcome, Pos, Reason, Record, SignedEntry, SimRate, Tree, TreePath,
};

use crate::world::{
    BodyKind, BodyRow, BodyStateRow, Change, ClaimRow, Effects, HandRow, PhysicsSettings, PushFx, ShapePart, ShapeRow,
    WorldModel,
};

/// Farthest a body may be from the origin, in metres: pe-physics's `MAX_POSITION`.
const RANGE: f64 = 1.0e6;

/// One op of the test codec. A payload is a sequence of ops, applied all or nothing.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum MockOp {
    /// Create a physical object.
    Spawn {
        id: ObjectId,
        kind: BodyKind,
        shape: ShapeRow,
        pos: [f64; 3],
    },
    Remove {
        id: ObjectId,
    },
    /// Set a body's position (a teleport).
    Teleport {
        id: ObjectId,
        pos: [f64; 3],
    },
    /// An impulse at the centre of mass.
    Push {
        id: ObjectId,
        impulse: [f32; 3],
    },
}

/// Encode ops as an entry payload.
pub fn encode(ops: &[MockOp]) -> Vec<u8> {
    let mut out = Vec::new();
    for op in ops {
        match *op {
            MockOp::Spawn { id, kind, shape, pos } => {
                out.push(1);
                out.extend_from_slice(&id.to_u128().to_le_bytes());
                out.push(kind as u8);
                match shape {
                    ShapeRow::Sphere { radius, .. } => {
                        out.push(0);
                        out.extend_from_slice(&radius.to_le_bytes());
                    }
                    ShapeRow::Box { half_extents, .. } => {
                        out.push(1);
                        for h in half_extents {
                            out.extend_from_slice(&h.to_le_bytes());
                        }
                    }
                    ShapeRow::Capsule { center1, center2, radius } => {
                        out.push(2);
                        for v in center1.iter().chain(&center2).chain([&radius]) {
                            out.extend_from_slice(&v.to_le_bytes());
                        }
                    }
                }
                for p in pos {
                    out.extend_from_slice(&p.to_le_bytes());
                }
            }
            MockOp::Remove { id } => {
                out.push(2);
                out.extend_from_slice(&id.to_u128().to_le_bytes());
            }
            MockOp::Teleport { id, pos } => {
                out.push(3);
                out.extend_from_slice(&id.to_u128().to_le_bytes());
                for p in pos {
                    out.extend_from_slice(&p.to_le_bytes());
                }
            }
            MockOp::Push { id, impulse } => {
                out.push(4);
                out.extend_from_slice(&id.to_u128().to_le_bytes());
                for i in impulse {
                    out.extend_from_slice(&i.to_le_bytes());
                }
            }
        }
    }
    out
}

struct Reader<'a>(&'a [u8]);

impl Reader<'_> {
    fn take<const N: usize>(&mut self) -> Option<[u8; N]> {
        let (head, rest) = self.0.split_first_chunk::<N>()?;
        self.0 = rest;
        Some(*head)
    }
    fn u8(&mut self) -> Option<u8> {
        self.take::<1>().map(|b| b[0])
    }
    fn id(&mut self) -> Option<ObjectId> {
        self.take::<16>().map(|b| ObjectId::from_u128(u128::from_le_bytes(b)))
    }
    fn f32(&mut self) -> Option<f32> {
        self.take::<4>().map(f32::from_le_bytes).filter(|v| v.is_finite())
    }
    fn f64(&mut self) -> Option<f64> {
        self.take::<8>().map(f64::from_le_bytes).filter(|v| v.is_finite())
    }
    fn pos(&mut self) -> Option<[f64; 3]> {
        Some([self.f64()?, self.f64()?, self.f64()?])
    }
    fn vec3(&mut self) -> Option<[f32; 3]> {
        Some([self.f32()?, self.f32()?, self.f32()?])
    }
}

/// Decode a payload; `None` if it is not a canonical sequence of ops.
pub fn decode(bytes: &[u8]) -> Option<Vec<MockOp>> {
    let mut r = Reader(bytes);
    let mut ops = Vec::new();
    while !r.0.is_empty() {
        let op = match r.u8()? {
            1 => {
                let id = r.id()?;
                let kind = match r.u8()? {
                    0 => BodyKind::Static,
                    1 => BodyKind::Kinematic,
                    2 => BodyKind::Dynamic,
                    _ => return None,
                };
                let shape = match r.u8()? {
                    0 => ShapeRow::Sphere { center: [0.0; 3], radius: r.f32()?.max(0.0) },
                    1 => ShapeRow::Box { center: [0.0; 3], rotation: [0.0, 0.0, 0.0, 1.0], half_extents: r.vec3()? },
                    2 => ShapeRow::Capsule { center1: r.vec3()?, center2: r.vec3()?, radius: r.f32()?.max(0.0) },
                    _ => return None,
                };
                MockOp::Spawn { id, kind, shape, pos: r.pos()? }
            }
            2 => MockOp::Remove { id: r.id()? },
            3 => MockOp::Teleport { id: r.id()?, pos: r.pos()? },
            4 => MockOp::Push { id: r.id()?, impulse: r.vec3()? },
            _ => return None,
        };
        ops.push(op);
    }
    Some(ops)
}

fn at(p: [f64; 3]) -> Option<Pos> {
    if p.iter().any(|c| c.abs() > RANGE) {
        return None;
    }
    Pos::ORIGIN.checked_add(Offset(p)).ok()
}

fn body(kind: BodyKind, shape: ShapeRow, pos: Pos) -> BodyRow {
    BodyRow {
        kind,
        pos,
        rotation: [0.0, 0.0, 0.0, 1.0],
        shapes: vec![ShapePart { shape, density: 1000.0 }],
        friction: 0.6,
        restitution: 0.0,
        rolling_resistance: 0.0,
        linear_damping: 0.0,
        angular_damping: 0.0,
        gravity_scale: 1.0,
        bullet: false,
        may_sleep: true,
        grabbable: true,
    }
}

/// A frame clock for tests: frame `f` starts `f / rate` seconds after `start`, rounded up to the nanosecond.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MockClock {
    pub rate: SimRate,
    pub start: HostTime,
}

impl MockClock {
    /// Frame 0 at host time 0.
    pub fn new(rate: SimRate) -> MockClock {
        MockClock { rate, start: HostTime(0) }
    }
}

impl FrameClock for MockClock {
    fn rate_hz(&self) -> u32 {
        self.rate.hz()
    }

    fn frame_at(&self, now: HostTime) -> Option<Frame> {
        let since = i128::from(now.0) - i128::from(self.start.0);
        let frame = since * i128::from(self.rate.hz()) / 1_000_000_000;
        (since >= 0).then(|| Frame(u64::try_from(frame).unwrap_or(u64::MAX)))
    }

    fn host_time_of(&self, frame: Frame) -> HostTime {
        let hz = i128::from(self.rate.hz());
        let nanos = (i128::from(frame.0) * 1_000_000_000 + hz - 1) / hz;
        self.start.plus_nanos(i64::try_from(nanos).unwrap_or(i64::MAX))
    }
}

/// The mock world. Cloning it is its save.
#[derive(Clone, Debug)]
pub struct MockWorld {
    domain: DomainId,
    settings: PhysicsSettings,
    /// The next frame to step: `end_frame(f)` makes it `f + 1`.
    frame: Frame,
    bodies: BTreeMap<ObjectId, BodyRow>,
    physics: BTreeMap<ObjectId, BodyStateRow>,
    global: Hash,
    /// A one-shot fault for repair tests, shared by every save of this world, so restoring a snapshot does not arm
    /// it again: at the end of the frame it names, the lowest static body's row moves by a millimetre.
    glitch: Arc<Mutex<Option<Frame>>>,
    /// A frame whose every step reports one repaired transform (a fault, 3.1), for the fault counts' tests.
    transform_fault: Option<Frame>,
}

impl MockWorld {
    pub fn new(domain: DomainId, rate: SimRate) -> MockWorld {
        MockWorld {
            domain,
            settings: PhysicsSettings { rate, gravity: [0.0, -10.0, 0.0], origin: Pos::ORIGIN },
            frame: Frame(0),
            bodies: BTreeMap::new(),
            physics: BTreeMap::new(),
            global: Hash::ZERO,
            glitch: Arc::new(Mutex::new(None)),
            transform_fault: None,
        }
    }

    /// Every step of `frame`, live or re-stepped, reports one repaired transform: a fault each time it is stepped.
    pub fn fault_transforms_at(&mut self, frame: Frame) {
        self.transform_fault = Some(frame);
    }

    /// Arm the one-shot fault at the end of `frame`: a transient corruption, as from a bad memory bit, that a replay
    /// does not repeat.
    pub fn glitch_at(&self, frame: Frame) {
        *self.glitch.lock().unwrap_or_else(|e| e.into_inner()) = Some(frame);
    }

    /// A body's position, as plain numbers from the origin.
    pub fn position(&self, id: ObjectId) -> Option<[f64; 3]> {
        self.bodies.get(&id).map(|b| (b.pos - Pos::ORIGIN).0)
    }

    /// Every leaf the mock's root covers, for an export of the whole state: its objects, then its `physics_global`.
    pub fn leaves(&self) -> Vec<LeafKey> {
        self.bodies.keys().map(|&id| LeafKey::Object(id)).chain([LeafKey::Physics(self.domain)]).collect()
    }

    fn object_hash(id: ObjectId, b: &BodyRow, phys: Option<&BodyStateRow>) -> Hash {
        let mut h = Hasher::new("pe-replica.mock.object");
        h.write_u128(id.to_u128());
        h.write_u8(b.kind as u8);
        for c in (b.pos - Pos::ORIGIN).0 {
            h.write_u64(c.to_bits());
        }
        for q in b.rotation {
            h.write_u32(q.to_bits());
        }
        if let Some(p) = phys {
            for v in p.linear.iter().chain(&p.angular) {
                h.write_u32(v.to_bits());
            }
            h.write_u8(p.awake as u8);
            h.write_u32(p.sleep_time.to_bits());
            h.write(&p.digest);
        }
        h.finish()
    }

    fn apply_ops(
        &mut self,
        ops: &[MockOp],
        effects: &mut Effects,
        created: &mut Vec<ObjectId>,
    ) -> Result<(), (u32, Reason)> {
        let mut changed: BTreeMap<ObjectId, Change> = BTreeMap::new();
        for (i, op) in ops.iter().enumerate() {
            let i = i as u32;
            match *op {
                MockOp::Spawn { id, kind, shape, pos } => {
                    if self.bodies.contains_key(&id) {
                        return Err((i, Reason::Exists(id)));
                    }
                    let pos = at(pos).ok_or((i, Reason::OutOfRange))?;
                    self.bodies.insert(id, body(kind, shape, pos));
                    changed.insert(id, Change::Created);
                    created.push(id);
                }
                MockOp::Remove { id } => {
                    if self.bodies.remove(&id).is_none() {
                        return Err((i, Reason::Unknown(id)));
                    }
                    self.physics.remove(&id);
                    if changed.get(&id) == Some(&Change::Created) {
                        changed.remove(&id);
                    } else {
                        changed.insert(id, Change::Removed);
                    }
                }
                MockOp::Teleport { id, pos } => {
                    let pos = at(pos).ok_or((i, Reason::OutOfRange))?;
                    let b = self.bodies.get_mut(&id).ok_or((i, Reason::Unknown(id)))?;
                    b.pos = pos;
                    if changed.get(&id) != Some(&Change::Created) {
                        changed.insert(id, Change::Teleported);
                    }
                }
                MockOp::Push { id, impulse } => {
                    if !self.bodies.contains_key(&id) {
                        return Err((i, Reason::Unknown(id)));
                    }
                    effects.pushes.push(PushFx { object: id, impulse, at: None });
                }
            }
        }
        effects.changed = changed.into_iter().collect();
        Ok(())
    }
}

impl WorldModel for MockWorld {
    type Saved = MockWorld;

    fn domain(&self) -> DomainId {
        self.domain
    }

    fn apply(&mut self, _frame: Frame, entry: &SignedEntry) -> (Outcome, Effects) {
        let applied = |created| Outcome::Applied { created, overwrote: Vec::new(), notes: Vec::new(), inverse: None };
        if !matches!(entry.entry.class, Class::Edit | Class::Input) {
            return (applied(Vec::new()), Effects::default());
        }
        let Some(ops) = decode(&entry.entry.payload) else {
            return (Outcome::Refused { op: None, reason: Reason::Malformed }, Effects::default());
        };
        let mut next = self.clone();
        let (mut effects, mut created) = (Effects::default(), Vec::new());
        match next.apply_ops(&ops, &mut effects, &mut created) {
            Ok(()) => {
                *self = next;
                (applied(created), effects)
            }
            Err((op, reason)) => (Outcome::Refused { op: Some(op), reason }, Effects::default()),
        }
    }

    fn end_frame(&mut self, frame: Frame) {
        self.frame = Frame(frame.0.saturating_add(1));
        let fire = {
            let mut armed = self.glitch.lock().unwrap_or_else(|e| e.into_inner());
            (*armed == Some(frame)).then(|| armed.take()).is_some()
        };
        if let Some(b) = fire.then(|| self.bodies.values_mut().find(|b| b.kind == BodyKind::Static)).flatten() {
            b.pos = b.pos.checked_add(Offset([0.001, 0.0, 0.0])).unwrap_or(b.pos);
        }
    }

    fn body(&self, id: ObjectId) -> Option<BodyRow> {
        self.bodies.get(&id).cloned()
    }

    fn physical(&self) -> Vec<ObjectId> {
        self.bodies.keys().copied().collect()
    }

    fn hands(&self) -> Vec<HandRow> {
        Vec::new()
    }

    fn claims(&self) -> Vec<ClaimRow> {
        Vec::new()
    }

    fn settings(&self) -> PhysicsSettings {
        self.settings
    }

    fn write_poses(&mut self, poses: &[(ObjectId, Pos, [f32; 4])]) -> Vec<ObjectId> {
        let mut refused = Vec::new();
        for &(id, pos, rotation) in poses {
            let p = (pos - Pos::ORIGIN).0;
            if p.iter().any(|c| !c.is_finite() || c.abs() > RANGE) {
                refused.push(id);
            } else if let Some(b) = self.bodies.get_mut(&id) {
                b.pos = pos;
                b.rotation = rotation;
            }
        }
        refused
    }

    fn maintain_transforms(&mut self) -> u32 {
        // Called before `end_frame`, so `frame` is the frame being stepped.
        u32::from(self.transform_fault == Some(self.frame))
    }

    fn write_physics(&mut self, rows: &[(ObjectId, BodyStateRow)], global: Hash) {
        self.physics = rows.iter().copied().collect();
        self.global = global;
    }

    fn root(&self) -> Hash {
        let mut h = Hasher::new("pe-replica.mock.root");
        h.write_u64(self.domain.0);
        h.write_u64(self.frame.0);
        for (&id, b) in &self.bodies {
            h.write_hash(&Self::object_hash(id, b, self.physics.get(&id)));
        }
        h.write_hash(&self.global);
        h.finish()
    }

    fn save(&self) -> MockWorld {
        self.clone()
    }

    fn restore(&mut self, saved: &MockWorld) {
        *self = saved.clone();
    }

    fn tree<'a>(&'a self, saved: &'a MockWorld) -> Box<dyn Tree + 'a> {
        Box::new(MockTree(saved))
    }

    /// No objects and no physics rows. The mock keeps its settings, which its root does not cover.
    fn empty(&self) -> MockWorld {
        MockWorld {
            frame: Frame(0),
            bodies: BTreeMap::new(),
            physics: BTreeMap::new(),
            global: Hash::ZERO,
            ..self.clone()
        }
    }

    /// The mock has no inner nodes, so it checks no proofs. A record that does not decode, or names another domain or
    /// a table, is `BadProof`. Since it cannot check records one by one, it checks the patched root against
    /// `target_root` at the end: a wrong or missing record shows there as `RootMismatch` (chunks-reviewer's L2 on
    /// 48e4e521). Every mock object is a bound body, so a record or an absent key for one needs the physics image.
    fn import(&self, saved: &MockWorld, import: &pe_seams::CutImport) -> Result<MockWorld, ImportError> {
        let mut next = saved.clone();
        for key in &import.absent {
            if let LeafKey::Object(id) = *key {
                if import.physics.is_none() {
                    return Err(ImportError::NeedsPhysicsImage(id));
                }
                next.bodies.remove(&id);
                next.physics.remove(&id);
            }
        }
        for (i, record) in import.records.iter().enumerate() {
            match record.key {
                LeafKey::Object(id) => {
                    let (b, phys) = decode_object(&record.bytes).ok_or(ImportError::BadProof(i))?;
                    if import.physics.is_none() {
                        return Err(ImportError::NeedsPhysicsImage(id));
                    }
                    next.bodies.insert(id, b);
                    match phys {
                        Some(p) => next.physics.insert(id, p),
                        None => next.physics.remove(&id),
                    };
                }
                LeafKey::Physics(d) if d == self.domain => {
                    let global = <[u8; 32]>::try_from(&record.bytes[..]).map_err(|_| ImportError::BadProof(i))?;
                    next.global = Hash(global);
                }
                _ => return Err(ImportError::BadProof(i)),
            }
        }
        if let Some(image) = &import.physics
            && (image.domain != self.domain || image.global != next.global)
        {
            return Err(ImportError::PhysicsCheck);
        }
        next.frame = import.cut.at.frame;
        let got = next.root();
        if got != import.target_root {
            return Err(ImportError::RootMismatch { got });
        }
        Ok(next)
    }
}

// ---- the mock's records ----------------------------------------------------------------------------------------------

/// An object's record bytes: its whole row and, at a cut, its `BodyState` row.
fn encode_object(b: &BodyRow, phys: Option<&BodyStateRow>) -> Vec<u8> {
    let mut out = vec![b.kind as u8];
    let f32s = |out: &mut Vec<u8>, vs: &[f32]| vs.iter().for_each(|v| out.extend_from_slice(&v.to_le_bytes()));
    for c in (b.pos - Pos::ORIGIN).0 {
        out.extend_from_slice(&c.to_le_bytes());
    }
    f32s(&mut out, &b.rotation);
    out.push(u8::try_from(b.shapes.len()).expect("a mock body has few shapes"));
    for part in &b.shapes {
        match part.shape {
            ShapeRow::Sphere { center, radius } => {
                out.push(0);
                f32s(&mut out, &center);
                f32s(&mut out, &[radius]);
            }
            ShapeRow::Capsule { center1, center2, radius } => {
                out.push(1);
                f32s(&mut out, &center1);
                f32s(&mut out, &center2);
                f32s(&mut out, &[radius]);
            }
            ShapeRow::Box { center, rotation, half_extents } => {
                out.push(2);
                f32s(&mut out, &center);
                f32s(&mut out, &rotation);
                f32s(&mut out, &half_extents);
            }
        }
        f32s(&mut out, &[part.density]);
    }
    let materials =
        [b.friction, b.restitution, b.rolling_resistance, b.linear_damping, b.angular_damping, b.gravity_scale];
    f32s(&mut out, &materials);
    out.extend([b.bullet as u8, b.may_sleep as u8, b.grabbable as u8]);
    match phys {
        None => out.push(0),
        Some(p) => {
            out.push(1);
            f32s(&mut out, &p.linear);
            f32s(&mut out, &p.angular);
            out.push(p.awake as u8);
            f32s(&mut out, &[p.sleep_time]);
            out.extend_from_slice(&p.digest);
        }
    }
    out
}

fn decode_object(bytes: &[u8]) -> Option<(BodyRow, Option<BodyStateRow>)> {
    let mut r = Reader(bytes);
    let flag = |r: &mut Reader| match r.u8()? {
        0 => Some(false),
        1 => Some(true),
        _ => None,
    };
    let kind = match r.u8()? {
        0 => BodyKind::Static,
        1 => BodyKind::Kinematic,
        2 => BodyKind::Dynamic,
        _ => return None,
    };
    let pos = at(r.pos()?)?;
    let rotation = [r.f32()?, r.f32()?, r.f32()?, r.f32()?];
    let mut shapes = Vec::new();
    for _ in 0..r.u8()? {
        let shape = match r.u8()? {
            0 => ShapeRow::Sphere { center: r.vec3()?, radius: r.f32()? },
            1 => ShapeRow::Capsule { center1: r.vec3()?, center2: r.vec3()?, radius: r.f32()? },
            2 => ShapeRow::Box {
                center: r.vec3()?,
                rotation: [r.f32()?, r.f32()?, r.f32()?, r.f32()?],
                half_extents: r.vec3()?,
            },
            _ => return None,
        };
        shapes.push(ShapePart { shape, density: r.f32()? });
    }
    let row = BodyRow {
        kind,
        pos,
        rotation,
        shapes,
        friction: r.f32()?,
        restitution: r.f32()?,
        rolling_resistance: r.f32()?,
        linear_damping: r.f32()?,
        angular_damping: r.f32()?,
        gravity_scale: r.f32()?,
        bullet: flag(&mut r)?,
        may_sleep: flag(&mut r)?,
        grabbable: flag(&mut r)?,
    };
    let phys = match flag(&mut r)? {
        false => None,
        true => Some(BodyStateRow {
            linear: r.vec3()?,
            angular: r.vec3()?,
            awake: flag(&mut r)?,
            sleep_time: r.f32()?,
            digest: r.take::<32>()?,
        }),
    };
    r.0.is_empty().then_some((row, phys))
}

/// The mock's tree: its root, its objects' hashes and their records. It has no inner nodes, so `node` finds nothing,
/// and its records carry no proof.
struct MockTree<'a>(&'a MockWorld);

impl Tree for MockTree<'_> {
    fn root(&self) -> Hash {
        self.0.root()
    }
    fn frame(&self) -> Frame {
        self.0.frame
    }
    fn node(&self, _path: &TreePath) -> Option<Node> {
        None
    }
    fn export(&self, keys: &[LeafKey]) -> Vec<Record> {
        let w = self.0;
        let record = |key| {
            let bytes = match key {
                LeafKey::Object(id) => encode_object(w.bodies.get(&id)?, w.physics.get(&id)),
                LeafKey::Physics(d) if d == w.domain => w.global.0.to_vec(),
                _ => return None,
            };
            Some(Record { key, bytes: Bytes::from(bytes), proof: Vec::new() })
        };
        keys.iter().filter_map(|&key| record(key)).collect()
    }
    fn object_hashes(&self, ids: &[ObjectId]) -> Vec<(ObjectId, Option<Hash>)> {
        ids.iter()
            .map(|&id| (id, self.0.bodies.get(&id).map(|b| MockWorld::object_hash(id, b, self.0.physics.get(&id)))))
            .collect()
    }
    fn verify(&self) -> Result<(), Mismatch> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_codec_round_trips_and_refuses_junk() {
        let ops = [
            MockOp::Spawn {
                id: ObjectId::from_u128(7),
                kind: BodyKind::Dynamic,
                shape: ShapeRow::Sphere { center: [0.0; 3], radius: 0.04 },
                pos: [1.0, 2.0, 3.0],
            },
            MockOp::Teleport { id: ObjectId::from_u128(7), pos: [0.0, 1.0, 0.0] },
            MockOp::Push { id: ObjectId::from_u128(7), impulse: [0.0, 1.0, 0.0] },
            MockOp::Remove { id: ObjectId::from_u128(7) },
        ];
        assert_eq!(decode(&encode(&ops)).as_deref(), Some(&ops[..]));
        assert_eq!(decode(&[9]), None);
        assert_eq!(decode(&encode(&ops)[..5]), None);
    }

    fn spawn(w: &mut MockWorld, n: u128, kind: BodyKind, shape: ShapeRow, pos: [f64; 3]) {
        let mut fx = Effects::default();
        w.apply_ops(&[MockOp::Spawn { id: ObjectId::from_u128(n), kind, shape, pos }], &mut fx, &mut Vec::new())
            .expect("spawn");
    }

    fn import_of(w: &MockWorld, records: Vec<Record>, absent: Vec<LeafKey>, image: bool) -> pe_seams::CutImport {
        let cut = pe_seams::Cut {
            at: pe_seams::CutRef { line: pe_seams::LineId::origin(), height: 3, frame: w.frame },
            heads: Vec::new(),
        };
        let physics = image.then(|| pe_seams::PhysicsImage {
            domain: w.domain,
            frame: w.frame,
            global: w.global,
            bytes: Bytes::new(),
        });
        pe_seams::CutImport { cut, target_root: w.root(), records, absent, physics }
    }

    /// A donor at a cut frame: two bodies of every shape kind, `BodyState` rows on one, a `physics_global`.
    fn donor() -> MockWorld {
        let mut w = MockWorld::new(DomainId(4), SimRate::DEFAULT);
        let rotation = [0.0, 0.6, 0.0, 0.8];
        spawn(&mut w, 1, BodyKind::Dynamic, ShapeRow::Sphere { center: [0.0; 3], radius: 0.04 }, [1.0, 2.0, 3.0]);
        let shape = ShapeRow::Box { center: [0.1, 0.0, 0.0], rotation, half_extents: [0.5, 0.25, 0.125] };
        spawn(&mut w, 2, BodyKind::Static, shape, [-4.0, 0.0, 0.5]);
        let shape = ShapeRow::Capsule { center1: [0.0; 3], center2: [0.0, 1.0, 0.0], radius: 0.3 };
        spawn(&mut w, 3, BodyKind::Kinematic, shape, [0.0, 9.0, 0.0]);
        let state = BodyStateRow {
            linear: [0.5, -1.0, 0.0],
            angular: [0.0, 0.0, 2.0],
            awake: true,
            sleep_time: 0.25,
            digest: [7; 32],
        };
        w.write_physics(&[(ObjectId::from_u128(1), state)], Hash([9; 32]));
        w.end_frame(Frame(29));
        w
    }

    #[test]
    fn a_whole_export_imported_into_empty_has_the_donor_s_root() {
        let d = donor();
        let records = d.tree(&d).export(&d.leaves());
        assert_eq!(records.len(), 4);
        let got = d.import(&d.empty(), &import_of(&d, records, Vec::new(), true)).expect("import");
        assert_eq!(got.root(), d.root());
        assert_eq!(got.body(ObjectId::from_u128(2)), d.body(ObjectId::from_u128(2)));
        assert_eq!(d.empty().root(), MockWorld::new(DomainId(4), SimRate::DEFAULT).root());
    }

    #[test]
    fn an_import_patches_and_removes_from_a_base() {
        let d = donor();
        let mut base = d.clone();
        spawn(&mut base, 8, BodyKind::Dynamic, ShapeRow::Sphere { center: [0.0; 3], radius: 1.0 }, [0.0; 3]);
        base.bodies.get_mut(&ObjectId::from_u128(1)).expect("body 1").pos = at([5.0, 5.0, 5.0]).expect("in range");
        let records = d.tree(&d).export(&[LeafKey::Object(ObjectId::from_u128(1))]);
        let absent = vec![LeafKey::Object(ObjectId::from_u128(8))];
        let got = d.import(&base, &import_of(&d, records, absent, true)).expect("import");
        assert_eq!(got.root(), d.root());
    }

    #[test]
    fn an_import_refuses_bad_or_missing_records_a_missing_image_and_a_wrong_global() {
        let d = donor();
        let all = d.tree(&d).export(&d.leaves());
        let import = |records, image| d.import(&d.empty(), &import_of(&d, records, Vec::new(), image));
        assert_eq!(import(all.clone(), false).err(), Some(ImportError::NeedsPhysicsImage(ObjectId::from_u128(1))));
        let mut bad = all.clone();
        bad[1].bytes = Bytes::from_static(&[2, 0]);
        assert_eq!(import(bad, true).err(), Some(ImportError::BadProof(1)));
        let mut other = all.clone();
        other[3].key = LeafKey::Physics(DomainId(5));
        assert_eq!(import(other, true).err(), Some(ImportError::BadProof(3)));
        let mut missing = all.clone();
        missing.remove(1);
        let got = import(missing, true).err();
        assert!(matches!(got, Some(ImportError::RootMismatch { .. })), "a missing record: {got:?}");
        let mut wrong = import_of(&d, all, Vec::new(), true);
        wrong.physics.as_mut().expect("image").global = Hash([1; 32]);
        assert_eq!(d.import(&d.empty(), &wrong).err(), Some(ImportError::PhysicsCheck));
    }
}
