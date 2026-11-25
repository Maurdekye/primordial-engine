//! The local player's own hands as drawn (render.md 5 and 8, D4; xr.md 5.6 and 6).
//!
//! - **A tracked hand** (`DisplayHand::joints` is `Some`) is drawn as its joints: a sphere per joint the runtime
//!   located, at the runtime's joint radius, and a capsule per bone whose two joints it located ([`joint_items`]).
//!   It is display only: the simulation's hand body stays the 9 cm sphere, and a capture draws that sphere.
//!   [`draw_tracked_joints`] puts the joints in place of an `OWN_HAND` object's own shape (pe-app's case);
//!   [`markers`] draws them when there is no hand object.
//! - **A controller or the desktop hand** (`joints` is `None`) draws as before. The display layer moves the frame
//!   view's `OWN_HAND` objects to `LocalPresentation::hands`.
//!
//! When the view has no hand objects (no simulation, as in pe-xr's smoke run, or before the session publishes the
//! local member's hand bodies), [`markers`] gives two items per controller-tracked hand, in the hand's pose:
//! - a block of 4 × 6 × 12 cm, long along the pose's Z (a controller's handle in OpenXR's grip space);
//! - a 12 cm pointer out of the block's −Z face, the way the hand points (grip and aim spaces both point along −Z).
//!
//! The left hand is blue and the right orange, both slightly emissive so they read in any lighting. Markers and
//! joints are sized in the rig frame, so they keep the player's real hand size at any rig scale. They carry
//! [`ObjectId::NONE`], which names no object, so no highlight, label or `visible_list` entry ever lands on them.
//! They go in `Scene::front`, so a notice never covers them (render.md 8.1).

use glam::{DQuat, DVec3};
use pe_seams::{Appearance, FrameView, Hand, ObjectId, RigPose, RigTransform, Shape, ViewFlags};

use crate::pose::{quat, rig_to_world, vec};
use crate::scene::{DrawItem, Scene};
use crate::{DisplayHand, HandJoint, HandJoints, LocalPresentation};

/// The block's half extents, metres in the rig frame.
pub const BLOCK_HALF: [f32; 3] = [0.02, 0.03, 0.06];
/// The pointer's half extents, metres in the rig frame.
pub const POINTER_HALF: [f32; 3] = [0.004, 0.004, 0.06];
/// The pointer's centre in the hand's frame: just past the block's −Z face.
pub const POINTER_CENTER: [f64; 3] = [0.0, 0.0, -0.12];

/// The bones drawn between a tracked hand's joints, as `HandJoints` indices: the wrist to each digit's metacarpal,
/// then along each digit (the thumb has no intermediate joint). The palm joint is drawn as a sphere only.
pub const BONES: [(usize, usize); 24] = [
    (1, 2),
    (1, 6),
    (1, 11),
    (1, 16),
    (1, 21),
    (2, 3),
    (3, 4),
    (4, 5),
    (6, 7),
    (7, 8),
    (8, 9),
    (9, 10),
    (11, 12),
    (12, 13),
    (13, 14),
    (14, 15),
    (16, 17),
    (17, 18),
    (18, 19),
    (19, 20),
    (21, 22),
    (22, 23),
    (23, 24),
    (24, 25),
];
/// A bone's radius, as a share of the smaller of its two joints' radii, so the joints read as knuckles [I].
pub const BONE_SHARE: f32 = 0.7;
/// The longest bone drawn, as its capsule's straight half length over its radius. A bone's shape is quantised to
/// a whole-number ratio from 1 to this, so a tracked hand uses at most this many capsule meshes (scene.rs keys them by
/// that ratio and never drops one, and the joints jitter every frame; @shader-reviewer, on review). The renderer
/// makes them at start.
pub const MAX_BONE_ASPECT: u32 = 16;
/// The smallest joint radius drawn, metres in the rig frame, should a runtime report none [I].
pub const MIN_JOINT_RADIUS: f32 = 0.004;

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

