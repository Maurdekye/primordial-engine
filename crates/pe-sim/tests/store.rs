//! Object table, columns, validation, hierarchy and transforms.

mod scenario;

use pe_core::coords::WorldPos;
use pe_core::{Error, ObjectId};
use pe_sim::registry::ComponentId;
use pe_sim::{Node, SimStore, Transform, WorldTransform};
use scenario::*;

fn kids(s: &SimStore, p: ObjectId) -> Vec<ObjectId> {
    s.children(p).collect()
}

#[test]
fn objects_and_counters() {
    let mut s = new_store();
    let a = s.create(5).unwrap();
    let b = s.create(5).unwrap();
    assert_eq!((a, b), (ObjectId::new(5, 1), ObjectId::new(5, 2)));
    s.create_with_id(ObjectId::new(5, 10)).unwrap();
    assert_eq!(s.create(5).unwrap(), ObjectId::new(5, 11), "counters move past explicit ids");
    assert_eq!(s.create_with_id(a), Err(Error::DuplicateObject(a)));
    assert!(s.create_with_id(ObjectId::NONE).is_err());
    s.destroy(b).unwrap();
    assert!(!s.exists(b));
    assert_eq!(s.create(5).unwrap(), ObjectId::new(5, 12), "ids are never reused");
    assert_eq!(s.objects(), &[a, ObjectId::new(5, 10), ObjectId::new(5, 11), ObjectId::new(5, 12)]);
    assert_eq!(s.count(ComponentId::NODE), 4);
}

#[test]
fn components_typed_and_raw() {
    let mut s = new_store();
    let a = s.create(1).unwrap();
    let ghost = ObjectId::new(9, 9);
    let v = Velocity { v: [1.0, 2.0, 3.0], spin: 0.5 };
    s.insert(a, v).unwrap();
    assert_eq!(s.get::<Velocity>(a), Some(&v));
    assert_eq!(s.get_raw(<Velocity as pe_sim::Component>::ID, a), Some(bytemuck::bytes_of(&v)));
    assert_eq!(s.insert(ghost, v), Err(Error::UnknownObject(ghost)));
    // Invalid values are refused at the boundary.
    let bad = Velocity { v: [f32::NAN, 0.0, 0.0], spin: 0.0 };
    assert!(matches!(s.insert(a, bad), Err(Error::InvalidValue { field, .. }) if field == "v"));
    assert!(matches!(s.insert_raw(HEAT, a, &heat_row(f64::INFINITY, 0, false)), Err(Error::InvalidValue { .. })));
    let mut bad_bool = heat_row(1.0, 0, false);
    bad_bool[12] = 7;
    assert!(s.insert_raw(HEAT, a, &bad_bool).is_err());
    assert!(matches!(s.insert_raw(HEAT, a, &[0u8; 8]), Err(Error::WrongSize { .. })));
    assert_eq!(s.insert_raw(ComponentId(4000), a, &[0u8; 8]), Err(Error::UnknownComponent(4000)));
    assert_eq!(s.insert_raw(ComponentId::NODE, a, bytemuck::bytes_of(&Node::ROOT)), Err(Error::Reserved { component: 1 }));
    assert_eq!(s.remove_raw(ComponentId::NODE, a), Err(Error::Reserved { component: 1 }));
    // Unvalidated writes are caught by the check pass.
    s.get_mut::<Velocity>(a).unwrap().spin = f32::INFINITY;
    assert!(matches!(s.check_values(), Err(Error::InvalidValue { object, .. }) if object == a));
    s.get_mut::<Velocity>(a).unwrap().spin = 1.0;
    assert_eq!(s.check_values(), Ok(()));
    assert_eq!(s.remove::<Velocity>(a), Ok(true));
    assert_eq!(s.remove::<Velocity>(a), Ok(false));
    assert!(!s.has::<Velocity>(a));
}

