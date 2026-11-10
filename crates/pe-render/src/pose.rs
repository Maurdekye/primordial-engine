//! A world pose for the display layer: an exact position ([`Pos`]) and an f64 rotation.
//!
//! The display layer works in f64 and in `Pos` throughout. Numbers reach the GPU only after subtracting the
//! camera's `Pos` (render.md 5), in [`crate::camera`].

use glam::{DQuat, DVec3};
use pe_seams::{Offset, Pos, RigPose, RigTransform, ViewObject};

/// A pose in the world.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WorldPose {
    pub pos: Pos,
    pub rot: DQuat,
}

impl WorldPose {
    pub fn of(o: &ViewObject) -> WorldPose {
        WorldPose { pos: o.pos, rot: quat(o.rotation) }
    }

    /// `self` moved by `d`, keeping `self` where the move would leave the supported range (a pose past the range is
    /// never drawn differently from its last valid place).
    pub fn moved(self, d: DVec3) -> WorldPose {
        WorldPose { pos: self.pos.checked_add(Offset(d.to_array())).unwrap_or(self.pos), rot: self.rot }
    }
}

/// A unit quaternion from S5's `(x, y, z, w)` f32 order. A zero or non-finite quaternion reads as the identity.
pub fn quat(q: [f32; 4]) -> DQuat {
    let d = DQuat::from_xyzw(q[0] as f64, q[1] as f64, q[2] as f64, q[3] as f64);
    let n = d.length();
    if n.is_finite() && n > 1e-12 { d / n } else { DQuat::IDENTITY }
}

pub fn vec(v: [f32; 3]) -> DVec3 {
    DVec3::new(v[0] as f64, v[1] as f64, v[2] as f64)
}

pub fn offset_vec(o: Offset) -> DVec3 {
    DVec3::from_array(o.0)
}

/// The angle of a rotation, in radians, in [0, π].
pub fn angle_of(q: DQuat) -> f64 {
    let w = q.w.abs().min(1.0);
    2.0 * w.acos()
}

/// A rig-frame pose of the local player placed in the world through the rig transform (render.md 5): position
/// `rig.pos + rig.rotation × (rig.scale × p)`, rotation `rig.rotation × q`.
pub fn rig_to_world(rig: &RigTransform, p: &RigPose) -> WorldPose {
    let r = quat(rig.rotation);
    let local = r * (vec(p.position) * rig.scale as f64);
    WorldPose { pos: rig.pos, rot: r }.moved(local).with_rot(r * quat(p.orientation))
}

impl WorldPose {
    pub fn with_rot(self, rot: DQuat) -> WorldPose {
        WorldPose { pos: self.pos, rot }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rig_transform_places_hands() {
        let rig = RigTransform {
            pos: Pos::ORIGIN.checked_add(Offset([10.0, 0.0, 0.0])).unwrap(),
            // 90 degrees about +Y.
            rotation: [0.0, std::f32::consts::FRAC_1_SQRT_2, 0.0, std::f32::consts::FRAC_1_SQRT_2],
            scale: 2.0,
        };
        let p = RigPose { position: [0.0, 1.0, -1.0], orientation: [0.0, 0.0, 0.0, 1.0] };
        let w = rig_to_world(&rig, &p);
        let d = offset_vec(w.pos - Pos::ORIGIN);
        // -Z forward rotated +90 deg about Y is -X; scaled by 2.
        assert!((d - DVec3::new(8.0, 2.0, 0.0)).length() < 1e-6, "{d:?}");
    }

    #[test]
    fn degenerate_quaternions_read_as_identity() {
        assert_eq!(quat([0.0; 4]), DQuat::IDENTITY);
        assert_eq!(quat([f32::NAN, 0.0, 0.0, 1.0]), DQuat::IDENTITY);
    }
}
