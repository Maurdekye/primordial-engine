//! pe-world's [`World`] as the replica's [`WorldModel`] (replica.md 8.1; world-model.md 9.2), agreed with
//! @world-model. pe-world's side is its `replica` calls (its slice A).
//!
//! - **The domain:** pe-world has none, so [`PeWorld`] carries it.
//! - **`apply`:** edits go through `World::apply_edit`, the one edit path, and inputs through `World::apply_input`
//!   (pe-world's slice B). The `Footprint` becomes the physics [`Effects`] here by comparing each affected body's row
//!   before and after the entry: a body that appears is `Created`, one that goes is `Removed`, changed shapes or
//!   fields are `Rebuilt`, and a moved world pose is `Teleported`. A body whose shapes and pose both changed gets
//!   `Rebuilt` and then `Teleported`, since a rebuild keeps a moving body's physics pose. A physical object that
//!   cannot be built ([`Unbuildable`]) has no body: it is `Created` when it becomes physical and `Rebuilt` when the
//!   body it had can no longer be built, and the replica counts a fault for each (replica.md 2.1). Affected bodies:
//!   those of every object the entry created, removed, wrote a row of or reparented, with each object's body being
//!   itself or its nearest ancestor with a `Body` (a compound child's), before and after; for a `Transform` write or a
//!   reparent, those of the whole subtree. A hand input writes its hand's `Transform` for its own frame, so it counts
//!   as one: a body under the hand moves with it on that frame. The pack's later poses are written by `end_frame`,
//!   which gives no effects, so such a body does not follow them yet (@design-reviewer's Medium on e6d3ee9c). Grabs
//!   hold by claims, not parenting, and the M1 ball pit puts no body under a hand. An input's pushes become [`PushFx`],
//!   one for one.
//! - **Entry classes:** edits and inputs as above. A `Leave` (a Protocol entry, `pe_seams::LEAVE`) goes through
//!   `World::leave` at its stamp ([`PeWorld`]'s `leave`), and an `Admit` (a Protocol entry, `pe_seams::Admit`)
//!   through `World::admit` at its stamp ([`PeWorld`]'s `admit`). Other Governance and Protocol entries are
//!   `Applied` with no effects, as pe-world applies none yet.
//! - **The order within a frame:** [`WorldModel::phase`] is pe-world's `input_phase`, so a frame's hand inputs apply
//!   before its other entries, and a grab's reach is measured after the frame's hand movement (world-model.md 8.1).
//! - **`end_frame(f)`:** `World::end_frame(f)`, which expires claims and push windows and writes each pending hand
//!   pack's pose for `f + 1`, then the frame becomes `f + 1`.
//!   `World::frame()` is the next frame to step, so a cut's root is taken when it equals the cut's frame.
//! - **The tree** (pe-world's slice C, world-model.md 6.2-6.6): `root` is the world root of pe-world's hash tree for
//!   this domain, `tree` is pe-world's `WorldTree` of a saved world, and `import` is `World::import`, which checks
//!   every record against the target root before anything applies and takes every setting from the config record.
//!   The replica compares the patched root with the target, which covers absent leaves and a missing config record.
//!   `empty` is a bare `World::new` with the engine registry and default settings but this machine's rate, which a
//!   joiner imports into.
//! - **Hands:** every `Hand` row, in id order. The target is the hand object's world transform, which its inputs
//!   write (and `end_frame` writes a pending pack's next pose). The rest is the row's: `span` counts frames, and
//!   `previous` is the input before, when there was one (@world-model). A hand whose
//!   object has no world transform is left out.
//! - **Claims:** every claim, with its hand. An agent's grab has no hand (`ObjectId::NONE`), so it gets no joint.
//! - **The view** (S5): every object with a `Shape`, a `Light` or a `Hand`, at its world transform, with its
//!   shape in its own units (the view's scale scales it), appearance, light, hand owner and holder. An object with no
//!   world transform (no `Transform`, or out of range) is not drawn; a hand without a `Shape` is drawn as a
//!   [`HAND_RADIUS`] sphere. A compound child without a body of its own is published with no velocity (nothing in
//!   M1's ball pit is compound). A spot light carries its cone angles from pe-world's `Light` into S5's `Spot`.
//!
//! **Shapes** (replica.md 2.1): a body's shapes are its own `Shape`, then those of its compound children (the
//! descendants reached without passing another `Body`) in id order, each placed in the body's frame and scaled by
//! its object's world scale. A child's place composes the local transforms from the body down to it, starting from
//! the body's world scale, so it does not depend on the body's pose; an object without a `Transform` sits at its
//! frame (world-model.md 3.3). Boxes scale per axis. A sphere or capsule under non-uniform scale, or an unknown kind,
//! cannot be built ([`Unbuildable::Shape`]), and neither can an unknown body kind or a body with no world pose: the
//! object is physical, [`WorldModel::body`] says why it cannot be built, and it has no body. Meshes and hulls are
//! built from their blobs, in a world whose blob-shapes setting is on (`blob_part`; CCD part 2 D): a mesh at its
//! body's origin, unturned, with the signed world scale, and a hull's points placed into the body's frame.

use std::any::Any;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use pe_seams::{
    Activation, ActivationKey, ActivationOutcome, Class, DomainId, EntryRef, Frame, Hash, Holder, ImportError,
    LightKind, MemberKey, ObjectClass, ObjectId, Outcome, Pos, Reason, SignedEntry, Stage, Tree, Version, ViewFlags,
    ViewObject, VoidedWhy,
};
use pe_sim::transform::rotate;
use pe_sim::{Component, ComponentId, SimStore, Transform, WorldTransform};
use pe_world::components::{
    Appearance, Body, BodyState, Hand, Light, Player, Shape, body_kind, light_kind, shape_kind,
};
use pe_world::edit::{AssetTarget, Op};
use pe_world::script::AssetId;
use pe_world::{Footprint, FrameEnd, RekeyReport, VoidKind, World, input_phase};

use crate::blobs::{BlobShapes, Built, Read};
use crate::logic::{ScriptRow, ScriptRunner, StageWorld};
use crate::world::{
    BodyKind, BodyRow, BodyStateRow, Change, ClaimRow, Effects, HAND_RADIUS, HandRow, LogicFx, Phase, PhysicsSettings,
    PlayerRow, PushFx, ShapePart, ShapeReason, ShapeRow, ThrowFx, Unbuildable, WorldModel,
};

