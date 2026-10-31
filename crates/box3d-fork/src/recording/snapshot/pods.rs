//! Shared POD helpers for world snapshots.
//!
//! SPDX-FileCopyrightText: 2026 Erin Catto
//! SPDX-License-Identifier: MIT

use crate::bitset::BitSet;
use crate::dynamic_tree::{DynamicTree, TreeNode, TreeProxy};
use crate::id_pool::IdPool;
use crate::recording::buffer::{RecBuffer, SnapReader};
use crate::table::{HashSet, SetItem};
use crate::types::Capacity;
use crate::world::World;

pub fn ser_id_pool(buf: &mut RecBuffer, pool: &IdPool) {
    buf.append_i32(pool.next_index);
    ser_i32_array(buf, &pool.free_array);
}

pub fn des_id_pool(r: &mut SnapReader<'_>) -> IdPool {
    let next_index = r.i32();
    let free_array = des_i32_array(r);
    IdPool {
        free_array,
        next_index,
    }
}

pub fn ser_i32_array(buf: &mut RecBuffer, arr: &[i32]) {
    buf.append_i32(arr.len() as i32);
    for &v in arr {
        buf.append_i32(v);
    }
}

pub fn des_i32_array(r: &mut SnapReader<'_>) -> Vec<i32> {
    let cnt = r.i32();
    if !r.ok || !r.check_count(cnt, 4, 4) {
        r.ok = false;
        return Vec::new();
    }
    let mut out = Vec::with_capacity(cnt.max(0) as usize);
    for _ in 0..cnt.max(0) {
        out.push(r.i32());
    }
    out
}

pub fn ser_bit_set(buf: &mut RecBuffer, bs: &BitSet) {
    buf.append_u32(bs.block_count);
    if bs.block_count > 0 {
        for i in 0..bs.block_count as usize {
            buf.append_u64(bs.blocks[i]);
        }
    }
}

pub fn des_bit_set(r: &mut SnapReader<'_>) -> BitSet {
    let block_count = r.u32();
    if r.ok && !r.check_count(block_count as i32, 8, 8) {
        r.ok = false;
    }
    if !r.ok {
        return BitSet::new(0);
    }
    let block_capacity = if block_count > 0 { block_count } else { 1 };
    let mut blocks = vec![0u64; block_capacity as usize];
    for i in 0..block_count as usize {
        blocks[i] = r.u64();
    }
    BitSet {
        blocks,
        block_count,
    }
}

pub fn ser_hash_set(buf: &mut RecBuffer, hs: &HashSet) {
    let cap = hs.items.len() as u32;
    buf.append_u32(cap);
    buf.append_u32(hs.count);
    for item in &hs.items {
        buf.append_u64(item.key);
        buf.append_u32(item.hash);
        buf.append_u32(0); // pad to 16 bytes like C SetItem
    }
}

pub fn des_hash_set(r: &mut SnapReader<'_>) -> HashSet {
    let cap = r.u32();
    let cnt = r.u32();
    let valid =
        r.check_count(cap as i32, 16, 16) && (cap == 0 || (cap & (cap - 1)) == 0) && cnt <= cap;
    if r.ok && !valid && (cap != 0 || cnt != 0) {
        r.ok = false;
    }
    if !r.ok {
        return HashSet::new(16);
    }
    if cap == 0 {
        return HashSet {
            items: Vec::new(),
            count: 0,
        };
    }
    let mut items = Vec::with_capacity(cap as usize);
    for _ in 0..cap {
        let key = r.u64();
        let hash = r.u32();
        let _pad = r.u32();
        items.push(SetItem { key, hash });
    }
    HashSet { items, count: cnt }
}

// DynamicTree: version, scalars, then nodes and parents to nodeEnd and proxies at capacity.
// The empty node and the free pairs are sentinels below the end and travel as they are. Only
// the stale tail above the end, the spare array and the rebuild scratch are skipped.
pub fn ser_tree(buf: &mut RecBuffer, tree: &DynamicTree) {
    buf.append_u64(tree.version);
    buf.append_i32(tree.node_end);
    buf.append_i32(tree.pair_free_list);
    buf.append_i32(tree.proxy_count);
    buf.append_i32(tree.proxy_capacity());
    buf.append_i32(tree.proxy_free_list);
    buf.append_i32(if tree.dfs_ordered { 1 } else { 0 });
    if tree.node_end > 0 {
        for node in &tree.nodes[..tree.node_end as usize] {
            ser_tree_node(buf, node);
        }
        for &parent in &tree.parents[..tree.node_end as usize] {
            buf.append_i32(parent);
        }
    }
    for proxy in &tree.proxies {
        ser_tree_proxy(buf, proxy);
    }
}

