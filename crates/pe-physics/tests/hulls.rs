//! The hull builder (CCD part 2 E). Every hull shape is built by the fork's quickhull, on each machine, from the same
//! points (a hull blob's: world-model.md 3.2), so what it builds must be the same on every platform. A digest of the
//! hulls it builds from fixed inputs, degenerate ones too, is a RESULT line that tier 2 compares across its four
//! platforms, and a golden here. The inputs are literals and square roots (correctly rounded everywhere), never
//! trigonometry.

use box3d_rust::body::body_get_shapes;
use box3d_rust::hull::{HullData, get_hull_planes, get_hull_points};
use box3d_rust::shape::shape_get_hull;
use pe_physics::{BodyDesc, BodyKind, Geometry, ObjectId, PhysicsConfig, PhysicsWorld, ShapeDesc, check_hull};

fn id(n: u32) -> ObjectId {
    ObjectId::from_u128((13u128 << 32) | u128::from(n))
}

/// A box's corners: centre `c`, half extents `h`.
fn corners(c: [f32; 3], h: [f32; 3]) -> Vec<[f32; 3]> {
    (0..8u32)
        .map(|k| {
            let s = |bit: u32| if k & bit != 0 { 1.0 } else { -1.0 };
            [c[0] + s(1) * h[0], c[1] + s(2) * h[1], c[2] + s(4) * h[2]]
        })
        .collect()
}

/// `r` times each integer direction (i, j, k) of the 5 x 5 x 5 cube's surface (98 of them), normalized, in loop order:
/// points round a sphere, every one on the hull.
fn shell(r: f32) -> Vec<[f32; 3]> {
    let mut points = Vec::new();
    for i in -2i32..=2 {
        for j in -2i32..=2 {
            for k in -2i32..=2 {
                if i.abs().max(j.abs()).max(k.abs()) == 2 {
                    let (x, y, z) = (i as f32, j as f32, k as f32);
                    let l = (x * x + y * y + z * z).sqrt();
                    points.push([r * x / l, r * y / l, r * z / l]);
                }
            }
        }
    }
    points
}

