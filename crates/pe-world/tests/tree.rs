//! The hash tree (world-model.md 6.2-6.6): the canonical trie, the walk a peer checks, export proofs, what the root
//! covers, and the byte layouts 6.6 fixes.

use pe_core::{Frame, Hash, MemberKey, ObjectId, Offset, Pos};
use pe_seams::{DomainId, LeafKey, Nibbles, Node, Tree, TreePath};
use pe_sim::Transform;
use pe_world::components::{Body, Shape, shape_kind};
use pe_world::preset::{ball_pit, find, names};
use pe_world::tree::{table, table_hash};
use pe_world::{BlobStore, Claim, GENESIS, World, perm};

const D: DomainId = DomainId(7);

/// A deterministic spread of ids: a 64-bit LCG, widened, so prefixes collide at a few nibbles.
fn ids(n: usize, seed: u64) -> Vec<ObjectId> {
    let mut x = seed;
    (0..n)
        .map(|_| {
            x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let hi = x;
            x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            ObjectId::from_u128(((hi as u128) << 64 | x as u128).max(1))
        })
        .collect()
}

fn at(x: f64) -> Pos {
    Pos::ORIGIN.checked_add(Offset([x, 0.0, 0.0])).unwrap()
}

fn world_with(order: &[ObjectId]) -> World {
    let mut w = World::new(1, BlobStore::default());
    for (i, &id) in order.iter().enumerate() {
        w.create(id, None, MemberKey::LOGIC, GENESIS).unwrap();
        w.insert(id, Transform::at(at(i as f64 * 0.0 + 1.0))).unwrap();
    }
    w
}

#[test]
fn the_trie_depends_on_the_id_set_only() {
    let set = ids(300, 11);
    let mut rev = set.clone();
    rev.reverse();
    let mut shuffled = set.clone();
    shuffled.sort_by_key(|id| id.to_u128().rotate_left(37));
    let roots: Vec<Hash> = [&set, &rev, &shuffled].iter().map(|o| world_with(o).root_hash(D)).collect();
    assert_eq!(roots[0], roots[1]);
    assert_eq!(roots[0], roots[2]);
    // One id more or less changes it.
    assert_ne!(roots[0], world_with(&set[1..]).root_hash(D));
}

/// Walks the objects trie from the domain root, checking each node against the hash its parent named, as a peer does.
fn walk(t: &dyn Tree, prefix: Nibbles, expect: Hash, leaves: &mut Vec<ObjectId>) {
    let node = t.node(&TreePath::Objects { domain: D, prefix: prefix.clone() }).unwrap();
    assert_eq!(node.hash(), Some(expect), "node at {:?}", prefix.as_slice());
    match node {
        Node::Leaf { id, .. } => {
            assert!(Nibbles::of_id(id).starts_with(&prefix));
            leaves.push(id);
        }
        Node::Inner { prefix: p, children } => {
            assert!(p.starts_with(&prefix));
            assert!(children.len() >= 2, "the canonical trie has no one-child node");
            for (n, h) in children {
                walk(t, p.child(n).unwrap(), h, leaves);
            }
        }
        Node::Empty => assert_eq!(prefix.len(), 0),
        other => panic!("{other:?}"),
    }
}

#[test]
fn every_node_checks_against_its_parent_from_the_root() {
    let set = ids(500, 3);
    let w = world_with(&set);
    let t = w.tree(D);
    let world = t.node(&TreePath::World).unwrap();
    assert_eq!(world.hash(), Some(t.root()));
    let Node::World { tables, domains, frame, .. } = world else { panic!() };
    assert_eq!(frame, w.frame());
    assert_eq!(t.node(&TreePath::Tables).unwrap().hash(), Some(tables));
    assert_eq!(domains.len(), 1);
    let domain = t.node(&TreePath::Domain(D)).unwrap();
    assert_eq!(domain.hash(), Some(domains[0].1));
    let Node::Domain { objects, .. } = domain else { panic!() };
    let mut leaves = Vec::new();
    walk(&t, Nibbles::new(vec![]).unwrap(), objects, &mut leaves);
    let mut want = set.clone();
    want.sort();
    assert_eq!(leaves, want);
    assert!(t.node(&TreePath::Domain(DomainId(8))).is_none());
    // A prefix no id has is empty.
    let missing = (0..16u8).map(|n| Nibbles::new(vec![n, 15, 15, 15, 15, 15]).unwrap());
    assert!(missing.into_iter().any(|p| t.node(&TreePath::Objects { domain: D, prefix: p }) == Some(Node::Empty)));
}

