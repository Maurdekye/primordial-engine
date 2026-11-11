//! The deterministic network model (DESIGN.md 3.1), the harness's implementation of S4's delivery promises.
//!
//! - **Directed links.** Each has a base delay, uniform jitter, loss (Bernoulli or Gilbert-Elliott bursts),
//!   duplication, and an optional bottleneck: a serialising rate with a drop-tail queue of bounded bytes.
//! - **Lanes** are S4's (`pe_seams::Lane`): protocol, action, pose, repair and bulk, in priority order. The bottleneck
//!   sends the highest-priority queued message first. A new pose supersedes an unsent one on the same link, and is
//!   never queued behind it.
//! - **Reliability follows S4.** Every lane but Pose is "reliable, each once, any order". A reliable message lost to
//!   loss, a partition or a full queue is retransmitted after `rto_ns`, with fresh draws, until it arrives exactly
//!   once or `give_up_ns` passes, as a QUIC connection would time out. Duplication applies to Pose only. Pose is
//!   lossy and newest-only. [M0 simplification: a retransmission does not pass through the bottleneck again.]
//! - **Partitions:** while one is active, a message that leaves its sender across two groups is lost (and, on a
//!   reliable lane, retransmitted later).
//! - **Randomness:** each draw is `rng::draw(seed, link, message index and attempt, purpose)`. Reordering comes from
//!   jitter.
//!
//! Nothing here is shown to a node. A node learns only what arrives. `LinkTruth` exists for the harness's own checks
//! and reports, and must never feed code under test (no oracle: file 01, defect 10).

use std::collections::{BTreeMap, VecDeque};

pub use pe_seams::Lane;

use crate::clock::{EventQueue, MS, Nanos, SEC};
use crate::rng::{Purpose, below, chance, draw};

pub type NodeId = u32;

fn lane_index(l: Lane) -> usize {
    l as usize
}

/// Whether S4 promises delivery on this lane.
pub fn reliable(l: Lane) -> bool {
    l != Lane::Pose
}

/// How a link loses messages. Rates are parts per million.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Loss {
    #[default]
    None,
    Bernoulli {
        ppm: u32,
    },
    /// A two-state burst model. The state moves once per transmission, before the loss draw.
    GilbertElliott {
        good_to_bad_ppm: u32,
        bad_to_good_ppm: u32,
        loss_good_ppm: u32,
        loss_bad_ppm: u32,
    },
}

/// A serialising bottleneck with a drop-tail queue.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Bottleneck {
    pub rate_bps: u64,
    pub queue_bytes: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LinkCfg {
    pub delay_ns: Nanos,
    /// Uniform in `0..=jitter_ns`.
    pub jitter_ns: Nanos,
    pub loss: Loss,
    /// Pose only: reliable lanes deliver each message once.
    pub duplicate_ppm: u32,
    pub bottleneck: Option<Bottleneck>,
    /// Bytes added to each message on the wire, for the bottleneck's timing and queue.
    pub overhead_bytes: u64,
    /// A reliable message lost in transit is sent again this long after it left.
    pub rto_ns: Nanos,
    /// A reliable message still undelivered this long after it was offered is given up, as a connection times out.
    pub give_up_ns: Nanos,
}

impl Default for LinkCfg {
    fn default() -> Self {
        Self {
            delay_ns: 30 * MS,
            jitter_ns: 0,
            loss: Loss::None,
            duplicate_ppm: 0,
            bottleneck: None,
            overhead_bytes: 0,
            rto_ns: 200 * MS,
            give_up_ns: 30 * SEC,
        }
    }
}

/// A partition: while `from <= t < until`, messages between different groups are lost. Nodes in no group are cut
/// off from everyone.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Partition {
    pub groups: Vec<Vec<NodeId>>,
    pub from: Nanos,
    pub until: Nanos,
}

impl Partition {
    fn separates(&self, t: Nanos, a: NodeId, b: NodeId) -> bool {
        if t < self.from || t >= self.until {
            return false;
        }
        let group = |n| self.groups.iter().position(|g| g.contains(&n));
        match (group(a), group(b)) {
            (Some(x), Some(y)) => x != y,
            _ => true,
        }
    }
}

