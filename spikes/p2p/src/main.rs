//! Phase-1 spike (c), revised: two peer processes, no server, running the session protocol of
//! `pe_sim_core::session` over real UDP sockets on a real (monotonic) clock.
//!
//! * Every perturbation is an entry carrying its canonical frame and the resulting state of the
//!   touched object (user ruling 09:30Z). Receivers roll back, apply, re-simulate.
//! * Acks carry horizons and holdings; the confirmed frame is computed from acks only (review C1);
//!   missing entries are found by gap detection and fetched (R6 repair).
//! * Every datagram is Ed25519-signed; the peer key is pinned from its self-signed Hello, and an
//!   entry is accepted only from its own author (review L4; production signs each entry).
//! * Acceptance (user, 09:31Z): after re-simulating a remote touch, the receiver logs its copy of
//!   the object at its present frame; scripts/run-p2p.sh compares it with the author's history.
//!
//! usage: pe-spike-p2p --id N --bind ADDR --peer ADDR [--delay-ms D] [--jitter-ms J]
//!                     [--loss-pct P] [--drop-stamp S] [--frames F] [--out FILE]

use std::collections::BTreeMap;
use std::io::Write;
use std::net::{SocketAddr, UdpSocket};
use std::time::Duration;

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use pe_sim_core::session::*;
use pe_sim_core::splitmix64;

const MAGIC: u16 = 0x5046;
const DT_US: u64 = 16_667;

enum Ctl {
    Hello([u8; 32]),
    Ping(u64),
    Pong(u64, u64),
    Start(u64),
    StartAck,
    Session(Msg),
}

fn encode(sender: u32, m: &Ctl, key: &SigningKey) -> Vec<u8> {
    let mut b = Vec::with_capacity(160);
    b.extend_from_slice(&MAGIC.to_le_bytes());
    b.extend_from_slice(&sender.to_le_bytes());
    match m {
        Ctl::Hello(pk) => { b.push(1); b.extend_from_slice(pk) }
        Ctl::Ping(t) => { b.push(2); b.extend_from_slice(&t.to_le_bytes()) }
        Ctl::Pong(t, r) => { b.push(3); b.extend_from_slice(&t.to_le_bytes()); b.extend_from_slice(&r.to_le_bytes()) }
        Ctl::Start(e) => { b.push(4); b.extend_from_slice(&e.to_le_bytes()) }
        Ctl::StartAck => b.push(5),
        Ctl::Session(s) => { b.push(6); b.extend_from_slice(&encode_msg(s)) }
    }
    let sig = key.sign(&b);
    b.extend_from_slice(&sig.to_bytes());
    b
}

fn decode(buf: &[u8]) -> Option<(u32, Ctl, &[u8], Signature)> {
    if buf.len() < 2 + 4 + 1 + 64 {
        return None;
    }
    let (body, sig) = buf.split_at(buf.len() - 64);
    let sig = Signature::from_bytes(sig.try_into().ok()?);
    if u16::from_le_bytes(body[0..2].try_into().ok()?) != MAGIC {
        return None;
    }
    let sender = u32::from_le_bytes(body[2..6].try_into().ok()?);
    let p = &body[7..];
    let u64at = |o: usize| -> Option<u64> { Some(u64::from_le_bytes(p.get(o..o + 8)?.try_into().ok()?)) };
    let m = match body[6] {
        1 => Ctl::Hello(p.get(0..32)?.try_into().ok()?),
        2 => Ctl::Ping(u64at(0)?),
        3 => Ctl::Pong(u64at(0)?, u64at(8)?),
        4 => Ctl::Start(u64at(0)?),
        5 => Ctl::StartAck,
        6 => Ctl::Session(decode_msg(p)?),
        _ => return None,
    };
    Some((sender, m, body, sig))
}

fn touch(obj: u32, x: f32, y: f32, z: f32) -> Payload {
    Payload::Touch { obj, impulse: [x, y, z], grab: false }
}

