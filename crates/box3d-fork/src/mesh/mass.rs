//! Primordial Engine fork (CCD part 2 C): the mass of the solid a closed triangle mesh encloses (world-model.md 3.2).
//!
//! Box3D gives a mesh no mass. Here a closed mesh's mass is its density times the volume it encloses, with that
//! solid's centre of mass and inertia, by the divergence theorem: one signed tetrahedron per triangle from the centre
//! of the triangles' bounding box, summed in `f64` and rounded to `f32` once. A mesh that isn't closed, encloses less
//! than [`MIN_MESH_VOLUME`], or is wound inward, has no mass (it still collides).

use super::types::MeshTriangle;
use crate::geometry::MassData;
use crate::math_functions::{mul, Matrix3, Vec3};

/// The least volume a mesh's mass counts (a 1 mm cube), in cubic metres.
pub const MIN_MESH_VOLUME: f64 = 1.0e-9;

/// A position's bits, with −0 read as +0: closedness compares positions exactly, not vertex indices, since exporters
/// split vertices at seams and hard edges.
fn key(p: Vec3) -> [u32; 3] {
    let bits = |x: f32| if x == 0.0 { 0u32 } else { x.to_bits() };
    [bits(p.x), bits(p.y), bits(p.z)]
}

/// The triangles' corners as position ids, in order: equal positions (by [`key`]) get one id, so each corner is named
/// by its position, not its vertex index. Less every triangle with two equal positions (it encloses nothing).
fn kept_ids(vertices: &[Vec3], triangles: &[MeshTriangle]) -> Vec<[u32; 3]> {
    // Every vertex by its position's key, sorted, so that equal positions sit together.
    let mut order: Vec<([u32; 3], u32)> = vertices.iter().enumerate().map(|(i, &p)| (key(p), i as u32)).collect();
    order.sort_unstable();
    let mut id = vec![0u32; vertices.len()];
    let mut next = 0u32;
    for k in 0..order.len() {
        if k > 0 && order[k].0 != order[k - 1].0 {
            next += 1;
        }
        id[order[k].1 as usize] = next;
    }
    let mut kept = Vec::with_capacity(triangles.len());
    for t in triangles {
        let (a, b, c) = (id[t.index1 as usize], id[t.index2 as usize], id[t.index3 as usize]);
        if a != b && b != c && a != c {
            kept.push([a, b, c]);
        }
    }
    kept
}

/// Whether every edge of `triangles` is in exactly two of them, once each way. Two solids sharing an edge or a face
/// read as open (that edge is in four triangles). Each directed edge is one `u64`; the mesh is closed when no directed
/// edge repeats and the edges, reversed, are the same set. Sorted, then compared, so nothing depends on a hash's
/// order.
fn is_closed(triangles: &[[u32; 3]]) -> bool {
    if triangles.is_empty() {
        return false;
    }
    let edge = |x: u32, y: u32| (u64::from(x) << 32) | u64::from(y);
    let mut forward: Vec<u64> = Vec::with_capacity(3 * triangles.len());
    for &[a, b, c] in triangles {
        forward.extend_from_slice(&[edge(a, b), edge(b, c), edge(c, a)]);
    }
    // (x, y) reversed is (y, x): the halves swapped.
    let mut reversed: Vec<u64> = forward.iter().map(|e| e.rotate_left(32)).collect();
    forward.sort_unstable();
    if forward.windows(2).any(|w| w[0] == w[1]) {
        return false;
    }
    reversed.sort_unstable();
    forward == reversed
}

/// Whether the mesh of `vertices` and `triangles` is closed (world-model.md 3.2): every edge of its triangles is in
/// exactly two of them, once each way, edges compared by their ends' positions bit for bit (−0 read as +0), with
/// every triangle that has two equal positions left out. Judged on the positions as given, before any scale, so a
/// scale never changes it. Reversing every triangle doesn't change it either.
pub fn is_closed_mesh(vertices: &[Vec3], triangles: &[MeshTriangle]) -> bool {
    is_closed(&kept_ids(vertices, triangles))
}

