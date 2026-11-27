//! (S3 v1.10; agreement.md 8.) Who may join: the `Admit` entry and the governance fold that decides it.
//!
//! - **An `Admit` is a [`Class::Protocol`](crate::Class::Protocol) entry** whose payload is [`Admit::encode`]: a
//!   versioned tag, the admitted key, the role it is admitted with (pe-world's permission bits), a `request` (signed
//!   by the joiner, naming the line and the epoch) and `vouches`. Both of the last two are empty in M1; they are there
//!   so the user's join rules (a relation x target matrix: friends, confidants, invite tickets, roles;
//!   <notes>) can carry their evidence without a new protocol (@fable-reviewer,
//!   coordinator).
//! - **The decision is a pure function of the entry and the epoch's [`GovState`]** ([`GovState::step`]): no world state,
//!   no callback into the app. S3 runs it in the final log's order beside the `Leave` rules, on final and on certified
//!   ancestry alike, so every member makes the same `Admit`s effective. S2 applies an effective `Admit`'s world side:
//!   pe-replica calls `World::admit` with the role the payload names at the entry's frame, so the world at the
//!   reconfiguring cut holds the joiner's member row, role and hands (@coordinator-opus).
//! - **M1's rule is one row:** vouched by an admin, admit, with the signer's own signature as its vouch. An admin
//!   chooses the role, ADMIN included (the user: any admin can add another admin).
//! - **[`GovState`] is per epoch:** every member's role bits, one table like the world's member table. It starts from
//!   the genesis members' roles and changes only when an epoch ends, by the `Admit`s (with their roles) and `Leave`s its
//!   reconfiguring cut made effective, so a line whose opener left can still admit. A joiner imports it with the epoch
//!   it starts in ([`GovState::encode`]); once S2 writes both changes into the world, the joiner can check it against
//!   the world it imports at E, whose root E's certificate covers (@engine-architect's Low on b84295a2).
//! - **A `Remove`** (S3 v1.13; agreement.md 8) takes a member out at the next epoch: a Protocol entry whose payload is
//!   [`Remove::encode`], the key removed and why ([`RemoveReason`]). [`RemoveReason::Admin`] is an admin's removal of
//!   another member (never its own: that is a `Leave`). [`RemoveReason::Silent`] is the user's automatic removal of a
//!   member silent for 30 s; its rule isn't built, so the fold denies it for now.

use std::collections::BTreeMap;

use bytes::Bytes;

use crate::entry::{Class, SignedEntry};
use crate::ids::MemberKey;

/// The tag an `Admit` payload starts with; the byte after it is the format's version ([`ADMIT_VERSION`]). It ends in
/// a `/`, so another Protocol tag can't begin with it by accident (@design-reviewer's nit on b84295a2): a payload that
/// does is an `Admit`, malformed if it doesn't decode.
pub const ADMIT: &[u8] = b"pe.agree.admit/";
/// The `Admit` payload's format version.
pub const ADMIT_VERSION: u8 = 1;
/// The admin permission bit: pe-world's `perm::ADMIN`, the one role bit the governance fold reads.
pub const ROLE_ADMIN: u32 = 1 << 31;

/// The tag a `Remove` payload starts with, then its version ([`REMOVE_VERSION`]); ends in `/` as [`ADMIT`] does.
pub const REMOVE: &[u8] = b"pe.agree.remove/";
/// The `Remove` payload's format version.
pub const REMOVE_VERSION: u8 = 1;

/// Why a member is removed (module doc).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RemoveReason {
    /// An admin removes another member.
    Admin = 0,
    /// A member silent for 30 s (the user's); not yet allowed by the fold.
    Silent = 1,
}

/// A `Remove` entry's payload (module doc).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Remove {
    /// The member removed.
    pub key: MemberKey,
    pub reason: RemoveReason,
}

impl Remove {
    /// The payload: tag, version, key, reason (one byte).
    pub fn encode(&self) -> Bytes {
        let mut p = Vec::with_capacity(REMOVE.len() + 1 + 32 + 1);
        p.extend_from_slice(REMOVE);
        p.push(REMOVE_VERSION);
        p.extend_from_slice(&self.key.0);
        p.push(self.reason as u8);
        Bytes::from(p)
    }

