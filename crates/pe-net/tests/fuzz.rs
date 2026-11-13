//! Structured fuzzing of everything pe-net parses from a peer (transport.md 9, test 6: no panic on any bytes).
//!
//! Pure Rust and seeded, under the normal gate: valid encodings are generated, then mutated (bit flips, edge
//! bytes, truncation, splices, edge varints), and fed to the decoders, to the lanes' datagram parser, to a live
//! `Core` as stream records (through the `testing` hook), and to the Core as raw UDP. It is NOT coverage-guided,
//! so it finds less per CPU hour than libFuzzer would. Every case must end in an error, a fault or a close,
//! never a panic. `PE_FUZZ_ITERS=<n>` multiplies the case counts (default 1, the normal suite);
//! `PE_FUZZ_SEED=<u64>` picks another seed. A failing case prints its seed, index and bytes; keep it as a
//! regression case in `tests/fuzz_corpus/<target>.hex` (one hex input per line), which every run replays first.

use std::net::SocketAddr;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::time::Duration;

use bytes::Bytes;
use pe_net::core::{Config, Core, Lifecycle};
use pe_net::lanes::{Control, LaneParams, Link};
use pe_net::lifecycle::{Hello, Welcome, decode_rendezvous, encode_rendezvous};
use pe_net_crypto::identity::{Identity, Invite};
use pe_seams::{HostTime, Lane, MemberKey, NetEvent, PeerId, Received, Transport};

// ---------------------------------------------------------------------------------------------------------------
// The generator and the mutator.

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        // splitmix64
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: usize) -> usize {
        if n == 0 { 0 } else { (self.next() % n as u64) as usize }
    }
    fn bytes(&mut self, n: usize) -> Vec<u8> {
        (0..n).map(|_| self.next() as u8).collect()
    }
    /// Up to `max - 1` random bytes.
    fn some(&mut self, max: usize) -> Vec<u8> {
        let n = self.below(max);
        self.bytes(n)
    }
}

fn env_u64(name: &str) -> Option<u64> {
    std::env::var(name).ok().and_then(|v| v.parse().ok())
}

fn seed() -> u64 {
    env_u64("PE_FUZZ_SEED").unwrap_or(0x05ee_d0f9_e7e7)
}

fn cases(base: usize) -> usize {
    base * env_u64("PE_FUZZ_ITERS").unwrap_or(1).max(1) as usize
}

const EDGE_BYTES: [u8; 9] = [0x00, 0x01, 0x3f, 0x40, 0x7f, 0x80, 0xbf, 0xc0, 0xff];
const EDGE_VARINTS: [&[u8]; 7] = [
    &[0x00],
    &[0x3f],
    &[0x40, 0x00],
    &[0x7f, 0xff],
    &[0xbf, 0xff, 0xff, 0xff],
    &[0xc0, 0, 0, 0, 0, 0, 0, 0],
    &[0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff],
];

/// One seed, changed one to four times.
fn mutate(r: &mut Rng, seeds: &[Vec<u8>]) -> Vec<u8> {
    let mut b = seeds[r.below(seeds.len())].clone();
    for _ in 0..1 + r.below(4) {
        let at = r.below(b.len() + 1);
        match r.below(8) {
            0 if !b.is_empty() => {
                let i = r.below(b.len());
                b[i] ^= 1 << r.below(8);
            }
            1 if !b.is_empty() => {
                let i = r.below(b.len());
                b[i] = EDGE_BYTES[r.below(EDGE_BYTES.len())];
            }
            2 => b.truncate(at),
            3 => {
                let n = r.below(64);
                let extra = r.bytes(n);
                b.extend_from_slice(&extra);
            }
            4 => {
                let v = EDGE_VARINTS[r.below(EDGE_VARINTS.len())];
                b.splice(at..at, v.iter().copied());
            }
            5 if !b.is_empty() => {
                let s = r.below(b.len());
                let e = (s + 1 + r.below(32)).min(b.len());
                let piece = b[s..e].to_vec();
                b.splice(at..at, piece);
            }
            6 => {
                let other = &seeds[r.below(seeds.len())];
                let cut = r.below(other.len() + 1);
                b.truncate(at);
                b.extend_from_slice(&other[cut..]);
            }
            _ if !b.is_empty() => {
                let s = r.below(b.len());
                let e = (s + 1 + r.below(16)).min(b.len());
                let fill = r.bytes(e - s);
                b[s..e].copy_from_slice(&fill);
            }
            _ => {}
        }
    }
    b
}

