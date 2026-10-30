// The b3AABBV helpers from simd.h. The port implements the scalar (B3_SIMD_NONE) path,
// where `b3AABBV` is a plain `b3AABB`, `b3LoadAABBV` is the identity, and
// `b3StoreAABBV( aabb, value, condition )` is `if ( condition ) *aabb = value;`, so those
// two helpers have no Rust counterpart; call sites load/store the AABB directly.
//
// SPDX-FileCopyrightText: 2025 Erin Catto
// SPDX-License-Identifier: MIT

use super::TreeNode;
use crate::math_functions::{aabb_union, Aabb};

/// (b3OverlapAABBV, scalar path)
#[inline]
pub(crate) fn overlap_aabb_v(a: &Aabb, b: &Aabb) -> bool {
    a.lower_bound.x <= b.upper_bound.x
        && a.lower_bound.y <= b.upper_bound.y
        && a.lower_bound.z <= b.upper_bound.z
        && b.lower_bound.x <= a.upper_bound.x
        && b.lower_bound.y <= a.upper_bound.y
        && b.lower_bound.z <= a.upper_bound.z
}

/// (b3OverlapNode, scalar path)
#[inline]
pub(crate) fn overlap_node(av: &Aabb, node: &TreeNode) -> bool {
    overlap_aabb_v(av, &node.aabb)
}

/// (b3OverlapV, scalar path)
#[inline]
pub(crate) fn overlap_v(a: &Aabb, b: &Aabb) -> bool {
    overlap_aabb_v(a, b)
}

/// (b3UnionPairV, scalar path) — the union of the sibling pair starting at `pair`.
#[inline]
pub(crate) fn union_pair_v(nodes: &[TreeNode], pair: i32) -> Aabb {
    aabb_union(nodes[pair as usize].aabb, nodes[pair as usize + 1].aabb)
}

/// (b3UnionV, scalar path: b3UnionAABBV == b3AABB_Union)
#[inline]
pub(crate) fn union_v(a: Aabb, b: Aabb) -> Aabb {
    aabb_union(a, b)
}
