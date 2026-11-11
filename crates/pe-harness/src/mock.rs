//! `LockstepNode`: a deliberately old-style agreement, for testing the harness before pe-agree exists (DESIGN.md 12).
//!
//! Every member broadcasts its inputs for each frame. A frame is final once every member's message for it is held,
//! and the replica steps only final frames: unanimous lockstep, no prediction. That is the shape the review found
//! wanting (file 01, unanimous confirmation; file 07, H1): one silent member stops everyone. That makes it a useful
//! target. The probes run against it and fail with the property they name (P8), which is what the pending register
//! requires until pe-agree replaces it.
//!
//! It exists only to test the harness, and is never a reference for pe-agree.

use std::collections::{BTreeMap, BTreeSet};

use crate::kv::{KvEntry, KvReplica, Op, Template};
use crate::net::{Lane, NodeId};
use crate::sim::{Fault, NodeUnderTest, Out};
use crate::trace::{Ev, Frame, Hash};

/// Frames this far behind the local clock make the node pause: the old 2 s window, as a duration.
pub const WINDOW_NS: i64 = 2_000_000_000;

pub struct LockstepNode {
    /// The mock's own planted bug, for testing randsched and the shrinker: accept a message whose claimed author is
    /// not its sender. Off by default. (The mock is harness-internal; pe-agree's mutations live behind `mutants`.)
    pub accept_forged_name: bool,
    me: NodeId,
    members: Vec<NodeId>,
    frame_ns: i64,
    replica: KvReplica,
    /// Our own inputs waiting for the next frame we send.
    pending: Vec<Op>,
    /// The last frame we broadcast.
    sent: Frame,
    /// Every member's ops per frame, as received (and our own).
    held: BTreeMap<Frame, BTreeMap<NodeId, Vec<Op>>>,
    /// Final entries per frame, in the agreed order.
    finals: Vec<(Frame, Vec<KvEntry>)>,
    events: Vec<Ev>,
    paused_at: BTreeSet<Frame>,
}

impl LockstepNode {
    pub fn new(me: NodeId, members: &[NodeId], template: &Template, objects: &[u128], hz: u32) -> Self {
        Self {
            accept_forged_name: false,
            me,
            members: members.to_vec(),
            frame_ns: 1_000_000_000 / i64::from(hz.max(1)),
            replica: KvReplica::new(members, template, objects),
            pending: Vec::new(),
            sent: 0,
            held: BTreeMap::new(),
            finals: Vec::new(),
            events: Vec::new(),
            paused_at: BTreeSet::new(),
        }
    }

    fn local_frame(&self, now: i64) -> Frame {
        (now.max(0) / self.frame_ns) as Frame
    }

    fn advance_final(&mut self) {
        loop {
            let f = self.replica.frame() + 1;
            let Some(row) = self.held.get(&f) else { return };
            if !self.members.iter().all(|m| row.contains_key(m)) {
                return;
            }
            let row = self.held.remove(&f).unwrap_or_default();
            let entries: Vec<KvEntry> = row
                .into_iter()
                .flat_map(|(author, ops)| ops.into_iter().map(move |op| KvEntry { author, op }))
                .collect();
            for (e, o) in entries.iter().zip(self.replica.step(&entries)) {
                if let crate::kv::Outcome::Refused(r) = o {
                    let seq = u64::from(e.author) << 32 | f;
                    self.events.push(Ev::Refused {
                        entry: crate::trace::EntryRef { author: e.author, seq },
                        reason: r as u16,
                    });
                }
            }
            let root = self.replica.root_hash();
            self.events.push(Ev::ReplicaRoot { frame: f, root });
            self.events.push(Ev::Final { line: 0, height: f, frame: f, cut: cut_hash(f, &entries), root });
            // Lockstep holds every entry of a frame before it is final, so the floor is the final frame itself.
            self.events.push(Ev::Floor { frame: f });
            self.finals.push((f, entries));
        }
    }
}

fn cut_hash(frame: Frame, entries: &[KvEntry]) -> Hash {
    let mut h = blake3::Hasher::new();
    h.update(b"pe.mock.cut\0");
    h.update(&frame.to_le_bytes());
    for e in entries {
        h.update(&e.author.to_le_bytes());
        h.update(&encode_op(&e.op));
    }
    *h.finalize().as_bytes()
}

impl NodeUnderTest for LockstepNode {
    fn input(&mut self, _now: i64, op: Op) {
        self.pending.push(op);
    }

    fn receive(&mut self, _now: i64, from: NodeId, bytes: &[u8]) -> Result<(), Fault> {
        let (author, frame, ops) = decode(bytes).ok_or(Fault { author: from, reason: 1 })?;
        if (author != from && !self.accept_forged_name) || !self.members.contains(&author) {
            return Err(Fault { author: from, reason: 2 });
        }
        if frame > self.replica.frame() {
            self.held.entry(frame).or_default().entry(author).or_insert(ops);
        }
        self.advance_final();
        Ok(())
    }

