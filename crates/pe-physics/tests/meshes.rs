//! Meshes (world-model.md 3.2; CCD part 2 C): pe-physics's mesh colliders, their mass, their back (both sides), their
//! blobs, and how they hold through images, transfers, worker counts, bullets and holding.

use box3d_rust::body::{body_get_mass_data, body_get_shape_count};
use pe_physics::{
    BodyDesc, BodyKind, Geometry, Hit, Mesh, ObjectId, PhysicsConfig, PhysicsError, PhysicsWorld, Pose, ShapeDesc,
    Transfer, hull_points_from_blob,
};

const DT: f32 = 1.0 / 60.0;

fn id(n: u32) -> ObjectId {
    ObjectId::from_u128((7u128 << 32) | u128::from(n))
}

/// A box's 8 corners and 12 triangles, each counter-clockwise seen from outside: half extents `h`, centred at `c`.
fn box_parts(c: [f32; 3], h: [f32; 3]) -> (Vec<[f32; 3]>, Vec<[u32; 3]>) {
    let vertices = (0..8u32)
        .map(|k| {
            let s = |bit: u32| if k & bit != 0 { 1.0 } else { -1.0 };
            [c[0] + s(1) * h[0], c[1] + s(2) * h[1], c[2] + s(4) * h[2]]
        })
        .collect();
    let triangles = vec![
        [0, 2, 1],
        [1, 2, 3],
        [4, 5, 6],
        [5, 7, 6],
        [0, 1, 4],
        [1, 5, 4],
        [2, 6, 3],
        [3, 6, 7],
        [0, 4, 2],
        [2, 4, 6],
        [1, 3, 5],
        [3, 7, 5],
    ];
    (vertices, triangles)
}

fn box_mesh(c: [f32; 3], h: [f32; 3]) -> Mesh {
    let (v, t) = box_parts(c, h);
    Mesh::new(&v, &t).unwrap()
}

/// A 4 m square of two triangles at y = 0, its front up (+y).
fn plane() -> Mesh {
    Mesh::new(&[[-2.0, 0.0, -2.0], [2.0, 0.0, -2.0], [2.0, 0.0, 2.0], [-2.0, 0.0, 2.0]], &[[0, 3, 2], [0, 2, 1]])
        .unwrap()
}

fn mesh_shape(mesh: &Mesh, density: f32) -> ShapeDesc {
    ShapeDesc::new(Geometry::Mesh { mesh: mesh.clone(), scale: [1.0; 3], both_sides: false }).with_density(density)
}

fn ball(at: [f64; 3], radius: f32, velocity: [f32; 3]) -> BodyDesc {
    BodyDesc::new(BodyKind::Dynamic, at)
        .with_shape(ShapeDesc::new(Geometry::Sphere { center: [0.0; 3], radius }))
        .with_velocity(velocity, [0.0; 3])
}

/// A mesh v1 blob (world-model.md 3.2).
fn blob(vertices: &[[f32; 3]], triangles: &[[u32; 3]]) -> Vec<u8> {
    let mut b = b"PEM1".to_vec();
    b.extend_from_slice(&(vertices.len() as u32).to_le_bytes());
    for c in vertices.iter().flatten() {
        b.extend_from_slice(&c.to_le_bytes());
    }
    b.extend_from_slice(&(triangles.len() as u32).to_le_bytes());
    for i in triangles.iter().flatten() {
        b.extend_from_slice(&i.to_le_bytes());
    }
    b
}

/// A static 40 m box floor with its top at y = 0.
fn floor(pw: &mut PhysicsWorld, n: u32) {
    let ground = BodyDesc::new(BodyKind::Static, [0.0, -1.0, 0.0])
        .with_shape(ShapeDesc::new(Geometry::Box { half_extents: [20.0, 1.0, 20.0] }));
    pw.add_body(id(n), &ground).unwrap();
}

/// A closed mesh weighs the solid it encloses: the box's mass, centre and inertia, off its body's origin too.
#[test]
fn a_closed_mesh_body_weighs_the_box_it_encloses() {
    let mut pw = PhysicsWorld::new(PhysicsConfig::engine()).unwrap();
    let h = [0.5, 0.25, 0.1];
    let boxed = BodyDesc::new(BodyKind::Dynamic, [0.0, 5.0, 0.0])
        .with_shape(ShapeDesc::new(Geometry::Box { half_extents: h }).with_density(800.0));
    pw.add_body(id(1), &boxed).unwrap();
    let c = [0.3, -0.2, 0.1];
    pw.add_body(
        id(2),
        &BodyDesc::new(BodyKind::Dynamic, [3.0, 5.0, 0.0]).with_shape(mesh_shape(&box_mesh(c, h), 800.0)),
    )
    .unwrap();
    let (a, b) = (pw.mass(id(1)).unwrap(), pw.mass(id(2)).unwrap());
    assert!((a - 80.0).abs() < 1e-3 && (b - a).abs() <= 1e-5 * a, "box {a} kg, mesh {b} kg");
    let centre = pw.local_center(id(2)).unwrap();
    assert!((0..3).all(|k| (centre[k] - c[k]).abs() < 1e-6), "{centre:?}");
    let (ma, mb) = (
        body_get_mass_data(pw.raw(), pw.body_id(id(1)).unwrap()),
        body_get_mass_data(pw.raw(), pw.body_id(id(2)).unwrap()),
    );
    let (ia, ib) = (ma.inertia, mb.inertia);
    let scale = ia.cx.x.max(ia.cy.y).max(ia.cz.z);
    for (x, y) in [
        (ia.cx.x, ib.cx.x),
        (ia.cy.y, ib.cy.y),
        (ia.cz.z, ib.cz.z),
        (ia.cx.y, ib.cx.y),
        (ia.cx.z, ib.cx.z),
        (ia.cy.z, ib.cy.z),
    ] {
        assert!((x - y).abs() <= 1e-5 * scale, "inertia {x} against {y}");
    }
}

