//! The published snapshot and its read API (world-model.md 9.4; pe-api's port, api.md 9.1).

use pe_core::{EntryRef, Frame, Hash, MemberKey, ObjectId, Offset, Pos};
use pe_sim::Transform;
use pe_world::components::{Body, Hand, Name, Protection, Shape, body_kind, shape_kind};
use pe_world::{BlobRef, BlobStore, Claim, DEPTH_LIMIT, GENESIS, Value, Version, World, WorldError};

fn id(n: u128) -> ObjectId {
    ObjectId::from_u128(n)
}

const ALICE: MemberKey = MemberKey([1; 32]);
const BOB: MemberKey = MemberKey([2; 32]);

fn ver(b: u8, op: u32) -> Version {
    Version { entry: EntryRef(Hash([b; 32])), op }
}

fn at(x: f64, y: f64, z: f64) -> Pos {
    Pos::ORIGIN.checked_add(Offset([x, y, z])).unwrap()
}

/// A table (root, protected by Alice) with two children, a ball held by Bob's hand.
fn scene() -> World {
    let mut w = World::new(7, BlobStore::default());
    let name = w.blobs().put(b"table");
    w.create(id(10), None, ALICE, ver(1, 0)).unwrap();
    w.insert(id(10), Transform::at(at(5.0, 0.0, 0.0)).with_scale([2.0; 3])).unwrap();
    w.insert(id(10), Name { text: name }).unwrap();
    w.insert(id(10), Protection { owner: ALICE }).unwrap();
    w.create(id(30), Some(id(10)), ALICE, ver(1, 1)).unwrap();
    w.insert(id(30), Transform::local([0.0, 1.0, 0.0])).unwrap();
    w.create(id(20), Some(id(10)), BOB, ver(2, 0)).unwrap();
    w.insert(id(20), Transform::local([0.5, 0.0, 0.0])).unwrap();
    w.insert(id(20), Shape { kind: shape_kind::SPHERE, density: 1000.0, size: [0.1, 0.0, 0.0], ..Default::default() })
        .unwrap();
    w.insert(id(20), Body { kind: body_kind::DYNAMIC, friction: 0.5, grabbable: 1, ..Default::default() }).unwrap();
    w.create(id(40), None, BOB, ver(3, 0)).unwrap();
    w.insert(id(40), Hand { owner: BOB, side: 1, ..Default::default() }).unwrap();
    w.set_claim(id(20), Claim { hand: id(40), member: BOB, since: Frame(3) }).unwrap();
    w.set_frame(Frame(9));
    w
}

#[test]
fn order_and_hierarchy() {
    let s = scene().snapshot(Frame(4));
    assert_eq!((s.frame(), s.final_frame()), (Frame(9), Frame(4)));
    assert_eq!(s.roots(), [id(10), id(40)]);
    assert_eq!(s.all(), [id(10), id(20), id(30), id(40)]);
    // Children in their order (insertion), not id order.
    assert_eq!(s.children(id(10)), [id(30), id(20)]);
    assert!(s.children(id(99)).is_empty());
}

#[test]
fn info() {
    let s = scene().snapshot(Frame(4));
    let t = s.info(id(10)).unwrap();
    assert_eq!(t.name.as_deref(), Some("table"));
    assert_eq!((t.parent, t.creator, t.version, t.structural), (None, ALICE, ver(1, 0), ver(1, 0)));
    assert_eq!(t.types, ["Transform", "Name", "Protection"]);
    let b = s.info(id(20)).unwrap();
    assert_eq!((b.parent, b.creator, b.version), (Some(id(10)), BOB, ver(2, 0)));
    assert_eq!(b.held_by, Some(BOB));
    // Protection reaches down the subtree.
    assert_eq!(b.protected_by, Some(ALICE));
    assert_eq!(b.types, ["Transform", "Shape", "Body"]);
    let h = s.info(id(40)).unwrap();
    assert_eq!((h.name, h.held_by, h.protected_by), (None, None, None));
    assert!(s.info(id(99)).is_none());
}

