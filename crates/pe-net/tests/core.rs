//! pe-net's Core between in-memory peers (transport.md 6, 9.1 and 9.4): joining from an invite, the guards,
//! refusals, a wrong key, staggered dials, two dials at once, and every lane both ways once the link is up.

use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;

use bytes::Bytes;
use pe_net::core::{Config, Core, Lifecycle};
use pe_net::lifecycle::{CloseCode, Hello, Refusal, Welcome, encode_rendezvous};
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
    /// Addresses that swallow everything sent to them.
    dead: Vec<SocketAddr>,
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
            delay_ms: 10,
            flight: Vec::new(),
            hold: Vec::new(),
            extra: None,
            stray: Vec::new(),
        }
    }

    fn add(&mut self, seed: u8, addr: &str, member: bool, build: &str) -> usize {
        let id = Identity::from_seed(&[seed; 32]);
        let key = pe_seams::MemberKey(id.public());
        let core = Core::new(Config::new(hello(build), member), id, [seed; 32]).unwrap();
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
        Invite { id: [5; 8], key: self.nodes[i].key.0, expiry_unix_s: u64::MAX, addrs: vec![self.nodes[i].addr] }
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
                    if !self.dead.contains(&tx.to) {
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
    let mut net = Net::new();
    let a = net.add(1, "10.0.0.1:4433", true, "pe 0.1");
    let b = net.add(2, "10.0.0.2:4433", false, "pe 0.1");
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
