//! pe-net's Core between in-memory peers (transport.md 6, 9.1 and 9.4): joining from an invite, the guards,
//! refusals, a wrong key, staggered dials, two dials at once, and every lane both ways once the link is up.

use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;

use bytes::Bytes;
use pe_net::core::{Config, Core, CoreError, Lifecycle, RedialParams};
use pe_net::lifecycle::{CloseCode, Hello, HelloError, MAX_BUILD, Refusal, Welcome, encode_rendezvous};
use pe_net_crypto::identity::{Identity, Invite, Reply};
use pe_seams::{FrameClock, HostTime, Lane, LinkState, NetEvent, PeerId, Received, SimRate, Transport};

fn hello(build: &str) -> Hello {
    Hello {
        protocol: 1,
        build: build.as_bytes().to_vec(),
        selftest: [7; 32],
        selftest_unix_s: 1_800_000_000,
        line: [9; 32],
        token: Vec::new(),
        addrs: Vec::new(),
        clock_ready: true,
    }
}

/// A build id over the hello's bound is refused when the Core is built, with a typed error. Sent, every peer would
/// drop the hello as malformed and the connection would only time out (@iter-bench's finding).
#[test]
fn a_build_id_over_the_hello_s_bound_is_refused_when_the_core_is_built() {
    let core = |build: &str| Core::new(Config::new(hello(build), true), Identity::from_seed(&[9; 32]), [9; 32]);
    let refused = core(&"x".repeat(MAX_BUILD + 1));
    assert!(
        matches!(
            refused,
            Err(CoreError::Hello(HelloError::TooLong { field: "build id", len, max })) if len == MAX_BUILD + 1 && max == MAX_BUILD
        ),
        "{:?}",
        refused.err()
    );
    assert!(core(&"x".repeat(MAX_BUILD)).is_ok(), "a build id at the bound is fine");
}

struct Node {
    core: Core,
    /// This node's host clock minus the network's time, ns.
    skew: i64,
    addr: SocketAddr,
    key: PeerId,
    got: Vec<Received>,
    events: Vec<NetEvent>,
    life: Vec<Lifecycle>,
}

/// Extra delay, in milliseconds, for a packet: (now, from, to, bytes).
type Extra = Box<dyn Fn(HostTime, SocketAddr, SocketAddr, &[u8]) -> i64>;

struct Net {
    nodes: Vec<Node>,
    now: HostTime,
    /// Addresses that swallow everything sent to them, and the bytes they swallowed.
    dead: Vec<SocketAddr>,
    swallowed: usize,
    /// One-way delay, in milliseconds, and the packets on their way: (arrival, from, to, bytes).
    delay_ms: i64,
    flight: Vec<(HostTime, SocketAddr, SocketAddr, Vec<u8>)>,
    /// Packets from an address held until a time, so they arrive in one flight.
    hold: Vec<(SocketAddr, HostTime)>,
    /// Extra delay, in milliseconds, for a packet: (now, from, to, bytes).
    extra: Option<Extra>,
    /// Packets sent to an address no node has: (from, to).
    stray: Vec<(SocketAddr, SocketAddr)>,
}

impl Net {
    fn new() -> Self {
        Self {
            nodes: Vec::new(),
            now: HostTime(1_000_000_000),
            dead: Vec::new(),
            swallowed: 0,
            delay_ms: 10,
            flight: Vec::new(),
            hold: Vec::new(),
            extra: None,
            stray: Vec::new(),
        }
    }

    fn add(&mut self, seed: u8, addr: &str, member: bool, build: &str) -> usize {
        self.add_with(seed, addr, member, build, |_| {})
    }

    fn add_with(&mut self, seed: u8, addr: &str, member: bool, build: &str, tweak: impl Fn(&mut Config)) -> usize {
        let id = Identity::from_seed(&[seed; 32]);
        let key = pe_seams::MemberKey(id.public());
        let mut cfg = Config::new(hello(build), member);
        tweak(&mut cfg);
        let core = Core::new(cfg, id, [seed; 32]).unwrap();
        self.nodes.push(Node {
            core,
            skew: 0,
            addr: addr.parse().unwrap(),
            key,
            got: Vec::new(),
            events: Vec::new(),
            life: Vec::new(),
        });
        self.nodes.len() - 1
    }

    fn invite(&self, i: usize) -> Invite {
        Invite {
            id: [5; 8],
            key: self.nodes[i].key.0,
            expiry_unix_s: u64::MAX,
            addrs: vec![self.nodes[i].addr],
            pin: None,
        }
    }

    /// Step `ms` milliseconds, 1 ms at a time; packets arrive `delay_ms` after they leave.
    fn run(&mut self, ms: u32) {
        for _ in 0..ms {
            let now = self.now;
            for n in &mut self.nodes {
                let local = now.plus_nanos(n.skew);
                if n.core.poll_timeout().is_some_and(|t| t <= local) {
                    n.core.handle_timeout(local);
                }
            }
            let (due, later): (Vec<_>, Vec<_>) = std::mem::take(&mut self.flight).into_iter().partition(|f| f.0 <= now);
            self.flight = later;
            for (_, from, to, bytes) in due {
                if let Some(j) = self.nodes.iter().position(|n| n.addr == to) {
                    let local = now.plus_nanos(self.nodes[j].skew);
                    self.nodes[j].core.handle_datagram(local, from, &bytes);
                } else {
                    self.stray.push((from, to));
                }
            }
            let arrive = now.plus_nanos(self.delay_ms * 1_000_000);
            for i in 0..self.nodes.len() {
                while let Some(tx) = self.nodes[i].core.poll_transmit() {
                    if self.dead.contains(&tx.to) {
                        self.swallowed += tx.bytes.len();
                    } else {
                        let from = self.nodes[i].addr;
                        let mut at = self.hold.iter().filter(|h| h.0 == from).map(|h| h.1).fold(arrive, HostTime::max);
                        if let Some(f) = &self.extra {
                            at = at.plus_nanos(f(now, from, tx.to, &tx.bytes) * 1_000_000);
                        }
                        self.flight.push((at, from, tx.to, tx.bytes));
                    }
                }
            }
            for n in &mut self.nodes {
                while let Some(r) = n.core.poll() {
                    n.got.push(r);
                }
                n.events.extend(n.core.events());
                while let Some(l) = n.core.poll_lifecycle() {
                    n.life.push(l);
                }
            }
            self.now = self.now.plus_nanos(1_000_000);
        }
    }

    /// Admit every pending join request with a welcome naming the line of `hello()`.
    fn admit_all(&mut self, i: usize) {
        let now = self.now;
        let reqs: Vec<PeerId> = self.nodes[i]
            .events
            .iter()
            .filter_map(|e| match e {
                NetEvent::JoinRequest { peer, .. } => Some(*peer),
                _ => None,
            })
            .collect();
        for p in reqs {
            let w = Welcome { line: [9; 32], epoch_ns: 0, rate_hz: 60, members: vec![], agree: b"agree".to_vec() };
            self.nodes[i].core.admit(now, p, &w);
        }
        self.nodes[i].events.retain(|e| !matches!(e, NetEvent::JoinRequest { .. }));
    }

    fn up(&self, i: usize, peer: PeerId) -> bool {
        self.nodes[i].events.contains(&NetEvent::LinkUp { peer })
    }
}

fn refused(n: &Node) -> Option<&Refusal> {
    n.life.iter().find_map(|l| match l {
        Lifecycle::Refused { refusal, .. } => Some(refusal),
        _ => None,
    })
}

/// A (member) and B (joiner), with B joined from A's invite.
fn joined() -> (Net, usize, usize) {
    joined_with(|_| {})
}

/// As [`joined`], with both configs tweaked.
fn joined_with(tweak: impl Fn(&mut Config)) -> (Net, usize, usize) {
    let mut net = Net::new();
    let a = net.add_with(1, "10.0.0.1:4433", true, "pe 0.1", &tweak);
    let b = net.add_with(2, "10.0.0.2:4433", false, "pe 0.1", &tweak);
    let inv = net.invite(a);
    let now = net.now;
    net.nodes[b].core.dial(now, &inv);
    net.run(100);
    assert!(net.nodes[a].events.iter().any(|e| matches!(e, NetEvent::JoinRequest { .. })), "no join request");
    net.admit_all(a);
    net.run(100);
    let (ka, kb) = (net.nodes[a].key, net.nodes[b].key);
    assert!(net.up(a, kb) && net.up(b, ka), "link not up: {:?} / {:?}", net.nodes[a].events, net.nodes[b].events);
    assert!(
        matches!(net.nodes[b].life.first(), Some(Lifecycle::Welcomed { welcome, .. }) if welcome.agree == b"agree")
    );
    (net, a, b)
}

