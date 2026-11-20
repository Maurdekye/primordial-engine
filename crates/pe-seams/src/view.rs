//! Seam S5, the frame view: what the simulation publishes once per frame for pe-render, pe-xr and pe-api, and the
//! hand inputs that flow the other way (xr.md 8.1; render.md 5, 8, 12; replica.md 7; divergence.md 8, 11.3;
//! world-model.md 9.3; api.md 9.1, 11).
//!
//! - **A plain value, published as `Arc<FrameView>`** into a slot the render thread reads without blocking the
//!   simulation. It is never mutated after publishing. Consumers take `&FrameView`; a mock is just a value.
//! - **Two poses** (divergence.md D1): the view carries the simulated pose only. The drawn pose is pe-render's own;
//!   the simulation never reads it. pe-render blends a correction when an object is marked
//!   [`ViewFlags::CORRECTED`], and never corrects the local player's hands or what they hold (D4).
//! - **Numbers reach the renderer only by subtracting a `Pos`** (the camera's), so a large coordinate never reaches
//!   a GPU float.
//! - **One-shot effects fire once:** each has a stable [`EffectKey`] across replays, and a replay that removes one
//!   lists its key in [`FrameView::retracted`].
//! - **Code commits activate or are voided at a frame's end** (logic.md 6.2a): [`FrameView::activations`] records
//!   each, keyed by [`ActivationKey`], and [`FrameView::activations_retracted`] names those a re-simulation no longer
//!   produced (S5 v1.8).
//! - **No GPU types here.** `GpuContext`, eye views and `render_views` are pe-render's, agreed between WS-render
//!   and WS-xr.
//!
//! **Provisional in v0:** the drawable component shapes ([`Shape`], [`Appearance`], [`Light`]) follow
//! world-model.md 3 and may gain fields with pe-world; [`Effect`]'s freshness fields, which WS-render and WS-diverge
//! settle with the display layer's tests.

use bytes::Bytes;

use crate::entry::{Class, DomainId};
use crate::ids::{EntryRef, Frame, Hash, MemberKey, ObjectId, Pos};
use crate::replica::{Outcome, Version};
use crate::time::HostTime;

// ---- hands, from pe-xr and the desktop hand ------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Hand {
    Left,
    Right,
}

impl Hand {
    pub const BOTH: [Hand; 2] = [Hand::Left, Hand::Right];

    pub fn index(self) -> usize {
        match self {
            Hand::Left => 0,
            Hand::Right => 1,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum HandSource {
    Tracked,
    Controller,
    Desktop,
}

/// A pose in the local player's rig frame (the tracking space): metres, right-handed, +Y up, -Z forward. The
/// orientation is a unit quaternion in OpenXR's order, (x, y, z, w).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RigPose {
    pub position: [f32; 3],
    pub orientation: [f32; 4],
}

impl RigPose {
    pub const IDENTITY: RigPose = RigPose { position: [0.0; 3], orientation: [0.0, 0.0, 0.0, 1.0] };
}

/// One hand of the local player at one host time (xr.md 5.1). The simulation thread quantizes it into pe-world's
/// `Input` (a target `Pos`, a rotation, the grip and the engaged flag).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct HandInput {
    pub hand: Hand,
    pub source: HandSource,
    pub sampled_at: HostTime,
    /// Whether the pose is valid now. When false, the pose is the last valid one.
    pub tracked: bool,
    pub pose: RigPose,
    pub linear_velocity: Option<[f32; 3]>,
    pub angular_velocity: Option<[f32; 3]>,
    /// 0 open .. 1 closed.
    pub grip: f32,
    /// The grasp, decided on the author's machine with hysteresis.
    pub closed: bool,
    /// Bumped when the pose jumps for a reason other than motion, so a jump is never read as a throw.
    pub discontinuity: u32,
}

/// Written by pe-xr (or pe-render's desktop hand), read by the simulation thread.
pub trait HandFeed: Send + Sync {
    /// The local player's hand for a simulation frame's host time; `None` while the hand is absent.
    fn hand_at(&self, hand: Hand, at: HostTime) -> Option<HandInput>;
}

// ---- the view ------------------------------------------------------------------------------------------------------

/// The local player's rig in the world: an exact position, a rotation and a uniform scale (the player's scale).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RigTransform {
    pub pos: Pos,
    pub rotation: [f32; 4],
    pub scale: f32,
}