/// A message as it arrives.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Delivery {
    pub at: Nanos,
    pub from: NodeId,
    pub to: NodeId,
    pub lane: Lane,
    pub bytes: Vec<u8>,
    /// The message's index on its link, for traces. A duplicate carries the original's index.
    pub index: u64,
}

/// What happened on one link. For the harness only, never for a node.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LinkTruth {
    pub offered: u64,
    pub delivered: u64,
    /// Transmissions lost to loss (a reliable message may be lost several times before it arrives).
    pub lost: u64,
    pub lost_partition: u64,
    pub dropped_queue: u64,
    pub superseded: u64,
    pub duplicated: u64,
    pub retransmitted: u64,
    /// Reliable messages given up after `give_up_ns`.
    pub given_up: u64,
}

#[derive(Clone, Debug)]
struct Queued {
    lane: Lane,
    bytes: Vec<u8>,
    index: u64,
    size: u64,
    offered_at: Nanos,
    attempt: u64,
}

#[derive(Debug)]
struct LinkState {
    cfg: LinkCfg,
    sent: u64,
    queues: [VecDeque<Queued>; 5],
    queued_bytes: u64,
    in_flight: Option<Queued>,
    burst_bad: bool,
    truth: LinkTruth,
}

impl LinkState {
    fn new(cfg: LinkCfg) -> Self {
        Self {
            cfg,
            sent: 0,
            queues: Default::default(),
            queued_bytes: 0,
            in_flight: None,
            burst_bad: false,
            truth: LinkTruth::default(),
        }
    }
}

#[derive(Debug)]
enum Ev {
    TxDone { from: NodeId, to: NodeId },
    Retry { from: NodeId, to: NodeId, m: Queued },
    Arrive(Delivery),
}

/// The network: every directed link, its in-flight messages and the partitions.
#[derive(Debug)]
pub struct Net {
    seed: u64,
    default: LinkCfg,
    links: BTreeMap<(NodeId, NodeId), LinkState>,
    events: EventQueue<Ev>,
    partitions: Vec<Partition>,
}

const fn stream(from: NodeId, to: NodeId) -> u64 {
    ((from as u64) << 32) | to as u64
}

/// Nanoseconds to serialise `bytes` at `rate_bps`, rounded up.
fn tx_time(bytes: u64, rate_bps: u64) -> Nanos {
    let bits = bytes as u128 * 8 * SEC as u128;
    bits.div_ceil(rate_bps.max(1) as u128) as Nanos
}

impl Net {
    pub fn new(seed: u64, default: LinkCfg) -> Self {
        Self { seed, default, links: BTreeMap::new(), events: EventQueue::new(), partitions: Vec::new() }
    }

    /// Sets one directed link's configuration. Messages already queued keep their place.
    pub fn set_link(&mut self, from: NodeId, to: NodeId, cfg: LinkCfg) {
        match self.links.get_mut(&(from, to)) {
            Some(l) => l.cfg = cfg,
            None => {
                self.links.insert((from, to), LinkState::new(cfg));
            }
        }
    }

    pub fn add_partition(&mut self, p: Partition) {
        self.partitions.push(p);
    }

    /// The harness's record of one link. Never pass this to code under test.
    pub fn truth(&self, from: NodeId, to: NodeId) -> LinkTruth {
        self.links.get(&(from, to)).map(|l| l.truth.clone()).unwrap_or_default()
    }

    fn link(&mut self, from: NodeId, to: NodeId) -> &mut LinkState {
        let default = &self.default;
        self.links.entry((from, to)).or_insert_with(|| LinkState::new(default.clone()))
    }