#[test]
fn an_empty_world_has_the_empty_trie() {
    let w = World::new(1, BlobStore::default());
    let t = w.tree(D);
    let Some(Node::Domain { objects, .. }) = t.node(&TreePath::Domain(D)) else { panic!() };
    assert_eq!(Some(objects), Node::Empty.hash());
    let prefix = Nibbles::new(vec![]).unwrap();
    assert_eq!(t.node(&TreePath::Objects { domain: D, prefix }), Some(Node::Empty));
}

#[test]
fn an_exported_record_proves_itself_against_the_root() {
    let w = ball_pit(5, BlobStore::default(), Pos::ORIGIN);
    let t = w.tree(D);
    let ball = find(&w, "Ball 7").unwrap();
    let recs = t.export(&[LeafKey::Object(ball), LeafKey::Table(table::REGISTRY), LeafKey::Physics(D)]);
    assert_eq!(recs.len(), 2, "physics travels as the image, never as a record");
    // The object: its leaf from the bytes, then each proof node names the next, up to the root.
    let r = &recs[0];
    let object_hash = Hash::of("pe.obj", &r.bytes);
    assert_eq!(t.object_hashes(&[ball]), vec![(ball, Some(object_hash))]);
    let mut child = Node::Leaf { id: ball, object_hash }.hash().unwrap();
    assert_eq!(r.proof[0].hash(), Some(t.root()));
    for node in r.proof[2..].iter().rev() {
        let Node::Inner { prefix, children } = node else { panic!() };
        let n = Nibbles::of_id(ball).as_slice()[prefix.len()];
        assert_eq!(children.iter().find(|c| c.0 == n).map(|c| c.1), Some(child));
        child = node.hash().unwrap();
    }
    let Node::Domain { objects, .. } = &r.proof[1] else { panic!() };
    assert_eq!(*objects, child);
    // The table: its hash from the bytes, named by the tables node.
    let r = &recs[1];
    let Node::Tables(hashes) = &r.proof[1] else { panic!() };
    assert_eq!(hashes.len(), table::COUNT);
    assert_eq!(hashes[table::REGISTRY as usize], table_hash(table::REGISTRY, &r.bytes));
    // An absent object gives no record.
    assert!(t.export(&[LeafKey::Object(ObjectId::from_u128(99))]).is_empty());
}

#[test]
fn the_root_covers_rows_tables_config_and_physics() {
    let base = ball_pit(5, BlobStore::default(), Pos::ORIGIN);
    let r0 = base.root_hash(D);
    assert_eq!(base.clone().root_hash(D), r0);
    assert_ne!(base.root_hash(DomainId(8)), r0, "the domain id is in the root");
    let ball = find(&base, "Ball 0").unwrap();
    let mut changes: Vec<(&str, World)> = Vec::new();
    let mut w = base.clone();
    w.insert(ball, Shape { kind: shape_kind::SPHERE, density: 21.0, size: [0.06, 0.0, 0.0], ..Shape::default() })
        .unwrap();
    changes.push(("a row", w));
    let mut w = base.clone();
    w.insert(ball, Body { friction: 0.7, ..*base.store().get::<Body>(ball).unwrap() }).unwrap();
    changes.push(("another row", w));
    let mut w = base.clone();
    w.set_member(MemberKey([3; 32]), perm::EDIT);
    changes.push(("governance", w));
    let mut w = base.clone();
    let claim = Claim { hand: ObjectId::NONE, member: MemberKey([3; 32]), since: Frame(0), until: Frame(30) };
    w.set_claim(ball, claim).unwrap();
    changes.push(("a claim", w));
    changes.push(("the seed", ball_pit(6, BlobStore::default(), Pos::ORIGIN)));
    let mut w = base.clone();
    w.set_depth_limit(100);
    changes.push(("a setting", w));
    let mut w = base.clone();
    w.set_agree_settings(b"quorum 2/3, cuts every 250 ms");
    changes.push(("agreement settings", w));
    let mut w = base.clone();
    w.write_physics(&[], Hash([2; 32]));
    changes.push(("physics_global", w));
    let mut w = base.clone();
    w.set_frame(Frame(1));
    changes.push(("the frame", w));
    for (what, w) in &changes {
        assert_ne!(w.root_hash(D), r0, "{what}");
    }
}

