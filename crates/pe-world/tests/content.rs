//! Domain content and the home domain (pe-world `content`, `preset::home`; design domain-eng d1, 1.2-1.4): content
//! round-trips through its bytes and through a world, a home is deterministic, and malformed bytes never panic.

use pe_core::{LineId, MemberKey, Offset, Pos};
use pe_sim::{ComponentId, Transform};
use pe_world::components::{Body, Hand, Name, body_kind};
use pe_world::content::{Content, ContentError, Item, MAGIC};
use pe_world::edit::{Actor, MAX_EDIT_BYTES, Target};
use pe_world::preset::home::{self, Home, home_on, starter};
use pe_world::preset::{Preset, Session, find, genesis_line, m1_session_with, plane_names};
use pe_world::{BlobStore, perm};

fn origin() -> Pos {
    Pos::ORIGIN.checked_add(Offset([3.0, 0.0, -2.0])).unwrap()
}

fn line() -> LineId {
    genesis_line(b"a home's record")
}

fn home(content: &Content) -> Home {
    home_on(line(), 5, BlobStore::default(), origin(), content).expect("the home")
}

fn name_of(c: &Content, item: &Item) -> String {
    let (_, row) = item.rows.iter().find(|r| r.0 == pe_world::components::ids::NAME).expect("a name");
    let n: Name = bytemuck::pod_read_unaligned(row);
    String::from_utf8(c.blobs[&n.text.hash].clone()).unwrap()
}

#[test]
fn the_starter_set_is_top_level_physical_objects_in_one_row() {
    let c = starter();
    let names: Vec<String> = c.tops().map(|i| name_of(&c, &c.items[i as usize])).collect();
    let n = plane_names::BOX;
    assert_eq!(names[0], n, "{names:?}");
    assert!(names.contains(&plane_names::STACK.to_string()), "{names:?}");
    assert_eq!(names.len(), 7, "box, sphere, capsule, stack and the three heavy objects: {names:?}");
    // Every object with a shape is dynamic and grabbable (physics on by default).
    for it in &c.items {
        if let Some((_, b)) = it.rows.iter().find(|r| r.0 == pe_world::components::ids::BODY) {
            let b: Body = bytemuck::pod_read_unaligned(b);
            assert_eq!((b.kind, b.grabbable), (body_kind::DYNAMIC, 1));
        }
    }
}

#[test]
fn content_round_trips_through_its_bytes() {
    let c = starter();
    let bytes = c.encode();
    assert!(bytes.starts_with(MAGIC));
    assert_eq!(Content::decode(&bytes).unwrap(), c);
    assert_eq!(Content::decode(&bytes).unwrap().encode(), bytes, "canonical");
}

#[test]
fn malformed_bytes_are_refused_never_a_panic() {
    let bytes = starter().encode();
    for n in 0..bytes.len() {
        assert!(Content::decode(&bytes[..n]).is_err(), "a prefix of {n} bytes decoded");
    }
    let mut long = bytes.clone();
    long.push(0);
    assert_eq!(Content::decode(&long), Err(ContentError::Malformed("trailing bytes")));
    let mut magic = bytes.clone();
    magic[0] ^= 1;
    assert_eq!(Content::decode(&magic), Err(ContentError::Malformed("magic")));
    // Flipping any one byte either decodes to other content or is refused; it never panics.
    for i in 0..bytes.len() {
        let mut b = bytes.clone();
        b[i] ^= 0x5a;
        let _ = Content::decode(&b);
    }
}