fn hex(b: &[u8]) -> String {
    b.iter().take(512).map(|x| format!("{x:02x}")).collect()
}

/// The saved regression inputs for a target (`tests/fuzz_corpus/<target>.hex`).
fn corpus(name: &str) -> Vec<Vec<u8>> {
    let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fuzz_corpus").join(format!("{name}.hex"));
    let Ok(text) = std::fs::read_to_string(p) else { return Vec::new() };
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(|l| (0..l.len() / 2).map(|i| u8::from_str_radix(&l[2 * i..2 * i + 2], 16).expect("hex")).collect())
        .collect()
}

/// Run `f` on the target's saved corpus, then on `n` mutated inputs; a panic names the target, the seed, the
/// case and the bytes.
fn run(name: &str, r: &mut Rng, n: usize, seeds: &[Vec<u8>], mut f: impl FnMut(&[u8])) {
    for (i, input) in corpus(name).iter().enumerate() {
        if catch_unwind(AssertUnwindSafe(|| f(input))).is_err() {
            panic!("fuzz {name}: saved corpus entry {i} panicked on {}", hex(input));
        }
    }
    for i in 0..n {
        let input = mutate(r, seeds);
        if catch_unwind(AssertUnwindSafe(|| f(&input))).is_err() {
            panic!("fuzz {name}: case {i} with PE_FUZZ_SEED={} panicked on {}", seed(), hex(&input));
        }
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Valid encodings to start from.

fn addr(r: &mut Rng) -> SocketAddr {
    if r.below(2) == 0 {
        SocketAddr::from(([10, 0, r.below(256) as u8, r.below(256) as u8], 1 + r.below(65535) as u16))
    } else {
        let mut o = [0u8; 16];
        o.copy_from_slice(&r.bytes(16));
        SocketAddr::from((o, 1 + r.below(65535) as u16))
    }
}

fn key(r: &mut Rng) -> PeerId {
    let mut k = [0u8; 32];
    k.copy_from_slice(&r.bytes(32));
    MemberKey(k)
}

fn a_hello(r: &mut Rng) -> Hello {
    let n = r.below(4);
    Hello {
        protocol: 1,
        build: r.some(64),
        selftest: [7; 32],
        selftest_unix_s: 1_800_000_000,
        line: [9; 32],
        token: r.some(64),
        addrs: (0..n).map(|_| addr(r)).collect(),
        clock_ready: r.below(2) == 0,
    }
}

fn a_welcome(r: &mut Rng) -> Welcome {
    let n = r.below(5);
    Welcome {
        line: [9; 32],
        epoch_ns: r.next() as i64,
        rate_hz: [1, 60, 90, 1000][r.below(4)],
        members: (0..n)
            .map(|_| {
                let m = r.below(4);
                (key(r), (0..m).map(|_| addr(r)).collect())
            })
            .collect(),
        agree: r.some(200),
    }
}

fn a_rendezvous(r: &mut Rng) -> Vec<u8> {
    let n = r.below(17);
    let addrs: Vec<SocketAddr> = (0..n).map(|_| addr(r)).collect();
    encode_rendezvous(&key(r), &addrs)
}

/// A lane record as the Core sends it on a stream: kind, varint sequence, message.
fn a_lane_record(r: &mut Rng) -> Vec<u8> {
    let mut b = vec![[0u8, 1, 3, 4][r.below(4)], r.below(64) as u8];
    let n = r.below(300);
    b.extend_from_slice(&r.bytes(n));
    b
}

/// Every record kind the Core reads from a stream (3.6).
fn record_seeds(r: &mut Rng) -> Vec<Vec<u8>> {
    let mut s = Vec::new();
    for _ in 0..8 {
        s.push([&[0x80u8][..], &a_hello(r).encode()].concat());
        s.push([&[0x81u8][..], &a_welcome(r).encode()].concat());
        s.push([&[0x83u8][..], &a_rendezvous(r)].concat());
        s.push(a_lane_record(r));
    }
    s.push(vec![0x82]);
    s.push(vec![2, 0, 1, 2, 3]); // a pose on a stream
    s.push(vec![0x99]);
    s
}

// ---------------------------------------------------------------------------------------------------------------
// The decoders.

#[test]
fn fuzz_the_lifecycle_decoders() {
    let mut r = Rng(seed());
    let hellos: Vec<Vec<u8>> = (0..16).map(|_| a_hello(&mut r).encode()).collect();
    let welcomes: Vec<Vec<u8>> = (0..16).map(|_| a_welcome(&mut r).encode()).collect();
    let rdvs: Vec<Vec<u8>> = (0..16).map(|_| a_rendezvous(&mut r)).collect();
    // Generated values round-trip exactly.
    for _ in 0..cases(300) {
        let h = a_hello(&mut r);
        assert_eq!(Hello::decode(&h.encode()).ok(), Some(h));
        let w = a_welcome(&mut r);
        assert_eq!(Welcome::decode(&w.encode()).ok(), Some(w));
    }
    // Any bytes a decoder accepts are the canonical encoding of what it decoded: one message, one encoding
    // (@design-reviewer). A second accepted encoding is a finding.
    let mut ok = [0usize; 3];
    run("hello", &mut r, cases(3000), &hellos, |b| {
        if let Ok(h) = Hello::decode(b) {
            ok[0] += 1;
            assert_eq!(h.encode(), b, "a hello accepted in a non-canonical encoding: {h:?}");
        }
    });
    run("welcome", &mut r, cases(3000), &welcomes, |b| {
        if let Ok(w) = Welcome::decode(b) {
            ok[1] += 1;
            assert_eq!(w.encode(), b, "a welcome accepted in a non-canonical encoding");
        }
    });
    run("rendezvous", &mut r, cases(3000), &rdvs, |b| {
        if let Ok((k, a)) = decode_rendezvous(b) {
            ok[2] += 1;
            assert!(a.len() <= pe_net::lifecycle::MAX_ADDRS);
            assert_eq!(encode_rendezvous(&k, &a), b, "a rendezvous accepted in a non-canonical encoding");
        }
    });
    eprintln!(
        "accepted after mutation: hello {}, welcome {}, rendezvous {} of {} each",
        ok[0],
        ok[1],
        ok[2],
        cases(3000)
    );
    // Past the size bounds (MAX_HELLO, MAX_WELCOME): a valid prefix padded beyond the bound is refused.
    let mut big = hellos[0].clone();
    big.resize(pe_net::lifecycle::MAX_HELLO + 1, 0);
    assert!(Hello::decode(&big).is_err());
    let mut big = welcomes[0].clone();
    big.resize(pe_net::lifecycle::MAX_WELCOME + 1, 0);
    assert!(Welcome::decode(&big).is_err());
    // Arbitrary bytes, not only near-valid ones.
    let noise: Vec<Vec<u8>> = (0..16).map(|i| r.bytes(i * 37)).collect();
    run("lifecycle noise", &mut r, cases(3000), &noise, |b| {
        let _ = Hello::decode(b);
        let _ = Welcome::decode(b);
        let _ = decode_rendezvous(b);
    });
}

// ---------------------------------------------------------------------------------------------------------------
// The lanes' datagrams.

fn t(ms: i64) -> HostTime {
    HostTime(1_000_000_000 + ms * 1_000_000)
}

#[test]
fn fuzz_the_lane_datagrams() {
    let mut r = Rng(seed() ^ 1);
    // Real datagrams: messages on every lane, clock records, and the acks a receiver sends back.
    let mut tx = Link::new(LaneParams::default());
    tx.set_open(true, true);
    let mut rx = Link::new(LaneParams::default());
    rx.set_open(true, true);
    let mut seeds = Vec::new();
    for i in 0..40 {
        let lane = [Lane::Protocol, Lane::Action, Lane::Pose][i % 3];
        let n = r.below(900);
        let _ = tx.send(lane, Bytes::from(r.bytes(n)), 1200);
        if i % 5 == 0 {
            tx.send_control(Control::ClockPing { t1: r.next() as i64 });
            tx.send_control(Control::ClockPong { t1: r.next() as i64, t2: r.next() as i64, t3: r.next() as i64 });
        }
        while let Some(d) = tx.poll_datagram(t(i as i64), 1200, true) {
            let _ = rx.on_datagram(t(i as i64), &d);
            seeds.push(d);
        }
        while let Some(d) = rx.poll_datagram(t(i as i64 + 30), 1200, true) {
            seeds.push(d);
        }
    }
    assert!(seeds.len() > 20, "{} seed datagrams", seeds.len());
    // A fresh receiver per case, and one that lives through every case and keeps its state.
    let mut long = Link::new(LaneParams::default());
    long.set_open(true, true);
    let mut now = 0;
    let (mut good, mut msgs) = (0usize, 0usize);
    run("datagram", &mut r, cases(5000), &seeds, |b| {
        now += 1;
        let mut l = Link::new(LaneParams::default());
        l.set_open(true, true);
        if let Ok(inc) = l.on_datagram(t(now), b) {
            good += 1;
            msgs += inc.messages.len();
        }
        let _ = l.poll_datagram(t(now + 25), 1200, true);
        let _ = long.on_datagram(t(now), b);
        while long.poll_datagram(t(now), 1200, true).is_some() {}
        let _ = long.next_timeout();
    });
    eprintln!("datagrams parsed after mutation: {good} of {}, {msgs} messages delivered", cases(5000));
    // The stream path of the lanes, with any sequence number, on an open link and on a closed one.
    let mut closed = Link::new(LaneParams::default());
    closed.set_open(true, false);
    run("lane stream", &mut r, cases(2000), &seeds, |b| {
        let lane = Lane::ALL[b.first().map_or(0, |x| usize::from(*x) % 5)];
        let seq = b.iter().take(8).fold(0u64, |a, x| (a << 8) | u64::from(*x));
        let _ = long.on_stream(lane, seq, Bytes::copy_from_slice(b));
        assert!(closed.on_stream(lane, seq, Bytes::copy_from_slice(b)).is_none(), "a closed side delivered");
    });
}

/// A message payload that names its place in the sender's order: an 8-byte tag (the send count), then noise.
fn tagged(r: &mut Rng, i: usize, max: usize) -> Bytes {
    let mut b = (i as u64).to_be_bytes().to_vec();
    b.extend(r.some(max));
    Bytes::from(b)
}

fn tag_of(b: &Bytes) -> Option<u64> {
    Some(u64::from_be_bytes(b.get(..8)?.try_into().ok()?))
}

/// Real datagrams from a sender to a receiver, in order: messages on every lane and clock records, with the
/// receiver's acks interleaved (taken from the receiver as it would answer). Payloads are `tagged`.
fn a_session(r: &mut Rng, n: usize) -> (Vec<Vec<u8>>, Vec<Vec<u8>>) {
    let mut tx = Link::new(LaneParams::default());
    tx.set_open(true, true);
    let mut rx = Link::new(LaneParams::default());
    rx.set_open(true, true);
    let (mut fwd, mut back) = (Vec::new(), Vec::new());
    for i in 0..n {
        let lane = [Lane::Protocol, Lane::Action, Lane::Pose][r.below(3)];
        let _ = tx.send(lane, tagged(r, i, 600), 1200);
        if r.below(4) == 0 {
            tx.send_control(Control::ClockPing { t1: i as i64 });
        }
        while let Some(d) = tx.poll_datagram(t(i as i64), 1200, true) {
            let _ = rx.on_datagram(t(i as i64), &d);
            fwd.push(d);
        }
        while let Some(d) = rx.poll_datagram(t(i as i64 + 20), 1200, true) {
            let _ = tx.on_datagram(t(i as i64 + 20), &d);
            back.push(d);
        }
    }
    (fwd, back)
}

#[test]
fn fuzz_datagram_sequences_across_link_states() {
    // @design-reviewer: seeded sequences, not single calls. A receiver goes closed, receive-only, open, closed
    // again (the welcome exchange and a close, 6.4), and is fed a real session's datagrams replayed, reordered,
    // duplicated, dropped and mutated, while it sends and takes acks. No panic, and a closed side never delivers.
    // Under replay (@design-reviewer's second Low): each real Protocol or Action message is delivered at most
    // once; real poses only newer than the last (Pose is latest-wins; no lane is ordered, 3.2); and no ack drops
    // a message that was never transmitted (before the link first opens for sending, nothing queued may go).
    let mut r = Rng(seed() ^ 5);
    for case in 0..cases(200) {
        let n = 20 + r.below(40);
        let (fwd, back) = a_session(&mut r, n);
        let all: Vec<Vec<u8>> = fwd.iter().chain(&back).cloned().collect();
        let mut l = Link::new(LaneParams::default());
        let states = [(false, false), (false, true), (true, true), (false, false)];
        let mut now = 0i64;
        let (mut once, mut last_pose) = (std::collections::BTreeSet::new(), None);
        let res = catch_unwind(AssertUnwindSafe(|| {
            for (si, (send, recv)) in states.iter().enumerate() {
                l.set_open(*send, *recv);
                for _ in 0..10 + r.below(40) {
                    now += r.below(30) as i64;
                    let pick = r.below(6);
                    let d = match pick {
                        0 => fwd[r.below(fwd.len())].clone(),
                        1 if !back.is_empty() => back[r.below(back.len())].clone(),
                        2 => mutate(&mut r, &all),
                        _ => fwd[(now as usize / 7) % fwd.len()].clone(),
                    };
                    let queued = |l: &Link| l.queued(t(now)).0[..2].iter().sum::<u64>();
                    let before = queued(&l);
                    if let Ok(inc) = l.on_datagram(t(now), &d) {
                        assert!(*recv || inc.messages.is_empty(), "case {case}: state {si} delivered while closed");
                        // A mutated datagram may carry a forged seq, so only real ones are traced.
                        for (lane, b) in inc.messages.iter().filter(|_| pick != 2) {
                            let tag = tag_of(b).expect("a real payload is tagged");
                            if *lane == Lane::Pose {
                                assert!(last_pose.is_none_or(|p| tag > p), "case {case}: an older pose delivered");
                                last_pose = Some(tag);
                            } else {
                                assert!(once.insert(tag), "case {case}: {lane:?} message {tag} delivered twice");
                            }
                        }
                    }
                    if si < 2 {
                        assert!(queued(&l) >= before, "case {case}: an ack dropped a message never transmitted");
                    }
                    if r.below(3) == 0 {
                        let len = r.below(300);
                        let lane = [Lane::Protocol, Lane::Action, Lane::Pose][r.below(3)];
                        let _ = l.send(lane, Bytes::from(r.bytes(len)), 1200);
                    }
                    let red = r.below(2) == 0;
                    while l.poll_datagram(t(now), 1200, red).is_some() {}
                    let _ = l.next_timeout();
                }
            }
        }));
        if res.is_err() {
            panic!("fuzz sequences: case {case} with PE_FUZZ_SEED={} panicked", seed());
        }
    }
}

// ---------------------------------------------------------------------------------------------------------------
// A live Core: stream records, the hello+welcome flight, raw UDP.

struct Node {
    core: Core,
    addr: SocketAddr,
    key: PeerId,
    got: Vec<Received>,
    events: Vec<NetEvent>,
    life: Vec<Lifecycle>,
}

struct Net {
    nodes: Vec<Node>,
    now: HostTime,
    /// Every datagram sent, for the raw-UDP seeds.
    log: Vec<Vec<u8>>,
    flight: Vec<(HostTime, SocketAddr, SocketAddr, Vec<u8>)>,
}

fn hello() -> Hello {
    Hello {
        protocol: 1,
        build: b"pe 0.1".to_vec(),
        selftest: [7; 32],
        selftest_unix_s: 1_800_000_000,
        line: [9; 32],
        token: Vec::new(),
        addrs: Vec::new(),
        clock_ready: true,
    }
}

fn welcome() -> Welcome {
    Welcome { line: [9; 32], epoch_ns: 0, rate_hz: 60, members: vec![], agree: b"agree".to_vec() }
}

impl Net {
    fn new() -> Self {
        Self { nodes: Vec::new(), now: HostTime(1_000_000_000), log: Vec::new(), flight: Vec::new() }
    }

    fn add(&mut self, seed: u8, addr: &str, member: bool) -> usize {
        let id = Identity::from_seed(&[seed; 32]);
        let key = MemberKey(id.public());
        let core = Core::new(Config::new(hello(), member), id, [seed; 32]).unwrap();
        let addr = addr.parse().unwrap();
        self.nodes.push(Node { core, addr, key, got: Vec::new(), events: Vec::new(), life: Vec::new() });
        self.nodes.len() - 1
    }

    fn run(&mut self, ms: u32) {
        for _ in 0..ms {
            let now = self.now;
            for n in &mut self.nodes {
                if n.core.poll_timeout().is_some_and(|x| x <= now) {
                    n.core.handle_timeout(now);
                }
            }
            let (due, later): (Vec<_>, Vec<_>) = std::mem::take(&mut self.flight).into_iter().partition(|f| f.0 <= now);
            self.flight = later;
            for (_, from, to, bytes) in due {
                if let Some(n) = self.nodes.iter_mut().find(|n| n.addr == to) {
                    n.core.handle_datagram(now, from, &bytes);
                }
            }
            let arrive = now.plus(Duration::from_millis(5));
            for i in 0..self.nodes.len() {
                while let Some(tx) = self.nodes[i].core.poll_transmit() {
                    if self.log.len() < 4096 {
                        self.log.push(tx.bytes.clone());
                    }
                    self.flight.push((arrive, self.nodes[i].addr, tx.to, tx.bytes));
                }
            }
            for n in &mut self.nodes {
                while let Some(x) = n.core.poll() {
                    n.got.push(x);
                }
                n.events.extend(n.core.events());
                while let Some(l) = n.core.poll_lifecycle() {
                    n.life.push(l);
                }
            }
            self.now = self.now.plus(Duration::from_millis(1));
        }
    }

    fn invite(&self, i: usize) -> Invite {
        Invite { id: [5; 8], key: self.nodes[i].key.0, expiry_unix_s: u64::MAX, addrs: vec![self.nodes[i].addr] }
    }

    fn join_requests(&mut self, i: usize) -> Vec<PeerId> {
        let v = self.nodes[i]
            .events
            .iter()
            .filter_map(|e| match e {
                NetEvent::JoinRequest { peer, .. } => Some(*peer),
                _ => None,
            })
            .collect();
        self.nodes[i].events.retain(|e| !matches!(e, NetEvent::JoinRequest { .. }));
        v
    }

    fn up(&self, i: usize, p: PeerId) -> bool {
        self.nodes[i].core.peers().contains(&p)
            && self.nodes[i].events.iter().rev().find_map(|e| match e {
                NetEvent::LinkUp { peer } if *peer == p => Some(true),
                NetEvent::LinkDown { peer } if *peer == p => Some(false),
                _ => None,
            }) == Some(true)
    }
}

/// A (member) and B (joiner, a fresh identity each time), joined and up.
fn joined(seed: u8) -> (Net, usize, usize) {
    let mut net = Net::new();
    let a = net.add(1, "10.0.0.1:4433", true);
    let b = net.add(seed, "10.0.0.2:4433", false);
    let inv = net.invite(a);
    let now = net.now;
    net.nodes[b].core.dial(now, &inv);
    net.run(60);
    let now = net.now;
    for p in net.join_requests(a) {
        net.nodes[a].core.admit(now, p, &welcome());
    }
    net.run(60);
    let (ka, kb) = (net.nodes[a].key, net.nodes[b].key);
    assert!(net.up(a, kb) && net.up(b, ka), "not joined: {:?} / {:?}", net.nodes[a].life, net.nodes[b].life);
    (net, a, b)
}

#[test]
fn fuzz_stream_records_into_a_live_core() {
    // Mutated records of every kind, from the joiner to the member and back, until the receiver faults the link;
    // then a fresh pair. Whatever the bytes, the receiver delivers, ignores, faults or closes; it never panics.
    let mut r = Rng(seed() ^ 2);
    let seeds = record_seeds(&mut r);
    let total = cases(400);
    let (mut sent, mut faults, mut pairs) = (0, 0, 0);
    let mut seed_byte = 10u8;
    while sent < total {
        let (mut net, a, b) = joined(seed_byte);
        seed_byte = seed_byte.wrapping_add(1).max(10);
        pairs += 1;
        let (ka, kb) = (net.nodes[a].key, net.nodes[b].key);
        while sent < total && net.up(a, kb) && net.up(b, ka) {
            let rec = mutate(&mut r, &seeds);
            let (from, to) = if r.below(2) == 0 { (b, ka) } else { (a, kb) };
            let res = catch_unwind(AssertUnwindSafe(|| {
                net.nodes[from].core.testing_send_record(&to, rec.clone());
                net.run(15);
            }));
            if res.is_err() {
                panic!("fuzz record: case {sent} with PE_FUZZ_SEED={} panicked on {}", seed(), hex(&rec));
            }
            sent += 1;
        }
        faults += net.nodes.iter().flat_map(|n| &n.life).filter(|l| matches!(l, Lifecycle::Fault { .. })).count();
    }
    eprintln!("records: {sent} over {pairs} pairs, {faults} faults");
    assert!(pairs >= 1 && faults >= 1, "{sent} records over {pairs} pairs, {faults} faults");
}

#[test]
fn fuzz_the_hello_and_welcome_flight() {
    // @design-reviewer: a hostile joiner sends its hello and a welcome (valid or mutated) at once, with lane records
    // behind them, before the member's app has decided. The member must take in nothing and send nothing on the
    // lanes, and must not count the link as up; then its app refuses.
    let mut r = Rng(seed() ^ 3);
    let mut welcomes: Vec<Vec<u8>> = (0..8).map(|_| [&[0x81u8][..], &a_welcome(&mut r).encode()].concat()).collect();
    welcomes.push([&[0x81u8][..], &welcome().encode()].concat());
    for i in 0..cases(24) {
        let mut net = Net::new();
        let a = net.add(1, "10.0.0.1:4433", true);
        let b = net.add(40 + (i % 200) as u8, "10.0.0.2:4433", false);
        let (ka, kb) = (net.nodes[a].key, net.nodes[b].key);
        let inv = net.invite(a);
        let now = net.now;
        net.nodes[b].core.dial(now, &inv);
        // Until B has confirmed the handshake (its peers list A), then the flight at once.
        for _ in 0..200 {
            net.run(1);
            if net.nodes[b].core.peers().contains(&ka) {
                break;
            }
        }
        assert!(net.nodes[b].core.peers().contains(&ka), "B never confirmed");
        let w = if i % 3 == 0 { welcomes[welcomes.len() - 1].clone() } else { mutate(&mut r, &welcomes) };
        let lane = a_lane_record(&mut r);
        let res = catch_unwind(AssertUnwindSafe(|| {
            net.nodes[b].core.testing_send_record(&ka, w.clone());
            net.nodes[b].core.testing_send_record(&ka, lane.clone());
            net.nodes[b].core.send(ka, Lane::Action, Bytes::from_static(b"sneak"));
            net.run(300);
        }));
        if res.is_err() {
            panic!("fuzz flight: case {i} with PE_FUZZ_SEED={} panicked on {} + {}", seed(), hex(&w), hex(&lane));
        }
        assert!(net.nodes[a].got.is_empty(), "case {i}: A took in {:?}", net.nodes[a].got);
        assert!(!net.nodes[a].events.contains(&NetEvent::LinkUp { peer: kb }), "case {i}: A counted B up");
        assert!(net.nodes[b].got.is_empty(), "case {i}: B got lane traffic from A");
        let now = net.now;
        net.nodes[a].core.refuse(
            now,
            kb,
            pe_net::lifecycle::Refusal { code: pe_net::lifecycle::CloseCode::NotAdmitted, detail: "no".into() },
        );
        net.run(50);
        assert!(net.nodes[a].got.is_empty() && net.nodes[b].got.is_empty(), "case {i}: traffic after the refusal");
    }
}

#[test]
fn fuzz_raw_udp_into_a_core() {
    // Mutated QUIC datagrams from a real join, from strangers' addresses and from the peer's own, into a member that
    // is up with a joiner. quinn-proto parses them; pe-net must survive whatever it reports, and the honest link
    // must still carry a message afterwards.
    let mut r = Rng(seed() ^ 4);
    let (mut net, a, b) = joined(9);
    let seeds: Vec<Vec<u8>> = net.log.iter().filter(|d| !d.is_empty()).cloned().collect();
    assert!(seeds.len() > 4);
    let b_addr = net.nodes[b].addr;
    let mut i = 0;
    run("udp", &mut r, cases(1500), &seeds, |d| {
        i += 1;
        let from = if i % 4 == 0 { b_addr } else { SocketAddr::from(([192, 0, 2, (i % 250) as u8], 4433)) };
        let now = net.now;
        net.nodes[a].core.handle_datagram(now, from, d);
        if i % 50 == 0 {
            net.run(5);
        }
    });
    net.run(200);
    let (ka, kb) = (net.nodes[a].key, net.nodes[b].key);
    net.nodes[a].got.clear();
    net.nodes[b].core.send(ka, Lane::Action, Bytes::from_static(b"still"));
    net.run(200);
    let survived = net.nodes[a].got.iter().any(|x| x.from == kb && x.bytes == Bytes::from_static(b"still"));
    // Forged packets from B's own address may legitimately kill the connection (a stateless reset, a close); then
    // the member must have reported it rather than hung.
    let reported = net.nodes[a].events.contains(&NetEvent::LinkDown { peer: kb })
        || net.nodes[a].life.iter().any(|l| matches!(l, Lifecycle::Fault { .. } | Lifecycle::Refused { .. }));
    assert!(survived || reported, "the link neither carried a message nor was reported down");
}
