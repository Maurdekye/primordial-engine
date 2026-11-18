//! The calls pe-replica's WorldModel adapter makes (replica.md 8.1; vdw-physics).

use std::collections::BTreeSet;

use pe_core::{Frame, Hash, ObjectId, Offset, Pos};
use pe_sim::{Component, Transform};
use pe_world::components::{Body, BodyState, Shape};
use pe_world::{BlobStore, Claim, GRAVITY, World};

fn oid(n: u128) -> ObjectId {
    ObjectId::from_u128(n)
}

fn at(x: f64) -> Pos {
    Pos::ORIGIN.checked_add(Offset([x, 0.0, 0.0])).unwrap()
}

/// A table at x = 5 (a body with a shape), a ball on it (its own body), and a compound child of the table (shape,
/// no body), plus a bare object.
fn world() -> World {
    let mut w = World::new(1, BlobStore::default());
    let k = pe_core::MemberKey([1; 32]);
    for (id, parent) in [(1, None), (2, Some(1)), (3, Some(1)), (4, None)] {
        w.create(oid(id), parent.map(oid), k, pe_world::GENESIS).unwrap();
    }
    w.insert(oid(1), Transform::at(at(5.0))).unwrap();
    w.insert(oid(2), Transform::local([0.0, 1.0, 0.0])).unwrap();
    w.insert(oid(3), Transform::local([1.0, 0.0, 0.0])).unwrap();
    w.insert(oid(1), Body::default()).unwrap();
    w.insert(oid(2), Body::default()).unwrap();
    w.insert(oid(2), Shape::default()).unwrap();
    w.insert(oid(3), Shape::default()).unwrap();
    w
}

#[test]
fn physical_objects_and_settings() {
    let w = world();
    assert_eq!(w.physical(), [oid(1), oid(2)], "the table through its compound child, the ball by its own shape");
    let s = w.settings();
    assert_eq!((s.gravity, s.origin), (GRAVITY, Pos::ORIGIN));
}

#[test]
fn write_poses_converts_parents_first_whatever_the_order() {
    let mut a = world();
    let mut b = world();
    let id = [0.0, 0.0, 0.0, 1.0];
    let poses = [(oid(2), at(9.0), id), (oid(1), at(7.0), id)];
    assert!(a.write_poses(&poses).is_empty());
    let mut rev = poses;
    rev.reverse();
    assert!(b.write_poses(&rev).is_empty());
    assert_eq!(a.root(), b.root(), "order-independent");
    // The ball is at its written world pose, under the moved table.
    assert_eq!(a.store().world_transform(oid(2)).unwrap().pos, at(9.0));
    assert_eq!(a.store().world_transform(oid(1)).unwrap().pos, at(7.0));
    // No transform, or no object: refused, in id order, and the rest still apply.
    let r = a.write_poses(&[(oid(9), at(1.0), id), (oid(4), at(1.0), id), (oid(1), at(6.0), id)]);
    assert_eq!(r, [oid(4), oid(9)]);
    assert_eq!(a.store().world_transform(oid(1)).unwrap().pos, at(6.0));
}

#[test]
fn write_physics_replaces_every_body_state_and_the_root_covers_the_tables() {
    let mut w = world();
    let r0 = w.root();
    w.write_physics(&[(oid(1), BodyState::default()), (oid(99), BodyState::default())], Hash([3; 32]));
    assert!(w.store().get::<BodyState>(oid(1)).is_some());
    assert_eq!(w.physics_global(), Hash([3; 32]));
    let r1 = w.root();
    assert_ne!(r0, r1);
    w.write_physics(&[(oid(2), BodyState::default())], Hash([3; 32]));
    assert!(w.store().get::<BodyState>(oid(1)).is_none(), "a body missing from rows has none afterwards");
    assert!(w.store().get::<BodyState>(oid(2)).is_some());
    // Tables outside the store move the root: a claim, a setting.
    let r2 = w.root();
    w.set_claim(
        oid(2),
        Claim { hand: oid(4), member: pe_core::MemberKey([2; 32]), since: Frame(0), until: Frame(u64::MAX) },
    )
    .unwrap();
    assert_ne!(w.root(), r2);
    assert_eq!(w.claims().map(|(o, _)| o).collect::<Vec<_>>(), [oid(2)]);
    let r3 = w.root();
    w.set_gravity([0.0, -1.0, 0.0]);
    assert_ne!(w.root(), r3);
    // A copy-on-write clone (Saved) restores to the same root.
    let saved = w.clone();
    w.set_gravity(GRAVITY);
    assert_eq!(saved.root(), {
        let mut x = w.clone();
        x.set_gravity([0.0, -1.0, 0.0]);
        x.root()
    });
}

#[test]
fn write_poses_refuses_a_repeated_id_in_any_order() {
    // @chunks-reviewer's Low 1 on f3f64f69: the result must not depend on the order of `poses`.
    let id = [0.0, 0.0, 0.0, 1.0];
    let poses = [(oid(2), at(8.0), id), (oid(1), at(7.0), id), (oid(2), at(9.0), id)];
    let (mut a, mut b) = (world(), world());
    let mut rev = poses;
    rev.reverse();
    assert_eq!(a.write_poses(&poses), [oid(2)]);
    assert_eq!(b.write_poses(&rev), [oid(2)]);
    assert_eq!(a.root(), b.root());
}

