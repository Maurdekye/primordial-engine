//! Cameras, projections and camera-relative transforms (render.md 5).
//!
//! - **A position reaches the GPU only as `object Pos − camera Pos`**, an f64 difference rotated into the camera's
//!   frame in f64 and only then cast to f32. The view matrix the GPU sees is the projection alone: the camera sits
//!   at the origin of the space the instances are written in. A raw absolute never reaches a GPU float.
//! - **Reverse Z:** depth 1 at the near plane, 0 at the far plane (or at infinity), compared with `Greater`.
//! - **Near and far scale with the rig's scale,** so a giant's world and an ant's world both fit the depth range.

use glam::{DMat3, DQuat, DVec3, Mat4, Vec4};
use pe_seams::{Pos, RigPose, RigTransform};

use crate::pose::{WorldPose, offset_vec, rig_to_world};

/// A field of view as four angles from the view axis, in radians, OpenXR's convention: `left` and `down` are
/// negative for a centred view.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Fov {
    pub left: f32,
    pub right: f32,
    pub up: f32,
    pub down: f32,
}

impl Fov {
    /// A symmetric field of view: `vertical` radians top to bottom, `aspect` width over height.
    pub fn symmetric(vertical: f32, aspect: f32) -> Fov {
        let tv = (vertical * 0.5).tan();
        let th = (tv * aspect).atan();
        Fov { left: -th, right: th, up: vertical * 0.5, down: -vertical * 0.5 }
    }

    fn tans(&self) -> [f64; 4] {
        [self.left as f64, self.right as f64, self.up as f64, self.down as f64].map(f64::tan)
    }
}

/// Near and far for a view, in metres at rig scale 1.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DepthRange {
    pub near: f64,
    /// `None`: an infinite far plane (the desktop and captures). VR states a finite one (render.md R2).
    pub far: Option<f64>,
}

impl DepthRange {
    pub const DESKTOP: DepthRange = DepthRange { near: 0.02, far: None };
    /// Finite in VR, 100 × the rig scale [I], until Virtual Desktop's runtime is shown to accept infinite (R2).
    pub const VR: DepthRange = DepthRange { near: 0.02, far: Some(100.0) };
}

/// One view's camera in the world.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Camera {
    pub pose: WorldPose,
    pub fov: Fov,
    /// Already scaled by the rig's scale.
    pub near: f64,
    pub far: Option<f64>,
}

impl Camera {
    /// The camera of an eye at `eye` (rig frame) in the rig `rig`.
    pub fn of_eye(rig: &RigTransform, eye: &RigPose, fov: Fov, depth: DepthRange) -> Camera {
        let s = (rig.scale as f64).max(1e-9);
        Camera { pose: rig_to_world(rig, eye), fov, near: depth.near * s, far: depth.far.map(|f| f * s) }
    }

    /// A camera placed in the world directly (captures).
    pub fn placed(pose: WorldPose, fov: Fov, depth: DepthRange) -> Camera {
        Camera { pose, fov, near: depth.near, far: depth.far }
    }

    /// `p` in the camera's frame (+X right, +Y up, −Z forward), in f64: the only route from a `Pos` to numbers.
    pub fn relative(&self, p: Pos) -> DVec3 {
        self.pose.rot.inverse() * offset_vec(p - self.pose.pos)
    }

    /// The camera-relative model matrix of an object at `pose` with per-axis `scale`, cast to f32 last.
    pub fn model(&self, pose: &WorldPose, scale: [f32; 3]) -> Mat4 {
        let r = DMat3::from_quat((self.pose.rot.inverse() * pose.rot).normalize());
        let s = DVec3::new(scale[0] as f64, scale[1] as f64, scale[2] as f64);
        let t = self.relative(pose.pos);
        let c = |v: DVec3, k: f64| Vec4::new((v.x * k) as f32, (v.y * k) as f32, (v.z * k) as f32, 0.0);
        Mat4::from_cols(
            c(r.x_axis, s.x),
            c(r.y_axis, s.y),
            c(r.z_axis, s.z),
            Vec4::new(t.x as f32, t.y as f32, t.z as f32, 1.0),
        )
    }

    /// The reverse-Z projection, right-handed, for wgpu's clip space (depth 0..1, +Y up).
    pub fn projection(&self) -> Mat4 {
        let [tl, tr, tu, td] = self.fov.tans();
        // Depth = (a z + b) / −z: 1 at z = −near, 0 at z = −far (a = 0 for an infinite far plane).
        let n = self.near;
        let (a, b) = match self.far {
            None => (0.0, n),
            Some(f) => (n / (f - n), n * f / (f - n)),
        };
        let (w, h) = (tr - tl, tu - td);
        Mat4::from_cols(
            Vec4::new((2.0 / w) as f32, 0.0, 0.0, 0.0),
            Vec4::new(0.0, (2.0 / h) as f32, 0.0, 0.0),
            Vec4::new(((tr + tl) / w) as f32, ((tu + td) / h) as f32, a as f32, -1.0),
            Vec4::new(0.0, 0.0, b as f32, 0.0),
        )
    }

