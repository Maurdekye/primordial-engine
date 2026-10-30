// Port of box3d-cpp-reference/src/broad_phase.h and broad_phase.c: storage, proxy
// operations, overlap testing, and validation. The tree-against-tree pair finder
// (b3UpdateBroadPhasePairs) lives in pairs.rs.
//
// SPDX-FileCopyrightText: 2025 Erin Catto
// SPDX-License-Identifier: MIT

mod pairs;

pub use pairs::update_broad_phase_pairs;

use crate::dynamic_tree::DynamicTree;
use crate::math_functions::{aabb_overlaps, max_int, Aabb};
use crate::table::HashSet;
use crate::types::{BodyType, Capacity, BODY_TYPE_COUNT};

/// Store the proxy type in the lower 2 bits of the proxy key. This leaves 30 bits for the
/// id. (B3_PROXY_TYPE)
pub fn proxy_type(key: i32) -> BodyType {
    match key & 3 {
        0 => BodyType::Static,
        1 => BodyType::Kinematic,
        _ => BodyType::Dynamic,
    }
}

/// (B3_PROXY_ID)
pub fn proxy_id(key: i32) -> i32 {
    key >> 2
}

/// (B3_PROXY_KEY)
pub fn proxy_key(id: i32, type_: BodyType) -> i32 {
    (id << 2) | (type_ as i32)
}

/// The broad-phase is used for computing pairs and performing volume queries and ray
/// casts. This broad-phase does not persist pairs. Instead, this reports potentially new
/// pairs. It is up to the client to consume the new pairs and to track subsequent
/// overlap. (b3BroadPhase)
#[derive(Debug)]
pub struct BroadPhase {
    /// One tree for each body type.
    pub trees: [DynamicTree; BODY_TYPE_COUNT],

    /// The moved siblings gathered from the dynamic body tree. Per step scratch: C stack
    /// allocates it inside b3UpdateBroadPhasePairs and nulls it afterwards; the port
    /// keeps the buffer and clears it.
    pub moved_siblings: Vec<i32>,

    /// Tracks shape pairs that have a Contact.
    // todo pairSet can grow quite large on the first time step and remain large
    pub pair_set: HashSet,
}

impl BroadPhase {
    /// (b3CreateBroadPhase)
    pub fn new(capacity: &Capacity) -> BroadPhase {
        debug_assert!(BODY_TYPE_COUNT == 3);

        let pair_set = HashSet::new(max_int(32, 2 * capacity.contact_count));

        let static_capacity = max_int(16, capacity.static_shape_count);
        let kinematic_capacity = 16;
        let dynamic_capacity = max_int(16, capacity.dynamic_shape_count);

        BroadPhase {
            trees: [
                DynamicTree::new(static_capacity),
                DynamicTree::new(kinematic_capacity),
                DynamicTree::new(dynamic_capacity),
            ],
            moved_siblings: Vec::new(),
            pair_set,
        }
    }

    /// (b3DestroyBroadPhase)
    pub fn destroy(&mut self) {
        for tree in &mut self.trees {
            tree.destroy();
        }

        // memset( bp, 0, sizeof( b3BroadPhase ) )
        *self = BroadPhase {
            trees: [
                DynamicTree::zeroed(),
                DynamicTree::zeroed(),
                DynamicTree::zeroed(),
            ],
            moved_siblings: Vec::new(),
            pair_set: HashSet::default(),
        };
    }

    /// (b3BroadPhase_CreateProxy)
    pub fn create_proxy(
        &mut self,
        proxy_type_: BodyType,
        aabb: Aabb,
        category_bits: u64,
        shape_index: i32,
        force_pair_creation: bool,
    ) -> i32 {
        debug_assert!((proxy_type_ as usize) < BODY_TYPE_COUNT);

        let mark = proxy_type_ != BodyType::Static || force_pair_creation;

        let proxy_id_ = self.trees[proxy_type_ as usize].create_proxy_internal(
            aabb,
            category_bits,
            shape_index as u64,
            mark,
        );
        proxy_key(proxy_id_, proxy_type_)
    }

    /// (b3BroadPhase_DestroyProxy)
    pub fn destroy_proxy(&mut self, proxy_key_: i32) {
        let proxy_type_ = proxy_type(proxy_key_);
        let proxy_id_ = proxy_id(proxy_key_);

        debug_assert!((proxy_type_ as usize) < BODY_TYPE_COUNT);
        self.trees[proxy_type_ as usize].destroy_proxy(proxy_id_);
    }

    /// (b3BroadPhase_MoveProxy)
    pub fn move_proxy(&mut self, proxy_key_: i32, aabb: Aabb) {
        let proxy_type_ = proxy_type(proxy_key_);
        let proxy_id_ = proxy_id(proxy_key_);

        let mark = true;
        self.trees[proxy_type_ as usize].move_proxy_internal(proxy_id_, aabb, mark);
    }

    /// (b3BroadPhase_MarkProxyMovedSerial)
    pub fn mark_proxy_moved_serial(&mut self, proxy_key_: i32) {
        let proxy_type_ = proxy_type(proxy_key_);
        let proxy_id_ = proxy_id(proxy_key_);
        self.trees[proxy_type_ as usize].mark_proxy_moved_serial(proxy_id_);
    }

    /// (b3BroadPhase_MarkProxyMoved)
    pub fn mark_proxy_moved(&mut self, proxy_key_: i32, aabb: Aabb) {
        let proxy_type_ = proxy_type(proxy_key_);
        let proxy_id_ = proxy_id(proxy_key_);
        self.trees[proxy_type_ as usize].mark_proxy_moved(proxy_id_, aabb);
    }

    /// (b3BroadPhase_GetShapeIndex)
    pub fn shape_index(&self, proxy_key_: i32) -> i32 {
        let proxy_type_ = proxy_type(proxy_key_);
        let proxy_id_ = proxy_id(proxy_key_);

        self.trees[proxy_type_ as usize].user_data(proxy_id_) as i32
    }

    /// (b3BroadPhase_TestOverlap)
    pub fn test_overlap(&self, proxy_key_a: i32, proxy_key_b: i32) -> bool {
        let type_a = proxy_type(proxy_key_a);
        let id_a = proxy_id(proxy_key_a);
        let type_b = proxy_type(proxy_key_b);
        let id_b = proxy_id(proxy_key_b);

        let aabb_a = self.trees[type_a as usize].aabb(id_a);
        let aabb_b = self.trees[type_b as usize].aabb(id_b);
        aabb_overlaps(aabb_a, aabb_b)
    }

    /// (b3ValidateBroadPhase)
    pub fn validate(&self) {
        self.trees[BodyType::Dynamic as usize].validate();
        self.trees[BodyType::Kinematic as usize].validate();

        // todo validate every shape AABB is contained in tree AABB
    }

    /// (b3ValidateNoMoved — C compiles the body under B3_ENABLE_VALIDATION; here the check
    /// runs in debug builds only)
    pub fn validate_no_moved(&self) {
        if cfg!(debug_assertions) {
            for tree in &self.trees {
                tree.validate_no_moved();
            }
        }
    }
}