/// The marker items for the tracked hands (`None` draws nothing), placed in the world through `rig`: a hand with
/// joints as its joints ([`joint_items`]), any other as a block and a pointer.
pub fn markers(hands: &[Option<DisplayHand>; 2], rig: &RigTransform) -> Vec<DrawItem> {
    let mut items = Vec::with_capacity(4);
    for hand in Hand::BOTH {
        let Some(h) = hands[hand.index()] else { continue };
        if let Some(j) = &h.joints {
            items.extend(joint_items(hand, j, rig));
            continue;
        }
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

/// A tracked hand drawn as its joints, placed in the world through `rig`: a sphere per valid joint at the runtime's
/// radius (at least [`MIN_JOINT_RADIUS`]), and a capsule per bone of [`BONES`] whose two joints are valid, from one
/// joint's centre to the other's, at most [`BONE_SHARE`] of the thinner joint wide (a whole-number aspect,
/// [`MAX_BONE_ASPECT`]). An invalid joint draws nothing, nor do its bones.
pub fn joint_items(hand: Hand, joints: &HandJoints, rig: &RigTransform) -> Vec<DrawItem> {
    let s = rig.scale;
    let item = |position: DVec3, rot: DQuat, shape: Shape, bound: f32| DrawItem {
        id: ObjectId::NONE,
        pose: rig_to_world(
            rig,
            &RigPose { position: position.as_vec3().to_array(), orientation: rot.as_quat().to_array() },
        ),
        scale: [s; 3],
        shape,
        appearance: appearance(hand),
        radius: bound * s,
        outline: None,
        ghost: None,
    };
    let radius = |j: &HandJoint| j.radius.max(MIN_JOINT_RADIUS);
    let mut items = Vec::with_capacity(HandJoints::COUNT + BONES.len());
    for j in joints.0.iter().filter(|j| j.valid) {
        let r = radius(j);
        items.push(item(vec(j.position), DQuat::IDENTITY, Shape::Sphere { radius: r }, r));
    }
    for &(a, b) in &BONES {
        let (ja, jb) = (&joints.0[a], &joints.0[b]);
        if !(ja.valid && jb.valid) {
            continue;
        }
        let (pa, pb) = (vec(ja.position), vec(jb.position));
        let len = (pb - pa).length();
        if len < 1e-6 {
            continue;
        }
        // The capsule's axis is its +Y (scene.rs, capsule_mesh); `half_height` is its straight part's half length.
        let rot = DQuat::from_rotation_arc(DVec3::Y, (pb - pa) / len);
        let half = (len * 0.5) as f32;
        // A whole-number aspect, the radius shrunk to fit: never wider than BONE_SHARE of the thinner joint, so the
        // caps stay inside the joint spheres, and one of MAX_BONE_ASPECT meshes whatever the jitter.
        let widest = radius(ja).min(radius(jb)) * BONE_SHARE;
        let r = half / (half / widest).ceil().clamp(1.0, MAX_BONE_ASPECT as f32);
        items.push(item((pa + pb) * 0.5, rot, Shape::Capsule { radius: r, half_height: half }, half + r));
    }
    items
}

/// Draw each own hand that has tracked joints as its joints, in place of its `OWN_HAND` object's own shape (the
/// simulation's 9 cm sphere). Only while the own hands are late-latched, since the joints are; a hand without
/// joints, and every held object, stay as drawn. `scene` is the display layer's (`Scene::displayed`) for `view`.
pub fn draw_tracked_joints(scene: &mut Scene, view: &FrameView, local: &LocalPresentation, rig: &RigTransform) {
    let mut joints = Vec::new();
    scene.front.retain(|item| {
        let Some(o) = view.object(item.id) else { return true };
        let (true, Some((_, hand))) = (o.flags.contains(ViewFlags::OWN_HAND), o.hand_of) else { return true };
        let Some(j) = local.hands[hand.index()].and_then(|h| h.joints) else { return true };
        joints.extend(joint_items(hand, &j, rig));
        false
    });
    scene.front.extend(joints);
}

#[cfg(test)]
mod tests {
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
        let one = markers(&[None, Some(DisplayHand { pose: RigPose::IDENTITY, joints: None })], &rig(1.0));
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
        let items = markers(&[Some(DisplayHand { pose, joints: None }), None], &rig(s));
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

    /// A straight hand along the rig's −Z: joint i at z = −i cm, 1 cm radius, all valid.
    fn straight() -> HandJoints {
        let mut j = [HandJoint::default(); HandJoints::COUNT];
        for (i, joint) in j.iter_mut().enumerate() {
            *joint = HandJoint { position: [0.0, 1.0, -0.01 * i as f32], radius: 0.01, valid: true };
        }
        HandJoints(j)
    }

    #[test]
    fn every_valid_joint_and_bone_draws() {
        let items = joint_items(Hand::Left, &straight(), &rig(1.0));
        let spheres = items.iter().filter(|i| matches!(i.shape, Shape::Sphere { .. })).count();
        let capsules = items.iter().filter(|i| matches!(i.shape, Shape::Capsule { .. })).count();
        assert_eq!((spheres, capsules), (26, 24));
        assert!(items.iter().all(|i| i.id == ObjectId::NONE && i.outline.is_none()));
        assert_eq!(items[0].appearance.base_color[2], COLORS[0][2], "the left hand is blue");
    }

    #[test]
    fn an_invalid_joint_draws_nothing_nor_do_its_bones() {
        let mut j = straight();
        j.0[7].valid = false; // the index finger's proximal joint: bones 6-7 and 7-8 go with it
        let items = joint_items(Hand::Right, &j, &rig(1.0));
        let spheres = items.iter().filter(|i| matches!(i.shape, Shape::Sphere { .. })).count();
        let capsules = items.iter().filter(|i| matches!(i.shape, Shape::Capsule { .. })).count();
        assert_eq!((spheres, capsules), (25, 22));
    }

    /// A bone runs from one joint's centre to the other's, through the rig's turn and scale.
    #[test]
    fn a_bone_spans_its_two_joints_through_the_rig() {
        let mut j = [HandJoint::default(); HandJoints::COUNT];
        j[1] = HandJoint { position: [0.3, 1.2, 0.0], radius: 0.02, valid: true };
        j[2] = HandJoint { position: [0.3, 1.2, -0.1], radius: 0.01, valid: true };
        let s = 2.0;
        let items = joint_items(Hand::Left, &HandJoints(j), &rig(s));
        let bone = items.iter().find(|i| matches!(i.shape, Shape::Capsule { .. })).expect("one bone");
        let Shape::Capsule { radius, half_height } = bone.shape else { unreachable!() };
        // 0.7 of the 1 cm joint is 7 mm: aspect 0.05 / 0.007 = 7.1, rounded up to 8, so the radius is 6.25 mm.
        assert!((half_height - 0.05).abs() < 1e-6 && (radius - 0.00625).abs() < 1e-6, "{radius}");
        assert_eq!(bone.scale, [s; 3]);
        // The rig turns +90° about Y (rig −Z to world −X), origin (10, 0, −3), scale 2: the joints land at
        // (10, 2.4, −3.6) and (9.8, 2.4, −3.6); the bone's middle between them, its +Y along world −X.
        let mid = offset_of(bone.pose.pos);
        assert!((mid - DVec3::new(9.9, 2.4, -3.6)).length() < 1e-6, "mid {mid:?}");
        let axis = bone.pose.rot * DVec3::Y;
        assert!((axis - DVec3::NEG_X).length() < 1e-6, "axis {axis:?}");
        assert!((bone.radius - (0.05 + 0.00625) * s).abs() < 1e-6);
    }

    /// A jittering hand keeps to a few capsule meshes: every bone's aspect is a whole number up to
    /// MAX_BONE_ASPECT, which scene.rs keys exactly (ratio × 64), and no bone is wider than BONE_SHARE of its joints.
    #[test]
    fn a_jittering_hand_uses_at_most_max_bone_aspect_capsule_meshes() {
        let mut seed = 0x9e37_79b9_u32;
        let mut jitter = || {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (seed >> 8) as f32 / (1u32 << 24) as f32 * 0.002 - 0.001 // ±1 mm
        };
        let mut keys = std::collections::BTreeSet::new();
        for _ in 0..100 {
            let mut j = straight();
            for joint in &mut j.0 {
                joint.position = joint.position.map(|c| c + jitter());
                joint.radius = 0.006 + jitter().abs() * 2.0;
            }
            for i in joint_items(Hand::Left, &j, &rig(1.0)) {
                let Shape::Capsule { radius, half_height } = i.shape else { continue };
                let aspect = half_height / radius;
                assert!(
                    (aspect - aspect.round()).abs() < 1e-3 && (1.0..=MAX_BONE_ASPECT as f32).contains(&aspect.round())
                );
                // Only a bone longer than MAX_BONE_ASPECT allows (none in a real hand; this hand's wrist bones) widens.
                assert!(radius <= 0.008 * BONE_SHARE + 1e-6 || aspect.round() == MAX_BONE_ASPECT as f32, "{radius}");
                keys.insert(((half_height / radius) * 64.0).round() as u32);
            }
        }
        assert!(keys.len() <= MAX_BONE_ASPECT as usize, "{keys:?}");
    }

    #[test]
    fn markers_draw_a_tracked_hand_as_its_joints() {
        let hands = [Some(DisplayHand { pose: RigPose::IDENTITY, joints: Some(straight()) }), None];
        assert_eq!(markers(&hands, &rig(1.0)).len(), 26 + 24);
    }
}