#[test]
fn a_joiner_is_welcomed_and_every_lane_works_both_ways() {
    let (mut net, a, b) = joined();
    let (ka, kb) = (net.nodes[a].key, net.nodes[b].key);
    let msgs: Vec<(Lane, Bytes)> = vec![
        (Lane::Protocol, Bytes::from_static(b"vote")),
        (Lane::Action, Bytes::from(vec![1u8; 30])),
        (Lane::Pose, Bytes::from_static(b"pose")),
        (Lane::Protocol, Bytes::from(vec![2u8; 2_000])), // over half a datagram: the stream path
        (Lane::Repair, Bytes::from(vec![3u8; 100_000])),
        (Lane::Bulk, Bytes::from(vec![4u8; 300_000])),
    ];
    for (lane, m) in &msgs {
        net.nodes[a].core.send(kb, *lane, m.clone());
        net.nodes[b].core.send(ka, *lane, m.clone());
    }
    net.run(2_000);
    for (i, from) in [(b, ka), (a, kb)] {
        let got: BTreeMap<(Lane, usize), usize> = net.nodes[i].got.iter().fold(BTreeMap::new(), |mut m, r| {
            assert_eq!(r.from, from);
            *m.entry((r.lane, r.bytes.len())).or_default() += 1;
            m
        });
        for (lane, m) in &msgs {
            assert_eq!(got.get(&(*lane, m.len())), Some(&1), "{lane:?} {} B at node {i}: {got:?}", m.len());
        }
        assert_eq!(net.nodes[i].got.len(), msgs.len(), "nothing twice");
    }
    let s = net.nodes[a].core.link(kb);
    assert_eq!(s.state, LinkState::Up);
    assert!((15_000_000..60_000_000).contains(&s.srtt_ns) && s.budget_bps > 0, "{s:?}");
}

/// More stream messages than the peer's stream limit (`N_streams_in`, 64) all arrive, each once: a message whose
/// stream quinn can't open yet waits for the peer's credit (3.5), it is never dropped (@engine-architect's finding).
#[test]
fn more_stream_messages_than_the_peer_s_stream_limit_all_arrive_once() {
    let (mut net, a, b) = joined();
    let (ka, kb) = (net.nodes[a].key, net.nodes[b].key);
    const N: usize = 200;
    for i in 0..N {
        for (lane, fill) in [(Lane::Repair, 1u8), (Lane::Bulk, 2)] {
            // Each message is distinct by its first bytes, so a duplicate would show.
            let mut m = vec![fill; 64];
            m[..4].copy_from_slice(&(i as u32).to_le_bytes());
            net.nodes[a].core.send(kb, lane, Bytes::from(m.clone()));
            net.nodes[b].core.send(ka, lane, Bytes::from(m));
        }
    }
    net.run(4_000);
    for i in [a, b] {
        let got: BTreeMap<(Lane, Vec<u8>), usize> = net.nodes[i].got.iter().fold(BTreeMap::new(), |mut m, r| {
            *m.entry((r.lane, r.bytes.to_vec())).or_default() += 1;
            m
        });
        for lane in [Lane::Repair, Lane::Bulk] {
            let n = got.iter().filter(|((l, _), _)| *l == lane).count();
            assert_eq!(n, N, "node {i}: {n} distinct {lane:?} messages of {N}");
        }
        assert!(got.values().all(|&c| c == 1), "node {i}: a message arrived twice");
        assert_eq!(net.nodes[i].got.len(), 2 * N, "node {i}");
    }
}

#[test]
fn each_guard_refuses_with_its_reason_and_the_joiner_is_told() {
    let mut net = Net::new();
    let a = net.add(1, "10.0.0.1:4433", true, "pe 0.1");
    let b = net.add(2, "10.0.0.2:4433", false, "pe 0.2");
    let inv = net.invite(a);
    let now = net.now;
    net.nodes[b].core.dial(now, &inv);
    net.run(200);
    let r = refused(&net.nodes[b]).expect("the joiner learns why");
    assert_eq!(r.code, CloseCode::Build);
    assert!(r.detail.contains("pe 0.1") && r.detail.contains("pe 0.2"), "{}", r.detail);
    assert!(!net.nodes[a].events.iter().any(|e| matches!(e, NetEvent::JoinRequest { .. })), "no join request");
    assert!(!net.nodes[b].events.iter().any(|e| matches!(e, NetEvent::LinkUp { .. })));
}

#[test]
fn a_refusal_reaches_the_joiner() {
    let mut net = Net::new();
    let a = net.add(1, "10.0.0.1:4433", true, "pe 0.1");
    let b = net.add(2, "10.0.0.2:4433", false, "pe 0.1");
    let inv = net.invite(a);
    let now = net.now;
    net.nodes[b].core.dial(now, &inv);
    net.run(100);
    let kb = net.nodes[b].key;
    let now = net.now;
    net.nodes[a].core.refuse(now, kb, Refusal { code: CloseCode::NotAdmitted, detail: "banned".into() });
    net.run(100);
    let r = refused(&net.nodes[b]).expect("refused");
    assert_eq!((r.code, r.detail.as_str()), (CloseCode::NotAdmitted, "banned"));
    assert!(!net.nodes[b].events.iter().any(|e| matches!(e, NetEvent::LinkUp { .. })));
}

#[test]
fn a_wrong_key_fails_and_nothing_is_admitted() {
    let mut net = Net::new();
    let a = net.add(1, "10.0.0.1:4433", true, "pe 0.1");
    let b = net.add(2, "10.0.0.2:4433", false, "pe 0.1");
    let mut inv = net.invite(a);
    inv.key = Identity::from_seed(&[3; 32]).public();
    let now = net.now;
    net.nodes[b].core.dial(now, &inv);
    net.run(300);
    assert!(refused(&net.nodes[b]).is_some(), "{:?}", net.nodes[b].life);
    assert!(net.nodes[a].events.is_empty() && net.nodes[b].events.is_empty());
}

#[test]
fn staggered_dials_reach_the_address_that_answers() {
    let mut net = Net::new();
    let a = net.add(1, "10.0.0.1:4433", true, "pe 0.1");
    let b = net.add(2, "10.0.0.2:4433", false, "pe 0.1");
    let mut inv = net.invite(a);
    let dead: SocketAddr = "192.0.2.9:4433".parse().unwrap();
    net.dead.push(dead);
    inv.addrs.insert(0, dead);
    let now = net.now;
    net.nodes[b].core.dial(now, &inv);
    net.run(400);
    net.admit_all(a);
    net.run(100);
    assert!(net.up(b, net.nodes[a].key), "{:?}", net.nodes[b].life);
}

#[test]
fn two_dials_at_once_keep_one_connection_on_both_sides() {
    let mut net = Net::new();
    let a = net.add(1, "10.0.0.1:4433", true, "pe 0.1");
    let b = net.add(2, "10.0.0.2:4433", false, "pe 0.1");
    let inv = net.invite(a);
    let reply = Reply { invite_id: inv.id, key: net.nodes[b].key.0, addrs: vec![net.nodes[b].addr] };
    let now = net.now;
    net.nodes[b].core.dial(now, &inv);
    net.nodes[a].core.take_reply(now, &reply);
    net.run(150);
    net.admit_all(a);
    net.run(150);
    let (ka, kb) = (net.nodes[a].key, net.nodes[b].key);
    let ups = |i: usize, p| net.nodes[i].events.iter().filter(|e| **e == NetEvent::LinkUp { peer: p }).count();
    assert_eq!((ups(a, kb), ups(b, ka)), (1, 1), "{:?} / {:?}", net.nodes[a].events, net.nodes[b].events);
    assert!(!net.nodes[a].events.iter().chain(&net.nodes[b].events).any(|e| matches!(e, NetEvent::LinkDown { .. })));
    net.nodes[b].core.send(ka, Lane::Action, Bytes::from_static(b"hi"));
    net.run(100);
    assert_eq!(net.nodes[a].got.len(), 1);
}

