// Tree rebuild with the median-split heuristic from dynamic_tree.c. The rebuild puts the
// whole tree in DFS order and retains unmoved subtrees. The SAH heuristic
// (B3_TREE_HEURISTIC == 1) is compiled out upstream and is not ported.
//
// SPDX-FileCopyrightText: 2025 Erin Catto
// SPDX-License-Identifier: MIT

use super::insert::make_internal_node;
use super::{
    make_empty_node, DynamicTree, TreeNode, MOVED_NODE, NODE_INDEX_MASK, ROOT_NODE, TREE_STACK_SIZE,
};
use crate::core::NULL_INDEX;
use crate::math_functions::{aabb_center, add, max, min, mul_sv, sub, Vec3};

/// Median split heuristic. (static b3PartitionMid)
fn partition_mid(indices: &mut [i32], centers: &mut [Vec3]) -> i32 {
    let count = indices.len();

    // Handle trivial case
    if count <= 2 {
        return (count / 2) as i32;
    }

    let mut lower_bound = centers[0];
    let mut upper_bound = centers[0];

    for center in centers.iter().skip(1) {
        lower_bound = min(lower_bound, *center);
        upper_bound = max(upper_bound, *center);
    }

    let d = sub(upper_bound, lower_bound);
    let c = mul_sv(0.5, add(lower_bound, upper_bound));

    // Partition longest axis using the Hoare partition scheme
    // https://en.wikipedia.org/wiki/Quicksort
    // https://nicholasvadivelu.com/2021/01/11/array-partition/
    let (mut i1, mut i2) = (0usize, count);
    if d.x >= d.y && d.x >= d.z {
        let pivot = c.x;

        while i1 < i2 {
            while i1 < i2 && centers[i1].x < pivot {
                i1 += 1;
            }

            while i1 < i2 && centers[i2 - 1].x >= pivot {
                i2 -= 1;
            }

            if i1 < i2 {
                // Swap indices and centers
                indices.swap(i1, i2 - 1);
                centers.swap(i1, i2 - 1);

                i1 += 1;
                i2 -= 1;
            }
        }
    } else if d.y >= d.z {
        let pivot = c.y;

        while i1 < i2 {
            while i1 < i2 && centers[i1].y < pivot {
                i1 += 1;
            }

            while i1 < i2 && centers[i2 - 1].y >= pivot {
                i2 -= 1;
            }

            if i1 < i2 {
                indices.swap(i1, i2 - 1);
                centers.swap(i1, i2 - 1);

                i1 += 1;
                i2 -= 1;
            }
        }
    } else {
        let pivot = c.z;

        while i1 < i2 {
            while i1 < i2 && centers[i1].z < pivot {
                i1 += 1;
            }

            while i1 < i2 && centers[i2 - 1].z >= pivot {
                i2 -= 1;
            }

            if i1 < i2 {
                indices.swap(i1, i2 - 1);
                centers.swap(i1, i2 - 1);

                i1 += 1;
                i2 -= 1;
            }
        }
    }
    debug_assert!(i1 == i2);

    if i1 > 0 && i1 < count {
        i1 as i32
    } else {
        (count / 2) as i32
    }
}

/// Temporary data used to track the rebuild of a tree node. (struct b3RebuildItem)
#[derive(Clone, Copy, Default)]
struct RebuildItem {
    // Where this node is written and the pair its children go in.
    node_index: i32,
    pair: i32,
    child_count: i32,

    // Leaf indices
    start_index: i32,
    split_index: i32,
    end_index: i32,
}

/// (struct b3CopyItem)
#[derive(Clone, Copy, Default)]
struct CopyItem {
    old_pair: i32,
    new_index: i32,
}

/// (static b3SetLeftChild)
#[inline]
fn set_left_child(node: &mut TreeNode, pair: i32) {
    node.flag_index = (node.flag_index & !NODE_INDEX_MASK) | pair as u32;
}

impl DynamicTree {
    /// Bump allocate a pair of sibling nodes. Returns index to the first one.
    /// (static b3BumpPair)
    #[inline]
    fn bump_pair(&mut self, parent: i32) -> i32 {
        let pair = self.node_end;
        debug_assert!(pair + 2 <= self.node_capacity());
        self.node_end += 2;
        self.parents[pair as usize] = parent;
        self.parents[pair as usize + 1] = parent;
        pair
    }

