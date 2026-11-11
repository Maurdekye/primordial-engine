//! A small world model for pe-replica's tests and the harness, until pe-world exists: objects that are spheres or
//! boxes, created, removed, teleported and pushed by edit entries in a tiny test codec. It implements
//! [`WorldModel`] the way replica.md 8.1 asks of pe-world: every entry is all or nothing, refused entries change
//! nothing, and the root covers every row, the physics rows and the frame.

use std::collections::BTreeMap;

use pe_seams::{
    Class, DomainId, Frame, Hash, Hasher, ImportError, LeafKey, Mismatch, Node, ObjectId, Offset, Outcome, Pos, Reason,
    Record, SignedEntry, SimRate, Tree, TreePath,
};

use crate::world::{
    BodyKind, BodyRow, BodyStateRow, Change, ClaimRow, Effects, HandRow, PhysicsSettings, PoseOutOfRange, PushFx,
    ShapeRow, WorldModel,
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
        shapes: vec![shape],
        density: 1000.0,
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
        }
    }

    /// A body's position, as plain numbers from the origin.
    pub fn position(&self, id: ObjectId) -> Option<[f64; 3]> {
        self.bodies.get(&id).map(|b| (b.pos - Pos::ORIGIN).0)
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

    fn write_pose(&mut self, id: ObjectId, pos: Pos, rotation: [f32; 4]) -> Result<(), PoseOutOfRange> {
        let p = (pos - Pos::ORIGIN).0;
        if p.iter().any(|c| !c.is_finite() || c.abs() > RANGE) {
            return Err(PoseOutOfRange);
        }
        if let Some(b) = self.bodies.get_mut(&id) {
            b.pos = pos;
            b.rotation = rotation;
        }
        Ok(())
    }

    fn maintain_transforms(&mut self) -> u32 {
        0
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

    fn import(&self, _saved: &MockWorld, _import: &pe_seams::CutImport) -> Result<MockWorld, ImportError> {
        Err(ImportError::NotKept)
    }
}

/// The mock's tree: its root, and its objects' hashes. It has no inner nodes, so `node` and `export` find nothing.
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
    fn export(&self, _keys: &[LeafKey]) -> Vec<Record> {
        Vec::new()
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
}
