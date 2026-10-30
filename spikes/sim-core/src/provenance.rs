//! R14 per-object provenance chains (design 22.7, D46's option for untrusted partners; phase-2
//! package P2-5). Without them, a route-listed world whose majority is dishonest can certify a
//! departure of any global id: one born elsewhere, live elsewhere, or spent.
//!
//! * **Birth binding.** A global id is `birth world << 32 | index`. Its birth record is the birth
//!   world's genesis object list (the spike has no runtime births; production adds a certified
//!   `Born` fact by the birth world's majority [I]).
//! * **Hops.** Once a source world commits a transfer (the destination's certified acceptance), its
//!   members attest [`fact_committed`]. That certificate is the [`Hop`]: proof that the object
//!   really left `src` for `dest`, having entered `src` by transfer `via` (0: born there). A
//!   departure certificate alone is not enough: a dishonest destination could certify "rejected"
//!   to the source (refund) and still claim the object.
//! * **Chains.** Every `Arrive` carries the hops that brought the object from its birth world into
//!   the source; the source's departure certificate covers the chain's hash, so a relayer cannot
//!   strip or swap it. The destination checks each hop against the epoch certificates of the
//!   world that signed it ([`check_chain`]), whether or not that world is on its route list.
//! * **Last exits.** Each world remembers, per global id, whether the object is here or which
//!   transfer last took it away ([`Seen`]). An arrival whose chain skips that exit is a fork.
//!
//! What remains [I]: a dishonest-majority world can still double-spend an object it legitimately
//! received, into a world that has not seen the object since (both exports cite the same incoming
//! hop, and are a transferable equivocation proof against that world). Only a per-object registry
//! that orders every hop prevents that.

use std::collections::BTreeMap;

use crate::fnv_u64s;
use crate::session::{cert_ok, MemberId};

/// The world an object was born in.
pub fn birth_world(global: u64) -> u32 {
    (global >> 32) as u32
}

/// One completed hop of an object's journey, with the certificate of the world it left.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hop {
    pub src: u32,
    pub dest: u32,
    pub transfer: u64,
    /// The transfer that brought the object into `src` (0: born there).
    pub via: u64,
    /// The membership epoch of `src` whose majority signed `cert`.
    pub epoch: u32,
    /// Signatures over [`fact_committed`] by a strict majority of `src`'s members in `epoch`.
    pub cert: Vec<(MemberId, u64)>,
}

/// "World `src` committed transfer `transfer` of object `global` to world `dest`; the object had
/// entered `src` by transfer `via`."
pub fn fact_committed(src: u32, transfer: u64, global: u64, dest: u32, via: u64) -> u64 {
    fnv_u64s([3, src as u64, transfer, global, dest as u64, via])
}

/// Hash of a chain, certificates included, so a departure certificate pins the exact chain.
pub fn chain_hash(chain: &[Hop]) -> u64 {
    let mut v = vec![chain.len() as u64];
    for h in chain {
        v.extend([h.src as u64, h.dest as u64, h.transfer, h.via, h.epoch as u64, h.cert.len() as u64]);
        for (m, s) in &h.cert {
            v.extend([*m as u64, *s]);
        }
    }
    fnv_u64s(v)
}

/// What P2-5 needs from P2-4's epoch certificates. Production: a world id commits to its genesis
/// document, and each membership change is certified by the previous epoch's majority, so anyone
/// can verify any world's epoch chain [I].
pub trait EpochCerts {
    /// Members of `world` in `epoch`.
    fn members(&self, world: u32, epoch: u32) -> Option<&[MemberId]>;
    /// How many objects `world` was born with: its birth records.
    fn genesis_objects(&self, world: u32) -> Option<u64>;
    /// `cert` holds signatures over `fact` by a strict majority of `world`'s members in `epoch`.
    fn cert_ok(&self, world: u32, epoch: u32, fact: u64, cert: &[(MemberId, u64)]) -> bool {
        self.members(world, epoch).is_some_and(|m| cert_ok(m, world, fact, cert))
    }
}

/// Static stub of [`EpochCerts`]: every world stays in epoch 0, its genesis membership. It knows
/// every world in the simulation, standing in for self-certifying world ids.
#[derive(Clone, Debug, Default)]
pub struct StaticEpochs {
    /// world -> (genesis members, genesis object count)
    pub worlds: BTreeMap<u32, (Vec<MemberId>, u64)>,
}

impl EpochCerts for StaticEpochs {
    fn members(&self, world: u32, epoch: u32) -> Option<&[MemberId]> {
        (epoch == 0).then(|| self.worlds.get(&world).map(|(m, _)| m.as_slice())).flatten()
    }
    fn genesis_objects(&self, world: u32) -> Option<u64> {
        self.worlds.get(&world).map(|(_, n)| *n)
    }
}

/// Why a chain does not prove an object's way into its source world.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChainError {
    /// The global id is not a birth record of its birth world.
    NoBirth,
    /// Hop `i` does not continue from where the chain had the object.
    Link(usize),
    /// Hop `i`'s certificate is not a majority of its world's epoch.
    Cert(usize),
    /// The chain does not end with the object entering the source by `via`.
    End,
}

