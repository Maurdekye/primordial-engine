//! Prepare wide contact constraints. (b3PrepareConstraintW, b3PrepareContacts_Convex,
//! b3PrepareContacts_MeshWide)
//!
//! SPDX-FileCopyrightText: 2025 Erin Catto
//! SPDX-License-Identifier: MIT

use super::{
    add_sym_w, gather_bodies, ints_to_float_w, invert_sym_w, lane, mul_mvw, perp_w,
    ContactConstraintPointWide, ContactConstraintWide, ContactManifoldWide, SymMatrix2W,
    SymMatrix3W, Vec2W, WideContactConstraints, WideView, EMPTY_LANE, SYM3W_ZERO, VEC3W_ZERO,
};
use crate::worker_pool::SharedSlice;
use crate::body::{decode_awake_index, BodySim, BodyState};
use crate::constants::{speculative_distance, MAX_MANIFOLD_POINTS, MIN_FRICTION_WEIGHT};
use crate::contact::{contact_flags, Contact, ContactSpec};
use crate::core::{NULL_INDEX, SIMD_WIDTH};
use crate::manifold::{Manifold, ManifoldPoint};
use crate::math_functions::max_int;
use crate::simd_scalar::{
    abs_w, add_vw, add_w, all_zero_w, and_w, any_true_w, blend_w, cross_w, div_w, dot_w,
    greater_than_w, max_w, min_w, mul_add_svw, mul_svw, mul_w, neg_w, set_w, splat_w, sqrt_w,
    sub_vw, sub_w, zero_w, FloatW, Vec3W,
};

macro_rules! gather_lanes {
    (|$l:ident| $field:expr) => {{
        let f = |$l: usize| $field;
        set_w(f(0), f(1), f(2), f(3))
    }};
}

/// (b3GatherInvInertiaW)
fn gather_inv_inertia_w(sims: &[&BodySim; SIMD_WIDTH]) -> SymMatrix3W {
    SymMatrix3W {
        cxx: gather_lanes!(|l| sims[l].inv_inertia_world.cx.x),
        cxy: gather_lanes!(|l| sims[l].inv_inertia_world.cx.y),
        cxz: gather_lanes!(|l| sims[l].inv_inertia_world.cx.z),
        cyy: gather_lanes!(|l| sims[l].inv_inertia_world.cy.y),
        cyz: gather_lanes!(|l| sims[l].inv_inertia_world.cy.z),
        czz: gather_lanes!(|l| sims[l].inv_inertia_world.cz.z),
    }
}

const EMPTY_POINT: ContactConstraintPointWide = ContactConstraintPointWide {
    anchor_as: VEC3W_ZERO,
    anchor_bs: VEC3W_ZERO,
    base_separations: VEC3W_ZERO.x,
    normal_impulses: VEC3W_ZERO.x,
    total_normal_impulses: VEC3W_ZERO.x,
    normal_masses: VEC3W_ZERO.x,
    lever_arms: VEC3W_ZERO.x,
    relative_velocities: VEC3W_ZERO.x,
    restitution_impulses: VEC3W_ZERO.x,
};

