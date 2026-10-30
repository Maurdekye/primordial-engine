//! Phase-1 spike (c): two peer processes, no server.
//!
//! Each process runs the same deterministic pile scene on a real-time fixed-step clock
//! (requirement 2), issues its own scripted interactions stamped with the sim frame they apply
//! at, and sends them directly to the other peer over UDP (requirement 1). Remote interactions
//! that arrive for past frames trigger rollback + re-simulation (requirement 3). Conflicts on
//! one object resolve by "soonest wins" with a seeded tie-break (requirement 5). Periodic and
//! interaction-triggered sync checks compare state hashes; on mismatch the peers exchange
//! per-frame (state hash, input digest) ranges, locate the earliest divergent frame, swap the
//! interactions they are missing and roll back (requirement 6).
//!
//! Every datagram is Ed25519-signed; a peer's key is pinned from its first Hello.
//!
//! usage: pe-spike-p2p --id N --bind ADDR --peer ADDR [--delay-ms D] [--jitter-ms J]
//!                     [--loss-pct P] [--drop-seq S] [--frames F] [--out FILE]

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::net::{SocketAddr, UdpSocket};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use pe_sim_core::*;

const SEED: u64 = 0x5052_494D_4F52_4449; // must match the physics spike's reference run
const DT_US: u64 = 16_667; // 60 Hz
const ROLLBACK_WINDOW: u64 = 120;
const SYNC_EVERY: u64 = 30;
const SYNC_DELAY: u64 = 45; // compare frame c once both peers are past c + SYNC_DELAY
const MAGIC: u16 = 0x5045;

// ---- wire format -----------------------------------------------------------------------------

#[derive(Debug, Clone)]
enum Msg {
    Hello { pubkey: [u8; 32] },
    Ping { t_send_us: u64 },
    Pong { t_ping_send_us: u64, t_recv_us: u64 },
    Start { epoch_us: u64 },
    StartAck,
    Input(Interaction),
    Ack { seq: u32 },
    SyncCheck { frame: u64, state_hash: u64 },
    RangeRequest { from: u64, to: u64 },
    RangeReply { from: u64, entries: Vec<(u64, u64)> },
    InputsRequest { from: u64, to: u64 },
    InputsReply { inputs: Vec<Interaction> },
}

struct W(Vec<u8>);
impl W {
    fn u8(&mut self, v: u8) { self.0.push(v) }
    fn u16(&mut self, v: u16) { self.0.extend_from_slice(&v.to_le_bytes()) }
    fn u32(&mut self, v: u32) { self.0.extend_from_slice(&v.to_le_bytes()) }
    fn u64(&mut self, v: u64) { self.0.extend_from_slice(&v.to_le_bytes()) }
    fn f32(&mut self, v: f32) { self.u32(v.to_bits()) }
    fn input(&mut self, i: &Interaction) {
        self.u64(i.frame);
        self.u32(i.peer);
        self.u32(i.seq);
        self.u32(i.body);
        for c in i.impulse { self.f32(c) }
    }
}

struct R<'a>(&'a [u8], usize);
impl R<'_> {
    fn take(&mut self, n: usize) -> Option<&[u8]> {
        let s = self.0.get(self.1..self.1 + n)?;
        self.1 += n;
        Some(s)
    }
    fn u8(&mut self) -> Option<u8> { Some(self.take(1)?[0]) }
    fn u16(&mut self) -> Option<u16> { Some(u16::from_le_bytes(self.take(2)?.try_into().ok()?)) }
    fn u32(&mut self) -> Option<u32> { Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?)) }
    fn u64(&mut self) -> Option<u64> { Some(u64::from_le_bytes(self.take(8)?.try_into().ok()?)) }
    fn f32(&mut self) -> Option<f32> { Some(f32::from_bits(self.u32()?)) }
    fn input(&mut self) -> Option<Interaction> {
        Some(Interaction {
            frame: self.u64()?,
            peer: self.u32()?,
            seq: self.u32()?,
            body: self.u32()?,
            impulse: [self.f32()?, self.f32()?, self.f32()?],
        })
    }
}

