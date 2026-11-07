//! Datagrams on the wire. Every datagram is signed by its sender (Ed25519) with the key pinned in the
//! session file, so the sender is authenticated; nothing is trusted on first use. Entries carry their
//! author's own signature inside the protocol message (pe-session's `SignedEntry`), and the session
//! core verifies those itself, so an entry relayed by a third member (fetch repair, equivocation
//! proofs) stays checkable. Noise encryption and NAT traversal come in phase 3.
//!
//! ```text
//! datagram = "PE" | version u8 | session u64 | sender u32 | stamp | count u16 | item* | signature[64]
//! stamp    = ts u32 | seq u16 | qrep u16    (R18 link signal, design 10.9; see `Stamp`)
//! item     = 1 protocol message: len u16 | pe-session encoding (an entry includes its signature)
//!          | 3 ping: t u64 | 4 pong: t u64, r u64 | 5 start: epoch u64 | 6 start-ack | 7 ready
//! ```
//! The datagram signature covers a domain tag and the session id, so no datagram replays across
//! sessions; entry signatures are bound to the session's 32-byte genesis id by pe-session.

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use pe_session::session::{decode_msg, encode_msg, MemberId, Msg};

const MAGIC: &[u8; 2] = b"PE";
const VERSION: u8 = 5;
const HEADER: usize = 2 + 1 + 8 + 4 + 8 + 2;
pub const SIG: usize = 64;
/// Largest datagram we send (bytes, before IP/UDP headers).
pub const MTU: usize = 1200;

#[derive(Clone, Debug, PartialEq)]
pub enum Item {
    /// A session protocol message: an entry (with its author's signature), a run of one author's
    /// entries (signed once), or a fetch request or answer. Joins, batches and state transfer
    /// (joiners only) are not part of M1.
    Msg(Msg),
    /// Clock sync (design 10.4): my send time; the reply carries it back with the replier's session
    /// time, its raw local clock (the frequency ratio reads it: no correction moves it) and its applied
    /// frequency correction (f64 bits).
    Ping(u64),
    Pong(u64, u64, u64, u64),
    /// The session epoch in the sender's clock (the leader's).
    Start(u64),
    StartAck,
    /// "My clock offset to the leader is measured; start when you like."
    Ready,
}

/// Per-datagram link fields (R18, design 10.9), under the datagram signature like the rest.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stamp {
    /// The sender's monotonic clock at send time, µs, mod 2^32 (compared with wrapping arithmetic).
    pub ts: u32,
    /// The sender's datagram counter towards this recipient (loss), wrapping.
    pub seq: u16,
    /// The queue delay the sender measured on the recipient's datagrams to it, ms; `QREP_NONE` if none.
    pub qrep: u16,
}

pub const QREP_NONE: u16 = u16::MAX;

fn item_bytes(i: &Item, out: &mut Vec<u8>) {
    match i {
        Item::Msg(m) => {
            let b = encode_msg(m);
            out.push(1);
            out.extend_from_slice(&(b.len() as u16).to_le_bytes());
            out.extend_from_slice(&b);
        }
        Item::Ping(t) => {
            out.push(3);
            out.extend_from_slice(&t.to_le_bytes());
        }
        Item::Pong(t, r, raw, rate) => {
            out.push(4);
            out.extend_from_slice(&t.to_le_bytes());
            out.extend_from_slice(&r.to_le_bytes());
            out.extend_from_slice(&raw.to_le_bytes());
            out.extend_from_slice(&rate.to_le_bytes());
        }
        Item::Start(e) => {
            out.push(5);
            out.extend_from_slice(&e.to_le_bytes());
        }
        Item::StartAck => out.push(6),
        Item::Ready => out.push(7),
    }
}

fn signing_bytes(body: &[u8]) -> Vec<u8> {
    let mut b = b"pe-datagram".to_vec();
    b.extend_from_slice(body);
    b
}

