// Primordial Engine fork: the worker threads behind `WorldDef::worker_count` (D2).
//
// Box3D C leaves threading to the application (b3WorldDef::enqueueTask / finishTask); the fork
// owns its threads instead, so a world with `worker_count > 1` spawns `worker_count - 1`
// workers once and keeps them. `WorkerPool::run` hands one job to the calling thread (worker
// 0) and every worker, and returns when all of them are done. A job decides for itself how
// to split work; the step's jobs claim blocks from atomic counters and synchronise stages the
// way C's b3SolverTask does.
//
// Determinism does not depend on this module. Every parallel stage writes each output slot
// from exactly one block, and blocks are combined in a fixed order or with order-independent
// operations (integer sums, bit ORs), so the result is the same for any worker count and any
// schedule. See docs/parallel-physics.md.
//
// Idle workers spin briefly, then yield, then sleep on a condition variable, so a world that
// is not stepping costs no CPU.
//
// Floating point: every helper runs a job with the calling thread's floating-point control
// register (MXCSR on x86, FPCR on aarch64): rounding mode, flush-to-zero and denormals-are-zero.
// Otherwise a caller with, say, flush-to-zero set would round its own blocks differently from
// the helpers', and the result would depend on which worker claimed which block. Whether the
// caller's register is the canonical one is the engine's check (design 4.3 rule 9,
// `pe_core::fpenv`).
//
// SPDX-License-Identifier: MIT

use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;

/// A job: called once per worker with the worker index, `0..worker_count`.
type Job<'a> = dyn Fn(usize) + Sync + 'a;

struct Shared {
    /// Bumped by `run` to publish a job; workers wait for it to change.
    epoch: AtomicU32,
    /// The caller's floating-point control register for the current job.
    fp_control: AtomicU64,
    /// Points at a `&Job` on the caller's stack while a job runs.
    job: AtomicPtr<()>,
    /// Workers still running the current job.
    pending: AtomicUsize,
    /// A worker job panicked.
    panicked: AtomicBool,
    shutdown: AtomicBool,
    sleepers: AtomicUsize,
    lock: Mutex<()>,
    wake: Condvar,
}

pub(crate) struct WorkerPool {
    shared: Arc<Shared>,
    threads: Vec<JoinHandle<()>>,
    worker_count: usize,
    /// Serialises `run`: one job at a time per pool.
    busy: Mutex<()>,
}

impl std::fmt::Debug for WorkerPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkerPool")
            .field("worker_count", &self.worker_count)
            .finish()
    }
}

/// Spin iterations before a waiter starts yielding, and yields before it sleeps.
const SPIN_COUNT: u32 = 2000;
const YIELD_COUNT: u32 = 200;

impl WorkerPool {
    /// Start `worker_count - 1` threads. Returns None if `worker_count < 2` or the platform
    /// cannot spawn threads (then the caller steps serially, with identical results).
    pub(crate) fn new(worker_count: usize) -> Option<WorkerPool> {
        if worker_count < 2 || cfg!(target_arch = "wasm32") {
            return None;
        }
        let shared = Arc::new(Shared {
            epoch: AtomicU32::new(0),
            fp_control: AtomicU64::new(0),
            job: AtomicPtr::new(std::ptr::null_mut()),
            pending: AtomicUsize::new(0),
            panicked: AtomicBool::new(false),
            shutdown: AtomicBool::new(false),
            sleepers: AtomicUsize::new(0),
            lock: Mutex::new(()),
            wake: Condvar::new(),
        });
        let mut threads = Vec::with_capacity(worker_count - 1);
        for worker_index in 1..worker_count {
            let worker_shared = Arc::clone(&shared);
            let spawned = std::thread::Builder::new()
                .name(format!("box3d-worker-{worker_index}"))
                .spawn(move || worker_main(&worker_shared, worker_index));
            match spawned {
                Ok(handle) => threads.push(handle),
                Err(_) => {
                    let pool = WorkerPool {
                        shared,
                        threads,
                        worker_count,
                        busy: Mutex::new(()),
                    };
                    drop(pool);
                    return None;
                }
            }
        }
        Some(WorkerPool {
            shared,
            threads,
            worker_count,
            busy: Mutex::new(()),
        })
    }

