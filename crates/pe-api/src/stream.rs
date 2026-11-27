//! The change stream behind `watch` (spec 5.4, 9.1): a bounded, append-only window of items with cursors.
//!
//! - A cursor names the last position a reader has passed, in this process's stream (`"c:<epoch>.<seq>"`). The
//!   epoch is per stream, so a cursor from another process or an earlier run is refused, never misread.
//! - The window keeps the last `max_items` items and nothing older than `max_age`. A cursor whose next item has left
//!   the window is `Expired`, with a fresh cursor at the head; the caller re-reads by query.
//! - Items are never rewritten. A rollback is reported by pushing a new item (spec 5.4).
//! - `read` with a wait blocks the calling thread only, on a condition variable that `push` notifies. The simulation
//!   thread's `push` takes the lock for an append and a notify. A reader holds it only to copy items out (cheap
//!   clones, `Arc`s in practice, at most `SCAN` at a time) and filters them outside it.

use std::collections::VecDeque;
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

use pe_seams::world_api::Cursor;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Retention {
    pub max_items: usize,
    pub max_age: Duration,
}

impl Retention {
    /// Spec 9.1: the last 60 s or 100,000 items, whichever is less.
    pub const DEFAULT: Retention = Retention { max_items: 100_000, max_age: Duration::from_secs(60) };
}

#[derive(Debug, PartialEq, Eq)]
pub enum StreamError {
    /// Not a cursor of this stream.
    Invalid,
    /// The items after this cursor have left the window; `fresh` is a cursor at the head.
    Expired { fresh: Cursor },
}

#[derive(Debug, PartialEq)]
pub struct Read<T> {
    pub items: Vec<T>,
    pub cursor: Cursor,
    /// More matching items may wait past `limit`.
    pub more: bool,
}

struct Inner<T> {
    /// (seq, pushed at, item); seqs are consecutive, starting at 1.
    items: VecDeque<(u64, Instant, T)>,
    /// The seq of the newest item pushed (0 before any).
    head: u64,
    /// [`Stream::close`]d: a read waits for nothing.
    closed: bool,
}

/// The most items a read copies out under the lock at once.
const SCAN: usize = 4_096;

pub struct Stream<T> {
    epoch: u64,
    retention: Retention,
    inner: Mutex<Inner<T>>,
    pushed: Condvar,
}

impl<T: Clone> Stream<T> {
    pub fn new(epoch: u64, retention: Retention) -> Self {
        assert!(retention.max_items > 0);
        Stream {
            epoch,
            retention,
            inner: Mutex::new(Inner { items: VecDeque::new(), head: 0, closed: false }),
            pushed: Condvar::new(),
        }
    }

    fn cursor_at(&self, seq: u64) -> Cursor {
        Cursor(format!("c:{:x}.{seq}", self.epoch))
    }

    fn parse(&self, c: &Cursor) -> Option<u64> {
        let rest = c.0.strip_prefix("c:")?;
        let (e, s) = rest.split_once('.')?;
        if u64::from_str_radix(e, 16).ok()? != self.epoch {
            return None;
        }
        s.parse().ok()
    }

    fn evict(&self, inner: &mut Inner<T>, now: Instant) {
        while inner.items.len() > self.retention.max_items {
            inner.items.pop_front();
        }
        while let Some((_, at, _)) = inner.items.front() {
            if now.saturating_duration_since(*at) > self.retention.max_age {
                inner.items.pop_front();
            } else {
                break;
            }
        }
    }

    /// Appends one item (the simulation thread, once per change).
    pub fn push(&self, item: T, now: Instant) {
        let mut g = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        g.head += 1;
        let seq = g.head;
        g.items.push_back((seq, now, item));
        self.evict(&mut g, now);
        drop(g);
        self.pushed.notify_all();
    }

    /// Ends every read's wait now and from now on (the session ended, app.md 4.1): a read gives what is there.
    pub fn close(&self) {
        self.inner.lock().unwrap_or_else(|p| p.into_inner()).closed = true;
        self.pushed.notify_all();
    }