#[test]
fn fields_decode_by_name() {
    let s = scene().snapshot(Frame(4));
    let body = s.fields(id(20), "Body").unwrap();
    let get = |n: &str| body.iter().find(|(k, _)| k == n).map(|(_, v)| v.clone());
    assert_eq!(get("kind"), Some(Value::U(body_kind::DYNAMIC as u64)));
    assert_eq!(get("friction"), Some(Value::F(0.5)));
    assert_eq!(get("grabbable"), Some(Value::Bool(true)));
    assert_eq!(get("_pad"), None, "padding is not shown");
    let shape = s.fields(id(20), "Shape").unwrap();
    assert_eq!(shape.iter().find(|(k, _)| k == "mesh").map(|(_, v)| v), Some(&Value::SimBlob(None)));
    assert!(s.fields(id(20), "Light").is_none());
    assert!(s.fields(id(20), "Node").is_none());
    assert!(s.fields(id(20), "NoSuchType").is_none());
    let hand = s.fields(id(40), "Hand").unwrap();
    assert_eq!(hand[0], ("owner".to_string(), Value::Member(BOB)));
}

#[test]
fn positions_never_come_out_as_numbers() {
    let s = scene().snapshot(Frame(4));
    let root = s.fields(id(10), "Transform").unwrap();
    assert_eq!(root[0], ("pos".to_string(), Value::Pos(at(5.0, 0.0, 0.0))));
    assert!(root.iter().all(|(k, _)| k != "chunk"));
    let child = s.fields(id(20), "Transform").unwrap();
    assert_eq!(child[0], ("pos".to_string(), Value::Offset(Offset([0.5, 0.0, 0.0]))));
}

#[test]
fn offsets_and_distance() {
    let s = scene().snapshot(Frame(4));
    // The ball is 0.5 m along the table's x, and the table has scale 2: 1 m in world terms.
    assert_eq!(s.world_transform(id(20)).unwrap().pos, at(6.0, 0.0, 0.0));
    assert_eq!(s.offset_in(id(20), None), Some(Offset([6.0, 0.0, 0.0])));
    assert_eq!(s.offset_in(id(20), Some(id(10))), Some(Offset([0.5, 0.0, 0.0])));
    // The child at (0, 1, 0) is at world (5, 2, 0); the ball at (6, 0, 0).
    assert_eq!(s.distance(id(20), id(30)), Some(5.0f64.sqrt()));
    assert_eq!(s.distance(id(20), id(40)), None, "the hand has no Transform");
}

#[test]
fn a_snapshot_never_changes() {
    let mut w = scene();
    let s = w.snapshot(Frame(4));
    w.insert(id(20), Body { kind: body_kind::STATIC, ..Default::default() }).unwrap();
    w.create(id(50), None, ALICE, ver(4, 0)).unwrap();
    w.clear_claim(id(20));
    w.set_frame(Frame(10));
    assert_eq!(s.frame(), Frame(9));
    assert_eq!(s.all().len(), 4);
    assert_eq!(s.info(id(20)).unwrap().held_by, Some(BOB));
    let kind = s.fields(id(20), "Body").unwrap()[0].clone();
    assert_eq!(kind, ("kind".to_string(), Value::U(body_kind::DYNAMIC as u64)));
    let now = w.snapshot(Frame(4));
    assert_eq!(now.all().len(), 5);
    assert_eq!(now.info(id(20)).unwrap().held_by, None);
}

#[test]
fn snapshots_are_shareable() {
    fn send_sync<T: Send + Sync>(_: &T) {}
    let s = scene().snapshot(Frame(4));
    send_sync(&s);
    let t = std::thread::spawn(move || s.all().len()).join().unwrap();
    assert_eq!(t, 4);
}

#[test]
fn names_need_their_blob() {
    let blobs = BlobStore::default();
    let mut w = World::new(1, blobs.clone());
    let text = BlobRef::of(b"late");
    w.create(id(1), None, ALICE, GENESIS).unwrap();
    w.insert(id(1), Name { text }).unwrap();
    let s = w.snapshot(Frame(0));
    assert_eq!(s.info(id(1)).unwrap().name, None, "a missing display blob is not an error");
    assert!(!blobs.put_checked(&text, b"wrong"));
    assert!(blobs.put_checked(&text, b"late"));
    // The blob store is the machine's, shared: the bytes show up in a snapshot taken before they arrived.
    assert_eq!(s.name(id(1)).as_deref(), Some("late"));
}

