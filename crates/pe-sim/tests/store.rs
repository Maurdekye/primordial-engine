//! Object table, columns, validation, hierarchy and transforms.

mod scenario;

use pe_core::{Error, ObjectId};
use pe_sim::registry::ComponentId;
use pe_sim::{Node, SimStore, Transform, WorldTransform};
use scenario::*;

fn kids(s: &SimStore, p: ObjectId) -> Vec<ObjectId> {
    s.children(p).collect()
}

#[test]
fn objects_by_given_id() {
    // Ids are derived by their creators (world-model.md 2.1); the store only refuses duplicates and NONE.
    let mut s = new_store();
    let (a, b) = (oid(5, 1), oid(5, 2));
    s.create(a).unwrap();
    s.create(b).unwrap();
    assert_eq!(s.create(a), Err(Error::DuplicateObject(a)));
    assert!(s.create(ObjectId::NONE).is_err());
    s.destroy(b).unwrap();
    assert!(!s.exists(b));
    s.create(b).unwrap(); // the store keeps no counter: re-creating a destroyed id is the caller's choice
    assert_eq!(s.objects(), &[a, b]);
    assert_eq!(s.count(ComponentId::NODE), 2);
}

#[test]
fn components_typed_and_raw() {
    let mut s = new_store();
    let a = s.mk(1);
    let ghost = oid(9, 9);
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
    let ids: Vec<ObjectId> = (0..50).map(|_| s.mk(2)).collect();
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
    let [p, a, b, c, d] = [0; 5].map(|_| s.mk(1));
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
    let [root, mid, leaf, plain, under] = [0; 5].map(|_| s.mk(1));
    // Root: chunk (3, 0, -2), 1023.5 m into it, turned 90 degrees about +y, scaled x2.
    let q = [0.0, core::f32::consts::FRAC_1_SQRT_2, 0.0, core::f32::consts::FRAC_1_SQRT_2];
    // Build the hierarchy first, then give each object its local transform in its frame.
    s.set_parent(mid, Some(root), None).unwrap();
    s.set_parent(plain, Some(mid), None).unwrap();
    s.set_parent(under, Some(plain), None).unwrap();
    s.set_parent(leaf, Some(mid), Some(0)).unwrap();
    s.insert(root, Transform::at(wp([3, 0, -2], [1023.5, 0.0, 0.0])).with_rot(q).with_scale([2.0; 3])).unwrap();
    s.insert(mid, Transform::local([1.0, 0.0, 0.0])).unwrap();
    s.insert(leaf, Transform::local([0.0, 1.0, 0.0])).unwrap();
    // `under` sits below an object without a Transform: its frame is `mid`'s.
    s.insert(under, Transform::local([0.0, 0.0, 1.0])).unwrap();
    let mid_w = s.world_transform(mid).unwrap();
    // +x local, rotated 90 degrees about +y, is -z; scaled by 2.
    let d = dsub(mid_w.pos, wp([3, 0, -2], [1023.5, 0.0, 0.0]));
    assert!((d[0]).abs() < 1e-6 && (d[2] + 2.0).abs() < 1e-6, "{d:?}");
    assert_eq!(mid_w.pos.cell(0).c, [3, 0, -3], "the child crossed into the next chunk");
    let under_w = s.world_transform(under).unwrap();
    let d2 = dsub(under_w.pos, mid_w.pos);
    assert!((d2[0] - 2.0).abs() < 1e-6, "+z local under the turned root is +x, scaled by 2: {d2:?}");
    assert!(s.world_transform(plain).is_none());
    // The one-pass propagation gives exactly the same bits as the per-object composition.
    let all = s.world_transforms();
    assert_eq!(all.iter().map(|(id, _)| *id).collect::<Vec<_>>(), vec![root, mid, leaf, under]);
    for (id, w) in all {
        assert_eq!(Some(w), s.world_transform(id));
    }
}