/// The phase-1 scripted schedule (members 1 and 2), including a same-frame conflict on object 50.
fn schedule(me: u32) -> BTreeMap<u64, Vec<(u32, Payload)>> {
    let all: Vec<(u64, u32, Payload)> = vec![
        (60, 1, touch(10, 0.0, 6.0, 0.0)),
        (90, 2, touch(33, 3.0, 2.0, 0.0)),
        (120, 1, touch(50, 0.0, 8.0, 2.0)),
        (120, 2, touch(50, -4.0, 8.0, 0.0)),
        (125, 2, touch(50, 0.0, 9.0, 0.0)),
        (126, 1, touch(50, 0.0, 3.0, 0.0)),
        (200, 2, touch(77, 5.0, 5.0, 5.0)),
        (260, 1, touch(120, -6.0, 4.0, 1.0)),
        (305, 1, touch(77, 0.0, 4.0, 0.0)),
        (330, 2, touch(5, 0.0, 12.0, 0.0)),
        (400, 1, touch(140, 2.0, 7.0, -3.0)),
        (470, 2, touch(99, -2.0, 6.0, 4.0)),
    ];
    let mut m: BTreeMap<u64, Vec<(u32, Payload)>> = BTreeMap::new();
    for (f, who, p) in all {
        if who == me {
            let obj = match p { Payload::Touch { obj, .. } => obj, _ => 0 };
            m.entry(f).or_default().push((obj, p));
        }
    }
    m
}

struct Cfg {
    id: u32,
    bind: SocketAddr,
    peer: SocketAddr,
    delay_ms: u64,
    jitter_ms: u64,
    loss_pct: u64,
    drop_stamp: Option<u64>,
    frames: u64,
    out: String,
}

fn parse() -> Cfg {
    let a: Vec<String> = std::env::args().collect();
    let get = |k: &str| a.iter().position(|x| x == k).and_then(|i| a.get(i + 1)).cloned();
    let id: u32 = get("--id").expect("--id").parse().unwrap();
    Cfg {
        id,
        bind: get("--bind").expect("--bind").parse().unwrap(),
        peer: get("--peer").expect("--peer").parse().unwrap(),
        delay_ms: get("--delay-ms").map_or(0, |v| v.parse().unwrap()),
        jitter_ms: get("--jitter-ms").map_or(0, |v| v.parse().unwrap()),
        loss_pct: get("--loss-pct").map_or(0, |v| v.parse().unwrap()),
        drop_stamp: get("--drop-stamp").map(|v| v.parse().unwrap()),
        frames: get("--frames").map_or(600, |v| v.parse().unwrap()),
        out: get("--out").unwrap_or_else(|| format!("p2p-peer{id}.txt")),
    }
}

struct Peer {
    cfg: Cfg,
    sock: UdpSocket,
    key: SigningKey,
    remote_key: Option<VerifyingKey>,
    outq: Vec<(u64, Vec<u8>)>,
    rng: u64,
    sent: u64,
    dropped: u64,
    bad_sig: u64,
    forged: u64,
}

impl Peer {
    fn rand(&mut self) -> u64 {
        self.rng = splitmix64(self.rng);
        self.rng
    }
    fn send(&mut self, m: &Ctl) {
        let bytes = encode(self.cfg.id, m, &self.key);
        if self.cfg.loss_pct > 0 && self.rand() % 100 < self.cfg.loss_pct {
            self.dropped += 1;
            return;
        }
        let jitter = if self.cfg.jitter_ms > 0 { self.rand() % (self.cfg.jitter_ms * 1000 + 1) } else { 0 };
        self.outq.push((now_us() + self.cfg.delay_ms * 1000 + jitter, bytes));
        self.sent += 1;
    }
    fn flush(&mut self) {
        let now = now_us();
        let mut i = 0;
        while i < self.outq.len() {
            if self.outq[i].0 <= now {
                let (_, b) = self.outq.swap_remove(i);
                let _ = self.sock.send_to(&b, self.cfg.peer);
            } else {
                i += 1;
            }
        }
    }
}