/// An object's persistence class (world-model.md 3.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ObjectClass {
    /// Shared, hashed and saved.
    World,
    /// This machine's own: never shared or hashed.
    Local,
}

/// A blob by its content hash.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BlobRef(pub Hash);

/// What to draw (world-model.md 3.2, `Shape`), in the object's own space before its scale.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Shape {
    Sphere { radius: f32 },
    Box { half_extents: [f32; 3] },
    Capsule { radius: f32, half_height: f32 },
    Mesh(BlobRef),
}

/// world-model.md 3.2, `Appearance`. Colours are linear RGB(A).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Appearance {
    pub base_color: [f32; 4],
    pub emissive: [f32; 3],
    pub roughness: f32,
    pub metalness: f32,
    pub material: Option<BlobRef>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum LightKind {
    Point,
    Spot { inner_angle: f32, outer_angle: f32 },
    Directional,
}

/// world-model.md 3.2, `Light`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Light {
    pub kind: LightKind,
    pub color: [f32; 3],
    pub intensity: f32,
    pub range: f32,
}

/// Per-object flags for the display layer.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct ViewFlags(pub u8);

impl ViewFlags {
    /// A hand of the local player: drawn from tracking, never corrected.
    pub const OWN_HAND: ViewFlags = ViewFlags(1);
    /// Held by a hand of the local player, in the current predicted state: never corrected.
    pub const HELD_LOCAL: ViewFlags = ViewFlags(2);
    /// Its simulated pose was rewritten by a replay, a repair or a merge since the last publication.
    pub const CORRECTED: ViewFlags = ViewFlags(4);
    /// A `Release` of it by the local player applied in the steps this view covers (render.md 8, D4's hand-off): a hold
    /// that ends in this view ended because the player let go, not by a lost race or a lapsed claim. Set by pe-app,
    /// which knows its own `Release` entries, from the view's step records (app.md 3, step 6).
    pub const RELEASED_LOCAL: ViewFlags = ViewFlags(8);
    /// Held by a hand of the local player ([`ViewFlags::HELD_LOCAL`] too), but kept more than [`BREAK_AWAY`] from its
    /// place in the hand in the simulation: blocked by a wall, say, or too heavy to follow (render.md 8, D4's
    /// break-away; S5 v1.9). It is drawn where it is simulated, still never corrected. Set by pe-replica, which knows
    /// each grab's place, at publication.
    pub const HELD_AWAY: ViewFlags = ViewFlags(16);

    pub fn contains(self, f: ViewFlags) -> bool {
        self.0 & f.0 == f.0
    }

    pub fn with(self, f: ViewFlags) -> ViewFlags {
        ViewFlags(self.0 | f.0)
    }
}

/// D4's break-away distance, m (render.md 8; provisional, @coordinator-opus medc665571803): a held object kept farther
/// than this from its place in the hand is [`ViewFlags::HELD_AWAY`], drawn where it is simulated.
pub const BREAK_AWAY: f64 = 0.05;
/// The distance a [`ViewFlags::HELD_AWAY`] object must come back within before it is drawn at the hand again, m
/// (provisional): below [`BREAK_AWAY`], so a gap hovering near it doesn't flicker between the two (@design-reviewer's
/// L1 on the break-away).
pub const BREAK_BACK: f64 = 0.03;

/// Which hand holds an object.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Holder {
    pub member: MemberKey,
    /// The hand object. A held object's grip offset is its pose relative to this hand's simulated pose.
    /// `ObjectId::NONE` for a hand-less hold (an agent's grab, world-model.md 8.1): there is no grip to latch to,
    /// the hold is shown as such ("moved by X, hand-less"), and the object is never `HELD_LOCAL`.
    pub hand: ObjectId,
}