fn close(a: &WorldTransform, b: &WorldTransform, tol: f64) -> bool {
    let d = dsub(a.pos, b.pos);
    d.iter().all(|x| x.abs() < tol)
        && a.rot.iter().zip(&b.rot).all(|(x, y)| (x - y).abs() < 1e-6)
        && a.scale.iter().zip(&b.scale).all(|(x, y)| (x - y).abs() < 1e-6 * x.abs().max(1.0))
}

/// Review P21-6: the keep-world edits (reparenting, and adding or removing an ancestor's Transform) keep
/// world transforms.
#[test]
fn keep_world_edits() {
    let mut s = new_store();
    let [far, near, obj, kid, holder] = [0; 5].map(|_| s.mk(1));
    let q = [0.0, 0.6, 0.0, 0.8];
    s.insert(far, Transform::at(wp([5_000_000_000, 0, -7], [10.0, 2.0, 3.0])).with_rot(q).with_scale([3.0, 1.0, 0.5])).unwrap();
    // `near` is about 3 km from `far`, turned about another axis.
    let q2 = [0.28, 0.0, 0.96, 0.0];
    s.insert(near, Transform::at(wp([5_000_000_003, 0, -5], [500.0, 0.0, 9.0])).with_rot(q2)).unwrap();
    s.set_parent(obj, Some(far), None).unwrap();
    s.insert(obj, Transform::local([4.0, 1.0, -2.0]).with_scale([2.0; 3])).unwrap();
    s.set_parent(kid, Some(obj), None).unwrap();
    s.insert(kid, Transform::local([0.0, 0.0, 1.0])).unwrap();
    let (w_obj, w_kid) = (s.world_transform(obj).unwrap(), s.world_transform(kid).unwrap());
    // Detach to a root: placed in its world chunk, not at chunk 0.
    s.set_parent_keep_world(obj, None, None).unwrap();
    assert!(close(&s.world_transform(obj).unwrap(), &w_obj, 1e-9));
    assert_eq!(s.get::<Transform>(obj).unwrap().chunk, w_obj.pos.cell(0).c.map(|c| c as i64));
    assert!(close(&s.world_transform(kid).unwrap(), &w_kid, 1e-9), "the subtree moves with it");
    // Under another parent, and under an object without a Transform (whose frame is `near`).
    s.set_parent(holder, Some(near), None).unwrap();
    s.set_parent_keep_world(obj, Some(holder), None).unwrap();
    assert!(close(&s.world_transform(obj).unwrap(), &w_obj, 1e-9));
    assert_eq!(s.get::<Transform>(obj).unwrap().chunk, [0; 3]);
    // `holder` gains a Transform: `obj` is rewritten for the new frame and stays put.
    s.insert_transform_keep_world(holder, Transform::local([7.0, 7.0, 7.0]).with_rot(q)).unwrap();
    assert!(close(&s.world_transform(obj).unwrap(), &w_obj, 1e-9));
    // ...and loses it again.
    assert_eq!(s.remove_transform_keep_world(holder), Ok(true));
    assert!(close(&s.world_transform(obj).unwrap(), &w_obj, 1e-9));
    // The kid sits 2 m out along `obj`'s rotation, which is stored as f32 and rounded on each rewrite:
    // about 1e-7 x 2 m.
    assert!(close(&s.world_transform(kid).unwrap(), &w_kid, 1e-6));
    // A move whose rewritten transform would exceed the limits (2^50 m from its new frame) is refused,
    // and nothing changes.
    let remote = s.mk(1);
    s.insert(remote, Transform::at(wp([2_000_000_000_000, 0, 0], [0.0; 3]))).unwrap();
    let before = s.state_hash();
    assert!(matches!(s.set_parent_keep_world(remote, Some(near), None), Err(Error::InvalidValue { field, .. }) if field == "pos"));
    assert_eq!(s.state_hash(), before);
    assert_eq!(s.parent(remote), None);
}

