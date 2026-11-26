//! Triangle meshes (world-model.md 3.2; CCD part 2 C): built once, from their vertices and triangles or from a mesh
//! v1 blob, and shared by every shape that uses them ([`crate::Geometry::Mesh`]). Also the hull v1 blob's reader.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, OnceLock};

use box3d_rust::math_functions::Vec3;
use box3d_rust::mesh::{MeshData, MeshDef, MeshTriangle, compute_closed_mesh_mass, create_mesh, is_closed_mesh};

use crate::desc::limits::{MAX_HULL_BLOB_POINTS, MAX_SIM_BLOB_BYTES, MAX_SIZE};
use crate::error::PhysicsError;

/// A triangle mesh as the physics builds it: Box3D's mesh, with its bounding volume tree, built from the vertices
/// and triangles given, with welding off (world-model.md 3.2, "the weld"). Box3D leaves out every triangle with an
/// area under 0.01·slop² (2.5×10⁻⁷ m², judged before any scale): [`Mesh::dropped`] counts them. Cheap to clone:
/// every clone shares one build, so a body rebuilt with the same mesh doesn't build it again.
///
/// The mesh is closed ([`Mesh::is_closed`]) when every edge of its built triangles is in exactly two of them, once
/// each way, compared by position bit for bit. A closed mesh weighs the solid it encloses; an open one weighs nothing
/// (and still collides).
#[derive(Clone)]
pub struct Mesh(Arc<Built>);

struct Built {
    front: MeshData,
    /// The least and greatest coordinate of every vertex given, used by a triangle or not, before any scale.
    extent: ([f32; 3], [f32; 3]),
    /// The same triangles reversed, for a shape with both sides, built when first asked for.
    back: OnceLock<Option<MeshData>>,
    blob_triangles: usize,
    closed: bool,
    blob_closed: bool,
    /// [`Mesh::mass`]'s newest answers, at most [`MASSES_KEPT`], by the bits of the scale and the density: each sum
    /// runs the closedness test's sort again, and a body is weighed more than once as it is built (pe-replica's
    /// check, then `add_body`'s). A cache of a pure function: what it keeps never changes an answer.
    masses: Mutex<VecDeque<([u32; 4], f32)>>,
}

/// How many of [`Mesh::mass`]'s answers a mesh keeps.
const MASSES_KEPT: usize = 8;

/// A mesh v1 blob's magic (world-model.md 3.2).
const MESH_MAGIC: [u8; 4] = *b"PEM1";
/// A hull v1 blob's magic.
const HULL_MAGIC: [u8; 4] = *b"PEH1";

fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().expect("four bytes"))
}

fn point_at(bytes: &[u8], at: usize) -> [f32; 3] {
    [0, 4, 8].map(|k| f32::from_bits(u32_at(bytes, at + k)))
}

fn vec3(p: [f32; 3]) -> Vec3 {
    Vec3 { x: p[0], y: p[1], z: p[2] }
}

/// The bytes a mesh of `vertices` and `triangles` takes as a mesh v1 blob.
fn blob_bytes(vertices: usize, triangles: usize) -> u64 {
    12 + 12 * (vertices as u64 + triangles as u64)
}

