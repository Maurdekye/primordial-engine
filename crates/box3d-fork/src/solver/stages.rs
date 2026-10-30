//! Primordial Engine fork (D2): solver stages run across the world's worker threads.
//!
//! This is C's b3SolverTask design. Worker 0 runs the solver driver; every parallel stage it
//! reaches is published to the other workers, who spin waiting for it, claim blocks of the
//! stage's items, and go back to waiting. A stage ends when all of its items are done, so the
//! next stage sees every write of the previous one. Work that C keeps on the main thread
//! (overflow constraints, joints here) runs on worker 0 between stages.
//!
//! Blocks are claimed with a compare-exchange on a word that holds the stage sequence number
//! in its high half, so a worker still finishing an old stage can never claim a block of the
//! next one (C does the same with per-block sync indices).
//!
//! Results do not depend on the block split or on which worker runs a block: within a stage
//! every item is independent (graph coloring for constraints, one body per item for body
//! stages). [`SerialStages`] runs each stage as one block on the caller, so the serial path is
//! the same code.
//!
//! SPDX-License-Identifier: MIT

use std::ops::Range;
use std::sync::atomic::{AtomicPtr, AtomicU32, AtomicU64, AtomicUsize, Ordering};

use crate::worker_pool::wait_until;

/// One stage's work: `block(items, worker_index)`.
pub(crate) type StageBlock<'a> = dyn Fn(Range<usize>, usize) + Sync + 'a;

pub(crate) trait Stages {
    /// Run `block` over `0..item_count` in blocks of `block_size`; return when all are done.
    fn run(&self, item_count: usize, block_size: usize, block: &StageBlock<'_>);
}

/// Every stage as a single block on the calling thread (worker 0).
pub(crate) struct SerialStages;

impl Stages for SerialStages {
    #[inline]
    fn run(&self, item_count: usize, _block_size: usize, block: &StageBlock<'_>) {
        if item_count > 0 {
            block(0..item_count, 0);
        }
    }
}

const EXIT: u32 = u32::MAX;

/// Shared state of one parallel solve.
pub(crate) struct StageSync {
    /// The published stage (EXIT when the solve is over). Workers wait for it to change.
    seq: AtomicU32,
    /// `(seq << 32) | next_item`: the claim word of the current stage.
    claim: AtomicU64,
    /// Points at a `&StageBlock` on worker 0's stack while the stage runs.
    block: AtomicPtr<()>,
    item_count: AtomicUsize,
    block_size: AtomicUsize,
    /// Items finished in the current stage.
    done: AtomicUsize,
}

impl StageSync {
    pub(crate) fn new() -> StageSync {
        StageSync {
            seq: AtomicU32::new(0),
            claim: AtomicU64::new(0),
            block: AtomicPtr::new(std::ptr::null_mut()),
            item_count: AtomicUsize::new(0),
            block_size: AtomicUsize::new(1),
            done: AtomicUsize::new(0),
        }
    }

    /// Claim and run blocks of stage `seq` until none are left.
    fn work(
        &self,
        seq: u32,
        item_count: usize,
        block_size: usize,
        worker: usize,
        run: impl Fn(Range<usize>, usize),
    ) {
        loop {
            let current = self.claim.load(Ordering::Acquire);
            if (current >> 32) as u32 != seq {
                return;
            }
            let start = (current & 0xFFFF_FFFF) as usize;
            if start >= item_count {
                return;
            }
            let end = (start + block_size).min(item_count);
            let next = ((seq as u64) << 32) | end as u64;
            if self
                .claim
                .compare_exchange_weak(current, next, Ordering::AcqRel, Ordering::Relaxed)
                .is_ok()
            {
                run(start..end, worker);
                self.done.fetch_add(end - start, Ordering::Release);
            }
        }
    }

