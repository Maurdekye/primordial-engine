//! Datagrams on the wire. Every datagram is signed by its sender (Ed25519), and every entry inside
//! carries its author's own signature, so an entry relayed by a third member (fetch repair,
//! equivocation proofs) stays verifiable. Keys are pinned from the session file; nothing is trusted
//! on first use. Noise encryption and NAT traversal come in phase 3.
//!
//! ```text
//! datagram = "PE" | version u8 | session u64 | sender u32 | count u16 | item* | signature[64]
//! item     = 1 entry: len u16 | entry bytes | author signature[64]
//!          | 2 fetch: len u16 | fetch bytes
//!          | 3 ping: t u64 | 4 pong: t u64, r u64 | 5 start: epoch u64 | 6 start-ack | 7 ready
//! ```
//! Signatures cover a domain tag and the session id, so nothing replays across sessions or kinds.

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use pe_sim_core::session::{decode_msg, encode_msg, Entry, MemberId, Msg};

const MAGIC: &[u8; 2] = b"PE";
const VERSION: u8 = 1;
const HEADER: usize = 2 + 1 + 8 + 4 + 2;
pub const SIG: usize = 64;
/// Largest datagram we send (bytes, before IP/UDP headers).
pub const MTU: usize = 1200;

#[derive(Clone, Debug, PartialEq)]
pub enum Item {
    /// An entry with its author's signature.
    Entry(Entry, [u8; SIG]),
    Fetch(Msg),
    /// Clock sync: my send time; the reply carries it back with the replier's time.
    Ping(u64),
    Pong(u64, u64),
    /// The session epoch in the sender's clock (the leader's).
    Start(u64),
    StartAck,
    /// "My clock offset to the leader is measured; start when you like."
    Ready,
}

pub fn entry_signing_bytes(session: u64, e: &Entry) -> Vec<u8> {
    let mut b = b"pe-entry".to_vec();
    b.extend_from_slice(&session.to_le_bytes());
    b.extend_from_slice(&encode_msg(&Msg::Entry(e.clone())));
    b
}

pub fn sign_entry(key: &SigningKey, session: u64, e: &Entry) -> [u8; SIG] {
    key.sign(&entry_signing_bytes(session, e)).to_bytes()
}

pub fn verify_entry(key: &VerifyingKey, session: u64, e: &Entry, sig: &[u8; SIG]) -> bool {
    key.verify(&entry_signing_bytes(session, e), &Signature::from_bytes(sig)).is_ok()
}

