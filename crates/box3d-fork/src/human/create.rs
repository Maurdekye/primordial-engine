//! Create a human ragdoll. Port of `CreateHuman` from `shared/human.c`.
//!
//! SPDX-FileCopyrightText: 2026 Erin Catto
//! SPDX-License-Identifier: MIT

use super::types::{
    BoneId, Human, BONE_COUNT, COLOR_DODGER_BLUE, COLOR_LIGHT_YELLOW, COLOR_MEDIUM_TURQUOISE,
    COLOR_NAVAJO_WHITE, COLOR_PERU, COLOR_TAN, FILTER_JOINT_COUNT,
};
use crate::body::create_body;
use crate::geometry::Capsule;
use crate::id::{NULL_BODY_ID, NULL_JOINT_ID};
use crate::joint::{create_filter_joint, create_revolute_joint, create_spherical_joint, JointType};
use crate::math_functions::{
    normalize_quat, offset_pos, Pos, Quat, Transform, Vec2, Vec3, DEG_TO_RAD,
};
use crate::shape::create_capsule_shape;
use crate::types::{
    default_body_def, default_filter_joint_def, default_revolute_joint_def, default_shape_def,
    default_spherical_joint_def, BodyType,
};
use crate::world::World;

/// Build a `Transform` from C aggregate initializer order `{p, {q.v, q.s}}`.
#[inline]
fn xf(px: f32, py: f32, pz: f32, qx: f32, qy: f32, qz: f32, qs: f32) -> Transform {
    Transform {
        p: Vec3 {
            x: px,
            y: py,
            z: pz,
        },
        q: Quat {
            v: Vec3 {
                x: qx,
                y: qy,
                z: qz,
            },
            s: qs,
        },
    }
}

/// Build a `Capsule` from C aggregate initializer order `{center1, center2, radius}`.
#[inline]
fn cap(x1: f32, y1: f32, z1: f32, x2: f32, y2: f32, z2: f32, radius: f32) -> Capsule {
    Capsule {
        center1: Vec3 {
            x: x1,
            y: y1,
            z: z1,
        },
        center2: Vec3 {
            x: x2,
            y: y2,
            z: z2,
        },
        radius,
    }
}

