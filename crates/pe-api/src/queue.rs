//! The bounded edit queue between WorldApi's handlers and the simulation thread (api.md 9.2 steps 4-5; app.md 2,
//! 3 step 2; shape agreed with @engine-architect, m9990f847931c).
//!
//! - API side: `submit` never blocks; a full queue answers `Full` with a retry time (`rate_limited`). It wakes the
//!   simulation thread through the hook pe-app gives at construction. `receipt` blocks only the API thread, until
//!   the ticket's receipt arrives or the deadline passes.
//! - Simulation side: `drain` takes at most `max` per tick, oldest first; it stamps each with the frame it
//!   simulates and submits it to agreement, which assigns the entry id; `settle` hands back that `TxId` and frame,
//!   or why it couldn't be submitted.
//! - The outcome, and the later final, refused or dropped status, come from the per-frame change record (the same
//!   one `watch` reads), not from here. Blob bytes go into the store on the API thread before `submit`.
//! - Receipts are kept until taken or for `RECEIPT_TTL`, so an abandoned ticket can't grow memory.

use std::collections::{HashMap, VecDeque};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

use pe_core::ChainId;
use pe_seams::SubmitError;
use pe_seams::world_api::{Frame, TxId};
use pe_world::edit::Transaction;

/// The most changes waiting for the simulation thread [I].
pub const CAPACITY: usize = 256;
/// How long a receipt waits to be taken.
pub const RECEIPT_TTL: Duration = Duration::from_secs(5);

/// One change for the simulation thread to stamp and submit.
#[derive(Clone, Debug, PartialEq)]
pub struct Submission {
    pub tx: Transaction,
    /// The frame it was resolved against, for pe-api's own reporting (the preconditions catch races at apply).
    pub base: Frame,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Ticket(pub u64);

/// What became of a ticket when the simulation thread drained it.
#[derive(Clone, Debug, PartialEq)]
pub enum Receipt {
    /// Submitted to agreement as entry `change`, stamped `frame`, at `seq` on `chain` (for final cuts' `includes`).
    Submitted { change: TxId, frame: Frame, chain: ChainId, seq: u64 },
    /// Not submitted: agreement's reason (S3 `submit`).
    Refused(SubmitError),
}

/// The queue is full; try again after this long.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Full {
    pub retry_after: Duration,
}

struct Inner {
    next: u64,
    pending: VecDeque<(Ticket, Submission)>,
    receipts: HashMap<Ticket, (Receipt, Instant)>,
}

pub struct EditQueue {
    inner: Mutex<Inner>,
    settled: Condvar,
    capacity: usize,
    wake: Box<dyn Fn() + Send + Sync>,
    /// The change log every submitted change is tracked in, where its receipt is made, so final cuts decide it
    /// whether or not anyone takes the receipt (@chunks-reviewer's Low on c322084e). Set by `Api::new`.
    log: std::sync::OnceLock<std::sync::Arc<crate::changes::ChangeLog>>,
}

impl EditQueue {
    /// A queue of `capacity` that calls `wake` (the simulation thread's waker) on every submit.
    pub fn new(capacity: usize, wake: impl Fn() + Send + Sync + 'static) -> Self {
        assert!(capacity > 0);
        EditQueue {
            inner: Mutex::new(Inner { next: 1, pending: VecDeque::new(), receipts: HashMap::new() }),
            settled: Condvar::new(),
            capacity,
            wake: Box::new(wake),
            log: std::sync::OnceLock::new(),
        }
    }