    /// The loop of workers 1.. for the whole solve.
    pub(crate) fn worker_loop(&self, worker: usize) {
        let mut seen = 0u32;
        loop {
            let mut seq = self.seq.load(Ordering::Acquire);
            if seq == seen {
                wait_until(|| self.seq.load(Ordering::Acquire) != seen);
                seq = self.seq.load(Ordering::Acquire);
            }
            if seq == EXIT {
                return;
            }
            seen = seq;
            let block_ptr = self.block.load(Ordering::Acquire) as *const &StageBlock<'_>;
            let item_count = self.item_count.load(Ordering::Acquire);
            let block_size = self.block_size.load(Ordering::Acquire);
            // These loads may already see a later stage; then every claim under `seq` fails
            // and nothing is run. A successful claim under `seq` means stage `seq` is still
            // running, so `block_ptr` is its block and worker 0 keeps it alive until the
            // claimed items are done.
            self.work(seq, item_count, block_size, worker, |items, worker| {
                // SAFETY: see above.
                let block: &StageBlock<'_> = unsafe { *block_ptr };
                block(items, worker);
            });
        }
    }

    /// End the solve: workers leave `worker_loop`.
    pub(crate) fn finish(&self) {
        self.seq.store(EXIT, Ordering::Release);
    }
}

/// Stages published to the workers by worker 0.
pub(crate) struct ParallelStages<'a> {
    pub(crate) sync: &'a StageSync,
    next_seq: std::cell::Cell<u32>,
}

impl<'a> ParallelStages<'a> {
    pub(crate) fn new(sync: &'a StageSync) -> ParallelStages<'a> {
        ParallelStages {
            sync,
            next_seq: std::cell::Cell::new(1),
        }
    }
}

impl Stages for ParallelStages<'_> {
    fn run(&self, item_count: usize, block_size: usize, block: &StageBlock<'_>) {
        if item_count == 0 {
            return;
        }
        let block_size = block_size.max(1);
        if item_count <= block_size {
            // One block: not worth waking anyone (C's b3ExecuteMainStage does the same).
            block(0..item_count, 0);
            return;
        }
        assert!(item_count < u32::MAX as usize);
        let sync = self.sync;
        let seq = self.next_seq.get();
        assert!(seq != EXIT);
        self.next_seq.set(seq + 1);

        let block_ref: &StageBlock<'_> = block;
        sync.block
            .store(&block_ref as *const &StageBlock<'_> as *mut (), Ordering::Relaxed);
        sync.item_count.store(item_count, Ordering::Relaxed);
        sync.block_size.store(block_size, Ordering::Relaxed);
        sync.done.store(0, Ordering::Relaxed);
        sync.claim.store((seq as u64) << 32, Ordering::Release);
        sync.seq.store(seq, Ordering::Release);

        sync.work(seq, item_count, block_size, 0, block);
        wait_until(|| sync.done.load(Ordering::Acquire) == item_count);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::worker_pool::WorkerPool;

    /// Thousands of back-to-back stages of varying size: every item of every stage runs
    /// exactly once, under that stage (a stale worker never runs a block of a later stage).
    #[test]
    fn every_item_once_per_stage() {
        for workers in [2usize, 4, 16] {
            let pool = WorkerPool::new(workers).expect("threads");
            let sync = StageSync::new();
            let stage_count = 3000usize;
            let counts: Vec<AtomicUsize> = (0..stage_count).map(|_| AtomicUsize::new(0)).collect();
            let wrong = AtomicUsize::new(0);
            pool.run(&|worker| {
                if worker == 0 {
                    let stages = ParallelStages::new(&sync);
                    for stage in 0..stage_count {
                        let n = (stage * 37) % 500;
                        let hits: Vec<AtomicUsize> = (0..n).map(|_| AtomicUsize::new(0)).collect();
                        stages.run(n, 1 + stage % 9, &|items, _| {
                            for i in items {
                                hits[i].fetch_add(1, Ordering::Relaxed);
                            }
                            counts[stage].fetch_add(1, Ordering::Relaxed);
                        });
                        if hits.iter().any(|h| h.load(Ordering::Relaxed) != 1) {
                            wrong.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    sync.finish();
                } else {
                    sync.worker_loop(worker);
                }
            });
            assert_eq!(wrong.load(Ordering::Relaxed), 0, "{workers} workers");
        }
    }
}