#[test]
fn nothing_moves_on_the_lanes_before_the_welcome() {
    let mut net = Net::new();
    let a = net.add(1, "10.0.0.1:4433", true, "pe 0.1");
    let b = net.add(2, "10.0.0.2:4433", false, "pe 0.1");
    let inv = net.invite(a);
    let now = net.now;
    net.nodes[b].core.dial(now, &inv);
    net.run(100);
    let (ka, kb) = (net.nodes[a].key, net.nodes[b].key);
    net.nodes[b].core.send(ka, Lane::Action, Bytes::from_static(b"early"));
    net.nodes[b].core.send(ka, Lane::Bulk, Bytes::from(vec![1u8; 5_000]));
    net.nodes[a].core.send(kb, Lane::Protocol, Bytes::from_static(b"early too"));
    net.run(500);
    assert!(net.nodes[a].got.is_empty() && net.nodes[b].got.is_empty(), "lane traffic before the welcome");
    net.admit_all(a);
    net.run(500);
    assert_eq!(net.nodes[a].got.len(), 2, "the joiner's queued messages go once it is welcomed");
    assert_eq!(net.nodes[b].got.len(), 1);
}

#[test]
fn a_member_opens_nothing_before_its_own_admit() {
    // @design-reviewer's Medium on draft 1.6: B, a member, admits A at once, so B's welcome reaches A before A's
    // app has decided. A never admits B and then refuses: not one lane item may cross either way. (An honest peer
    // can't put its hello and welcome in one flight, since its welcome needs our hello, which needs its Finished;
    // the welcome-before-hello path goes through the same member branch.)
    let mut net = Net::new();
    let a = net.add(1, "10.0.0.1:4433", true, "pe 0.1");
    let b = net.add(2, "10.0.0.2:4433", true, "pe 0.1");
    let (ka, kb) = (net.nodes[a].key, net.nodes[b].key);
    let inv = net.invite(a);
    let now = net.now;
    net.nodes[b].core.dial(now, &inv);
    net.run(150);
    net.admit_all(b); // B admits A as soon as A's hello is in
    net.nodes[a].events.retain(|e| !matches!(e, NetEvent::JoinRequest { .. })); // A's app sits on its request
    net.run(400);
    assert!(
        net.nodes[a].life.iter().any(|l| matches!(l, Lifecycle::Welcomed { .. })),
        "the welcome arrived: {:?}",
        net.nodes[a].life
    );
    net.nodes[a].core.send(kb, Lane::Action, Bytes::from_static(b"to b"));
    net.nodes[b].core.send(ka, Lane::Action, Bytes::from_static(b"to a"));
    net.nodes[b].core.send(ka, Lane::Bulk, Bytes::from(vec![1u8; 5_000]));
    net.run(300);
    assert!(net.nodes[a].got.is_empty() && net.nodes[b].got.is_empty(), "lane traffic before A's admit");
    assert!(!net.up(a, kb) && !net.up(b, ka));
    let now = net.now;
    net.nodes[a].core.refuse(now, kb, Refusal { code: CloseCode::NotAdmitted, detail: "banned".into() });
    net.run(200);
    assert!(net.nodes[a].got.is_empty() && net.nodes[b].got.is_empty());
    assert_eq!(refused(&net.nodes[b]).map(|r| r.code), Some(CloseCode::NotAdmitted));
}

#[test]
fn two_members_admit_each_other_without_an_up_record() {
    let mut net = Net::new();
    let a = net.add(1, "10.0.0.1:4433", true, "pe 0.1");
    let b = net.add(2, "10.0.0.2:4433", true, "pe 0.1");
    let (ka, kb) = (net.nodes[a].key, net.nodes[b].key);
    let inv = net.invite(a);
    let now = net.now;
    net.nodes[b].core.dial(now, &inv);
    net.run(150);
    net.admit_all(b);
    net.run(100);
    assert!(!net.up(a, kb) && !net.up(b, ka), "nobody is up before both have admitted");
    net.admit_all(a);
    net.run(150);
    assert!(net.up(a, kb) && net.up(b, ka), "{:?} / {:?}", net.nodes[a].life, net.nodes[b].life);
    assert!(!net.nodes[a].life.iter().chain(&net.nodes[b].life).any(|l| matches!(l, Lifecycle::Fault { .. })));
    net.nodes[a].core.send(kb, Lane::Protocol, Bytes::from_static(b"x"));
    net.nodes[b].core.send(ka, Lane::Protocol, Bytes::from_static(b"y"));
    net.run(100);
    assert_eq!((net.nodes[a].got.len(), net.nodes[b].got.len()), (1, 1));
}

#[test]
fn a_reply_taken_while_the_joiner_s_closed_connection_drains_still_dials() {
    // @engine-architect's Low 1 on net/app-net f7d31a31: A closes B's early dial (here a refusal; `T_welcome`
    // alike) and takes B's reply at once, while that connection drains. The dial must go out: a closed connection
    // is not "already connected" (6.2).
    let mut net = Net::new();
    let a = net.add(1, "10.0.0.1:4433", true, "pe 0.1");
    let b = net.add(2, "10.0.0.2:4433", false, "pe 0.1");
    let inv = net.invite(a);
    let now = net.now;
    net.nodes[b].core.dial(now, &inv);
    net.run(100);
    let kb = net.nodes[b].key;
    let requests =
        |n: &Node| n.events.iter().filter(|e| matches!(e, NetEvent::JoinRequest { peer, .. } if *peer == kb)).count();
    assert_eq!(requests(&net.nodes[a]), 1, "B's early dial");
    let now = net.now;
    net.nodes[a].core.refuse(now, kb, Refusal { code: CloseCode::Overloaded, detail: "no decision".into() });
    let reply = Reply { invite_id: inv.id, key: kb.0, addrs: vec!["10.0.0.2:4433".parse().unwrap()] };
    net.nodes[a].core.take_reply(now, &reply);
    net.run(300);
    assert_eq!(requests(&net.nodes[a]), 2, "A's own dial reached B: {:?}", net.nodes[a].events);
}

#[test]
fn t_welcome_closes_a_connection_nobody_decides_on() {
    let mut net = Net::new();
    let a = net.add(1, "10.0.0.1:4433", true, "pe 0.1");
    let b = net.add(2, "10.0.0.2:4433", false, "pe 0.1");
    let inv = net.invite(a);
    let now = net.now;
    net.nodes[b].core.dial(now, &inv);
    net.run(9_000);
    assert!(refused(&net.nodes[a]).is_none() && refused(&net.nodes[b]).is_none(), "not before T_welcome");
    net.run(2_000);
    let r = net.nodes[a].life.iter().find_map(|l| match l {
        Lifecycle::Refused { refusal, by_me: true, .. } => Some(refusal.code),
        _ => None,
    });
    assert!(matches!(r, Some(CloseCode::Overloaded)), "{:?}", net.nodes[a].life);
    assert!(refused(&net.nodes[b]).is_some(), "the joiner is told: {:?}", net.nodes[b].life);
}

#[test]
fn a_joiner_converges_on_the_first_members_clock() {
    // B's host clock runs 3.7 s ahead of A's. After the welcome and a few sample rounds, both read the same session
    // time and the same frame (5.4); the symmetric 10 ms paths make the offset exact, within a millisecond.
    let mut net = Net::new();
    let a = net.add(1, "10.0.0.1:4433", true, "pe 0.1");
    let b = net.add(2, "10.0.0.2:4433", false, "pe 0.1");
    net.nodes[b].skew = 3_700_000_000;
    let (ka, kb) = (net.nodes[a].key, net.nodes[b].key);
    let epoch = net.now.0 + 1_000_000_000;
    let now = net.now;
    net.nodes[a].core.start_first_member(now, epoch, SimRate::DEFAULT);
    let inv = net.invite(a);
    let now_b = net.now.plus_nanos(net.nodes[b].skew);
    net.nodes[b].core.dial(now_b, &inv);
    net.run(100);
    let w = Welcome { line: [9; 32], epoch_ns: epoch, rate_hz: 60, members: vec![(ka, vec![])], agree: vec![] };
    let now = net.now;
    net.nodes[a].core.admit(now, kb, &w);
    net.run(3_000);
    assert!(net.nodes[b].life.contains(&Lifecycle::ClockReady), "{:?}", net.nodes[b].life);
    let (fa, fb) = (net.nodes[a].core.frame_clock(), net.nodes[b].core.frame_clock());
    let (ta, tb) = (net.now, net.now.plus_nanos(3_700_000_000));
    let diff = fa.session_ns(ta) - fb.session_ns(tb);
    assert!(diff.abs() < 1_000_000, "session times differ by {diff} ns");
    assert_eq!(fa.frame_at(ta), fb.frame_at(tb));
    assert!(fa.frame_at(ta).is_some(), "past frame 0");
}

