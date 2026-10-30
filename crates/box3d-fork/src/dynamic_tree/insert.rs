// SAH sibling selection, rotations, leaf insert/remove, and proxy operations from
// dynamic_tree.c.
//
// SPDX-FileCopyrightText: 2025 Erin Catto
// SPDX-License-Identifier: MIT

use super::{
    make_empty_node, union_pair_v, union_v, DynamicTree, TreeNode, LEAF_NODE, MOVED_NODE, ROOT_NODE,
};
use crate::aabb::{enlarge_aabb, perimeter};
use crate::constants::huge;
use crate::core::NULL_INDEX;
use crate::math_functions::{
    aabb_center, aabb_contains, aabb_union, is_valid_aabb, length_squared, max_int, min_float, sub,
    Aabb,
};

/// (static b3GetNodeHeight)
#[inline]
pub(crate) fn get_node_height(node: &TreeNode) -> i32 {
    if node.is_leaf() {
        0
    } else {
        node.height
    }
}

/// The internal node above a children pair. (static b3MakeInternalNode)
#[inline]
pub(crate) fn make_internal_node(nodes: &[TreeNode], pair: i32) -> TreeNode {
    let c1 = &nodes[pair as usize];
    let c2 = &nodes[pair as usize + 1];

    TreeNode {
        aabb: union_pair_v(nodes, pair),
        flag_index: pair as u32 | ((c1.flag_index | c2.flag_index) & MOVED_NODE),
        height: 1 + max_int(get_node_height(c1), get_node_height(c2)),
    }
}

/// (static b3MakeLeafNode)
#[inline]
fn make_leaf_node(aabb: Aabb, proxy_id: i32, user_data: u64, moved: bool) -> TreeNode {
    TreeNode {
        aabb,
        flag_index: proxy_id as u32 | LEAF_NODE | if moved { MOVED_NODE } else { 0 },
        // shapeIndex view of the union, truncated from the proxy user data
        height: user_data as u32 as i32,
    }
}

/// The sweep refit visits indices from high to low, so it needs every child above its
/// parent. (static b3IsNodeOrdered)
#[inline]
fn is_node_ordered(nodes: &[TreeNode], node_index: i32) -> bool {
    let node = &nodes[node_index as usize];
    node.is_leaf() || node_index < node.left_child()
}

impl DynamicTree {
    /// A node landed at a new index, so tell what hangs below it.
    /// (static b3LinkChildren)
    #[inline]
    fn link_children(&mut self, node_index: i32) {
        let node = self.nodes[node_index as usize];
        if node.is_leaf() {
            self.proxies[node.proxy_id() as usize].node = node_index;
        } else {
            let pair = node.left_child();
            self.parents[pair as usize] = node_index;
            self.parents[pair as usize + 1] = node_index;
        }
    }

    // Greedy algorithm for sibling selection using the SAH
    // We have three nodes A-(B,C) and want to add a leaf D, there are three choices.
    // 1: make a new parent for A and D : E-(A-(B,C), D)
    // 2: associate D with B
    //   a: B is a leaf : A-(E-(B,D), C)
    //   b: B is an internal node: A-(B{D},C)
    // 3: associate D with C
    //   a: C is a leaf : A-(B, E-(C,D))
    //   b: C is an internal node: A-(B, C{D})
    // All of these have a clear cost except when B or C is an internal node. Hence we need
    // to be greedy.

    // The cost for cases 1, 2a, and 3a can be computed using the sibling cost formula.
    // cost of sibling H = area(union(H, D)) + increased area of ancestors