/// One object in the view.
#[derive(Clone, Debug, PartialEq)]
pub struct ViewObject {
    pub id: ObjectId,
    pub class: ObjectClass,
    /// The simulated world pose.
    pub pos: Pos,
    pub rotation: [f32; 4],
    pub scale: [f32; 3],
    /// Live velocities, so the display layer can extrapolate between simulation frames.
    pub linear_velocity: [f32; 3],
    pub angular_velocity: [f32; 3],
    /// `None` for an object with nothing to draw.
    pub shape: Option<Shape>,
    pub appearance: Option<Appearance>,
    pub light: Option<Light>,
    /// The bounding radius in metres, world scale applied, for culling (render.md 12).
    pub radius: f32,
    /// For a hand: its member, and which hand.
    pub hand_of: Option<(MemberKey, Hand)>,
    pub held_by: Option<Holder>,
    pub flags: ViewFlags,
    /// The object's edit version, so a UI can show "changed by the agent"; `None` for an object no edit has written
    /// (genesis, or trusted engine code).
    pub version: Option<Version>,
}

/// What caused an effect.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum EffectCause {
    Entry(EntryRef),
    Physics,
    Logic,
}

/// The kind of a one-shot effect.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum EffectKind {
    GrabWon,
    /// A grab lost a race; the effect's data names the winner.
    GrabLost,
    EditApplied,
    EditRefused,
    ContactBegin,
    /// An event a script declared (logic.md 3.2). The number is the first 8 bytes, little-endian, of
    /// `Hash::of("pe.script.event", asset ‖ name)`: the script's 32-byte asset id, then the UTF-8 name of the event
    /// type as its system's `emits` list declares it. So it stays the same when a script reorders that list, and for
    /// every system that emits it (@coordinator-opus m154810982c41; S5 v1.7). Such an effect's cause is
    /// [`EffectCause::Logic`]; its `object` is the emitting object, `at` that object's world position after the stage,
    /// `data` the event's bytes, `own` false, and `ordinal` counts that object's events of this kind in the frame, in
    /// stage order. A re-simulated one has no `trigger_arrived`, so it is never played (D6), though it can be
    /// retracted.
    Script(u64),
}

/// An effect's key, stable across replays (divergence.md D6): a key already played is never played again.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EffectKey {
    pub frame: Frame,
    pub cause: EffectCause,
    pub object: ObjectId,
    pub kind: EffectKind,
    /// The ordinal among that object's events of that frame.
    pub ordinal: u32,
}

/// A one-shot effect: a sound, particles, a haptic pulse.
#[derive(Clone, Debug, PartialEq)]
pub struct Effect {
    pub key: EffectKey,
    pub at: Pos,
    /// Caused by the local player's own action: plays at once.
    pub own: bool,
    /// It arose in a re-simulation of its frame, not the first simulation.
    pub resimulated: bool,
    /// For a re-simulated effect, when the entry whose arrival caused the re-simulation arrived. An effect with no
    /// newly arrived cause (a repair, a merge) has `None` and is never played (D6); otherwise it plays only within
    /// `T_effect_lag` (1 s) of its frame.
    pub trigger_arrived: Option<HostTime>,
    /// Kind-specific data.
    pub data: Bytes,
}

/// The view's counters for this publication.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ViewCounters {
    /// Objects whose simulated pose a re-simulation changed since the last view.
    pub corrections: u32,
    /// The deepest rollback since the last view, in frames.
    pub deepest_rollback: u64,
    pub frames_resimulated: u64,
    /// Floating-point environment resets by the guard.
    pub fp_resets: u32,
    /// The local member's own interactions (grab begins, pushes and throws) whose body the final history left other
    /// than the author first simulated it: a correction that hit a body this member had just acted on (replica.md
    /// 3.4). Counted as each frame goes final.
    pub interactions_corrected: u32,
}