#[test]
fn a_home_is_deterministic_and_holds_its_scenery_and_content() {
    let (a, b) = (home(&starter()), home(&starter()));
    assert_eq!(a.world.root(), b.world.root());
    assert_eq!(a.scenery, b.scenery);
    assert_ne!(a.world.root(), home(&Content::default()).world.root(), "the content is in the root");
    let w = &a.world;
    for n in [home::names::FLOOR, home::names::LAMP, home::names::SETTINGS] {
        let id = find(w, n).unwrap_or_else(|| panic!("{n}"));
        assert!(a.scenery.contains(&id), "{n}");
    }
    let floor = find(w, home::names::FLOOR).unwrap();
    assert_eq!(w.store().get::<Body>(floor).map(|b| b.kind), Some(body_kind::STATIC), "the floor is explicitly static");
    let roots = w.content_roots(&a.scenery);
    assert_eq!(roots.len(), starter().tops().count());
    // The box sits where the starter set put it, relative to the origin.
    let bx = find(w, plane_names::BOX).unwrap();
    let t = w.store().get::<Transform>(bx).unwrap();
    let d = t.root_pos().unwrap() - origin();
    assert!((d.0[2] - home::STARTER_ROW_Z).abs() < 1e-12, "{d:?}");
}

/// Each top-level object's subtree as (name, rows without the top's transform, top's offset), sorted by name: what a
/// save keeps, independent of the order and ids the world gave the objects.
type Rows = Vec<(ComponentId, Vec<u8>)>;