#[test]
fn columns_stay_sorted() {
    let mut s = new_store();
    let ids: Vec<ObjectId> = (0..50).map(|_| s.create(2).unwrap()).collect();
    // Insert in a scrambled order; iteration is always by id.
    for k in 0..50 {
        let i = (k * 37) % 50;
        s.insert(ids[i], Velocity { v: [i as f32, 0.0, 0.0], spin: 0.0 }).unwrap();
    }
    let got: Vec<(ObjectId, f32)> = s.iter::<Velocity>().map(|(id, v)| (id, v.v[0])).collect();
    let want: Vec<(ObjectId, f32)> = ids.iter().enumerate().map(|(i, &id)| (id, i as f32)).collect();
    assert_eq!(got, want);
    for i in (0..50).step_by(3) {
        s.destroy(ids[i]).unwrap();
    }
    let (cids, rows) = s.column::<Velocity>();
    assert!(cids.windows(2).all(|w| w[0] < w[1]));
    assert!(rows.iter().all(|v| !(v.v[0] as usize).is_multiple_of(3)));
}

#[test]
fn hierarchy_order_cycles_and_destroy() {
    let mut s = new_store();
    let [p, a, b, c, d] = [0; 5].map(|_| s.create(1).unwrap());
    s.set_parent(a, Some(p), None).unwrap();
    s.set_parent(b, Some(p), None).unwrap();
    s.set_parent(c, Some(p), Some(1)).unwrap();
    assert_eq!(kids(&s, p), vec![a, c, b]);
    s.set_parent(b, Some(p), Some(0)).unwrap();
    assert_eq!(kids(&s, p), vec![b, a, c]);
    s.set_parent(d, Some(a), None).unwrap();
    assert_eq!(s.set_parent(p, Some(d), None), Err(Error::Cycle { child: p, parent: d }));
    assert_eq!(s.set_parent(a, Some(a), None), Err(Error::Cycle { child: a, parent: a }));
    assert_eq!(s.depth_first(), vec![p, b, a, d, c]);
    assert_eq!(s.roots().collect::<Vec<_>>(), vec![p]);
    s.set_parent(a, None, None).unwrap();
    assert_eq!(kids(&s, p), vec![b, c]);
    assert_eq!(s.roots().collect::<Vec<_>>(), vec![p, a]);
    s.insert(d, Transform::IDENTITY).unwrap();
    s.destroy(a).unwrap();
    assert!(!s.exists(a) && !s.exists(d), "destroy removes the subtree");
    assert!(s.get::<Transform>(d).is_none());
    assert_eq!(s.count(ComponentId::NODE), 3);
    // Destroying a middle child relinks its siblings.
    s.destroy(b).unwrap();
    assert_eq!(kids(&s, p), vec![c]);
    assert_eq!(s.node(c).prev_sibling, ObjectId::NONE);
    assert_eq!(s.node(p).first_child, c);
    assert_eq!(s.node(p).last_child, c);
}

#[test]
fn world_transforms_compose() {
    let mut s = new_store();
    let [root, mid, leaf, plain, under] = [0; 5].map(|_| s.create(1).unwrap());
    // Root: chunk (3, 0, -2), 1023.5 m into it, turned 90 degrees about +y, scaled x2.
    let q = [0.0, core::f32::consts::FRAC_1_SQRT_2, 0.0, core::f32::consts::FRAC_1_SQRT_2];
    // Build the hierarchy first, then give each object its local transform in its frame.
    s.set_parent(mid, Some(root), None).unwrap();
    s.set_parent(plain, Some(mid), None).unwrap();
    s.set_parent(under, Some(plain), None).unwrap();
    s.set_parent(leaf, Some(mid), Some(0)).unwrap();
    s.insert(root, Transform::at(WorldPos::new([3, 0, -2], [1023.5, 0.0, 0.0])).with_rot(q).with_scale([2.0; 3])).unwrap();
    s.insert(mid, Transform::local([1.0, 0.0, 0.0])).unwrap();
    s.insert(leaf, Transform::local([0.0, 1.0, 0.0])).unwrap();
    // `under` sits below an object without a Transform: its frame is `mid`'s.
    s.insert(under, Transform::local([0.0, 0.0, 1.0])).unwrap();
    let mid_w = s.world_transform(mid).unwrap();
    // +x local, rotated 90 degrees about +y, is -z; scaled by 2.
    let d = mid_w.pos.sub(&WorldPos::new([3, 0, -2], [1023.5, 0.0, 0.0]));
    assert!((d[0]).abs() < 1e-6 && (d[2] + 2.0).abs() < 1e-6, "{d:?}");
    assert_eq!(mid_w.pos.chunk, [3, 0, -3], "the child crossed into the next chunk");
    let under_w = s.world_transform(under).unwrap();
    let d2 = under_w.pos.sub(&mid_w.pos);
    assert!((d2[0] - 2.0).abs() < 1e-6, "+z local under the turned root is +x, scaled by 2: {d2:?}");
    assert!(s.world_transform(plain).is_none());
    // The one-pass propagation gives exactly the same bits as the per-object composition.
    let all = s.world_transforms();
    assert_eq!(all.iter().map(|(id, _)| *id).collect::<Vec<_>>(), vec![root, mid, leaf, under]);
    for (id, w) in all {
        assert_eq!(Some(w), s.world_transform(id));
    }
}