    /// Tracks every submitted change in `log` from now on (the first log given wins).
    pub fn track_into(&self, log: std::sync::Arc<crate::changes::ChangeLog>) {
        let _ = self.log.set(log);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Hands a change to the simulation thread. Never blocks.
    pub fn submit(&self, s: Submission) -> Result<Ticket, Full> {
        let t = {
            let mut g = self.lock();
            if g.pending.len() >= self.capacity {
                // About one tick at 60 Hz drains a batch; ask for a short wait.
                return Err(Full { retry_after: Duration::from_millis(50) });
            }
            let t = Ticket(g.next);
            g.next += 1;
            g.pending.push_back((t, s));
            t
        };
        (self.wake)();
        Ok(t)
    }

    /// Simulation side: at most `max` waiting changes, oldest first.
    pub fn drain(&self, max: usize) -> Vec<(Ticket, Submission)> {
        let mut g = self.lock();
        let n = max.min(g.pending.len());
        g.pending.drain(..n).collect()
    }

    /// Simulation side: what became of a drained ticket.
    pub fn settle(&self, t: Ticket, r: Receipt) {
        if let (Receipt::Submitted { change, frame, chain, seq }, Some(log)) = (&r, self.log.get()) {
            log.track(*change, *chain, *seq, *frame);
        }
        let now = Instant::now();
        let mut g = self.lock();
        g.receipts.retain(|_, (_, at)| now.duration_since(*at) < RECEIPT_TTL);
        g.receipts.insert(t, (r, now));
        drop(g);
        self.settled.notify_all();
    }

    /// API side: the ticket's receipt, waiting until `deadline`; `None` on timeout (or if it expired untaken).
    pub fn receipt(&self, t: Ticket, deadline: Instant) -> Option<Receipt> {
        let mut g = self.lock();
        loop {
            if let Some((r, _)) = g.receipts.remove(&t) {
                return Some(r);
            }
            let now = Instant::now();
            if now >= deadline {
                return None;
            }
            g = self.settled.wait_timeout(g, deadline - now).unwrap_or_else(|e| e.into_inner()).0;
        }
    }

    /// Changes waiting for the simulation thread.
    pub fn len(&self) -> usize {
        self.lock().pending.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pe_world::edit::Actor;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn sub(label: &str) -> Submission {
        Submission {
            tx: Transaction {
                actor: Actor::Person,
                label: Some(label.into()),
                preconditions: vec![],
                ops: vec![],
                blobs: vec![],
            },
            base: 4,
        }
    }

    #[test]
    fn submit_drain_settle_receipt_across_threads() {
        let woken = Arc::new(AtomicUsize::new(0));
        let w = woken.clone();
        let q = Arc::new(EditQueue::new(8, move || {
            w.fetch_add(1, Ordering::SeqCst);
        }));
        let a = q.submit(sub("a")).unwrap();
        let b = q.submit(sub("b")).unwrap();
        assert_eq!(woken.load(Ordering::SeqCst), 2);
        // The simulation thread drains a bounded batch, oldest first, and settles later.
        let sim = {
            let q = q.clone();
            std::thread::spawn(move || {
                let got = q.drain(1);
                assert_eq!(got[0].1.tx.label.as_deref(), Some("a"));
                std::thread::sleep(Duration::from_millis(30));
                q.settle(
                    got[0].0,
                    Receipt::Submitted {
                        change: TxId([9; 32]),
                        frame: 5,
                        chain: ChainId(pe_core::Hash([1; 32])),
                        seq: 2,
                    },
                );
            })
        };
        let r = q.receipt(a, Instant::now() + Duration::from_secs(5));
        assert_eq!(
            r,
            Some(Receipt::Submitted {
                change: TxId([9; 32]),
                frame: 5,
                chain: ChainId(pe_core::Hash([1; 32])),
                seq: 2
            })
        );
        sim.join().unwrap();
        assert_eq!(q.len(), 1);
        // Not settled: the wait ends at its deadline.
        assert_eq!(q.receipt(b, Instant::now() + Duration::from_millis(20)), None);
        q.drain(10);
        q.settle(b, Receipt::Refused(SubmitError::TooLate));
        assert_eq!(q.receipt(b, Instant::now()), Some(Receipt::Refused(SubmitError::TooLate)));
        // Taken once.
        assert_eq!(q.receipt(b, Instant::now()), None);
    }

    #[test]
    fn a_full_queue_answers_with_a_retry_time() {
        let q = EditQueue::new(2, || {});
        q.submit(sub("1")).unwrap();
        q.submit(sub("2")).unwrap();
        let e = q.submit(sub("3")).unwrap_err();
        assert!(e.retry_after > Duration::ZERO);
        q.drain(1);
        assert!(q.submit(sub("3")).is_ok());
    }
}
