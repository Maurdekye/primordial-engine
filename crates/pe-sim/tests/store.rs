//! Object table, columns, validation, hierarchy and transforms.

mod scenario;

use pe_core::coords::WorldPos;
use pe_core::{Error, ObjectId};
use pe_sim::registry::ComponentId;
use pe_sim::{Node, SimStore, Transform};
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
    assert!(rows.iter().all(|v| v.v[0] as usize % 3 != 0));
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
    s.insert(root, Transform::at(WorldPos::new([3, 0, -2], [1023.5, 0.0, 0.0])).with_rot(q).with_scale([2.0; 3])).unwrap();
    s.insert(mid, Transform::local([1.0, 0.0, 0.0])).unwrap();
    s.insert(leaf, Transform::local([0.0, 1.0, 0.0])).unwrap();
    s.set_parent(mid, Some(root), None).unwrap();
    s.set_parent(plain, Some(mid), None).unwrap();
    // `under` sits below an object without a Transform: its frame is `mid`'s.
    s.insert(under, Transform::local([0.0, 0.0, 1.0])).unwrap();
    s.set_parent(under, Some(plain), None).unwrap();
    s.set_parent(leaf, Some(mid), Some(0)).unwrap();
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
