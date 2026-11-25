//! What a mesh costs at build, at a rebuild (a Scale edit), in an image and in the step (CCD part 2 C): a closed UV
//! sphere of about 1,000, 10,000 and 50,000 triangles (the last near the 960 KiB blob cap).
//!
//! `cargo run --release -p pe-physics --example mesh_cost`. Timings depend on the machine; they are printed as COST
//! lines, never RESULT lines (a RESULT line is a cross-platform contract).

use std::time::Instant;

use pe_physics::{BodyDesc, BodyKind, Geometry, Mesh, ObjectId, PhysicsConfig, PhysicsWorld, ShapeDesc};

/// A closed UV sphere of radius 0.5 with `segments` around and `rings` from pole to pole, outward.
fn sphere(segments: u32, rings: u32) -> (Vec<[f32; 3]>, Vec<[u32; 3]>) {
    let mut v = vec![[0.0, 0.5, 0.0], [0.0, -0.5, 0.0]];
    for i in 1..rings {
        let theta = std::f32::consts::PI * i as f32 / rings as f32;
        for j in 0..segments {
            let phi = 2.0 * std::f32::consts::PI * j as f32 / segments as f32;
            v.push([0.5 * theta.sin() * phi.cos(), 0.5 * theta.cos(), 0.5 * theta.sin() * phi.sin()]);
        }
    }
    let at = |i: u32, j: u32| 2 + (i - 1) * segments + j % segments;
    let mut t = Vec::new();
    for j in 0..segments {
        t.push([0, at(1, j + 1), at(1, j)]);
        t.push([1, at(rings - 1, j), at(rings - 1, j + 1)]);
        for i in 1..rings - 1 {
            t.push([at(i, j), at(i, j + 1), at(i + 1, j + 1)]);
            t.push([at(i, j), at(i + 1, j + 1), at(i + 1, j)]);
        }
    }
    (v, t)
}

fn blob(v: &[[f32; 3]], t: &[[u32; 3]]) -> Vec<u8> {
    let mut b = b"PEM1".to_vec();
    b.extend_from_slice(&(v.len() as u32).to_le_bytes());
    v.iter().flatten().for_each(|c| b.extend_from_slice(&c.to_le_bytes()));
    b.extend_from_slice(&(t.len() as u32).to_le_bytes());
    t.iter().flatten().for_each(|i| b.extend_from_slice(&i.to_le_bytes()));
    b
}

/// The median of `runs` timings of `f`, in milliseconds.
fn median_ms(runs: usize, mut f: impl FnMut()) -> f64 {
    let mut ms: Vec<f64> = (0..runs)
        .map(|_| {
            let start = Instant::now();
            f();
            start.elapsed().as_secs_f64() * 1e3
        })
        .collect();
    ms.sort_by(f64::total_cmp);
    ms[runs / 2]
}

fn main() {
    let id = |n: u128| ObjectId::from_u128(n);
    for (segments, rings) in [(25, 21), (100, 51), (250, 101)] {
        let (v, mut t) = sphere(segments, rings);
        if Mesh::new(&v, &t).unwrap().mass([1.0; 3], 1.0) == 0.0 {
            t.iter_mut().for_each(|tri| tri.swap(1, 2));
        }
        let bytes = blob(&v, &t);
        let mesh = Mesh::from_blob(&bytes).unwrap();
        assert!(mesh.is_closed() && mesh.mass([1.0; 3], 1.0) > 0.0);
        let build = median_ms(5, || drop(Mesh::from_blob(&bytes).unwrap()));
        let back = median_ms(5, || {
            let m = Mesh::from_blob(&bytes).unwrap();
            let shape = ShapeDesc::new(Geometry::Mesh { mesh: m, scale: [1.0; 3], both_sides: true });
            let mut pw = PhysicsWorld::new(PhysicsConfig::engine()).unwrap();
            pw.add_body(id(9), &BodyDesc::new(BodyKind::Static, [0.0; 3]).with_shape(shape)).unwrap();
        }) - build;
        let sum = median_ms(5, || assert!(mesh.mass([1.1, 0.9, 1.0], 1000.0) > 0.0));
        // What a joiner receives for it: the transfer of a world holding only this mesh, one-sided and both-sided.
        let transfer_kib = |both_sides: bool| {
            let mut pw = PhysicsWorld::new(PhysicsConfig::engine()).unwrap();
            let shape = ShapeDesc::new(Geometry::Mesh { mesh: mesh.clone(), scale: [1.0; 3], both_sides });
            pw.add_body(id(9), &BodyDesc::new(BodyKind::Static, [0.0; 3]).with_shape(shape)).unwrap();
            pw.save_transfer().unwrap().to_bytes().len() / 1024
        };
        let (one_side, both_sides) = (transfer_kib(false), transfer_kib(true));
        // A rebuild (a Scale edit): remove the body and add it again at a new scale, with the built mesh kept.
        let mut pw = PhysicsWorld::new(PhysicsConfig::engine()).unwrap();
        let ground = BodyDesc::new(BodyKind::Static, [0.0, -1.0, 0.0])
            .with_shape(ShapeDesc::new(Geometry::Box { half_extents: [20.0, 1.0, 20.0] }));
        pw.add_body(id(1), &ground).unwrap();
        let mut k = 0u32;
        let rebuild = median_ms(9, || {
            k += 1;
            let scale = [1.0 + 0.01 * k as f32, 1.0, 1.0];
            let shape =
                ShapeDesc::new(Geometry::Mesh { mesh: mesh.clone(), scale, both_sides: false }).with_density(500.0);
            let _ = pw.remove_body(id(2));
            let mut desc = BodyDesc::new(BodyKind::Dynamic, [0.0, 0.5, 0.0]).with_shape(shape);
            // Awake, so the step below pays for it.
            desc.enable_sleep = false;
            pw.add_body(id(2), &desc).unwrap();
        });
        for _ in 0..30 {
            pw.step(1.0 / 60.0).unwrap();
        }
        let save = median_ms(9, || drop(pw.save_image().unwrap()));
        let image = pw.save_image().unwrap();
        let restore = median_ms(9, || pw.restore_image(&image).unwrap());
        // A step with the mesh body awake on the floor (a moving mesh's triangles are found again each step).
        let step = median_ms(31, || pw.step(1.0 / 60.0).unwrap());
        println!(
            "COST mesh of {} triangles ({} vertices, blob {} KiB): build from blob {build:.2} ms, its back {back:.2} ms; \
             mass sum {sum:.3} ms; rebuild of a dynamic body (Scale edit, mesh kept) {rebuild:.2} ms; image save \
             {save:.3} ms, restore {restore:.3} ms; step with it awake on the floor {step:.3} ms; a join transfer with \
             it {one_side} KiB, both sides {both_sides} KiB",
            t.len(),
            v.len(),
            bytes.len() / 1024
        );
    }
}