const IDENTITY_ROTATION: [f32; 4] = [0.0, 0.0, 0.0, 1.0];

/// pe-world's world, for one domain.
#[derive(Clone, Debug)]
pub struct PeWorld {
    world: World,
    domain: DomainId,
    /// The author's prediction of its own code commit: this machine's state, never the world's, so it stays across
    /// rollbacks and restores.
    predict: Predict,
    /// The meshes and hulls built from their blobs, by blob (crate::blobs): this machine's cache, never the world's,
    /// so it stays across rollbacks and restores too.
    shapes: BlobShapes,
}

/// The author's prediction of its own code commit (logic.md 6.2a's option (i); @coordinator-opus).
/// - **Start:** an edit of the local member's that commits a version and leaves it pending without a quorum, at its
///   frame c. It runs from c + 1, by `World::activate_for_prediction` right after `World::end_frame(c)`, in every step
///   and replay of frame c while the prediction stands. No root this machine votes or finalizes with ever runs it: a
///   re-step starts from a state that never ran it (`WorldModel::predicted_from`, C4), so it votes on time, with no
///   hold, and a slow attester never holds cuts.
/// - **End:** another member's `AttestBuild` or `BuildFailed` of the version, whatever its outcome, a `Leave`
///   (any of them can decide the commit under the rule: the shared void rule runs before D), or reaching D. The
///   replica then steps again from c under the rule, at once ([`WorldModel::take_replay_from`]): the old code up to
///   F and the new code after, or the old code throughout on a void. A commit whose prediction ended is not predicted
///   again in that replay or later ones. At n = 2 the other member's attestation completes the quorum, so nothing is
///   left to predict after it. One prediction at a time.
/// - A prediction that can't activate (`None`: nothing live is pending; `Some(Err)`: its re-key would fail) ends at
///   once with no replay: this history already runs the old code.
#[derive(Clone, Debug, Default)]
struct Predict {
    local: Option<MemberKey>,
    now: Option<Prediction>,
    /// The commits whose prediction ended.
    ended: BTreeSet<(AssetId, Version)>,
    /// Where the replica steps again from, once taken.
    replay_from: Option<Frame>,
    /// The first frame the ended prediction's preview ran from, until its replay is taken (`predicted_from`).
    ended_from: Option<Frame>,
    /// The replica is stepping off its live history: nothing here changes, and nothing is predicted.
    scratch: bool,
    /// The live frame being stepped is canonical, a rung-1 replay's below its cut: no prediction activates in it,
    /// though its entries are watched as anywhere live (C4).
    canonical: bool,
}

#[derive(Clone, Copy, Debug)]
struct Prediction {
    asset: AssetId,
    /// The commit: its entry and op.
    version: Version,
    /// The version hash V the attestations name.
    v: Hash,
    c: Frame,
    deadline: Frame,
    /// The first live frame whose start ran the preview: c + 1, or r + 1 after it was activated again at r (C4).
    from: Frame,
}

impl PeWorld {
    pub fn new(world: World, domain: DomainId) -> PeWorld {
        PeWorld { world, domain, predict: Predict::default(), shapes: BlobShapes::default() }
    }

    pub fn world(&self) -> &World {
        &self.world
    }

    /// The world, for trusted construction (genesis and tests) before the replica takes it.
    pub fn world_mut(&mut self) -> &mut World {
        &mut self.world
    }

    /// A member's graceful leave at its entry's stamp (agreement.md 8; world-model.md 2.4): pe-world removes the
    /// leaver's hands, moving their children up with their world poses kept, ends its claims and drops its permission
    /// bits. The physics follows as it would an edit that removed the hands and reparented their children: the
    /// children's bodies through the effects, and the hands' bodies and grab joints at the frame's hand sync, which
    /// builds both from the world's hands and claims. A `Leave` that S3 voids (`LastMember`, or one stepped before
    /// its void is known) is dropped like any voided entry, and the re-simulation brings the hands back.
    fn leave(&mut self, entry: &SignedEntry) -> (Outcome, Effects) {
        let before = self.world.clone();
        match self.world.leave(entry.entry.author, entry.id()) {
            Ok(report) => {
                // The leaver's player body goes after its hands (world-model.md 8.2): the replica's player for it goes
                // at the next frame's player sync, as a hand's body does at the hand sync.
                let footprint = Footprint {
                    removed: report.hands.into_iter().chain(report.bodies).collect(),
                    reparented: report.moved.into_iter().map(|id| (id, true)).collect(),
                    ..Footprint::default()
                };
                (applied(), effects(&before, &self.world, &footprint, &self.shapes))
            }
            // A key with no permission bits: S3 admits no such author, and pe-world changed nothing.
            Err(_) => (Outcome::Refused { op: None, reason: Reason::NoPermission }, Effects::default()),
        }
    }

    /// Stage 1 for one entry, by its class (module doc).
    fn apply_entry(&mut self, frame: Frame, entry: &SignedEntry) -> (Outcome, Effects) {
        match entry.entry.class {
            class @ (Class::Edit | Class::Input) => {
                let before = self.world.clone();
                let (outcome, footprint) = if class == Class::Edit {
                    self.world.apply_edit(frame, entry)
                } else {
                    self.world.apply_input(frame, entry)
                };
                let effects = match outcome {
                    Outcome::Applied { .. } => effects(&before, &self.world, &footprint, &self.shapes),
                    _ => Effects::default(),
                };
                (outcome, effects)
            }
            Class::Protocol if pe_seams::is_leave(entry) => self.leave(entry),
            Class::Protocol => match pe_seams::Admit::of(entry) {
                Some(admit) => self.admit(frame, entry, admit),
                None => (applied(), Effects::default()),
            },
            Class::Governance => (applied(), Effects::default()),
        }
    }