/// Prepare one wide constraint with `manifold_count` manifolds and append it (followed by its
/// manifolds) to `set`. Returns true if any lane has restitution. (b3PrepareConstraintW)
///
/// Contact constraint data is stored in a heterogeneous stream in C. Constraints followed by an
/// array of constraint manifolds. See [`WideContactConstraints`].
///
/// Writes the sampled normal velocity onto the manifold points of lanes that enable hit
/// events, like C.
#[allow(clippy::too_many_arguments)]
///
/// Primordial Engine fork (D2): writes `constraint_out` and `manifolds_out` (exactly
/// `manifold_count` slots) instead of appending, so the constraints of a color can be prepared
/// in parallel blocks. Reads only the lane contacts and writes only their manifold points.
///
/// # Safety
/// No other thread accesses the lane contacts during the call.
unsafe fn prepare_constraint_w(
    constraint_out: &mut ContactConstraintWide,
    manifolds_out: &mut [ContactManifoldWide],
    lane_contact_ids: &[i32; SIMD_WIDTH],
    manifold_count: i32,
    contacts: SharedSlice<Contact>,
    sims: &[BodySim],
    states: &[BodyState],
    warm_start_scale: FloatW,
) -> bool {
    debug_assert!(manifolds_out.len() == manifold_count as usize);
    let zero_sim = BodySim {
        inv_mass: 0.0,
        inv_inertia_world: crate::math_functions::MAT3_ZERO,
        ..BodySim::default()
    };
    let zero_manifold = Manifold::default();

    let zero = zero_w();
    let one_w = splat_w(1.0);
    let two_w = splat_w(2.0);
    let inv_tau = splat_w(1.0 / speculative_distance());
    let min_friction_weight = splat_w(MIN_FRICTION_WEIGHT);
    let min_det = splat_w(1000.0 * f32::MIN_POSITIVE);

    let mut index_a = [0i32; SIMD_WIDTH];
    let mut index_b = [0i32; SIMD_WIDTH];
    let mut sim_index_a = [NULL_INDEX; SIMD_WIDTH];
    let mut sim_index_b = [NULL_INDEX; SIMD_WIDTH];
    let mut manifold_counts = [0i32; SIMD_WIDTH];
    let mut hit_event_lanes = 0u32;
    let mut max_manifold_count = 0;

    for lane_index in 0..SIMD_WIDTH {
        let contact_id = lane_contact_ids[lane_index];
        if contact_id != EMPTY_LANE {
            // SAFETY: the function contract.
            let contact = unsafe { contacts.get(contact_id as usize) };
            let a = decode_awake_index(contact.encoded_body_sim_a);
            let b = decode_awake_index(contact.encoded_body_sim_b);

            index_a[lane_index] = a + 1;
            index_b[lane_index] = b + 1;
            manifold_counts[lane_index] = contact.manifold_count();

            sim_index_a[lane_index] = a;
            sim_index_b[lane_index] = b;
            if (contact.flags & contact_flags::SIM_ENABLE_HIT_EVENT) != 0 {
                hit_event_lanes |= 1 << lane_index;
            }
            max_manifold_count = max_int(max_manifold_count, contact.manifold_count());
        }
    }

    debug_assert!(max_manifold_count == manifold_count);

    let sim_lanes_a: [&BodySim; SIMD_WIDTH] = core::array::from_fn(|i| {
        if sim_index_a[i] == NULL_INDEX {
            &zero_sim
        } else {
            &sims[sim_index_a[i] as usize]
        }
    });
    let sim_lanes_b: [&BodySim; SIMD_WIDTH] = core::array::from_fn(|i| {
        if sim_index_b[i] == NULL_INDEX {
            &zero_sim
        } else {
            &sims[sim_index_b[i] as usize]
        }
    });
    let contact_lanes: [Option<&Contact>; SIMD_WIDTH] = core::array::from_fn(|i| {
        if lane_contact_ids[i] == EMPTY_LANE {
            None
        } else {
            // SAFETY: the function contract.
            Some(unsafe { contacts.get(lane_contact_ids[i] as usize) })
        }
    });

    let m_a = gather_lanes!(|l| sim_lanes_a[l].inv_mass);
    let m_b = gather_lanes!(|l| sim_lanes_b[l].inv_mass);
    let i_a = gather_inv_inertia_w(&sim_lanes_a);
    let i_b = gather_inv_inertia_w(&sim_lanes_b);

    // An empty lane reads b3_zeroContact.
    let contact_field = |l: usize, f: fn(&Contact) -> f32| match contact_lanes[l] {
        Some(c) => f(c),
        None => 0.0,
    };
    let friction = gather_lanes!(|l| contact_field(l, |c| c.friction));
    let rolling_resistance = gather_lanes!(|l| contact_field(l, |c| c.rolling_resistance));
    let restitution = gather_lanes!(|l| contact_field(l, |c| c.restitution));
    let tangent_velocity = Vec3W {
        x: gather_lanes!(|l| contact_field(l, |c| c.tangent_velocity.x)),
        y: gather_lanes!(|l| contact_field(l, |c| c.tangent_velocity.y)),
        z: gather_lanes!(|l| contact_field(l, |c| c.tangent_velocity.z)),
    };

    let inv_i_ab = add_sym_w(i_a, i_b);
    let rolling_mass = if !all_zero_w(rolling_resistance) {
        invert_sym_w(inv_i_ab)
    } else {
        SYM3W_ZERO
    };

    let rolling_resistance_mask = greater_than_w(rolling_resistance, zero);


    for manifold_index in 0..manifold_count as usize {
        let mut active_lanes = [0i32; SIMD_WIDTH];
        let mut point_counts = [0i32; SIMD_WIDTH];
        let mut point_count = 0;
        let manifold_lanes: [&Manifold; SIMD_WIDTH] = core::array::from_fn(|l| {
            if (manifold_index as i32) < manifold_counts[l] {
                match contact_lanes[l] {
                    Some(c) => &c.manifolds[manifold_index],
                    None => &zero_manifold,
                }
            } else {
                &zero_manifold
            }
        });
        for l in 0..SIMD_WIDTH {
            if (manifold_index as i32) < manifold_counts[l] {
                active_lanes[l] = 1;
                point_counts[l] = manifold_lanes[l].point_count;
            }

            point_count = max_int(point_count, point_counts[l]);
        }

        debug_assert!(0 <= point_count && point_count as usize <= MAX_MANIFOLD_POINTS);
        let rolling_mask = and_w(
            rolling_resistance_mask,
            greater_than_w(ints_to_float_w(&active_lanes), zero),
        );

        let normal = Vec3W {
            x: gather_lanes!(|l| manifold_lanes[l].normal.x),
            y: gather_lanes!(|l| manifold_lanes[l].normal.y),
            z: gather_lanes!(|l| manifold_lanes[l].normal.z),
        };
        let twist_impulse = gather_lanes!(|l| manifold_lanes[l].twist_impulse);
        let friction_impulse = Vec3W {
            x: gather_lanes!(|l| manifold_lanes[l].friction_impulse.x),
            y: gather_lanes!(|l| manifold_lanes[l].friction_impulse.y),
            z: gather_lanes!(|l| manifold_lanes[l].friction_impulse.z),
        };
        let rolling_impulse = Vec3W {
            x: gather_lanes!(|l| manifold_lanes[l].rolling_impulse.x),
            y: gather_lanes!(|l| manifold_lanes[l].rolling_impulse.y),
            z: gather_lanes!(|l| manifold_lanes[l].rolling_impulse.z),
        };

        let tangent1 = perp_w(normal);
        let tangent2 = cross_w(tangent1, normal);
        let tangent_velocity1 = dot_w(tangent_velocity, tangent1);
        let tangent_velocity2 = dot_w(tangent_velocity, tangent2);

        let cm_twist_impulse = mul_w(warm_start_scale, twist_impulse);
        let cm_rolling_impulse = Vec3W {
            x: blend_w(
                zero,
                mul_w(warm_start_scale, rolling_impulse.x),
                rolling_mask,
            ),
            y: blend_w(
                zero,
                mul_w(warm_start_scale, rolling_impulse.y),
                rolling_mask,
            ),
            z: blend_w(
                zero,
                mul_w(warm_start_scale, rolling_impulse.z),
                rolling_mask,
            ),
        };
        let cm_friction_impulse = Vec2W {
            x: mul_w(warm_start_scale, dot_w(friction_impulse, tangent1)),
            y: mul_w(warm_start_scale, dot_w(friction_impulse, tangent2)),
        };

        let point_count_w = ints_to_float_w(&point_counts);

        let mut center_a = VEC3W_ZERO;
        let mut center_b = VEC3W_ZERO;
        let mut total_friction_weight = zero;

        let mut points = [EMPTY_POINT; MAX_MANIFOLD_POINTS];

        for point_index in 0..point_count as usize {
            let cp = &mut points[point_index];
            let point_mask = greater_than_w(point_count_w, splat_w(point_index as f32));

            let mp: [&ManifoldPoint; SIMD_WIDTH] =
                core::array::from_fn(|l| &manifold_lanes[l].points[point_index]);

            let mut r_a = Vec3W {
                x: gather_lanes!(|l| mp[l].anchor_a.x),
                y: gather_lanes!(|l| mp[l].anchor_a.y),
                z: gather_lanes!(|l| mp[l].anchor_a.z),
            };
            let mut r_b = Vec3W {
                x: gather_lanes!(|l| mp[l].anchor_b.x),
                y: gather_lanes!(|l| mp[l].anchor_b.y),
                z: gather_lanes!(|l| mp[l].anchor_b.z),
            };
            let mut separation = gather_lanes!(|l| mp[l].separation);
            let mut normal_impulse = gather_lanes!(|l| mp[l].normal_impulse);

            r_a.x = blend_w(zero, r_a.x, point_mask);
            r_a.y = blend_w(zero, r_a.y, point_mask);
            r_a.z = blend_w(zero, r_a.z, point_mask);
            r_b.x = blend_w(zero, r_b.x, point_mask);
            r_b.y = blend_w(zero, r_b.y, point_mask);
            r_b.z = blend_w(zero, r_b.z, point_mask);
            separation = blend_w(zero, separation, point_mask);
            normal_impulse = blend_w(zero, normal_impulse, point_mask);

            // C0 friction center decay. Needed to prevent spinning top drift (GyroscopicPrecession sample).
            // See details in b3PrepareContacts_Mesh. This code should stay in sync.
            let mut weight = min_w(
                max_w(
                    sub_w(two_w, mul_w(separation, inv_tau)),
                    min_friction_weight,
                ),
                one_w,
            );
            weight = blend_w(zero, weight, point_mask);
            center_a = mul_add_svw(center_a, weight, r_a);
            center_b = mul_add_svw(center_b, weight, r_b);
            total_friction_weight = add_w(total_friction_weight, weight);

            cp.anchor_as = r_a;
            cp.anchor_bs = r_b;
            cp.base_separations = sub_w(separation, dot_w(sub_vw(r_b, r_a), normal));
            cp.normal_impulses = mul_w(warm_start_scale, normal_impulse);
            cp.total_normal_impulses = zero;
            cp.relative_velocities = zero;
            cp.restitution_impulses = zero;

            let rn_a = cross_w(r_a, normal);
            let rn_b = cross_w(r_b, normal);
            let mut k_normal = add_w(m_a, m_b);
            k_normal = add_w(k_normal, dot_w(rn_a, mul_mvw(i_a, rn_a)));
            k_normal = add_w(k_normal, dot_w(rn_b, mul_mvw(i_b, rn_b)));
            let valid = and_w(greater_than_w(k_normal, zero), point_mask);
            cp.normal_masses = blend_w(zero, div_w(one_w, k_normal), valid);
        }

        let friction_valid = greater_than_w(total_friction_weight, zero);
        let inv_weight = blend_w(zero, div_w(one_w, total_friction_weight), friction_valid);
        center_a = mul_svw(inv_weight, center_a);
        center_b = mul_svw(inv_weight, center_b);

        for point_index in 0..point_count as usize {
            let cp = &mut points[point_index];
            let point_mask = greater_than_w(point_count_w, splat_w(point_index as f32));
            let d = sub_vw(cp.anchor_as, center_a);
            cp.lever_arms = blend_w(zero, sqrt_w(dot_w(d, d)), point_mask);
        }

        let tangent_mass = {
            let rt_a1 = cross_w(center_a, tangent1);
            let rt_a2 = cross_w(center_a, tangent2);
            let rt_b1 = cross_w(center_b, tangent1);
            let rt_b2 = cross_w(center_b, tangent2);
            let i_a_rt_a1 = mul_mvw(i_a, rt_a1);
            let i_a_rt_a2 = mul_mvw(i_a, rt_a2);
            let i_b_rt_b1 = mul_mvw(i_b, rt_b1);
            let i_b_rt_b2 = mul_mvw(i_b, rt_b2);

            let kxx = add_w(
                add_w(add_w(m_a, m_b), dot_w(rt_a1, i_a_rt_a1)),
                dot_w(rt_b1, i_b_rt_b1),
            );
            let kyy = add_w(
                add_w(add_w(m_a, m_b), dot_w(rt_a2, i_a_rt_a2)),
                dot_w(rt_b2, i_b_rt_b2),
            );
            let kxy = add_w(dot_w(rt_a1, i_a_rt_a2), dot_w(rt_b1, i_b_rt_b2));

            let det = sub_w(mul_w(kxx, kyy), mul_w(kxy, kxy));
            let valid = and_w(greater_than_w(abs_w(det), min_det), friction_valid);
            let inv_det = blend_w(zero, div_w(one_w, det), valid);
            SymMatrix2W {
                cxx: mul_w(inv_det, kyy),
                cxy: neg_w(mul_w(inv_det, kxy)),
                cyy: mul_w(inv_det, kxx),
            }
        };

        let twist_mass = {
            let k_twist = dot_w(normal, mul_mvw(inv_i_ab, normal));
            blend_w(zero, div_w(one_w, k_twist), greater_than_w(k_twist, zero))
        };

        manifolds_out[manifold_index] = ContactManifoldWide {
            normal,
            tangent1,
            tangent2,
            center_a,
            center_b,
            twist_mass,
            twist_impulse: cm_twist_impulse,
            tangent_mass,
            friction_impulse: cm_friction_impulse,
            rolling_impulse: cm_rolling_impulse,
            rolling_mask,
            tangent_velocity1,
            tangent_velocity2,
            point_count,
            points,
        };
    }

    *constraint_out = ContactConstraintWide {
        index_a,
        index_b,
        contact_ids: *lane_contact_ids,
        inv_mass_a: m_a,
        inv_mass_b: m_b,
        inv_i_a: i_a,
        inv_i_b: i_b,
        rolling_mass,
        friction,
        rolling_resistance,
        restitution,
        manifold_count,
    };

    // Only sample contact point normal velocity if needed.
    let have_restitution = any_true_w(greater_than_w(restitution, zero));
    if hit_event_lanes != 0 || have_restitution {
        let b_a = gather_bodies(states, &index_a);
        let b_b = gather_bodies(states, &index_b);

        for manifold_index in 0..manifold_count as usize {
            let cm = &mut manifolds_out[manifold_index];

            for point_index in 0..cm.point_count as usize {
                let cp = &mut cm.points[point_index];

                let vr_a = add_vw(b_a.v, cross_w(b_a.w, cp.anchor_as));
                let vr_b = add_vw(b_b.v, cross_w(b_b.w, cp.anchor_bs));
                let vn = dot_w(cm.normal, sub_vw(vr_b, vr_a));
                cp.relative_velocities = vn;

                if hit_event_lanes != 0 {
                    for l in 0..SIMD_WIDTH {
                        if (hit_event_lanes & (1 << l)) == 0
                            || manifold_index as i32 >= manifold_counts[l]
                        {
                            continue;
                        }

                        // SAFETY: the function contract.
                        let manifold = &mut unsafe {
                            contacts.get_mut(lane_contact_ids[l] as usize)
                        }
                        .manifolds[manifold_index];
                        if (point_index as i32) < manifold.point_count {
                            manifold.points[point_index].normal_velocity = lane(vn, l);
                        }
                    }
                }
            }
        }
    }

    have_restitution
}