fn sub3(a: [f64; 3], b: [f64; 3]) -> [f64; 3] {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

fn cross3(a: [f64; 3], b: [f64; 3]) -> [f64; 3] {
    [a[1] * b[2] - a[2] * b[1], a[2] * b[0] - a[0] * b[2], a[0] * b[1] - a[1] * b[0]]
}

fn dot3(a: [f64; 3], b: [f64; 3]) -> f64 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

fn wide(p: Vec3) -> [f64; 3] {
    [f64::from(p.x), f64::from(p.y), f64::from(p.z)]
}

/// The mass properties, at `density`, of the solid the mesh of `vertices` and `triangles` encloses at `scale`, about
/// its centre of mass (Box3D's `MassData`), or no mass.
///
/// Closedness ([`is_closed_mesh`]) is judged on the positions as given, before the scale. Then the scale is applied
/// to every vertex, in `f32` as Box3D's mesh contact does, and a scale with an odd number of negative axes, which
/// mirrors the mesh, has its winding read back, so a mirrored closed mesh keeps its positive volume. A triangle with
/// two equal positions encloses nothing and is left out of the test and the sum. The triangles are read in the order
/// given.
pub fn compute_closed_mesh_mass(
    vertices: &[Vec3],
    triangles: &[MeshTriangle],
    scale: Vec3,
    density: f32,
) -> MassData {
    if !is_closed_mesh(vertices, triangles) {
        return MassData::default();
    }
    let mirrored = scale.x * scale.y * scale.z < 0.0;
    let at = |i: i32| mul(scale, vertices[i as usize]);
    let mut kept: Vec<[Vec3; 3]> = Vec::with_capacity(triangles.len());
    for t in triangles {
        let (ka, kb, kc) =
            (key(vertices[t.index1 as usize]), key(vertices[t.index2 as usize]), key(vertices[t.index3 as usize]));
        if ka == kb || kb == kc || ka == kc {
            continue;
        }
        let (a, mut b, mut c) = (at(t.index1), at(t.index2), at(t.index3));
        if mirrored {
            core::mem::swap(&mut b, &mut c);
        }
        kept.push([a, b, c]);
    }

    // From the centre of the triangles' bounding box: a sum from the shape's origin loses precision for a mesh far
    // from it.
    let (mut lo, mut hi) = ([f64::INFINITY; 3], [f64::NEG_INFINITY; 3]);
    for tri in &kept {
        for p in tri {
            let q = wide(*p);
            for k in 0..3 {
                if q[k] < lo[k] {
                    lo[k] = q[k];
                }
                if q[k] > hi[k] {
                    hi[k] = q[k];
                }
            }
        }
    }
    let origin = [0.5 * (lo[0] + hi[0]), 0.5 * (lo[1] + hi[1]), 0.5 * (lo[2] + hi[2])];

    // Per tetrahedron (origin, u, v, w), det = u . (v x w) is six times its signed volume, its first moment is
    // det (u + v + w) / 24, and its second moment (the integral of x x') is det (uu' + vv' + ww' + ss') / 120.
    let mut six_volume = 0.0f64;
    let mut first = [0.0f64; 3];
    let mut second = [[0.0f64; 3]; 3];
    for [a, b, c] in &kept {
        let (u, v, w) = (sub3(wide(*a), origin), sub3(wide(*b), origin), sub3(wide(*c), origin));
        let det = dot3(u, cross3(v, w));
        let s = [u[0] + v[0] + w[0], u[1] + v[1] + w[1], u[2] + v[2] + w[2]];
        six_volume += det;
        for i in 0..3 {
            first[i] += det * s[i];
            for j in 0..3 {
                second[i][j] += det * (u[i] * u[j] + v[i] * v[j] + w[i] * w[j] + s[i] * s[j]);
            }
        }
    }
    let volume = six_volume / 6.0;
    // Below the floor, wound inward (negative), or not a number: no mass.
    if volume.is_nan() || volume < MIN_MESH_VOLUME {
        return MassData::default();
    }
    let g = [first[0] / 24.0 / volume, first[1] / 24.0 / volume, first[2] / 24.0 / volume];
    // The second moment about the centre of mass, then the inertia tensor tr(C) I - C at the density.
    let mut c = [[0.0f64; 3]; 3];
    for i in 0..3 {
        for j in 0..3 {
            c[i][j] = second[i][j] / 120.0 - volume * g[i] * g[j];
        }
    }
    let trace = c[0][0] + c[1][1] + c[2][2];
    let d = f64::from(density);
    let column = |j: usize| Vec3 {
        x: (d * (if j == 0 { trace } else { 0.0 } - c[0][j])) as f32,
        y: (d * (if j == 1 { trace } else { 0.0 } - c[1][j])) as f32,
        z: (d * (if j == 2 { trace } else { 0.0 } - c[2][j])) as f32,
    };
    MassData {
        mass: (d * volume) as f32,
        center: Vec3 { x: (origin[0] + g[0]) as f32, y: (origin[1] + g[1]) as f32, z: (origin[2] + g[2]) as f32 },
        inertia: Matrix3 { cx: column(0), cy: column(1), cz: column(2) },
    }
}