/// Two machines whose host clocks are two days apart, which a loopback test can't show (both ends share one host
/// clock). A opens a world whose genesis lists B, so A's clock counts B as a member from the start, as pe-app's
/// genesis does (`SetClockMembers`), while B's clock still converges. Neither side may report a clock fault, B is
/// clock-ready within `T_ready`, and both then read one session time (the first two-machine run: A reported
/// PeerRate of -5.2e13 ppb and Pinned, B NotReady; @engine-architect).
#[test]
fn a_genesis_member_whose_host_clock_is_days_away_converges_without_a_clock_fault() {
    let mut net = Net::new();
    let a = net.add(1, "10.0.0.1:4433", true, "pe 0.1");
    let b = net.add(2, "10.0.0.2:4433", false, "pe 0.1");
    let skew = 2 * 86_400 * 1_000_000_000i64;
    net.nodes[b].skew = skew;
    let (ka, kb) = (net.nodes[a].key, net.nodes[b].key);
    let epoch = net.now.0 + 8_000_000_000;
    let now = net.now;
    net.nodes[a].core.start_first_member(now, epoch, SimRate::DEFAULT);
    let inv = net.invite(a);
    net.nodes[b].core.dial(net.now.plus_nanos(skew), &inv);
    net.run(100);
    net.nodes[a].core.set_clock_members(vec![kb]);
    let w = Welcome { line: [9; 32], epoch_ns: epoch, rate_hz: 60, members: vec![(ka, vec![])], agree: vec![] };
    let now = net.now;
    net.nodes[a].core.admit(now, kb, &w);
    // The clock events of both sides, with the time each was first seen, ms after the welcome.
    let mut timeline: Vec<(u32, &str, Lifecycle)> = Vec::new();
    let mut seen = [0usize; 2];
    let mut ready_ms = None;
    for step in 1..=450u32 {
        net.run(100);
        for (j, (name, i)) in [("A", a), ("B", b)].into_iter().enumerate() {
            for l in &net.nodes[i].life[seen[j]..] {
                if matches!(l, Lifecycle::ClockFault(_) | Lifecycle::ClockReady) {
                    timeline.push((step * 100, name, l.clone()));
                    if name == "B" && *l == Lifecycle::ClockReady {
                        ready_ms.get_or_insert(step * 100);
                    }
                }
            }
            seen[j] = net.nodes[i].life.len();
        }
    }
    // T_ready is 5 s from the welcome. The run lasts 45 s, past W_clock (30 s) and T_pinned (10 s), so a
    // PeerRate or Pinned would have shown.
    let faults = timeline.iter().filter(|e| matches!(e.2, Lifecycle::ClockFault(_))).count();
    println!("B ready at {ready_ms:?} ms; clock events: {timeline:?}");
    assert!(
        ready_ms.is_some_and(|t| t <= 5_000) && faults == 0,
        "B ready at {ready_ms:?} ms; clock events (ms after the welcome, side, event): {timeline:?}"
    );
    let (fa, fb) = (net.nodes[a].core.frame_clock(), net.nodes[b].core.frame_clock());
    let (ta, tb) = (net.now, net.now.plus_nanos(skew));
    let diff = fa.session_ns(ta) - fb.session_ns(tb);
    assert!(diff.abs() < 1_000_000, "session times differ by {diff} ns");
    assert_eq!(fa.frame_at(ta), fb.frame_at(tb));
}

#[test]
fn a_silent_link_is_degraded_and_recovers() {
    let (mut net, a, b) = joined();
    let ka = net.nodes[a].key;
    // B hears nothing: everything sent to B is lost.
    let b_addr = net.nodes[b].addr;
    net.dead.push(b_addr);
    net.run(1_600);
    assert!(net.nodes[b].events.contains(&NetEvent::LinkDegraded { peer: ka }), "{:?}", net.nodes[b].events);
    net.dead.clear();
    net.nodes[b].events.clear();
    net.run(1_000);
    assert!(net.nodes[b].events.contains(&NetEvent::LinkUp { peer: ka }), "{:?}", net.nodes[b].events);
    assert!(!net.nodes[b].events.iter().any(|e| matches!(e, NetEvent::LinkDown { .. })));
}

#[test]
fn a_third_member_is_met_through_a_rendezvous() {
    // C is a member up with A. B joins through A with a welcome that names nobody, so only A's rendezvous
    // records can bring B and C together (6.7); C admits B, and messages flow between them.
    let mut net = Net::new();
    let a = net.add(1, "10.0.0.1:4433", true, "pe 0.1");
    let c = net.add(3, "10.0.0.3:4433", true, "pe 0.1");
    let b = net.add(2, "10.0.0.2:4433", false, "pe 0.1");
    let (kb, kc) = (net.nodes[b].key, net.nodes[c].key);
    let inv = net.invite(a);
    let now = net.now;
    net.nodes[c].core.dial(now, &inv);
    net.run(100);
    net.admit_all(a);
    net.admit_all(c);
    net.run(100);
    assert!(net.up(a, kc) && net.up(c, net.nodes[a].key), "A and C are up");
    let now = net.now;
    net.nodes[b].core.dial(now, &inv);
    net.run(100);
    net.admit_all(a);
    net.run(200);
    net.admit_all(c);
    net.run(200);
    assert!(net.up(b, kc) && net.up(c, kb), "B and C met: {:?} / {:?}", net.nodes[b].life, net.nodes[c].events);
    net.nodes[b].core.send(kc, Lane::Action, Bytes::from_static(b"hi c"));
    net.run(100);
    assert!(net.nodes[c].got.iter().any(|r| r.from == kb && r.bytes == Bytes::from_static(b"hi c")));
    let downs = net.nodes.iter().flat_map(|n| &n.events).filter(|e| matches!(e, NetEvent::LinkDown { .. })).count();
    assert_eq!(downs, 0, "one connection per pair, none dropped");
}

#[test]
fn a_link_degraded_long_after_joining_stays_open() {
    // @design-reviewer's High on v1: `T_welcome` covers only the way to the first Up, so a blip 11 s in leaves the
    // link Degraded and then Up again, with no close and no refusal on either side.
    let (mut net, a, b) = joined();
    let (ka, kb) = (net.nodes[a].key, net.nodes[b].key);
    net.run(11_000);
    let b_addr = net.nodes[b].addr;
    net.dead.push(b_addr);
    net.run(1_600);
    assert!(net.nodes[b].events.contains(&NetEvent::LinkDegraded { peer: ka }), "{:?}", net.nodes[b].events);
    net.dead.clear();
    net.nodes[b].events.clear();
    net.run(1_000);
    assert!(net.nodes[b].events.contains(&NetEvent::LinkUp { peer: ka }), "{:?}", net.nodes[b].events);
    for i in [a, b] {
        assert!(refused(&net.nodes[i]).is_none(), "{:?}", net.nodes[i].life);
        assert!(
            !net.nodes[i].events.iter().any(|e| matches!(e, NetEvent::LinkDown { .. })),
            "{:?}",
            net.nodes[i].events
        );
    }
    net.nodes[b].core.send(ka, Lane::Action, Bytes::from_static(b"still here"));
    net.run(100);
    assert!(net.nodes[a].got.iter().any(|r| r.from == kb && r.bytes == Bytes::from_static(b"still here")));
}

/// `n` addresses nobody answers, in a block of their own.
fn nowhere(block: u8, n: u8) -> Vec<SocketAddr> {
    (1..=n).map(|i| format!("198.51.{block}.{i}:4433").parse().unwrap()).collect()
}

fn rendezvous(key: PeerId, addrs: &[SocketAddr]) -> Vec<u8> {
    [&[0x83u8][..], &encode_rendezvous(&key, addrs)].concat()
}