/// Spawn a capsule-bone ragdoll with spherical / revolute joint tree.
/// (CreateHuman)
#[allow(clippy::too_many_arguments)]
pub fn create_human(
    human: &mut Human,
    world: &mut World,
    position: Pos,
    friction_torque: f32,
    hertz: f32,
    damping_ratio: f32,
    group_index: i32,
    user_data: u64,
    colorize: bool,
) {
    debug_assert!(!human.is_spawned);

    for i in 0..BONE_COUNT {
        human.bones[i].body_id = NULL_BODY_ID;
        human.bones[i].anchor_id = NULL_BODY_ID;
        human.bones[i].joint_id = NULL_JOINT_ID;
        human.bones[i].joint_friction = 1.0;
        human.bones[i].parent_index = -1;
    }

    for i in 0..FILTER_JOINT_COUNT {
        human.filter_joints[i] = NULL_JOINT_ID;
    }

    human.friction_torque = friction_torque;

    let mut body_def = default_body_def();
    body_def.type_ = BodyType::Dynamic;
    body_def.user_data = user_data;

    let mut shape_def = default_shape_def();
    let default_density = shape_def.density;

    let shirt_color = COLOR_MEDIUM_TURQUOISE;
    let pant_color = COLOR_DODGER_BLUE;
    let skin_colors = [
        COLOR_NAVAJO_WHITE,
        COLOR_LIGHT_YELLOW,
        COLOR_PERU,
        COLOR_TAN,
    ];
    let skin_color = skin_colors[(group_index.rem_euclid(4)) as usize];

    {
        let bone = &mut human.bones[BoneId::Pelvis as usize];
        bone.parent_index = -1;

        body_def.name = "pelvis".to_string();
        bone.reference_frame = xf(
            0.000000, 0.996219, -0.023868, 1.000000, 0.000000, 0.000000, 0.000000,
        );
        body_def.rotation = bone.reference_frame.q;
        body_def.position = offset_pos(position, bone.reference_frame.p);
        bone.body_id = create_body(world, &body_def);

        shape_def.filter.group_index = 0;
        shape_def.base_material.custom_color = if colorize { pant_color } else { 0 };

        shape_def.base_material.friction = 0.6;
        shape_def.base_material.rolling_resistance = 0.1;
        let capsule = cap(
            0.040000, 0.000001, 0.000000, -0.040000, -0.000001, 0.000000, 0.150000,
        );
        create_capsule_shape(world, bone.body_id, &shape_def, &capsule);
    }

    {
        let bone = &mut human.bones[BoneId::Spine01 as usize];
        bone.parent_index = BoneId::Pelvis as i32;

        body_def.name = "spine_01".to_string();
        bone.reference_frame = xf(
            0.000000, 1.017288, -0.024882, 1.000000, 0.000000, 0.000000, 0.000000,
        );
        body_def.rotation = bone.reference_frame.q;
        body_def.position = offset_pos(position, bone.reference_frame.p);
        bone.body_id = create_body(world, &body_def);

        shape_def.filter.group_index = -group_index;
        shape_def.base_material.custom_color = if colorize { shirt_color } else { 0 };

        shape_def.base_material.friction = 0.5;
        shape_def.base_material.rolling_resistance = 0.1;
        let capsule = cap(
            0.029876, -0.146581, 0.006260, -0.029876, -0.146574, 0.006260, 0.145663,
        );
        create_capsule_shape(world, bone.body_id, &shape_def, &capsule);

        bone.joint_type = JointType::Spherical;
        bone.local_frame_a = xf(
            0.000000, -0.021069, 0.001014, -0.642737, 0.000000, 0.000000, -0.766087,
        );
        bone.local_frame_b = xf(0.0, 0.0, 0.0, -0.707107, 0.000000, 0.000000, -0.707107);
        bone.swing_limit = 35.0 * DEG_TO_RAD;
        bone.twist_limit = Vec2 {
            x: -17.5 * DEG_TO_RAD,
            y: 17.5 * DEG_TO_RAD,
        };
    }

    {
        let bone = &mut human.bones[BoneId::Spine03 as usize];
        bone.parent_index = BoneId::Spine01 as i32;

        body_def.name = "spine_03".to_string();
        bone.reference_frame = xf(
            0.000000, 1.267766, -0.022320, 1.000000, 0.000000, 0.000000, 0.000000,
        );
        body_def.rotation = bone.reference_frame.q;
        body_def.position = offset_pos(position, bone.reference_frame.p);
        bone.body_id = create_body(world, &body_def);

        shape_def.filter.group_index = 0;
        shape_def.base_material.custom_color = if colorize { shirt_color } else { 0 };

        shape_def.base_material.friction = 0.5;
        shape_def.base_material.rolling_resistance = 0.1;
        let capsule = cap(
            0.063996, -0.117434, -0.040199, -0.063996, -0.117432, -0.040199, 0.165004,
        );
        create_capsule_shape(world, bone.body_id, &shape_def, &capsule);

        bone.joint_type = JointType::Spherical;
        bone.local_frame_a = xf(
            0.000000, -0.250478, -0.002562, -0.642766, 0.000000, 0.000000, -0.766063,
        );
        bone.local_frame_b = xf(0.0, 0.0, 0.0, -0.707107, 0.000000, 0.000000, -0.707107);
        bone.swing_limit = 35.0 * DEG_TO_RAD;
        bone.twist_limit = Vec2 {
            x: -17.5 * DEG_TO_RAD,
            y: 17.5 * DEG_TO_RAD,
        };
    }

    {
        let bone = &mut human.bones[BoneId::Neck as usize];
        bone.parent_index = BoneId::Spine03 as i32;

        body_def.name = "neck_01".to_string();
        bone.reference_frame = xf(
            0.000000, 1.498783, 0.024462, 0.987879, 0.000000, 0.000000, 0.155228,
        );
        body_def.rotation = bone.reference_frame.q;
        body_def.position = offset_pos(position, bone.reference_frame.p);
        bone.body_id = create_body(world, &body_def);

        shape_def.filter.group_index = 0;
        shape_def.base_material.custom_color = if colorize { skin_color } else { 0 };

        shape_def.base_material.friction = 0.2;
        shape_def.base_material.rolling_resistance = 0.05;
        let head = cap(
            0.000000, -0.212340, 0.000000, 0.000000, -0.087340, 0.000000, 0.110000,
        );
        create_capsule_shape(world, bone.body_id, &shape_def, &head);

        bone.joint_type = JointType::Spherical;
        bone.local_frame_a = xf(
            0.000000, -0.231017, -0.046781, -0.516157, -0.000002, 0.000003, -0.856494,
        );
        bone.local_frame_b = xf(0.0, 0.0, 0.0, -0.707108, -0.000002, 0.000002, -0.707106);
        bone.swing_limit = 30.0 * DEG_TO_RAD;
        bone.twist_limit = Vec2 {
            x: -17.5 * DEG_TO_RAD,
            y: 17.5 * DEG_TO_RAD,
        };
    }

    {
        let bone = &mut human.bones[BoneId::ThighL as usize];
        bone.parent_index = BoneId::Pelvis as i32;

        body_def.name = "thigh_l".to_string();
        bone.reference_frame = xf(
            0.092175, 0.971562, -0.011177, 0.471412, 0.529510, -0.499888, -0.497495,
        );
        body_def.rotation = bone.reference_frame.q;
        body_def.position = offset_pos(position, bone.reference_frame.p);
        bone.body_id = create_body(world, &body_def);

        shape_def.filter.group_index = -group_index;
        shape_def.base_material.custom_color = if colorize { pant_color } else { 0 };

        shape_def.base_material.friction = 0.6;
        shape_def.base_material.rolling_resistance = 0.1;
        let capsule = cap(
            -0.047269, 0.000001, 0.000000, -0.379769, 0.000005, 0.000000, 0.091539,
        );
        create_capsule_shape(world, bone.body_id, &shape_def, &capsule);

        bone.joint_type = JointType::Spherical;
        bone.local_frame_a = xf(
            0.092175, 0.024656, -0.012691, -0.460186, -0.365368, -0.637662, 0.498118,
        );
        bone.local_frame_b = xf(0.0, 0.0, 0.0, -0.499997, -0.500003, 0.500002, 0.499998);
        bone.swing_limit = 30.0 * DEG_TO_RAD;
        bone.twist_limit = Vec2 {
            x: -10.0 * DEG_TO_RAD,
            y: 10.0 * DEG_TO_RAD,
        };
    }

    {
        let bone = &mut human.bones[BoneId::CalfL as usize];
        bone.parent_index = BoneId::ThighL as i32;

        body_def.name = "calf_l".to_string();
        bone.reference_frame = xf(
            0.118720, 0.534541, -0.035362, 0.521188, 0.480597, -0.546380, -0.445935,
        );
        body_def.rotation = bone.reference_frame.q;
        body_def.position = offset_pos(position, bone.reference_frame.p);
        bone.body_id = create_body(world, &body_def);

        shape_def.filter.group_index = 0;
        shape_def.base_material.custom_color = if colorize { pant_color } else { 0 };

        shape_def.base_material.friction = 0.1;
        shape_def.base_material.rolling_resistance = 0.0;
        let shin = cap(
            -0.445000, 0.000001, 0.000000, 0.005000, -0.000001, 0.000000, 0.080000,
        );
        create_capsule_shape(world, bone.body_id, &shape_def, &shin);

        shape_def.density = 0.5 * default_density;
        let foot = cap(
            -0.456371, 0.129321, 0.000013, -0.359977, 0.014394, 0.000013, 0.070000,
        );
        create_capsule_shape(world, bone.body_id, &shape_def, &foot);
        shape_def.density = default_density;

        bone.joint_type = JointType::Revolute;
        bone.local_frame_a = xf(
            -0.438494, -0.000175, 0.000000, 0.344866, -0.938652, 0.000002, -0.000008,
        );
        bone.local_frame_b = xf(0.0, 0.0, 0.0, -0.008182, -0.999967, 0.000001, -0.000007);
        bone.twist_limit = Vec2 {
            x: -30.0 * DEG_TO_RAD,
            y: 30.0 * DEG_TO_RAD,
        };
    }

    {
        let bone = &mut human.bones[BoneId::ThighR as usize];
        bone.parent_index = BoneId::Pelvis as i32;

        body_def.name = "thigh_r".to_string();
        bone.reference_frame = xf(
            -0.092175, 0.971562, -0.011177, -0.497495, 0.499888, 0.529510, -0.471412,
        );
        body_def.rotation = bone.reference_frame.q;
        body_def.position = offset_pos(position, bone.reference_frame.p);
        bone.body_id = create_body(world, &body_def);

        shape_def.filter.group_index = -group_index;
        shape_def.base_material.custom_color = if colorize { pant_color } else { 0 };

        shape_def.base_material.friction = 0.6;
        shape_def.base_material.rolling_resistance = 0.1;
        let capsule = cap(
            0.047269, -0.000001, 0.000000, 0.379769, -0.000005, 0.000000, 0.092587,
        );
        create_capsule_shape(world, bone.body_id, &shape_def, &capsule);

        bone.joint_type = JointType::Spherical;
        bone.local_frame_a = xf(
            -0.092175, 0.024656, -0.012691, 0.380666, 0.491846, 0.488644, -0.611889,
        );
        bone.local_frame_b = xf(0.0, 0.0, 0.0, -0.500002, 0.499998, -0.499998, 0.500002);
        bone.swing_limit = 30.0 * DEG_TO_RAD;
        bone.twist_limit = Vec2 {
            x: -10.0 * DEG_TO_RAD,
            y: 10.0 * DEG_TO_RAD,
        };
    }

    {
        let bone = &mut human.bones[BoneId::CalfR as usize];
        bone.parent_index = BoneId::ThighR as i32;

        body_def.name = "calf_r".to_string();
        bone.reference_frame = xf(
            -0.118720, 0.534541, -0.035362, -0.445935, 0.546380, 0.480597, -0.521188,
        );
        body_def.rotation = bone.reference_frame.q;
        body_def.position = offset_pos(position, bone.reference_frame.p);
        bone.body_id = create_body(world, &body_def);

        shape_def.filter.group_index = 0;
        shape_def.base_material.custom_color = if colorize { pant_color } else { 0 };

        shape_def.base_material.friction = 0.1;
        shape_def.base_material.rolling_resistance = 0.0;
        let shin = cap(
            0.445000, -0.000002, 0.000003, -0.005000, 0.000000, 0.000003, 0.080000,
        );
        create_capsule_shape(world, bone.body_id, &shape_def, &shin);

        shape_def.density = 0.5 * default_density;
        let foot = cap(
            0.369113, -0.006830, 0.000017, 0.465507, -0.121757, 0.000017, 0.070000,
        );
        create_capsule_shape(world, bone.body_id, &shape_def, &foot);
        shape_def.density = default_density;

        bone.joint_type = JointType::Revolute;
        bone.local_frame_a = xf(
            0.438494, 0.000175, 0.000000, 0.000007, 0.000005, -0.344860, -0.938654,
        );
        bone.local_frame_b = xf(0.0, 0.0, 0.0, 0.000006, 0.000005, 0.008188, -0.999966);
        bone.twist_limit = Vec2 {
            x: -30.0 * DEG_TO_RAD,
            y: 30.0 * DEG_TO_RAD,
        };
    }

    {
        let bone = &mut human.bones[BoneId::UpperArmL as usize];
        bone.parent_index = BoneId::Spine03 as i32;

        body_def.name = "upperarm_l".to_string();
        bone.reference_frame = xf(
            0.185817, 1.443630, 0.031306, 0.560220, -0.307179, 0.366233, -0.676512,
        );
        body_def.rotation = bone.reference_frame.q;
        body_def.position = offset_pos(position, bone.reference_frame.p);
        bone.body_id = create_body(world, &body_def);

        shape_def.filter.group_index = 0;
        shape_def.base_material.custom_color = if colorize { shirt_color } else { 0 };

        shape_def.base_material.friction = 0.6;
        shape_def.base_material.rolling_resistance = 0.1;
        let capsule = cap(
            0.296038, 0.004386, -0.000794, -0.016086, 0.004384, -0.016127, 0.065000,
        );
        create_capsule_shape(world, bone.body_id, &shape_def, &capsule);

        bone.joint_type = JointType::Spherical;
        bone.local_frame_a = xf(
            0.185817, -0.175865, -0.053625, -0.559466, -0.213784, -0.670199, 0.438323,
        );
        bone.local_frame_b = xf(0.0, 0.0, 0.0, -0.500953, -0.502617, -0.509318, -0.486844);
        bone.swing_limit = 45.0 * DEG_TO_RAD;
        bone.twist_limit = Vec2 {
            x: -20.0 * DEG_TO_RAD,
            y: 20.0 * DEG_TO_RAD,
        };
        bone.joint_friction = 0.8;
    }

    {
        let bone = &mut human.bones[BoneId::LowerArmL as usize];
        bone.parent_index = BoneId::UpperArmL as i32;

        body_def.name = "lowerarm_l".to_string();
        bone.reference_frame = xf(
            0.347683, 1.193455, 0.029378, 0.406736, -0.493012, 0.089135, -0.763911,
        );
        body_def.rotation = bone.reference_frame.q;
        body_def.position = offset_pos(position, bone.reference_frame.p);
        bone.body_id = create_body(world, &body_def);

        shape_def.filter.group_index = 0;
        shape_def.base_material.custom_color = if colorize { skin_color } else { 0 };

        shape_def.base_material.friction = 0.3;
        shape_def.base_material.rolling_resistance = 0.1;
        let capsule = cap(
            0.312329, -0.008256, -0.016454, 0.033153, 0.008259, -0.002741, 0.060000,
        );
        create_capsule_shape(world, bone.body_id, &shape_def, &capsule);

        bone.joint_type = JointType::Revolute;
        bone.local_frame_a = xf(
            0.297979, 0.000361, 0.000000, 0.916726, 0.399352, 0.000839, -0.011456,
        );
        bone.local_frame_b = xf(0.0, 0.0, 0.0, 0.999922, -0.003857, 0.004659, -0.010908);
        bone.twist_limit = Vec2 {
            x: -35.0 * DEG_TO_RAD,
            y: 35.0 * DEG_TO_RAD,
        };
        bone.joint_friction = 0.06;
    }

    {
        let bone = &mut human.bones[BoneId::UpperArmR as usize];
        bone.parent_index = BoneId::Spine03 as i32;

        body_def.name = "upperarm_r".to_string();
        bone.reference_frame = xf(
            -0.185817, 1.443630, 0.031306, 0.676512, 0.366233, 0.307179, 0.560220,
        );
        body_def.rotation = bone.reference_frame.q;
        body_def.position = offset_pos(position, bone.reference_frame.p);
        bone.body_id = create_body(world, &body_def);

        shape_def.filter.group_index = 0;
        shape_def.base_material.custom_color = if colorize { shirt_color } else { 0 };

        shape_def.base_material.friction = 0.6;
        shape_def.base_material.rolling_resistance = 0.1;
        let capsule = cap(
            -0.300237, -0.000001, -0.000319, 0.011887, 0.000001, 0.015013, 0.065000,
        );
        create_capsule_shape(world, bone.body_id, &shape_def, &capsule);

        bone.joint_type = JointType::Spherical;
        bone.local_frame_a = xf(
            -0.185817, -0.175865, -0.053625, 0.213793, 0.559449, 0.438334, -0.670204,
        );
        bone.local_frame_b = xf(0.0, 0.0, 0.0, -0.509319, 0.486843, 0.500954, -0.502617);
        bone.swing_limit = 45.0 * DEG_TO_RAD;
        bone.twist_limit = Vec2 {
            x: -20.0 * DEG_TO_RAD,
            y: 20.0 * DEG_TO_RAD,
        };
        bone.joint_friction = 0.8;
    }

    {
        let bone = &mut human.bones[BoneId::LowerArmR as usize];
        bone.parent_index = BoneId::UpperArmR as i32;

        body_def.name = "lowerarm_r".to_string();
        bone.reference_frame = xf(
            -0.347683, 1.193455, 0.029378, 0.763911, 0.089135, 0.493012, 0.406736,
        );
        body_def.rotation = bone.reference_frame.q;
        body_def.position = offset_pos(position, bone.reference_frame.p);
        bone.body_id = create_body(world, &body_def);

        shape_def.filter.group_index = 0;
        shape_def.base_material.custom_color = if colorize { skin_color } else { 0 };

        shape_def.base_material.friction = 0.3;
        shape_def.base_material.rolling_resistance = 0.1;
        let capsule = cap(
            -0.312649, -0.000003, 0.016471, -0.032986, 0.000000, 0.002732, 0.060000,
        );
        create_capsule_shape(world, bone.body_id, &shape_def, &capsule);

        bone.joint_type = JointType::Revolute;
        bone.local_frame_a = xf(
            -0.297979, -0.000361, 0.000000, -0.405444, 0.914059, 0.010541, 0.000852,
        );
        bone.local_frame_b = xf(0.0, 0.0, 0.0, -0.002794, 0.999936, 0.010042, 0.004364);
        bone.twist_limit = Vec2 {
            x: -35.0 * DEG_TO_RAD,
            y: 35.0 * DEG_TO_RAD,
        };
        bone.joint_friction = 0.06;
    }

    create_bone_joints(human, world, friction_torque, hertz, damping_ratio);

    // Disable some collisions
    human.filter_joint_count = 0;
    let mut filter_def = default_filter_joint_def();
    filter_def.base.body_id_a = human.bones[BoneId::ThighL as usize].body_id;
    filter_def.base.body_id_b = human.bones[BoneId::ThighR as usize].body_id;
    human.filter_joints[human.filter_joint_count as usize] =
        create_filter_joint(world, &filter_def);
    human.filter_joint_count += 1;
    filter_def.base.body_id_a = human.bones[BoneId::Neck as usize].body_id;
    filter_def.base.body_id_b = human.bones[BoneId::UpperArmL as usize].body_id;
    human.filter_joints[human.filter_joint_count as usize] =
        create_filter_joint(world, &filter_def);
    human.filter_joint_count += 1;
    filter_def.base.body_id_a = human.bones[BoneId::Neck as usize].body_id;
    filter_def.base.body_id_b = human.bones[BoneId::UpperArmR as usize].body_id;
    human.filter_joints[human.filter_joint_count as usize] =
        create_filter_joint(world, &filter_def);
    human.filter_joint_count += 1;

    human.is_spawned = true;
}