/// Review P21R-4: keep-world round trips under rotated, scaled frames 5 km away do not drift.
#[test]
fn keep_world_round_trips_do_not_drift() {
    let mut s = new_store();
    let [a, b, o] = [0; 3].map(|_| s.mk(1));
    let (sa, ca) = pe_core::dmath::sin_cos(0.7);
    let (sb, cb) = pe_core::dmath::sin_cos(-1.9);
    s.insert(a, Transform::at(wp([10, 0, 0], [3.0, 0.0, 0.0])).with_rot([0.0, sa, 0.0, ca]).with_scale([1.5; 3])).unwrap();
    s.insert(b, Transform::at(wp([15, 1, 0], [9.0, 0.0, 0.0])).with_rot([sb, 0.0, 0.0, cb])).unwrap();
    s.insert(o, Transform::at(wp([12, 0, 2], [100.0, 5.0, 7.0]))).unwrap();
    let w0 = s.world_transform(o).unwrap();
    for _ in 0..50 {
        s.set_parent_keep_world(o, Some(a), None).unwrap();
        s.set_parent_keep_world(o, Some(b), None).unwrap();
        s.set_parent_keep_world(o, None, None).unwrap();
    }
    // 150 moves between frames about 5 km apart: about f64 rounding, not the 4e-7 x distance of an
    // unnormalized f32 quaternion (about 2 mm per round trip).
    let w = s.world_transform(o).unwrap();
    let d = dsub(w.pos, w0.pos);
    assert!(d.iter().all(|x| x.abs() < 1e-8), "position drifted by {d:?}");
    // The world rotation itself is stored as f32, so it carries f32 rounding per move.
    assert!(w.rot.iter().zip(&w0.rot).all(|(x, y)| (x - y).abs() < 1e-4));
}

/// Review P21R-3: plain edits are data edits, so a loader's insert order does not change the state.
#[test]
fn plain_edits_do_not_depend_on_order() {
    let reg = registry();
    let (p, c) = (oid(2, 5), oid(1, 9)); // the child's id sorts below its parent's
    let tp = Transform::at(wp([3, 0, 0], [1.0, 2.0, 3.0])).with_rot([0.0, 0.6, 0.0, 0.8]);
    let tc = Transform::local([4.0, 0.0, 0.0]);
    let build = |child_first: bool| {
        let mut s = SimStore::new(reg.clone(), SEED);
        s.create(p).unwrap();
        s.create(c).unwrap();
        s.set_parent(c, Some(p), None).unwrap();
        if child_first {
            s.insert(c, tc).unwrap();
            s.insert(p, tp).unwrap();
        } else {
            s.insert(p, tp).unwrap();
            s.insert(c, tc).unwrap();
        }
        s
    };
    let (x, y) = (build(true), build(false));
    assert_eq!(x.snapshot().to_bytes(), y.snapshot().to_bytes());
    assert_eq!(x.get::<Transform>(c), Some(&tc), "the child's local is stored as given");
    // Parenting before or after the transforms also gives the same state.
    let mut z = SimStore::new(reg.clone(), SEED);
    z.create(p).unwrap();
    z.create(c).unwrap();
    z.insert(c, tc).unwrap();
    z.insert(p, tp).unwrap();
    z.set_parent(c, Some(p), None).unwrap();
    assert_eq!(z.state_hash(), x.state_hash());
    // A plain move that would give a frame to an object with a non-zero chunk is refused.
    let r = z.mk(1);
    z.insert(r, Transform::at(wp([1, 0, 0], [0.0; 3]))).unwrap();
    assert_eq!(z.set_parent(r, Some(p), None), Err(Error::ChunkUnderFrame { object: r }));
    // The keep-world move recomputes the chunk instead (review P21S-2).
    let mut z2 = z.clone();
    z2.set_parent_keep_world(r, Some(p), None).unwrap();
    assert_eq!(z2.get::<Transform>(r).unwrap().chunk, [0; 3]);
    let g = z.mk(1);
    z.set_parent(r, Some(g), None).unwrap();
    assert_eq!(z.insert(g, Transform::IDENTITY), Err(Error::ChunkUnderFrame { object: r }));
}