/// Where node `i` sent packets that nobody received.
fn strays(net: &Net, i: usize) -> BTreeSet<SocketAddr> {
    net.stray.iter().filter(|s| s.0 == net.nodes[i].addr).map(|s| s.1).collect()
}

fn faulted(n: &Node, what: &str) -> bool {
    n.life.iter().any(|l| matches!(l, Lifecycle::Fault { what: w, .. } if w.contains(what)))
}

#[test]
fn a_rendezvous_from_a_joiner_is_a_fault_and_dials_nothing() {
    // @design-reviewer's Medium 1 on v1: only a member introduces (6.7). B, a joiner, sends 100 records naming
    // 16 addresses each; A dials none of them and faults B.
    let (mut net, a, b) = joined();
    let ka = net.nodes[a].key;
    for i in 0..100u8 {
        let key = Identity::from_seed(&[100 + i; 32]).public();
        net.nodes[b].core.testing_send_record(&ka, rendezvous(pe_seams::MemberKey(key), &nowhere(i, 16)));
        if i % 16 == 15 {
            net.run(30);
        }
    }
    net.run(300);
    assert!(strays(&net, a).is_empty(), "{:?}", strays(&net, a));
    assert!(faulted(&net.nodes[a], "non-member"), "{:?}", net.nodes[a].life);
}

#[test]
fn a_members_rendezvous_records_dial_one_set_per_key_within_the_bounds() {
    // @design-reviewer's Medium 1 on v1. C, a member up with A, names one key 30 times: A dials its 16 addresses
    // once. Then 70 more keys: A never has more than 64 such dials, and faults C past 64 records in `T_rdv`.
    let mut net = Net::new();
    let a = net.add(1, "10.0.0.1:4433", true, "pe 0.1");
    let c = net.add(3, "10.0.0.3:4433", true, "pe 0.1");
    let inv = net.invite(a);
    let now = net.now;
    net.nodes[c].core.dial(now, &inv);
    net.run(100);
    net.admit_all(a);
    net.admit_all(c);
    net.run(100);
    let ka = net.nodes[a].key;
    assert!(net.up(c, ka), "A and C are up");
    let k = pe_seams::MemberKey(Identity::from_seed(&[99; 32]).public());
    let first = nowhere(1, 16);
    for i in 0..30 {
        net.nodes[c].core.testing_send_record(&ka, rendezvous(k, &first));
        if i % 16 == 15 {
            net.run(30);
        }
    }
    net.run(300);
    let s = strays(&net, a);
    assert!(!s.is_empty() && s.iter().all(|x| first.contains(x)), "{s:?}");
    assert!(!faulted(&net.nodes[a], ""), "{:?}", net.nodes[a].life);
    for i in 0..70u8 {
        let key = Identity::from_seed(&[100 + i; 32]).public();
        net.nodes[c].core.testing_send_record(&ka, rendezvous(pe_seams::MemberKey(key), &nowhere(10 + i, 16)));
        if i % 16 == 15 {
            net.run(30);
        }
    }
    net.run(300);
    let s = strays(&net, a);
    assert!(s.len() <= 64, "{} addresses dialled", s.len());
    assert!(faulted(&net.nodes[a], "too many rendezvous"), "{:?}", net.nodes[a].life);
}

/// A QUIC packet's destination connection ID, and its source ID if it has a long header.
fn cids(b: &[u8], short_len: usize) -> (Vec<u8>, Option<Vec<u8>>) {
    if b[0] & 0x80 == 0 {
        return (b.get(1..1 + short_len).unwrap_or_default().to_vec(), None);
    }
    let dl = usize::from(b[5]);
    let dcid = b[6..6 + dl].to_vec();
    let sl = usize::from(b[6 + dl]);
    (dcid, Some(b[7 + dl..7 + dl + sl].to_vec()))
}

#[test]
fn staggered_dials_confirmed_in_opposite_orders_keep_one_connection() {
    // @design-reviewer's Medium 2 on v1. B dials A twice and completes the first handshake first; its handshake
    // packets on the first connection are held 100 ms, so A completes the second first, and its short-header
    // packets on the second are lost for 300 ms. Both sides keep the same connection: the link comes up once,
    // nothing is refused, nothing goes down. Measured: the old "first confirmed" rule passes this too, because B
    // closes the duplicate it completes second before its Finished leaves, and quinn sends that close in a
    // Handshake packet, so A never completes it. The duplicate tag removes the dependence on that timing.
    let mut net = Net::new();
    let a = net.add(1, "10.0.0.1:4433", true, "pe 0.1");
    let b = net.add(2, "10.0.0.2:4433", false, "pe 0.1");
    let (a_addr, b_addr) = (net.nodes[a].addr, net.nodes[b].addr);
    let inv = net.invite(a);
    let client_cid = |net: &Net| {
        let p = net.flight.iter().rev().find(|f| f.1 == b_addr).expect("B's Initial");
        cids(&p.3, 0).1.expect("a long header")
    };
    let now = net.now;
    net.nodes[b].core.dial(now, &inv);
    net.run(1);
    let c1 = client_cid(&net);
    let now = net.now;
    net.nodes[b].core.dial(now, &inv);
    net.run(1);
    let c2 = client_cid(&net);
    assert_ne!(c1, c2);
    let start = net.now;
    // A's connection ID for the second connection, learnt from A's first long header to B on it.
    let s2: std::cell::RefCell<Option<Vec<u8>>> = Default::default();
    net.extra = Some(Box::new(move |now, from, to, bytes| {
        if now.0 > start.0 + 300_000_000 {
            return 0;
        }
        if from == a_addr && to == b_addr {
            if let (d, Some(s)) = cids(bytes, 0)
                && d == c2
                && s2.borrow().is_none()
            {
                *s2.borrow_mut() = Some(s);
            }
            return 0;
        }
        let len = s2.borrow().as_ref().map_or(0, Vec::len);
        let (d, s) = cids(bytes, len);
        if s.as_deref() == Some(&c1[..]) {
            100 // B's handshake packets on the first connection, its Finished among them
        } else if s.is_none() && len > 0 && s2.borrow().as_deref() == Some(&d[..]) {
            10_000 // B's short-header packets on the second connection, a close among them: lost
        } else {
            0
        }
    }));
    net.run(600);
    net.admit_all(a);
    net.run(300);
    let (ka, kb) = (net.nodes[a].key, net.nodes[b].key);
    let ups = |i: usize, p| net.nodes[i].events.iter().filter(|e| **e == NetEvent::LinkUp { peer: p }).count();
    assert_eq!((ups(a, kb), ups(b, ka)), (1, 1), "{:?} / {:?}", net.nodes[a].events, net.nodes[b].events);
    for i in [a, b] {
        assert!(refused(&net.nodes[i]).is_none(), "{:?}", net.nodes[i].life);
        assert!(
            !net.nodes[i].events.iter().any(|e| matches!(e, NetEvent::LinkDown { .. })),
            "{:?}",
            net.nodes[i].events
        );
    }
    net.nodes[b].core.send(ka, Lane::Action, Bytes::from_static(b"one link"));
    net.run(100);
    assert!(net.nodes[a].got.iter().any(|r| r.from == kb && r.bytes == Bytes::from_static(b"one link")));
}

#[test]
fn a_duplicate_close_on_the_only_connection_is_an_ordinary_loss() {
    // @design-reviewer's Medium on v1b: a close saying "duplicate connection" is quiet only if another connection
    // to that peer stands in for it. A closes its only, up connection to B that way; B gets LinkDown.
    let (mut net, a, b) = joined();
    let (ka, kb) = (net.nodes[a].key, net.nodes[b].key);
    let now = net.now;
    net.nodes[a].core.refuse(now, kb, Refusal { code: CloseCode::Left, detail: "duplicate connection".into() });
    net.run(200);
    assert!(net.nodes[b].events.contains(&NetEvent::LinkDown { peer: ka }), "{:?}", net.nodes[b].events);
    assert!(!net.nodes[b].core.peers().contains(&ka));
}

