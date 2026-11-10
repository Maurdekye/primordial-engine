//! QUIC through quinn-proto with this crate's provider, in memory (transport.md 2.5): a handshake with
//! raw keys both ways, data on a stream and in datagrams, a wrong pinned key, a tampered packet, replayed
//! packets, 0-RTT refused, and (with `insecure-deterministic-rng`) a run that repeats byte for byte.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::{Bytes, BytesMut};
use pe_net_crypto::identity::{hex, AnyPeerKey, Identity, PeerId, PinnedKey};
use pe_net_crypto::quinn::{Cids, QuicClientConfig, QuicServerConfig, ResetKey, TokenKey};
use pe_net_crypto::Random;
use quinn_proto::{ClientConfig, Connection, ConnectionHandle, DatagramEvent, Dir, Endpoint, EndpointConfig, Event, ServerConfig, TimeSource};

const ALPN: &[u8] = b"pe/0";

fn tls_server(id: &Identity, rng: &'static dyn Random) -> rustls::ServerConfig {
    let p = pe_net_crypto::provider_with(rng);
    let ck = id.certified_key(&p).unwrap();
    let mut c = rustls::ServerConfig::builder_with_provider(p)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .unwrap()
        .with_client_cert_verifier(Arc::new(AnyPeerKey))
        .with_cert_resolver(Arc::new(rustls::server::AlwaysResolvesServerRawPublicKeys::new(ck)));
    c.alpn_protocols = vec![ALPN.to_vec()];
    c
}

fn tls_client(id: &Identity, pin: &PeerId, rng: &'static dyn Random) -> rustls::ClientConfig {
    let p = pe_net_crypto::provider_with(rng);
    let ck = id.certified_key(&p).unwrap();
    let mut c = rustls::ClientConfig::builder_with_provider(p)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .unwrap()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(PinnedKey::new(pin).unwrap()))
        .with_client_cert_resolver(Arc::new(rustls::client::AlwaysResolvesClientRawPublicKeys::new(ck)));
    c.alpn_protocols = vec![ALPN.to_vec()];
    c
}

/// Address-validation tokens carry a wall-clock time (quinn-proto's `TimeSource`); the harness fixes it so a
/// seeded run repeats exactly (transport.md 8).
struct FixedTime;

impl TimeSource for FixedTime {
    fn now(&self) -> std::time::SystemTime {
        std::time::UNIX_EPOCH + Duration::from_secs(1_800_000_000)
    }
}

struct Peer {
    ep: Endpoint,
    addr: SocketAddr,
    conn: Option<(ConnectionHandle, Connection)>,
    out: Vec<Vec<u8>>,
    events: Vec<Event>,
}

impl Peer {
    fn new(rng: &'static dyn Random, server: Option<ServerConfig>, addr: &str, seed: u8) -> Self {
        let mut cfg = EndpointConfig::new(Arc::new(ResetKey::new(rng).unwrap()));
        cfg.cid_generator(move || Box::new(Cids::new(rng)));
        let ep = Endpoint::new(Arc::new(cfg), server.map(Arc::new), true, Some([seed; 32]));
        Self { ep, addr: addr.parse().unwrap(), conn: None, out: Vec::new(), events: Vec::new() }
    }

    fn recv(&mut self, now: Instant, from: SocketAddr, data: &[u8]) {
        let mut buf = Vec::new();
        match self.ep.handle(now, from, None, None, BytesMut::from(data), &mut buf) {
            Some(DatagramEvent::NewConnection(inc)) => {
                if self.conn.is_none() {
                    if let Ok(c) = self.ep.accept(inc, now, &mut buf, None) {
                        self.conn = Some(c);
                    }
                }
            }
            Some(DatagramEvent::ConnectionEvent(h, ev)) => {
                if let Some((ch, c)) = self.conn.as_mut() {
                    if *ch == h {
                        c.handle_event(ev);
                    }
                }
            }
            Some(DatagramEvent::Response(t)) => self.out.push(buf[..t.size].to_vec()),
            None => {}
        }
    }