/// Pack items into as few datagrams of at most `MTU` bytes as possible, in order. `stamp` is called
/// once per datagram, as it is built.
pub fn pack(session: u64, sender: MemberId, items: &[Item], key: &SigningKey, mut stamp: impl FnMut() -> Stamp) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < items.len() {
        let mut body = Vec::with_capacity(MTU);
        body.extend_from_slice(MAGIC);
        body.push(VERSION);
        body.extend_from_slice(&session.to_le_bytes());
        body.extend_from_slice(&sender.to_le_bytes());
        let st = stamp();
        body.extend_from_slice(&st.ts.to_le_bytes());
        body.extend_from_slice(&st.seq.to_le_bytes());
        body.extend_from_slice(&st.qrep.to_le_bytes());
        body.extend_from_slice(&0u16.to_le_bytes());
        let mut n: u16 = 0;
        while i < items.len() {
            let mut b = Vec::new();
            item_bytes(&items[i], &mut b);
            // Always at least one item per datagram, even an oversized one.
            if n > 0 && body.len() + b.len() + SIG > MTU {
                break;
            }
            body.extend_from_slice(&b);
            n += 1;
            i += 1;
        }
        body[HEADER - 2..HEADER].copy_from_slice(&n.to_le_bytes());
        let sig = key.sign(&signing_bytes(&body)).to_bytes();
        body.extend_from_slice(&sig);
        out.push(body);
    }
    out
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reject {
    Malformed,
    WrongSession,
    UnknownSender,
    BadSignature,
}

/// Verify and decode a datagram. `key_of` maps a member to its pinned key. Entry signatures are the
/// session core's to check (it skips entries it already holds, which repair resends often).
pub fn unpack(session: u64, buf: &[u8], key_of: impl Fn(MemberId) -> Option<VerifyingKey>) -> Result<(MemberId, Stamp, Vec<Item>), Reject> {
    if buf.len() < HEADER + SIG || &buf[0..2] != MAGIC || buf[2] != VERSION {
        return Err(Reject::Malformed);
    }
    let (body, sig) = buf.split_at(buf.len() - SIG);
    if u64::from_le_bytes(body[3..11].try_into().unwrap()) != session {
        return Err(Reject::WrongSession);
    }
    let sender = u32::from_le_bytes(body[11..15].try_into().unwrap());
    let key = key_of(sender).ok_or(Reject::UnknownSender)?;
    let sig = Signature::from_bytes(sig.try_into().unwrap());
    key.verify(&signing_bytes(body), &sig).map_err(|_| Reject::BadSignature)?;
    let stamp = Stamp {
        ts: u32::from_le_bytes(body[15..19].try_into().unwrap()),
        seq: u16::from_le_bytes(body[19..21].try_into().unwrap()),
        qrep: u16::from_le_bytes(body[21..23].try_into().unwrap()),
    };
    let n = u16::from_le_bytes(body[HEADER - 2..HEADER].try_into().unwrap());
    let mut p = &body[HEADER..];
    // Every item takes at least one byte: never reserve more than the body could hold.
    let mut items = Vec::with_capacity((n as usize).min(body.len() - HEADER));
    let take = |p: &mut &[u8], k: usize| -> Result<Vec<u8>, Reject> {
        if p.len() < k {
            return Err(Reject::Malformed);
        }
        let (a, b) = p.split_at(k);
        *p = b;
        Ok(a.to_vec())
    };
    let u64_ = |p: &mut &[u8]| -> Result<u64, Reject> { Ok(u64::from_le_bytes(take(p, 8)?.try_into().unwrap())) };
    for _ in 0..n {
        let kind = take(&mut p, 1)?[0];
        let item = match kind {
            1 => {
                let len = u16::from_le_bytes(take(&mut p, 2)?.try_into().unwrap()) as usize;
                match decode_msg(&take(&mut p, len)?).ok_or(Reject::Malformed)? {
                    // Msg::Run: an author's consecutive entries signed once, lean (implicit ids) or not (the R18
                    // below-20 item, options 1 and 2); the core rebuilds and verifies them.
                    m @ (Msg::Entry(_) | Msg::Run { .. } | Msg::Fetch { .. } | Msg::FetchValue { .. } | Msg::FetchEntry { .. }) => Item::Msg(m),
                    _ => return Err(Reject::Malformed),
                }
            }
            3 => Item::Ping(u64_(&mut p)?),
            4 => Item::Pong(u64_(&mut p)?, u64_(&mut p)?, u64_(&mut p)?, u64_(&mut p)?),
            5 => Item::Start(u64_(&mut p)?),
            6 => Item::StartAck,
            7 => Item::Ready,
            _ => return Err(Reject::Malformed),
        };
        items.push(item);
    }
    if !p.is_empty() {
        return Err(Reject::Malformed);
    }
    Ok((sender, stamp, items))
}