    // Suppose B (or C) is an internal node, then the lowest cost would be one of two cases:
    // case1: D becomes a sibling of B
    // case2: D becomes a descendant of B along with a new internal node of area(D).
    /// (static b3FindBestSibling)
    fn find_best_sibling(&self, box_d: Aabb) -> i32 {
        let center_d = aabb_center(box_d);
        let area_d = perimeter(box_d);

        let nodes = &self.nodes;
        let mut node_index = ROOT_NODE;

        if nodes[node_index as usize].is_leaf() {
            return node_index;
        }

        let root_box = nodes[node_index as usize].aabb;

        // Area of current node
        let mut area_base = perimeter(root_box);

        // Area of inflated node
        let mut direct_cost = perimeter(aabb_union(root_box, box_d));
        let mut inherited_cost = 0.0f32;

        let mut best_sibling = node_index;
        let mut best_cost = direct_cost;

        // Descend the tree, following a single greedy path.
        loop {
            let child1 = nodes[node_index as usize].left_child();
            let child2 = child1 + 1;

            // Cost of creating a new parent for this node and the new leaf
            let cost = direct_cost + inherited_cost;

            // Sometimes there are multiple identical costs within tolerance.
            // This breaks the ties using the centroid distance.
            if cost < best_cost {
                best_sibling = node_index;
                best_cost = cost;
            }

            // Inheritance cost seen by children
            inherited_cost += direct_cost - area_base;

            let leaf1 = nodes[child1 as usize].is_leaf();
            let leaf2 = nodes[child2 as usize].is_leaf();

            // Cost of descending into child 1
            let mut lower_cost1 = f32::MAX;
            let box1 = nodes[child1 as usize].aabb;
            let direct_cost1 = perimeter(aabb_union(box1, box_d));
            let mut area1 = 0.0f32;
            if leaf1 {
                // Child 1 is a leaf
                // Cost of creating new node and increasing area of node P
                let cost1 = direct_cost1 + inherited_cost;
                if cost1 < best_cost {
                    best_sibling = child1;
                    best_cost = cost1;
                }
            } else {
                // Child 1 is an internal node
                area1 = perimeter(box1);

                // Lower bound cost of inserting under child 1. The minimum accounts for two
                // possibilities:
                // 1. Child1 could be the sibling with cost1 = inheritedCost + directCost1
                // 2. A descendant of child1 could be the sibling with the lower bound cost of
                //       cost1 = inheritedCost + (directCost1 - area1) + areaD
                // This minimum here leads to the minimum of these two costs.
                lower_cost1 = inherited_cost + direct_cost1 + min_float(area_d - area1, 0.0);
            }

            // Cost of descending into child 2
            let mut lower_cost2 = f32::MAX;
            let box2 = nodes[child2 as usize].aabb;
            let direct_cost2 = perimeter(aabb_union(box2, box_d));
            let mut area2 = 0.0f32;
            if leaf2 {
                let cost2 = direct_cost2 + inherited_cost;
                if cost2 < best_cost {
                    best_sibling = child2;
                    best_cost = cost2;
                }
            } else {
                area2 = perimeter(box2);
                lower_cost2 = inherited_cost + direct_cost2 + min_float(area_d - area2, 0.0);
            }

            if leaf1 && leaf2 {
                break;
            }

            // Can the cost possibly be decreased?
            if best_cost <= lower_cost1 && best_cost <= lower_cost2 {
                break;
            }

            if lower_cost1 == lower_cost2 && !leaf1 {
                debug_assert!(lower_cost1 < f32::MAX);
                debug_assert!(lower_cost2 < f32::MAX);

                // No clear choice based on lower bound surface area. This can happen when both
                // children fully contain D. Fall back to node distance.
                let d1 = sub(aabb_center(box1), center_d);
                let d2 = sub(aabb_center(box2), center_d);
                lower_cost1 = length_squared(d1);
                lower_cost2 = length_squared(d2);
            }

            // Descend
            if lower_cost1 < lower_cost2 && !leaf1 {
                node_index = child1;
                area_base = area1;
                direct_cost = direct_cost1;
            } else {
                node_index = child2;
                area_base = area2;
                direct_cost = direct_cost2;
            }
        }

        best_sibling
    }

    /// Swap a child of A with a grandchild under the other child.
    /// Example swapping B with G:
    /// (A (B (D E) C (F G)) -> (A (G) C (F B (D E))
    /// (static b3SwapNodes)
    fn swap_nodes(&mut self, i_down: i32, i_up: i32) {
        self.nodes.swap(i_down as usize, i_up as usize);
        self.link_children(i_down);
        self.link_children(i_up);

        if !is_node_ordered(&self.nodes, i_down) || !is_node_ordered(&self.nodes, i_up) {
            self.dfs_ordered = false;
        }

        // The sibling of the node that went down holds a different subtree now
        let i_c = i_down ^ 1;
        let pair = self.nodes[i_c as usize].left_child();
        self.nodes[i_c as usize] = make_internal_node(&self.nodes, pair);
    }