#[test]
fn creation_refusals_change_nothing() {
    let mut w = scene();
    let before = w.snapshot(Frame(0));
    assert_eq!(w.create(id(10), None, ALICE, GENESIS), Err(WorldError::Duplicate(id(10))));
    assert_eq!(w.create(id(60), Some(id(99)), ALICE, GENESIS), Err(WorldError::UnknownObject(id(99))));
    assert_eq!(w.create(ObjectId::NONE, None, ALICE, GENESIS), Err(WorldError::UnknownObject(ObjectId::NONE)));
    assert_eq!(w.store().objects(), before.all());
}

#[test]
fn depth_limit() {
    let mut w = World::new(1, BlobStore::default());
    w.create(id(1), None, ALICE, GENESIS).unwrap();
    for n in 2..=(DEPTH_LIMIT as u128 + 1) {
        w.create(id(n), Some(id(n - 1)), ALICE, GENESIS).unwrap();
    }
    // id(DEPTH_LIMIT + 1) is DEPTH_LIMIT levels below the root: one more is refused.
    let deepest = id(DEPTH_LIMIT as u128 + 1);
    let next = id(DEPTH_LIMIT as u128 + 2);
    assert_eq!(
        w.create(next, Some(deepest), ALICE, GENESIS),
        Err(WorldError::TooDeep { object: next, limit: DEPTH_LIMIT })
    );
    assert!(!w.store().exists(next));
    // Lowering the limit refuses new depth but leaves what is there.
    w.set_depth_limit(4);
    assert!(w.store().exists(deepest));
    assert!(w.create(next, Some(id(5)), ALICE, GENESIS).is_err());
    assert!(w.create(next, Some(id(4)), ALICE, GENESIS).is_ok());
    // Reads over a deep chain are iterative: the deepest object still finds the root's protection.
    w.insert(id(1), Protection { owner: BOB }).unwrap();
    assert_eq!(w.snapshot(Frame(0)).info(deepest).unwrap().protected_by, Some(BOB));
}

/// Padding is zero on every way in (@chunks-reviewer's M1 on world/read-v1): readers hide it, and every hash covers
/// it, so a nonzero pad would be state no tool can see.
#[test]
fn padding_must_be_zero() {
    use pe_world::components::ids;
    let mut w = World::new(1, BlobStore::default());
    w.create(id(1), None, ALICE, GENESIS).unwrap();
    let shape = Shape { kind: shape_kind::BOX, density: 1.0, size: [1.0; 3], ..Default::default() };
    let bad = Shape { _pad: 0xdead_beef, ..shape };
    // Through the typed insert and the raw one (logic, MCP, scene loading).
    assert!(matches!(w.insert(id(1), bad), Err(WorldError::Store(pe_core::Error::InvalidValue { .. }))));
    let mut store = w.store().clone();
    assert!(store.insert_raw(ids::SHAPE, id(1), bytemuck::bytes_of(&bad)).is_err());
    assert!(!w.store().has::<Shape>(id(1)));
    // Through a loaded snapshot: flip one pad byte of a valid row in the serialized bytes.
    w.insert(id(1), shape).unwrap();
    let mut bytes = w.store().snapshot().to_bytes();
    let row = bytemuck::bytes_of(&shape);
    let at = bytes.windows(row.len()).position(|x| x == row).expect("the row is in the bytes");
    assert!(pe_sim::Snapshot::from_bytes(&bytes, w.store().registry()).is_ok());
    bytes[at + core::mem::offset_of!(Shape, _pad)] = 1;
    assert!(matches!(
        pe_sim::Snapshot::from_bytes(&bytes, w.store().registry()),
        Err(pe_core::Error::InvalidValue { field, .. }) if field == "_pad"
    ));
}