/// A dynamic body whose meshes give it no mass is refused before anything is made: an open mesh, one wound inward, a
/// closed one of density 0. A static or kinematic body takes any mesh, and an open mesh with a sphere weighs the
/// sphere, bit for bit.
#[test]
fn a_dynamic_body_with_no_mass_from_its_meshes_is_refused_and_nothing_changes() {
    let (v, t) = box_parts([0.0; 3], [0.5; 3]);
    let open = Mesh::new(&v, &t[..11]).unwrap();
    assert!(!open.is_closed() && !open.is_blob_closed());
    let inward: Vec<[u32; 3]> = t.iter().map(|&[a, b, c]| [a, c, b]).collect();
    let inward = Mesh::new(&v, &inward).unwrap();
    assert!(inward.is_closed());
    let closed = Mesh::new(&v, &t).unwrap();
    let mut pw = PhysicsWorld::new(PhysicsConfig::engine()).unwrap();
    floor(&mut pw, 1);
    let before = (pw.body_count(), pw.check_hash().unwrap());
    for (mesh, density) in [(&open, 1000.0), (&inward, 1000.0), (&closed, 0.0)] {
        let desc = BodyDesc::new(BodyKind::Dynamic, [0.0, 2.0, 0.0]).with_shape(mesh_shape(mesh, density));
        assert_eq!(pw.add_body(id(2), &desc), Err(PhysicsError::Massless { dropped: 0 }), "{mesh:?} at {density}");
        assert_eq!((pw.body_count(), pw.check_hash().unwrap()), before, "a refused call changes nothing");
    }
    assert!(PhysicsError::Massless { dropped: 0 }.to_string().contains("close the mesh, or add a primitive or a hull"));
    for (n, kind) in [(3, BodyKind::Static), (4, BodyKind::Kinematic)] {
        pw.add_body(id(n), &BodyDesc::new(kind, [5.0 * n as f64, 2.0, 0.0]).with_shape(mesh_shape(&open, 1000.0)))
            .unwrap();
    }
    let sphere = ShapeDesc::new(Geometry::Sphere { center: [0.1, 0.0, 0.0], radius: 0.3 }).with_density(1000.0);
    let both = BodyDesc::new(BodyKind::Dynamic, [9.0, 2.0, 0.0])
        .with_shape(sphere.clone())
        .with_shape(mesh_shape(&open, 1000.0));
    pw.add_body(id(5), &both).unwrap();
    pw.add_body(id(6), &BodyDesc::new(BodyKind::Dynamic, [12.0, 2.0, 0.0]).with_shape(sphere)).unwrap();
    assert_eq!(pw.mass(id(5)).unwrap().to_bits(), pw.mass(id(6)).unwrap().to_bits());
    assert_eq!(pw.local_center(id(5)).unwrap(), pw.local_center(id(6)).unwrap());
}

/// The golden sliver (world-model.md 3.2, @fable-reviewer): a closed box, off centre, whose +z face is a
/// fan of four triangles around a point `inset` in from one edge, so one triangle is a sliver 0.6 m long. Its mass,
/// centre and inertia, and its open or closed verdict, are bit-equal on every tier-2 platform (the RESULT lines) and
/// pinned here.
fn sliver_blob(inset: f32) -> Vec<u8> {
    let (c, h) = ([0.123, -0.456, 0.789], [0.3, 0.7, 0.11]);
    let (mut v, mut t) = box_parts(c, h);
    // The +z face is 4 5 7 6, counter-clockwise from outside; the fan's point is `inset` in from its edge 4-5.
    v.push([c[0], c[1] - h[1] + inset, c[2] + h[2]]);
    t.retain(|tri| *tri != [4, 5, 6] && *tri != [5, 7, 6]);
    t.extend([[4, 5, 8], [5, 7, 8], [7, 6, 8], [6, 4, 8]]);
    blob(&v, &t)
}

/// Mass, the centre's x y z, and the inertia's xx yy zz xy xz yz, as f32 bits.
const SLIVER_GOLDEN: [u32; 10] = [
    0x4329_8201,
    0x3dfb_e76c,
    0xbee9_78d7,
    0x3f49_fbe7,
    0x41e2_f5ae,
    0x40b8_9af8,
    0x4203_1608,
    0x8a89_dfcf,
    0xa3f4_999a,
    0xa3f4_999a,
];

#[test]
fn golden_a_closed_mesh_with_a_sliver_weighs_the_same_on_every_platform() {
    let density = 917.25;
    // Kept: the sliver's area, 0.6 m by 4 um over 2, is over Box3D's minimum (2.5e-7 m^2).
    let kept = Mesh::from_blob(&sliver_blob(4.0e-6)).unwrap();
    assert_eq!((kept.blob_triangle_count(), kept.triangle_count(), kept.dropped()), (14, 14, 0));
    assert!(kept.is_closed() && kept.is_blob_closed());
    let mut pw = PhysicsWorld::new(PhysicsConfig::engine()).unwrap();
    pw.add_body(id(1), &BodyDesc::new(BodyKind::Dynamic, [0.0; 3]).with_shape(mesh_shape(&kept, density))).unwrap();
    let m = body_get_mass_data(pw.raw(), pw.body_id(id(1)).unwrap());
    let i = m.inertia;
    let bits =
        [m.mass, m.center.x, m.center.y, m.center.z, i.cx.x, i.cy.y, i.cz.z, i.cx.y, i.cx.z, i.cy.z].map(f32::to_bits);
    let line: Vec<String> = bits.iter().map(|b| format!("{b:08x}")).collect();
    println!("RESULT mesh sliver golden, kept: closed {}, mass centre inertia {}", kept.is_closed(), line.join(" "));
    assert_eq!(bits, SLIVER_GOLDEN, "{m:?}");
    // Dropped: 0.2 um in, under the minimum, so Box3D leaves the sliver out, and the box is open without it.
    let dropped = Mesh::from_blob(&sliver_blob(2.0e-7)).unwrap();
    println!(
        "RESULT mesh sliver golden, dropped: built {} of {}, closed {}, closed as given {}",
        dropped.triangle_count(),
        dropped.blob_triangle_count(),
        dropped.is_closed(),
        dropped.is_blob_closed()
    );
    assert_eq!((dropped.triangle_count(), dropped.dropped()), (13, 1));
    assert!(!dropped.is_closed() && dropped.is_blob_closed());
    let refused = pw
        .add_body(id(2), &BodyDesc::new(BodyKind::Dynamic, [2.0, 0.0, 0.0]).with_shape(mesh_shape(&dropped, density)));
    assert_eq!(refused, Err(PhysicsError::Massless { dropped: 1 }));
    assert_eq!(
        refused.unwrap_err().to_string(),
        "1 of the mesh's triangles are under the minimum area and were left out; without them the mesh is open"
    );
}