    /// The view's volume, for the display layer's "in view" test and culling.
    pub fn volume(&self) -> ViewVolume {
        let [tl, tr, tu, td] = self.fov.tans();
        let p = |v: DVec3| v.normalize();
        ViewVolume {
            camera: self.pose.pos,
            inv_rot: self.pose.rot.inverse(),
            sides: [
                p(DVec3::new(1.0, 0.0, tl)),
                p(DVec3::new(-1.0, 0.0, -tr)),
                p(DVec3::new(0.0, 1.0, td)),
                p(DVec3::new(0.0, -1.0, -tu)),
            ],
            near: self.near,
            far: self.far,
        }
    }
}

/// A view frustum in the world, tested in the camera's frame.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ViewVolume {
    camera: Pos,
    inv_rot: DQuat,
    /// Inward normals of the four side planes, which pass through the camera.
    sides: [DVec3; 4],
    near: f64,
    far: Option<f64>,
}

impl ViewVolume {
    /// Whether a sphere at `center` of radius `r` metres touches the volume.
    pub fn touches_sphere(&self, center: Pos, r: f64) -> bool {
        let c = self.inv_rot * offset_vec(center - self.camera);
        if !c.is_finite() {
            return false;
        }
        if -c.z + r < self.near {
            return false;
        }
        if let Some(f) = self.far
            && -c.z - r > f
        {
            return false;
        }
        self.sides.iter().all(|n| n.dot(c) >= -r)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use glam::Vec3;
    use pe_seams::Offset;

    fn cam_at(p: Pos) -> Camera {
        Camera::placed(WorldPose { pos: p, rot: DQuat::IDENTITY }, Fov::symmetric(1.0, 16.0 / 9.0), DepthRange::DESKTOP)
    }

    #[test]
    fn reverse_z_maps_near_to_one_and_far_toward_zero() {
        let c = cam_at(Pos::ORIGIN);
        let p = c.projection();
        let at = |z: f32| {
            let v = p * Vec4::new(0.0, 0.0, z, 1.0);
            v.z / v.w
        };
        assert!((at(-0.02) - 1.0).abs() < 1e-6);
        assert!(at(-1000.0) < 1e-4 && at(-1000.0) > 0.0);
        let fin = Camera { far: Some(10.0), ..c }.projection();
        let v = fin * Vec4::new(0.0, 0.0, -10.0, 1.0);
        assert!((v.z / v.w).abs() < 1e-6);
    }

    #[test]
    fn asymmetric_fov_edges_map_to_clip_edges() {
        let mut c = cam_at(Pos::ORIGIN);
        c.fov = Fov { left: -0.9, right: 0.7, up: 0.8, down: -0.6 };
        let p = c.projection();
        let v = p * Vec4::new(0.7f32.tan(), 0.8f32.tan(), -1.0, 1.0);
        assert!((v.x / v.w - 1.0).abs() < 1e-5 && (v.y / v.w - 1.0).abs() < 1e-5);
        let v = p * Vec4::new(-(0.9f32.tan()), -(0.6f32.tan()), -1.0, 1.0);
        assert!((v.x / v.w + 1.0).abs() < 1e-5 && (v.y / v.w + 1.0).abs() < 1e-5);
    }

    /// Test 4's CPU half: a scene 10^15 m from the origin gives bit-identical model matrices to the same scene at the
    /// origin, because only differences reach f32.
    #[test]
    fn far_from_the_origin_the_matrices_are_identical() {
        let far = Pos::ORIGIN.checked_add(Offset([1e15, -3e14, 7e14])).unwrap();
        let obj = |base: Pos| WorldPose {
            pos: base.checked_add(Offset([0.25, 1.5, -2.0])).unwrap(),
            rot: DQuat::from_rotation_y(0.4),
        };
        let a = cam_at(Pos::ORIGIN).model(&obj(Pos::ORIGIN), [0.1, 0.2, 0.3]);
        let b = cam_at(far).model(&obj(far), [0.1, 0.2, 0.3]);
        assert_eq!(a.to_cols_array(), b.to_cols_array());
        assert_eq!(a.w_axis.truncate(), Vec3::new(0.25, 1.5, -2.0));
    }

    #[test]
    fn volume_tests_spheres() {
        let c = cam_at(Pos::ORIGIN);
        let v = c.volume();
        let at = |x, y, z| Pos::ORIGIN.checked_add(Offset([x, y, z])).unwrap();
        assert!(v.touches_sphere(at(0.0, 0.0, -5.0), 0.1));
        assert!(!v.touches_sphere(at(0.0, 0.0, 5.0), 0.1), "behind");
        assert!(!v.touches_sphere(at(50.0, 0.0, -5.0), 0.1), "far right");
        assert!(v.touches_sphere(at(50.0, 0.0, -5.0), 60.0), "big enough to reach in");
        assert!(!v.touches_sphere(at(0.0, 0.0, -0.001), 0.001), "inside the near plane");
    }
}