    /// The `Remove` in `payload`: `None` unless it starts with [`REMOVE`]; `Some(None)` when it does but isn't a
    /// well-formed `Remove` of this version (S3 voids it as not authorized).
    pub fn decode(payload: &[u8]) -> Option<Option<Remove>> {
        let rest = payload.strip_prefix(REMOVE)?;
        Some(match rest {
            [REMOVE_VERSION, key @ .., reason] if key.len() == 32 => {
                let reason = match reason {
                    0 => RemoveReason::Admin,
                    1 => RemoveReason::Silent,
                    _ => return Some(None),
                };
                Some(Remove { key: MemberKey(key.try_into().expect("32 bytes")), reason })
            }
            _ => None,
        })
    }

    /// The `Remove` `e` carries: `None` if `e` isn't one; `Some(None)` if it is a malformed one.
    pub fn of(e: &SignedEntry) -> Option<Option<Remove>> {
        if e.entry.class != Class::Protocol {
            return None;
        }
        Remove::decode(&e.entry.payload)
    }
}

/// An `Admit` entry's payload (module doc).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Admit {
    /// The key admitted.
    pub key: MemberKey,
    /// The role it is admitted with: pe-world's permission bits.
    pub role: u32,
    /// The joiner's signed request, naming the line and the epoch. Empty in M1.
    pub request: Bytes,
    /// Vouches for the joiner (friendship, an invite ticket). Empty in M1.
    pub vouches: Vec<Bytes>,
}

impl Admit {
    /// M1's `Admit`: no request, no vouches.
    pub fn new(key: MemberKey, role: u32) -> Admit {
        Admit { key, role, request: Bytes::new(), vouches: Vec::new() }
    }

    /// The payload: tag, version, key, role (u32 LE), the request (u32 LE length, bytes), the vouches (u16 LE count,
    /// each a u32 LE length and bytes).
    pub fn encode(&self) -> Bytes {
        let mut p = Vec::with_capacity(ADMIT.len() + 1 + 32 + 4 + 4 + self.request.len() + 2);
        p.extend_from_slice(ADMIT);
        p.push(ADMIT_VERSION);
        p.extend_from_slice(&self.key.0);
        p.extend_from_slice(&self.role.to_le_bytes());
        p.extend_from_slice(&(self.request.len() as u32).to_le_bytes());
        p.extend_from_slice(&self.request);
        p.extend_from_slice(&(self.vouches.len() as u16).to_le_bytes());
        for v in &self.vouches {
            p.extend_from_slice(&(v.len() as u32).to_le_bytes());
            p.extend_from_slice(v);
        }
        Bytes::from(p)
    }

    /// The `Admit` in `payload`: `None` unless it starts with [`ADMIT`]; `Some(None)` when it does but isn't a
    /// well-formed `Admit` of this version (S3 voids it as not authorized).
    pub fn decode(payload: &[u8]) -> Option<Option<Admit>> {
        let rest = payload.strip_prefix(ADMIT)?;
        Some(Self::decode_body(rest))
    }

    fn decode_body(b: &[u8]) -> Option<Admit> {
        let (&version, b) = b.split_first()?;
        if version != ADMIT_VERSION {
            return None;
        }
        let (key, b) = b.split_first_chunk::<32>()?;
        let (role, b) = b.split_first_chunk::<4>()?;
        let (len, b) = b.split_first_chunk::<4>()?;
        let len = u32::from_le_bytes(*len) as usize;
        let (request, b) = (b.get(..len)?, b.get(len..)?);
        let (count, mut b) = b.split_first_chunk::<2>()?;
        let mut vouches = Vec::new();
        for _ in 0..u16::from_le_bytes(*count) {
            let (len, rest) = b.split_first_chunk::<4>()?;
            let len = u32::from_le_bytes(*len) as usize;
            vouches.push(Bytes::copy_from_slice(rest.get(..len)?));
            b = rest.get(len..)?;
        }
        if !b.is_empty() {
            return None;
        }
        Some(Admit {
            key: MemberKey(*key),
            role: u32::from_le_bytes(*role),
            request: Bytes::copy_from_slice(request),
            vouches,
        })
    }