fn encode(sender: u32, m: &Msg, key: &SigningKey) -> Vec<u8> {
    let mut w = W(Vec::with_capacity(128));
    w.u16(MAGIC);
    w.u32(sender);
    match m {
        Msg::Hello { pubkey } => { w.u8(1); w.0.extend_from_slice(pubkey) }
        Msg::Ping { t_send_us } => { w.u8(2); w.u64(*t_send_us) }
        Msg::Pong { t_ping_send_us, t_recv_us } => { w.u8(3); w.u64(*t_ping_send_us); w.u64(*t_recv_us) }
        Msg::Start { epoch_us } => { w.u8(4); w.u64(*epoch_us) }
        Msg::StartAck => w.u8(5),
        Msg::Input(i) => { w.u8(6); w.input(i) }
        Msg::Ack { seq } => { w.u8(7); w.u32(*seq) }
        Msg::SyncCheck { frame, state_hash } => { w.u8(8); w.u64(*frame); w.u64(*state_hash) }
        Msg::RangeRequest { from, to } => { w.u8(9); w.u64(*from); w.u64(*to) }
        Msg::RangeReply { from, entries } => {
            w.u8(10);
            w.u64(*from);
            w.u32(entries.len() as u32);
            for (s, d) in entries { w.u64(*s); w.u64(*d) }
        }
        Msg::InputsRequest { from, to } => { w.u8(11); w.u64(*from); w.u64(*to) }
        Msg::InputsReply { inputs } => {
            w.u8(12);
            w.u32(inputs.len() as u32);
            for i in inputs { w.input(i) }
        }
    }
    let sig = key.sign(&w.0);
    w.0.extend_from_slice(&sig.to_bytes());
    w.0
}

/// Returns (sender, message, signed bytes, signature) without verifying.
fn decode(buf: &[u8]) -> Option<(u32, Msg, &[u8], Signature)> {
    if buf.len() < 2 + 4 + 1 + 64 {
        return None;
    }
    let (body, sig) = buf.split_at(buf.len() - 64);
    let sig = Signature::from_bytes(sig.try_into().ok()?);
    let mut r = R(body, 0);
    if r.u16()? != MAGIC {
        return None;
    }
    let sender = r.u32()?;
    let m = match r.u8()? {
        1 => Msg::Hello { pubkey: r.take(32)?.try_into().ok()? },
        2 => Msg::Ping { t_send_us: r.u64()? },
        3 => Msg::Pong { t_ping_send_us: r.u64()?, t_recv_us: r.u64()? },
        4 => Msg::Start { epoch_us: r.u64()? },
        5 => Msg::StartAck,
        6 => Msg::Input(r.input()?),
        7 => Msg::Ack { seq: r.u32()? },
        8 => Msg::SyncCheck { frame: r.u64()?, state_hash: r.u64()? },
        9 => Msg::RangeRequest { from: r.u64()?, to: r.u64()? },
        10 => {
            let from = r.u64()?;
            let n = r.u32()? as usize;
            let mut entries = Vec::with_capacity(n.min(1024));
            for _ in 0..n { entries.push((r.u64()?, r.u64()?)) }
            Msg::RangeReply { from, entries }
        }
        11 => Msg::InputsRequest { from: r.u64()?, to: r.u64()? },
        12 => {
            let n = r.u32()? as usize;
            let mut inputs = Vec::with_capacity(n.min(1024));
            for _ in 0..n { inputs.push(r.input()?) }
            Msg::InputsReply { inputs }
        }
        _ => return None,
    };
    Some((sender, m, body, sig))
}

fn unix_us() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_micros() as u64
}

// ---- peer ------------------------------------------------------------------------------------

struct Cfg {
    id: u32,
    bind: SocketAddr,
    peer: SocketAddr,
    delay_ms: u64,
    jitter_ms: u64,
    loss_pct: u64,
    drop_seq: Option<u32>,
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
        drop_seq: get("--drop-seq").map(|v| v.parse().unwrap()),
        frames: get("--frames").map_or(600, |v| v.parse().unwrap()),
        out: get("--out").unwrap_or_else(|| format!("p2p-peer{id}.txt")),
    }
}

struct Peer {
    cfg: Cfg,
    sock: UdpSocket,
    key: SigningKey,
    remote_key: Option<VerifyingKey>,
    sim: RollbackSim,
    epoch_us: Option<u64>,
    start_acked: bool,
    pongs: Vec<(u64, i64)>, // (rtt, offset) samples
    outq: Vec<(Instant, Vec<u8>)>,
    rng: u64,
    my_inputs: Vec<Interaction>,
    next_input: usize,
    unacked: BTreeMap<u32, (Interaction, Instant)>,
    sent_checks: BTreeSet<u64>,
    extra_checks: BTreeSet<u64>,
    remote_checks: BTreeMap<u64, u64>,
    evaluated: BTreeSet<u64>,
    checks_ok: u64,
    checks_bad: u64,
    reconciles: u64,
    nondeterminism: u64,
    rejected_sig: u64,
    sent: u64,
    dropped: u64,
    t0: Instant,
}