fn ser_tree_node(buf: &mut RecBuffer, n: &TreeNode) {
    buf.append_aabb(n.aabb);
    buf.append_u32(n.flag_index);
    buf.append_i32(n.height);
}

fn ser_tree_proxy(buf: &mut RecBuffer, p: &TreeProxy) {
    buf.append_u64(p.user_data);
    buf.append_u64(p.category_bits);
    buf.append_i32(p.node);
    buf.append_i32(p.next);
}

pub fn des_tree(r: &mut SnapReader<'_>) -> DynamicTree {
    let version = r.u64();
    let node_end = r.i32();
    let pair_free_list = r.i32();
    let proxy_count = r.i32();
    let proxy_capacity = r.i32();
    let proxy_free_list = r.i32();
    let dfs_ordered = r.i32() != 0;

    let node_size = core::mem::size_of::<TreeNode>() as i32;
    let proxy_size = core::mem::size_of::<TreeProxy>() as i32;
    if r.ok && !r.check_count(node_end, node_size, node_size) {
        r.ok = false;
    }
    if r.ok && !r.check_count(proxy_capacity, proxy_size, proxy_size) {
        r.ok = false;
    }
    if r.ok && (node_end < 2 || (node_end & 1) != 0 || proxy_capacity < 1) {
        r.ok = false;
    }

    // Free existing allocation including the spare array and any rebuild scratch
    if !r.ok {
        return DynamicTree::zeroed();
    }

    // The node capacity is restored as the node end, like C.
    let mut nodes = Vec::with_capacity(node_end as usize);
    for _ in 0..node_end {
        nodes.push(des_tree_node(r));
    }
    let mut parents = Vec::with_capacity(node_end as usize);
    for _ in 0..node_end {
        parents.push(r.i32());
    }
    let mut proxies = Vec::with_capacity(proxy_capacity as usize);
    for _ in 0..proxy_capacity {
        proxies.push(des_tree_proxy(r));
    }

    let mut tree = DynamicTree::zeroed();
    tree.version = version;
    tree.nodes = nodes;
    tree.parents = parents;
    tree.proxies = proxies;
    tree.node_end = node_end;
    tree.pair_free_list = pair_free_list;
    tree.proxy_count = proxy_count;
    tree.proxy_free_list = proxy_free_list;
    tree.dfs_ordered = dfs_ordered;
    tree
}

fn des_tree_node(r: &mut SnapReader<'_>) -> TreeNode {
    TreeNode {
        aabb: r.aabb(),
        flag_index: r.u32(),
        height: r.i32(),
    }
}

fn des_tree_proxy(r: &mut SnapReader<'_>) -> TreeProxy {
    TreeProxy {
        user_data: r.u64(),
        category_bits: r.u64(),
        node: r.i32(),
        next: r.i32(),
    }
}

pub fn ser_world_config(buf: &mut RecBuffer, world: &World) {
    ser_settings_head(buf, world);
    buf.append_u64(world.step_index);
    buf.append_i32(world.split_island_id);
    buf.append_f32(world.inv_h);
    buf.append_f32(world.inv_dt);
    buf.append_i32(world.end_event_array_index);
    buf.append_i32(world.compound_shape_count);
    ser_capacity(buf, &world.max_capacity);
    ser_settings_tail(buf, world);
}

// Primordial Engine fork: the config record is split around the step state (and the capacity
// high-water marks, which grow as the world steps) so that
// ser_world_settings can write the settings alone. The bytes are unchanged.
fn ser_settings_head(buf: &mut RecBuffer, world: &World) {
    buf.append_vec3(world.gravity);
    buf.append_f32(world.hit_event_threshold);
    buf.append_f32(world.restitution_threshold);
    buf.append_i32(world.restitution_iterations);
    buf.append_f32(world.max_linear_speed);
    buf.append_f32(world.isotropic_inertia_tolerance);
    buf.append_f32(world.contact_speed);
    buf.append_f32(world.contact_hertz);
    buf.append_f32(world.contact_damping_ratio);
    buf.append_f32(world.contact_recycle_distance);
}