/// A triangle with two equal positions is dropped too, and leaves a closed mesh closed: no refusal, the box's mass.
#[test]
fn a_dropped_triangle_that_opens_nothing_changes_nothing() {
    let (v, mut t) = box_parts([0.0; 3], [0.5; 3]);
    let whole = Mesh::new(&v, &t).unwrap();
    t.insert(4, [2, 2, 5]);
    let with = Mesh::new(&v, &t).unwrap();
    assert_eq!((with.dropped(), with.is_closed(), with.is_blob_closed()), (1, true, true));
    assert_eq!(with.mass([1.0; 3], 1000.0).to_bits(), whole.mass([1.0; 3], 1000.0).to_bits());
}

/// A mesh with both sides collides from behind too, as two Box3D shapes: the back shares its front's place in the
/// body's shapes (a hit names it), and adds no mass. Its place holds through an image and a transfer, which keep the
/// back's name.
#[test]
fn a_mesh_with_both_sides_collides_from_behind_too_and_keeps_its_place() {
    let config = PhysicsConfig { gravity: [0.0; 3], ..PhysicsConfig::engine() };
    for both_sides in [false, true] {
        let mut pw = PhysicsWorld::new(config).unwrap();
        // One static body: a sphere far off, the plane, and a capsule far off the other way.
        let body = BodyDesc::new(BodyKind::Static, [0.0; 3])
            .with_shape(ShapeDesc::new(Geometry::Sphere { center: [10.0, 0.0, 0.0], radius: 0.5 }))
            .with_shape(ShapeDesc::new(Geometry::Mesh { mesh: plane(), scale: [1.0; 3], both_sides }))
            .with_shape(ShapeDesc::new(Geometry::Capsule {
                center1: [-10.0, 0.0, 0.0],
                center2: [-10.0, 1.0, 0.0],
                radius: 0.5,
            }));
        pw.add_body(id(1), &body).unwrap();
        assert_eq!(body_get_shape_count(pw.raw(), pw.body_id(id(1)).unwrap()), 3 + i32::from(both_sides));
        pw.add_body(id(2), &ball([0.0, -1.0, 0.0], 0.1, [0.0, 4.0, 0.0])).unwrap();
        pw.add_body(id(3), &ball([1.0, 1.0, 0.0], 0.1, [0.0, -4.0, 0.0])).unwrap();
        pw.add_body(id(4), &ball([-10.0, 3.0, 0.0], 0.1, [0.0, -4.0, 0.0])).unwrap();
        let image = pw.save_image().unwrap();
        let transfer = pw.save_transfer().unwrap();
        let run = |pw: &mut PhysicsWorld| {
            let mut hits: Vec<Hit> = Vec::new();
            for _ in 0..60 {
                pw.step(DT).unwrap();
                hits.extend(pw.hits());
            }
            (hits, [2, 3, 4].map(|n| pw.pose(id(n)).unwrap().position[1]))
        };
        let (hits, [below, above, _]) = run(&mut pw);
        assert!(above > 0.0, "from the front, held above ({above})");
        if both_sides {
            assert!(below < 0.0, "from behind, held below ({below})");
        } else {
            assert!(below > 1.0, "one-sided: it passed through from behind ({below})");
        }
        // Every hit names the static body's shape by its place: the plane 1, front or back, the capsule 2.
        let place = |h: &Hit, n: u32| {
            if h.a == id(1) { (h.b == id(n)).then_some(h.shape_a) } else { (h.a == id(n)).then_some(h.shape_b) }
        };
        let on = |n: u32| hits.iter().filter_map(|h| place(h, n)).collect::<Vec<u32>>();
        assert!(!on(3).is_empty() && on(3).iter().all(|&s| s == 1), "{:?}", on(3));
        assert!(!on(4).is_empty() && on(4).iter().all(|&s| s == 2), "{:?}", on(4));
        if both_sides {
            assert!(!on(2).is_empty() && on(2).iter().all(|&s| s == 1), "{:?}", on(2));
        }
        pw.restore_image(&image).unwrap();
        assert_eq!(run(&mut pw).0, hits, "after a restore");
        let mut joined = PhysicsWorld::from_transfer(config, &transfer, &transfer.check_hash()).unwrap();
        assert_eq!(run(&mut joined).0, hits, "on a joiner");
    }
}

/// Both sides keep the mesh's own mass, bit for bit: the back has no density.
#[test]
fn a_mesh_with_both_sides_weighs_its_front_only() {
    let mesh = box_mesh([0.2, 0.0, 0.0], [0.4, 0.3, 0.2]);
    let mut pw = PhysicsWorld::new(PhysicsConfig::engine()).unwrap();
    for (n, both_sides) in [(1, false), (2, true)] {
        let shape =
            ShapeDesc::new(Geometry::Mesh { mesh: mesh.clone(), scale: [1.0; 3], both_sides }).with_density(700.0);
        pw.add_body(id(n), &BodyDesc::new(BodyKind::Dynamic, [3.0 * n as f64, 2.0, 0.0]).with_shape(shape)).unwrap();
    }
    let (one, two) = (
        body_get_mass_data(pw.raw(), pw.body_id(id(1)).unwrap()),
        body_get_mass_data(pw.raw(), pw.body_id(id(2)).unwrap()),
    );
    assert_eq!(format!("{one:?}"), format!("{two:?}"));
    assert!(one.mass > 0.0);
    // Wound inward, its front has none, and its back adds none.
    let (v, t) = box_parts([0.0; 3], [0.5; 3]);
    let inward: Vec<[u32; 3]> = t.iter().map(|&[a, b, c]| [a, c, b]).collect();
    let shape =
        ShapeDesc::new(Geometry::Mesh { mesh: Mesh::new(&v, &inward).unwrap(), scale: [1.0; 3], both_sides: true });
    let refused = pw.add_body(
        id(3),
        &BodyDesc::new(BodyKind::Dynamic, [9.0, 2.0, 0.0]).with_shape(shape.clone().with_density(700.0)),
    );
    assert_eq!(refused, Err(PhysicsError::Massless { dropped: 0 }));
    // With a sphere it builds, and weighs the sphere alone: the back, wound outward here, has no density.
    let sphere = ShapeDesc::new(Geometry::Sphere { center: [0.0; 3], radius: 0.3 }).with_density(700.0);
    let with = BodyDesc::new(BodyKind::Dynamic, [12.0, 2.0, 0.0])
        .with_shape(sphere.clone())
        .with_shape(shape.with_density(700.0));
    pw.add_body(id(4), &with).unwrap();
    pw.add_body(id(5), &BodyDesc::new(BodyKind::Dynamic, [15.0, 2.0, 0.0]).with_shape(sphere)).unwrap();
    assert_eq!(pw.mass(id(4)).unwrap().to_bits(), pw.mass(id(5)).unwrap().to_bits());
}