/// An input: its name, its points, and the vertex count it must build to (`None`: refused).
type Input = (&'static str, Vec<[f32; 3]>, Option<i32>);

fn inputs() -> Vec<Input> {
    let cube = corners([0.0; 3], [0.1; 3]);
    let mut centred = cube.clone();
    centred.push([0.0; 3]);
    let twice: Vec<[f32; 3]> = cube.iter().chain(cube.iter()).copied().collect();
    let near: Vec<[f32; 3]> = cube.iter().flat_map(|&p| [p, [p[0] + 1.0e-4, p[1], p[2] - 1.0e-4]]).collect();
    let octagon =
        [[2.0, 1.0], [1.0, 2.0], [-1.0, 2.0], [-2.0, 1.0], [-2.0, -1.0], [-1.0, -2.0], [1.0, -2.0], [2.0, -1.0]];
    let prism: Vec<[f32; 3]> =
        [-0.1f32, 0.1].iter().flat_map(|&y| octagon.map(|[x, z]| [0.05 * x, y, 0.05 * z])).collect();
    let mut grid = Vec::new();
    for i in 0..4 {
        for j in 0..4 {
            for k in 0..4 {
                if [i, j, k].iter().any(|&c| c == 0 || c == 3) {
                    grid.push([0.1 * i as f32, 0.1 * j as f32, 0.1 * k as f32]);
                }
            }
        }
    }
    vec![
        ("a tetrahedron", vec![[0.1, 0.1, 0.1], [0.1, -0.1, -0.1], [-0.1, 0.1, -0.1], [-0.1, -0.1, 0.1]], Some(4)),
        ("a cube and its centre", centred, Some(8)),
        ("a cube's corners twice", twice, Some(8)),
        ("a cube's corners, each with a copy 0.1 mm off", near, Some(12)),
        ("an octagonal prism (faces of 4 and 8 sides)", prism, Some(16)),
        ("a sliver 1 mm thick", corners([0.0; 3], [0.02, 0.0005, 0.02]), Some(8)),
        ("a sliver 0.01 mm thick", corners([0.0; 3], [0.02, 0.000_005, 0.02]), Some(8)),
        (
            "six points on a plane",
            vec![[0.0; 3], [0.1, 0.0, 0.0], [0.1, 0.0, 0.1], [0.0, 0.0, 0.1], [0.05, 0.0, 0.2], [-0.05, 0.0, 0.05]],
            None,
        ),
        ("four points on a line", vec![[0.0; 3], [0.1, 0.1, 0.1], [0.2, 0.2, 0.2], [0.3, 0.3, 0.3]], None),
        ("a cube 100 m from its body", corners([100.0, 0.0, 0.0], [0.1; 3]), Some(8)),
        ("a grid's 56 surface points", grid, Some(8)),
        ("48 points round a ball", shell(0.5)[..48].to_vec(), Some(44)),
        ("98 points round a ball", shell(0.5), Some(44)),
    ]
}

/// What the builder made: its counts, measures and arrays, bit for bit.
fn digest(hull: &HullData) -> blake3::Hash {
    let mut h = blake3::Hasher::new();
    let mut f = |x: f32| {
        h.update(&x.to_bits().to_le_bytes());
    };
    for p in get_hull_points(hull) {
        f(p.x);
        f(p.y);
        f(p.z);
    }
    for q in get_hull_planes(hull) {
        f(q.normal.x);
        f(q.normal.y);
        f(q.normal.z);
        f(q.offset);
    }
    for x in [hull.volume, hull.surface_area, hull.inner_radius, hull.center.x, hull.center.y, hull.center.z] {
        f(x);
    }
    let i = hull.central_inertia;
    for v in [i.cx, i.cy, i.cz, hull.aabb.lower_bound, hull.aabb.upper_bound] {
        f(v.x);
        f(v.y);
        f(v.z);
    }
    for n in [hull.vertex_count, hull.edge_count, hull.face_count] {
        h.update(&n.to_le_bytes());
    }
    h.update(&hull.hash.to_le_bytes());
    h.finalize()
}

/// The golden: the first 16 hex digits of the digest of every input's outcome (the RESULT lines below).
const GOLDEN: &str = "46d02caf1c437c1b";

/// The hull builder builds the same hulls from the same points on every platform: each input's outcome is a RESULT
/// line, and their digest the golden. Planar and collinear points are refused; copies 0.1 mm off and a sliver 0.01 mm
/// thick are built as given (Box3D welds neither). A hull of triangles too big for Box3D's 128 edges is built again at
/// 44 vertices (@coordinator-opus). [`check_hull`] and the build take the same path.
#[test]
fn golden_the_hull_builder_builds_the_same_hulls_on_every_platform() {
    let mut all = blake3::Hasher::new();
    let mut wrong = Vec::new();
    for (name, points, expect) in inputs() {
        let built = check_hull(&points).ok().map(|()| {
            let mut pw = PhysicsWorld::new(PhysicsConfig::engine()).unwrap();
            let hull = ShapeDesc::new(Geometry::Hull { points: points.clone() });
            pw.add_body(id(1), &BodyDesc::new(BodyKind::Static, [0.0; 3]).with_shape(hull)).unwrap();
            let shape = body_get_shapes(pw.raw(), pw.body_id(id(1)).unwrap(), 1)[0];
            let hull = shape_get_hull(pw.raw(), shape).unwrap();
            (hull.vertex_count, hull.face_count, digest(hull))
        });
        let outcome = match built {
            None => format!("refused ({:?})", check_hull(&points).unwrap_err()),
            Some((v, f, d)) => format!("{v} vertices, {f} faces, {}", &d.to_hex()[..16]),
        };
        if built.map(|(v, _, _)| v) != expect {
            wrong.push(format!("{name}: {outcome}, expected {expect:?} vertices"));
        }
        all.update(name.as_bytes());
        all.update(outcome.as_bytes());
        println!("RESULT hull builder, {name}: {outcome}");
    }
    let total = all.finalize().to_hex().to_string();
    println!("RESULT hull builder, all: {}", &total[..16]);
    assert!(wrong.is_empty(), "{wrong:#?}");
    assert_eq!(&total[..16], GOLDEN, "the hull builder's output changed");
}