    fn pump(&mut self, now: Instant) {
        let Peer { ep, conn, out, events, .. } = self;
        let Some((h, c)) = conn.as_mut() else { return };
        if c.poll_timeout().is_some_and(|t| t <= now) {
            c.handle_timeout(now);
        }
        loop {
            while let Some(ev) = c.poll_endpoint_events() {
                if let Some(back) = ep.handle_event(*h, ev) {
                    c.handle_event(back);
                }
            }
            let mut buf = Vec::new();
            match c.poll_transmit(now, 1, &mut buf) {
                Some(t) => out.push(buf[..t.size].to_vec()),
                None => break,
            }
        }
        while let Some(e) = c.poll() {
            events.push(e);
        }
    }

    fn conn(&mut self) -> &mut Connection {
        &mut self.conn.as_mut().unwrap().1
    }

    fn connected(&self) -> bool {
        self.events.iter().any(|e| matches!(e, Event::Connected))
    }

    fn lost(&self) -> Option<String> {
        self.events.iter().find_map(|e| match e {
            Event::ConnectionLost { reason } => Some(reason.to_string()),
            _ => None,
        })
    }
}

/// Which way a datagram goes, for a filter.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Way {
    ClientToServer,
    ServerToClient,
}

/// Steps both peers 1 ms at a time; every datagram goes through `filter`, which may change, drop or
/// repeat it. Stops when `done` holds, or after `limit` of virtual time.
fn run(c: &mut Peer, s: &mut Peer, now: &mut Instant, filter: &mut dyn FnMut(Way, Vec<u8>) -> Vec<Vec<u8>>, done: &dyn Fn(&mut Peer, &mut Peer) -> bool, limit: Duration) -> bool {
    let end = *now + limit;
    while *now < end {
        c.pump(*now);
        s.pump(*now);
        for d in std::mem::take(&mut c.out) {
            for d in filter(Way::ClientToServer, d) {
                s.recv(*now, c.addr, &d);
            }
        }
        for d in std::mem::take(&mut s.out) {
            for d in filter(Way::ServerToClient, d) {
                c.recv(*now, s.addr, &d);
            }
        }
        c.pump(*now);
        s.pump(*now);
        if done(c, s) {
            return true;
        }
        *now += Duration::from_millis(1);
    }
    false
}

fn pass(_: Way, d: Vec<u8>) -> Vec<Vec<u8>> {
    vec![d]
}

struct Setup {
    client: Peer,
    server: Peer,
    now: Instant,
    client_id: PeerId,
    server_id: PeerId,
}

fn setup(rng: &'static dyn Random, pin_right_key: bool) -> Setup {
    let sid = Identity::from_seed(&[1; 32]);
    let cid = Identity::from_seed(&[2; 32]);
    let pin = if pin_right_key { sid.public() } else { Identity::from_seed(&[3; 32]).public() };
    let mut scfg = ServerConfig::new(Arc::new(QuicServerConfig::new(tls_server(&sid, rng)).unwrap()), Arc::new(TokenKey::new(rng).unwrap()));
    scfg.time_source(Arc::new(FixedTime));
    let server = Peer::new(rng, Some(scfg), "127.0.0.1:4433", 11);
    let mut client = Peer::new(rng, None, "127.0.0.1:5000", 22);
    let now = Instant::now();
    let mut ccfg = ClientConfig::new(Arc::new(QuicClientConfig::new(tls_client(&cid, &pin, rng)).unwrap()));
    ccfg.initial_dst_cid_provider(Cids::initial_dcid(rng));
    let conn = client.ep.connect(now, ccfg, server.addr, "pe").unwrap();
    client.conn = Some(conn);
    Setup { client, server, now, client_id: cid.public(), server_id: sid.public() }
}

fn peer_key(c: &Connection) -> Option<PeerId> {
    let certs = c.crypto_session().peer_identity()?.downcast::<Vec<rustls::pki_types::CertificateDer<'static>>>().ok()?;
    pe_net_crypto::identity::peer_id_of(certs.first()?.as_ref())
}