    fn tick(&mut self, now: i64) -> Vec<Out> {
        let mut out = Vec::new();
        let target = self.local_frame(now);
        while self.sent < target {
            self.sent += 1;
            let ops = if self.sent == target { std::mem::take(&mut self.pending) } else { Vec::new() };
            let bytes = encode(self.me, self.sent, &ops);
            self.held.entry(self.sent).or_default().insert(self.me, ops);
            for &m in &self.members {
                if m != self.me {
                    out.push(Out { to: m, lane: Lane::Action, bytes: bytes.clone() });
                }
            }
        }
        self.advance_final();
        let behind = self.local_frame(now).saturating_sub(self.replica.frame());
        if behind as i64 * self.frame_ns > WINDOW_NS && self.paused_at.insert(target) {
            self.events.push(Ev::Paused { frame: target });
        }
        out
    }

    fn drain(&mut self) -> Vec<Ev> {
        std::mem::take(&mut self.events)
    }

    fn final_entries(&self) -> Vec<(Frame, Vec<KvEntry>)> {
        self.finals.clone()
    }
}

// A small wire format: author u32, frame u64, op count u16, then each op as a tag and its fields.

fn encode_op(op: &Op) -> Vec<u8> {
    let mut b = Vec::new();
    match *op {
        Op::Create { obj } => {
            b.push(1);
            b.extend(obj.to_le_bytes());
        }
        Op::Delete { obj } => {
            b.push(2);
            b.extend(obj.to_le_bytes());
        }
        Op::Set { obj, field, value } => {
            b.push(3);
            b.extend(obj.to_le_bytes());
            b.extend(field.to_le_bytes());
            b.extend(value.to_le_bytes());
        }
        Op::Add { obj, field, delta } => {
            b.push(4);
            b.extend(obj.to_le_bytes());
            b.extend(field.to_le_bytes());
            b.extend(delta.to_le_bytes());
        }
        Op::DefineRole { role, perms } => b.extend([5, role, perms]),
        Op::SetRole { member, role } => {
            b.push(6);
            b.extend(member.to_le_bytes());
            b.push(role);
        }
    }
    b
}

pub fn encode(author: NodeId, frame: Frame, ops: &[Op]) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend(author.to_le_bytes());
    b.extend(frame.to_le_bytes());
    b.extend((ops.len() as u16).to_le_bytes());
    for op in ops {
        b.extend(encode_op(op));
    }
    b
}

struct Rd<'a>(&'a [u8]);

impl Rd<'_> {
    fn take<const N: usize>(&mut self) -> Option<[u8; N]> {
        let (h, t) = self.0.split_at_checked(N)?;
        self.0 = t;
        h.try_into().ok()
    }
    fn u8(&mut self) -> Option<u8> {
        self.take::<1>().map(|b| b[0])
    }
}

pub fn decode(bytes: &[u8]) -> Option<(NodeId, Frame, Vec<Op>)> {
    let mut r = Rd(bytes);
    let author = u32::from_le_bytes(r.take()?);
    let frame = u64::from_le_bytes(r.take()?);
    let n = u16::from_le_bytes(r.take()?);
    let mut ops = Vec::with_capacity(usize::from(n).min(64));
    for _ in 0..n {
        let op = match r.u8()? {
            1 => Op::Create { obj: u128::from_le_bytes(r.take()?) },
            2 => Op::Delete { obj: u128::from_le_bytes(r.take()?) },
            3 => Op::Set {
                obj: u128::from_le_bytes(r.take()?),
                field: u16::from_le_bytes(r.take()?),
                value: i64::from_le_bytes(r.take()?),
            },
            4 => Op::Add {
                obj: u128::from_le_bytes(r.take()?),
                field: u16::from_le_bytes(r.take()?),
                delta: i64::from_le_bytes(r.take()?),
            },
            5 => Op::DefineRole { role: r.u8()?, perms: r.u8()? },
            6 => Op::SetRole { member: u32::from_le_bytes(r.take()?), role: r.u8()? },
            _ => return None,
        };
        ops.push(op);
    }
    r.0.is_empty().then_some((author, frame, ops))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codec_round_trips_and_rejects_junk() {
        let ops = vec![
            Op::Create { obj: 5 },
            Op::Set { obj: 5, field: 2, value: -9 },
            Op::Add { obj: 5, field: 2, delta: 3 },
            Op::DefineRole { role: 1, perms: 0 },
            Op::SetRole { member: 3, role: 1 },
            Op::Delete { obj: 5 },
        ];
        let b = encode(7, 42, &ops);
        assert_eq!(decode(&b), Some((7, 42, ops)));
        assert_eq!(decode(&b[..b.len() - 1]), None);
        let mut long = b.clone();
        long.push(0);
        assert_eq!(decode(&long), None);
        assert_eq!(decode(&[0xFF; 3]), None);
        let mut bad_tag = encode(1, 1, &[Op::Create { obj: 1 }]);
        bad_tag[14] = 99;
        assert_eq!(decode(&bad_tag), None);
    }
}