fn close(a: &WorldTransform, b: &WorldTransform) -> bool {
    let d = a.pos.sub(&b.pos);
    d.iter().all(|x| x.abs() < 1e-6)
        && a.rot.iter().zip(&b.rot).all(|(x, y)| (x - y).abs() < 1e-6)
        && a.scale.iter().zip(&b.scale).all(|(x, y)| (x - y).abs() < 1e-6 * x.abs().max(1.0))
}

/// Review P21-6: reparenting, and adding or removing an ancestor's Transform, keep world transforms.
#[test]
fn hierarchy_edits_keep_world_transforms() {
    let mut s = new_store();
    let [far, near, obj, kid, holder] = [0; 5].map(|_| s.create(1).unwrap());
    let q = [0.0, 0.6, 0.0, 0.8];
    s.insert(far, Transform::at(WorldPos::new([5_000_000_000, 0, -7], [10.0, 2.0, 3.0])).with_rot(q).with_scale([3.0, 1.0, 0.5])).unwrap();
    s.insert(near, Transform::at(WorldPos::new([-2, 0, 1], [500.0, 0.0, 9.0]))).unwrap();
    s.set_parent(obj, Some(far), None).unwrap();
    s.insert(obj, Transform::local([4.0, 1.0, -2.0]).with_scale([2.0; 3])).unwrap();
    s.set_parent(kid, Some(obj), None).unwrap();
    s.insert(kid, Transform::local([0.0, 0.0, 1.0])).unwrap();
    let (w_obj, w_kid) = (s.world_transform(obj).unwrap(), s.world_transform(kid).unwrap());
    // Detach to a root: placed in its world chunk, not at chunk 0.
    s.set_parent(obj, None, None).unwrap();
    assert!(close(&s.world_transform(obj).unwrap(), &w_obj));
    assert_eq!(s.get::<Transform>(obj).unwrap().chunk, w_obj.pos.chunk);
    assert!(close(&s.world_transform(kid).unwrap(), &w_kid), "the subtree moves with it");
    // Under another parent, and under an object without a Transform (whose frame is `near`).
    s.set_parent(holder, Some(near), None).unwrap();
    s.set_parent(obj, Some(holder), None).unwrap();
    assert!(close(&s.world_transform(obj).unwrap(), &w_obj));
    assert_eq!(s.get::<Transform>(obj).unwrap().chunk, [0; 3]);
    // `holder` gains a Transform: `obj` is rewritten for the new frame and stays put.
    s.insert(holder, Transform::local([7.0, 7.0, 7.0])).unwrap();
    assert!(close(&s.world_transform(obj).unwrap(), &w_obj));
    // ...and loses it again.
    s.remove::<Transform>(holder).unwrap();
    assert!(close(&s.world_transform(obj).unwrap(), &w_obj));
    assert!(close(&s.world_transform(kid).unwrap(), &w_kid));
    // A move whose rewritten transform would exceed the limits (2^50 m from its new frame) is refused,
    // and nothing changes.
    let remote = s.create(1).unwrap();
    s.insert(remote, Transform::at(WorldPos::new([2_000_000_000_000, 0, 0], [0.0; 3]))).unwrap();
    let before = s.state_hash();
    assert!(matches!(s.set_parent(remote, Some(near), None), Err(Error::InvalidValue { field, .. }) if field == "pos"));
    assert_eq!(s.state_hash(), before);
    assert_eq!(s.parent(remote), None);
}

