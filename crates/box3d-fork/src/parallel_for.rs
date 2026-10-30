// Port of box3d-cpp-reference/src/parallel_for.c for the serial task path.
//
// C splits the item range into blocks and enqueues one trampoline task per worker; each
// trampoline claims blocks from a shared atomic counter until none are left. With the
// default task system (b3DefaultAddTaskFcn runs a task inline and returns NULL) the first
// trampoline, worker 0, runs to completion before the others are enqueued, so it claims
// every block in order and the remaining trampolines find nothing to do. The port is
// serial and reproduces exactly that: every block, in order, on worker 0. The block
// partition matters to callers whose per-block epilogue is observable (for example the
// broad-phase flushes its candidate batch at the end of each block).
//
// SPDX-FileCopyrightText: 2026 Erin Catto
// SPDX-License-Identifier: MIT

use crate::constants::MAX_WORKERS;

/// Run `callback(start_index, end_index, worker_index)` over `0..item_count` in the blocks
/// C's b3ParallelFor would use for `worker_count` workers and `min_range`. (b3ParallelFor)
pub(crate) fn parallel_for(
    worker_count: i32,
    item_count: i32,
    min_range: i32,
    mut callback: impl FnMut(i32, i32, i32),
) {
    if item_count <= 0 {
        return;
    }

    debug_assert!(min_range > 0);
    debug_assert!(0 < worker_count && worker_count <= MAX_WORKERS);

    // Target multiple blocks per worker to reduce thread stalls.
    // block size grows once items exceed maxBlockCount * minRange
    // so the block count stays bounded and per-block sync overhead stays low.
    // Benchmarking shows 32 is optimal for the convex pile benchmark and others.
    // This is a lot different than Box2D because the 3D narrow phase cost
    // varies a lot.
    let blocks_per_worker = 32;
    let max_block_count = blocks_per_worker * worker_count;

    let block_size;
    let block_count;
    if item_count <= min_range * max_block_count {
        block_size = min_range;
        block_count = (item_count + block_size - 1) / block_size;
    } else {
        block_size = (item_count + max_block_count - 1) / max_block_count;
        block_count = (item_count + block_size - 1) / block_size;
    }
    debug_assert!(block_count >= 1);
    debug_assert!(block_size * block_count >= item_count);

    // (b3ParallelForTrampoline for worker 0, which claims every block serially)
    let worker_index = 0;
    for block_index in 0..block_count {
        let start = block_index * block_size;
        let end = (start + block_size).min(item_count);
        callback(start, end, worker_index);
    }
}