    /// The `Admit` `e` carries: `None` if `e` isn't one; `Some(None)` if it is a malformed one.
    pub fn of(e: &SignedEntry) -> Option<Option<Admit>> {
        if e.entry.class != Class::Protocol {
            return None;
        }
        Admit::decode(&e.entry.payload)
    }
}

/// [`GovState::step`]'s answer for an `Admit` or a `Remove`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GovVerdict {
    /// Not allowed (or malformed): S3 voids it as `NotAuthorized`.
    Deny,
    /// Allowed: `key` is admitted with `role`, if it changes the members (S3 voids it as `NoChange` otherwise).
    Admit { key: MemberKey, role: u32 },
    /// Allowed: `key` leaves the members at the next epoch, if it is one (S3 voids it as `NoChange` otherwise).
    Remove { key: MemberKey },
}

/// One epoch's governance state (module doc): each member's role bits (pe-world's permission bits); only
/// [`ROLE_ADMIN`] is read today.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GovState {
    roles: BTreeMap<MemberKey, u32>,
}

impl GovState {
    /// Genesis: every genesis member with its role bits (the genesis record's roles).
    pub fn genesis(members: impl IntoIterator<Item = (MemberKey, u32)>) -> GovState {
        GovState { roles: members.into_iter().collect() }
    }

