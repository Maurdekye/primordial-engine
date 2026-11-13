//! The network thread over real UDP sockets on loopback (transport.md 7): a joiner dials an invite, is welcomed,
//! and messages go both ways through the simulation thread's handle, which never blocks.

use std::net::UdpSocket;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use pe_net::core::{Config, Core, Lifecycle};
use pe_net::lifecycle::{Hello, Welcome};
use pe_net::thread::{Command, HostClock, NetHandle};
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

    let invite = Invite { id: [5; 8], key: ka.0, expiry_unix_s: u64::MAX, addrs: vec![addr_a] };
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