/// Review P21-2 and P21R-1: finite but absurd transforms are refused at the boundary, the stored form is
/// the validated form, and propagation never panics.
#[test]
fn transform_limits() {
    let mut s = new_store();
    let [a, b] = [0; 2].map(|_| s.mk(1));
    let bad = |s: &mut SimStore, id, t: Transform| matches!(s.insert(id, t), Err(Error::InvalidValue { .. }));
    // Review P21-2's repro: chunk [1, 0, 0], local 1e300.
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
    // Review P21R-1's repro: a root at chunk 2^61 with pos 1024 is accepted, stays as stored (no rebase
    // past the limit), passes the check pass and loads.
    let edge = s.mk(1);
    s.insert(edge, Transform { chunk: [1 << 61, 0, 0], pos: [1024.0, 0.0, 0.0], ..Transform::IDENTITY }).unwrap();
    assert_eq!(s.check_values(), Ok(()));
    assert_eq!(s.maintain_transforms(), 0, "the rebased form would break the limit, so it is left as is");
    assert_eq!(s.get::<Transform>(edge).unwrap().chunk[0], 1 << 61);
    pe_sim::Snapshot::from_bytes(&s.snapshot().to_bytes(), s.registry()).unwrap();
    // Root positions are rebased into their chunk by the maintenance pass, exactly.
    let c = s.mk(1);
    s.insert(c, Transform { chunk: [0, 0, 0], pos: [5000.5, -1.0, 0.0], ..Transform::IDENTITY }).unwrap();
    assert_eq!(s.maintain_transforms(), 1);
    let t = *s.get::<Transform>(c).unwrap();
    assert_eq!((t.chunk, t.pos), ([4, -1, 0], [904.5, 1023.0, 0.0]));
    // Sim code can still write anything through get_mut: propagation refuses an out-of-range result (None)
    // instead of panicking or clamping (world-model.md 4.3), the check pass reports it, and the maintenance pass
    // repairs it (review P21R-5).
    s.get_mut::<Transform>(a).unwrap().pos = [1e300, 0.0, 0.0];
    s.get_mut::<Transform>(a).unwrap().rot = [1e30, 1e30, 0.0, 0.0];
    s.get_mut::<Transform>(b).unwrap().chunk = [7, 0, 0];
    assert!(s.world_transform(b).is_none_or(|w| w.rot.iter().all(|x| x.is_finite())));
    assert!(s.check_values().is_err());
    assert!(s.checked_state_hash().is_err());
    assert!(s.maintain_transforms() >= 2);
    assert_eq!(s.check_values(), Ok(()));
    // Review P21S-1: a tiny garbage rotation (q.q subnormal in f32) is repaired to a valid one in one pass.
    s.get_mut::<Transform>(a).unwrap().rot = [3e-23, 0.0, 0.0, 0.0];
    s.get_mut::<Transform>(b).unwrap().rot = [1e-45, 1e-45, 0.0, 0.0];
    assert!(s.maintain_transforms() >= 2);
    assert_eq!(s.check_values(), Ok(()));
    assert_eq!(s.get::<Transform>(b).unwrap().chunk, [0; 3]);
    pe_sim::Snapshot::from_bytes(&s.snapshot().to_bytes(), s.registry()).unwrap();
}

/// Review P21-1: loading a deep chain is linear, not quadratic.
#[test]
fn deep_chain_loads_in_linear_time() {
    let mut s = new_store();
    let chain: Vec<ObjectId> = (0..20_000).map(|_| s.mk(1)).collect();
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