    /// A member's join (agreement.md 8; app.md 4, "Joining from a cut"): an `Admit` that S3 makes effective at its
    /// place in the final log. pe-world records the key with the role the entry names and creates its two hands, owned
    /// by it, at the entry's frame (`World::admit`), so every replica holds the joiner from the same frame, and the
    /// world at the reconfiguring cut E, which E's certificate covers, holds it for the joiner's import. Who may admit
    /// is S3's governance fold: an `Admit` S3 voids (`NotAuthorized`, `NoChange`) and this replica stepped before its
    /// void was known is dropped like any voided entry. Refused here, as S3 voids it: a malformed payload. A key that
    /// is a member of the world already is a returning member (F3's n = 2 branch: B on the new line, which S3's epoch
    /// doesn't hold yet): `World::admit` keeps its hands and permissions and spawns nothing. In a session's own line S3
    /// voids such an `Admit` (`NoChange`), so stepping it before the void is known changes nothing.
    /// The hands need no effects: the frame's hand sync builds their bodies from the world's hands, as for a `Leave`.
    fn admit(&mut self, frame: Frame, entry: &SignedEntry, admit: Option<pe_seams::Admit>) -> (Outcome, Effects) {
        let refused = |reason| (Outcome::Refused { op: None, reason }, Effects::default());
        let Some(admit) = admit else { return refused(Reason::Malformed) };
        // Where it stands: the lowest slot of the genesis row no hand takes, read from the world as it stands when this
        // `Admit` applies (pe-world's `World::join_spawn`, world-model.md 2.4).
        let spawn = self.world.join_spawn();
        match self.world.admit(entry.entry.line, admit.key, admit.role, frame, spawn) {
            Ok(_) => (applied(), Effects::default()),
            Err(_) => refused(Reason::Invalid { field: None }),
        }
    }

    /// The author's prediction ([`Predict`]) around one applied entry. A commit of the local member's starts one. Once
    /// one stands, another member's attestation or build failure of its version, or a `Leave`, ends it, whatever the
    /// entry's outcome: in the predicted history an attestation is refused `Stale`, since the pending record is gone.
    fn watch(&mut self, frame: Frame, entry: &SignedEntry, outcome: &Outcome) {
        if self.predict.scratch {
            return;
        }
        if let Some(p) = self.predict.now {
            // Only from c on: a rollback past an earlier entry, a third member's `Leave` say, applies it again.
            if frame >= p.c && (pe_seams::is_leave(entry) || decides(entry, &p, self.predict.local)) {
                self.end_prediction();
            }
            return;
        }
        let applied = matches!(outcome, Outcome::Applied { .. });
        if !applied || entry.entry.class != Class::Edit || self.predict.local != Some(entry.entry.author) {
            return;
        }
        // A re-issued edit's new ids derive from its original's coordinates (world-model.md 2.3, F3 L3).
        let Some((tx, o)) = pe_world::branch::decode_edit(entry) else { return };
        for (i, op) in tx.ops.iter().enumerate() {
            let Op::CommitScript { script, .. } = op else { continue };
            // The asset it commits to: an existing one, or a new one's id derived from the entry's origin, its
            // original's coordinates when re-issued (world-model.md 7.9, 2.4).
            let asset = match *script {
                AssetTarget::Existing(a) => a,
                AssetTarget::New(k) => pe_world::script::asset_created(o.line, entry.entry.author, o.chain, o.seq, k),
            };
            // Its version names its origin's entry, the original when re-issued, as pe-world writes it (2.4).
            let version = Version { entry: o.entry, op: i as u32 };
            let pending = self.world.pending().get(&asset);
            let live = pending.filter(|p| p.version == version && p.void.is_none() && !p.attested_by_quorum());
            if let Some(p) = live.filter(|_| !self.predict.ended.contains(&(asset, version))) {
                let (v, deadline, from) = (p.v(), p.deadline, frame.next());
                self.predict.now = Some(Prediction { asset, version, v, c: frame, deadline, from });
                return;
            }
        }
    }

    /// Ends the prediction that stands: its commit is not predicted again, and the replica steps again from c.
    fn end_prediction(&mut self) {
        if let Some(p) = self.predict.now.take() {
            self.predict.ended.insert((p.asset, p.version));
            self.predict.replay_from = Some(self.predict.replay_from.map_or(p.c, |f| f.min(p.c)));
            self.predict.ended_from = Some(self.predict.ended_from.map_or(p.from, |f| f.min(p.from)));
        }
    }

    /// Whether this state lacks the standing prediction's preview: its exact version is still pending here, live and
    /// short of a quorum, so nothing activated it in this history (C4's fallback).
    fn unpreviewed(&self, p: &Prediction) -> bool {
        let pending = self.world.pending().get(&p.asset);
        pending.is_some_and(|q| q.version == p.version && q.void.is_none() && !q.attested_by_quorum())
    }
}

/// Whether `entry` is another member's `AttestBuild` or `BuildFailed` of `p`'s version: it can decide the commit under
/// the rule (logic.md 6.2a), so the prediction ends.
fn decides(entry: &SignedEntry, p: &Prediction, local: Option<MemberKey>) -> bool {
    if entry.entry.class != Class::Edit || local == Some(entry.entry.author) {
        return false;
    }
    let Some((tx, _)) = pe_world::branch::decode_edit(entry) else { return false };
    tx.ops.iter().any(|op| match op {
        Op::AttestBuild { script, version, .. } | Op::BuildFailed { script, version, .. } => {
            *script == p.asset && *version == p.v
        }
        _ => false,
    })
}

/// S5's records of what pe-world's `end_frame(frame)` did with pending code commits (S5 v1.8; logic.md 6.2a).
fn activations(frame: Frame, end: &FrameEnd) -> Vec<Activation> {
    let activated_ =
        end.activated.iter().map(|(asset, version, report)| activated(*asset, *version, frame, report.as_ref()));
    let voided = end.voided.iter().map(|v| Activation {
        key: ActivationKey { asset: v.asset, version: v.version },
        frame,
        outcome: ActivationOutcome::Voided {
            why: voided_why(v.kind),
            objects: v.blocked.as_ref().map(|b| b.objects.clone()).unwrap_or_default(),
            more: v.blocked.as_ref().map_or(0, |b| b.more),
            failed: v.failed.clone(),
        },
    });
    activated_.chain(voided).collect()
}

/// An activation's record: the objects its re-key moved (pe-world's report, in id order), none without a re-key.
fn activated(asset: AssetId, version: Version, frame: Frame, report: Option<&RekeyReport>) -> Activation {
    let rekeyed = report.map(|r| r.objects.clone()).unwrap_or_default();
    Activation { key: ActivationKey { asset, version }, frame, outcome: ActivationOutcome::Activated { rekeyed } }
}