/// Check that `chain` takes object `global` from its birth to world `src`, entering it by `via`.
pub fn check_chain(book: &dyn EpochCerts, global: u64, src: u32, via: u64, chain: &[Hop]) -> Result<(), ChainError> {
    let born = birth_world(global);
    if !book.genesis_objects(born).is_some_and(|n| (global & 0xFFFF_FFFF) < n) {
        return Err(ChainError::NoBirth);
    }
    let (mut at, mut by) = (born, 0u64);
    for (i, h) in chain.iter().enumerate() {
        if h.src != at || h.via != by || h.dest == h.src {
            return Err(ChainError::Link(i));
        }
        if !book.cert_ok(h.src, h.epoch, fact_committed(h.src, h.transfer, global, h.dest, h.via), &h.cert) {
            return Err(ChainError::Cert(i));
        }
        (at, by) = (h.dest, h.transfer);
    }
    if at != src || by != via {
        return Err(ChainError::End);
    }
    Ok(())
}

/// What a world knows of one global id (part of its rollback state).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Seen {
    /// The object is here, live or in escrow. It entered by `via` (0: born here) from world `from`,
    /// where it had entered by `from_via`. `chain` is its verified provenance up to `from`; once
    /// `settled`, it also holds the hop into this world, and the object may leave again.
    Here { via: u64, from: u32, from_via: u64, chain: Vec<Hop>, settled: bool },
    /// The object left by `transfer` (committed at `frame`) for `dest`, having entered by `via`
    /// from world `from` (where it had entered by `from_via`).
    Left { transfer: u64, dest: u32, via: u64, frame: u64, from: u32, from_via: u64 },
}

impl Seen {
    pub fn hash_into(&self, global: u64, v: &mut Vec<u64>) {
        match self {
            Seen::Here { via, from, from_via, chain, settled } => {
                v.extend([global, 12, *via, *from as u64, *from_via, chain_hash(chain), *settled as u64])
            }
            Seen::Left { transfer, dest, via, frame, from, from_via } => {
                v.extend([global, 13, *transfer, *dest as u64, *via, *frame, *from as u64, *from_via])
            }
        }
    }

    /// The export that brought the object here: (world, the transfer it had entered that world by,
    /// the transfer it left by).
    fn entry_export(&self) -> Option<(u32, u64, u64)> {
        match self {
            Seen::Here { via, from, from_via, .. } | Seen::Left { via, from, from_via, .. } if *via != 0 => Some((*from, *from_via, *via)),
            _ => None,
        }
    }
}

/// A transferable equivocation proof against `world`: its majority certified two exports of one
/// object that it had received once (by `via`), as transfers `a` and `b`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ForkProof {
    pub world: u32,
    pub global: u64,
    pub via: u64,
    pub a: u64,
    pub b: u64,
}

/// Compare what this world knows of an object (`known`) with an arriving path: the chain plus the
/// source's own export (`src`, `via`, `transfer`). Two different exports by one world of the same
/// entry are a double spend by that world's majority.
pub fn fork_proof(known: &Seen, global: u64, chain: &[Hop], src: u32, via: u64, transfer: u64) -> Option<ForkProof> {
    let (world, entry, mine) = known.entry_export()?;
    let path = chain.iter().map(|h| (h.src, h.via, h.transfer)).chain([(src, via, transfer)]);
    path.filter(|&(w, v, t)| w == world && v == entry && t != mine).map(|(_, _, t)| ForkProof { world, global, via: entry, a: mine, b: t }).next()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::member_sig;

    fn book() -> StaticEpochs {
        let mut b = StaticEpochs::default();
        b.worlds.insert(1, (vec![1, 2], 100));
        b.worlds.insert(2, (vec![1, 3], 100));
        b.worlds.insert(3, (vec![1, 4], 100));
        b
    }

    fn hop(b: &StaticEpochs, src: u32, dest: u32, transfer: u64, via: u64, g: u64) -> Hop {
        let fact = fact_committed(src, transfer, g, dest, via);
        let cert = b.members(src, 0).unwrap().iter().map(|&m| (m, member_sig(src, m, fact))).collect();
        Hop { src, dest, transfer, via, epoch: 0, cert }
    }

    #[test]
    fn a_b_c_a() {
        let b = book();
        let g = (1u64 << 32) | 40;
        assert_eq!(check_chain(&b, g, 1, 0, &[]), Ok(()));
        let h1 = hop(&b, 1, 2, 11, 0, g);
        let h2 = hop(&b, 2, 3, 22, 11, g);
        let h3 = hop(&b, 3, 1, 33, 22, g);
        assert_eq!(check_chain(&b, g, 2, 11, &[h1.clone()]), Ok(()));
        assert_eq!(check_chain(&b, g, 3, 22, &[h1.clone(), h2.clone()]), Ok(()));
        assert_eq!(check_chain(&b, g, 1, 33, &[h1.clone(), h2.clone(), h3]), Ok(()));
        // Born elsewhere, minted, forged, spliced, or cut short.
        assert_eq!(check_chain(&b, g, 2, 0, &[]), Err(ChainError::End));
        assert_eq!(check_chain(&b, (1u64 << 32) | 100, 1, 0, &[]), Err(ChainError::NoBirth));
        assert_eq!(check_chain(&b, (9u64 << 32) | 1, 9, 0, &[]), Err(ChainError::NoBirth));
        let mut forged = h1.clone();
        forged.cert.truncate(1);
        assert_eq!(check_chain(&b, g, 2, 11, &[forged]), Err(ChainError::Cert(0)));
        assert_eq!(check_chain(&b, g + 1, 2, 11, &[h1.clone()]), Err(ChainError::Cert(0)));
        assert_eq!(check_chain(&b, g, 3, 22, &[h2]), Err(ChainError::Link(0)));
        assert_eq!(check_chain(&b, g, 3, 22, &[h1]), Err(ChainError::End));
    }
}