    /// Perform a left or right rotation if node A is imbalanced.
    /// Tree: (A (B (D E) C (F G))
    /// (static b3RotateNodes)
    fn rotate_nodes(&mut self, i_a: i32) {
        let nodes = &self.nodes;
        let a = &nodes[i_a as usize];
        debug_assert!(!a.is_leaf());

        let i_b = a.left_child();
        let i_c = i_b + 1;
        let b = &nodes[i_b as usize];
        let c = &nodes[i_c as usize];

        let leaf_b = b.is_leaf();
        let leaf_c = c.is_leaf();
        if leaf_b && leaf_c {
            return;
        }

        let mut best_down = NULL_INDEX;
        let mut best_up = NULL_INDEX;
        let mut best_delta = 0.0f32;

        if !leaf_c {
            // Swap B with F or G
            let i_f = c.left_child();
            let i_g = i_f + 1;
            let area_c = perimeter(c.aabb);

            // B <-> F then C (B G)
            let delta_bf = perimeter(union_v(b.aabb, nodes[i_g as usize].aabb)) - area_c;
            if delta_bf < best_delta {
                best_down = i_b;
                best_up = i_f;
                best_delta = delta_bf;
            }

            // B <-> G then C (F B)
            let delta_bg = perimeter(union_v(b.aabb, nodes[i_f as usize].aabb)) - area_c;
            if delta_bg < best_delta {
                best_down = i_b;
                best_up = i_g;
                best_delta = delta_bg;
            }
        }

        if !leaf_b {
            // Swap C with D or E
            let i_d = b.left_child();
            let i_e = i_d + 1;
            let area_b = perimeter(b.aabb);

            // C <-> D then B (C E)
            let delta_cd = perimeter(union_v(c.aabb, nodes[i_e as usize].aabb)) - area_b;
            if delta_cd < best_delta {
                best_down = i_c;
                best_up = i_d;
                best_delta = delta_cd;
            }

            // C <-> E then B (D C)
            let delta_ce = perimeter(union_v(c.aabb, nodes[i_d as usize].aabb)) - area_b;
            if delta_ce < best_delta {
                best_down = i_c;
                best_up = i_e;
                best_delta = delta_ce;
            }
        }

        let _ = best_delta;

        if best_down != NULL_INDEX {
            self.swap_nodes(best_down, best_up);
        }
    }

    /// (static b3InsertLeaf)
    fn insert_leaf(&mut self, aabb: Aabb, proxy_id: i32, moved: bool, should_rotate: bool) {
        let leaf = make_leaf_node(
            aabb,
            proxy_id,
            self.proxies[proxy_id as usize].user_data,
            moved,
        );

        if self.nodes[ROOT_NODE as usize].is_empty() {
            self.nodes[ROOT_NODE as usize] = leaf;
            self.proxies[proxy_id as usize].node = ROOT_NODE;
            return;
        }

        let sibling = self.find_best_sibling(aabb);

        // The sibling's position becomes the new parent. The sibling moves down into a new
        // pair beside the leaf, which puts it below its own children when it is internal.
        let pair = self.allocate_sibling_pair();

        self.nodes[pair as usize] = self.nodes[sibling as usize];
        self.nodes[pair as usize + 1] = leaf;
        self.parents[pair as usize] = sibling;
        self.parents[pair as usize + 1] = sibling;
        self.link_children(pair);
        self.proxies[proxy_id as usize].node = pair + 1;

        self.nodes[sibling as usize] = make_internal_node(&self.nodes, pair);

        if !is_node_ordered(&self.nodes, sibling) || !is_node_ordered(&self.nodes, pair) {
            self.dfs_ordered = false;
        }

        // Walk back up the tree refitting ancestors, the root included.
        let mut index = sibling;
        while index != NULL_INDEX {
            if should_rotate {
                self.rotate_nodes(index);
            }

            let left = self.nodes[index as usize].left_child();
            self.nodes[index as usize] = make_internal_node(&self.nodes, left);
            index = self.parents[index as usize];
        }
    }

    /// (static b3RemoveLeaf)
    fn remove_leaf(&mut self, proxy_id: i32) {
        let leaf = self.proxies[proxy_id as usize].node;
        debug_assert!(0 <= leaf && leaf < self.node_end);
        debug_assert!(
            self.nodes[leaf as usize].is_leaf() && self.nodes[leaf as usize].proxy_id() == proxy_id
        );

        if leaf == ROOT_NODE {
            self.nodes[ROOT_NODE as usize] = make_empty_node();
            return;
        }

        // The sibling takes the parent's position and the pair is freed. This keeps the
        // order, the sibling only moves down.
        let parent = self.parents[leaf as usize];
        self.nodes[parent as usize] = self.nodes[(leaf ^ 1) as usize];
        self.link_children(parent);
        self.free_pair(leaf & !1);

        // Update ancestors.
        let mut index = self.parents[parent as usize];
        while index != NULL_INDEX {
            let left = self.nodes[index as usize].left_child();
            self.nodes[index as usize] = make_internal_node(&self.nodes, left);
            index = self.parents[index as usize];
        }
    }

    /// Create a proxy in the tree as a leaf node. We return the index of the proxy instead
    /// of a pointer so that we can grow the node pool. (b3CreateTreeProxyInternal)
    pub fn create_proxy_internal(
        &mut self,
        aabb: Aabb,
        category_bits: u64,
        user_data: u64,
        mark_moved: bool,
    ) -> i32 {
        debug_assert!(is_valid_aabb(aabb));

        let proxy_id = self.allocate_proxy();

        let proxy = &mut self.proxies[proxy_id as usize];
        proxy.category_bits = category_bits;
        proxy.user_data = user_data;

        let should_rotate = true;
        self.insert_leaf(aabb, proxy_id, mark_moved, should_rotate);

        proxy_id
    }