impl Peer {
    fn log(&self, s: String) {
        let f = self.sim.frame();
        println!("[peer {} t={:6.3}s f={:4}] {s}", self.cfg.id, self.t0.elapsed().as_secs_f64(), f);
    }

    fn rand(&mut self) -> u64 {
        self.rng = splitmix64(self.rng);
        self.rng
    }

    fn send(&mut self, m: &Msg) {
        let bytes = encode(self.cfg.id, m, &self.key);
        if self.cfg.loss_pct > 0 && self.rand() % 100 < self.cfg.loss_pct {
            self.dropped += 1;
            return;
        }
        let jitter = if self.cfg.jitter_ms > 0 { self.rand() % (self.cfg.jitter_ms + 1) } else { 0 };
        let at = Instant::now() + Duration::from_millis(self.cfg.delay_ms + jitter);
        self.outq.push((at, bytes));
        self.sent += 1;
    }

    fn flush(&mut self) {
        let now = Instant::now();
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

    fn handle(&mut self, buf: &[u8]) {
        let Some((sender, m, body, sig)) = decode(buf) else { return };
        if sender == self.cfg.id {
            return;
        }
        // Pin the remote key from its (self-signed) Hello; verify everything else against it.
        if let Msg::Hello { pubkey } = &m {
            if self.remote_key.is_none() {
                if let Ok(vk) = VerifyingKey::from_bytes(pubkey) {
                    if vk.verify(body, &sig).is_ok() {
                        self.log(format!("pinned peer {sender} key {}", hex(&pubkey[..8])));
                        self.remote_key = Some(vk);
                    }
                }
            }
        }
        let Some(vk) = self.remote_key else { return };
        if vk.verify(body, &sig).is_err() {
            self.rejected_sig += 1;
            return;
        }
        match m {
            Msg::Hello { .. } => {}
            Msg::Ping { t_send_us } => self.send(&Msg::Pong { t_ping_send_us: t_send_us, t_recv_us: unix_us() }),
            Msg::Pong { t_ping_send_us, t_recv_us } => {
                // NTP-style: offset = remote - local, assuming symmetric paths.
                let t4 = unix_us();
                let rtt = t4.saturating_sub(t_ping_send_us);
                let offset = t_recv_us as i64 - (t_ping_send_us as i64 + rtt as i64 / 2);
                self.pongs.push((rtt, offset));
            }
            Msg::Start { epoch_us } => {
                // Epoch is expressed in the proposer's clock: convert with the best (lowest-RTT) offset.
                let off = self.best_offset();
                let local = (epoch_us as i64 - off) as u64;
                if self.epoch_us.is_none() {
                    self.log(format!("session start received; clock offset estimate {off} us"));
                }
                self.epoch_us = Some(local);
                self.send(&Msg::StartAck);
            }
            Msg::StartAck => self.start_acked = true,
            Msg::Input(i) => {
                self.send(&Msg::Ack { seq: i.seq });
                let now = self.sim.frame();
                match self.sim.add_input(i) {
                    Ok(true) => self.log(format!(
                        "remote input peer {} seq {} for frame {} arrived {} frames late -> rollback",
                        i.peer, i.seq, i.frame, now - i.frame
                    )),
                    Ok(false) => {}
                    Err(InputError::Duplicate) => {}
                    Err(e) => self.log(format!("remote input refused: {e:?}")),
                }
                self.extra_checks.insert(i.frame + 15);
            }
            Msg::Ack { seq } => {
                self.unacked.remove(&seq);
            }
            Msg::SyncCheck { frame, state_hash } => {
                self.remote_checks.insert(frame, state_hash);
            }
            Msg::RangeRequest { from, to } => {
                let from = from.max(self.sim.oldest_restorable());
                let entries = (from..=to)
                    .filter_map(|f| Some((*self.sim.hashes.get(&f)?, self.sim.input_digest(f))))
                    .collect();
                self.send(&Msg::RangeReply { from, entries });
            }
            Msg::RangeReply { from, entries } => {
                let mut earliest = None;
                for (k, (sh, dig)) in entries.iter().enumerate() {
                    let f = from + k as u64;
                    if self.sim.hashes.get(&f) != Some(sh) {
                        earliest = Some((f, self.sim.input_digest(f) != *dig));
                        break;
                    }
                }
                match earliest {
                    None => self.log("range re-check: histories agree now (stale check)".into()),
                    Some((f, true)) => {
                        self.log(format!("earliest divergent frame {f}: interaction logs differ -> exchanging inputs"));
                        self.reconciles += 1;
                        let to = from + entries.len() as u64;
                        self.send(&Msg::InputsRequest { from: f, to });
                        let mine = self.sim.inputs_in(f, to);
                        self.send(&Msg::InputsReply { inputs: mine });
                    }
                    Some((f, false)) => {
                        self.nondeterminism += 1;
                        self.log(format!("earliest divergent frame {f}: SAME inputs, different state -> nondeterminism (needs state transfer)"));
                    }
                }
            }
            Msg::InputsRequest { from, to } => {
                let inputs = self.sim.inputs_in(from, to);
                self.send(&Msg::InputsReply { inputs });
            }
            Msg::InputsReply { inputs } => {
                for i in inputs {
                    if let Ok(true) = self.sim.add_input(i) {
                        self.log(format!(
                            "reconciled missing input peer {} seq {} frame {} -> rollback",
                            i.peer, i.seq, i.frame
                        ));
                    }
                }
            }
        }
    }

    fn best_offset(&self) -> i64 {
        self.pongs.iter().min_by_key(|(rtt, _)| *rtt).map_or(0, |(_, o)| *o)
    }

    fn issue_local_inputs(&mut self) {
        while self.next_input < self.my_inputs.len() && self.my_inputs[self.next_input].frame == self.sim.frame() {
            let i = self.my_inputs[self.next_input];
            self.next_input += 1;
            self.sim.add_input(i).unwrap();
            self.extra_checks.insert(i.frame + 15);
            if Some(i.seq) == self.cfg.drop_seq {
                self.log(format!("FAULT INJECTION: input seq {} (frame {}) is never transmitted", i.seq, i.frame));
                continue;
            }
            self.send(&Msg::Input(i));
            self.unacked.insert(i.seq, (i, Instant::now()));
        }
    }

    fn sync_checks(&mut self) {
        let now = self.sim.frame();
        // Send our hash for every check frame that is now old enough.
        let mut due: Vec<u64> = (1..)
            .map(|k| k * SYNC_EVERY)
            .take_while(|c| c + SYNC_DELAY <= now)
            .filter(|c| !self.sent_checks.contains(c))
            .collect();
        due.extend(self.extra_checks.iter().copied().filter(|c| c + SYNC_DELAY <= now && !self.sent_checks.contains(c)));
        for c in due {
            self.sent_checks.insert(c);
            if let Some(&h) = self.sim.hashes.get(&c) {
                self.send(&Msg::SyncCheck { frame: c, state_hash: h });
            }
        }
        // Compare the remote hashes we hold for frames we are past.
        let ready: Vec<(u64, u64)> = self
            .remote_checks
            .iter()
            .filter(|(c, _)| **c + SYNC_DELAY <= now && !self.evaluated.contains(c))
            .map(|(c, h)| (*c, *h))
            .collect();
        for (c, h) in ready {
            self.evaluated.insert(c);
            match self.sim.hashes.get(&c) {
                Some(mine) if *mine == h => self.checks_ok += 1,
                Some(mine) => {
                    self.checks_bad += 1;
                    self.log(format!("SYNC MISMATCH at frame {c}: mine {mine:016x} theirs {h:016x} -> requesting hash range"));
                    let from = c.saturating_sub(60).max(self.sim.oldest_restorable());
                    self.send(&Msg::RangeRequest { from, to: c });
                }
                None => {}
            }
        }
    }
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn main() {
    let cfg = parse();
    let sock = UdpSocket::bind(cfg.bind).expect("bind");
    sock.set_nonblocking(true).unwrap();
    // Spike-only: key derived from the peer id. Real peers generate and persist a keypair.
    let mut seed = [0u8; 32];
    seed[..8].copy_from_slice(&splitmix64(0xC0FFEE ^ cfg.id as u64).to_le_bytes());
    let key = SigningKey::from_bytes(&seed);
    let my_inputs: Vec<Interaction> = scripted_inputs().into_iter().filter(|i| i.peer == cfg.id).collect();
    let mut p = Peer {
        rng: splitmix64(cfg.id as u64 * 7919),
        sim: RollbackSim::new(GameState::new(build_pile_scene(8, 4), SEED), ROLLBACK_WINDOW),
        cfg,
        sock,
        key,
        remote_key: None,
        epoch_us: None,
        start_acked: false,
        pongs: Vec::new(),
        outq: Vec::new(),
        my_inputs,
        next_input: 0,
        unacked: BTreeMap::new(),
        sent_checks: BTreeSet::new(),
        extra_checks: BTreeSet::new(),
        remote_checks: BTreeMap::new(),
        evaluated: BTreeSet::new(),
        checks_ok: 0,
        checks_bad: 0,
        reconciles: 0,
        nondeterminism: 0,
        rejected_sig: 0,
        sent: 0,
        dropped: 0,
        t0: Instant::now(),
    };
    p.log(format!(
        "bound {} -> peer {} (delay {} ms, jitter {} ms, loss {}%, drop_seq {:?})",
        p.cfg.bind, p.cfg.peer, p.cfg.delay_ms, p.cfg.jitter_ms, p.cfg.loss_pct, p.cfg.drop_seq
    ));

    let end = p.cfg.frames;
    let mut last_hello = Instant::now() - Duration::from_secs(1);
    let mut last_ping = last_hello;
    let mut last_start = last_hello;
    let mut done_at: Option<Instant> = None;
    let mut buf = [0u8; 65536];
    let mut announced = false;
    loop {
        p.flush();
        while let Ok((n, from)) = p.sock.recv_from(&mut buf) {
            if from == p.cfg.peer {
                let b = buf[..n].to_vec();
                p.handle(&b);
            }
        }
        let now = Instant::now();
        if now.duration_since(last_hello) > Duration::from_millis(100) && (p.remote_key.is_none() || p.epoch_us.is_none()) {
            last_hello = now;
            let pk = p.key.verifying_key().to_bytes();
            p.send(&Msg::Hello { pubkey: pk });
        }
        if p.remote_key.is_some() && p.pongs.len() < 20 && now.duration_since(last_ping) > Duration::from_millis(25) {
            last_ping = now;
            p.send(&Msg::Ping { t_send_us: unix_us() });
        }
        // Peer 1 proposes the session epoch (a session parameter, not an ongoing authority).
        if p.cfg.id == 1 && p.pongs.len() >= 10 && !p.start_acked && now.duration_since(last_start) > Duration::from_millis(100) {
            last_start = now;
            let epoch = *p.epoch_us.get_or_insert(unix_us() + 500_000);
            let rtt = p.pongs.iter().map(|x| x.0).min().unwrap();
            if !announced {
                announced = true;
                p.log(format!("proposing session epoch (+500 ms); min RTT {rtt} us, offset estimate {} us", p.best_offset()));
            }
            p.send(&Msg::Start { epoch_us: epoch });
        }

        if let Some(epoch) = p.epoch_us {
            let t = unix_us();
            if t >= epoch {
                let target = ((t - epoch) / DT_US).min(end + 90);
                // Rollback (if pending) happens at the start of advance_to.
                p.sim.advance_to(p.sim.frame());
                while p.sim.frame() < target {
                    p.issue_local_inputs();
                    let f = p.sim.frame();
                    p.sim.advance_to(f + 1);
                }
                p.sync_checks();
                // Reliability: resend unacked inputs every 100 ms.
                let resend: Vec<Interaction> = p
                    .unacked
                    .values()
                    .filter(|(_, at)| at.elapsed() > Duration::from_millis(100))
                    .map(|(i, _)| *i)
                    .collect();
                for i in resend {
                    p.send(&Msg::Input(i));
                    p.unacked.insert(i.seq, (i, Instant::now()));
                }
                if p.sim.frame() >= end + 90 && done_at.is_none() {
                    done_at = Some(Instant::now());
                }
            }
        }
        if let Some(d) = done_at {
            // Keep answering for a while so the other peer can finish its checks.
            if d.elapsed() > Duration::from_millis(1500) {
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(1));
    }

    let mut f = std::fs::File::create(&p.cfg.out).expect("out file");
    for fr in 0..end {
        writeln!(f, "{fr} {:016x}", p.sim.hashes[&fr]).unwrap();
    }
    let digest = fnv_u64s((0..end).map(|fr| p.sim.hashes[&fr]));
    let s = p.sim.stats;
    p.log(format!(
        "DONE frames 0..{end}: digest {digest:016x}; rollbacks {} (max depth {}, resimulated {}), sync checks ok {} mismatched {}, reconciles {}, nondeterminism {}, bad signatures {}, datagrams sent {} dropped {}",
        s.rollbacks, s.max_depth, s.resimulated_frames, p.checks_ok, p.checks_bad, p.reconciles, p.nondeterminism, p.rejected_sig, p.sent, p.dropped
    ));
    for fr in [120u64, 125, 126] {
        if let Some(v) = p.sim.verdicts.get(&fr) {
            for (i, verdict) in v {
                p.log(format!("verdict frame {fr}: peer {} seq {} object {} -> {:?}", i.peer, i.seq, i.body, verdict));
            }
        }
    }
}