impl Mesh {
    /// Build the mesh of `vertices` (metres, in the body's frame, before any scale) and `triangles` (indices into
    /// `vertices`, each counter-clockwise seen from its front), in the order given. Refused, never a panic: fewer than
    /// 3 vertices or no triangle, more than a 960 KiB blob holds, a coordinate not finite or beyond
    /// [`crate::limits::MAX_SIZE`], an index out of range, or no triangle left once Box3D drops those under the
    /// minimum area.
    pub fn new(vertices: &[[f32; 3]], triangles: &[[u32; 3]]) -> Result<Mesh, PhysicsError> {
        if vertices.len() < 3 || triangles.is_empty() {
            return Err(PhysicsError::BadMesh("a mesh needs at least 3 vertices and a triangle"));
        }
        if blob_bytes(vertices.len(), triangles.len()) > MAX_SIM_BLOB_BYTES as u64 {
            return Err(PhysicsError::BadMesh("a mesh larger than a 960 KiB mesh blob holds"));
        }
        if !vertices.iter().flatten().all(|c| c.is_finite() && c.abs() <= MAX_SIZE) {
            return Err(PhysicsError::InvalidValue("mesh vertex"));
        }
        if !triangles.iter().flatten().all(|&i| (i as usize) < vertices.len()) {
            return Err(PhysicsError::BadMesh("a triangle names a vertex the mesh doesn't have"));
        }
        let def = MeshDef {
            vertices: vertices.iter().map(|&p| vec3(p)).collect(),
            // Every index is below the vertex count, at most 81,919 (the blob cap), so it fits an i32.
            indices: triangles.iter().flatten().map(|&i| i as i32).collect(),
            // Box3D's smooth-edge marks, which keep a body sliding across the mesh from catching on inner edges. It
            // finds them by shared vertex index, so a blob that splits its vertices at a seam is sharp there.
            identify_edges: true,
            // Welding off, Box3D's default, stated (world-model.md 3.2, "the weld").
            weld_vertices: false,
            ..MeshDef::default()
        };
        let front = create_mesh(&def, None)
            .ok_or(PhysicsError::BadMesh("Box3D could not build the mesh: every triangle is under the minimum area"))?;
        let closed = is_closed_mesh(&front.vertices, &front.triangles);
        // With nothing dropped, the triangles given are the ones built (a triangle with two equal positions has no
        // area, so Box3D drops it). Otherwise they are judged on their own: a dropped sliver may have three distinct
        // positions, and then the triangles given can be open while the built mesh is closed.
        let blob_closed = if front.triangles.len() == triangles.len() {
            closed
        } else {
            let given: Vec<MeshTriangle> = triangles
                .iter()
                .map(|t| MeshTriangle { index1: t[0] as i32, index2: t[1] as i32, index3: t[2] as i32 })
                .collect();
            is_closed_mesh(&def.vertices, &given)
        };
        let mut extent = (vertices[0], vertices[0]);
        for p in vertices {
            for (k, &c) in p.iter().enumerate() {
                extent.0[k] = extent.0[k].min(c);
                extent.1[k] = extent.1[k].max(c);
            }
        }
        let blob_triangles = triangles.len();
        let masses = Mutex::new(VecDeque::new());
        Ok(Mesh(Arc::new(Built { front, extent, back: OnceLock::new(), blob_triangles, closed, blob_closed, masses })))
    }

    /// Read a mesh v1 blob (world-model.md 3.2): the magic `PEM1`, a `u32` vertex count, the vertices as `f32 x3`, a
    /// `u32` triangle count, then the triangles as `u32 x3`, little-endian, exactly as long as its counts say and at
    /// most 960 KiB; then [`Mesh::new`]. A blob that can't be read is refused, the same on every machine.
    pub fn from_blob(bytes: &[u8]) -> Result<Mesh, PhysicsError> {
        if bytes.len() > MAX_SIM_BLOB_BYTES {
            return Err(PhysicsError::BadBlob("a mesh blob over 960 KiB"));
        }
        if bytes.len() < 12 || bytes[..4] != MESH_MAGIC {
            return Err(PhysicsError::BadBlob("not a mesh v1 blob (magic PEM1)"));
        }
        let vertex_count = u32_at(bytes, 4) as usize;
        if vertex_count > (bytes.len() - 12) / 12 {
            return Err(PhysicsError::BadBlob("a mesh blob's vertices run past its end"));
        }
        let at = 8 + 12 * vertex_count;
        let triangle_count = u32_at(bytes, at) as usize;
        if blob_bytes(vertex_count, triangle_count) != bytes.len() as u64 {
            return Err(PhysicsError::BadBlob("a mesh blob's length differs from what its counts say"));
        }
        let vertices: Vec<[f32; 3]> = (0..vertex_count).map(|i| point_at(bytes, 8 + 12 * i)).collect();
        let triangles: Vec<[u32; 3]> =
            (0..triangle_count).map(|i| [0, 4, 8].map(|k| u32_at(bytes, at + 4 + 12 * i + k))).collect();
        Mesh::new(&vertices, &triangles)
    }

    /// The triangles Box3D built (the blob's, less those it dropped), in its order.
    pub fn triangle_count(&self) -> usize {
        self.0.front.triangles.len()
    }

    /// The triangles the mesh was given.
    pub fn blob_triangle_count(&self) -> usize {
        self.0.blob_triangles
    }

    /// How many of the triangles given Box3D left out, under the minimum area (a triangle with two equal positions is
    /// one of them).
    pub fn dropped(&self) -> usize {
        self.0.blob_triangles - self.triangle_count()
    }