fn warm_start_scale_w(enable_warm_starting: bool) -> FloatW {
    if enable_warm_starting {
        splat_w(1.0)
    } else {
        zero_w()
    }
}

/// Size `set` for `wide_count` constraints whose manifolds start at `manifold_starts` (one per
/// constraint plus a sentinel, relative to the first). The slots are zeroed and every one is
/// overwritten by the prepare. Primordial Engine fork (D2): preallocating lets blocks of a
/// color be prepared in parallel; C writes into a preallocated stream too.
fn allocate_wide(
    set: &mut WideContactConstraints,
    wide_count: usize,
    manifold_starts: impl Iterator<Item = i32>,
) {
    set.clear();
    set.manifold_starts.extend(manifold_starts);
    debug_assert!(set.manifold_starts.len() == wide_count + 1);
    let manifold_count = *set.manifold_starts.last().unwrap() as usize;
    set.constraints.resize(wide_count, ContactConstraintWide::zeroed());
    set.manifolds.resize(manifold_count, ContactManifoldWide::zeroed());
}

/// Allocate the wide constraints for a color with `color_contact_count` convex contacts.
pub fn allocate_contacts_convex(set: &mut WideContactConstraints, color_contact_count: usize) {
    let wide_count = if color_contact_count == 0 {
        0
    } else {
        ((color_contact_count - 1) >> 2) + 1
    };
    allocate_wide(set, wide_count, 0..=wide_count as i32);
}