/// A scale applies axis by axis, a mirror keeps the mass (and mirrors the centre), and an axis under 1e-6 in
/// magnitude is refused, whatever the body's kind (@coordinator-opus; 0.01 until E2), as
/// is a scaled vertex beyond the size limit. 1e-6 itself builds.
#[test]
fn a_mesh_scale_applies_per_axis_and_an_axis_under_the_floor_is_refused() {
    let (c, h) = ([0.3, -0.2, 0.1], [0.5, 0.25, 0.1]);
    let mesh = box_mesh(c, h);
    let mut pw = PhysicsWorld::new(PhysicsConfig::engine()).unwrap();
    let with = |kind, scale: [f32; 3]| {
        BodyDesc::new(kind, [0.0, 5.0, 0.0]).with_shape(
            ShapeDesc::new(Geometry::Mesh { mesh: mesh.clone(), scale, both_sides: false }).with_density(1000.0),
        )
    };
    pw.add_body(id(1), &with(BodyKind::Dynamic, [1.0; 3])).unwrap();
    let plain = pw.mass(id(1)).unwrap();
    pw.add_body(id(2), &with(BodyKind::Dynamic, [-1.0, 1.0, 1.0])).unwrap();
    assert!((pw.mass(id(2)).unwrap() - plain).abs() <= 1e-6 * plain);
    let centre = pw.local_center(id(2)).unwrap();
    assert!((centre[0] + c[0]).abs() < 1e-6 && (centre[1] - c[1]).abs() < 1e-6, "mirrored in x: {centre:?}");
    pw.add_body(id(3), &with(BodyKind::Dynamic, [2.0, 1.0, 0.5])).unwrap();
    assert!((pw.mass(id(3)).unwrap() - plain).abs() <= 1e-5 * plain, "2 x 1 x 0.5 keeps the volume");
    pw.add_body(id(4), &with(BodyKind::Dynamic, [0.01, -0.01, 1.0])).unwrap();
    // Under the old 0.01 floor: a dynamic body while its volume holds 10^-9 m^3, a static one down to the floor.
    pw.add_body(id(5), &with(BodyKind::Dynamic, [0.005, -0.005, 1.0])).unwrap();
    assert!((pw.mass(id(5)).unwrap() - 2.5e-5 * plain).abs() <= 1e-5 * 2.5e-5 * plain, "0.005 x 0.005 x 1");
    pw.add_body(id(6), &with(BodyKind::Static, [1.0e-6, 1.0, -1.0e-6])).unwrap();
    for kind in [BodyKind::Dynamic, BodyKind::Static] {
        for (scale, axis) in [([0.99e-6, 1.0, 1.0], "x"), ([1.0, -0.0, 1.0], "y"), ([1.0, 1.0, 0.0], "z")] {
            match pw.add_body(id(9), &with(kind, scale)) {
                Err(PhysicsError::InvalidValue(why)) => {
                    assert!(why.starts_with(&format!("mesh scale {axis} is under 1e-6 in magnitude")), "{why}");
                    assert!(why.ends_with("rescale the mesh at import"), "{why}");
                }
                other => panic!("{scale:?}: {other:?}"),
            }
        }
    }
    assert_eq!(
        pw.add_body(id(9), &with(BodyKind::Static, [f32::NAN, 1.0, 1.0])),
        Err(PhysicsError::InvalidValue("mesh scale"))
    );
    // x reaches 0.8 m; scaled by 3e4 that is 24 km, beyond the 10 km limit.
    assert_eq!(
        pw.add_body(id(9), &with(BodyKind::Static, [3.0e4, 1.0, 1.0])),
        Err(PhysicsError::InvalidValue("mesh vertex, scaled, beyond the size limit"))
    );
    assert!(!pw.has_body(id(9)));
}

/// A 6 x 6 grid of 1 m cells, 72 triangles, its front up, at y = 0 in its body.
fn grid() -> Mesh {
    let n = 7u32;
    let vertices: Vec<[f32; 3]> = (0..n * n).map(|k| [(k % n) as f32 - 3.0, 0.0, (k / n) as f32 - 3.0]).collect();
    let mut triangles = Vec::new();
    for z in 0..n - 1 {
        for x in 0..n - 1 {
            let a = z * n + x;
            triangles.extend([[a, a + n, a + n + 1], [a, a + n + 1, a + 1]]);
        }
    }
    Mesh::new(&vertices, &triangles).unwrap()
}

/// A scene of every kind of mesh body: a static mesh floor that balls and boxes land on, a dynamic closed mesh box on
/// a box floor, a kinematic mesh paddle turning through balls, and a both-sided static plane.
fn mesh_scene(config: PhysicsConfig) -> PhysicsWorld {
    let mut pw = PhysicsWorld::new(config).unwrap();
    floor(&mut pw, 1);
    pw.add_body(id(2), &BodyDesc::new(BodyKind::Static, [0.0, 1.0, 0.0]).with_shape(mesh_shape(&grid(), 0.0))).unwrap();
    for k in 0..12u32 {
        let at = [(k % 4) as f64 - 1.5, 2.0 + 0.4 * (k / 4) as f64, (k % 3) as f64 - 1.0];
        let shape = if k % 2 == 0 {
            ShapeDesc::new(Geometry::Sphere { center: [0.0; 3], radius: 0.2 })
        } else {
            ShapeDesc::new(Geometry::Box { half_extents: [0.15, 0.15, 0.15] })
        };
        let desc =
            BodyDesc::new(BodyKind::Dynamic, at).with_shape(shape).with_velocity([0.3, 0.0, -0.2], [0.0, 1.0, 0.0]);
        pw.add_body(id(10 + k), &desc).unwrap();
    }
    pw.add_body(
        id(3),
        &BodyDesc::new(BodyKind::Dynamic, [6.0, 0.6, 0.0]).with_shape(mesh_shape(&box_mesh([0.0; 3], [0.5; 3]), 300.0)),
    )
    .unwrap();
    pw.add_body(id(4), &ball([6.2, 2.0, 0.1], 0.2, [0.0; 3])).unwrap();
    let paddle = BodyDesc::new(BodyKind::Kinematic, [-6.0, 0.5, 0.0])
        .with_shape(mesh_shape(&box_mesh([0.8, 0.0, 0.0], [0.8, 0.2, 0.05]), 0.0))
        .with_velocity([0.0; 3], [0.0, 3.0, 0.0]);
    pw.add_body(id(5), &paddle).unwrap();
    for k in 0..3u32 {
        pw.add_body(id(30 + k), &ball([-6.0 + 0.5 * f64::from(k), 0.25, 1.0], 0.2, [0.0; 3])).unwrap();
    }
    let both = ShapeDesc::new(Geometry::Mesh { mesh: plane(), scale: [0.5, 1.0, 0.5], both_sides: true });
    pw.add_body(id(6), &BodyDesc::new(BodyKind::Static, [0.0, 4.0, 6.0]).with_shape(both)).unwrap();
    pw.add_body(id(7), &ball([0.0, 5.0, 6.0], 0.2, [0.0; 3])).unwrap();
    pw
}