/// @ticket:primordial-engine/pe-diverge-repair-fails-under-internet-like-late (K4 R4'): when a link's delay rises
/// mid-session, as a route change or Wi-Fi jitter does, a Repair message still goes out at once and arrives about one
/// one-way delay later. The delay guard held it until `base_rtt`'s window (`W_base`, 30 s) rolled past the faster
/// samples: 30.6 s, while every repair at n = 2 timed out.
#[test]
fn a_repair_message_arrives_about_one_delay_after_the_link_slows() {
    let (mut net, a, b) = joined();
    let (ka, kb) = (net.nodes[a].key, net.nodes[b].key);
    // Both sides' hands: an Action message each way every frame.
    let traffic = |net: &mut Net, ms: u32| {
        for _ in 0..ms.div_ceil(16) {
            net.nodes[a].core.send(kb, Lane::Action, Bytes::from(vec![1u8; 120]));
            net.nodes[b].core.send(ka, Lane::Action, Bytes::from(vec![2u8; 120]));
            net.run(16);
        }
    };
    traffic(&mut net, 3_000);
    net.delay_ms = 45;
    let mut waits = Vec::new();
    for _ in 0..20 {
        let before = net.nodes[b].got.iter().filter(|r| r.lane == Lane::Repair).count();
        net.nodes[a].core.send(kb, Lane::Repair, Bytes::from(vec![3u8; 200]));
        let mut waited = 0;
        while net.nodes[b].got.iter().filter(|r| r.lane == Lane::Repair).count() == before && waited < 5_000 {
            traffic(&mut net, 16);
            waited += 16;
        }
        waits.push(waited);
        traffic(&mut net, 240);
    }
    // One way is 45 ms; the traffic loop steps 16 ms at a time, and a lost-and-resent stream packet would cost an RTO.
    assert!(waits.iter().all(|w| *w <= 45 + 2 * 16), "Repair waits after the delay rose, ms: {waits:?}");
}

// The outage probe (transport.md 6.5, pe_net::outage). An outage of L leaves a link dark for up to about 2L under
// quinn's plain Cubic, since its probe wait doubles and resets only on an ACK, and each direction waits for its own
// probe (@engine-architect's K8 on two machines; @fable-reviewer's reading of quinn-proto 0.11.19).

/// One 16 ms tick: with `moving`, each side sends the other an Action message and a Pose, as a session's hands do.
fn tick(net: &mut Net, a: usize, b: usize, moving: bool) {
    if moving {
        let (ka, kb) = (net.nodes[a].key, net.nodes[b].key);
        net.nodes[a].core.send(kb, Lane::Action, Bytes::from_static(b"entry from a"));
        net.nodes[a].core.send(kb, Lane::Pose, Bytes::from_static(b"pose from a"));
        net.nodes[b].core.send(ka, Lane::Action, Bytes::from_static(b"entry from b"));
        net.nodes[b].core.send(ka, Lane::Pose, Bytes::from_static(b"pose from b"));
    }
    net.run(16);
}

/// When each side, A then B, first hears the other after an outage, and (moving) when it holds every Action the
/// other sent before the link came back: in ms after the link's return.
#[derive(Debug)]
struct Tail {
    heard: [Option<i64>; 2],
    caught_up: [Option<i64>; 2],
}

/// Both ways dark for `outage_ms`, after 2 s of ticks, then up to 40 s more. Anything received counts as heard,
/// a Received or a LinkUp.
fn dark_tail(tweak: impl Fn(&mut Config), outage_ms: u32, moving: bool) -> (Option<i64>, Option<i64>) {
    let t = tail(tweak, outage_ms, moving);
    (t.heard[0], t.heard[1])
}

fn tail(tweak: impl Fn(&mut Config), outage_ms: u32, moving: bool) -> Tail {
    let (mut net, a, b) = joined_with(tweak);
    for _ in 0..125 {
        tick(&mut net, a, b, moving);
    }
    net.dead = vec![net.nodes[a].addr, net.nodes[b].addr];
    for _ in 0..outage_ms / 16 {
        tick(&mut net, a, b, moving);
    }
    net.dead.clear();
    let back = net.now;
    // Every tick so far sent one Action each way: 125 before the outage and outage_ms / 16 in it.
    let owed = if moving { 125 + outage_ms as usize / 16 } else { 0 };
    let actions = |n: &Node| n.got.iter().filter(|r| r.lane == Lane::Action).count();
    let had: Vec<usize> = net.nodes.iter().map(actions).collect();
    for n in &mut net.nodes {
        n.got.clear();
        n.events.clear();
    }
    let mut heard = [None, None];
    let mut caught_up = [None, None];
    for _ in 0..40_000 / 16 {
        tick(&mut net, a, b, moving);
        for (k, i) in [a, b].into_iter().enumerate() {
            let n = &net.nodes[i];
            if heard[k].is_none() {
                let got = n.got.first().map(|r| r.at);
                let up = n.events.iter().any(|e| matches!(e, NetEvent::LinkUp { .. }));
                if got.is_some() || up {
                    heard[k] = Some((got.unwrap_or(net.now).0 - back.0) / 1_000_000);
                }
            }
            // The Actions sent after the return arrive first; caught up once all the earlier ones are in too.
            let after = (net.now.0 - back.0) / 16_000_000;
            if caught_up[k].is_none() && had[i] + actions(n) >= owed + after as usize {
                caught_up[k] = Some((net.now.0 - back.0) / 1_000_000);
            }
        }
        if heard.iter().all(Option::is_some) && caught_up.iter().all(Option::is_some) {
            break;
        }
    }
    for i in [a, b] {
        assert!(
            !net.nodes[i].events.iter().any(|e| matches!(e, NetEvent::LinkDown { .. })),
            "the link stays open: {:?}",
            net.nodes[i].events
        );
    }
    Tail { heard, caught_up }
}

#[test]
fn quinn_s_plain_backoff_leaves_a_dark_tail_after_an_outage() {
    // The problem, kept as a measurement: with the probe off, a 10 s outage leaves at least one side dark for
    // seconds after the link is back.
    let (ta, tb) = dark_tail(|c| c.t_trip = None, 10_000, true);
    let worst = ta.unwrap_or(i64::MAX).max(tb.unwrap_or(i64::MAX));
    eprintln!("plain Cubic, 10 s outage, moving: dark tail A {ta:?} ms, B {tb:?} ms");
    assert!(worst >= 2_000, "expected a tail of seconds: A {ta:?} ms, B {tb:?} ms");
}

#[test]
fn after_an_outage_both_sides_hear_each_other_within_a_second() {
    // The acceptance of @ticket pe-net-recover-within-1-s-after-a-link-outage-qu, on the default config: dark time
    // within about 1 s of the outage, both ways, for K8's cases (0.5 s and 10 s, hands still and moving) and a
    // 2-3 s drop.
    for (outage, moving) in [(500, true), (2_500, true), (10_000, true), (2_500, false), (10_000, false)] {
        let (ta, tb) = dark_tail(|_| {}, outage, moving);
        eprintln!("outage probe, {outage} ms outage, moving {moving}: dark tail A {ta:?} ms, B {tb:?} ms");
        for t in [ta, tb] {
            assert!(t.is_some_and(|t| t <= 1_000), "{outage} ms, moving {moving}: A {ta:?} ms, B {tb:?} ms");
        }
    }
}

/// The bytes A puts into a 5 s outage over its last 4 s, with ticks both ways, and with `repair` bytes of Repair
/// queued 1 s in; and whether B has the Repair 4 s after the link is back.
fn into_outage(repair: usize) -> (usize, bool) {
    let (mut net, a, b) = joined();
    let kb = net.nodes[b].key;
    for _ in 0..125 {
        tick(&mut net, a, b, true);
    }
    net.dead = vec![net.nodes[a].addr, net.nodes[b].addr];
    for _ in 0..60 {
        tick(&mut net, a, b, true);
    }
    let before = net.swallowed;
    if repair > 0 {
        net.nodes[a].core.send(kb, Lane::Repair, Bytes::from(vec![7u8; repair]));
    }
    for _ in 0..250 {
        tick(&mut net, a, b, true);
    }
    let during = net.swallowed - before;
    net.dead.clear();
    for _ in 0..250 {
        tick(&mut net, a, b, true);
    }
    let arrived = net.nodes[b].got.iter().any(|r| r.lane == Lane::Repair && r.bytes.len() == repair);
    (during, arrived)
}

