//! The local player's input (app.md 3, step 2; xr.md 5.2): both hands at a frame's host time, through the rig, as
//! pe-world's hand inputs, each submitted through agreement on this member's chain.
//!
//! The floating-point work here runs only on the author's machine. Every peer steps the encoded, signed input, so
//! determinism needs nothing from it (xr.md 5.2).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use pe_seams::{
    Agreement, ChainId, Class, DomainId, Entry, EntryRef, Frame, FrameView, Hand, HandFeed, HandInput, HostTime,
    LineId, MemberKey, Offset, RigTransform, SignedEntry,
};
use pe_world::input::{HandPose, Input};

/// This member's chain (S1): every entry it authors, inputs and edits alike, in order. The first entry carries the
/// nonce its chain id derives from; each later one names the one before.
pub struct Chain {
    line: LineId,
    me: MemberKey,
    nonce: [u8; 32],
    id: ChainId,
    next: u64,
    prev: Option<EntryRef>,
    /// The stamp of the last entry agreement took: an entry stamped ahead never steps below it (stamp-ahead).
    last: Option<Frame>,
}

impl Chain {
    pub fn new(line: LineId, me: MemberKey, nonce: [u8; 32]) -> Chain {
        Chain { line, me, nonce, id: ChainId::new(line, me, nonce), next: 1, prev: None, last: None }
    }

    pub fn me(&self) -> MemberKey {
        self.me
    }

    /// The chain's id, and the seq of the last entry agreement took (0 before the first).
    pub fn head(&self) -> (ChainId, u64) {
        (self.id, self.next - 1)
    }

    /// The chain's next entry, unsigned: `Agreement::submit` signs it.
    pub fn entry(&self, domain: DomainId, stamp: Frame, class: Class, payload: Vec<u8>) -> Entry {
        Entry {
            line: self.line,
            author: self.me,
            chain: self.id,
            seq: self.next,
            prev: self.prev,
            nonce: (self.next == 1).then_some(self.nonce),
            domain,
            stamp,
            class,
            payload: payload.into(),
        }
    }

    /// Agreement took `e`, this chain's next entry.
    pub fn took(&mut self, e: &SignedEntry) {
        self.next += 1;
        self.prev = Some(e.id());
        self.last = Some(e.entry.stamp);
    }

    /// The stamp of the last entry agreement took, `None` before the first.
    pub fn last_stamp(&self) -> Option<Frame> {
        self.last
    }
}

/// A nonce for a new chain. It only has to differ between this member's chains, since `ChainId::new` hashes it with
/// the line and the key: the start time to the nanosecond and the process id.
pub fn chain_nonce() -> [u8; 32] {
    let mut n = [0u8; 32];
    let t = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos());
    n[..16].copy_from_slice(&t.to_le_bytes());
    n[16..20].copy_from_slice(&std::process::id().to_le_bytes());
    n
}

/// The hand feed in use: VR's ring while an XR session runs, the desktop hand otherwise (app.md 1; xr.md 3.3).
/// pe-app flips it when the session starts and ends. VR's ring keeps answering from its last samples after a session,
/// so a feed that took whichever answered first would hold the hands where VR left them.
pub struct SwitchedHands {
    vr: Arc<dyn HandFeed>,
    desktop: Arc<dyn HandFeed>,
    vr_on: AtomicBool,
}

impl SwitchedHands {
    pub fn new(vr: Arc<dyn HandFeed>, desktop: Arc<dyn HandFeed>) -> SwitchedHands {
        SwitchedHands { vr, desktop, vr_on: AtomicBool::new(false) }
    }

    pub fn use_vr(&self, on: bool) {
        self.vr_on.store(on, Ordering::Release);
    }
}

impl HandFeed for SwitchedHands {
    fn hand_at(&self, hand: Hand, at: HostTime) -> Option<HandInput> {
        if self.vr_on.load(Ordering::Acquire) { self.vr.hand_at(hand, at) } else { self.desktop.hand_at(hand, at) }
    }
}

/// A hand's world pose for pe-world's input: its rig-frame pose through the rig (xr.md 3.2), the grip clamped to
/// 0..1, and engaged when the author's machine decided the grasp closed. `None` if the pose falls outside the world,
/// or its rotation is no rotation.
pub fn world_pose(rig: &RigTransform, h: &HandInput) -> Option<HandPose> {
    let [x, y, z] = h.pose.position;
    let p = rotate(rig.rotation, [x * rig.scale, y * rig.scale, z * rig.scale]);
    let pos = rig.pos.checked_add(Offset(p.map(f64::from))).ok()?;
    let rot = normalized(mul(rig.rotation, h.pose.orientation))?;
    Some(HandPose { pos, rot, grip: h.grip.clamp(0.0, 1.0), engaged: h.closed })
}

