// Moved-flag propagation, gathering, clearing, and refit from dynamic_tree.c.
//
// SPDX-FileCopyrightText: 2025 Erin Catto
// SPDX-License-Identifier: MIT

use super::aabbv::{union_pair_v, union_v};
use super::{
    DynamicTree, TreeNode, LEAF_NODE, MOVED_NODE, NODE_INDEX_MASK, ROOT_NODE, TREE_STACK_SIZE,
};
use crate::core::NULL_INDEX;
use crate::math_functions::{aabb_contains, Aabb};

/// Slow refit for unit tests. (static b3RefitSubtree)
fn refit_subtree(nodes: &mut [TreeNode], root_index: i32) {
    let root = nodes[root_index as usize];
    if root.is_leaf() || !root.is_moved() {
        return;
    }

    // Negative entries indicate deferred unions. A parent is pushed before its children so
    // the LIFO order guarantees both children are finished when the union runs.
    let mut stack = [0i32; TREE_STACK_SIZE];
    let mut stack_count = 0usize;
    // ~0 == -1
    stack[stack_count] = !root_index;
    stack_count += 1;
    stack[stack_count] = root.left_child();
    stack_count += 1;
    stack[stack_count] = root.left_child() + 1;
    stack_count += 1;

    while stack_count > 0 {
        stack_count -= 1;
        let item = stack[stack_count];
        if item < 0 {
            let node_index = !item as usize;
            let pair = nodes[node_index].left_child() as usize;
            nodes[node_index].aabb = union_v(nodes[pair].aabb, nodes[pair + 1].aabb);
            continue;
        }

        let node = nodes[item as usize];
        if node.is_leaf() || !node.is_moved() {
            continue;
        }

        if stack_count + 3 <= TREE_STACK_SIZE {
            let pair = node.left_child();
            stack[stack_count] = !item;
            stack_count += 1;
            stack[stack_count] = pair;
            stack_count += 1;
            stack[stack_count] = pair + 1;
            stack_count += 1;
        } else {
            debug_assert!(stack_count + 3 <= TREE_STACK_SIZE);
        }
    }
}

impl DynamicTree {
    /// Set the moved flag on the ancestors of the proxy. Serial use case.
    /// (b3DynamicTree_MarkProxyMovedSerial)
    pub fn mark_proxy_moved_serial(&mut self, proxy_id: i32) {
        debug_assert!(0 <= proxy_id && proxy_id < self.proxy_capacity());

        let mut index = self.proxies[proxy_id as usize].node;
        debug_assert!(0 <= index && index < self.node_end);
        debug_assert!(self.nodes[index as usize].is_leaf());

        while index != NULL_INDEX {
            self.nodes[index as usize].flag_index |= MOVED_NODE;
            index = self.parents[index as usize];
        }
    }

    /// Update a proxy AABB and flag the ancestors as moved.
    /// (b3DynamicTree_MarkProxyMoved)
    ///
    /// The C version is thread-safe using a relaxed load and an atomic fetch-or on the
    /// ancestors, because the solver's finalize task calls it from many workers. The port
    /// is serial, so plain loads and stores stand in for the atomics. The result is the
    /// same: the flag is only ever OR'ed in, and an ancestor that is already marked means
    /// every node above it is marked (or is being marked by the walk that marked it).
    pub fn mark_proxy_moved(&mut self, proxy_id: i32, aabb: Aabb) {
        debug_assert!(0 <= proxy_id && proxy_id < self.proxy_capacity());

        let mut index = self.proxies[proxy_id as usize].node;
        debug_assert!(0 <= index && index < self.node_end);

        {
            let node = &mut self.nodes[index as usize];
            debug_assert!(node.is_leaf());
            debug_assert!(!aabb_contains(node.aabb, aabb));

            // This is not raced since it is the leaf.
            node.aabb = aabb;
            node.flag_index |= MOVED_NODE;
        }

        index = self.parents[index as usize];
        while index != NULL_INDEX {
            let node = &mut self.nodes[index as usize];

            // Read first to avoid the FetchOr if possible.
            if node.flag_index & MOVED_NODE != 0 {
                break;
            }

            // b3AtomicFetchOrU32: the previous flags cannot carry the mark here, since the
            // serial read above just saw it clear. (C: "Ancestor already visited" break.)
            node.flag_index |= MOVED_NODE;

            index = self.parents[index as usize];
        }
    }