    /// Create a proxy. Provide an AABB and a user data value. (b3DynamicTree_CreateProxy)
    pub fn create_proxy(&mut self, aabb: Aabb, category_bits: u64, user_data: u64) -> i32 {
        self.create_proxy_internal(aabb, category_bits, user_data, false)
    }

    /// Destroy a proxy. This asserts if the id is invalid.
    /// (b3DynamicTree_DestroyProxy)
    pub fn destroy_proxy(&mut self, proxy_id: i32) {
        debug_assert!(0 <= proxy_id && proxy_id < self.proxy_capacity());

        self.remove_leaf(proxy_id);
        self.free_proxy(proxy_id);
    }

    /// (b3DynamicTree_MoveProxyInternal)
    pub fn move_proxy_internal(&mut self, proxy_id: i32, aabb: Aabb, mark_moved: bool) {
        debug_assert!(is_valid_aabb(aabb));
        debug_assert!(aabb.upper_bound.x - aabb.lower_bound.x < huge());
        debug_assert!(aabb.upper_bound.y - aabb.lower_bound.y < huge());
        debug_assert!(aabb.upper_bound.z - aabb.lower_bound.z < huge());
        debug_assert!(0 <= proxy_id && proxy_id < self.proxy_capacity());

        self.remove_leaf(proxy_id);

        let should_rotate = false;
        self.insert_leaf(aabb, proxy_id, mark_moved, should_rotate);
    }

    /// Move a proxy to a new AABB by removing and reinserting into the tree.
    /// (b3DynamicTree_MoveProxy)
    pub fn move_proxy(&mut self, proxy_id: i32, aabb: Aabb) {
        self.move_proxy_internal(proxy_id, aabb, false);
    }

    /// Enlarge a proxy and enlarge ancestors as necessary. The ancestors are marked as
    /// moved all the way to the root. (b3DynamicTree_EnlargeProxy)
    pub fn enlarge_proxy(&mut self, proxy_id: i32, aabb: Aabb) {
        debug_assert!(is_valid_aabb(aabb));
        debug_assert!(aabb.upper_bound.x - aabb.lower_bound.x < huge());
        debug_assert!(aabb.upper_bound.y - aabb.lower_bound.y < huge());
        debug_assert!(aabb.upper_bound.z - aabb.lower_bound.z < huge());
        debug_assert!(0 <= proxy_id && proxy_id < self.proxy_capacity());

        let mut index = self.proxies[proxy_id as usize].node;
        debug_assert!(self.nodes[index as usize].is_leaf());

        // Caller must ensure this
        debug_assert!(!aabb_contains(self.nodes[index as usize].aabb, aabb));

        {
            let node = &mut self.nodes[index as usize];
            node.aabb = aabb;
            node.flag_index |= MOVED_NODE;
        }

        index = self.parents[index as usize];
        while index != NULL_INDEX {
            let node = &mut self.nodes[index as usize];
            let changed = enlarge_aabb(&mut node.aabb, aabb);

            // This is marked to ensure the root is marked in this loop or the one below.
            node.flag_index |= MOVED_NODE;

            index = self.parents[index as usize];

            if !changed {
                break;
            }
        }

        // Mark all the way up to the root.
        while index != NULL_INDEX {
            let node = &mut self.nodes[index as usize];
            if node.flag_index & MOVED_NODE != 0 {
                // Early out because this ancestor was previously ascended and marked as moved.
                break;
            }

            node.flag_index |= MOVED_NODE;
            index = self.parents[index as usize];
        }
    }

    /// Modify the category bits on a proxy. This is an expensive operation.
    /// (b3DynamicTree_SetCategoryBits)
    pub fn set_category_bits(&mut self, proxy_id: i32, category_bits: u64) {
        debug_assert!(0 <= proxy_id && proxy_id < self.proxy_capacity());
        self.proxies[proxy_id as usize].category_bits = category_bits;
    }

    /// Get the category bits on a proxy. (b3DynamicTree_GetCategoryBits)
    pub fn category_bits(&self, proxy_id: i32) -> u64 {
        debug_assert!(0 <= proxy_id && proxy_id < self.proxy_capacity());
        self.proxies[proxy_id as usize].category_bits
    }

    /// Get the height of the binary tree. (b3DynamicTree_GetHeight)
    pub fn height(&self) -> i32 {
        get_node_height(&self.nodes[ROOT_NODE as usize])
    }
}