#[test]
fn an_open_window_carries_no_repair_into_an_outage() {
    // @fable-reviewer's rule (via @coordinator-opus): while a link has heard nothing for `T_trip`, it
    // sends no Repair or Bulk, so the probe's open window doesn't pour them into a dead link. 256 KiB of Repair
    // queued 1 s into a 5 s outage add next to nothing to what the ticks put into it, and arrive once it's back.
    let (ticks, _) = into_outage(0);
    let (with, arrived) = into_outage(256 * 1024);
    eprintln!("4 s into a 5 s outage: {ticks} B from the ticks alone, {with} B with 256 KiB of Repair queued");
    assert!(with <= ticks + 4 * 1024, "the Repair went into the outage: {with} B against {ticks} B");
    assert!(arrived, "the Repair arrives once the link is back");
}

#[test]
fn after_an_outage_the_missed_actions_arrive_within_a_second() {
    // The Actions sent into the outage are pe-net's own retransmissions (3.3), each on a timer that doubled through
    // it; an acknowledgement of anything newer sends them back to the base RTO, so they follow the link's return
    // within about a round trip, not up to 64 RTOs later.
    for outage in [2_500, 10_000] {
        let t = tail(|_| {}, outage, true);
        eprintln!("outage probe, {outage} ms outage: {t:?}");
        for c in t.caught_up {
            assert!(c.is_some_and(|c| c <= 1_000), "{outage} ms: {t:?}");
        }
    }
}

#[test]
fn an_idle_link_on_a_long_path_is_never_silent() {
    // @design-reviewer's L1: the silence rule waits `T_trip` = max(0.3 s, 3 x srtt), as the outage probe does. Here
    // 120 ms one way, and the path bunches B's packets to A so they land only on 400 ms boundaries (a jittery
    // internet path): A hears B every 0.4 s, well inside 3 x srtt. With a fixed 0.3 s, A went silent for the last
    // 0.1 s of each cycle and held Repair until the next bunch. Repair sent at staggered times arrives one delay later.
    let (mut net, a, b) = joined();
    let (ka, kb) = (net.nodes[a].addr, net.nodes[b].key);
    net.delay_ms = 120;
    net.extra = Some(Box::new(move |now, _from, to, _bytes| {
        if to == ka {
            let ms = now.0 / 1_000_000 + 120;
            (400 - ms % 400) % 400
        } else {
            0
        }
    }));
    net.run(5_000);
    let mut waits = Vec::new();
    for i in 0..20u32 {
        let before = net.nodes[b].got.iter().filter(|r| r.lane == Lane::Repair).count();
        net.nodes[a].core.send(kb, Lane::Repair, Bytes::from(vec![3u8; 200]));
        let mut waited = 0;
        while net.nodes[b].got.iter().filter(|r| r.lane == Lane::Repair).count() == before && waited < 5_000 {
            net.run(1);
            waited += 1;
        }
        waits.push(waited);
        net.run(400 + 37 * i);
    }
    assert!(waits.iter().all(|w| *w <= 120 + 16), "Repair waits on an idle 120 ms link, ms: {waits:?}");
}

// The redial (transport.md 6.5): a member's link that closed for silence, past `T_idle`, is dialled again by both
// sides until `T_redial`, and comes back up on a new connection.

const T_IDLE_TEST: std::time::Duration = std::time::Duration::from_secs(3);

/// Short settings, so a test's outage outlasts `T_idle` in seconds: `T_idle` 3 s, attempts 0.5 s doubling to 2 s,
/// for `window_ms`.
fn redialling(window_ms: u64) -> impl Fn(&mut Config) {
    move |c: &mut Config| {
        c.t_idle = T_IDLE_TEST;
        c.redial = Some(RedialParams {
            window: std::time::Duration::from_millis(window_ms),
            first: std::time::Duration::from_millis(500),
            max: std::time::Duration::from_secs(2),
        });
    }
}

/// [`joined_with`], then both are members of one session, as pe-app's join leaves them (`set_member`,
/// `set_clock_members`).
fn members_with(tweak: impl Fn(&mut Config)) -> (Net, usize, usize) {
    let (mut net, a, b) = joined_with(tweak);
    let (ka, kb) = (net.nodes[a].key, net.nodes[b].key);
    net.nodes[b].core.set_member(true);
    net.nodes[a].core.set_clock_members(vec![kb]);
    net.nodes[b].core.set_clock_members(vec![ka]);
    (net, a, b)
}

/// Run `ms`, admitting every join request on both sides every 10 ms, as pe-app's hook admits a member.
fn run_admitting(net: &mut Net, a: usize, b: usize, ms: u32) {
    for _ in 0..ms / 10 {
        net.run(10);
        net.admit_all(a);
        net.admit_all(b);
    }
}

fn redialling_events(n: &Node) -> usize {
    n.life.iter().filter(|l| matches!(l, Lifecycle::Redialling { .. })).count()
}

fn lost(n: &Node, peer: PeerId) -> bool {
    n.life.contains(&Lifecycle::Lost { peer })
}

#[test]
fn a_member_s_link_closed_by_silence_is_redialled_and_comes_back_up() {
    let (mut net, a, b) = members_with(redialling(60_000));
    let (ka, kb) = (net.nodes[a].key, net.nodes[b].key);
    net.dead = vec![net.nodes[a].addr, net.nodes[b].addr];
    net.run(10_000);
    for (i, peer) in [(a, kb), (b, ka)] {
        let n = &net.nodes[i];
        assert!(n.events.contains(&NetEvent::LinkDown { peer }), "closed at T_idle: {:?}", n.events);
        assert_eq!(redialling_events(n), 1, "{:?}", n.life);
        assert!(!lost(n, peer), "{:?}", n.life);
    }
    // 7 s dark after the close: attempts at 0, 0.5, 1.5, 3.5, 5.5 s, each a handful of Initials: no tight loop.
    eprintln!("swallowed {} bytes in 10 s dark, 7 s of them redialling", net.swallowed);
    assert!(net.swallowed < 60_000, "{} bytes into the outage", net.swallowed);
    net.dead.clear();
    for n in &mut net.nodes {
        n.events.clear();
    }
    let back = net.now;
    let mut up_at = None;
    for _ in 0..600 {
        run_admitting(&mut net, a, b, 10);
        if up_at.is_none() && net.up(a, kb) && net.up(b, ka) {
            up_at = Some((net.now.0 - back.0) / 1_000_000);
        }
    }
    eprintln!("redial: both up {up_at:?} ms after the path's return");
    // The next attempt is at most `T_redial_max` (2 s) away, then a round trip or two and the welcomes.
    assert!(up_at.is_some_and(|t| t <= 2_500), "up {up_at:?} ms after the return: {:?}", net.nodes[a].events);
    net.nodes[a].core.send(kb, Lane::Action, Bytes::from_static(b"back"));
    net.nodes[b].core.send(ka, Lane::Action, Bytes::from_static(b"back too"));
    net.run(200);
    assert!(net.nodes[b].got.iter().any(|r| r.from == ka && r.bytes == Bytes::from_static(b"back")));
    assert!(net.nodes[a].got.iter().any(|r| r.from == kb && r.bytes == Bytes::from_static(b"back too")));
    for i in [a, b] {
        assert_eq!(net.nodes[i].core.peers().len(), 1, "one connection per pair");
        assert!(refused(&net.nodes[i]).is_none(), "{:?}", net.nodes[i].life);
        assert_eq!(redialling_events(&net.nodes[i]), 1, "and the redial ended: {:?}", net.nodes[i].life);
    }
    // Ended: nothing more is dialled.
    let before = net.stray.len();
    run_admitting(&mut net, a, b, 3_000);
    assert_eq!(net.stray.len(), before);
}