/// Allocate the wide constraints for a color's mesh contact groups (`manifold_starts` from
/// `solver::mesh_lanes`, one per group plus a sentinel).
pub fn allocate_contacts_mesh_wide(set: &mut WideContactConstraints, manifold_starts: &[i32]) {
    let group_count = manifold_starts.len().saturating_sub(1);
    let base = manifold_starts.first().copied().unwrap_or(0);
    if group_count == 0 {
        allocate_wide(set, 0, std::iter::once(0));
    } else {
        allocate_wide(set, group_count, manifold_starts.iter().map(|s| s - base));
    }
}

/// Prepare the wide constraints for the convex contacts of one graph color. Returns true if
/// any lane has restitution. (b3PrepareContacts_Convex, one color)
///
/// `contact_ids` is the color's convex contact array.
pub fn prepare_contacts_convex(
    set: &mut WideContactConstraints,
    contact_ids: &[i32],
    contacts: &mut [Contact],
    sims: &[BodySim],
    states: &[BodyState],
    enable_warm_starting: bool,
) -> bool {
    allocate_contacts_convex(set, contact_ids.len());
    let wide_count = set.len();
    // SAFETY: exclusive borrows of the set and the contacts.
    unsafe {
        prepare_contacts_convex_range(
            WideView::new(set),
            0..wide_count,
            contact_ids,
            SharedSlice::new(contacts),
            sims,
            states,
            enable_warm_starting,
        )
    }
}