/// Every worker count steps a mesh scene to the same state, frame by frame.
#[test]
fn mesh_scenes_step_the_same_on_any_worker_count() {
    let run = |workers| {
        let mut pw = mesh_scene(PhysicsConfig { workers, ..PhysicsConfig::engine() });
        (0..120)
            .map(|_| {
                pw.step(DT).unwrap();
                pw.state_hash().unwrap()
            })
            .collect::<Vec<u64>>()
    };
    let one = run(1);
    for workers in [2, 4, 8] {
        assert_eq!(run(workers), one, "{workers} workers");
    }
    // The scene did something: the falling bodies came to the mesh floor and the mesh box sits on its box floor.
    let mut pw = mesh_scene(PhysicsConfig::engine());
    for _ in 0..120 {
        pw.step(DT).unwrap();
    }
    for k in 0..12u32 {
        let y = pw.pose(id(10 + k)).unwrap().position[1];
        assert!((1.1..1.4).contains(&y), "body {k} at y = {y}");
    }
    let y = pw.pose(id(3)).unwrap().position[1];
    assert!((0.45..0.55).contains(&y), "the mesh box at y = {y}");
}

/// A mesh scene holds through an image and a transfer, frame by frame, and the transfer reads back to itself.
#[test]
fn mesh_scenes_hold_through_images_and_transfers() {
    let config = PhysicsConfig::engine();
    let mut pw = mesh_scene(config);
    for _ in 0..30 {
        pw.step(DT).unwrap();
    }
    let image = pw.save_image().unwrap();
    let transfer = pw.save_transfer().unwrap();
    let run = |pw: &mut PhysicsWorld| {
        (0..60)
            .map(|_| {
                pw.step(DT).unwrap();
                pw.state_hash().unwrap()
            })
            .collect::<Vec<u64>>()
    };
    let straight = run(&mut pw);
    pw.restore_image(&image).unwrap();
    assert_eq!(run(&mut pw), straight, "after a restore");
    assert_eq!(Transfer::from_bytes(&transfer.to_bytes()).unwrap(), transfer);
    let mut joined = PhysicsWorld::from_transfer(config, &transfer, &transfer.check_hash()).unwrap();
    assert_eq!(run(&mut joined), straight, "on a joiner");
}

/// A bullet never passes a dynamic mesh body, awake or asleep (CCD part 2 A's partners, on moving meshes): a 5 cm
/// steel ball at 300 m/s into a 1 m closed mesh box resting on a box floor. Not a bullet, it passes through.
#[test]
fn a_bullet_never_passes_a_moving_or_sleeping_mesh_body() {
    for (asleep, bullet) in [(false, true), (true, true), (false, false)] {
        let mut pw = PhysicsWorld::new(PhysicsConfig::engine()).unwrap();
        floor(&mut pw, 1);
        pw.add_body(
            id(2),
            &BodyDesc::new(BodyKind::Dynamic, [5.0, 0.5, 0.0])
                .with_shape(mesh_shape(&box_mesh([0.0; 3], [0.5; 3]), 200.0)),
        )
        .unwrap();
        if asleep {
            for _ in 0..600 {
                if !pw.is_awake(id(2)).unwrap() {
                    break;
                }
                pw.step(DT).unwrap();
            }
            assert!(!pw.is_awake(id(2)).unwrap(), "the mesh box sleeps");
        }
        let mut shot = BodyDesc::new(BodyKind::Dynamic, [0.0, 0.5, 0.0])
            .with_shape(ShapeDesc::new(Geometry::Sphere { center: [0.0; 3], radius: 0.05 }).with_density(7800.0))
            .with_velocity([300.0, 0.0, 0.0], [0.0; 3]);
        shot.bullet = bullet;
        pw.add_body(id(3), &shot).unwrap();
        // How far the bullet's centre gets past the box's centre.
        let mut farthest = f64::NEG_INFINITY;
        for _ in 0..30 {
            pw.step(DT).unwrap();
            let past = pw.pose(id(3)).unwrap().position[0] - pw.pose(id(2)).unwrap().position[0];
            farthest = farthest.max(past);
        }
        if bullet {
            assert!(farthest < -0.5, "asleep {asleep}: the bullet reached {farthest} m past the box's centre");
            assert!(pw.velocity(id(2)).unwrap().0[0] > 1.0, "the box was knocked");
        } else {
            assert!(farthest > 0.5, "not a bullet, it passes through ({farthest})");
        }
    }
}