/// pe-world's void kinds as S5 names them: the same five, with the same codes.
fn voided_why(kind: VoidKind) -> VoidedWhy {
    match kind {
        VoidKind::Unattested => VoidedWhy::Unattested,
        VoidKind::RekeyBlockedAtActivation => VoidedWhy::RekeyBlockedAtActivation,
        VoidKind::RekeyTooLarge => VoidedWhy::RekeyTooLarge,
        VoidKind::AuthorRevokedAtActivation => VoidedWhy::AuthorRevokedAtActivation,
        VoidKind::QuorumUnreachable => VoidedWhy::QuorumUnreachable,
    }
}

/// An entry applied with nothing to report: a protocol or governance entry's outcome.
fn applied() -> Outcome {
    Outcome::Applied { created: Vec::new(), overwrote: Vec::new(), notes: Vec::new(), inverse: None }
}

impl WorldModel for PeWorld {
    type Saved = World;

    fn domain(&self) -> DomainId {
        self.domain
    }

    /// pe-world's `input_phase`: 0, a hand input, is `First`; anything else is `Second`.
    fn phase(entry: &SignedEntry) -> Phase {
        if input_phase(entry) == 0 { Phase::First } else { Phase::Second }
    }

    fn apply(&mut self, frame: Frame, entry: &SignedEntry) -> (Outcome, Effects) {
        let done = self.apply_entry(frame, entry);
        self.watch(frame, entry, &done.0);
        done
    }

    fn end_frame(&mut self, frame: Frame) -> Vec<Activation> {
        let end = self.world.end_frame(frame);
        let mut acts = activations(frame, &end);
        if let Some(p) = self.predict.now.filter(|_| !self.predict.scratch) {
            // At c the preview starts. Past it, the fallback (C4, @coordinator-opus): a rebase onto a
            // state without it (a voided late entry, an import, a rung-1 replay) activates the same version again,
            // once per such history, with no second report, since the first at c stands.
            let again = frame > p.c && frame < p.deadline && self.unpreviewed(&p);
            if self.predict.canonical {
                // A canonical frame never runs the preview; past D the prediction ends as anywhere.
                if frame > p.c && frame >= p.deadline {
                    self.end_prediction();
                }
            } else if frame == p.c || again {
                match self.world.activate_for_prediction(p.asset, frame) {
                    Some(Ok(report)) => {
                        if frame == p.c {
                            acts.push(activated(p.asset, p.version, frame, report.as_ref()));
                        }
                        if let Some(now) = self.predict.now.as_mut() {
                            now.from = frame.next();
                        }
                    }
                    _ => {
                        self.predict.now = None;
                        self.predict.ended.insert((p.asset, p.version));
                    }
                }
            } else if frame >= p.deadline {
                self.end_prediction();
            }
        }
        // Asset order within the frame (S5 v1.8), activated and voided together; the sort is stable.
        acts.sort_by_key(|a| a.key.asset);
        self.world.set_frame(frame.next());
        acts
    }

    fn set_local(&mut self, local: Option<MemberKey>) {
        self.predict.local = local;
    }

    fn take_replay_from(&mut self) -> Option<Frame> {
        self.predict.ended_from = None;
        self.predict.replay_from.take()
    }