#[cfg(test)]
mod tests {
    use super::*;
    use pe_session::session::{test_sign, Entry, EntryId, Payload};

    fn key(i: u8) -> SigningKey {
        SigningKey::from_bytes(&[i; 32])
    }

    fn entry(member: MemberId, seq: u32) -> Msg {
        let e = Entry { member, seq, prev: EntryId::NONE, stamp: 7, payload: Payload::Touch { obj: 3, impulse: [0.0, 1.0, 0.0], grab: false }, carried: None };
        Msg::Entry(test_sign(1, e))
    }

    fn keys(m: MemberId) -> Option<VerifyingKey> {
        (1..=3).contains(&m).then(|| key(m as u8).verifying_key())
    }

    #[test]
    fn roundtrip_and_relay() {
        // Member 1 relays member 2's entry: its author signature travels inside the message.
        let items = vec![Item::Ping(5), Item::Msg(entry(2, 1)), Item::Msg(Msg::Fetch { author: 2, from: 1, to: 3, held: 0 }), Item::Start(77), Item::Ready];
        let st = Stamp { ts: 0xDEAD_BEEF, seq: 65535, qrep: 41 };
        let d = pack(9, 1, &items, &key(1), || st);
        assert_eq!(d.len(), 1);
        let (from, got_st, got) = unpack(9, &d[0], keys).unwrap();
        assert_eq!((from, got_st, got), (1, st, items));
    }

    #[test]
    fn rejects() {
        let good = pack(9, 1, &[Item::Msg(entry(2, 1))], &key(1), Stamp::default).remove(0);
        assert_eq!(unpack(10, &good, keys), Err(Reject::WrongSession));
        let mut flipped = good.clone();
        flipped[20] ^= 1;
        assert!(unpack(9, &flipped, keys).is_err());
        // The stamp is signed too: a forged queue report is refused.
        let mut qrep = good.clone();
        qrep[21] ^= 1;
        assert_eq!(unpack(9, &qrep, keys), Err(Reject::BadSignature));
        // Signed by the wrong datagram key.
        let imp = pack(9, 1, &[Item::Ping(1)], &key(3), Stamp::default).remove(0);
        assert_eq!(unpack(9, &imp, keys), Err(Reject::BadSignature));
        let unknown = pack(9, 4, &[Item::Ping(1)], &key(4), Stamp::default).remove(0);
        assert_eq!(unpack(9, &unknown, keys), Err(Reject::UnknownSender));
        assert_eq!(unpack(9, &good[..10], keys), Err(Reject::Malformed));
        // Joins and state transfer are not part of M1.
        let fetch_state = pack(9, 1, &[Item::Msg(Msg::FetchState)], &key(1), Stamp::default).remove(0);
        assert_eq!(unpack(9, &fetch_state, keys), Err(Reject::Malformed));
    }

    #[test]
    fn a_run_passes_the_allowlist() {
        // Options 1 and 2: an author's entries as one run, lean and signed once; the wire takes it as encoded.
        let Msg::Entry(a) = entry(2, 1) else { unreachable!() };
        let Msg::Entry(b) = entry(2, 2) else { unreachable!() };
        let run = Msg::Run { lean: true, heads: true, signed: true, entries: vec![a, b] };
        let d = pack(9, 2, &[Item::Msg(run.clone())], &key(2), Stamp::default).remove(0);
        let (_, _, got) = unpack(9, &d, keys).unwrap();
        assert_eq!(got, vec![Item::Msg(decode_msg(&encode_msg(&run)).unwrap())]);
    }

    #[test]
    fn packing_respects_the_mtu() {
        let items: Vec<Item> = (1..=40).map(|s| Item::Msg(entry(2, s))).collect();
        let mut seq = 0u16;
        let d = pack(9, 2, &items, &key(2), || {
            seq += 1;
            Stamp { ts: 1, seq, qrep: QREP_NONE }
        });
        assert!(d.len() > 1);
        assert!(d.iter().all(|x| x.len() <= MTU));
        let all: Vec<Item> = d.iter().flat_map(|x| unpack(9, x, keys).unwrap().2).collect();
        assert_eq!(all, items);
        // One stamp per datagram, in order.
        let seqs: Vec<u16> = d.iter().map(|x| unpack(9, x, keys).unwrap().1.seq).collect();
        assert_eq!(seqs, (1..=d.len() as u16).collect::<Vec<_>>());
    }
}