fn item_bytes(i: &Item, out: &mut Vec<u8>) {
    match i {
        Item::Entry(e, sig) => {
            let b = encode_msg(&Msg::Entry(e.clone()));
            out.push(1);
            out.extend_from_slice(&(b.len() as u16).to_le_bytes());
            out.extend_from_slice(&b);
            out.extend_from_slice(sig);
        }
        Item::Fetch(m) => {
            let b = encode_msg(m);
            out.push(2);
            out.extend_from_slice(&(b.len() as u16).to_le_bytes());
            out.extend_from_slice(&b);
        }
        Item::Ping(t) => {
            out.push(3);
            out.extend_from_slice(&t.to_le_bytes());
        }
        Item::Pong(t, r) => {
            out.push(4);
            out.extend_from_slice(&t.to_le_bytes());
            out.extend_from_slice(&r.to_le_bytes());
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

/// Pack items into as few datagrams of at most `MTU` bytes as possible, in order.
pub fn pack(session: u64, sender: MemberId, items: &[Item], key: &SigningKey) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < items.len() {
        let mut body = Vec::with_capacity(MTU);
        body.extend_from_slice(MAGIC);
        body.push(VERSION);
        body.extend_from_slice(&session.to_le_bytes());
        body.extend_from_slice(&sender.to_le_bytes());
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
    /// An entry whose author signature does not verify (or whose author is unknown).
    BadEntrySignature,
}

/// Verify and decode a datagram. `key_of` maps a member to its pinned key. Entries whose author
/// signature fails make the whole datagram invalid: its sender relayed a forgery. `held` says whether
/// the receiver already holds an entry with this id (same id, same content): its signature was
/// verified on first receipt and is not checked again. Repair resends held entries often, and an
/// Ed25519 verification costs far more than the rest of the datagram.
pub fn unpack(
    session: u64,
    buf: &[u8],
    key_of: impl Fn(MemberId) -> Option<VerifyingKey>,
    held: impl Fn(&Entry) -> bool,
) -> Result<(MemberId, Vec<Item>), Reject> {
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
    let n = u16::from_le_bytes(body[15..17].try_into().unwrap());
    let mut p = &body[HEADER..];
    let mut items = Vec::with_capacity(n as usize);
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
            1 | 2 => {
                let len = u16::from_le_bytes(take(&mut p, 2)?.try_into().unwrap()) as usize;
                let m = decode_msg(&take(&mut p, len)?).ok_or(Reject::Malformed)?;
                match (kind, m) {
                    (1, Msg::Entry(e)) => {
                        let s: [u8; SIG] = take(&mut p, SIG)?.try_into().unwrap();
                        if !held(&e) {
                            let k = key_of(e.member).ok_or(Reject::BadEntrySignature)?;
                            if !verify_entry(&k, session, &e, &s) {
                                return Err(Reject::BadEntrySignature);
                            }
                        }
                        Item::Entry(e, s)
                    }
                    (2, m @ Msg::Fetch { .. }) => Item::Fetch(m),
                    _ => return Err(Reject::Malformed),
                }
            }
            3 => Item::Ping(u64_(&mut p)?),
            4 => Item::Pong(u64_(&mut p)?, u64_(&mut p)?),
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
    Ok((sender, items))
}

#[cfg(test)]
mod tests {
    use super::*;
    use pe_sim_core::session::Payload;

    fn key(i: u8) -> SigningKey {
        SigningKey::from_bytes(&[i; 32])
    }

    fn entry(member: MemberId, seq: u32) -> Entry {
        Entry { member, seq, prev: 0, stamp: 7, payload: Payload::Touch { obj: 3, impulse: [0.0, 1.0, 0.0], grab: false }, carried: None }
    }

    fn keys(m: MemberId) -> Option<VerifyingKey> {
        (1..=3).contains(&m).then(|| key(m as u8).verifying_key())
    }

    #[test]
    fn roundtrip_and_relay() {
        let e2 = entry(2, 1);
        let s2 = sign_entry(&key(2), 9, &e2);
        let items = vec![Item::Ping(5), Item::Entry(e2.clone(), s2), Item::Fetch(Msg::Fetch { author: 2, from: 1, to: 3 }), Item::Start(77), Item::Ready];
        // Member 1 relays member 2's entry: fine, the author signature travels with it.
        let d = pack(9, 1, &items, &key(1));
        assert_eq!(d.len(), 1);
        let (from, got) = unpack(9, &d[0], keys, |_| false).unwrap();
        assert_eq!((from, got), (1, items));
    }

    #[test]
    fn rejects() {
        let e = entry(2, 1);
        let good = pack(9, 1, &[Item::Entry(e.clone(), sign_entry(&key(2), 9, &e))], &key(1)).remove(0);
        let none = |_: &Entry| false;
        assert_eq!(unpack(10, &good, keys, none), Err(Reject::WrongSession));
        let mut flipped = good.clone();
        flipped[20] ^= 1;
        assert!(unpack(9, &flipped, keys, none).is_err());
        // Signed by the wrong datagram key.
        let imp = pack(9, 1, &[Item::Ping(1)], &key(3)).remove(0);
        assert_eq!(unpack(9, &imp, keys, none), Err(Reject::BadSignature));
        // An entry "by member 2" signed by member 1: a forgery, even inside 1's valid datagram.
        let forged = pack(9, 1, &[Item::Entry(e.clone(), sign_entry(&key(1), 9, &e))], &key(1)).remove(0);
        assert_eq!(unpack(9, &forged, keys, none), Err(Reject::BadEntrySignature));
        // A held entry is not re-verified (its first copy was), so a relayed copy passes.
        assert!(unpack(9, &forged, keys, |x| *x == e).is_ok());
        // An entry signed for another session.
        let other = pack(9, 1, &[Item::Entry(e.clone(), sign_entry(&key(2), 8, &e))], &key(1)).remove(0);
        assert_eq!(unpack(9, &other, keys, none), Err(Reject::BadEntrySignature));
        let unknown = pack(9, 4, &[Item::Ping(1)], &key(4)).remove(0);
        assert_eq!(unpack(9, &unknown, keys, none), Err(Reject::UnknownSender));
        assert_eq!(unpack(9, &good[..10], keys, none), Err(Reject::Malformed));
    }

    #[test]
    fn packing_respects_the_mtu() {
        let items: Vec<Item> = (1..=40).map(|s| {
            let e = entry(2, s);
            Item::Entry(e.clone(), sign_entry(&key(2), 9, &e))
        }).collect();
        let d = pack(9, 2, &items, &key(2));
        assert!(d.len() > 1);
        assert!(d.iter().all(|x| x.len() <= MTU));
        let all: Vec<Item> = d.iter().flat_map(|x| unpack(9, x, keys, |_| false).unwrap().1).collect();
        assert_eq!(all, items);
    }
}