#[test]
fn write_physics_keeps_wanted_rows_and_the_last_of_repeats() {
    let mut w = world();
    let s = |v: f32| BodyState { linear: [v, 0.0, 0.0], ..Default::default() };
    w.write_physics(&[(oid(2), s(1.0)), (oid(1), s(2.0))], Hash([1; 32]));
    let r = w.root();
    let mut v = world();
    v.write_physics(&[(oid(1), s(2.0)), (oid(2), s(9.0)), (oid(2), s(1.0))], Hash([1; 32]));
    assert_eq!(v.root(), r, "order-independent; the last row for an id stands");
    w.write_physics(&[(oid(1), s(3.0))], Hash([1; 32]));
    assert_eq!(w.store().get::<BodyState>(oid(1)).unwrap().linear[0], 3.0);
    assert!(w.store().get::<BodyState>(oid(2)).is_none());
}

/// `n` objects, 1 to n, each a body with a shape at x = its id, and no `BodyState` rows yet.
fn bodies(n: u128) -> World {
    let mut w = World::new(1, BlobStore::default());
    let k = pe_core::MemberKey([1; 32]);
    for id in 1..=n {
        w.create(oid(id), None, k, pe_world::GENESIS).unwrap();
        w.insert(oid(id), Transform::at(at(id as f64))).unwrap();
        w.insert(oid(id), Body::default()).unwrap();
        w.insert(oid(id), Shape::default()).unwrap();
    }
    w
}

/// One write of the L3a test: what it shows, its rows, and the objects it leaves stale.
type Step = (&'static str, Vec<(ObjectId, BodyState)>, BTreeSet<ObjectId>);

/// L3a: `write_physics` writes only the rows that changed, with the full rewrite's result. Two worlds take the same
/// writes; `full` empties the column first each time, so it always rewrites every row, as before L3a. After every
/// write both have the same root, the same `BodyState` rows byte for byte and the same row versions, and `fast`'s
/// cached tree is true. The writes cover bodies moving, falling asleep, waking, joining and leaving, nothing changing,
/// repeated ids, a row for a missing object and a refused row (a NaN velocity). The objects a write leaves stale show
/// which way it went: only the changed rows' when the bodies are the column's, every row's otherwise.
#[test]
fn write_physics_writes_only_changed_rows_with_the_full_rewrites_result() {
    let moving = |id: u128, v: f32| (oid(id), BodyState { linear: [v, 0.0, 0.0], awake: 1, ..Default::default() });
    let asleep = |id: u128| (oid(id), BodyState { sleep_time: 0.5, ..Default::default() });
    let set = |ids: &[u128]| ids.iter().map(|&i| oid(i)).collect::<BTreeSet<_>>();
    let mut rows: Vec<(ObjectId, BodyState)> = (1..=8).map(|i| moving(i, i as f32)).collect();
    let mut steps: Vec<Step> = Vec::new();
    steps.push(("the first rows", rows.clone(), set(&[1, 2, 3, 4, 5, 6, 7, 8])));
    rows[2] = moving(3, 30.0);
    rows[5] = moving(6, 60.0);
    steps.push(("3 and 6 move", rows.clone(), set(&[3, 6])));
    rows[1] = asleep(2);
    rows[4] = asleep(5);
    steps.push(("2 and 5 fall asleep", rows.clone(), set(&[2, 5])));
    steps.push(("nothing changes", rows.clone(), set(&[])));
    rows[4] = moving(5, 5.5);
    steps.push(("5 wakes, 2 sleeps on", rows.clone(), set(&[5])));
    let mut repeated = rows.clone();
    repeated.insert(0, moving(4, 99.0));
    repeated.push(moving(4, 40.0));
    repeated.insert(0, moving(7, 70.0));
    rows[3] = moving(4, 40.0);
    steps.push(("repeated ids: the last row stands", repeated, set(&[4])));
    let mut missing = rows.clone();
    missing.push(moving(99, 1.0));
    rows[0] = moving(1, 1.5);
    missing[0] = rows[0];
    steps.push(("a row for a missing object, and 1 moves", missing, set(&[1])));
    rows.push(moving(9, 9.0));
    steps.push(("9 joins", rows.clone(), set(&[1, 2, 3, 4, 5, 6, 7, 8, 9])));
    rows.remove(2);
    steps.push(("3 leaves", rows.clone(), set(&[1, 2, 3, 4, 5, 6, 7, 8, 9])));
    let mut refused = rows.clone();
    refused[4] = moving(6, f32::NAN);
    steps.push(("6's row is refused", refused, set(&[6])));
    rows[4] = moving(6, 6.5);
    steps.push(("6 is back", rows.clone(), set(&[1, 2, 4, 5, 6, 7, 8, 9])));

    let mut fast = bodies(9);
    let mut full = fast.clone();
    for (n, (what, rows, stale)) in steps.into_iter().enumerate() {
        let global = Hash([n as u8; 32]);
        fast.refresh_tree_cache();
        fast.write_physics(&rows, global);
        full.write_physics(&[], global);
        full.write_physics(&rows, global);
        assert_eq!(fast.store().stale_hashes(), Some(&stale), "{what}: the objects written");
        assert!(fast.verify_tree_cache(pe_seams::DomainId(0)).is_ok(), "{what}: the cached tree");
        assert_eq!(fast.root(), full.root(), "{what}: the root");
        let (a, b) = (fast.store().column::<BodyState>(), full.store().column::<BodyState>());
        assert_eq!(a.0, b.0, "{what}: the bodies with rows");
        let bytes = |r: &[BodyState]| bytemuck::cast_slice::<BodyState, u8>(r).to_vec();
        assert_eq!(bytes(a.1), bytes(b.1), "{what}: the rows' bytes");
        for &id in a.0 {
            let version = |w: &World| w.store().row_version(BodyState::ID, id);
            assert_eq!(version(&fast), version(&full), "{what}: {id:?}'s row version");
        }
    }
    assert!(fast.store().get::<BodyState>(oid(3)).is_none(), "3 left");
    assert_eq!(fast.store().get::<BodyState>(oid(4)).unwrap().linear[0], 40.0, "the last row for 4 stood");
}