#[test]
fn the_record_layouts_are_the_formats() {
    let w = ball_pit(5, BlobStore::default(), Pos::ORIGIN);
    let root = find(&w, names::ROOT).unwrap();
    let rec = w.object_record(root).unwrap();
    assert_eq!(&rec[..16], &root.to_u128().to_le_bytes());
    // 6.6's Node record, field by field: five ids as u128 LE (pe-sim stores an id high half first, so this is not
    // its memory layout), creator, the two version entries, three u32, then 4 zero bytes: 192 bytes.
    let node = w.store().node(root);
    let n = &rec[16..208];
    let first_child = node.first_child.to_u128().to_le_bytes();
    assert_eq!(&n[16..32], &first_child);
    assert_eq!(&n[0..16], &node.parent.to_u128().to_le_bytes());
    assert_eq!(&n[80..112], &node.creator.0);
    assert_eq!(&n[112..144], &node.version_entry);
    assert_eq!(&n[144..176], &node.structural_entry);
    assert_eq!(&n[176..180], &node.version_op.to_le_bytes());
    assert_eq!(&n[184..188], &node.born_on.to_le_bytes());
    assert_eq!(&n[188..192], &[0; 4]);
    let rows = u32::from_le_bytes(rec[208..212].try_into().unwrap());
    assert!(rows >= 2, "Transform and Name at least");
}

#[test]
fn the_tree_is_deterministic_for_the_ball_pit() {
    let a = ball_pit(9, BlobStore::default(), Pos::ORIGIN);
    let b = ball_pit(9, BlobStore::default(), Pos::ORIGIN);
    assert_eq!(a.root_hash(D), b.root_hash(D));
    assert_eq!(a.config_bytes(), b.config_bytes());
    for i in 0..table::COUNT as u8 {
        assert_eq!(a.table_entries(i), b.table_entries(i), "table {i}");
    }
}

/// Rough cost of the root from scratch (11 item 15, "measured, not gated"), for deciding on 6.4's leaf cache. Run with
/// `cargo test --release -p pe-world --test tree -- --ignored --nocapture root_cost`; not a clean timing run.
#[test]
#[ignore]
fn root_cost() {
    use std::time::Instant;
    let w = pe_world::preset::ball_pit_with(4, pe_world::BlobStore::default(), pe_core::Pos::ORIGIN, 500);
    let d = pe_seams::DomainId(1);
    let n = w.store().objects().len();
    let _ = w.root_hash(d);
    let runs = 50;
    let t = Instant::now();
    for _ in 0..runs {
        std::hint::black_box(w.root_hash(d));
    }
    let per = t.elapsed() / runs;
    let ids: Vec<_> = w.store().objects().to_vec();
    let t = Instant::now();
    for _ in 0..runs {
        for &id in &ids {
            std::hint::black_box(w.object_hash(id));
        }
    }
    let leaves = t.elapsed() / runs;
    let t = Instant::now();
    for _ in 0..runs {
        for &id in &ids {
            std::hint::black_box(w.object_record(id));
        }
    }
    let records = t.elapsed() / runs;
    let t = Instant::now();
    for _ in 0..runs {
        for i in 0..table::COUNT as u8 {
            std::hint::black_box(table_hash(i, &pe_world::tree::table_record(&w.table_entries(i))));
        }
    }
    let tables = t.elapsed() / runs;
    let mut c = w.clone();
    c.refresh_tree_cache();
    let t = Instant::now();
    for _ in 0..runs {
        std::hint::black_box(c.root_hash(d));
    }
    let cached = t.elapsed() / runs;
    println!(
        "MEASURED root_cost objects={n} per_root={per:?} leaves={leaves:?} records={records:?} tables={tables:?} \
         cached_root={cached:?}"
    );
}