/// Blobs that can't be read, and meshes that can't be built, are refused, never a panic.
#[test]
fn blobs_and_meshes_that_can_t_be_read_or_built_are_refused() {
    let (v, t) = box_parts([0.0; 3], [0.5; 3]);
    let good = blob(&v, &t);
    let read = Mesh::from_blob(&good).unwrap();
    assert!(read.is_closed());
    assert_eq!(read, Mesh::new(&v, &t).unwrap(), "the same build");
    let refused = |bytes: &[u8]| Mesh::from_blob(bytes).unwrap_err();
    let mut magic = good.clone();
    magic[3] = b'2';
    assert!(matches!(refused(&magic), PhysicsError::BadBlob(_)));
    assert!(matches!(refused(&good[..good.len() - 1]), PhysicsError::BadBlob(_)));
    let mut longer = good.clone();
    longer.push(0);
    assert!(matches!(refused(&longer), PhysicsError::BadBlob(_)));
    let mut huge = good.clone();
    huge[4..8].copy_from_slice(&u32::MAX.to_le_bytes());
    assert!(matches!(refused(&huge), PhysicsError::BadBlob(_)));
    let mut more = good.clone();
    let at = 8 + 12 * v.len();
    more[at..at + 4].copy_from_slice(&13u32.to_le_bytes());
    assert!(matches!(refused(&more), PhysicsError::BadBlob(_)));
    assert!(matches!(refused(&[]), PhysicsError::BadBlob(_)));
    let mut over = b"PEM1".to_vec();
    over.resize(960 * 1024 + 1, 0);
    assert_eq!(refused(&over), PhysicsError::BadBlob("a mesh blob over 960 KiB"));
    let mut out = t.clone();
    out[3] = [0, 1, 8];
    assert!(matches!(refused(&blob(&v, &out)), PhysicsError::BadMesh(_)));
    let mut nan = v.clone();
    nan[2] = [f32::NAN, 0.0, 0.0];
    assert_eq!(refused(&blob(&nan, &t)), PhysicsError::InvalidValue("mesh vertex"));
    let mut far = v.clone();
    far[2] = [2.0e4, 0.0, 0.0];
    assert_eq!(refused(&blob(&far, &t)), PhysicsError::InvalidValue("mesh vertex"));
    assert!(matches!(Mesh::new(&v[..2], &[[0, 1, 1]]), Err(PhysicsError::BadMesh(_))));
    assert!(matches!(Mesh::new(&v, &[]), Err(PhysicsError::BadMesh(_))));
    assert!(matches!(Mesh::new(&v, &[[0, 0, 1], [2, 2, 2]]), Err(PhysicsError::BadMesh(_))), "every triangle dropped");
    let big = vec![[0.0f32; 3]; 81_920];
    assert!(matches!(Mesh::new(&big, &[[0, 1, 2]]), Err(PhysicsError::BadMesh(_))), "over what a blob holds");

    // Hull v1: the magic PEH1, 4 to 64 points.
    let hull = |points: &[[f32; 3]]| {
        let mut b = b"PEH1".to_vec();
        b.extend_from_slice(&(points.len() as u32).to_le_bytes());
        for c in points.iter().flatten() {
            b.extend_from_slice(&c.to_le_bytes());
        }
        b
    };
    assert_eq!(hull_points_from_blob(&hull(&v)).unwrap(), v);
    for bad in [hull(&v[..3]), hull(&vec![[1.0, 2.0, 3.0]; 65]), hull(&v)[..20].to_vec(), b"PEM1\x08\0\0\0".to_vec()] {
        assert!(matches!(hull_points_from_blob(&bad), Err(PhysicsError::BadBlob(_))), "{} bytes", bad.len());
    }
}

/// A held mesh is held back by a wall like a box (world-model.md 3.2, "holding"): its capsule is the one around its
/// scaled vertices, with both sides or not.
#[test]
fn a_held_mesh_is_held_back_by_walls() {
    for both_sides in [false, true] {
        let mut pw = PhysicsWorld::new(PhysicsConfig::engine()).unwrap();
        let wall = BodyDesc::new(BodyKind::Static, [2.0, 1.0, 0.0])
            .with_shape(ShapeDesc::new(Geometry::Box { half_extents: [0.05, 1.0, 1.0] }));
        pw.add_body(id(1), &wall).unwrap();
        let shape =
            ShapeDesc::new(Geometry::Mesh { mesh: box_mesh([0.0; 3], [0.2; 3]), scale: [1.0, 2.0, 1.0], both_sides })
                .with_density(500.0);
        pw.add_body(id(2), &BodyDesc::new(BodyKind::Dynamic, [0.0, 1.0, 0.0]).with_shape(shape)).unwrap();
        let at = |x: f64, z: f64| Pose { position: [x, 1.0, z], rotation: [0.0, 0.0, 0.0, 1.0] };
        assert!(pw.static_in_way(id(2), &at(4.0, 0.0)).unwrap(), "through the wall");
        assert!(!pw.static_in_way(id(2), &at(1.5, 0.0)).unwrap(), "short of it (the wall's face at 1.95)");
        assert!(!pw.static_in_way(id(2), &at(0.0, 3.0)).unwrap(), "along it");
    }
}

/// A dynamic mesh body lands and rests from any height, on a static box floor and on a dynamic box (the fork's
/// moving-mesh triangle cache: Box3D found a mesh's triangles near the other shape only when that shape moved, so a
/// mesh body whose contact began 0.1 m up kept an empty list and fell through, box3d-fork FORK.md).
#[test]
fn a_dynamic_mesh_body_lands_from_any_height() {
    for drop in [0.02, 0.06, 0.1, 0.2, 0.5, 1.0, 2.0] {
        let mut pw = PhysicsWorld::new(PhysicsConfig::engine()).unwrap();
        floor(&mut pw, 1);
        let mesh = box_mesh([0.0; 3], [0.5; 3]);
        pw.add_body(
            id(2),
            &BodyDesc::new(BodyKind::Dynamic, [0.0, 0.5 + drop, 0.0]).with_shape(mesh_shape(&mesh, 300.0)),
        )
        .unwrap();
        // On a dynamic box, 1 m across, resting on the floor.
        let crate_ = ShapeDesc::new(Geometry::Box { half_extents: [0.5; 3] }).with_density(300.0);
        pw.add_body(id(3), &BodyDesc::new(BodyKind::Dynamic, [4.0, 0.5, 0.0]).with_shape(crate_)).unwrap();
        pw.add_body(
            id(4),
            &BodyDesc::new(BodyKind::Dynamic, [4.0, 1.5 + drop, 0.0]).with_shape(mesh_shape(&mesh, 300.0)),
        )
        .unwrap();
        for _ in 0..180 {
            pw.step(DT).unwrap();
        }
        let (on_floor, on_box) = (pw.pose(id(2)).unwrap().position[1], pw.pose(id(4)).unwrap().position[1]);
        assert!((0.49..0.52).contains(&on_floor), "dropped {drop} m onto the floor, at y = {on_floor}");
        assert!((1.49..1.53).contains(&on_box), "dropped {drop} m onto the box, at y = {on_box}");
    }
}

