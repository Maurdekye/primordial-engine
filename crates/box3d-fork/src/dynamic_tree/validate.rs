// Tree validation and metrics from dynamic_tree.c. The C validation bodies are compiled in
// only with B3_ENABLE_VALIDATION; here they run in debug builds and are no-ops otherwise.
//
// SPDX-FileCopyrightText: 2025 Erin Catto
// SPDX-License-Identifier: MIT

use super::{DynamicTree, TreeNode, TreeProxy, ROOT_NODE};
use crate::aabb::perimeter;
use crate::core::NULL_INDEX;
use crate::math_functions::{aabb_contains, is_valid_aabb, max_int, Aabb, Vec3, VEC3_ZERO};

impl DynamicTree {
    /// (static b3ValidateSubtree)
    fn validate_subtree(&self, node_index: i32, leaf_count: &mut i32) -> i32 {
        debug_assert!(0 <= node_index && node_index < self.node_end);
        let node = &self.nodes[node_index as usize];
        debug_assert!(!node.is_empty());
        debug_assert!(is_valid_aabb(node.aabb));

        if node.is_leaf() {
            let proxy_id = node.proxy_id();
            debug_assert!(0 <= proxy_id && proxy_id < self.proxy_capacity());
            debug_assert!(self.proxies[proxy_id as usize].node == node_index);
            debug_assert!(
                self.proxies[proxy_id as usize].user_data as u32 as i32 == node.shape_index()
            );
            *leaf_count += 1;
            return 0;
        }

        let pair = node.left_child();
        debug_assert!((pair & 1) == 0 && 2 <= pair && pair < self.node_end);
        debug_assert!(self.parents[pair as usize] == node_index);
        debug_assert!(self.parents[pair as usize + 1] == node_index);
        debug_assert!(!self.dfs_ordered || node_index < pair);

        let c1 = &self.nodes[pair as usize];
        let c2 = &self.nodes[pair as usize + 1];
        debug_assert!(aabb_contains(node.aabb, c1.aabb));
        debug_assert!(aabb_contains(node.aabb, c2.aabb));
        debug_assert!(node.is_moved() == (c1.is_moved() || c2.is_moved()));

        // A bad tree can stack overflow, but that is validation on its own.
        let height1 = self.validate_subtree(pair, leaf_count);
        let height2 = self.validate_subtree(pair + 1, leaf_count);
        let height = 1 + max_int(height1, height2);
        debug_assert!(node.height == height);
        height
    }

    /// Validate this tree. For testing. (b3DynamicTree_Validate)
    pub fn validate(&self) {
        if !cfg!(debug_assertions) {
            return;
        }

        debug_assert!(2 <= self.node_end && self.node_end <= self.node_capacity());
        debug_assert!((self.node_end & 1) == 0);
        debug_assert!(self.parents[ROOT_NODE as usize] == NULL_INDEX);
        debug_assert!(self.nodes[ROOT_NODE as usize + 1].is_empty());

        let mut free_pair_count = 0;
        let mut pair = self.pair_free_list;
        while pair != NULL_INDEX {
            debug_assert!((pair & 1) == 0 && 2 <= pair && pair < self.node_end);
            debug_assert!(self.nodes[pair as usize].is_empty());
            debug_assert!(self.nodes[pair as usize + 1].is_empty());
            pair = self.parents[pair as usize];
            free_pair_count += 1;
            debug_assert!(2 * free_pair_count < self.node_end);
        }

        // Validate proxy free list.
        let mut free_proxy_count = 0;
        let mut free_index = self.proxy_free_list;
        while free_index != NULL_INDEX {
            debug_assert!(0 <= free_index && free_index < self.proxy_capacity());
            debug_assert!(self.proxies[free_index as usize].node == NULL_INDEX);
            free_index = self.proxies[free_index as usize].next;
            free_proxy_count += 1;
        }
        debug_assert!(self.proxy_count + free_proxy_count == self.proxy_capacity());

        debug_assert!(self.node_end == 2 * max_int(self.proxy_count, 1) + 2 * free_pair_count);

        if self.proxy_count == 0 {
            debug_assert!(self.nodes[ROOT_NODE as usize].is_empty());
            return;
        }

        let mut leaf_count = 0;
        self.validate_subtree(ROOT_NODE, &mut leaf_count);
        debug_assert!(leaf_count == self.proxy_count);
    }

    /// Validate this tree has no moved nodes. For testing.
    /// (b3DynamicTree_ValidateNoMoved)
    pub fn validate_no_moved(&self) {
        if !cfg!(debug_assertions) {
            return;
        }

        for node in &self.nodes[..self.node_end.max(0) as usize] {
            debug_assert!(!node.is_moved());
        }
    }

    /// Get the ratio of the sum of the node areas to the root area.
    /// (b3DynamicTree_GetAreaRatio)
    ///
    /// The area ratio is the thing that SAH seeks to minimize. SAH cannot do anything about
    /// leaf boxes or the root box. It seeks to minimize the area of all non-root internal
    /// nodes. Divide this by the root area to make the metric non-dimensional. So this
    /// becomes a meaningful measure of tree quality.
    pub fn area_ratio(&self) -> f32 {
        if self.proxy_count == 0 {
            return 0.0;
        }

        let nodes = &self.nodes;
        let root_area = perimeter(nodes[ROOT_NODE as usize].aabb);
        if root_area <= 0.0 {
            return 0.0;
        }

        // Free nodes and the empty node are leaf tagged.
        let mut internal_area = 0.0f32;
        for node in &nodes[2..self.node_end as usize] {
            if !node.is_leaf() {
                internal_area += perimeter(node.aabb);
            }
        }

        internal_area / root_area
    }

    /// Get the bounding box that contains the entire tree.
    /// (b3DynamicTree_GetRootBounds)
    pub fn root_bounds(&self) -> Aabb {
        if self.proxy_count == 0 {
            return Aabb {
                lower_bound: VEC3_ZERO,
                upper_bound: VEC3_ZERO,
            };
        }

        self.nodes[ROOT_NODE as usize].aabb
    }

    /// Get the number of bytes used by this tree. (b3DynamicTree_GetByteCount)
    ///
    /// Matches the C formula, which always counts the leafBoxes/binIndices slots even when
    /// the median-split heuristic leaves those pointers null. The struct size is the Rust
    /// one, like the other ported byte counts.
    pub fn byte_count(&self) -> i32 {
        let node_capacity = self.nodes.len();
        let mut size = core::mem::size_of::<DynamicTree>();
        size += node_capacity * core::mem::size_of::<TreeNode>();
        size += node_capacity * core::mem::size_of::<i32>();
        size += self.proxies.len() * core::mem::size_of::<TreeProxy>();
        size += if self.swap_nodes.is_empty() {
            0
        } else {
            node_capacity * core::mem::size_of::<TreeNode>()
        };

        let rebuild_capacity = self.rebuild_capacity as usize;
        // leafIndices
        size += rebuild_capacity * core::mem::size_of::<i32>();
        // leafNodes
        size += rebuild_capacity * core::mem::size_of::<TreeNode>();
        // leafBoxes
        size += rebuild_capacity * core::mem::size_of::<Aabb>();
        // leafCenters
        size += rebuild_capacity * core::mem::size_of::<Vec3>();
        // binIndices
        size += rebuild_capacity * core::mem::size_of::<i32>();

        size as i32
    }
}
