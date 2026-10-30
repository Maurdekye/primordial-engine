//! 2D convex hull and hull simplification used for contact manifold reduction.
//!
//! Port of `b3Hull2D`, `b3SimplifyHull2D`, and their helpers (`b3Cross2D`,
//! `b3Sort2D`, `b3Weld2D`) from `box3d-cpp-reference/src/hull.c`, plus the
//! public `b3Point2D` type from `include/box3d/collision.h`.
//!
//! SPDX-FileCopyrightText: 2025 Erin Catto
//! SPDX-License-Identifier: MIT

use crate::constants::linear_slop;
use crate::math_functions::{distance_squared2, Vec2};

/// This is used for computing 2D hulls in service of contact manifold simplification.
/// Placed here for testing. Internal. (b3Point2D)
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Point2D {
    /// 2D point
    pub p: Vec2,
    /// separation of the associated contact point
    pub separation: f32,
    /// original index of the associated contact point
    pub original_index: i32,
}

// 2D hull (Andrew's monotone chain)

/// cross(b - a, c - a) (b3Cross2D)
#[inline]
fn cross_2d(a: Vec2, b: Vec2, c: Vec2) -> f32 {
    (b.x - a.x) * (c.y - a.y) - (b.y - a.y) * (c.x - a.x)
}

/// Simplify a 2D hull. Used internally for contact manifold simplification. Here for testing.
/// Iteratively remove the vertices contributing the least area. (b3SimplifyHull2D)
pub fn simplify_hull_2d(hull: &mut [Point2D], count1: i32, target: i32) -> i32 {
    debug_assert!(target >= 3);

    if count1 <= 3 {
        return count1;
    }

    let linear_slop = linear_slop();
    let area_tol = 0.25 * linear_slop * linear_slop;
    let mut count2 = count1;

    loop {
        let mut min_area = f32::MAX;
        let mut min_index = 0;

        for i in 0..count2 {
            let prev = (i + count2 - 1) % count2;
            let next = (i + 1) % count2;
            let area = cross_2d(
                hull[prev as usize].p,
                hull[i as usize].p,
                hull[next as usize].p,
            );
            debug_assert!(area >= -f32::EPSILON);

            if area + area_tol < min_area {
                min_area = area;
                min_index = i;
            }
        }

        if count2 <= 3 || (min_area > area_tol && count2 <= target) {
            break;
        }

        count2 -= 1;
        if min_index < count2 {
            // memmove( hull + minIndex, hull + ( minIndex + 1 ), ( count2 - minIndex ) * sizeof( b3Point2D ) )
            hull.copy_within(
                (min_index + 1) as usize..(count2 + 1) as usize,
                min_index as usize,
            );
        }
    }

    count2
}

/// Sort comparator: by x, then by y. Insertion since n is typically 20 or less. (b3Sort2D)
fn sort_2d(pts: &mut [Point2D], count: i32) {
    for i in 1..count {
        let base = pts[i as usize];
        let mut j = i - 1;
        while j >= 0
            && (pts[j as usize].p.x > base.p.x
                || (pts[j as usize].p.x == base.p.x && pts[j as usize].p.y > base.p.y))
        {
            pts[(j + 1) as usize] = pts[j as usize];
            j -= 1;
        }
        pts[(j + 1) as usize] = base;
    }
}

/// Welds points that have been sorted O(n) time (b3Weld2D)
fn weld_2d(pts: &mut [Point2D], count1: i32) -> i32 {
    if count1 <= 1 {
        return count1;
    }

    let linear_slop = 0.25 * linear_slop();
    let tol_sqr = linear_slop * linear_slop;

    let mut count2 = 1;
    let mut base_index = 0;
    for i in 1..count1 {
        let distance_sqr = distance_squared2(pts[i as usize].p, pts[base_index as usize].p);
        if distance_sqr < tol_sqr {
            if pts[i as usize].original_index < pts[base_index as usize].original_index {
                pts[base_index as usize] = pts[i as usize];
            }
        } else {
            pts[count2 as usize] = pts[i as usize];
            base_index = count2;
            count2 += 1;
        }
    }

    count2
}

/// Compute a 2D hull. Used internally for contact manifold simplification. Here for testing.
/// Andrew's monotone chain convex hull algorithm.
/// `hull` must have space for 2 * `count` points. `pts` is sorted and welded in place.
/// (b3Hull2D)
pub fn hull_2d(pts: &mut [Point2D], count: i32, hull: &mut [Point2D]) -> i32 {
    let mut count1 = count;

    if count1 <= 0 {
        return 0;
    }

    if count1 == 1 {
        hull[0] = pts[0];
        return 1;
    }

    sort_2d(pts, count1);
    count1 = weld_2d(pts, count1);

    if count1 == 1 {
        hull[0] = pts[0];
        return 1;
    }

    if count1 == 2 {
        hull[0] = pts[0];
        hull[1] = pts[1];
        return 2;
    }

    let mut count2: i32 = 0;

    for i in 0..count1 {
        while count2 >= 2 {
            let area = cross_2d(
                hull[(count2 - 2) as usize].p,
                hull[(count2 - 1) as usize].p,
                pts[i as usize].p,
            );
            if area > 0.0 {
                break;
            }

            count2 -= 1;
        }

        debug_assert!(count2 < 2 * count1);
        hull[count2 as usize] = pts[i as usize];
        count2 += 1;
    }

    let lower_count = count2 + 1;

    let mut i = count1 - 2;
    while i >= 0 {
        while count2 >= lower_count {
            let area = cross_2d(
                hull[(count2 - 2) as usize].p,
                hull[(count2 - 1) as usize].p,
                pts[i as usize].p,
            );
            if area > 0.0 {
                break;
            }

            count2 -= 1;
        }

        debug_assert!(count2 < 2 * count1);
        hull[count2 as usize] = pts[i as usize];
        count2 += 1;
        i -= 1;
    }

    debug_assert!(hull[0].original_index == hull[(count2 - 1) as usize].original_index);
    count2 - 1
}