/// An entry stepped, with its outcome: the change record beside S5 (api.md 9.1), for WorldApi's `apply`, `watch` and
/// history. A re-stepped entry appears again; the newest verdict wins, and within one view a later record of the same
/// entry is the newer one.
///
/// **Complete at the final frame** (S5 v1.4 and v1.5; @ballpit-viz md5e264073e68, after @shader-reviewer's L1 on
/// render.md 8.1; @coordinator-opus md52a099ab4c1 and m3cb7caa9343e). A view's `changes`, with those of the views
/// before it, hold the final verdict of every entry stamped below that view's `final_frame` (a cut at frame `f` covers
/// the entries stamped below `f`; S3), as the newest record of that entry. A view's `changes` may carry the final
/// record of an entry together with the `final_frame` that first passes its frame, so a reader applies a view's
/// `changes` before its `final_frame`; no record of an entry reaches a view whose *predecessor's* `final_frame` was
/// already past its frame (S5 v1.6; @design-reviewer's Low 2 on harness/changes, the reading pe-replica builds).
/// pe-render's notices and WorldApi's `watch` tell a verdict once `final_frame` passes its entry's frame, and rely
/// on this. It holds
/// because `final_frame` is my last agreed cut, which a diverged replica does not move, and because the replica
/// records, before `final_frame` passes them:
/// - the entries it re-steps while finalizing (its own root check, and its rebase onto the floor);
/// - after a rung-1 repair, the entries the replay re-stepped from my last agreed cut;
/// - after a rung-2 import at a cut, one [`Outcome::Unknown`] for each entry of the cut's order from my last agreed
///   cut up to the cut: they were stepped on the wrong state, and this replica cannot know their verdicts;
/// - one [`Outcome::Dropped`] for each entry it stepped that the final order leaves out (voided, or pending below a
///   final cut, which only a void ends).
///
/// A reader can rely on this only if it takes every view, in order. pe-app feeds WorldApi's `ChangeLog` at each
/// publication on the simulation thread, and pe-render's queue hands the render thread every view since its last
/// read. After a gap the queue marks (an overflow, or a new reader), the reader drops what it held per entry, so it
/// never tells a verdict whose newer record it skipped (@coordinator-opus mde0ea3d44512).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StepRecord {
    pub entry: EntryRef,
    pub author: MemberKey,
    pub class: Class,
    /// The frame it was stepped at.
    pub frame: Frame,
    pub outcome: Outcome,
}

/// A code commit's key in [`FrameView::activations`]: the script asset and the commit, its entry and op (S5 v1.8). A
/// record published later with the same key supersedes the earlier one, whatever its frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ActivationKey {
    /// The script asset (pe-world's `AssetId`).
    pub asset: [u8; 32],
    /// The commit: its entry and op, the version it brings.
    pub version: Version,
}

/// What became of a code commit at the end of a frame (logic.md 6.2a; pe-world's `FrameEnd`; S5 v1.8,
/// @coordinator-opus m636ca7d957da and m8602abb4b37e). Under 6.2a a commit only makes a pending version; it activates
/// once a quorum of its voters has attested their builds, or is voided, both at a frame's end and never in an entry's
/// step, so no [`StepRecord`] names it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Activation {
    pub key: ActivationKey,
    /// The frame at whose end it happened: an activated version runs from `frame + 1`.
    pub frame: Frame,
    pub outcome: ActivationOutcome,
}

/// An [`Activation`]'s outcome.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ActivationOutcome {
    /// The new version runs from the next frame. `rekeyed` names the objects whose rows the re-key moved to the new
    /// version's ids, in id order; it is empty when no re-key was needed (a script's first version, or a same-source
    /// commit, logic.md 6.1).
    Activated { rekeyed: Vec<ObjectId> },
    /// The commit was voided, and the old version stays bound.
    Voided {
        why: VoidedWhy,
        /// For [`VoidedWhy::RekeyBlockedAtActivation`], the objects that blocked the re-key, in id order: the first 16
        /// and how many more (pe-world's `Blocked`). Empty and 0 for every other reason.
        objects: Vec<ObjectId>,
        more: u32,
        /// Every counted `BuildFailed` at the void, in voter key order, each with why: who could not build it, for
        /// the author's notice (Q1 (c) of the user's build-asymmetry ruling, mbd847a64c831). The failures explain the
        /// void when `why` is `QuorumUnreachable` (with any voters who left) or `Unattested` (the rest had not
        /// attested by `D`); a `QuorumUnreachable` with no failure is one that voters' leaving caused
        /// (@api-eng me99398e5a1cb). Empty when none failed.
        failed: Vec<(MemberKey, BuildFailure)>,
    },
}