/// Step 2's hands: one input entry for each hand the feed tracks at `at`, frame `p`'s host time, each moving the local
/// member's own hand object of that side, as the newest view shows it, to its world pose. Returns the entries
/// agreement took and those it refused.
///
/// An untracked hand sends nothing, so the world coasts it and then holds it (xr.md 5.5, D52), except when its grasp
/// differs from the `engaged` last sent for that side (`sent`, left then right): that is pe-xr's deliberate release
/// of a lost hand that held something, after the grace period or on focus loss, and it goes out at the last valid
/// pose, so nothing stays held by a frozen hand (@xr-eng's Medium on cc8d0a96).
///
/// `thin`: the frame's stamp stands still (stamp-ahead is shedding its lead, app.md 3.2), so a pose would only replace
/// the previous frame's at the same stamp. Such poses aren't sent; a grasp that changed (a grab or a release) still
/// goes out, at the stamp (@coordinator-opus). Returns the entries agreement took, those it refused, and
/// the poses thinned.
#[allow(clippy::too_many_arguments)]
pub fn submit_hands(
    chain: &mut Chain,
    sent: &mut [bool; 2],
    feed: &dyn HandFeed,
    view: &FrameView,
    at: HostTime,
    p: Frame,
    domain: DomainId,
    agree: &mut dyn Agreement,
    thin: bool,
) -> (u32, u32, u32) {
    let (mut took, mut refused, mut thinned) = (0, 0, 0);
    for (i, side) in [Hand::Left, Hand::Right].into_iter().enumerate() {
        let Some(h) = feed.hand_at(side, at).filter(|h| h.tracked || h.closed != sent[i]) else { continue };
        if thin && h.closed == sent[i] {
            thinned += 1;
            continue;
        }
        let Some(hand) = view.objects.iter().find(|o| o.hand_of == Some((chain.me(), side))).map(|o| o.id) else {
            continue;
        };
        let Some(pose) = world_pose(&view.local_rig, &h) else { continue };
        let payload = Input::Hand { hand, poses: vec![pose], hold: 0 }.encode();
        match agree.submit(chain.entry(domain, p, Class::Input, payload)) {
            Ok(signed) => {
                chain.took(&signed);
                sent[i] = pose.engaged;
                took += 1;
            }
            Err(_) => refused += 1,
        }
    }
    (took, refused, thinned)
}

/// The Hamilton product `a b` of two quaternions in OpenXR's order, (x, y, z, w).
fn mul(a: [f32; 4], b: [f32; 4]) -> [f32; 4] {
    let [ax, ay, az, aw] = a;
    let [bx, by, bz, bw] = b;
    [
        aw * bx + ax * bw + ay * bz - az * by,
        aw * by - ax * bz + ay * bw + az * bx,
        aw * bz + ax * by - ay * bx + az * bw,
        aw * bw - ax * bx - ay * by - az * bz,
    ]
}

/// `v` rotated by the unit quaternion `q`.
fn rotate(q: [f32; 4], v: [f32; 3]) -> [f32; 3] {
    let r = mul(mul(q, [v[0], v[1], v[2], 0.0]), [-q[0], -q[1], -q[2], q[3]]);
    [r[0], r[1], r[2]]
}

fn normalized(q: [f32; 4]) -> Option<[f32; 4]> {
    let n = q.iter().map(|v| v * v).sum::<f32>().sqrt();
    (n.is_finite() && n > 1e-6).then(|| q.map(|v| v / n))
}

#[cfg(test)]
mod tests {
    use super::*;
    use pe_seams::{HandSource, Pos, RigPose};

    fn sample(position: [f32; 3], orientation: [f32; 4]) -> HandInput {
        HandInput {
            hand: Hand::Right,
            source: HandSource::Tracked,
            sampled_at: HostTime(0),
            tracked: true,
            pose: RigPose { position, orientation },
            linear_velocity: None,
            angular_velocity: None,
            grip: 1.5,
            closed: true,
            discontinuity: 0,
        }
    }

    /// A rig turned 90 degrees about +Y, scaled 2 and placed at (10, 0, 0): a hand 1 m ahead (-Z) of the player is 2 m
    /// along -X in the world, and its rotation turns with the rig.
    #[test]
    fn a_pose_goes_through_the_rig() {
        let half = std::f32::consts::FRAC_1_SQRT_2;
        let rig = RigTransform {
            pos: Pos::ORIGIN.checked_add(Offset([10.0, 0.0, 0.0])).expect("in range"),
            rotation: [0.0, half, 0.0, half],
            scale: 2.0,
        };
        let pose = world_pose(&rig, &sample([0.0, 1.0, -1.0], [0.0, 0.0, 0.0, 1.0])).expect("a pose");
        let d = pose.pos - rig.pos;
        assert!((d.0[0] + 2.0).abs() < 1e-5 && (d.0[1] - 2.0).abs() < 1e-5 && d.0[2].abs() < 1e-5, "{d:?}");
        assert!(pose.rot.iter().zip(rig.rotation).all(|(a, b)| (a - b).abs() < 1e-6), "{:?}", pose.rot);
        assert_eq!((pose.grip, pose.engaged), (1.0, true), "the grip clamps, and the grasp engages");
        assert!(world_pose(&rig, &sample([0.0; 3], [0.0; 4])).is_none(), "no rotation, no pose");
    }

    /// The chain: seq from 1, the nonce only on the first entry, and each later entry naming the one before.
    #[test]
    fn a_chain_numbers_its_entries_and_links_them() {
        let me = MemberKey([3; 32]);
        let mut c = Chain::new(LineId::origin(), me, [9; 32]);
        let first = c.entry(DomainId(0), Frame(4), Class::Input, vec![1]);
        assert_eq!((first.seq, first.prev, first.nonce), (1, None, Some([9; 32])));
        assert_eq!(first.chain, ChainId::new(LineId::origin(), me, [9; 32]));
        let signed = SignedEntry { entry: first, sig: pe_seams::Signature([0; 64]) };
        c.took(&signed);
        let second = c.entry(DomainId(0), Frame(5), Class::Input, vec![2]);
        assert_eq!((second.seq, second.prev, second.nonce), (2, Some(signed.id()), None));
    }
}