    /// Clear the moved flags from the entire tree. (b3DynamicTree_ClearMoved)
    pub fn clear_moved(&mut self) {
        let nodes = &mut self.nodes;

        let root = &mut nodes[ROOT_NODE as usize];
        if !root.is_moved() {
            return;
        }

        root.flag_index &= !MOVED_NODE;
        if root.is_leaf() {
            return;
        }

        let mut stack = [0i32; TREE_STACK_SIZE];
        let mut stack_count = 0usize;
        stack[stack_count] = root.left_child();
        stack_count += 1;

        while stack_count > 0 {
            stack_count -= 1;
            let pair = stack[stack_count];
            for i in 0..2 {
                let node = &mut nodes[pair as usize + i];
                if node.flag_index & MOVED_NODE != 0 {
                    node.flag_index &= !MOVED_NODE;
                    if !node.is_leaf() {
                        if stack_count < TREE_STACK_SIZE {
                            stack[stack_count] = node.left_child();
                            stack_count += 1;
                        } else {
                            // Bad stuff will happen if the moved flags don't get cleared.
                            debug_assert!(stack_count < TREE_STACK_SIZE);
                        }
                    }
                }
            }
        }
    }

    /// Gather the ids of the moved proxies into `proxy_ids`, which must have room for every
    /// proxy. Returns the count. (b3DynamicTree_GatherMovedProxies)
    pub fn gather_moved_proxies(&self, proxy_ids: &mut [i32]) -> i32 {
        let nodes = &self.nodes;
        let root = &nodes[ROOT_NODE as usize];
        if !root.is_moved() {
            return 0;
        }

        if root.is_leaf() {
            proxy_ids[0] = root.proxy_id();
            return 1;
        }

        let mut count = 0usize;
        let mut stack = [0i32; TREE_STACK_SIZE];
        let mut stack_count = 0usize;
        stack[stack_count] = root.left_child();
        stack_count += 1;

        while stack_count > 0 {
            stack_count -= 1;
            let pair = stack[stack_count];
            for i in 0..2 {
                let node = &nodes[pair as usize + i];
                if !node.is_moved() {
                    continue;
                }

                if node.is_leaf() {
                    proxy_ids[count] = node.proxy_id();
                    count += 1;
                } else if stack_count < TREE_STACK_SIZE {
                    stack[stack_count] = node.left_child();
                    stack_count += 1;
                } else {
                    debug_assert!(stack_count < TREE_STACK_SIZE);
                }
            }
        }

        count as i32
    }

    /// Refit the moved internal nodes so they contain their children. The moved flags are
    /// kept for pair finding. (b3DynamicTree_Refit)
    pub fn refit(&mut self) {
        if !self.has_tree_moved() {
            return;
        }

        let nodes = &mut self.nodes;

        if !self.dfs_ordered {
            // This only happens in unit tests.
            refit_subtree(nodes, ROOT_NODE);
            return;
        }

        let mut pair = self.node_end - 2;
        while pair >= 0 {
            let p = pair as usize;
            let flags1 = nodes[p].flag_index;
            let flags2 = nodes[p + 1].flag_index;

            // Did either move?
            if ((flags1 | flags2) & MOVED_NODE) == 0 {
                pair -= 2;
                continue;
            }

            // Is the node internal and moved?
            let refit1 = (flags1 & (LEAF_NODE | MOVED_NODE)) == MOVED_NODE;
            let refit2 = (flags2 & (LEAF_NODE | MOVED_NODE)) == MOVED_NODE;

            // C: if the node needs a refit then get the child index, otherwise use a valid
            // dummy index and let the conditional store skip it. The scalar
            // b3StoreAABBV( aabb, value, condition ) is `if ( condition ) *aabb = value;`,
            // so the union is only needed when the store happens.
            if refit1 {
                let children1 = (flags1 & NODE_INDEX_MASK) as i32;
                nodes[p].aabb = union_pair_v(nodes, children1);
            }
            if refit2 {
                let children2 = (flags2 & NODE_INDEX_MASK) as i32;
                nodes[p + 1].aabb = union_pair_v(nodes, children2);
            }

            pair -= 2;
        }
    }
}