#[test]
fn a_one_way_outage_past_t_idle_comes_back_within_a_second() {
    // Only one direction dies, the packets to one side X: X hears nothing and closes at T_idle, and starts its
    // redial; the other side Y heard X until then, so it holds the old link, quiet (Degraded) but not closed, when
    // the path returns and X's attempt reaches it. It comes up within a second, long before Y's own T_idle: Y's
    // packets on the old connection reach X, which no longer knows it and answers with a stateless reset, so Y
    // closes it (and redials too) at once. Measured: Y's old link ends that way in both cases, before X's attempt
    // is confirmed; a rule preferring the new connection over a quiet one, tried first, never fired. Both choices
    // of X, as the duplicate rule's keys differ.
    for x_is_a in [true, false] {
        let (mut net, a, b) = members_with(redialling(60_000));
        let (x, y) = if x_is_a { (a, b) } else { (b, a) };
        let (kx, ky) = (net.nodes[x].key, net.nodes[y].key);
        net.dead = vec![net.nodes[x].addr];
        // X closes at about 3 s; Y's link has been quiet since (X sends nothing once closed).
        net.run(3_000 + 1_800);
        assert!(net.nodes[x].events.contains(&NetEvent::LinkDown { peer: ky }), "X closed");
        assert!(!net.nodes[y].events.iter().any(|e| matches!(e, NetEvent::LinkDown { .. })), "Y still holds it");
        net.dead.clear();
        for n in &mut net.nodes {
            n.events.clear();
        }
        let back = net.now;
        let mut up_at = None;
        for _ in 0..300 {
            run_admitting(&mut net, a, b, 10);
            if up_at.is_none() && net.up(x, ky) && net.up(y, kx) {
                up_at = Some((net.now.0 - back.0) / 1_000_000);
            }
        }
        eprintln!(
            "one-way outage, X = {}: up {up_at:?} ms after the return; Y's events {:?}",
            if x_is_a { "A" } else { "B" },
            net.nodes[y].events
        );
        // Y's own T_idle would close the old link about 1.2 s after the return: up before that means Y took X's
        // attempt. (Y's old link may also end sooner by X's stateless reset, once X hears Y's packets for it.)
        assert!(up_at.is_some_and(|t| t < 1_000), "X = A: {x_is_a}, up {up_at:?}: {:?}", net.nodes[y].events);
        assert_eq!(net.nodes[y].core.peers(), vec![kx]);
        net.nodes[y].core.send(kx, Lane::Action, Bytes::from_static(b"on the new one"));
        net.run(200);
        assert!(net.nodes[x].got.iter().any(|r| r.from == ky && r.bytes == Bytes::from_static(b"on the new one")));
    }
}

#[test]
fn a_redial_gives_up_after_its_window_and_reports_the_link_lost() {
    let (mut net, a, b) = members_with(redialling(5_000));
    let (ka, kb) = (net.nodes[a].key, net.nodes[b].key);
    net.dead = vec![net.nodes[a].addr, net.nodes[b].addr];
    net.run(3_000 + 5_000 + 200);
    assert!(lost(&net.nodes[a], kb) && lost(&net.nodes[b], ka), "{:?} / {:?}", net.nodes[a].life, net.nodes[b].life);
    net.dead.clear();
    for n in &mut net.nodes {
        n.events.clear();
    }
    run_admitting(&mut net, a, b, 5_000);
    for (i, peer) in [(a, kb), (b, ka)] {
        assert!(!net.up(i, peer), "nothing dials once it gave up: {:?}", net.nodes[i].events);
        assert!(net.nodes[i].core.peers().is_empty());
    }
}

#[test]
fn a_closed_link_is_lost_at_once_and_never_redialled() {
    // A close says the peer meant it (it stopped, or refused): no redial, and the app hears `Lost` at once.
    let (mut net, a, b) = members_with(redialling(60_000));
    let kb = net.nodes[b].key;
    let now = net.now;
    net.nodes[b].core.close_all(now);
    net.run(200);
    assert!(net.nodes[a].events.contains(&NetEvent::LinkDown { peer: kb }));
    assert!(lost(&net.nodes[a], kb), "{:?}", net.nodes[a].life);
    assert_eq!(redialling_events(&net.nodes[a]), 0);
    net.nodes[a].events.clear();
    run_admitting(&mut net, a, b, 3_000);
    assert!(net.nodes[a].core.peers().is_empty());
    assert!(!net.nodes[b].events.iter().any(|e| matches!(e, NetEvent::JoinRequest { .. })), "A dialled nothing");
    assert!(!net.nodes[a].events.iter().any(|e| matches!(e, NetEvent::LinkUp { .. })));
}

#[test]
fn a_peer_that_refuses_a_redial_ends_it() {
    // B's attempt reaches an A whose app no longer takes B (say B was removed meanwhile): A refuses, and B stops.
    let (mut net, a, b) = members_with(redialling(60_000));
    let (ka, kb) = (net.nodes[a].key, net.nodes[b].key);
    net.dead = vec![net.nodes[a].addr, net.nodes[b].addr];
    net.run(4_000);
    net.nodes[a].core.set_clock_members(vec![]);
    net.dead.clear();
    let mut requests = 0;
    for _ in 0..300 {
        net.run(10);
        let now = net.now;
        let reqs: Vec<PeerId> = net.nodes[a]
            .events
            .drain(..)
            .filter_map(|e| match e {
                NetEvent::JoinRequest { peer, .. } => Some(peer),
                _ => None,
            })
            .collect();
        for p in reqs {
            requests += 1;
            net.nodes[a].core.refuse(now, p, Refusal { code: CloseCode::NotAdmitted, detail: "not a member".into() });
        }
    }
    assert!(lost(&net.nodes[b], ka), "{:?}", net.nodes[b].life);
    assert_eq!(redialling_events(&net.nodes[a]), 1, "A started one too, ended when B stopped being a member");
    assert!(!lost(&net.nodes[a], kb), "a key that is no longer a member isn't reported");
    assert_eq!(requests, 1, "B stopped at the first refusal");
    net.run(5_000);
    assert!(net.nodes[b].core.peers().is_empty());
    assert!(!net.nodes[a].events.iter().any(|e| matches!(e, NetEvent::JoinRequest { .. })));
}

#[test]
fn a_non_member_s_link_is_not_redialled() {
    // pe-net redials only keys the app names as members (`set_clock_members`): a joiner's link to its inviter that
    // closes mid-join isn't, nor one of a peer that was never a member.
    let (mut net, a, b) = joined_with(redialling(60_000));
    net.dead = vec![net.nodes[a].addr, net.nodes[b].addr];
    net.run(5_000);
    for i in [a, b] {
        assert_eq!(redialling_events(&net.nodes[i]), 0, "{:?}", net.nodes[i].life);
        assert!(!net.nodes[i].life.iter().any(|l| matches!(l, Lifecycle::Lost { .. })));
    }
}

#[test]
fn a_redial_carries_on_past_the_peer_s_overloaded_close() {
    // @chunks-reviewer's Low on 540eaacf: A's app doesn't decide on B's attempt within T_welcome (busy catching up
    // after the outage), so A closes it `Overloaded`. That isn't a refusal of B: B's redial carries on, and the link
    // comes up once A's app admits it.
    // Only A's T_welcome is short, so A's close reaches B before B's own T_welcome closes the attempt.
    let mut net = Net::new();
    let a = net.add_with(1, "10.0.0.1:4433", true, "pe 0.1", |c: &mut Config| {
        redialling(60_000)(c);
        c.t_welcome = std::time::Duration::from_secs(1);
    });
    let b = net.add_with(2, "10.0.0.2:4433", false, "pe 0.1", redialling(60_000));
    let inv = net.invite(a);
    let now = net.now;
    net.nodes[b].core.dial(now, &inv);
    net.run(100);
    net.admit_all(a);
    net.run(100);
    let (ka, kb) = (net.nodes[a].key, net.nodes[b].key);
    assert!(net.up(a, kb) && net.up(b, ka));
    net.nodes[b].core.set_member(true);
    net.nodes[a].core.set_clock_members(vec![kb]);
    net.nodes[b].core.set_clock_members(vec![ka]);
    net.dead = vec![net.nodes[a].addr, net.nodes[b].addr];
    net.run(4_000);
    net.dead.clear();
    for n in &mut net.nodes {
        n.events.clear();
    }
    // 3 s with A's app deciding nothing (only B admits); A's T_welcome closes the attempts it holds.
    for _ in 0..300 {
        net.run(10);
        net.admit_all(b);
    }
    let overloaded = |n: &Node| {
        n.life.iter().any(|l| matches!(l, Lifecycle::Refused { refusal, .. } if refusal.code == CloseCode::Overloaded))
    };
    assert!(overloaded(&net.nodes[b]), "A closed an attempt Overloaded: {:?}", net.nodes[b].life);
    assert!(!lost(&net.nodes[b], ka), "and B's redial carried on: {:?}", net.nodes[b].life);
    run_admitting(&mut net, a, b, 6_000);
    assert!(net.up(a, kb) && net.up(b, ka), "up once A admits: {:?}", net.nodes[b].events);
}