    /// Copy a retained subtree from the old array into the rebuilt DFS array using a bump
    /// allocator. (static b3CopySubtree)
    fn copy_subtree(&mut self, mut node: TreeNode, new_index: i32) {
        if node.is_leaf() {
            self.swap_nodes[new_index as usize] = node;
            self.proxies[node.proxy_id() as usize].node = new_index;
            return;
        }

        let mut stack = [CopyItem::default(); TREE_STACK_SIZE];
        let mut stack_count = 0usize;

        let mut old_pair = node.left_child();
        let mut new_pair = self.bump_pair(new_index);
        set_left_child(&mut node, new_pair);

        // Copy the subtree root. This can be a left or right sibling (even or odd).
        self.swap_nodes[new_index as usize] = node;

        loop {
            let np = new_pair as usize;

            // Copy the siblings.
            self.swap_nodes[np] = self.nodes[old_pair as usize];
            self.swap_nodes[np + 1] = self.nodes[old_pair as usize + 1];

            let right = self.swap_nodes[np + 1];
            if right.is_leaf() {
                // Hook up proxy on right sibling.
                let proxy_id = right.proxy_id();
                self.proxies[proxy_id as usize].node = new_pair + 1;
            } else if stack_count < TREE_STACK_SIZE {
                // Push the left child of the right sibling.
                let left_child = right.left_child();
                stack[stack_count] = CopyItem {
                    old_pair: left_child,
                    new_index: new_pair + 1,
                };
                stack_count += 1;
            } else {
                // todo fail gracefully if the stack runs out in release
                debug_assert!(stack_count < TREE_STACK_SIZE);
            }

            let left = self.swap_nodes[np];
            if !left.is_leaf() {
                // Descend into left subtree.
                let left_index = new_pair;
                old_pair = left.left_child();
                new_pair = self.bump_pair(left_index);
                set_left_child(&mut self.swap_nodes[left_index as usize], new_pair);
                continue;
            }

            // Hook up proxy on left sibling.
            self.proxies[left.proxy_id() as usize].node = new_pair;

            if stack_count == 0 {
                break;
            }

            // Descend a right subtree that was previously pushed.
            stack_count -= 1;
            let item = stack[stack_count];
            old_pair = item.old_pair;
            new_pair = self.bump_pair(item.new_index);
            set_left_child(&mut self.swap_nodes[item.new_index as usize], new_pair);
        }
    }

    /// (static b3PlaceLeaf)
    fn place_leaf(&mut self, leaf: TreeNode, new_index: i32) {
        if leaf.is_leaf() {
            self.swap_nodes[new_index as usize] = leaf;
            self.proxies[leaf.proxy_id() as usize].node = new_index;
        } else {
            self.copy_subtree(leaf, new_index);
        }
    }

    /// (static b3BuildTree)
    fn build_tree(&mut self, leaf_count: i32) {
        // Bump allocation into the swap nodes.
        self.node_end = 2;
        self.swap_nodes[ROOT_NODE as usize + 1] = make_empty_node();
        self.parents[ROOT_NODE as usize] = NULL_INDEX;
        self.parents[ROOT_NODE as usize + 1] = NULL_INDEX;

        if leaf_count == 1 {
            let leaf = self.leaf_nodes[self.leaf_indices[0] as usize];
            self.place_leaf(leaf, ROOT_NODE);
            return;
        }

        let mut stack = [RebuildItem::default(); TREE_STACK_SIZE];
        let mut top = 0usize;

        stack[0].node_index = ROOT_NODE;
        stack[0].pair = self.bump_pair(ROOT_NODE);
        stack[0].child_count = -1;
        stack[0].start_index = 0;
        stack[0].end_index = leaf_count;
        stack[0].split_index = partition_mid(
            &mut self.leaf_indices[..leaf_count as usize],
            &mut self.leaf_centers[..leaf_count as usize],
        );

        loop {
            let item = &mut stack[top];
            item.child_count += 1;

            if item.child_count == 2 {
                // Both children written, so the parent node can be finalized.
                let item = *item;
                self.swap_nodes[item.node_index as usize] =
                    make_internal_node(&self.swap_nodes, item.pair);
                if top == 0 {
                    break;
                }

                top -= 1;
                continue;
            }

            let item = *item;
            let slot = item.child_count;
            let start_index = if slot == 0 {
                item.start_index
            } else {
                item.split_index
            };
            let end_index = if slot == 0 {
                item.split_index
            } else {
                item.end_index
            };
            let count = end_index - start_index;
            debug_assert!(count > 0);

            let node_index = item.pair + slot;
            if count == 1 {
                let leaf = self.leaf_nodes[self.leaf_indices[start_index as usize] as usize];
                self.place_leaf(leaf, node_index);
                continue;
            }

            // todo fail gracefully if the stack runs out in release
            debug_assert!(top < TREE_STACK_SIZE - 1);

            top += 1;
            let pair = self.bump_pair(node_index);
            let range = start_index as usize..end_index as usize;
            let split = partition_mid(
                &mut self.leaf_indices[range.clone()],
                &mut self.leaf_centers[range],
            );
            stack[top] = RebuildItem {
                node_index,
                pair,
                child_count: -1,
                start_index,
                split_index: start_index + split,
                end_index,
            };
        }
    }