fn ser_settings_tail(buf: &mut RecBuffer, world: &World) {
    let mut flags = 0u8;
    if world.enable_sleep {
        flags |= 0x01;
    }
    if world.enable_warm_starting {
        flags |= 0x02;
    }
    if world.enable_continuous {
        flags |= 0x04;
    }
    if world.enable_speculative {
        flags |= 0x08;
    }
    if world.enable_restitution_propagation {
        flags |= 0x10;
    }
    // Primordial Engine fork: flag 0x20 = scale-dependent recycling enabled; its parameters
    // follow the flags byte. Disabled worlds write exactly the upstream bytes.
    let scaling = world.contact_recycle_scaling;
    if scaling.enabled {
        flags |= 0x20;
    }
    buf.append_u8(flags);
    if scaling.enabled {
        buf.append_f32(scaling.length_fraction);
        buf.append_f32(scaling.min_distance);
        buf.append_f32(scaling.max_distance);
        buf.append_u8(scaling.combine as u8);
    }
}

pub fn des_world_config(r: &mut SnapReader<'_>, world: &mut World) {
    world.gravity = r.vec3();
    world.hit_event_threshold = r.f32();
    world.restitution_threshold = r.f32();
    world.restitution_iterations = r.i32();
    world.max_linear_speed = r.f32();
    world.isotropic_inertia_tolerance = r.f32();
    world.contact_speed = r.f32();
    world.contact_hertz = r.f32();
    world.contact_damping_ratio = r.f32();
    world.contact_recycle_distance = r.f32();
    world.step_index = r.u64();
    world.split_island_id = r.i32();
    world.inv_h = r.f32();
    world.inv_dt = r.f32();
    world.end_event_array_index = r.i32();
    world.compound_shape_count = r.i32();
    world.max_capacity = des_capacity(r);
    let flags = r.u8();
    world.enable_sleep = flags & 0x01 != 0;
    world.enable_warm_starting = flags & 0x02 != 0;
    world.enable_continuous = flags & 0x04 != 0;
    world.enable_speculative = flags & 0x08 != 0;
    world.enable_restitution_propagation = flags & 0x10 != 0;
    // Primordial Engine fork: see ser_world_config. The radius callback is not part of the
    // image and stays whatever the destination world has registered.
    if flags & 0x20 != 0 {
        world.contact_recycle_scaling.enabled = true;
        world.contact_recycle_scaling.length_fraction = r.f32();
        world.contact_recycle_scaling.min_distance = r.f32();
        world.contact_recycle_scaling.max_distance = r.f32();
        let combine = r.u8();
        // Primordial Engine fork: an unknown combine byte is a corrupt image, not Min.
        if combine > 1 {
            r.ok = false;
        }
        world.contact_recycle_scaling.combine =
            crate::recycle_scale::RecycleLengthCombine::from_u8(combine);
    } else {
        world.contact_recycle_scaling.enabled = false;
    }
    // Primordial Engine fork: refuse settings World::new could never produce. The restitution
    // iterations are clamped there; an image could otherwise make every substep loop up to
    // 2^31 times. Every float setting must be finite.
    let floats = [
        world.gravity.x,
        world.gravity.y,
        world.gravity.z,
        world.hit_event_threshold,
        world.restitution_threshold,
        world.max_linear_speed,
        world.isotropic_inertia_tolerance,
        world.contact_speed,
        world.contact_hertz,
        world.contact_damping_ratio,
        world.contact_recycle_distance,
        world.inv_h,
        world.inv_dt,
        world.contact_recycle_scaling.length_fraction,
        world.contact_recycle_scaling.min_distance,
        world.contact_recycle_scaling.max_distance,
    ];
    if !(0..=crate::constants::MAX_RESTITUTION_ITERATIONS).contains(&world.restitution_iterations)
        || floats.iter().any(|f| !f.is_finite())
    {
        r.ok = false;
    }
}

/// The settings part of the world config: everything [`ser_world_config`] writes except the
/// step state (step index, split island, inverse step times, event array index, compound
/// shape count, capacity high-water marks). Two worlds with equal settings bytes simulate
/// alike.
pub fn ser_world_settings(buf: &mut RecBuffer, world: &World) {
    ser_settings_head(buf, world);
    ser_settings_tail(buf, world);
}

fn ser_capacity(buf: &mut RecBuffer, c: &Capacity) {
    buf.append_i32(c.static_shape_count);
    buf.append_i32(c.dynamic_shape_count);
    buf.append_i32(c.static_body_count);
    buf.append_i32(c.dynamic_body_count);
    buf.append_i32(c.contact_count);
}

fn des_capacity(r: &mut SnapReader<'_>) -> Capacity {
    Capacity {
        static_shape_count: r.i32(),
        dynamic_shape_count: r.i32(),
        static_body_count: r.i32(),
        dynamic_body_count: r.i32(),
        contact_count: r.i32(),
    }
}