fn create_bone_joints(
    human: &mut Human,
    world: &mut World,
    friction_torque: f32,
    hertz: f32,
    damping_ratio: f32,
) {
    for i in 1..BONE_COUNT {
        let parent_index = human.bones[i].parent_index as usize;
        let body_id_a = human.bones[parent_index].body_id;
        let body_id_b = human.bones[i].body_id;

        human.bones[i].local_frame_a.q = normalize_quat(human.bones[i].local_frame_a.q);
        human.bones[i].local_frame_b.q = normalize_quat(human.bones[i].local_frame_b.q);

        let joint_type = human.bones[i].joint_type;
        let local_frame_a = human.bones[i].local_frame_a;
        let local_frame_b = human.bones[i].local_frame_b;
        let twist_limit = human.bones[i].twist_limit;
        let swing_limit = human.bones[i].swing_limit;
        let joint_friction = human.bones[i].joint_friction;

        if joint_type == JointType::Revolute {
            let mut joint_def = default_revolute_joint_def();
            joint_def.base.body_id_a = body_id_a;
            joint_def.base.body_id_b = body_id_b;
            joint_def.base.local_frame_a = local_frame_a;
            joint_def.base.local_frame_b = local_frame_b;
            joint_def.enable_limit = true;
            joint_def.lower_angle = twist_limit.x;
            joint_def.upper_angle = twist_limit.y;
            joint_def.enable_spring = hertz > 0.0;
            joint_def.hertz = hertz;
            joint_def.damping_ratio = damping_ratio;
            joint_def.enable_motor = true;
            joint_def.max_motor_torque = joint_friction * friction_torque;
            human.bones[i].joint_id = create_revolute_joint(world, &joint_def);
        } else if joint_type == JointType::Spherical {
            let mut joint_def = default_spherical_joint_def();
            joint_def.base.body_id_a = body_id_a;
            joint_def.base.body_id_b = body_id_b;
            joint_def.base.local_frame_a = local_frame_a;
            joint_def.base.local_frame_b = local_frame_b;
            joint_def.enable_cone_limit = true;
            joint_def.cone_angle = swing_limit;
            joint_def.enable_twist_limit = true;
            joint_def.lower_twist_angle = twist_limit.x;
            joint_def.upper_twist_angle = twist_limit.y;
            joint_def.enable_spring = hertz > 0.0;
            joint_def.hertz = hertz;
            joint_def.damping_ratio = damping_ratio;
            joint_def.enable_motor = true;
            joint_def.max_motor_torque = joint_friction * friction_torque;
            human.bones[i].joint_id = create_spherical_joint(world, &joint_def);
        }
    }
}