/// [`prepare_contacts_convex`] for wide constraints `wide_range` of a color allocated with
/// [`allocate_contacts_convex`]. (D2 block)
///
/// # Safety
/// The set is allocated and live, and no other thread touches these constraints or the
/// contacts of their lanes during the call.
pub(crate) unsafe fn prepare_contacts_convex_range(
    set: WideView,
    wide_range: core::ops::Range<usize>,
    contact_ids: &[i32],
    contacts: SharedSlice<Contact>,
    sims: &[BodySim],
    states: &[BodyState],
    enable_warm_starting: bool,
) -> bool {
    let warm_start_scale = warm_start_scale_w(enable_warm_starting);
    let mut any_restitution = false;

    let color_contact_count = contact_ids.len();

    for local_wide_index in wide_range {
        let mut lane_ids = [EMPTY_LANE; SIMD_WIDTH];
        for (lane_index, lane_id) in lane_ids.iter_mut().enumerate() {
            let contact_index = SIMD_WIDTH * local_wide_index + lane_index;
            if contact_index < color_contact_count {
                let contact_id = contact_ids[contact_index];
                // SAFETY: the function contract.
                debug_assert!(unsafe { contacts.get(contact_id as usize) }.manifold_count() == 1);
                *lane_id = contact_id;
            }
        }

        // SAFETY: the function contract.
        let have_restitution = unsafe {
            prepare_constraint_w(
                set.constraint_mut(local_wide_index),
                set.manifolds_mut(local_wide_index),
                &lane_ids,
                1,
                contacts,
                sims,
                states,
                warm_start_scale,
            )
        };
        any_restitution = any_restitution || have_restitution;
    }

    any_restitution
}