    pub(crate) fn worker_count(&self) -> usize {
        self.worker_count
    }

    /// Run `job(worker_index)` on every worker, the caller being worker 0, and wait for all.
    /// The job must tolerate any interleaving: workers start at different times and a slow
    /// worker may find nothing left to do.
    pub(crate) fn run(&self, job: &Job<'_>) {
        let _busy = self.busy.lock().unwrap_or_else(|e| e.into_inner());
        let shared = &*self.shared;
        let helpers = self.worker_count - 1;

        // Publish the job. The pointer is to a fat `&Job` living on this stack frame; the
        // guard below keeps the frame alive until every worker has finished with it.
        let job_ref: &Job<'_> = job;
        let job_ptr = &job_ref as *const &Job<'_> as *mut ();
        shared.pending.store(helpers, Ordering::Relaxed);
        shared.fp_control.store(fp_control::get(), Ordering::Relaxed);
        shared.job.store(job_ptr, Ordering::Relaxed);
        shared.epoch.fetch_add(1, Ordering::SeqCst);
        if shared.sleepers.load(Ordering::SeqCst) > 0 {
            let _g = shared.lock.lock().unwrap_or_else(|e| e.into_inner());
            shared.wake.notify_all();
        }

        struct WaitAll<'a>(&'a Shared);
        impl Drop for WaitAll<'_> {
            fn drop(&mut self) {
                wait_until(|| self.0.pending.load(Ordering::Acquire) == 0);
                self.0.job.store(std::ptr::null_mut(), Ordering::Relaxed);
            }
        }
        let wait = WaitAll(shared);
        job(0);
        drop(wait);

        if shared.panicked.swap(false, Ordering::Relaxed) {
            panic!("box3d worker thread panicked");
        }
    }
}

impl Drop for WorkerPool {
    fn drop(&mut self) {
        self.shared.shutdown.store(true, Ordering::Release);
        self.shared.epoch.fetch_add(1, Ordering::Release);
        {
            let _g = self.shared.lock.lock().unwrap_or_else(|e| e.into_inner());
            self.shared.wake.notify_all();
        }
        for handle in self.threads.drain(..) {
            let _ = handle.join();
        }
    }
}

/// Spin, then yield, until `ready()`.
#[inline]
pub(crate) fn wait_until(ready: impl Fn() -> bool) {
    let mut spins = 0u32;
    while !ready() {
        if spins < SPIN_COUNT {
            std::hint::spin_loop();
            spins += 1;
        } else {
            std::thread::yield_now();
        }
    }
}

fn worker_main(shared: &Shared, worker_index: usize) {
    // Start from the pool's initial epoch, not the current one: a thread that starts late must
    // still run the jobs published before it got here (`run` waits for every worker).
    let mut seen = 0u32;
    loop {
        // Wait for a new epoch: spin, yield, then sleep.
        let mut spins = 0u32;
        let mut yields = 0u32;
        loop {
            let epoch = shared.epoch.load(Ordering::Acquire);
            if epoch != seen {
                seen = epoch;
                break;
            }
            if spins < SPIN_COUNT {
                std::hint::spin_loop();
                spins += 1;
            } else if yields < YIELD_COUNT {
                std::thread::yield_now();
                yields += 1;
            } else {
                let guard = shared.lock.lock().unwrap_or_else(|e| e.into_inner());
                shared.sleepers.fetch_add(1, Ordering::SeqCst);
                let guard = if shared.epoch.load(Ordering::SeqCst) == seen {
                    shared.wake.wait(guard).unwrap_or_else(|e| e.into_inner())
                } else {
                    guard
                };
                shared.sleepers.fetch_sub(1, Ordering::SeqCst);
                drop(guard);
            }
        }
        if shared.shutdown.load(Ordering::Acquire) {
            return;
        }
        let job_ptr = shared.job.load(Ordering::Relaxed) as *const &Job<'_>;
        if !job_ptr.is_null() {
            fp_control::set(shared.fp_control.load(Ordering::Relaxed));
            // SAFETY: `run` keeps the pointee alive until `pending` reaches zero, which
            // happens only after this call returns.
            let job: &Job<'_> = unsafe { *job_ptr };
            let result =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| job(worker_index)));
            if result.is_err() {
                shared.panicked.store(true, Ordering::Relaxed);
            }
        }
        shared.pending.fetch_sub(1, Ordering::Release);
    }
}