/// Sends `msg` on a new unidirectional stream from the client and returns what the server read.
fn stream_once(t: &mut Setup, filter: &mut dyn FnMut(Way, Vec<u8>) -> Vec<Vec<u8>>, msg: &[u8]) -> Vec<u8> {
    let id = t.client.conn().streams().open(Dir::Uni).unwrap();
    t.client.conn().send_stream(id).write(msg).unwrap();
    t.client.conn().send_stream(id).finish().unwrap();
    let got = std::cell::RefCell::new(Vec::new());
    let finished = std::cell::Cell::new(false);
    let done = |_: &mut Peer, s: &mut Peer| {
        if let Some(sid) = s.conn().streams().accept(Dir::Uni) {
            let mut rs = s.conn().recv_stream(sid);
            if let Ok(mut chunks) = rs.read(true) {
                loop {
                    match chunks.next(usize::MAX) {
                        Ok(Some(ch)) => got.borrow_mut().extend_from_slice(&ch.bytes),
                        Ok(None) => {
                            finished.set(true);
                            break;
                        }
                        Err(_) => break,
                    }
                }
                let _ = chunks.finalize();
            }
        }
        finished.get()
    };
    assert!(run(&mut t.client, &mut t.server, &mut t.now, filter, &done, Duration::from_secs(10)), "stream not delivered");
    got.into_inner()
}

#[test]
fn handshake_raw_keys_stream_and_datagram() {
    let mut t = setup(pe_net_crypto::random::os(), true);
    assert!(run(&mut t.client, &mut t.server, &mut t.now, &mut pass, &|c, s| c.connected() && s.connected(), Duration::from_secs(5)));
    // Each side proved the key the other expected (RFC 7250 raw keys, both ways).
    assert_eq!(peer_key(t.client.conn()), Some(t.server_id));
    assert_eq!(peer_key(t.server.conn()), Some(t.client_id));
    assert_eq!(stream_once(&mut t, &mut pass, b"hello over a stream"), b"hello over a stream");
    t.client.conn().datagrams().send(Bytes::from_static(b"a datagram"), false).unwrap();
    let got = std::cell::RefCell::new(None);
    assert!(run(&mut t.client, &mut t.server, &mut t.now, &mut pass, &|_, s| {
        if let Some(d) = s.conn().datagrams().recv() {
            *got.borrow_mut() = Some(d);
        }
        got.borrow().is_some()
    }, Duration::from_secs(5)));
    assert_eq!(got.into_inner().unwrap(), Bytes::from_static(b"a datagram"));
    println!("client {} <-> server {}", hex(&t.client_id[..4]), hex(&t.server_id[..4]));
}

#[test]
fn wrong_pinned_key_fails_the_handshake() {
    let mut t = setup(pe_net_crypto::random::os(), false);
    let ended = run(&mut t.client, &mut t.server, &mut t.now, &mut pass, &|c, _| c.lost().is_some(), Duration::from_secs(5));
    assert!(ended, "the client must give up");
    assert!(!t.client.connected());
    let reason = t.client.lost().unwrap();
    assert!(reason.contains("pinned key"), "{reason}");
}

#[test]
fn tampered_packets_are_dropped_and_the_data_still_arrives() {
    let mut t = setup(pe_net_crypto::random::os(), true);
    assert!(run(&mut t.client, &mut t.server, &mut t.now, &mut pass, &|c, s| c.connected() && s.connected(), Duration::from_secs(5)));
    // Flip one payload byte in the next few short-header packets from the client.
    let mut tampered = 0;
    let mut flip = |w: Way, mut d: Vec<u8>| {
        if w == Way::ClientToServer && d[0] & 0x80 == 0 && tampered < 3 && d.len() > 40 {
            let i = d.len() - 20;
            d[i] ^= 0x40;
            tampered += 1;
        }
        vec![d]
    };
    let msg: Vec<u8> = (0..3000u32).map(|i| i as u8).collect();
    assert_eq!(stream_once(&mut t, &mut flip, &msg), msg);
    assert_eq!(tampered, 3);
    assert!(t.client.lost().is_none() && t.server.lost().is_none(), "a tampered packet must not close the connection");
}