/// Why a code commit was voided (logic.md 6.2a). The codes are pe-world's `VoidKind`'s.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum VoidedWhy {
    /// No quorum of its voters attested it by its deadline `D` (6.2a item 4).
    Unattested = 1,
    /// At its activation frame, its re-key would lose data on an object its committer may not edit (world-model.md 7.9).
    RekeyBlockedAtActivation = 2,
    /// At its activation frame, its re-key would move more rows than pe-world allows.
    RekeyTooLarge = 3,
    /// At its activation frame, its author no longer held `EDIT`.
    AuthorRevokedAtActivation = 4,
    /// The voters that attested, plus those still members that had neither attested nor failed, could no longer
    /// reach the quorum (6.2a item 4, for `BuildFailed` and for Leave).
    QuorumUnreachable = 5,
}

impl VoidedWhy {
    pub fn code(self) -> u8 {
        self as u8
    }

    pub fn from_code(code: u8) -> Option<VoidedWhy> {
        match code {
            1 => Some(VoidedWhy::Unattested),
            2 => Some(VoidedWhy::RekeyBlockedAtActivation),
            3 => Some(VoidedWhy::RekeyTooLarge),
            4 => Some(VoidedWhy::AuthorRevokedAtActivation),
            5 => Some(VoidedWhy::QuorumUnreachable),
            _ => None,
        }
    }
}

/// Why a member's build of a pending version failed (logic.md 6.2a item 2): the reason a `BuildFailed` entry carries.
/// The codes are fixed, since pe-world writes them into the entry and into the hashed pending row (@world-model
/// m97880a492d78), and pe-world's op uses this type.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum BuildFailure {
    /// The compiler refused the source.
    Compile = 1,
    /// The build service refused what the build read (logic.md 6.2b's dep-info check).
    Refused = 2,
    /// The build passed the memory cap (6.2a item 5).
    Cap = 3,
    /// The module or the manifest differs from the author's, after a clean rebuild.
    Mismatch = 4,
    /// The machine failed with no compiler diagnostic (a disk, an antivirus, a killed process), after its retries.
    Local = 5,
}

impl BuildFailure {
    pub fn code(self) -> u8 {
        self as u8
    }

    pub fn from_code(code: u8) -> Option<BuildFailure> {
        match code {
            1 => Some(BuildFailure::Compile),
            2 => Some(BuildFailure::Refused),
            3 => Some(BuildFailure::Cap),
            4 => Some(BuildFailure::Mismatch),
            5 => Some(BuildFailure::Local),
            _ => None,
        }
    }

    /// A short name for history lines and notices (@api-eng, m3b931a15736e).
    pub fn name(self) -> &'static str {
        match self {
            BuildFailure::Compile => "compile error",
            BuildFailure::Refused => "refused input",
            BuildFailure::Cap => "memory cap",
            BuildFailure::Mismatch => "module mismatch",
            BuildFailure::Local => "local fault",
        }
    }
}

/// This replica's standing with the quorum, so a reader never infers it from two frame numbers (S5 v1.5;
/// @coordinator-opus md52a099ab4c1, on @fable-reviewer's advice).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Health {
    /// My root matched at my newest final cut, so `final_frame` moves with the final cuts.
    #[default]
    Agreed,
    /// My root differed from the quorum's at the final cut at frame `since`, or could not be computed there (rung 2
    /// is due), and no later final cut has agreed: a repair is under way, and `final_frame` waits at my last agreed
    /// cut. pe-render shows "resyncing" once this lasts about 1-2 s; agents see it in WorldApi's session.
    Repairing { since: Frame },
}

