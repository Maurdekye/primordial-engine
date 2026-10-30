//! Post-solve restitution for Mesh (and overflow) contact constraints. (b3ApplyRestitution_Mesh)
//!
//! SPDX-FileCopyrightText: 2025 Erin Catto
//! SPDX-License-Identifier: MIT

use super::ContactConstraint;
use crate::body::{body_flags, BodyState, IDENTITY_BODY_STATE};
use crate::core::NULL_INDEX;
use crate::math_functions::{
    add, cross, dot, max_float, min_float, mul_add, mul_mv, mul_sub, mul_sv, rotate_vector, sub,
};
use crate::solver::StepContext;

/// Poisson kinetic restitution. (b3ApplyRestitution_Mesh)
pub fn apply_restitution(
    constraints: &mut [ContactConstraint],
    states: &mut [BodyState],
    context: &StepContext,
    restitution_threshold: f32,
    propagate: bool,
) {
    let threshold = restitution_threshold;
    let inv_h = context.inv_h;

    for contact_constraint in constraints.iter_mut() {
        let restitution = contact_constraint.restitution;
        if !propagate && restitution == 0.0 {
            continue;
        }

        let manifold_count = contact_constraint.manifold_count;

        let index_a = contact_constraint.index_a;
        let index_b = contact_constraint.index_b;

        let m_a = contact_constraint.inv_mass_a;
        let i_a = contact_constraint.inv_i_a;
        let m_b = contact_constraint.inv_mass_b;
        let i_b = contact_constraint.inv_i_b;

        let mut state_a = if index_a == NULL_INDEX {
            IDENTITY_BODY_STATE
        } else {
            states[index_a as usize]
        };
        let mut v_a = state_a.linear_velocity;
        let mut w_a = state_a.angular_velocity;
        let dq_a = state_a.delta_rotation;

        let mut state_b = if index_b == NULL_INDEX {
            IDENTITY_BODY_STATE
        } else {
            states[index_b as usize]
        };
        let mut v_b = state_b.linear_velocity;
        let mut w_b = state_b.angular_velocity;
        let dq_b = state_b.delta_rotation;

        let dp = sub(state_b.delta_position, state_a.delta_position);

        for j in 0..manifold_count as usize {
            let constraint = &mut contact_constraint.constraints[j];

            let point_count = constraint.point_count as usize;
            let normal = constraint.normal;

            for point_index in 0..point_count {
                let cp = &mut constraint.points[point_index];

                let r_a = cp.r_a;
                let r_b = cp.r_b;

                // The total normal impulse is 0 for speculative points.
                let compression_impulse = cp.total_normal_impulse - cp.restitution_impulse;
                let armed = restitution > 0.0
                    && cp.relative_velocity < -threshold
                    && compression_impulse > 0.0;

                let velocity_bias = if armed {
                    restitution * cp.relative_velocity
                } else {
                    let ds = add(dp, sub(rotate_vector(dq_b, r_b), rotate_vector(dq_a, r_a)));
                    let s = dot(ds, normal) + cp.base_separation;

                    if s > 0.0 {
                        s * inv_h
                    } else {
                        0.0
                    }
                };

                let vr_a = add(v_a, cross(w_a, r_a));
                let vr_b = add(v_b, cross(w_b, r_b));
                let vn = dot(sub(vr_b, vr_a), normal);

                let mut impulse = -cp.normal_mass * (vn + velocity_bias);

                let new_impulse = max_float(cp.normal_impulse + impulse, 0.0);
                impulse = new_impulse - cp.normal_impulse;

                let approach_impulse = min_float(
                    max_float(-cp.normal_mass * vn, 0.0),
                    max_float(impulse, 0.0),
                );

                if armed {
                    // Poisson kinetic restitution guarantees no energy gain.
                    let allowance = restitution * (compression_impulse + approach_impulse)
                        - cp.restitution_impulse;
                    impulse = min_float(impulse, approach_impulse + max_float(allowance, 0.0));
                }

                cp.normal_impulse += impulse;
                cp.restitution_impulse += impulse - approach_impulse;
                cp.total_normal_impulse += impulse;

                let p = mul_sv(impulse, normal);
                v_a = mul_sub(v_a, m_a, p);
                w_a = sub(w_a, mul_mv(i_a, cross(r_a, p)));

                v_b = mul_add(v_b, m_b, p);
                w_b = add(w_b, mul_mv(i_b, cross(r_b, p)));
            }
        }

        if (state_a.flags & body_flags::DYNAMIC_FLAG) != 0 {
            state_a.linear_velocity = v_a;
            state_a.angular_velocity = w_a;
            states[index_a as usize] = state_a;
        }

        if (state_b.flags & body_flags::DYNAMIC_FLAG) != 0 {
            state_b.linear_velocity = v_b;
            state_b.angular_velocity = w_b;
            states[index_b as usize] = state_b;
        }
    }
}