/// `BodyDesc::check_mass` is `add_body`'s mass check without a world (CCD part 2 D, for pe-replica's
/// `Unbuildable::Mass`): it refuses exactly what `add_body` refuses for want of mass, with the same dropped count, and
/// passes any body that isn't a dynamic one with a mesh, unweighed. A closed mesh's mass is summed once per scale and
/// density: asked again, the same bits.
#[test]
fn check_mass_is_add_body_s_mass_check() {
    let (v, t) = box_parts([0.0; 3], [0.5; 3]);
    let open = Mesh::new(&v, &t[..11]).unwrap();
    let closed = Mesh::new(&v, &t).unwrap();
    let mesh = |mesh: &Mesh| ShapeDesc::new(Geometry::Mesh { mesh: mesh.clone(), scale: [1.0; 3], both_sides: false });
    let body = |kind, shapes: Vec<ShapeDesc>| {
        shapes.into_iter().fold(BodyDesc::new(kind, [0.0, 2.0, 0.0]), |d, s| d.with_shape(s))
    };
    let sphere = ShapeDesc::new(Geometry::Sphere { center: [0.0; 3], radius: 0.1 });
    let cases = [
        (body(BodyKind::Dynamic, vec![mesh(&open)]), Err(PhysicsError::Massless { dropped: 0 })),
        (body(BodyKind::Dynamic, vec![mesh(&open), sphere.clone()]), Ok(())),
        (body(BodyKind::Dynamic, vec![mesh(&closed)]), Ok(())),
        (body(BodyKind::Static, vec![mesh(&open)]), Ok(())),
        (
            body(BodyKind::Dynamic, vec![mesh(&open), sphere.with_density(0.0)]),
            Err(PhysicsError::Massless { dropped: 0 }),
        ),
        (
            body(BodyKind::Dynamic, vec![mesh(&Mesh::from_blob(&sliver_blob(2.0e-7)).unwrap())]),
            Err(PhysicsError::Massless { dropped: 1 }),
        ),
    ];
    for (n, (desc, want)) in cases.into_iter().enumerate() {
        assert_eq!(desc.check_mass(), want, "case {n}");
        let mut pw = PhysicsWorld::new(PhysicsConfig::engine()).unwrap();
        assert_eq!(pw.add_body(id(1), &desc), want, "case {n}: add_body agrees");
    }
    let first = closed.mass([1.5, -1.0, 2.0], 700.0);
    for k in 0..20 {
        let _ = closed.mass([1.0 + k as f32, 1.0, 1.0], 700.0);
    }
    assert_eq!(first.to_bits(), closed.mass([1.5, -1.0, 2.0], 700.0).to_bits(), "summed again, the same bits");
    assert_eq!(first.to_bits(), closed.mass([1.5, -1.0, 2.0], 700.0).to_bits(), "kept, the same bits");
    assert!((closed.mass([1.5, -1.0, 2.0], 350.0) / first - 0.5).abs() < 1e-6, "each density its own");
    assert!((first / 2100.0 - 1.0).abs() < 1e-5, "a unit box scaled to 3 m3 at 700 kg/m3: {first}");
}

/// `check_hull` is a hull shape's checks and the fork's hull builder (CCD part 2 D, for a hull blob's points):
/// a solid's points pass; too few, flat, out of range or non-finite ones are refused.
#[test]
fn check_hull_refuses_what_a_hull_shape_refuses() {
    let solid = [[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]];
    assert_eq!(pe_physics::check_hull(&solid), Ok(()));
    assert_eq!(pe_physics::check_hull(&solid[..3]), Err(PhysicsError::BadHull));
    let flat = [[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 0.0, 1.0], [1.0, 0.0, 1.0]];
    assert_eq!(pe_physics::check_hull(&flat), Err(PhysicsError::BadHull));
    let far = [[0.0, 0.0, 0.0], [2.0e4, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]];
    assert!(matches!(pe_physics::check_hull(&far), Err(PhysicsError::InvalidValue(_))));
    let nan = [[0.0, 0.0, 0.0], [f32::NAN, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]];
    assert!(pe_physics::check_hull(&nan).is_err());
}

/// E2 (@coordinator-opus): a mesh scaled under the old 0.01 floor collides where it is drawn. Each case
/// is a static slab and a ball dropped on it; the ball comes to rest at the slab's top plus its radius plus Box3D's
/// mesh rest offset, 4.93 mm in world units whatever the sizes (a known limit, world-model.md 3.2). With the old floor
/// each scale here was refused; Box3D alone would have raised it to 0.01, and the millimetre slab would have been
/// 10 m wide and 1 m thick. The resting heights' bits are pinned (the same on every platform).
#[test]
fn a_mesh_scaled_under_the_old_floor_collides_where_it_is_drawn() {
    // A millimetre blob: 1 m x 10 cm x 1 m at 0.001.
    let mm = box_mesh([0.0; 3], [500.0, 50.0, 500.0]);
    // A 10 m room's floor, 20 cm thick, at 1/200.
    let room = box_mesh([0.0; 3], [5.0, 0.1, 5.0]);
    // (mesh, scale, the slab's top, the ball's radius)
    let cases: [(&Mesh, [f32; 3], f64, f32); 5] = [
        (&mm, [0.001; 3], 0.05, 0.05),
        (&mm, [-0.001, 0.001, 0.001], 0.05, 0.05),
        (&room, [0.005; 3], 0.0005, 0.005),
        // Flattened to the floor (a 1 m slab 0.1 mm thick) and mirrored through it.
        (&mm, [0.001, 1.0e-6, 0.001], 50.0e-6, 0.05),
        (&mm, [0.001, -1.0e-6, 0.001], 50.0e-6, 0.05),
    ];
    let mut bits = Vec::new();
    for (k, &(mesh, scale, top, radius)) in cases.iter().enumerate() {
        let mut pw = PhysicsWorld::new(PhysicsConfig::engine()).unwrap();
        let slab = ShapeDesc::new(Geometry::Mesh { mesh: mesh.clone(), scale, both_sides: false });
        pw.add_body(id(1), &BodyDesc::new(BodyKind::Static, [0.0; 3]).with_shape(slab)).unwrap();
        let drop = top + 4.0 * f64::from(radius);
        pw.add_body(id(2), &ball([0.1 * f64::from(radius), drop, 0.0], radius, [0.0; 3])).unwrap();
        for _ in 0..120 {
            pw.step(DT).unwrap();
        }
        let y = pw.pose(id(2)).unwrap().position[1];
        let gap = y - top - f64::from(radius);
        assert!((gap - 4.93e-3).abs() < 1.0e-4, "case {k} {scale:?}: {gap} above the drawn top, not 4.93 mm");
        println!("RESULT e2_scaled_mesh case={k} y_bits={:016x}", y.to_bits());
        bits.push(y.to_bits());
    }
    let golden = [0x3fbadcab5123a90c, 0x3fbadcab5123a90c, 0x3f855c224acd70f0, 0x3fac264a8ec428d0, 0x3fac264a8ec428d0];
    assert_eq!(bits, golden, "resting heights' bits");
}