/// The frame view.
#[derive(Clone, Debug, PartialEq)]
pub struct FrameView {
    /// The state this view shows: the domain at frame `frame`, after every frame below it.
    pub frame: Frame,
    pub domain: DomainId,
    /// The host time of this frame on the shared time base.
    pub sim_time: HostTime,
    /// The frame of my last agreed cut (`Cv`): the newest final cut at which my root matched the quorum's. The verdict of
    /// an entry stamped below it is final and agrees with every honest replica's; an entry at this frame is not covered
    /// yet. It never goes backwards, and while `health` is `Repairing` it waits (S5 v1.5). `None` until my root first
    /// matches at a final cut.
    pub final_frame: Option<Frame>,
    /// Whether my root agrees with the quorum's, or a repair is under way (S5 v1.5).
    pub health: Health,
    pub local_rig: RigTransform,
    /// Sorted by id.
    pub objects: Vec<ViewObject>,
    pub effects: Vec<Effect>,
    /// Keys of effects a replay removed: a sustained effect stops, faded.
    pub retracted: Vec<EffectKey>,
    pub counters: ViewCounters,
    /// The entries stepped since the last publication, with their outcomes; complete at `final_frame` (see
    /// [`StepRecord`]).
    pub changes: Vec<StepRecord>,
    /// The code commits activated or voided at the ends of the frames this view's steps covered, in step order, then
    /// asset order (S5 v1.8; logic.md 6.2a). Like `changes`, a record a reader takes in every view, in order: a
    /// re-simulated frame end publishes its records again, and a record with the same [`ActivationKey`] supersedes
    /// the earlier one, whatever its frame. So the author's prediction of its own commit (6.2a's option (i)), recorded
    /// at the commit's frame, is replaced by the agreed record or the void. Provisional above `final_frame`, as
    /// `changes` are.
    pub activations: Vec<Activation>,
    /// Keys of activation records an earlier view published that this view's steps no longer produce, and that no
    /// record in this view supersedes: a re-simulation without them, as when a late entry removes the attestation
    /// that completed a quorum, or the commit itself. A rung-2 import retracts the records views carried below its
    /// cut, even when the imported history has them: the importer never steps those frames again, and the outcome
    /// then shows only in the world's modules row (@vdw-physics, accepted for M1). The reader drops what it held for
    /// each key; a later record with the same key may follow (S5 v1.8; @vdw-physics m2bd29ef3f549, as
    /// [`FrameView::retracted`] does for effects).
    pub activations_retracted: Vec<ActivationKey>,
}

impl FrameView {
    /// An object by id (the objects are sorted by id).
    pub fn object(&self, id: ObjectId) -> Option<&ViewObject> {
        self.objects.binary_search_by(|o| o.id.cmp(&id)).ok().map(|i| &self.objects[i])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_codes_round_trip() {
        for c in 1..=5u8 {
            assert_eq!(BuildFailure::from_code(c).map(BuildFailure::code), Some(c));
            assert_eq!(VoidedWhy::from_code(c).map(VoidedWhy::code), Some(c));
        }
        assert_eq!((BuildFailure::from_code(0), BuildFailure::from_code(6)), (None, None));
        assert_eq!((VoidedWhy::from_code(0), VoidedWhy::from_code(6)), (None, None));
        assert_eq!(BuildFailure::Mismatch.name(), "module mismatch");
    }

    #[test]
    fn flags_compose() {
        let f = ViewFlags::default().with(ViewFlags::OWN_HAND).with(ViewFlags::CORRECTED);
        assert!(f.contains(ViewFlags::OWN_HAND));
        assert!(f.contains(ViewFlags::CORRECTED));
        assert!(!f.contains(ViewFlags::HELD_LOCAL));
        assert!(!f.contains(ViewFlags::RELEASED_LOCAL));
        assert!(!f.contains(ViewFlags::HELD_AWAY));
        let all = [
            ViewFlags::OWN_HAND,
            ViewFlags::HELD_LOCAL,
            ViewFlags::CORRECTED,
            ViewFlags::RELEASED_LOCAL,
            ViewFlags::HELD_AWAY,
        ];
        assert_eq!(all.iter().fold(0, |bits, f| bits | f.0), 0b11111, "each flag its own bit");
    }
}
