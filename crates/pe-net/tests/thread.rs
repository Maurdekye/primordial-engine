//! The network thread over real UDP sockets on loopback (transport.md 7): a joiner dials an invite, is welcomed,
//! and messages go both ways through the simulation thread's handle, which never blocks.

use std::net::UdpSocket;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use pe_net::core::{Config, Core, Lifecycle};
use pe_net::lifecycle::{CloseCode, Hello, Refusal, Welcome};
use pe_net::thread::{Arrival, Command, HostClock, NetHandle};
use pe_net_crypto::identity::{Identity, Invite};
use pe_seams::{HostTime, Lane, MemberKey, NetEvent, Transport};

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

/// Wait up to 5 s of wall time for `f` to hold.
fn until(mut f: impl FnMut() -> bool) -> bool {
    let end = Instant::now() + Duration::from_secs(5);
    while Instant::now() < end {
        if f() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    false
}

#[test]
fn a_joiner_connects_over_loopback_and_messages_flow() {
    let start = Instant::now();
    let clock: HostClock = Arc::new(move || HostTime(start.elapsed().as_nanos() as i64));
    let (ia, ib) = (Identity::from_seed(&[1; 32]), Identity::from_seed(&[2; 32]));
    let (ka, kb) = (MemberKey(ia.public()), MemberKey(ib.public()));
    let sa = UdpSocket::bind("127.0.0.1:0").unwrap();
    let sb = UdpSocket::bind("127.0.0.1:0").unwrap();
    let addr_a = sa.local_addr().unwrap();
    let ca = Core::new(Config::new(hello(), true), ia, [1; 32]).unwrap();
    let cb = Core::new(Config::new(hello(), false), ib, [2; 32]).unwrap();
    let mut a = NetHandle::spawn(ca, sa, clock.clone(), 4096).unwrap();
    let mut b = NetHandle::spawn(cb, sb, clock, 4096).unwrap();

    let invite = Invite { id: [5; 8], key: ka.0, expiry_unix_s: u64::MAX, addrs: vec![addr_a], pin: None };
    assert!(b.command(Command::Dial(invite)));
    let mut a_events = Vec::new();
    assert!(until(|| {
        a_events.extend(a.events());
        a_events.iter().any(|e| matches!(e, NetEvent::JoinRequest { peer, .. } if *peer == kb))
    }));
    let welcome = Welcome { line: [9; 32], epoch_ns: 0, rate_hz: 60, members: vec![], agree: vec![] };
    assert!(a.command(Command::Admit { peer: kb, welcome }));
    let mut welcomed = false;
    assert!(until(|| {
        while let Some(l) = b.poll_lifecycle() {
            welcomed |= matches!(l, Lifecycle::Welcomed { peer, .. } if peer == ka);
        }
        welcomed
    }));

    let t0 = Instant::now();
    b.send(ka, Lane::Action, Bytes::from_static(b"hello"));
    a.send(kb, Lane::Bulk, Bytes::from(vec![7u8; 200_000]));
    let send_time = t0.elapsed();
    assert!(send_time < Duration::from_millis(5), "send blocked for {send_time:?}");
    let (mut got_a, mut got_b) = (None, None);
    assert!(until(|| {
        got_a = got_a.take().or_else(|| a.poll());
        got_b = got_b.take().or_else(|| b.poll());
        got_a.is_some() && got_b.is_some()
    }));
    assert_eq!(got_a.unwrap().bytes, Bytes::from_static(b"hello"));
    assert_eq!(got_b.unwrap().bytes.len(), 200_000);
    // Statistics arrive with the other outputs: `events` (called every tick) takes them in.
    assert!(until(|| {
        a.events();
        a.link(kb).state == pe_seams::LinkState::Up && a.link(kb).srtt_ns > 0
    }));
    a.stop().unwrap();
    b.stop().unwrap();
}

#[test]
fn events_never_drop_a_received_message_and_poll_serves_them_in_order() {
    // Reliable lanes deliver each message once (5.1), so `events`, which drains the whole queue, must keep the
    // messages it passes for `poll` (engine-architect's review of net/app-net f7d31a31).
    let start = Instant::now();
    let clock: HostClock = Arc::new(move || HostTime(start.elapsed().as_nanos() as i64));
    let (ia, ib) = (Identity::from_seed(&[1; 32]), Identity::from_seed(&[2; 32]));
    let (ka, kb) = (MemberKey(ia.public()), MemberKey(ib.public()));
    let sa = UdpSocket::bind("127.0.0.1:0").unwrap();
    let sb = UdpSocket::bind("127.0.0.1:0").unwrap();
    let addr_a = sa.local_addr().unwrap();
    let ca = Core::new(Config::new(hello(), true), ia, [1; 32]).unwrap();
    let cb = Core::new(Config::new(hello(), false), ib, [2; 32]).unwrap();
    let mut a = NetHandle::spawn(ca, sa, clock.clone(), 4096).unwrap();
    let mut b = NetHandle::spawn(cb, sb, clock, 4096).unwrap();
    let invite = Invite { id: [5; 8], key: ka.0, expiry_unix_s: u64::MAX, addrs: vec![addr_a], pin: None };
    assert!(b.command(Command::Dial(invite)));
    let mut a_events = Vec::new();
    assert!(until(|| {
        a_events.extend(a.events());
        a_events.iter().any(|e| matches!(e, NetEvent::JoinRequest { peer, .. } if *peer == kb))
    }));
    let welcome = Welcome { line: [9; 32], epoch_ns: 0, rate_hz: 60, members: vec![], agree: vec![] };
    assert!(a.command(Command::Admit { peer: kb, welcome }));
    let mut welcomed = false;
    assert!(until(|| {
        while let Some(l) = b.poll_lifecycle() {
            welcomed |= matches!(l, Lifecycle::Welcomed { peer, .. } if peer == ka);
        }
        welcomed
    }));

    const N: u32 = 50;
    for i in 0..N {
        b.send(ka, Lane::Action, Bytes::from(i.to_le_bytes().to_vec()));
    }
    // `events` runs before every `poll` round, as in the simulation's step 1, so before the fix it drained (and
    // dropped) whatever had arrived while the test slept.
    let mut seen = Vec::new();
    assert!(until(|| {
        a.events();
        while let Some(r) = a.poll() {
            assert_eq!((r.from, r.lane), (kb, Lane::Action));
            seen.push(u32::from_le_bytes(r.bytes[..].try_into().unwrap()));
        }
        seen.len() >= N as usize
    }));
    assert_eq!(seen, (0..N).collect::<Vec<_>>(), "every message, once, in order");
    a.stop().unwrap();
    b.stop().unwrap();
}

#[test]
fn the_control_end_dials_and_admits_while_the_handles_stay_transports() {
    // app.md 3: the handle goes into the simulation as its `Transport`, and the app keeps a control end for
    // dialling, admission and the lifecycle's outcomes, on a thread of its own.
    fn is_send<T: Send>(_: &T) {}
    let start = Instant::now();
    let clock: HostClock = Arc::new(move || HostTime(start.elapsed().as_nanos() as i64));
    let (ia, ib) = (Identity::from_seed(&[1; 32]), Identity::from_seed(&[2; 32]));
    let (ka, kb) = (MemberKey(ia.public()), MemberKey(ib.public()));
    let sa = UdpSocket::bind("127.0.0.1:0").unwrap();
    let sb = UdpSocket::bind("127.0.0.1:0").unwrap();
    let addr_a = sa.local_addr().unwrap();
    let ca = Core::new(Config::new(hello(), true), ia, [1; 32]).unwrap();
    let cb = Core::new(Config::new(hello(), false), ib, [2; 32]).unwrap();
    let mut a = NetHandle::spawn(ca, sa, clock.clone(), 4096).unwrap();
    let mut b = NetHandle::spawn(cb, sb, clock, 4096).unwrap();
    let mut ctl_a = a.control().unwrap().expect("the first control() splits it off");
    let mut ctl_b = b.control().unwrap().unwrap();
    assert!(a.control().unwrap().is_none(), "only one control end");
    is_send(&ctl_b);

    // The joiner's control end dials and waits for the welcome on another thread.
    let invite = Invite { id: [5; 8], key: ka.0, expiry_unix_s: u64::MAX, addrs: vec![addr_a], pin: None };
    let joiner = std::thread::spawn(move || {
        assert!(ctl_b.command(Command::Dial(invite)));
        let mut welcomed = false;
        assert!(until(|| {
            while let Some(l) = ctl_b.poll_lifecycle() {
                welcomed |= matches!(l, Lifecycle::Welcomed { peer, .. } if peer == ka);
            }
            welcomed
        }));
        ctl_b
    });
    // The member sees the join request as a handle event (what the simulation drains) and admits through its
    // control end.
    let mut a_events = Vec::new();
    assert!(until(|| {
        a_events.extend(a.events());
        a_events.iter().any(|e| matches!(e, NetEvent::JoinRequest { peer, .. } if *peer == kb))
    }));
    let welcome = Welcome { line: [9; 32], epoch_ns: 0, rate_hz: 60, members: vec![], agree: vec![] };
    assert!(ctl_a.command(Command::Admit { peer: kb, welcome }));
    let ctl_b = joiner.join().unwrap();
    assert!(b.poll_lifecycle().is_none(), "outcomes go to the control end once it is split off");

    // Dropping a control end discards later outcomes but stops nothing: messages still flow both ways.
    drop(ctl_a);
    drop(ctl_b);
    b.send(ka, Lane::Action, Bytes::from_static(b"after"));
    a.send(kb, Lane::Protocol, Bytes::from_static(b"back"));
    let (mut got_a, mut got_b) = (None, None);
    assert!(until(|| {
        got_a = got_a.take().or_else(|| a.poll());
        got_b = got_b.take().or_else(|| b.poll());
        got_a.is_some() && got_b.is_some()
    }));
    assert_eq!(got_a.unwrap().bytes, Bytes::from_static(b"after"));
    assert_eq!(got_b.unwrap().bytes, Bytes::from_static(b"back"));
    a.stop().unwrap();
    b.stop().unwrap();
}

#[test]
fn unpolled_lifecycle_outcomes_are_capped_and_a_welcome_survives() {
    // @design-reviewer: `Refused` and `Fault` are caused by whoever connects, so a control end nobody polls must
    // not grow without bound. The joiner (queue 4: 4 in the channel, at most 4 held) is refused N times without
    // polling, then welcomed: it keeps at most 8 outcomes, the welcome among them, and counts the rest.
    const N: usize = 16;
    let start = Instant::now();
    let clock: HostClock = Arc::new(move || HostTime(start.elapsed().as_nanos() as i64));
    let (ia, ib) = (Identity::from_seed(&[1; 32]), Identity::from_seed(&[2; 32]));
    let (ka, kb) = (MemberKey(ia.public()), MemberKey(ib.public()));
    let sa = UdpSocket::bind("127.0.0.1:0").unwrap();
    let sb = UdpSocket::bind("127.0.0.1:0").unwrap();
    let addr_a = sa.local_addr().unwrap();
    let ca = Core::new(Config::new(hello(), true), ia, [1; 32]).unwrap();
    let cb = Core::new(Config::new(hello(), false), ib, [2; 32]).unwrap();
    let mut a = NetHandle::spawn(ca, sa, clock.clone(), 4096).unwrap();
    let mut b = NetHandle::spawn(cb, sb, clock, 4).unwrap();
    let mut ctl_a = a.control().unwrap().unwrap();
    let mut ctl_b = b.control().unwrap().unwrap();
    let invite = Invite { id: [5; 8], key: ka.0, expiry_unix_s: u64::MAX, addrs: vec![addr_a], pin: None };
    for round in 0..=N {
        assert!(ctl_b.command(Command::Dial(invite.clone())), "round {round}");
        assert!(
            until(|| a.events().iter().any(|e| matches!(e, NetEvent::JoinRequest { peer, .. } if *peer == kb))),
            "round {round}: no join request"
        );
        let c = if round < N {
            Command::Refuse { peer: kb, refusal: Refusal { code: CloseCode::NotAdmitted, detail: "not yet".into() } }
        } else {
            Command::Admit {
                peer: kb,
                welcome: Welcome { line: [9; 32], epoch_ns: 0, rate_hz: 60, members: vec![], agree: vec![] },
            }
        };
        assert!(ctl_a.command(c));
        // A's own outcome for this round, then time for the close to reach the joiner before it dials again.
        let mut done = false;
        assert!(until(|| {
            while let Some(l) = ctl_a.poll_lifecycle() {
                done |= matches!(l, Lifecycle::Refused { by_me: true, .. });
            }
            done || round == N
        }));
        std::thread::sleep(Duration::from_millis(150));
    }
    assert!(
        until(|| {
            a.events();
            a.link(kb).state == pe_seams::LinkState::Up
        }),
        "the admitted joiner's link is up"
    );
    // Held outcomes move into the channel as it empties, at the thread's next turn (within MAX_WAIT).
    let mut got = Vec::new();
    let quiet = Instant::now() + Duration::from_millis(600);
    while Instant::now() < quiet {
        while let Some(l) = ctl_b.poll_lifecycle() {
            got.push(l);
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    eprintln!("kept {} outcomes, dropped {}", got.len(), ctl_b.lifecycle_dropped());
    let welcomed = got.iter().any(|l| matches!(l, Lifecycle::Welcomed { peer, .. } if *peer == ka));
    let refused = got.iter().filter(|l| matches!(l, Lifecycle::Refused { .. })).count();
    assert!(welcomed, "the welcome survives the cap: {got:?}");
    assert!(got.len() <= 8, "{} outcomes kept", got.len());
    assert!(
        ctl_b.lifecycle_dropped() as usize + refused >= N,
        "dropped {} + kept {refused}",
        ctl_b.lifecycle_dropped()
    );
    assert!(ctl_b.lifecycle_dropped() >= (N - 7) as u64);
    a.stop().unwrap();
    b.stop().unwrap();
}

#[test]
fn the_arrival_callback_fires_when_messages_reach_the_handle() {
    // app.md 3 step 7: the network thread wakes the simulation thread when something arrives.
    use std::sync::atomic::{AtomicUsize, Ordering};
    let start = Instant::now();
    let clock: HostClock = Arc::new(move || HostTime(start.elapsed().as_nanos() as i64));
    let (ia, ib) = (Identity::from_seed(&[1; 32]), Identity::from_seed(&[2; 32]));
    let (ka, kb) = (MemberKey(ia.public()), MemberKey(ib.public()));
    let sa = UdpSocket::bind("127.0.0.1:0").unwrap();
    let sb = UdpSocket::bind("127.0.0.1:0").unwrap();
    let addr_a = sa.local_addr().unwrap();
    let ca = Core::new(Config::new(hello(), true), ia, [1; 32]).unwrap();
    let cb = Core::new(Config::new(hello(), false), ib, [2; 32]).unwrap();
    let woken = Arc::new(AtomicUsize::new(0));
    let w = woken.clone();
    let arrival: Arrival = Arc::new(move || {
        w.fetch_add(1, Ordering::Relaxed);
    });
    let mut a = NetHandle::spawn_with(ca, sa, clock.clone(), 4096, Some(arrival)).unwrap();
    let mut b = NetHandle::spawn(cb, sb, clock, 4096).unwrap();
    let invite = Invite { id: [5; 8], key: ka.0, expiry_unix_s: u64::MAX, addrs: vec![addr_a], pin: None };
    assert!(b.command(Command::Dial(invite)));
    // The join request is an event on a's handle: it wakes.
    assert!(until(|| woken.load(Ordering::Relaxed) > 0));
    let mut joined = false;
    assert!(until(|| {
        joined |= a.events().iter().any(|e| matches!(e, NetEvent::JoinRequest { peer, .. } if *peer == kb));
        joined
    }));
    let welcome = Welcome { line: [9; 32], epoch_ns: 0, rate_hz: 60, members: vec![], agree: vec![] };
    assert!(a.command(Command::Admit { peer: kb, welcome }));
    assert!(until(|| matches!(b.poll_lifecycle(), Some(Lifecycle::Welcomed { .. }))));
    // A message wakes it again.
    let before = woken.load(Ordering::Relaxed);
    b.send(ka, Lane::Action, Bytes::from_static(b"wake"));
    assert!(until(|| woken.load(Ordering::Relaxed) > before));
    assert!(until(|| a.poll().is_some_and(|r| r.bytes == Bytes::from_static(b"wake"))));
    a.stop().unwrap();
    b.stop().unwrap();
}

/// A and B over loopback with B welcomed and the link up at A: (A, B, B's key, A's events so far).
fn linked() -> (NetHandle, NetHandle, MemberKey, Vec<NetEvent>) {
    let start = Instant::now();
    let clock: HostClock = Arc::new(move || HostTime(start.elapsed().as_nanos() as i64));
    let (ia, ib) = (Identity::from_seed(&[1; 32]), Identity::from_seed(&[2; 32]));
    let (ka, kb) = (MemberKey(ia.public()), MemberKey(ib.public()));
    let sa = UdpSocket::bind("127.0.0.1:0").unwrap();
    let sb = UdpSocket::bind("127.0.0.1:0").unwrap();
    let addr_a = sa.local_addr().unwrap();
    let ca = Core::new(Config::new(hello(), true), ia, [1; 32]).unwrap();
    let cb = Core::new(Config::new(hello(), false), ib, [2; 32]).unwrap();
    let mut a = NetHandle::spawn(ca, sa, clock.clone(), 4096).unwrap();
    let mut b = NetHandle::spawn(cb, sb, clock, 4096).unwrap();
    let invite = Invite { id: [5; 8], key: ka.0, expiry_unix_s: u64::MAX, addrs: vec![addr_a], pin: None };
    assert!(b.command(Command::Dial(invite)));
    let mut a_events = Vec::new();
    assert!(until(|| {
        a_events.extend(a.events());
        a_events.iter().any(|e| matches!(e, NetEvent::JoinRequest { peer, .. } if *peer == kb))
    }));
    let welcome = Welcome { line: [9; 32], epoch_ns: 0, rate_hz: 60, members: vec![], agree: vec![] };
    assert!(a.command(Command::Admit { peer: kb, welcome }));
    assert!(until(|| {
        a_events.extend(a.events());
        a_events.iter().any(|e| matches!(e, NetEvent::LinkUp { peer } if *peer == kb))
    }));
    (a, b, kb, a_events)
}

/// Whether A sees B's link go down within `until`'s 5 s, printing how long it took.
fn sees_down(a: &mut NetHandle, kb: MemberKey, mut events: Vec<NetEvent>, how: &str) -> bool {
    let stopped = Instant::now();
    let down = until(|| {
        events.extend(a.events());
        events.iter().any(|e| matches!(e, NetEvent::LinkDown { peer } if *peer == kb))
    });
    println!("{how}: A saw B's link go down: {down}, after {:?}", stopped.elapsed());
    down
}

/// A stopping endpoint closes its connections (code 0, `Left`), so its peer sees the link go down at once instead of
/// after `T_idle` (30 s). The first two-machine run: B's exit left A waiting for the idle timeout (@coordinator-opus).
#[test]
fn a_stopped_endpoint_closes_its_connections_and_the_peer_sees_the_link_go_down() {
    let (mut a, b, kb, events) = linked();
    b.stop().unwrap();
    assert!(sees_down(&mut a, kb, events, "stop"), "A saw no LinkDown within 5 s of B's stop");
    a.stop().unwrap();
}

/// The same when the handle is dropped without `stop`, as pe-app's session drops its transport.
#[test]
fn a_dropped_handle_closes_its_connections_and_the_peer_sees_the_link_go_down() {
    let (mut a, b, kb, events) = linked();
    drop(b);
    assert!(sees_down(&mut a, kb, events, "drop"), "A saw no LinkDown within 5 s of dropping B's handle");
    a.stop().unwrap();
}