    /// Every member with its role bits, by key.
    pub fn roles(&self) -> impl Iterator<Item = (MemberKey, u32)> + '_ {
        self.roles.iter().map(|(k, r)| (*k, *r))
    }

    pub fn is_admin(&self, k: &MemberKey) -> bool {
        self.roles.get(k).is_some_and(|r| r & ROLE_ADMIN != 0)
    }

    pub fn admins(&self) -> impl Iterator<Item = MemberKey> + '_ {
        self.roles.iter().filter(|(_, r)| **r & ROLE_ADMIN != 0).map(|(k, _)| *k)
    }

    /// The verdict on an `Admit` entry in this epoch, `None` for any other entry. Pure: M1's one row, vouched by an
    /// admin (the signer's own signature is its vouch), admits with the payload's role. S3 checks separately that the
    /// signer is a member and that the key isn't one.
    ///
    /// A `Remove`: an admin's removal of another member is allowed ([`RemoveReason::Admin`]); a removal of the
    /// signer's own key, by a non-admin, malformed or [`RemoveReason::Silent`] (not built) is denied.
    pub fn step(&self, e: &SignedEntry) -> Option<GovVerdict> {
        if let Some(remove) = Remove::of(e) {
            return Some(match remove {
                Some(Remove { key, reason: RemoveReason::Admin })
                    if self.is_admin(&e.entry.author) && key != e.entry.author =>
                {
                    GovVerdict::Remove { key }
                }
                _ => GovVerdict::Deny,
            });
        }
        let admit = Admit::of(e)?;
        Some(match admit {
            Some(a) if self.is_admin(&e.entry.author) => GovVerdict::Admit { key: a.key, role: a.role },
            _ => GovVerdict::Deny,
        })
    }

    /// The next epoch's state, after its reconfiguring cut admitted `admitted` (key, role) and `leavers` left.
    pub fn next(&self, admitted: &[(MemberKey, u32)], leavers: &[MemberKey]) -> GovState {
        let mut roles = self.roles.clone();
        for k in leavers {
            roles.remove(k);
        }
        for (k, role) in admitted {
            roles.insert(*k, *role);
        }
        GovState { roles }
    }

    /// For a joiner's import: a u32 LE count, then each key and its u32 LE role bits, keys ascending.
    pub fn encode(&self) -> Vec<u8> {
        let mut p = Vec::with_capacity(4 + self.roles.len() * 36);
        p.extend_from_slice(&(self.roles.len() as u32).to_le_bytes());
        for (k, r) in &self.roles {
            p.extend_from_slice(&k.0);
            p.extend_from_slice(&r.to_le_bytes());
        }
        p
    }

    /// [`GovState::encode`]'s inverse; `None` if malformed or keys aren't strictly ascending.
    pub fn decode(b: &[u8]) -> Option<GovState> {
        let (n, mut b) = b.split_first_chunk::<4>()?;
        let mut roles = BTreeMap::new();
        let mut last: Option<MemberKey> = None;
        for _ in 0..u32::from_le_bytes(*n) {
            let (k, rest) = b.split_first_chunk::<32>()?;
            let (r, rest) = rest.split_first_chunk::<4>()?;
            let k = MemberKey(*k);
            if last.is_some_and(|l| l >= k) {
                return None;
            }
            last = Some(k);
            roles.insert(k, u32::from_le_bytes(*r));
            b = rest;
        }
        b.is_empty().then_some(GovState { roles })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_admit_round_trips_and_a_malformed_one_decodes_as_malformed() {
        let a = Admit {
            key: MemberKey([7; 32]),
            role: ROLE_ADMIN | 3,
            request: Bytes::from_static(b"req"),
            vouches: vec![Bytes::from_static(b"v1"), Bytes::new()],
        };
        let p = a.encode();
        assert_eq!(Admit::decode(&p), Some(Some(a.clone())));
        assert_eq!(Admit::decode(&p[..p.len() - 1]), Some(None));
        let mut extra = p.to_vec();
        extra.push(0);
        assert_eq!(Admit::decode(&extra), Some(None));
        let mut v2 = p.to_vec();
        v2[ADMIT.len()] = 2;
        assert_eq!(Admit::decode(&v2), Some(None));
        assert_eq!(Admit::decode(b"pe.agree.leave.v1"), None);
        assert_eq!(
            Admit::decode(&Admit::new(MemberKey([1; 32]), 0).encode()),
            Some(Some(Admit::new(MemberKey([1; 32]), 0)))
        );
    }

    #[test]
    fn a_remove_round_trips_and_a_malformed_one_decodes_as_malformed() {
        let r = Remove { key: MemberKey([9; 32]), reason: RemoveReason::Admin };
        let p = r.encode();
        assert_eq!(Remove::decode(&p), Some(Some(r)));
        let s = Remove { reason: RemoveReason::Silent, ..r };
        assert_eq!(Remove::decode(&s.encode()), Some(Some(s)));
        assert_eq!(Remove::decode(&p[..p.len() - 1]), Some(None));
        let mut extra = p.to_vec();
        extra.push(0);
        assert_eq!(Remove::decode(&extra), Some(None));
        let mut bad = p.to_vec();
        *bad.last_mut().unwrap() = 2;
        assert_eq!(Remove::decode(&bad), Some(None));
        let mut v2 = p.to_vec();
        v2[REMOVE.len()] = 2;
        assert_eq!(Remove::decode(&v2), Some(None));
        assert_eq!(Remove::decode(&Admit::new(MemberKey([1; 32]), 0).encode()), None);
        assert_eq!(Admit::decode(&p), None);
    }

    #[test]
    fn the_state_follows_admits_and_leaves_and_round_trips() {
        let (a, b, c) = (MemberKey([1; 32]), MemberKey([2; 32]), MemberKey([3; 32]));
        let g = GovState::genesis([(a, ROLE_ADMIN), (b, 3)]);
        assert!(!g.is_admin(&b));
        assert!(g.is_admin(&a) && !g.is_admin(&b));
        let next = g.next(&[(b, ROLE_ADMIN | 3), (c, 3)], &[a]);
        assert_eq!(next.roles().collect::<Vec<_>>(), vec![(b, ROLE_ADMIN | 3), (c, 3)]);
        assert_eq!(next.admins().collect::<Vec<_>>(), vec![b]);
        assert!(!next.is_admin(&c));
        assert_eq!(GovState::decode(&next.encode()), Some(next.clone()));
        assert_eq!(GovState::decode(&next.encode()[..10]), None);
    }
}