/// The floating-point control register: MXCSR (x86, sticky exception flags masked off) or
/// FPCR (aarch64). Elsewhere a no-op returning 0.
pub(crate) mod fp_control {
    #[cfg(any(target_arch = "x86_64", target_arch = "x86"))]
    pub(crate) fn get() -> u64 {
        let mut v: u32 = 0;
        // SAFETY: stores MXCSR into `v`.
        unsafe { core::arch::asm!("stmxcsr [{}]", in(reg) &mut v, options(nostack, preserves_flags)) };
        (v & !0x3F) as u64
    }

    #[cfg(any(target_arch = "x86_64", target_arch = "x86"))]
    pub(crate) fn set(v: u64) {
        let v = v as u32;
        // SAFETY: loads a value read from MXCSR (reserved bits zero).
        unsafe { core::arch::asm!("ldmxcsr [{}]", in(reg) &v, options(nostack, preserves_flags)) };
    }

    #[cfg(target_arch = "aarch64")]
    pub(crate) fn get() -> u64 {
        let v: u64;
        // SAFETY: reads FPCR.
        unsafe { core::arch::asm!("mrs {}, fpcr", out(reg) v, options(nomem, nostack, preserves_flags)) };
        v
    }

    #[cfg(target_arch = "aarch64")]
    pub(crate) fn set(v: u64) {
        // SAFETY: writes a value read from FPCR.
        unsafe { core::arch::asm!("msr fpcr, {}", in(reg) v, options(nomem, nostack, preserves_flags)) };
    }

    #[cfg(not(any(target_arch = "x86_64", target_arch = "x86", target_arch = "aarch64")))]
    pub(crate) fn get() -> u64 {
        0
    }

    #[cfg(not(any(target_arch = "x86_64", target_arch = "x86", target_arch = "aarch64")))]
    pub(crate) fn set(_v: u64) {}
}

/// A raw pointer that may be shared between workers. Each user documents why its accesses
/// are disjoint.
pub(crate) struct SharedMut<T>(pub *mut T);
impl<T> Clone for SharedMut<T> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<T> Copy for SharedMut<T> {}
unsafe impl<T: Send> Send for SharedMut<T> {}
unsafe impl<T: Send> Sync for SharedMut<T> {}

impl<T> SharedMut<T> {
    pub(crate) fn new(slice: &mut [T]) -> SharedMut<T> {
        SharedMut(slice.as_mut_ptr())
    }

    /// The pointer. (A method, so closures capture the `Sync` wrapper, not the raw field.)
    #[inline]
    pub(crate) fn ptr(&self) -> *mut T {
        self.0
    }

    /// # Safety
    /// `index` is in bounds of the slice this came from, and no other worker accesses that
    /// element while the reference lives.
    #[inline]
    #[allow(clippy::mut_from_ref)]
    pub(crate) unsafe fn get_mut(&self, index: usize) -> &mut T {
        unsafe { &mut *self.0.add(index) }
    }
}

/// A slice that may be shared between workers, each touching only its own elements.
pub(crate) struct SharedSlice<T> {
    ptr: *mut T,
    len: usize,
}
impl<T> Clone for SharedSlice<T> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<T> Copy for SharedSlice<T> {}
unsafe impl<T: Send> Send for SharedSlice<T> {}
unsafe impl<T: Send + Sync> Sync for SharedSlice<T> {}

