// Sibling pair and proxy allocation from dynamic_tree.c.
//
// SPDX-FileCopyrightText: 2025 Erin Catto
// SPDX-License-Identifier: MIT

use super::{make_empty_node, DynamicTree, TreeNode, TreeProxy};
use crate::core::NULL_INDEX;
use crate::math_functions::max_int;

impl DynamicTree {
    /// The free sibling pair list is chained through the parent index of their first node.
    /// A pair comes from the free list when there is one and from the bump pointer
    /// otherwise. A rebuild empties the free list, so allocations can just bump.
    /// (static b3AllocateSiblingPair)
    pub(crate) fn allocate_sibling_pair(&mut self) -> i32 {
        // Allocate from free list.
        if self.pair_free_list != NULL_INDEX {
            let pair = self.pair_free_list;
            self.pair_free_list = self.parents[pair as usize];
            return pair;
        }

        // Extend the bump allocator if needed.
        if self.node_end + 2 > self.node_capacity() {
            let old_capacity = self.node_capacity();
            let mut new_capacity = old_capacity + (old_capacity >> 1);
            new_capacity += new_capacity & 1;

            // B3_GROW_ZERO
            self.nodes.resize(new_capacity as usize, TreeNode::zeroed());
            self.parents.resize(new_capacity as usize, 0);

            // The spare has to match, the rebuild allocates it again.
            self.swap_nodes = Vec::new();
        }

        let pair = self.node_end;
        self.node_end += 2;
        pair
    }

    /// (static b3FreePair)
    pub(crate) fn free_pair(&mut self, pair: i32) {
        debug_assert!((pair & 1) == 0 && 2 <= pair && pair < self.node_end);
        self.nodes[pair as usize] = make_empty_node();
        self.nodes[pair as usize + 1] = make_empty_node();
        self.parents[pair as usize] = self.pair_free_list;
        self.parents[pair as usize + 1] = NULL_INDEX;
        self.pair_free_list = pair;
    }

    /// Allocate a proxy from the pool. Grow the pool if necessary.
    /// (static b3AllocateProxy)
    pub(crate) fn allocate_proxy(&mut self) -> i32 {
        // Expand the pool as needed.
        if self.proxy_free_list == NULL_INDEX {
            debug_assert!(self.proxy_count == self.proxy_capacity());

            // The free list is empty. Rebuild a bigger pool.
            // A restored pool can have capacity 1, so guarantee strict growth.
            let old_capacity = self.proxy_capacity();
            let new_capacity = old_capacity + max_int(old_capacity >> 1, 1);
            self.proxies
                .resize(new_capacity as usize, TreeProxy::default());

            // Build a linked list for the free list.
            for i in old_capacity..new_capacity - 1 {
                self.proxies[i as usize].node = NULL_INDEX;
                self.proxies[i as usize].next = i + 1;
            }

            self.proxies[new_capacity as usize - 1].node = NULL_INDEX;
            self.proxies[new_capacity as usize - 1].next = NULL_INDEX;
            self.proxy_free_list = old_capacity;
        }

        // Peel a proxy off the free list.
        let proxy_index = self.proxy_free_list;
        self.proxy_free_list = self.proxies[proxy_index as usize].next;
        self.proxies[proxy_index as usize] = TreeProxy {
            user_data: 0,
            category_bits: 0,
            node: NULL_INDEX,
            next: NULL_INDEX,
        };
        self.proxy_count += 1;
        proxy_index
    }

    /// Return a proxy to the pool. (static b3FreeProxy)
    pub(crate) fn free_proxy(&mut self, proxy_id: i32) {
        debug_assert!(0 <= proxy_id && proxy_id < self.proxy_capacity());
        debug_assert!(0 < self.proxy_count);
        self.proxies[proxy_id as usize].node = NULL_INDEX;
        self.proxies[proxy_id as usize].next = self.proxy_free_list;
        self.proxy_free_list = proxy_id;
        self.proxy_count -= 1;
    }
}