#[test]
fn replayed_packets_deliver_nothing_twice() {
    let mut t = setup(pe_net_crypto::random::os(), true);
    // Every datagram, both ways, handshake included, arrives twice.
    let mut twice = |_: Way, d: Vec<u8>| vec![d.clone(), d];
    assert!(run(&mut t.client, &mut t.server, &mut t.now, &mut twice, &|c, s| c.connected() && s.connected(), Duration::from_secs(5)));
    let msg: Vec<u8> = (0..5000u32).map(|i| (i * 7) as u8).collect();
    assert_eq!(stream_once(&mut t, &mut twice, &msg), msg);
    // And a datagram replayed: delivered once.
    t.client.conn().datagrams().send(Bytes::from_static(b"once"), false).unwrap();
    let count = std::cell::Cell::new(0);
    run(&mut t.client, &mut t.server, &mut t.now, &mut twice, &|_, s| {
        while s.conn().datagrams().recv().is_some() {
            count.set(count.get() + 1);
        }
        false
    }, Duration::from_millis(200));
    assert_eq!(count.get(), 1);
    assert!(t.client.lost().is_none() && t.server.lost().is_none());
}

#[test]
fn zero_rtt_is_refused_at_config_time() {
    let rng = pe_net_crypto::random::os();
    let mut c = tls_client(&Identity::from_seed(&[2; 32]), &Identity::from_seed(&[1; 32]).public(), rng);
    c.enable_early_data = true;
    assert!(QuicClientConfig::new(c).is_err());
    let mut s = tls_server(&Identity::from_seed(&[1; 32]), rng);
    s.max_early_data_size = u32::MAX;
    assert!(QuicServerConfig::new(s).is_err());
}

#[cfg(feature = "insecure-deterministic-rng")]
#[test]
fn a_seeded_run_repeats_byte_for_byte() {
    fn transcript(seed: u8) -> Vec<Vec<u8>> {
        let rng = pe_net_crypto::random::seeded([seed; 32]);
        let mut t = setup(rng, true);
        let mut seen = Vec::new();
        let mut record = |_: Way, d: Vec<u8>| {
            seen.push(d.clone());
            vec![d]
        };
        assert!(run(&mut t.client, &mut t.server, &mut t.now, &mut record, &|c, s| c.connected() && s.connected(), Duration::from_secs(5)));
        seen
    }
    let a = transcript(7);
    let b = transcript(7);
    if a != b {
        let i = (0..a.len().min(b.len())).find(|&i| a[i] != b[i]).unwrap_or(a.len().min(b.len()));
        let off = if i < a.len().min(b.len()) { (0..a[i].len().min(b[i].len())).find(|&k| a[i][k] != b[i][k]) } else { None };
        let ndiff = |x: &[u8], y: &[u8]| x.iter().zip(y).filter(|(p, q)| p != q).count();
        eprintln!("datagram {i}: differing bytes {}; unmasked long header (version, DCID len, DCID, SCID len, SCID):
  a {:02x?}
  b {:02x?}", ndiff(&a[i], &b[i]), &a[i][1..30], &b[i][1..30]);
        // The TLS ClientHello alone, from two providers seeded alike.
        let hello = |seed: u8| {
            let rng = pe_net_crypto::random::seeded([seed; 32]);
            let cfg = Arc::new(tls_client(&Identity::from_seed(&[2; 32]), &Identity::from_seed(&[1; 32]).public(), rng));
            let mut c = rustls::quic::ClientConnection::new(cfg, rustls::quic::Version::V1, rustls::pki_types::ServerName::try_from("pe").unwrap(), vec![]).unwrap();
            let mut out = Vec::new();
            c.write_hs(&mut out);
            out
        };
        let (h1, h2) = (hello(7), hello(7));
        eprintln!("ClientHello alone equal: {}; first diff at {:?}", h1 == h2, (0..h1.len().min(h2.len())).find(|&k| h1[k] != h2[k]));
        panic!("transcripts differ: {} vs {} datagrams; first at datagram {i} (len {} vs {}), byte {:?}, first byte {:02x?}",
            a.len(), b.len(), a.get(i).map_or(0, |d| d.len()), b.get(i).map_or(0, |d| d.len()), off, a.get(i).map(|d| d[0]));
    }
    assert_ne!(a, transcript(8));
}
