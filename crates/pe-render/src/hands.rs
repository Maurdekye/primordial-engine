//! Markers for the local player's own hands (render.md 8, D4; xr.md 5.6): a stand-in drawn at the late-latched pose
//! when there is no hand object to draw there.
//!
//! The display layer draws the own hands by moving the frame view's `OWN_HAND` objects to `LocalPresentation::hands`.
//! When the view has none (no simulation, as in pe-xr's smoke run, or before the session publishes the local
//! member's hand bodies), [`markers`] gives two items per tracked hand, in the hand's pose:
//! - a block of 4 × 6 × 12 cm, long along the pose's Z (a controller's handle in OpenXR's grip space);
//! - a 12 cm pointer out of the block's −Z face, the way the hand points (grip and aim spaces both point along −Z).
//!
//! The left hand is blue and the right orange, both slightly emissive so they read in any lighting. The markers are
//! sized in the rig frame, so they keep the player's real hand size at any rig scale. They carry
//! [`ObjectId::NONE`], which names no object, so no highlight or label ever lands on them.

use glam::DVec3;
use pe_seams::{Appearance, Hand, ObjectId, RigPose, RigTransform, Shape};

use crate::DisplayHand;
use crate::pose::{quat, rig_to_world};
use crate::scene::DrawItem;

/// The block's half extents, metres in the rig frame.
pub const BLOCK_HALF: [f32; 3] = [0.02, 0.03, 0.06];
/// The pointer's half extents, metres in the rig frame.
pub const POINTER_HALF: [f32; 3] = [0.004, 0.004, 0.06];
/// The pointer's centre in the hand's frame: just past the block's −Z face.
pub const POINTER_CENTER: [f64; 3] = [0.0, 0.0, -0.12];

/// Linear RGB per hand, indexed by `Hand::index()`.
const COLORS: [[f32; 3]; 2] = [[0.10, 0.35, 0.95], [0.95, 0.45, 0.08]];

fn appearance(hand: Hand) -> Appearance {
    let c = COLORS[hand.index()];
    Appearance {
        base_color: [c[0], c[1], c[2], 1.0],
        emissive: c.map(|v| v * 0.25),
        roughness: 0.5,
        metalness: 0.0,
        material: None,
    }
}

fn half_diagonal(h: [f32; 3]) -> f32 {
    (h[0] * h[0] + h[1] * h[1] + h[2] * h[2]).sqrt()
}

/// The marker items for the tracked hands (`None` draws nothing), placed in the world through `rig`.
pub fn markers(hands: &[Option<DisplayHand>; 2], rig: &RigTransform) -> Vec<DrawItem> {
    let mut items = Vec::with_capacity(4);
    for hand in Hand::BOTH {
        let Some(h) = hands[hand.index()] else { continue };
        let q = quat(h.pose.orientation);
        let tip = DVec3::from_array(h.pose.position.map(f64::from)) + q * DVec3::from_array(POINTER_CENTER);
        let pointer = RigPose { position: tip.as_vec3().to_array(), orientation: h.pose.orientation };
        let s = rig.scale;
        for (pose, half) in [(h.pose, BLOCK_HALF), (pointer, POINTER_HALF)] {
            items.push(DrawItem {
                id: ObjectId::NONE,
                pose: rig_to_world(rig, &pose),
                scale: [s; 3],
                shape: Shape::Box { half_extents: half },
                appearance: appearance(hand),
                radius: half_diagonal(half) * s,
                outline: None,
                ghost: None,
            });
        }
    }
    items
}

#[cfg(test)]
mod tests {
    use glam::DQuat;
    use pe_seams::{Offset, Pos};

    use super::*;

    fn rig(scale: f32) -> RigTransform {
        let r = DQuat::from_rotation_y(std::f64::consts::FRAC_PI_2).as_quat();
        RigTransform {
            pos: Pos::ORIGIN.checked_add(Offset([10.0, 0.0, -3.0])).unwrap(),
            rotation: [r.x, r.y, r.z, r.w],
            scale,
        }
    }

    fn offset_of(p: Pos) -> DVec3 {
        DVec3::from_array((p - Pos::ORIGIN).0)
    }

    #[test]
    fn an_untracked_hand_draws_nothing() {
        assert!(markers(&[None, None], &rig(1.0)).is_empty());
        let one = markers(&[None, Some(DisplayHand { pose: RigPose::IDENTITY })], &rig(1.0));
        assert_eq!(one.len(), 2);
        assert_eq!(one[0].appearance.base_color[0], COLORS[1][0], "the right hand is orange");
        assert!(one.iter().all(|i| i.id == ObjectId::NONE));
    }

    /// The block sits at the hand; the pointer 12 cm along the hand's −Z, both through the rig transform, scaled.
    #[test]
    fn markers_follow_the_hand_through_the_rig() {
        // The hand 1.2 m up and 0.3 m right in the rig, turned 90° left (about +Y): it points along the rig's −X.
        let turn = DQuat::from_rotation_y(std::f64::consts::FRAC_PI_2).as_quat();
        let pose = RigPose { position: [0.3, 1.2, 0.0], orientation: [turn.x, turn.y, turn.z, turn.w] };
        let s = 2.0;
        let items = markers(&[Some(DisplayHand { pose }), None], &rig(s));
        assert_eq!(items.len(), 2);
        let block = offset_of(items[0].pose.pos);
        let tip = offset_of(items[1].pose.pos);
        // The rig turns +90° about Y: rig +X goes to world −Z, rig −X to world +Z. Origin at (10, 0, −3), scale 2.
        let expect_block = DVec3::new(10.0, 2.4, -3.0 - 0.6);
        // The hand points along the rig's −X, 0.12 m (0.24 m at scale 2): world +Z.
        let expect_tip = expect_block + DVec3::new(0.0, 0.0, 0.24);
        assert!((block - expect_block).length() < 1e-6, "block {block:?}");
        assert!((tip - expect_tip).length() < 1e-6, "tip {tip:?}");
        assert_eq!(items[0].scale, [s; 3]);
        assert!((items[0].radius - half_diagonal(BLOCK_HALF) * s).abs() < 1e-6);
        assert_eq!(items[0].appearance.base_color[2], COLORS[0][2], "the left hand is blue");
    }
}