    /// Offers a message to the link at `now`.
    pub fn send(&mut self, now: Nanos, from: NodeId, to: NodeId, lane: Lane, bytes: Vec<u8>) {
        let link = self.link(from, to);
        let index = link.sent;
        link.sent += 1;
        link.truth.offered += 1;
        let size = bytes.len() as u64 + link.cfg.overhead_bytes;
        let m = Queued { lane, bytes, index, size, offered_at: now, attempt: 0 };
        let Some(b) = link.cfg.bottleneck.clone() else {
            self.depart(now, from, to, m);
            return;
        };
        if lane == Lane::Pose {
            let q = &mut link.queues[lane_index(Lane::Pose)];
            let before = q.len();
            let freed: u64 = q.iter().map(|m| m.size).sum();
            q.clear();
            link.queued_bytes -= freed;
            link.truth.superseded += before as u64;
        }
        if link.queued_bytes + size > b.queue_bytes {
            link.truth.dropped_queue += 1;
            self.retry_or_drop(now, from, to, m);
            return;
        }
        link.queued_bytes += size;
        link.queues[lane_index(lane)].push_back(m);
        if link.in_flight.is_none() {
            self.start_tx(now, from, to);
        }
    }

    fn start_tx(&mut self, now: Nanos, from: NodeId, to: NodeId) {
        let link = self.link(from, to);
        let Some(rate) = link.cfg.bottleneck.as_ref().map(|b| b.rate_bps) else { return };
        let Some(m) = link.queues.iter_mut().find_map(|q| q.pop_front()) else { return };
        link.queued_bytes -= m.size;
        let done = now + tx_time(m.size, rate);
        link.in_flight = Some(m);
        self.events.push(done, Ev::TxDone { from, to });
    }

    /// A lost transmission: a reliable message is sent again after the link's `rto_ns`, until its `give_up_ns`.
    fn retry_or_drop(&mut self, t: Nanos, from: NodeId, to: NodeId, mut m: Queued) {
        if !reliable(m.lane) {
            return;
        }
        let link = self.link(from, to);
        let at = t + link.cfg.rto_ns;
        if at > m.offered_at + link.cfg.give_up_ns {
            link.truth.given_up += 1;
            return;
        }
        link.truth.retransmitted += 1;
        m.attempt += 1;
        self.events.push(at, Ev::Retry { from, to, m });
    }

    /// A message leaves the sender: partitions, loss, jitter and duplication are decided here.
    fn depart(&mut self, t: Nanos, from: NodeId, to: NodeId, m: Queued) {
        let seed = self.seed;
        let cut = self.partitions.iter().any(|p| p.separates(t, from, to));
        let link = self.link(from, to);
        if cut {
            link.truth.lost_partition += 1;
            self.retry_or_drop(t, from, to, m);
            return;
        }
        let s = stream(from, to);
        // A retransmission draws afresh: the attempt number sits in the index's top bits.
        let key = m.index | (m.attempt << 48);
        let lost = match link.cfg.loss {
            Loss::None => false,
            Loss::Bernoulli { ppm } => chance(draw(seed, s, key, Purpose::Loss), ppm),
            Loss::GilbertElliott { good_to_bad_ppm, bad_to_good_ppm, loss_good_ppm, loss_bad_ppm } => {
                let flip = if link.burst_bad { bad_to_good_ppm } else { good_to_bad_ppm };
                if chance(draw(seed, s, key, Purpose::BurstState), flip) {
                    link.burst_bad = !link.burst_bad;
                }
                let ppm = if link.burst_bad { loss_bad_ppm } else { loss_good_ppm };
                chance(draw(seed, s, key, Purpose::Loss), ppm)
            }
        };
        if lost {
            link.truth.lost += 1;
            self.retry_or_drop(t, from, to, m);
            return;
        }
        let jitter = |p| below(draw(seed, s, key, p), link.cfg.jitter_ns + 1);
        let at = t + link.cfg.delay_ns + jitter(Purpose::Jitter);
        let dup = !reliable(m.lane) && chance(draw(seed, s, key, Purpose::Duplicate), link.cfg.duplicate_ppm);
        let dup_at = t + link.cfg.delay_ns + jitter(Purpose::DuplicateJitter);
        if dup {
            link.truth.duplicated += 1;
        }
        let d = Delivery { at, from, to, lane: m.lane, bytes: m.bytes, index: m.index };
        if dup {
            self.events.push(dup_at, Ev::Arrive(Delivery { at: dup_at, ..d.clone() }));
        }
        self.events.push(at, Ev::Arrive(d));
    }