    /// A cursor at the head: "now".
    pub fn now_cursor(&self) -> Cursor {
        let g = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        self.cursor_at(g.head)
    }

    /// The items after `cursor` that `pick` turns into results, at most `limit` items picked. With none yet, waits
    /// up to `wait` for one. The returned cursor has passed every item scanned, picked or not. `pick` runs outside
    /// the lock, on copies taken at most `SCAN` at a time, so a reader's work never delays `push`.
    pub fn read<U>(
        &self,
        cursor: Option<&Cursor>,
        mut pick: impl FnMut(&T) -> Option<U>,
        limit: usize,
        wait: Duration,
    ) -> Result<Read<U>, StreamError> {
        let Some(c) = cursor else {
            return Ok(Read { items: vec![], cursor: self.now_cursor(), more: false });
        };
        let mut pos = self.parse(c).ok_or(StreamError::Invalid)?;
        let deadline = Instant::now() + wait;
        let limit = limit.max(1);
        let mut items = Vec::new();
        loop {
            let batch: Vec<(u64, T)> = {
                let mut g = self.inner.lock().unwrap_or_else(|p| p.into_inner());
                loop {
                    if pos > g.head {
                        return Err(StreamError::Invalid);
                    }
                    // The oldest seq still held; with an empty window, everything up to head has left it.
                    let oldest = g.items.front().map_or(g.head + 1, |(s, _, _)| *s);
                    if pos + 1 < oldest {
                        return Err(StreamError::Expired { fresh: self.cursor_at(g.head) });
                    }
                    if pos < g.head {
                        let start = (pos + 1 - oldest) as usize;
                        break g.items.iter().skip(start).take(SCAN).map(|(s, _, t)| (*s, t.clone())).collect();
                    }
                    let now = Instant::now();
                    if now >= deadline || g.closed {
                        return Ok(Read { items, cursor: self.cursor_at(pos), more: false });
                    }
                    let (ng, _) = self.pushed.wait_timeout(g, deadline - now).unwrap_or_else(|p| p.into_inner());
                    g = ng;
                }
            };
            for (seq, it) in &batch {
                if let Some(u) = pick(it) {
                    if items.len() == limit {
                        return Ok(Read { items, cursor: self.cursor_at(pos), more: true });
                    }
                    items.push(u);
                }
                pos = *seq;
            }
            if !items.is_empty() {
                return Ok(Read { items, cursor: self.cursor_at(pos), more: false });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;

    fn small() -> Stream<u32> {
        Stream::new(0xabc, Retention { max_items: 5, max_age: Duration::from_secs(60) })
    }

    /// A session's end (app.md 4.1): a read waiting out a long poll returns at once when the stream closes, and later
    /// reads don't wait.
    #[test]
    fn a_closed_stream_ends_a_waiting_read_at_once() {
        let s = Arc::new(small());
        let now = s.now_cursor();
        let (ss, c) = (s.clone(), now.clone());
        let reader = thread::spawn(move || ss.read(Some(&c), |x| Some(*x), 10, Duration::from_secs(30)));
        thread::sleep(Duration::from_millis(50));
        let asked = std::time::Instant::now();
        s.close();
        let r = reader.join().unwrap().expect("a read");
        assert!(r.items.is_empty() && asked.elapsed() < Duration::from_secs(1), "{:?}", asked.elapsed());
        let asked = std::time::Instant::now();
        assert!(s.read(Some(&now), |x| Some(*x), 10, Duration::from_secs(30)).unwrap().items.is_empty());
        assert!(asked.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn no_cursor_gives_now_and_nothing() {
        let s = small();
        s.push(1, Instant::now());
        let r = s.read(None, |x| Some(*x), 10, Duration::ZERO).unwrap();
        assert!(r.items.is_empty());
        assert_eq!(r.cursor, Cursor("c:abc.1".into()));
    }

    #[test]
    fn items_in_order_with_no_loss_or_repeat_across_cursors() {
        let s = small();
        let mut c = s.now_cursor();
        let t = Instant::now();
        let mut seen = vec![];
        for i in 0..20u32 {
            s.push(i, t);
            if i % 3 == 2 {
                let r = s.read(Some(&c), |x| Some(*x), 2, Duration::ZERO).unwrap();
                seen.extend(r.items);
                c = r.cursor;
                let r = s.read(Some(&c), |x| Some(*x), 10, Duration::ZERO).unwrap();
                seen.extend(r.items);
                c = r.cursor;
            }
        }
        assert_eq!(seen, (0..18).collect::<Vec<_>>());
    }

    #[test]
    fn a_filter_passes_unmatched_items_and_limit_sets_more() {
        let s = small();
        let c = s.now_cursor();
        let t = Instant::now();
        for i in 0..5u32 {
            s.push(i, t);
        }
        let r = s.read(Some(&c), |x| (x % 2 == 0).then_some(*x), 2, Duration::ZERO).unwrap();
        assert_eq!(r.items, vec![0, 2]);
        assert!(r.more);
        let r = s.read(Some(&r.cursor), |x| (x % 2 == 0).then_some(*x), 2, Duration::ZERO).unwrap();
        assert_eq!(r.items, vec![4]);
        assert!(!r.more);
        // Nothing more matches: the cursor still moves to the head.
        let r2 = s.read(Some(&r.cursor), |x| (x % 2 == 0).then_some(*x), 2, Duration::ZERO).unwrap();
        assert!(r2.items.is_empty());
        assert_eq!(r2.cursor, Cursor("c:abc.5".into()));
    }

    #[test]
    fn an_old_cursor_expires_with_a_fresh_one() {
        let s = small();
        let c = s.now_cursor();
        let t = Instant::now();
        for i in 0..7u32 {
            s.push(i, t); // the window keeps the last 5: items 2..=6
        }
        assert_eq!(
            s.read(Some(&c), |x| Some(*x), 10, Duration::ZERO),
            Err(StreamError::Expired { fresh: Cursor("c:abc.7".into()) })
        );
        // A cursor just before the oldest held item is still good.
        let r = s.read(Some(&Cursor("c:abc.2".into())), |x| Some(*x), 10, Duration::ZERO).unwrap();
        assert_eq!(r.items, vec![2, 3, 4, 5, 6]);
    }

    #[test]
    fn age_evicts_too() {
        let s: Stream<u32> = Stream::new(1, Retention { max_items: 100, max_age: Duration::from_secs(1) });
        let c = s.now_cursor();
        let t = Instant::now();
        s.push(1, t);
        s.push(2, t + Duration::from_secs(2)); // item 1 is now too old
        assert!(matches!(s.read(Some(&c), |x| Some(*x), 10, Duration::ZERO), Err(StreamError::Expired { .. })));
    }

    #[test]
    fn foreign_and_future_cursors_are_invalid() {
        let s = small();
        assert_eq!(s.read(Some(&Cursor("c:def.0".into())), |x| Some(*x), 1, Duration::ZERO), Err(StreamError::Invalid));
        assert_eq!(s.read(Some(&Cursor("c:abc.9".into())), |x| Some(*x), 1, Duration::ZERO), Err(StreamError::Invalid));
        assert_eq!(
            s.read(Some(&Cursor("nonsense".into())), |x| Some(*x), 1, Duration::ZERO),
            Err(StreamError::Invalid)
        );
    }

    #[test]
    fn a_long_poll_wakes_on_push_and_times_out_otherwise() {
        let s = Arc::new(small());
        let c = s.now_cursor();
        let s2 = s.clone();
        let h = thread::spawn(move || {
            thread::sleep(Duration::from_millis(50));
            s2.push(7, Instant::now());
        });
        let t0 = Instant::now();
        let r = s.read(Some(&c), |x| Some(*x), 10, Duration::from_secs(5)).unwrap();
        assert_eq!(r.items, vec![7]);
        assert!(t0.elapsed() < Duration::from_secs(4));
        h.join().unwrap();
        let t1 = Instant::now();
        let r = s.read(Some(&r.cursor), |x| Some(*x), 10, Duration::from_millis(30)).unwrap();
        assert!(r.items.is_empty());
        assert!(t1.elapsed() >= Duration::from_millis(30));
    }
}