/// Review P21-2: finite but absurd transforms are refused at the boundary, and propagation never panics.
#[test]
fn transform_limits() {
    let mut s = new_store();
    let [a, b] = [0; 2].map(|_| s.create(1).unwrap());
    let bad = |s: &mut SimStore, id, t: Transform| matches!(s.insert(id, t), Err(Error::InvalidValue { .. }));
    // The reviewer's repro: chunk [1, 0, 0], local 1e300.
    assert!(bad(&mut s, a, Transform { chunk: [1, 0, 0], pos: [1e300, 0.0, 0.0], ..Transform::IDENTITY }));
    assert!(bad(&mut s, a, Transform { chunk: [i64::MAX, 0, 0], ..Transform::IDENTITY }));
    assert!(bad(&mut s, a, Transform::IDENTITY.with_rot([3.0, 0.0, 0.0, 0.0])));
    assert!(bad(&mut s, a, Transform::IDENTITY.with_scale([0.0, 1.0, 1.0])));
    assert!(bad(&mut s, a, Transform::IDENTITY.with_scale([1e30, 1.0, 1.0])));
    assert!(bad(&mut s, a, Transform { _pad: 1, ..Transform::IDENTITY }));
    s.insert(a, Transform::IDENTITY).unwrap();
    s.set_parent(b, Some(a), None).unwrap();
    assert!(bad(&mut s, b, Transform { chunk: [0, 0, 1], ..Transform::IDENTITY }), "a child's chunk must be zero");
    s.insert(b, Transform::local([1.0, 0.0, 0.0])).unwrap();
    // A root insert is rebased into its chunk, exactly.
    let c = s.create(1).unwrap();
    s.insert(c, Transform::at(WorldPos { chunk: [0, 0, 0], local: [5000.5, -1.0, 0.0] })).unwrap();
    let t = *s.get::<Transform>(c).unwrap();
    assert_eq!((t.chunk, t.pos), ([4, -1, 0], [904.5, 1023.0, 0.0]));
    // Sim code can still write anything through get_mut: propagation saturates instead of panicking, and
    // the check pass reports it.
    s.get_mut::<Transform>(a).unwrap().pos = [1e300, 0.0, 0.0];
    s.get_mut::<Transform>(a).unwrap().rot = [1e30, 1e30, 0.0, 0.0];
    let w = s.world_transform(b).unwrap();
    assert!(w.pos.local.iter().all(|x| x.is_finite()) && w.rot.iter().all(|x| x.is_finite()));
    assert!(s.check_values().is_err());
    assert!(s.checked_state_hash().is_err());
    // rebase_roots moves positions edited through get_mut back inside their chunk.
    s.get_mut::<Transform>(c).unwrap().pos[0] += 2048.0;
    s.rebase_roots();
    let t = *s.get::<Transform>(c).unwrap();
    assert_eq!((t.chunk[0], t.pos[0]), (6, 904.5));
}

/// Review P21-1: loading a deep chain is linear, not quadratic.
#[test]
fn deep_chain_loads_in_linear_time() {
    let mut s = new_store();
    let chain: Vec<ObjectId> = (0..20_000).map(|_| s.create(1).unwrap()).collect();
    // Linked bottom-up, so each new parent is still a root.
    for i in (1..chain.len()).rev() {
        s.set_parent(chain[i], Some(chain[i - 1]), None).unwrap();
    }
    let bytes = s.snapshot().to_bytes();
    let t = std::time::Instant::now();
    let back = pe_sim::Snapshot::from_bytes(&bytes, s.registry()).unwrap();
    // Quadratic took 12.6 s at 16k objects in the review (release); linear is milliseconds.
    assert!(t.elapsed().as_secs_f64() < 5.0, "from_bytes took {:?}", t.elapsed());
    assert_eq!(back.state_hash(), s.state_hash());
}