    /// The time of the network's next internal event, if any.
    pub fn next_time(&self) -> Option<Nanos> {
        self.events.peek_time()
    }

    /// Runs the network up to and including `until`, and returns what arrived, in arrival order.
    pub fn advance(&mut self, until: Nanos) -> Vec<Delivery> {
        let mut out = Vec::new();
        while let Some((t, ev)) = self.events.pop_due(until) {
            match ev {
                Ev::TxDone { from, to } => {
                    let link = self.link(from, to);
                    if let Some(m) = link.in_flight.take() {
                        self.depart(t, from, to, m);
                    }
                    self.start_tx(t, from, to);
                }
                Ev::Retry { from, to, m } => self.depart(t, from, to, m),
                Ev::Arrive(d) => {
                    self.link(d.from, d.to).truth.delivered += 1;
                    out.push(d);
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(net: &mut Net) -> Vec<Delivery> {
        net.advance(u64::MAX)
    }

    #[test]
    fn delay_only() {
        let mut net = Net::new(1, LinkCfg::default());
        net.send(0, 1, 2, Lane::Action, vec![1]);
        net.send(5 * MS, 1, 2, Lane::Action, vec![2]);
        let d = run(&mut net);
        assert_eq!(d.iter().map(|d| (d.at, d.bytes[0])).collect::<Vec<_>>(), vec![(30 * MS, 1), (35 * MS, 2)]);
        assert_eq!(net.truth(1, 2).delivered, 2);
    }

    #[test]
    fn same_seed_same_run_and_links_are_independent() {
        let cfg = LinkCfg {
            jitter_ns: 10 * MS,
            loss: Loss::Bernoulli { ppm: 100_000 },
            duplicate_ppm: 50_000,
            ..Default::default()
        };
        let go = |extra_on_other_link: bool| {
            let mut net = Net::new(42, cfg.clone());
            for i in 0..500u64 {
                net.send(i * MS, 1, 2, Lane::Pose, i.to_le_bytes().to_vec());
                net.send(i * MS, 1, 2, Lane::Action, i.to_le_bytes().to_vec());
                if extra_on_other_link {
                    net.send(i * MS, 1, 3, Lane::Action, vec![0]);
                    net.send(i * MS, 3, 2, Lane::Pose, vec![0]);
                }
            }
            run(&mut net).into_iter().filter(|d| d.from == 1 && d.to == 2).collect::<Vec<_>>()
        };
        let a = go(false);
        assert_eq!(a, go(false), "same seed, same run");
        assert_eq!(a, go(true), "traffic on other links moves nothing on this one");
        let poses = a.iter().filter(|d| d.lane == Lane::Pose).count();
        assert_ne!(poses, 500, "some poses were lost or duplicated");
    }

    #[test]
    fn reliable_lanes_deliver_each_message_exactly_once_under_loss() {
        let cfg = LinkCfg {
            jitter_ns: 5 * MS,
            loss: Loss::Bernoulli { ppm: 300_000 },
            duplicate_ppm: 500_000,
            ..Default::default()
        };
        let mut net = Net::new(8, cfg);
        for i in 0..300u64 {
            for lane in [Lane::Protocol, Lane::Action, Lane::Repair, Lane::Bulk] {
                net.send(i * MS, 1, 2, lane, i.to_le_bytes().to_vec());
            }
        }
        let d = run(&mut net);
        for lane in [Lane::Protocol, Lane::Action, Lane::Repair, Lane::Bulk] {
            let mut idx: Vec<u64> = d.iter().filter(|x| x.lane == lane).map(|x| x.index).collect();
            idx.sort_unstable();
            let n = idx.len();
            idx.dedup();
            assert_eq!((n, idx.len()), (300, 300), "{lane:?}: every message once");
        }
        let t = net.truth(1, 2);
        assert!(t.retransmitted > 0 && t.lost == t.retransmitted, "{t:?}");
        assert_eq!(t.duplicated, 0, "no duplicates on reliable lanes");
    }

    #[test]
    fn a_retransmitted_message_arrives_late_by_its_timeouts() {
        // Loss of every first attempt: each message arrives one rto later.
        let cfg = LinkCfg {
            loss: Loss::GilbertElliott {
                good_to_bad_ppm: 1_000_000,
                bad_to_good_ppm: 1_000_000,
                loss_good_ppm: 0,
                loss_bad_ppm: 1_000_000,
            },
            ..Default::default()
        };
        let mut net = Net::new(2, cfg);
        net.send(0, 1, 2, Lane::Action, vec![1]);
        let d = run(&mut net);
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].at, 200 * MS + 30 * MS);
    }

    #[test]
    fn jitter_reorders_within_its_bound() {
        let cfg = LinkCfg { delay_ns: 20 * MS, jitter_ns: 15 * MS, ..Default::default() };
        let mut net = Net::new(3, cfg);
        for i in 0..200u64 {
            net.send(i * MS, 1, 2, Lane::Action, vec![i as u8]);
        }
        let d = run(&mut net);
        assert_eq!(d.len(), 200);
        let mut reordered = false;
        for w in d.windows(2) {
            reordered |= w[1].index < w[0].index;
        }
        assert!(reordered, "15 ms of jitter on 1 ms spacing reorders");
        for x in &d {
            let sent = x.index * MS;
            assert!(x.at >= sent + 20 * MS && x.at <= sent + 35 * MS);
        }
    }

    #[test]
    fn bottleneck_serialises_and_prioritises_lanes() {
        // 8,000 bit/s: a 100-byte message takes 100 ms.
        let cfg = LinkCfg {
            delay_ns: 0,
            bottleneck: Some(Bottleneck { rate_bps: 8_000, queue_bytes: 10_000 }),
            ..Default::default()
        };
        let mut net = Net::new(1, cfg);
        net.send(0, 1, 2, Lane::Bulk, vec![0; 100]); // starts at once
        net.send(0, 1, 2, Lane::Bulk, vec![1; 100]);
        net.send(0, 1, 2, Lane::Protocol, vec![2; 100]); // jumps the queued bulk message
        let d = run(&mut net);
        let order: Vec<_> = d.iter().map(|d| (d.at / MS, d.lane)).collect();
        assert_eq!(order, vec![(100, Lane::Bulk), (200, Lane::Protocol), (300, Lane::Bulk)]);
    }

    #[test]
    fn a_new_pose_supersedes_an_unsent_one() {
        let cfg = LinkCfg {
            delay_ns: 0,
            bottleneck: Some(Bottleneck { rate_bps: 8_000, queue_bytes: 10_000 }),
            ..Default::default()
        };
        let mut net = Net::new(1, cfg);
        net.send(0, 1, 2, Lane::Bulk, vec![0; 100]); // occupies the link for 100 ms
        for i in 1..=5u8 {
            net.send(u64::from(i) * MS, 1, 2, Lane::Pose, vec![i; 10]);
        }
        let d = run(&mut net);
        let poses: Vec<_> = d.iter().filter(|d| d.lane == Lane::Pose).map(|d| d.bytes[0]).collect();
        assert_eq!(poses, vec![5], "only the newest pose is sent");
        assert_eq!(net.truth(1, 2).superseded, 4);
    }

    #[test]
    fn a_full_queue_drops_poses_and_delays_reliable_messages() {
        let cfg = LinkCfg {
            delay_ns: 0,
            bottleneck: Some(Bottleneck { rate_bps: 8_000, queue_bytes: 250 }),
            ..Default::default()
        };
        let mut net = Net::new(1, cfg);
        for _ in 0..5 {
            net.send(0, 1, 2, Lane::Bulk, vec![0; 100]);
        }
        // One is in flight at once and two more fit in 250 queued bytes; the other two are retried and arrive.
        assert_eq!(run(&mut net).len(), 5);
        assert_eq!(net.truth(1, 2).dropped_queue, 2);
        let mut net = Net::new(
            1,
            LinkCfg {
                delay_ns: 0,
                bottleneck: Some(Bottleneck { rate_bps: 8_000, queue_bytes: 50 }),
                ..Default::default()
            },
        );
        net.send(0, 1, 2, Lane::Bulk, vec![0; 100]); // does not fit: retried, and arrives
        net.send(0, 1, 2, Lane::Pose, vec![0; 100]); // does not fit: a pose is lost for good
        assert_eq!(run(&mut net).len(), 1);
    }

    #[test]
    fn partitions_lose_poses_and_delay_reliable_messages_until_they_heal() {
        let mut net = Net::new(1, LinkCfg::default());
        net.add_partition(Partition { groups: vec![vec![1, 2], vec![3]], from: 100 * MS, until: 500 * MS });
        net.send(50 * MS, 1, 3, Lane::Action, vec![1]); // before
        net.send(150 * MS, 1, 3, Lane::Action, vec![2]); // across, during: retried until it heals
        net.send(150 * MS, 1, 3, Lane::Pose, vec![3]); // across, during: lost
        net.send(150 * MS, 1, 2, Lane::Action, vec![4]); // same group, during
        let got: Vec<_> = run(&mut net).iter().map(|d| (d.bytes[0], d.at / MS)).collect();
        assert_eq!(got, vec![(1, 80), (4, 180), (2, 580)]);
        assert_eq!(net.truth(1, 3).lost_partition, 3, "the action twice, the pose once");
    }

    #[test]
    fn a_reliable_message_is_given_up_after_its_timeout() {
        let mut net = Net::new(1, LinkCfg { give_up_ns: SEC, ..Default::default() });
        net.add_partition(Partition { groups: vec![vec![1], vec![2]], from: 0, until: 10 * SEC });
        net.send(0, 1, 2, Lane::Action, vec![1]);
        assert!(run(&mut net).is_empty());
        assert_eq!(net.truth(1, 2).given_up, 1);
    }

    #[test]
    fn gilbert_elliott_losses_come_in_bursts() {
        let n = 100_000u64;
        let ge = Loss::GilbertElliott {
            good_to_bad_ppm: 2_000,
            bad_to_good_ppm: 200_000,
            loss_good_ppm: 0,
            loss_bad_ppm: 1_000_000,
        };
        let lost_runs = |loss: Loss| {
            let mut net = Net::new(5, LinkCfg { delay_ns: 0, loss, ..Default::default() });
            for i in 0..n {
                net.send(i, 1, 2, Lane::Pose, vec![]);
            }
            let got: std::collections::BTreeSet<u64> = run(&mut net).iter().map(|d| d.index).collect();
            let lost = n - got.len() as u64;
            let runs = (0..n).filter(|i| !got.contains(i) && (*i == 0 || got.contains(&(i - 1)))).count() as u64;
            (lost, runs)
        };
        let (lost, runs) = lost_runs(ge);
        // Stationary bad share is 2,000 / 202,000, about 1%; bursts average 5 messages.
        assert!((600..=1_400).contains(&lost), "lost {lost}");
        assert!(lost as f64 / runs as f64 > 3.0, "mean burst {}", lost as f64 / runs as f64);
        let (blost, bruns) = lost_runs(Loss::Bernoulli { ppm: 10_000 });
        assert!((blost as f64 / bruns as f64) < 1.2, "Bernoulli losses are mostly single");
    }

    #[test]
    fn duplicates_arrive_twice_on_the_pose_lane_only() {
        let mut net = Net::new(9, LinkCfg { duplicate_ppm: 1_000_000, jitter_ns: 5 * MS, ..Default::default() });
        net.send(0, 1, 2, Lane::Pose, vec![7]);
        net.send(0, 1, 2, Lane::Action, vec![8]);
        let d = run(&mut net);
        assert_eq!(d.iter().filter(|x| x.lane == Lane::Pose).count(), 2);
        assert_eq!(d.iter().filter(|x| x.lane == Lane::Action).count(), 1);
        assert_eq!(net.truth(1, 2).duplicated, 1);
    }

    #[test]
    fn tx_time_rounds_up() {
        assert_eq!(tx_time(1, 3), 2_666_666_667);
        assert_eq!(tx_time(125, 1_000_000), MS);
    }
}