/// 6.4's leaf cache (acceptance 10): random writes, refreshes and rollbacks. A world that refreshes its cache at random
/// points always has the root of an identical world that never refreshes (every leaf from scratch), and `verify`
/// finds no stale leaf.
#[test]
fn the_cached_root_equals_the_root_from_scratch() {
    use bytes::Bytes;
    use pe_seams::test_util::{EntryBuilder, TestKeys};
    use pe_seams::{Class, CutImport, PhysicsImage};
    use pe_world::edit::{Actor, Op, Target, Transaction};
    let keys = TestKeys::new(1, 1);
    let mut author = EntryBuilder::new(&keys, 0);
    let mut cached = pe_world::preset::ball_pit_with(9, BlobStore::default(), Pos::ORIGIN, 60);
    cached.set_member(keys.key(0), perm::EDIT);
    let mut scratch = cached.clone();
    let mut edit = |ops: Vec<Op>| {
        let t = Transaction {
            inline: vec![],
            actor: Actor::Person,
            label: None,
            preconditions: vec![],
            ops,
            blobs: vec![],
        };
        author.next(Class::Edit, Frame(1), Bytes::from(t.encode()))
    };
    let balls: Vec<ObjectId> =
        cached.store().iter::<Body>().map(|(id, _)| id).filter(|&id| cached.store().has::<Shape>(id)).collect();
    let fresh = ids(40, 77);
    let mut saved: Option<(World, World)> = None;
    let mut x: u64 = 12345;
    let mut next = |n: u64| {
        x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (x >> 33) % n
    };
    for step in 0..400 {
        match next(11) {
            7 => {
                // A delete by edit: the subtree goes, and its neighbours' links change.
                let id = if next(2) == 0 {
                    fresh[next(fresh.len() as u64) as usize]
                } else {
                    balls[next(balls.len() as u64) as usize]
                };
                let e = edit(vec![Op::Delete { id: Target::Existing(id) }]);
                for w in [&mut cached, &mut scratch] {
                    w.apply_edit(Frame(1), &e);
                }
            }
            8 => {
                // A reparent by edit: the old and new parent and the siblings change.
                let id = balls[next(balls.len() as u64) as usize];
                let parent = balls[next(balls.len() as u64) as usize];
                let e = edit(vec![Op::Reparent {
                    id: Target::Existing(id),
                    parent: Target::Existing(parent),
                    index: None,
                    keep_world: next(2) == 0,
                }]);
                for w in [&mut cached, &mut scratch] {
                    w.apply_edit(Frame(1), &e);
                }
            }
            9 | 10 => {
                // An import: a physics-free object's record from a diverged copy, or its removal as absent.
                let id = fresh[next(fresh.len() as u64) as usize];
                if !scratch.store().exists(id) || scratch.store().has::<Body>(id) {
                    continue;
                }
                let mut other = scratch.clone();
                other.insert(id, Transform::at(at(next(30) as f64 * 0.1))).unwrap();
                let t = other.tree(D);
                let (records, absent) = if next(2) == 0 {
                    (t.export(&[LeafKey::Object(id)]), Vec::new())
                } else {
                    (Vec::new(), vec![LeafKey::Object(id)])
                };
                let imp = CutImport {
                    blobs: vec![],
                    cut: pe_seams::Cut {
                        at: pe_seams::CutRef { line: other.line(), height: 0, frame: other.frame() },
                        heads: Vec::new(),
                    },
                    target_root: t.root(),
                    records,
                    absent,
                    physics: None::<PhysicsImage>,
                };
                if let (Ok(c), Ok(s)) = (cached.import(D, &imp), scratch.import(D, &imp)) {
                    cached = c;
                    scratch = s;
                }
            }
            0 => {
                // A pose write, the replica's per-frame path.
                let b = balls[next(balls.len() as u64) as usize];
                let p = at(next(100) as f64 * 0.01);
                for w in [&mut cached, &mut scratch] {
                    w.write_poses(&[(b, p, [0.0, 0.0, 0.0, 1.0])]);
                }
            }
            1 => {
                // A new object under a random ball: the parent's and the last sibling's `Node` change too.
                let id = fresh[next(fresh.len() as u64) as usize];
                let parent = balls[next(balls.len() as u64) as usize];
                for w in [&mut cached, &mut scratch] {
                    let _ = w.create(id, Some(parent), MemberKey::LOGIC, GENESIS);
                }
            }
            2 => {
                let b = balls[next(balls.len() as u64) as usize];
                let t = Transform::at(at(1.0 + next(50) as f64 * 0.1));
                // Refused alike on both if the ball is gone or no longer a root.
                let r: Vec<bool> = [&mut cached, &mut scratch].into_iter().map(|w| w.insert(b, t).is_ok()).collect();
                assert_eq!(r[0], r[1]);
            }
            3 => cached.refresh_tree_cache(),
            4 => saved = Some((cached.clone(), scratch.clone())),
            5 => {
                // A rollback to a saved state, cache and all.
                if let Some((c, s)) = saved.take() {
                    cached = c;
                    scratch = s;
                }
            }
            _ => {
                let mut f = cached.clone();
                f.set_frame(Frame(step));
                f.refresh_tree_cache();
            }
        }
        assert_eq!(cached.root_hash(D), scratch.root_hash(D), "step {step}");
        assert_eq!(cached.tree(D).verify(), Ok(()), "step {step}");
    }
    // The cache was used: after a refresh every live object has a cached leaf equal to its rows'.
    cached.refresh_tree_cache();
    for &id in cached.store().objects() {
        assert_eq!(cached.store().cached_hash(id).map(Hash), scratch.object_hash(id));
    }
}