    /// Whether the mesh as built is closed (see [`Mesh`]): judged before any scale, so a scale never changes it.
    pub fn is_closed(&self) -> bool {
        self.0.closed
    }

    /// Whether the triangles given, less those with two equal positions, are closed. When they are and the built mesh
    /// isn't, the triangles Box3D dropped are what opened it (world-model.md 3.2, `Unbuildable::Mass`'s message).
    pub fn is_blob_closed(&self) -> bool {
        self.0.blob_closed
    }

    /// The mesh's mass, kg, at `scale` (applied to each vertex, axis by axis) and `density` (kg/m³), as the fork's
    /// `compute_shape_mass` gives it: the enclosed solid's, or 0 for a mesh that is open, wound inward or encloses
    /// less than 10⁻⁹ m³ (world-model.md 3.2). Linear in the triangles, plus the closedness test's sort.
    pub fn mass(&self, scale: [f32; 3], density: f32) -> f32 {
        if !self.0.closed || density == 0.0 {
            return 0.0;
        }
        let key = [scale[0].to_bits(), scale[1].to_bits(), scale[2].to_bits(), density.to_bits()];
        let kept = self.0.masses.lock().unwrap_or_else(|e| e.into_inner()).iter().find(|(k, _)| *k == key).map(|e| e.1);
        if let Some(mass) = kept {
            return mass;
        }
        let mass = compute_closed_mesh_mass(&self.0.front.vertices, &self.0.front.triangles, vec3(scale), density).mass;
        let mut masses = self.0.masses.lock().unwrap_or_else(|e| e.into_inner());
        if masses.len() == MASSES_KEPT {
            masses.pop_front();
        }
        masses.push_back((key, mass));
        mass
    }

    /// The least and greatest coordinate of every vertex, before any scale.
    pub fn extent(&self) -> ([f32; 3], [f32; 3]) {
        self.0.extent
    }

    pub(crate) fn front(&self) -> &MeshData {
        &self.0.front
    }

    /// The same triangles reversed: a shape's back, for both sides. Built once per mesh, when first asked for.
    pub(crate) fn back(&self) -> Option<&MeshData> {
        self.0
            .back
            .get_or_init(|| {
                let front = &self.0.front;
                let def = MeshDef {
                    vertices: front.vertices.clone(),
                    indices: front.triangles.iter().flat_map(|t| [t.index1, t.index2, t.index3]).collect(),
                    identify_edges: true,
                    clock_wise_winding: true,
                    ..MeshDef::default()
                };
                create_mesh(&def, None)
            })
            .as_ref()
    }
}

/// Equal when built from the same mesh: the same bytes as Box3D builds them.
impl PartialEq for Mesh {
    fn eq(&self, other: &Mesh) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
            || (self.0.front.hash == other.0.front.hash
                && self.0.blob_triangles == other.0.blob_triangles
                && self.0.front.to_bytes() == other.0.front.to_bytes())
    }
}

impl std::fmt::Debug for Mesh {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Mesh")
            .field("vertices", &self.0.front.vertices.len())
            .field("triangles", &self.triangle_count())
            .field("dropped", &self.dropped())
            .field("closed", &self.0.closed)
            .finish()
    }
}

/// Read a hull v1 blob (world-model.md 3.2): the magic `PEH1`, a `u32` point count of 4 to 64, then the points as
/// `f32 x3`, little-endian, exactly that long. The points are for [`crate::Geometry::Hull`], which checks their values
/// and builds the hull. A blob that can't be read is refused, the same on every machine.
pub fn hull_points_from_blob(bytes: &[u8]) -> Result<Vec<[f32; 3]>, PhysicsError> {
    if bytes.len() < 8 || bytes[..4] != HULL_MAGIC {
        return Err(PhysicsError::BadBlob("not a hull v1 blob (magic PEH1)"));
    }
    let count = u32_at(bytes, 4) as usize;
    if !(4..=MAX_HULL_BLOB_POINTS).contains(&count) {
        return Err(PhysicsError::BadBlob("a hull blob needs 4 to 64 points"));
    }
    if bytes.len() != 8 + 12 * count {
        return Err(PhysicsError::BadBlob("a hull blob's length differs from what its count says"));
    }
    Ok((0..count).map(|i| point_at(bytes, 8 + 12 * i)).collect())
}