    /// Rebuild the stale parts of the tree. The entire tree is put into DFS order. This
    /// makes refitting much faster. This is done async with threading, so the cost is
    /// hidden. Not safe to access tree during this operation. Returns the number of build
    /// leaves. (b3DynamicTree_Rebuild)
    pub fn rebuild(&mut self, full_build: bool) -> i32 {
        let proxy_count = self.proxy_count;
        if proxy_count == 0 {
            return 0;
        }

        // An unordered tree is rebuilt even when nothing moved, the sweep refit needs the
        // order
        let root = self.nodes[ROOT_NODE as usize];
        if !full_build && !root.is_moved() && self.dfs_ordered {
            return 0;
        }

        if self.swap_nodes.is_empty() {
            self.swap_nodes = vec![TreeNode::zeroed(); self.nodes.len()];
        }

        if proxy_count > self.rebuild_capacity {
            let new_capacity = proxy_count + proxy_count / 2;

            // b3Alloc leaves the storage uninitialized; every slot is written before it
            // is read.
            self.leaf_indices = vec![0; new_capacity as usize];
            self.leaf_nodes = vec![TreeNode::zeroed(); new_capacity as usize];
            self.leaf_centers = vec![Vec3::default(); new_capacity as usize];
            self.rebuild_capacity = new_capacity;
        }

        // Gather build leaves. A marked internal node is descended and abandoned in the old
        // array. Everything else is a build leaf, kept subtrees included. A leaf root, or an
        // unmarked root on an unordered array, is the one build leaf.
        let mut leaf_count = 0usize;
        let mut stack = [0i32; TREE_STACK_SIZE];
        let mut stack_count = 0usize;
        if !root.is_leaf() && (full_build || root.is_moved()) {
            stack[stack_count] = root.left_child();
            stack_count += 1;
        } else {
            let mut node = root;
            if !full_build {
                node.flag_index &= !MOVED_NODE;
            }
            self.leaf_indices[0] = 0;
            self.leaf_nodes[0] = node;
            self.leaf_centers[0] = aabb_center(node.aabb);
            leaf_count = 1;
        }

        while stack_count > 0 {
            stack_count -= 1;
            let pair = stack[stack_count];
            for i in 0..2 {
                let mut node = self.nodes[pair as usize + i];
                if !node.is_leaf() && (full_build || node.is_moved()) {
                    // todo fail gracefully if the stack runs out in release
                    debug_assert!(stack_count < TREE_STACK_SIZE);
                    stack[stack_count] = node.left_child();
                    stack_count += 1;
                    continue;
                }

                if !full_build {
                    node.flag_index &= !MOVED_NODE;
                }
                self.leaf_indices[leaf_count] = leaf_count as i32;
                self.leaf_nodes[leaf_count] = node;
                self.leaf_centers[leaf_count] = aabb_center(node.aabb);
                leaf_count += 1;
            }
        }

        debug_assert!(0 < leaf_count && leaf_count as i32 <= proxy_count);

        self.build_tree(leaf_count as i32);

        // The spare is now the tree and the old array is the spare. The build is dense, so
        // the stale tail of the old array sits above the end and is never read.
        core::mem::swap(&mut self.nodes, &mut self.swap_nodes);
        self.pair_free_list = NULL_INDEX;
        self.dfs_ordered = true;

        self.validate();
        if !full_build {
            self.validate_no_moved();
        }

        leaf_count as i32
    }
}