fn summary(c: &Content) -> Vec<(String, Vec<Rows>, [f64; 3])> {
    let tops: Vec<u32> = c.tops().collect();
    let mut out = Vec::new();
    for (t, &top) in tops.iter().enumerate() {
        let end = tops.get(t + 1).copied().unwrap_or(c.items.len() as u32);
        let mut rows = Vec::new();
        let mut at = [0.0; 3];
        for i in top..end {
            let mut r = c.items[i as usize].rows.clone();
            if i == top {
                let k = r.iter().position(|x| x.0 == ComponentId::TRANSFORM).unwrap();
                let t: Transform = bytemuck::pod_read_unaligned(&r.remove(k).1);
                at = t.pos;
            }
            rows.push(r);
        }
        out.push((name_of(c, &c.items[top as usize]), rows, at));
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

fn close(a: [f64; 3], b: [f64; 3]) -> bool {
    a.iter().zip(b).all(|(x, y)| (x - y).abs() < 1e-9)
}

#[test]
fn saving_a_home_and_opening_it_again_keeps_every_object() {
    let first = home(&starter());
    let saved = Content::from_world(&first.world, &first.world.content_roots(&first.scenery), origin()).unwrap();
    assert_eq!(saved.skipped_rows, 0);
    assert_eq!(saved.items.len(), starter().items.len());
    let again = home(&Content::decode(&saved.encode()).unwrap());
    let resaved = Content::from_world(&again.world, &again.world.content_roots(&again.scenery), origin()).unwrap();
    let (a, b, s) = (summary(&saved), summary(&resaved), summary(&starter()));
    assert_eq!(a.len(), s.len());
    for ((x, y), z) in a.iter().zip(&b).zip(&s) {
        assert_eq!((&x.0, &x.1), (&y.0, &y.1), "the rows a save keeps are the rows it opens with");
        // The world keeps every row of the starter set; it may add its defaults (a `SimSize`, 3.5).
        assert_eq!(x.0, z.0);
        for (have, want) in x.1.iter().zip(&z.1) {
            assert!(want.iter().all(|r| have.contains(r)), "{}: {want:?} not within {have:?}", x.0);
        }
        assert!(close(x.2, y.2) && close(x.2, z.2), "{}: {:?} {:?} {:?}", x.0, x.2, y.2, z.2);
    }
}

#[test]
fn content_keeps_its_placement_relative_to_the_origin() {
    let first = home(&starter());
    let elsewhere = Pos::ORIGIN.checked_add(Offset([-40.0, 2.0, 11.0])).unwrap();
    let saved = Content::from_world(&first.world, &first.world.content_roots(&first.scenery), origin()).unwrap();
    let moved = home_on(line(), 5, BlobStore::default(), elsewhere, &saved).unwrap();
    for n in [plane_names::BOX, plane_names::STACK] {
        let p = |w: &pe_world::World, o: Pos| {
            let id = find(w, n).unwrap();
            w.store().get::<Transform>(id).unwrap().root_pos().unwrap() - o
        };
        let (a, b) = (p(&first.world, origin()).0, p(&moved.world, elsewhere).0);
        assert!(close(a, b), "{n}: {a:?} {b:?}");
    }
}

#[test]
fn a_members_hand_is_never_content() {
    let s = Session {
        seed: 5,
        origin: origin(),
        preset: Preset::Home,
        line: line(),
        rate: pe_seams::SimRate::new(60).unwrap(),
        gravity: [0.0, -9.81, 0.0],
        agree: vec![],
        members: vec![(MemberKey([7; 32]), perm::ADMIN | perm::EDIT)],
    };
    let starter = starter();
    let w = m1_session_with(&s, BlobStore::default(), Some(&starter)).unwrap();
    let hand_root = w
        .store()
        .roots()
        .find(|&r| w.store().subtree(r).iter().any(|&x| w.store().has::<Hand>(x)))
        .expect("the member's hands");
    assert!(!w.content_roots(&[]).contains(&hand_root));
    assert!(matches!(Content::from_world(&w, &[hand_root], origin()), Err(ContentError::MemberObject(_))));
}

#[test]
fn large_content_splits_into_edits_that_each_fit_one_entry() {
    // Many copies of the starter box, each top-level.
    let one = starter();
    let bx = one.items[0].clone();
    let mut c = Content { blobs: one.blobs.clone(), ..Content::default() };
    for k in 0..3000 {
        let mut it = bx.clone();
        let t = Transform::local([(k % 50) as f64 * 0.3, 0.1, -((k / 50) as f64) * 0.3]);
        let at = it.rows.iter().position(|r| r.0 == ComponentId::TRANSFORM).unwrap();
        it.rows[at].1 = bytemuck::bytes_of(&t).to_vec();
        c.items.push(it);
    }
    let w = home(&Content::default()).world;
    let txs = c
        .transactions(w.store().registry(), &BlobStore::default(), Target::Root, origin(), Actor::Person, "test")
        .unwrap();
    assert!(txs.len() > 1, "{} transaction(s)", txs.len());
    assert!(txs.iter().all(|t| t.encode().len() <= MAX_EDIT_BYTES));
    assert_eq!(txs.iter().map(|t| t.ops.len()).sum::<usize>(), 3000);
    let h = home(&c);
    assert_eq!(h.world.content_roots(&h.scenery).len(), 3000);
}

#[test]
fn restore_starter_set_puts_back_only_what_the_home_lost() {
    let first = home(&starter());
    assert_eq!(home::missing_starters(&first.world).items.len(), 0, "a new home lacks nothing");
    // Save without the box and the stack: those two come back, with the stack's boxes.
    let keep: Vec<_> = first
        .world
        .content_roots(&first.scenery)
        .into_iter()
        .filter(|&r| ![find(&first.world, plane_names::BOX), find(&first.world, plane_names::STACK)].contains(&Some(r)))
        .collect();
    let saved = Content::from_world(&first.world, &keep, origin()).unwrap();
    let lost = home(&saved);
    let missing = home::missing_starters(&lost.world);
    let names: Vec<String> = missing.tops().map(|i| name_of(&missing, &missing.items[i as usize])).collect();
    assert_eq!(names, [plane_names::BOX, plane_names::STACK]);
    assert_eq!(missing.items.len(), 2 + plane_names::STACK_BOXES);
    let restored = home(&{
        let mut c = saved.clone();
        let base = c.items.len() as u32;
        c.items.extend(missing.items.iter().cloned().map(|mut it| {
            it.parent = it.parent.map(|p| p + base);
            it
        }));
        c.blobs.extend(missing.blobs.clone());
        c
    });
    assert_eq!(home::missing_starters(&restored.world).items.len(), 0);
}

#[test]
fn an_item_whose_parent_is_before_the_latest_top_is_refused() {
    // Box (top), Sphere (top), then a child of the box: out of order.
    let c = starter();
    let mut bad = Content { blobs: c.blobs.clone(), ..Content::default() };
    bad.items.push(c.items[0].clone());
    bad.items.push(c.items[1].clone());
    let mut child = c.items[0].clone();
    child.parent = Some(0);
    bad.items.push(child);
    assert_eq!(Content::decode(&bad.encode()), Err(ContentError::Malformed("parent")));
}