/// Prepare the wide constraints for the mesh contacts of one graph color. Returns true if any
/// lane has restitution. (b3PrepareContacts_MeshWide, one color)
///
/// `specs` is the color's mesh contact array. `order` groups contacts with similar manifold
/// counts to keep the lanes full, and `manifold_starts` holds the manifold slot start of each
/// group plus a sentinel (both built by the solver, see `solver::mesh_lanes`).
#[allow(clippy::too_many_arguments)]
pub fn prepare_contacts_mesh_wide(
    set: &mut WideContactConstraints,
    specs: &[ContactSpec],
    order: &[i32],
    manifold_starts: &[i32],
    contacts: &mut [Contact],
    sims: &[BodySim],
    states: &[BodyState],
    enable_warm_starting: bool,
) -> bool {
    allocate_contacts_mesh_wide(set, manifold_starts);
    let group_count = set.len();
    // SAFETY: exclusive borrows of the set and the contacts.
    unsafe {
        prepare_contacts_mesh_wide_range(
            WideView::new(set),
            0..group_count,
            specs,
            order,
            SharedSlice::new(contacts),
            sims,
            states,
            enable_warm_starting,
        )
    }
}

/// [`prepare_contacts_mesh_wide`] for groups `group_range` of a color allocated with
/// [`allocate_contacts_mesh_wide`]. (D2 block)
///
/// # Safety
/// As for [`prepare_contacts_convex_range`].
#[allow(clippy::too_many_arguments)]
pub(crate) unsafe fn prepare_contacts_mesh_wide_range(
    set: WideView,
    group_range: core::ops::Range<usize>,
    specs: &[ContactSpec],
    order: &[i32],
    contacts: SharedSlice<Contact>,
    sims: &[BodySim],
    states: &[BodyState],
    enable_warm_starting: bool,
) -> bool {
    let warm_start_scale = warm_start_scale_w(enable_warm_starting);
    let mut any_restitution = false;

    let color_contact_count = specs.len();
    debug_assert!(order.len() == color_contact_count);

    for local_wide_index in group_range {
        let mut lane_ids = [EMPTY_LANE; SIMD_WIDTH];
        for (lane_index, lane_id) in lane_ids.iter_mut().enumerate() {
            let contact_index = SIMD_WIDTH * local_wide_index + lane_index;
            if contact_index < color_contact_count {
                let spec = &specs[order[contact_index] as usize];
                // SAFETY: the function contract.
                let contact = unsafe { contacts.get(spec.contact_id as usize) };
                debug_assert!(contact.contact_id == spec.contact_id);
                debug_assert!(contact.manifold_count() == spec.manifold_count as i32);
                *lane_id = spec.contact_id;
            }
        }

        // SAFETY: the function contract.
        let have_restitution = unsafe {
            let manifolds = set.manifolds_mut(local_wide_index);
            let manifold_count = manifolds.len() as i32;
            prepare_constraint_w(
                set.constraint_mut(local_wide_index),
                manifolds,
                &lane_ids,
                manifold_count,
                contacts,
                sims,
                states,
                warm_start_scale,
            )
        };
        any_restitution = any_restitution || have_restitution;
    }

    any_restitution
}