impl<T> SharedSlice<T> {
    pub(crate) fn new(slice: &mut [T]) -> SharedSlice<T> {
        SharedSlice {
            ptr: slice.as_mut_ptr(),
            len: slice.len(),
        }
    }

    /// # Safety
    /// The slice is live and no other thread writes element `index` while the reference lives.
    #[inline]
    pub(crate) unsafe fn get<'a>(&self, index: usize) -> &'a T {
        assert!(index < self.len);
        unsafe { &*self.ptr.add(index) }
    }

    /// # Safety
    /// The slice is live and no other thread accesses element `index` while the reference lives.
    #[inline]
    #[allow(clippy::mut_from_ref)]
    pub(crate) unsafe fn get_mut<'a>(&self, index: usize) -> &'a mut T {
        assert!(index < self.len);
        unsafe { &mut *self.ptr.add(index) }
    }
}

/// Claim fixed-size blocks of `0..item_count` from a shared counter. Each block is claimed
/// exactly once, by whichever worker gets there first.
pub(crate) struct BlockCounter {
    next: AtomicUsize,
    item_count: usize,
    block_size: usize,
}

impl BlockCounter {
    pub(crate) fn new(item_count: usize, block_size: usize) -> BlockCounter {
        BlockCounter {
            next: AtomicUsize::new(0),
            item_count,
            block_size: block_size.max(1),
        }
    }

    #[inline]
    pub(crate) fn claim(&self) -> Option<std::ops::Range<usize>> {
        let start = self.next.fetch_add(self.block_size, Ordering::Relaxed);
        if start >= self.item_count {
            return None;
        }
        Some(start..(start + self.block_size).min(self.item_count))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A job published right after the pool starts must reach threads that start late
    /// (regression: workers used to take the current epoch as already seen).
    #[test]
    fn first_job_reaches_every_worker() {
        for workers in [2usize, 3, 8, 16, 32] {
            for _ in 0..20 {
                let pool = WorkerPool::new(workers).expect("threads");
                let hits = AtomicUsize::new(0);
                pool.run(&|_| {
                    hits.fetch_add(1, Ordering::Relaxed);
                });
                assert_eq!(hits.load(Ordering::Relaxed), workers);
            }
        }
    }

    #[test]
    fn blocks_are_claimed_once_across_many_jobs() {
        let pool = WorkerPool::new(8).expect("threads");
        for job in 0..2000usize {
            let n = job % 300;
            let blocks = BlockCounter::new(n, 7);
            let seen: Vec<AtomicUsize> = (0..n).map(|_| AtomicUsize::new(0)).collect();
            pool.run(&|_| {
                while let Some(range) = blocks.claim() {
                    for i in range {
                        seen[i].fetch_add(1, Ordering::Relaxed);
                    }
                }
            });
            assert!(seen.iter().all(|s| s.load(Ordering::Relaxed) == 1));
        }
    }

    /// Helpers adopt the caller's FP control register for each job (review of 73eaedc, P2).
    #[cfg(any(target_arch = "x86_64", target_arch = "x86"))]
    #[test]
    fn helpers_use_the_callers_fp_control() {
        let pool = WorkerPool::new(4).expect("threads");
        let original = fp_control::get();
        for control in [original, original | 0x8040, (original & !0x6000) | 0x2000, original] {
            fp_control::set(control);
            let seen: Vec<AtomicU64> = (0..4).map(|_| AtomicU64::new(u64::MAX)).collect();
            pool.run(&|worker| seen[worker].store(fp_control::get(), Ordering::Relaxed));
            fp_control::set(original);
            for (worker, s) in seen.iter().enumerate() {
                assert_eq!(s.load(Ordering::Relaxed), control, "worker {worker}");
            }
        }
    }

    #[test]
    fn single_worker_has_no_pool() {
        assert!(WorkerPool::new(1).is_none());
        assert!(WorkerPool::new(0).is_none());
    }
}