/// A wave terrain of `n` x `n` quads over 2 m (amplitude 0.1 m), its front up.
fn terrain(n: u32) -> Mesh {
    let mut vertices = Vec::new();
    for i in 0..=n {
        for j in 0..=n {
            let (x, z) = (-1.0 + 2.0 * i as f32 / n as f32, -1.0 + 2.0 * j as f32 / n as f32);
            vertices.push([x, 0.1 * (3.0 * x).sin() * (2.5 * z).cos(), z]);
        }
    }
    let at = |i: u32, j: u32| i * (n + 1) + j;
    let mut triangles = Vec::new();
    for i in 0..n {
        for j in 0..n {
            triangles.push([at(i, j), at(i, j + 1), at(i + 1, j)]);
            triangles.push([at(i + 1, j), at(i, j + 1), at(i + 1, j + 1)]);
        }
    }
    Mesh::new(&vertices, &triangles).unwrap()
}

/// E2 (@coordinator-opus, D2): a dense mesh holds what lands on it. Box3D gathers a mesh contact's
/// triangles from the other shape's box widened by 7 cm in world units, and upstream keeps at most 512 of them, so on
/// this 4,608-triangle terrain at scale 0.1 (20 cm across, 4 mm triangles, a scale the floor always allowed) 7 of the
/// 9 balls fell through. The fork now keeps them all, in index order. The balls' final positions' bits are pinned.
#[test]
fn a_dense_mesh_holds_what_lands_on_it() {
    let s = 0.1f32;
    let mesh = terrain(48);
    let mut pw = PhysicsWorld::new(PhysicsConfig::engine()).unwrap();
    let ground = ShapeDesc::new(Geometry::Mesh { mesh, scale: [s; 3], both_sides: false });
    pw.add_body(id(1), &BodyDesc::new(BodyKind::Static, [0.0; 3]).with_shape(ground)).unwrap();
    let mut n = 2;
    for i in 0..3 {
        for j in 0..3 {
            let (x, z) = (-0.6 + 0.6 * f64::from(i), -0.6 + 0.6 * f64::from(j));
            pw.add_body(id(n), &ball([x * f64::from(s), 0.3 * f64::from(s), z * f64::from(s)], 0.02 * s, [0.0; 3]))
                .unwrap();
            n += 1;
        }
    }
    for _ in 0..120 {
        pw.step(DT).unwrap();
    }
    let mut bits = Vec::new();
    for k in 2..n {
        let p = pw.pose(id(k)).unwrap().position;
        let (x, z) = (p[0] / f64::from(s), p[2] / f64::from(s));
        if x.abs() <= 1.0 && z.abs() <= 1.0 {
            let surface = 0.1 * (3.0 * x).sin() * (2.5 * z).cos() * f64::from(s);
            assert!(p[1] > surface, "ball {k} at {p:?} is under the terrain's surface ({surface})");
        }
        bits.extend(p.map(f64::to_bits));
    }
    let digest = bits.iter().fold(0xcbf29ce484222325u64, |h, &b| (h ^ b).wrapping_mul(0x100000001b3));
    println!("RESULT e2_dense_mesh digest={digest:016x}");
    assert_eq!(digest, 0xde1d964bf585e701, "the balls' final positions");
}

/// E2 (@vdw-physics, L1): a dynamic mesh flattened far under the old floor. A closed 1 m box mesh at
/// 1e-4 or 1e-6 on one axis still encloses volume, so it weighs that slab and rests on a floor like one (upstream's
/// 5 mm mesh rest offset above it: a mesh pair); a 1 cm box mesh
/// at 1e-6 encloses 1e-12 m^3, under the fork's MIN_MESH_VOLUME, so it has no mass and the body is refused.
#[test]
fn a_dynamic_mesh_flattened_far_under_the_old_floor_weighs_its_slab_or_is_refused() {
    let metre = box_mesh([0.0; 3], [0.5; 3]);
    let centimetre = box_mesh([0.0; 3], [0.005; 3]);
    let flat = |mesh: &Mesh, sy: f32| {
        ShapeDesc::new(Geometry::Mesh { mesh: mesh.clone(), scale: [1.0, sy, 1.0], both_sides: false })
            .with_density(1000.0)
    };
    let mut pw = PhysicsWorld::new(PhysicsConfig::engine()).unwrap();
    floor(&mut pw, 1);
    let desc = BodyDesc::new(BodyKind::Dynamic, [0.0, 1.0, 0.0]).with_shape(flat(&centimetre, 1.0e-6));
    assert_eq!(pw.add_body(id(2), &desc), Err(PhysicsError::Massless { dropped: 0 }));
    let mut bits = Vec::new();
    for (n, sy) in [(3u32, 1.0e-4f32), (4, 1.0e-6)] {
        let at = [3.0 * f64::from(n), 0.1, 0.0];
        pw.add_body(id(n), &BodyDesc::new(BodyKind::Dynamic, at).with_shape(flat(&metre, sy))).unwrap();
        let mass = pw.mass(id(n)).unwrap();
        assert!((mass / (1000.0 * sy) - 1.0).abs() < 1.0e-3, "1 m x {sy} m x 1 m at 1000 kg/m^3: {mass}");
        bits.push(u64::from(mass.to_bits()));
    }
    for _ in 0..120 {
        pw.step(DT).unwrap();
    }
    for n in [3u32, 4] {
        let p = pw.pose(id(n)).unwrap().position;
        let (v, w) = pw.velocity(id(n)).unwrap();
        println!("RESULT e2_flat_dynamic_mesh n={n} p={p:?} v={v:?} w={w:?}");
        assert!(p[1] > -1.0e-3 && p[1] < 0.01, "slab {n} rests on the floor: {p:?}");
        assert!(v.iter().chain(&w).all(|c| c.abs() < 1.0e-2), "slab {n} is still: {v:?} {w:?}");
        bits.extend(p.map(f64::to_bits));
    }
    let pinned: [u64; 8] = [
        1036831948,
        981668463,
        4621256159554039114,
        4572371951457977504,
        13751766488242886528,
        4622945009603324679,
        4572320395417643424,
        13750212478256026624,
    ];
    assert_eq!(bits, pinned, "masses and resting positions' bits");
}