    fn predicted_from(&self) -> Option<Frame> {
        let now = self.predict.now.map(|p| p.from);
        match (now, self.predict.ended_from) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    fn set_canonical(&mut self, canonical: bool) {
        self.predict.canonical = canonical;
    }

    fn set_scratch(&mut self, scratch: bool) {
        self.predict.scratch = scratch;
    }

    fn scripted(&self) -> bool {
        !self.world.modules().is_empty()
    }

    /// pe-world's hooks around the runner: `World::logic_stage` hands it the store, the guard, the faulted assets and
    /// the row check; `World::after_logic` records what it reported and gives the footprint, whose physics effects are
    /// found as an entry's are (module doc), with every `Transform` write and removal the runner reported. Each event
    /// is placed at its object's world position after the stage, or where it was at the stage's start if the stage
    /// removed it.
    fn logic_stage(&mut self, frame: Frame, stage: Stage, runner: &mut dyn ScriptRunner) -> LogicFx {
        let scripts: Vec<ScriptRow> = self
            .world
            .modules()
            .iter()
            .filter(|(_, m)| m.activate_at <= frame)
            .map(|(&asset, m)| ScriptRow {
                asset,
                module: m.author_module,
                version: m.v(),
                components: m.components.clone(),
            })
            .collect();
        if scripts.is_empty() {
            return LogicFx::default();
        }
        let (line, rate_hz) = (self.world.line(), self.world.rate().hz());
        let before = self.world.clone();
        let out = self.world.logic_stage(|store, guard, faulted, check| {
            let world = StageWorld { frame, line, rate_hz, guard, faulted, check };
            runner.run_stage(stage, &scripts, store, &world)
        });
        let mut footprint = self.world.after_logic(frame, &out.report);
        // `after_logic` names a moved or removed object only if it has a body. The physics needs the rest too: a moved
        // parent without one carries the bodies beneath it, and a removed shape changes its owner's body
        // (@engine-architect's Medium on 4ac9019f).
        // Sets for the membership tests, so a stage moving thousands of objects stays linear; the lists keep their order.
        let mut rows: BTreeSet<(ObjectId, ComponentId)> = footprint.rows.iter().copied().collect();
        for &(c, o) in &out.report.written {
            if c == Transform::ID && rows.insert((o, c)) {
                footprint.rows.push((o, c));
            }
        }
        let mut removed: BTreeSet<ObjectId> = footprint.removed.iter().copied().collect();
        for &o in &out.report.removed {
            if removed.insert(o) {
                footprint.removed.push(o);
            }
        }
        let effects = effects(&before, &self.world, &footprint, &self.shapes);
        let (now, then) = (self.world.store(), before.store());
        let events = (out.events.into_iter())
            .map(|e| {
                // An emitter the stage removed plays where it was at the stage's start.
                let t = now.world_transform(e.object).or_else(|| then.world_transform(e.object));
                let at = t.map_or(Pos::ORIGIN, |t| t.pos);
                (e, at)
            })
            .collect();
        LogicFx { effects, events, host_faults: out.host_faults, fp_resets: out.fp_resets, missing: out.missing }
    }

    fn body(&self, id: ObjectId) -> Result<Option<BodyRow>, Unbuildable> {
        body_row(&self.world, &self.shapes, id)
    }

    fn physical(&self) -> Vec<ObjectId> {
        self.world.physical()
    }

    fn hands(&self) -> Vec<HandRow> {
        let store = self.world.store();
        let mut hands: Vec<HandRow> = store
            .iter::<Hand>()
            .filter_map(|(hand, h)| {
                let at = store.world_transform(hand)?;
                Some(HandRow {
                    hand,
                    owner: h.owner,
                    target: at.pos,
                    rotation: at.rot,
                    grip: h.grip,
                    engaged: h.engaged != 0,
                    stamp: Frame(h.stamp),
                    span: h.span,
                    previous: (h.has_prev != 0).then_some((Frame(h.prev_stamp), h.prev_span)),
                    mode: h.mode,
                    strength: h.strength,
                })
            })
            .collect();
        hands.sort_unstable_by_key(|h| h.hand);
        hands
    }

    /// The lowest free spawn slot, where a member admitted now would stand (pe-world's `World::join_spawn`).
    fn respawn_point(&self) -> Option<Pos> {
        Some(self.world.join_spawn())
    }

    fn players(&self) -> Vec<PlayerRow> {
        let store = self.world.store();
        self.world
            .players()
            .into_iter()
            .filter_map(|r| {
                let feet = store.world_transform(r.body)?.pos;
                let p = r.player;
                Some(PlayerRow {
                    body: r.body,
                    owner: r.owner,
                    feet,
                    stick: p.stick,
                    stick_yaw: p.stick_yaw,
                    heading: p.heading,
                    jump: p.jump != 0,
                    head_pos: p.head_pos,
                    head_rot: p.head_rot,
                    prev_head: p.prev_head,
                    tracked: p.tracked != 0,
                })
            })
            .collect()
    }

    fn claims(&self) -> Vec<ClaimRow> {
        self.world
            .claims()
            .map(|(object, c)| ClaimRow { object, hand: c.hand, member: c.member, since: c.since })
            .collect()
    }

    fn settings(&self) -> PhysicsSettings {
        let s = self.world.settings();
        PhysicsSettings { rate: s.rate, gravity: s.gravity, origin: s.origin }
    }

    fn snapshot(&self, final_frame: Frame) -> Option<Arc<dyn Any + Send + Sync>> {
        Self::snapshot_of(&self.world, final_frame)
    }

    fn snapshot_of(saved: &World, final_frame: Frame) -> Option<Arc<dyn Any + Send + Sync>> {
        let snap: Arc<dyn Any + Send + Sync> = saved.snapshot(final_frame);
        Some(snap)
    }

    fn view_objects(&self) -> Vec<ViewObject> {
        let s = self.world.store();
        let mut ids: BTreeSet<ObjectId> = s.iter::<Shape>().map(|(id, _)| id).collect();
        ids.extend(s.iter::<Light>().map(|(id, _)| id));
        ids.extend(s.iter::<Hand>().map(|(id, _)| id));
        let held: BTreeMap<ObjectId, Holder> =
            self.world.claims().map(|(object, c)| (object, Holder { member: c.member, hand: c.hand })).collect();
        ids.into_iter()
            .filter_map(|id| {
                let t = s.world_transform(id)?;
                // pe-world writes a hand's side as 0 or 1; any other value is a bad row, drawn as no hand.
                let hand = s.get::<Hand>(id).filter(|h| h.side <= 1);
                // A player's body (world-model.md 8.2), or a drawn part of it: a child of the body that isn't a hand.
                let body_of = s.get::<Player>(id).map(|p| p.owner).or_else(|| {
                    s.parent(id).filter(|_| !s.has::<Hand>(id)).and_then(|b| s.get::<Player>(b)).map(|p| p.owner)
                });
                let own = s.get::<Shape>(id).filter(|sh| view_shape(sh).is_some());
                // A hand without a shape of its own is drawn as the sphere it will push with.
                let shape =
                    own.and_then(view_shape).or_else(|| hand.map(|_| pe_seams::Shape::Sphere { radius: HAND_RADIUS }));
                let radius = match (own, hand) {
                    (Some(sh), _) => shape_radius(sh, t.scale),
                    (None, Some(_)) => HAND_RADIUS * largest(t.scale),
                    (None, None) => 0.0,
                };
                let node = s.node(id);
                Some(ViewObject {
                    id,
                    class: ObjectClass::World,
                    pos: t.pos,
                    rotation: t.rot,
                    scale: t.scale,
                    linear_velocity: [0.0; 3],
                    angular_velocity: [0.0; 3],
                    radius,
                    shape,
                    appearance: s.get::<Appearance>(id).map(view_appearance),
                    light: s.get::<Light>(id).and_then(view_light),
                    hand_of: hand
                        .map(|h| (h.owner, if h.side == 0 { pe_seams::Hand::Left } else { pe_seams::Hand::Right })),
                    body_of,
                    held_by: held.get(&id).cloned(),
                    flags: ViewFlags::default(),
                    // The last edit that changed anything about the object (world-model.md 7.3), or none if no edit
                    // has written it: pe-world's GENESIS, for genesis and trusted engine code.
                    version: Some(Version { entry: EntryRef(Hash(node.version_entry)), op: node.version_op })
                        .filter(|v| *v != pe_world::GENESIS),
                })
            })
            .collect()
    }

    fn write_poses(&mut self, poses: &[(ObjectId, Pos, [f32; 4])]) -> Vec<ObjectId> {
        self.world.write_poses(poses)
    }

    fn maintain_transforms(&mut self) -> u32 {
        u32::try_from(self.world.maintain_transforms()).unwrap_or(u32::MAX)
    }

    fn write_physics(&mut self, rows: &[(ObjectId, BodyStateRow)], global: Hash) {
        let rows: Vec<(ObjectId, BodyState)> = rows
            .iter()
            .map(|&(id, r)| {
                let state = BodyState {
                    linear: r.linear,
                    angular: r.angular,
                    sleep_time: r.sleep_time,
                    awake: u8::from(r.awake),
                    _pad: [0; 3],
                    digest: r.digest,
                };
                (id, state)
            })
            .collect();
        self.world.write_physics(&rows, global);
        // The last write before a cut's root (the replica's take_root): store the leaf hash of every object written
        // since the last cut, so this root and later ones rehash only what changed (world-model.md 6.4;
        // @chunks-reviewer's Low on 5e7effd2). Roots are the same either way.
        self.world.refresh_tree_cache();
    }

    fn root(&self) -> Hash {
        self.world.root_hash(self.domain)
    }

    fn save(&self) -> World {
        self.world.clone()
    }

    fn restore(&mut self, saved: &World) {
        self.world = saved.clone();
    }

    fn tree<'a>(&'a self, saved: &'a World) -> Box<dyn Tree + 'a> {
        Box::new(saved.tree(self.domain))
    }

    /// A bare world (engine registry, default settings) at this machine's rate, the welcome's: pe-world's import
    /// refuses a config record at another rate than its base's (world-model.md 6.6), so a base at the default rate
    /// would refuse every world not at 60 Hz, whatever rate this replica runs at.
    fn empty(&self) -> World {
        let mut w = World::new(0, self.world.blobs().clone());
        w.set_rate(self.world.rate());
        w
    }

    fn import(&self, saved: &World, import: &pe_seams::CutImport) -> Result<World, ImportError> {
        saved.import(self.domain, import)
    }
}

/// pe-world's shape as the view's (S5), in the object's own units; `None` for an unknown kind or a missing mesh.
fn view_shape(sh: &Shape) -> Option<pe_seams::Shape> {
    match sh.kind {
        shape_kind::BOX => Some(pe_seams::Shape::Box { half_extents: sh.size }),
        shape_kind::SPHERE => Some(pe_seams::Shape::Sphere { radius: sh.size[0] }),
        shape_kind::CAPSULE => Some(pe_seams::Shape::Capsule { radius: sh.size[0], half_height: sh.size[1] }),
        shape_kind::MESH | shape_kind::HULL if !sh.mesh.is_none() => {
            Some(pe_seams::Shape::Mesh(pe_seams::BlobRef(Hash(sh.mesh.hash))))
        }
        _ => None,
    }
}

/// The bounding radius the view culls by (render.md 12): the shape's, world scale applied. A mesh's or a hull's
/// `size` is the half extents of its bounding box (world-model.md 8.1), so it is bounded like a box.
fn shape_radius(sh: &Shape, scale: [f32; 3]) -> f32 {
    match sh.kind {
        shape_kind::SPHERE => sh.size[0] * largest(scale),
        shape_kind::CAPSULE => (sh.size[0] + sh.size[1]) * largest(scale),
        _ => {
            let (x, y, z) = (sh.size[0] * scale[0], sh.size[1] * scale[1], sh.size[2] * scale[2]);
            (x * x + y * y + z * z).sqrt()
        }
    }
}

fn largest(scale: [f32; 3]) -> f32 {
    scale.iter().fold(0.0_f32, |m, s| m.max(s.abs()))
}

fn view_appearance(a: &Appearance) -> pe_seams::Appearance {
    pe_seams::Appearance {
        base_color: a.base,
        emissive: a.emissive,
        roughness: a.roughness,
        metalness: a.metalness,
        material: (!a.material.is_none()).then_some(pe_seams::BlobRef(Hash(a.material.hash))),
    }
}

/// pe-world's light as the view's: a spot light with its cone angles (radians from the object's local -z, which
/// pe-world keeps within 0 <= inner <= outer <= pi/2). `None` for a kind the view has no name for.
fn view_light(l: &Light) -> Option<pe_seams::Light> {
    let kind = match l.kind {
        light_kind::POINT => LightKind::Point,
        light_kind::SPOT => LightKind::Spot { inner_angle: l.inner_angle, outer_angle: l.outer_angle },
        light_kind::DIRECTIONAL => LightKind::Directional,
        _ => return None,
    };
    Some(pe_seams::Light { kind, color: l.color, intensity: l.intensity, range: l.range })
}

// ---- bodies ---------------------------------------------------------------------------------------------------------

/// The body row of `id`: `Ok(None)` if it is not physical (no `Body`, or no shape of its own or a compound child's:
/// exactly what pe-world's `World::physical` leaves out), and why it cannot be built if it is physical but has an
/// unknown body kind, no world pose, a shape that cannot be built, or no mass for a dynamic body with a mesh. A reason
/// every machine finds comes before a blob only this one lacks ([`Unbuildable::BlobAbsent`]).
fn body_row(world: &World, shapes: &BlobShapes, id: ObjectId) -> Result<Option<BodyRow>, Unbuildable> {
    let store = world.store();
    let Some(body) = store.get::<Body>(id) else {
        return Ok(None);
    };
    // A template is never physical (pe-world `templates`).
    if world.inert(id) {
        return Ok(None);
    }
    let own = store.get::<Shape>(id);
    let children: Vec<(ObjectId, &Shape)> =
        compound_children(world, id).into_iter().filter_map(|c| Some((c, store.get::<Shape>(c)?))).collect();
    if own.is_none() && children.is_empty() {
        return Ok(None);
    }
    let kind = match body.kind {
        body_kind::STATIC => BodyKind::Static,
        body_kind::KINEMATIC => BodyKind::Kinematic,
        body_kind::DYNAMIC => BodyKind::Dynamic,
        _ => return Err(Unbuildable::Kind),
    };
    let at = store.world_transform(id).ok_or(Unbuildable::Pose)?;
    let origin = WorldTransform { pos: Pos::ORIGIN, rot: IDENTITY_ROTATION, scale: at.scale };
    let mut parts = Vec::new();
    let mut absent = None;
    let mut add = |object: ObjectId, shape: &Shape, placed: &WorldTransform| {
        match part(world, shapes, object, shape, placed) {
            Ok(p) => parts.push(p),
            Err(a @ Unbuildable::BlobAbsent { .. }) => {
                absent.get_or_insert(a);
            }
            Err(e) => return Err(e),
        }
        Ok(())
    };
    if let Some(shape) = own {
        add(id, shape, &origin)?;
    }
    for (child, shape) in children {
        let placed = in_body(store, &origin, id, child).ok_or(Unbuildable::Pose)?;
        add(child, shape, &placed)?;
    }
    if let Some(absent) = absent {
        return Err(absent);
    }
    let row = BodyRow {
        kind,
        pos: at.pos,
        rotation: at.rot,
        shapes: parts,
        friction: body.friction,
        restitution: body.restitution,
        rolling_resistance: body.rolling_resistance,
        linear_damping: body.linear_damping,
        angular_damping: body.angular_damping,
        gravity_scale: body.gravity_scale,
        bullet: body.bullet != 0,
        may_sleep: body.may_sleep != 0,
        grabbable: body.grabbable != 0,
    };
    // A dynamic body with a mesh needs mass from its shapes together (world-model.md 3.2): pe-physics's own check,
    // which `add_body` makes again. Any other refusal of the body is `add_body`'s: a fault, without a reason here.
    let has_mesh = row.shapes.iter().any(|p| matches!(p.shape, ShapeRow::Mesh { .. }));
    if kind == BodyKind::Dynamic
        && has_mesh
        && let Err(pe_physics::PhysicsError::Massless { dropped }) = crate::bodies::desc(&row, row.pos).check_mass()
    {
        return Err(Unbuildable::Mass { dropped });
    }
    Ok(Some(row))
}

/// The descendants of `body` reached without passing another `Body`, in id order.
fn compound_children(world: &World, body: ObjectId) -> Vec<ObjectId> {
    let store = world.store();
    let mut found = Vec::new();
    let mut stack: Vec<ObjectId> = store.children(body).collect();
    while let Some(x) = stack.pop() {
        if store.has::<Body>(x) {
            continue;
        }
        found.push(x);
        stack.extend(store.children(x));
    }
    found.sort_unstable();
    found
}

/// Where a compound child sits in its body's frame: `origin` (the body's frame at `Pos::ORIGIN`, with the body's
/// world scale), composed with the local transforms of the objects from the body down to `child`. An object without
/// a `Transform` sits at its frame, so it adds nothing (chunks-reviewer's Medium and L1 on 48e4e521).
fn in_body(store: &SimStore, origin: &WorldTransform, body: ObjectId, child: ObjectId) -> Option<WorldTransform> {
    let mut chain = Vec::new();
    let mut at = child;
    while at != body {
        if let Some(t) = store.get::<Transform>(at) {
            chain.push(*t);
        }
        at = store.parent(at)?;
    }
    let mut placed = *origin;
    while let Some(t) = chain.pop() {
        placed = placed.compose(&t)?;
    }
    Some(placed)
}

/// One shape, `object`'s, at `placed`, its place in its body's frame and its world scale. The body's own shape sits at
/// the body's origin, unturned. A primitive is scaled by the scale's magnitude; a mesh or a hull keeps its signs, so a
/// negative axis mirrors it (@coordinator-opus, point 1).
fn part(
    world: &World,
    shapes: &BlobShapes,
    object: ObjectId,
    shape: &Shape,
    placed: &WorldTransform,
) -> Result<ShapePart, Unbuildable> {
    let center = (placed.pos - Pos::ORIGIN).0.map(|c| c as f32);
    let rotation = placed.rot;
    let scale = placed.scale.map(f32::abs);
    let uniform = scale[0] == scale[1] && scale[1] == scale[2];
    let size = shape.size;
    let row = match shape.kind {
        shape_kind::BOX => ShapeRow::Box {
            center,
            rotation,
            half_extents: [size[0] * scale[0], size[1] * scale[1], size[2] * scale[2]],
        },
        shape_kind::SPHERE if uniform => ShapeRow::Sphere { center, radius: size[0] * scale[0] },
        shape_kind::CAPSULE if uniform => {
            let axis = rotate(rotation, [0.0, f64::from(size[1] * scale[0]), 0.0]).map(|c| c as f32);
            ShapeRow::Capsule {
                center1: [center[0] - axis[0], center[1] - axis[1], center[2] - axis[2]],
                center2: [center[0] + axis[0], center[1] + axis[1], center[2] + axis[2]],
                radius: size[0] * scale[0],
            }
        }
        shape_kind::SPHERE | shape_kind::CAPSULE => return Err(Unbuildable::Shape(ShapeReason::NonUniformScale)),
        shape_kind::MESH | shape_kind::HULL => blob_part(world, shapes, shape, placed, center)?,
        _ => return Err(Unbuildable::Shape(ShapeReason::UnknownKind)),
    };
    Ok(ShapePart { shape: row, density: shape.density, object })
}

/// A mesh or a hull, from its blob (world-model.md 3.2), at `placed`; `center` is its position as the build uses it.
/// In order: the world must build them (its blob-shapes setting, 6.6 config item 8), the shape must name a blob, a mesh
/// must sit at its body's origin, unturned (v1: Box3D's mesh has no local transform), every axis of the world scale
/// must be 1e-6 or more in magnitude (`pe_physics::limits::MIN_MESH_SCALE`), this machine must hold the blob (a local
/// condition, [`Unbuildable::BlobAbsent`]), and the blob must read and build.
fn blob_part(
    world: &World,
    shapes: &BlobShapes,
    shape: &Shape,
    placed: &WorldTransform,
    center: [f32; 3],
) -> Result<ShapeRow, Unbuildable> {
    let refused = |why| Err(Unbuildable::Shape(why));
    if !world.settings().blob_shapes {
        return refused(ShapeReason::SwitchOff);
    }
    if shape.mesh.is_none() {
        return refused(ShapeReason::NoBlob);
    }
    let mesh = shape.kind == shape_kind::MESH;
    // The composed place exactly the body's: the f32 position all ±0, and the rotation's x, y and z all ±0 with w ±1
    // (the composed rotation's sign is not canonical).
    let r = placed.rot;
    let at_origin = center.iter().all(|&c| c == 0.0) && r[..3].iter().all(|&c| c == 0.0) && r[3].abs() == 1.0;
    if mesh && !at_origin {
        return refused(ShapeReason::MisplacedMesh);
    }
    let scale = placed.scale;
    // Under the floor, or not a number.
    if let Some(axis) = scale.iter().position(|s| s.is_nan() || s.abs() < pe_physics::limits::MIN_MESH_SCALE) {
        return refused(ShapeReason::ScaleFloor { axis: axis as u8 });
    }
    let Some(bytes) = world.blobs().get(&shape.mesh) else {
        return Err(Unbuildable::BlobAbsent { blob: Hash(shape.mesh.hash) });
    };
    match shapes.get(if mesh { Read::Mesh } else { Read::Hull }, shape.mesh.hash, &bytes) {
        Built::Mesh(mesh) => Ok(ShapeRow::Mesh { mesh, scale }),
        // Each point scaled on the shape's own axes, then turned, then offset into the body's frame.
        Built::Hull(points) => {
            let at = (placed.pos - Pos::ORIGIN).0;
            let place = |p: &[f32; 3]| {
                let turned = rotate(placed.rot, [0, 1, 2].map(|k| f64::from(p[k] * scale[k])));
                [0, 1, 2].map(|k| (at[k] + turned[k]) as f32)
            };
            let points: Vec<[f32; 3]> = points.iter().map(place).collect();
            // A scale other than 1 or -1 on an axis can flatten a hull past what the builder takes, or take it past
            // the size limit: checked here as add_body checks it, so the reason is named (R3). One more build, for a
            // scaled hull only. A turn or an offset can't flatten a hull; a child offset past the size limit (10 km
            // out) is still refused by add_body alone, without a reason.
            if scale.iter().any(|s| s.abs() != 1.0) && pe_physics::check_hull(&points).is_err() {
                return refused(ShapeReason::ScaledHull);
            }
            Ok(ShapeRow::Hull { points })
        }
        Built::Bad => refused(ShapeReason::BadBlob),
    }
}

// ---- effects --------------------------------------------------------------------------------------------------------

/// An applied entry's physics effects, from the bodies it affected (module doc), in id order.
fn effects(before: &World, after: &World, footprint: &Footprint, shapes: &BlobShapes) -> Effects {
    let mut touched: BTreeSet<ObjectId> = BTreeSet::new();
    let mut moved: Vec<ObjectId> = Vec::new();
    touched.extend(&footprint.created);
    touched.extend(&footprint.removed);
    for &(id, component) in &footprint.rows {
        touched.insert(id);
        if component == Transform::ID {
            moved.push(id);
        }
    }
    for &(id, _) in &footprint.reparented {
        touched.insert(id);
        moved.push(id);
    }
    // A root renamed to or from a template folder: every body under it became inert or live.
    for &top in &footprint.regrouped {
        for world in [before, after] {
            touched.extend(subtree(world, top));
        }
    }
    // A hand input writes its hand's `Transform`.
    for &id in &footprint.hands {
        touched.insert(id);
        moved.push(id);
    }
    for top in moved {
        for world in [before, after] {
            touched.extend(subtree(world, top));
        }
    }
    let bodies: BTreeSet<ObjectId> =
        touched.iter().flat_map(|&id| [owner(before, id), owner(after, id)]).flatten().collect();
    let mut changed = Vec::new();
    for id in bodies {
        // A physical object that cannot be built has no body (replica.md 2.1). The replica tries to build one that
        // becomes physical so, and rebuilds one whose body can no longer be built, counting a fault each time; one
        // that stays unbuildable, or stops being physical, had no body and changes nothing.
        match (body_row(before, shapes, id), body_row(after, shapes, id)) {
            (Ok(None) | Err(_), Ok(Some(_))) | (Ok(None), Err(_)) => changed.push((id, Change::Created)),
            (Ok(Some(_)), Err(_)) => changed.push((id, Change::Rebuilt)),
            (Ok(Some(_)), Ok(None)) => changed.push((id, Change::Removed)),
            (Ok(Some(a)), Ok(Some(b))) => {
                let reshaped = BodyRow { pos: a.pos, rotation: a.rotation, ..b.clone() } != a;
                if reshaped {
                    changed.push((id, Change::Rebuilt));
                }
                if a.pos != b.pos || a.rotation != b.rotation {
                    changed.push((id, Change::Teleported));
                }
            }
            (Ok(None), Ok(None)) | (Err(_), Ok(None) | Err(_)) => {}
        }
    }
    let pushes = footprint.pushes.iter().map(|p| PushFx { object: p.object, impulse: p.impulse, at: p.at }).collect();
    let throws =
        footprint.throws.iter().map(|t| ThrowFx { object: t.object, linear: t.linear, angular: t.angular }).collect();
    Effects { changed, pushes, throws, grabs: footprint.grabs.clone() }
}

/// `id`'s body: itself if it has a `Body`, else its nearest ancestor with one.
fn owner(world: &World, id: ObjectId) -> Option<ObjectId> {
    let store = world.store();
    let mut at = id;
    for _ in 0..=pe_world::DEPTH_LIMIT_MAX {
        if !store.exists(at) {
            return None;
        }
        if store.has::<Body>(at) {
            return Some(at);
        }
        at = store.parent(at)?;
    }
    None
}

/// `top` and every descendant of it.
fn subtree(world: &World, top: ObjectId) -> Vec<ObjectId> {
    let store = world.store();
    if !store.exists(top) {
        return Vec::new();
    }
    let mut found = vec![top];
    let mut i = 0;
    while let Some(&x) = found.get(i) {
        found.extend(store.children(x));
        i += 1;
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every kind of pe-world's `Light` the view names, a spot light with its cone; an unknown kind is not drawn.
    #[test]
    fn the_view_takes_each_light_kind_and_a_spot_lights_cone() {
        let light = |kind, inner_angle, outer_angle| Light {
            kind,
            intensity: 2.0,
            range: 5.0,
            color: [1.0, 0.9, 0.8],
            inner_angle,
            outer_angle,
        };
        let kind = |l: Light| view_light(&l).map(|v| v.kind);
        assert_eq!(kind(light(light_kind::POINT, 0.0, 0.0)), Some(LightKind::Point));
        assert_eq!(kind(light(light_kind::DIRECTIONAL, 0.0, 0.0)), Some(LightKind::Directional));
        let spot = view_light(&light(light_kind::SPOT, 0.3, 0.6)).expect("a spot light is drawn");
        assert_eq!(spot.kind, LightKind::Spot { inner_angle: 0.3, outer_angle: 0.6 });
        assert_eq!((spot.color, spot.intensity, spot.range), ([1.0, 0.9, 0.8], 2.0, 5.0));
        assert_eq!(kind(light(7, 0.0, 0.0)), None);
    }
}