fn main() {
    let cfg = parse();
    let sock = UdpSocket::bind(cfg.bind).expect("bind");
    sock.set_nonblocking(true).unwrap();
    // Spike-only: key derived from the peer id. Real peers generate and persist a keypair.
    let mut seed = [0u8; 32];
    seed[..8].copy_from_slice(&splitmix64(0xC0FFEE ^ cfg.id as u64).to_le_bytes());
    let key = SigningKey::from_bytes(&seed);
    let me = cfg.id;
    let other = if me == 1 { 2 } else { 1 };
    let mut sim = WorldSim::new(1, me, &[1, 2], SceneKind::Pile { side: 8, layers: 4 }, SimConfig::default());
    let mut plan = schedule(me);
    let end = cfg.frames;
    let log = |s: String, f: u64| println!("[peer {me} t={:7.3}s f={f:4}] {s}", now_us() as f64 / 1e6);
    let mut p = Peer { rng: splitmix64(me as u64 * 7919), cfg, sock, key, remote_key: None, outq: Vec::new(), sent: 0, dropped: 0, bad_sig: 0, forged: 0 };
    log(format!("bound {} -> {} (delay {} ms, jitter {} ms, loss {}%, drop_stamp {:?})", p.cfg.bind, p.cfg.peer, p.cfg.delay_ms, p.cfg.jitter_ms, p.cfg.loss_pct, p.cfg.drop_stamp), 0);

    let mut pongs: Vec<(u64, i64)> = Vec::new();
    let mut epoch: Option<u64> = None;
    let mut start_acked = false;
    let (mut last_hello, mut last_ping, mut last_start, mut next_ack) = (0u64, 0u64, 0u64, 0u64);
    let mut pending_accept: Vec<(EntryId, u32)> = Vec::new();
    let mut accept_lines: Vec<String> = Vec::new();
    let mut done_at: Option<u64> = None;
    let mut buf = [0u8; 65536];
    let mut last_frame = u64::MAX;
    loop {
        p.flush();
        while let Ok((n, from)) = p.sock.recv_from(&mut buf) {
            if from != p.cfg.peer {
                continue;
            }
            let b = buf[..n].to_vec();
            let Some((sender, m, body, sig)) = decode(&b) else { continue };
            if sender != other {
                continue;
            }
            if let Ctl::Hello(pk) = &m {
                if p.remote_key.is_none() {
                    if let Ok(vk) = VerifyingKey::from_bytes(pk) {
                        if vk.verify(body, &sig).is_ok() {
                            p.remote_key = Some(vk);
                        }
                    }
                }
            }
            let Some(vk) = p.remote_key else { continue };
            if vk.verify(body, &sig).is_err() {
                p.bad_sig += 1;
                continue;
            }
            match m {
                Ctl::Hello(_) => {}
                Ctl::Ping(t) => p.send(&Ctl::Pong(t, now_us())),
                Ctl::Pong(t, r) => {
                    let t4 = now_us();
                    let rtt = t4.saturating_sub(t);
                    pongs.push((rtt, r as i64 - (t as i64 + rtt as i64 / 2)));
                }
                Ctl::Start(e) => {
                    let off = pongs.iter().min_by_key(|x| x.0).map_or(0, |x| x.1);
                    if epoch.is_none() {
                        log(format!("session start received; clock offset estimate {off} us"), 0);
                    }
                    epoch = Some((e as i64 - off) as u64);
                    p.send(&Ctl::StartAck);
                }
                Ctl::StartAck => start_acked = true,
                Ctl::Session(msg) => match msg {
                    Msg::Entry(e) => {
                        if e.member != sender {
                            // Only the author's own entries travel in a two-member session (L4).
                            p.forged += 1;
                            continue;
                        }
                        let carried = e.carried.map(|(o, _)| o);
                        let id = e.id();
                        let f = sim.frame;
                        let stamp = e.stamp;
                        let late = stamp < f;
                        let votes = sim.insert(e, sim.now);
                        for v in votes {
                            p.send(&Ctl::Session(v));
                        }
                        if late {
                            log(format!("remote entry for frame {stamp} arrived {} frames late -> rollback", f - stamp), f);
                        }
                        if let Some(obj) = carried {
                            pending_accept.push((id, obj));
                        }
                    }
                    Msg::Ack(a) => {
                        let reqs = sim.on_ack(a);
                        for (_, fm) in sim.take_fetches(reqs, now_us() / 1000) {
                            log(format!("gap detected from acks -> fetching {fm:?}"), sim.frame);
                            p.send(&Ctl::Session(fm));
                        }
                    }
                    Msg::Fetch { author, from, to } => {
                        let reply = sim.on_fetch(author, from, to);
                        log(format!("fetch request for member {author} seq {from}..={to}: replying with {} entries", reply.len()), sim.frame);
                        for e in reply {
                            p.send(&Ctl::Session(e));
                        }
                    }
                    Msg::Hash { frame, hash } => sim.on_hash(other, frame, hash),
                    Msg::Vote { target, seq } => {
                        sim.on_vote(other, target, seq);
                    }
                },
            }
        }
        let now = now_us();
        if now >= last_hello + 100_000 && (p.remote_key.is_none() || epoch.is_none()) {
            last_hello = now;
            let pk = p.key.verifying_key().to_bytes();
            p.send(&Ctl::Hello(pk));
        }
        if p.remote_key.is_some() && pongs.len() < 20 && now >= last_ping + 25_000 {
            last_ping = now;
            p.send(&Ctl::Ping(now_us()));
        }
        // Peer 1 proposes the session epoch (a session parameter, not an ongoing authority).
        if me == 1 && pongs.len() >= 10 && !start_acked && now >= last_start + 100_000 {
            last_start = now;
            let e = *epoch.get_or_insert(now_us() + 500_000);
            p.send(&Ctl::Start(e));
        }
        if let Some(ep) = epoch {
            let t = now_us();
            if t >= ep {
                let clock_frame = ((t - ep) / DT_US).min(end + 120);
                sim.now = clock_frame;
                if clock_frame != last_frame || !sim.caught_up() {
                    last_frame = clock_frame;
                    sim.advance(clock_frame);
                    if sim.caught_up() && sim.frame == clock_frame {
                        // Acceptance checks for remote touches whose frames are now simulated.
                        let mut keep = Vec::new();
                        for (id, obj) in pending_accept.drain(..) {
                            match sim.effective_frame(id) {
                                Some(ef) if sim.frame > ef => {
                                    let fr = sim.frame - 1;
                                    if let Some(h) = sim.obj_hist.get(&(fr, obj)) {
                                        accept_lines.push(format!("ACCEPT {fr} {obj} {h:016x}"));
                                    }
                                }
                                Some(_) => keep.push((id, obj)),
                                None => {}
                            }
                        }
                        pending_accept = keep;
                        if let Some(list) = plan.remove(&clock_frame) {
                            for (obj, payload) in list {
                                let e = sim.author(payload, Some(obj));
                                if Some(e.stamp) == p.cfg.drop_stamp {
                                    log(format!("FAULT INJECTION: entry stamped {} is never transmitted directly", e.stamp), sim.frame);
                                    continue;
                                }
                                p.send(&Ctl::Session(Msg::Entry(e)));
                            }
                        }
                    }
                }
                if t >= next_ack {
                    next_ack = t + 2 * DT_US;
                    let a = sim.make_ack();
                    let _ = sim.on_ack(a.clone());
                    p.send(&Ctl::Session(Msg::Ack(a)));
                }
                if sim.conf >= end + 60 && done_at.is_none() {
                    done_at = Some(now_us());
                }
            }
        }
        if let Some(d) = done_at {
            // Keep acking for a while so the other peer can confirm too.
            if now_us() > d + 1_500_000 {
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(1));
    }

    let digest_live = digest(&sim.hashes, end);
    let reference = replay(1, SceneKind::Pile { side: 8, layers: 4 }, SimConfig::default(), &[1, 2], &final_log(&sim), end);
    let digest_ref = digest(&reference, end);
    let mut f = std::fs::File::create(&p.cfg.out).expect("out file");
    for fr in 0..end {
        writeln!(f, "{fr} {:016x}", sim.hashes[&fr]).unwrap();
    }
    let mut g = std::fs::File::create(format!("{}.obj", p.cfg.out)).expect("obj file");
    for ((fr, obj), h) in &sim.obj_hist {
        if *fr < end {
            writeln!(g, "OBJ {fr} {obj} {h:016x}").unwrap();
        }
    }
    for l in &accept_lines {
        writeln!(g, "{l}").unwrap();
    }
    let s = sim.stats;
    log(format!(
        "DONE frames 0..{end}: digest {digest_live:016x}; offline replay of my final log {digest_ref:016x} ({}); rollbacks {} (max depth {}, resimulated {}), carried-state overwrites {}, checks ok {} bad {}, confirmed frame {}, datagrams sent {} dropped {}, bad signatures {}, forged entries {}",
        if digest_live == digest_ref { "EQUAL" } else { "DIFFERENT" },
        s.rollbacks, s.max_depth, s.resim_frames, s.overwrites, s.checks_ok, s.checks_bad, sim.conf, p.sent, p.dropped, p.bad_sig, p.forged
    ), sim.frame);
    for e in &sim.events {
        println!("  event {e}");
    }
}
