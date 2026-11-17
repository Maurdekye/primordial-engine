//! M1's "What the hash covers", pe-world's half
//! (@item:primordial-engine/m1-test-a-divergence-in-each-hashed-kind-is-foun): a divergence injected into each of
//! the five hashed kinds (fields, governance, claims, logic state, module versions), into a body's physics state and
//! a pending commit's attestations, and into each other hashed table (registry, cooldowns, tombstones, hand packs,
//! push windows), moves the root, and descending the two trees from the root, as a peer's walk
//! does, finds exactly the damaged leaf. The repair by rungs 1 and 2 is @diverge-eng's, on the same hook
//! (`pe_world::test_util`).

use pe_core::Hash;
use pe_seams::{DomainId, LeafKey, Nibbles, Node, Tree, TreePath};
use pe_world::World;
use pe_world::test_util::{Kind, divergence, five_kinds};

const D: DomainId = DomainId(7);

fn node(w: &World, path: TreePath) -> Node {
    w.tree(D).node(&path).expect("a node the tree has")
}

/// The leaves where `a` and `b` differ, found from the root down by comparing child hashes, as a peer's walk does.
fn differing_leaves(a: &World, b: &World) -> Vec<LeafKey> {
    let mut out = Vec::new();
    let (
        Node::World { config: ca, tables: ta, domains: da, .. },
        Node::World { config: cb, tables: tb, domains: db, .. },
    ) = (node(a, TreePath::World), node(b, TreePath::World))
    else {
        panic!("the world root is a World node");
    };
    if ca != cb {
        out.push(LeafKey::Config);
    }
    if ta != tb {
        let (Node::Tables(xa), Node::Tables(xb)) = (node(a, TreePath::Tables), node(b, TreePath::Tables)) else {
            panic!("the tables root is a Tables node");
        };
        assert_eq!(xa.len(), xb.len());
        out.extend((0..xa.len()).filter(|&i| xa[i] != xb[i]).map(|i| LeafKey::Table(i as u8)));
    }
    assert_eq!(da.iter().map(|d| d.0).collect::<Vec<_>>(), db.iter().map(|d| d.0).collect::<Vec<_>>());
    for ((dom, ha), (_, hb)) in da.iter().zip(&db) {
        if ha == hb {
            continue;
        }
        let (Node::Domain { objects: oa, physics: pa, .. }, Node::Domain { objects: ob, physics: pb, .. }) =
            (node(a, TreePath::Domain(*dom)), node(b, TreePath::Domain(*dom)))
        else {
            panic!("a domain root is a Domain node");
        };
        if pa != pb {
            out.push(LeafKey::Physics(*dom));
        }
        if oa != ob {
            objects(a, b, Nibbles::default(), &mut out);
        }
    }
    out
}

/// Descends both objects tries under `prefix` where their hashes differ. The damage here never adds or removes an
/// object, so both tries have the same shape and only hashes differ.
fn objects(a: &World, b: &World, prefix: Nibbles, out: &mut Vec<LeafKey>) {
    let path = TreePath::Objects { domain: D, prefix };
    match (node(a, path.clone()), node(b, path)) {
        (Node::Leaf { id, object_hash: x }, Node::Leaf { id: idb, object_hash: y }) => {
            assert_eq!(id, idb);
            if x != y {
                out.push(LeafKey::Object(id));
            }
        }
        (Node::Inner { prefix: p, children: ca }, Node::Inner { prefix: pb, children: cb }) => {
            assert_eq!(p, pb);
            assert_eq!(ca.len(), cb.len());
            for ((n, x), (nb, y)) in ca.iter().zip(&cb) {
                assert_eq!(n, nb);
                if x != y {
                    objects(a, b, p.child(*n).expect("a child nibble"), out);
                }
            }
        }
        (x, y) => panic!("trie shapes differ: {x:?} against {y:?}"),
    }
}

#[test]
fn an_undamaged_twin_has_the_same_root_and_no_differing_leaf() {
    let w = five_kinds();
    let twin = w.clone();
    assert_eq!(w.root_hash(D), twin.root_hash(D));
    assert!(differing_leaves(&w, &twin).is_empty());
}

#[test]
fn a_divergence_in_each_hashed_kind_is_found_at_exactly_its_leaf() {
    let base = five_kinds();
    let r0 = base.root_hash(D);
    let mut roots: Vec<Hash> = vec![r0];
    for kind in Kind::ALL {
        let mut twin = base.clone();
        let leaf = divergence(&mut twin, kind).unwrap_or_else(|| panic!("{kind:?}: the fixture has one"));
        let r = twin.root_hash(D);
        assert_ne!(r, r0, "{kind:?}: the root covers it");
        assert_eq!(differing_leaves(&base, &twin), vec![leaf], "{kind:?}: the walk names exactly the damaged leaf");
        // The store's writes drop a damaged object's cached leaf, so a twin cached before the damage still finds it
        // (this shows writes invalidate the cache; a cached leaf that no longer matches its rows is
        // `verify_tree_cache`'s, R-H3).
        let mut cached = base.clone();
        cached.refresh_tree_cache();
        assert_eq!(divergence(&mut cached, kind), Some(leaf));
        assert_eq!(cached.root_hash(D), r, "{kind:?}: the cached root equals the root from scratch");
        assert!(cached.verify_tree_cache(D).is_ok(), "{kind:?}");
        roots.push(r);
    }
    let mut distinct = roots.clone();
    distinct.sort_by_key(|h| h.0);
    distinct.dedup();
    assert_eq!(distinct.len(), roots.len(), "each kind's damage gives its own root");
}

#[test]
fn the_modules_table_is_in_the_root() {
    // Module versions explicitly (M1 'What the hash covers'): the modules table's record, not only an import's
    // root equality, moves the root and is the one table that differs.
    let base = five_kinds();
    let mut twin = base.clone();
    assert_eq!(divergence(&mut twin, Kind::ModuleVersion), Some(LeafKey::Table(pe_world::tree::table::MODULES)));
    assert_ne!(twin.root_hash(D), base.root_hash(D));
    assert_ne!(twin.modules(), base.modules());
}

#[test]
fn a_world_without_a_kind_reports_none() {
    let mut empty = World::new(1, pe_world::BlobStore::default());
    for kind in Kind::ALL {
        assert_eq!(divergence(&mut empty, kind), None, "{kind:?}");
    }
}

/// Every hashed table has a kind that damages it (@coordinator-opus item 2), so a table added later
/// fails here until it has one. `LINES` is the exception: it holds no entries yet, so nothing in it can diverge.
#[test]
fn every_hashed_table_has_a_kind() {
    use pe_world::tree::table;
    let base = five_kinds();
    let mut reached: Vec<u8> = Kind::ALL
        .iter()
        .filter_map(|&k| match divergence(&mut base.clone(), k) {
            Some(LeafKey::Table(t)) => Some(t),
            _ => None,
        })
        .collect();
    reached.sort();
    reached.dedup();
    let want: Vec<u8> = (0..table::COUNT as u8).filter(|&t| t != table::LINES).collect();
    assert_eq!(reached, want);
}
