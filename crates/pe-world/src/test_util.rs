//! Divergence injection for M1's "What the hash covers" test (feature `test-util`, never in a shipped build; the gate
//! checks pe-app's normal edges). [`divergence`] damages one record of one of the five hashed kinds in a world,
//! bypassing every check (plus a body's physics state, the leaf most likely to diverge for real; @coordinator-opus), and into each of the other hashed tables (registry, cooldowns, tombstones, hand packs, push
//! windows; item 2), and returns the leaf it changed, so a test can assert that the hash tree finds
//! exactly that leaf: pe-world's own detection test, and @diverge-eng's repair by rungs 1 and 2 on the real pe-world.

use std::sync::Arc;

use pe_core::{EntryRef, Frame, Hash, MemberKey, ObjectId, Pos};
use pe_seams::{LeafKey, Version};
use pe_sim::{ComponentDesc, FieldDesc, FieldKind, Transform};

use crate::components::BodyState;
use crate::input::{HandPose, Pack};
use crate::script::{ModuleRow, PendingCommit, script_component_id, tag_desc};
use crate::tree::table;
use crate::{BlobRef, BlobStore, Claim, GENESIS, World, perm};

/// The five kinds of hashed state M1's test injects into (docs/refound-plan.md M1, "What the hash covers").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// An engine component row (here the first object's `Transform`).
    Field,
    /// The governance table (a member's permission bits).
    Governance,
    /// The claims table (a hold's expiry).
    Claim,
    /// A script component row (logic state).
    LogicState,
    /// The modules table (a script's current version).
    ModuleVersion,
    /// A body's physics state (a `BodyState` row: velocities, sleep and digest).
    BodyState,
    /// A pending commit's attestations (6.2a), hashed in the modules table beside the current row.
    PendingAttestation,
    /// The registry table (6.6): the asset that registered a script component.
    Registry,
    /// The cooldowns table (8.1): a member's grab cooldown on an object.
    Cooldown,
    /// The tombstones table (7.5): a deleted object's tombstone.
    Tombstone,
    /// The hand packs table (8.1): a hand's pending pose pack.
    HandPack,
    /// The push windows table (8.1): a member's pushes in the last second.
    PushWindow,
}

impl Kind {
    pub const ALL: [Kind; 12] = [
        Kind::Field,
        Kind::Governance,
        Kind::Claim,
        Kind::LogicState,
        Kind::ModuleVersion,
        Kind::BodyState,
        Kind::PendingAttestation,
        Kind::Registry,
        Kind::Cooldown,
        Kind::Tombstone,
        Kind::HandPack,
        Kind::PushWindow,
    ];
}

/// The asset of [`five_kinds`]' script.
pub const ASSET: [u8; 32] = [0xA5; 32];

/// A small world with something of every [`Kind`], built by trusted construction: two members, an object with a
/// `Transform` and a `BodyState`, a hand holding it (a claim), and a script version (its modules row, tag and one
/// component with a row on the object, both registered by the asset) with a next version pending, attested by its
/// author; a tombstone of a deleted third object; and an input record of each kind: a cooldown, the hand's pending
/// pack and a push window. For hashing tests only:
/// the pending commit reuses the current version's component ids under another V, which no real commit can (ids
/// derive from V), so don't activate it (@chunks-reviewer's note).
pub fn five_kinds() -> World {
    let (a, b) = (MemberKey([1; 32]), MemberKey([2; 32]));
    let (lamp, hand, gone) = (ObjectId::from_u128(100), ObjectId::from_u128(101), ObjectId::from_u128(102));
    let mut w = World::new(1, BlobStore::default());
    w.set_member(a, perm::EDIT);
    w.set_member(b, perm::EDIT);
    for id in [lamp, hand, gone] {
        w.create(id, None, a, GENESIS).expect("fresh id");
        w.insert(id, Transform::at(Pos::ORIGIN)).expect("live object");
    }
    let stone = crate::apply::tombstone_of(&w.store, gone, EntryRef(Hash([11; 32])), Frame(3), Frame(3603), None, 2);
    w.store.destroy(gone).expect("live object");
    Arc::make_mut(&mut w.tombstones).insert(gone, stone);
    let inputs = Arc::make_mut(&mut w.inputs);
    inputs.cooldowns.insert((b, lamp), Frame(30));
    let pose = HandPose { pos: Pos::ORIGIN, rot: [0.0, 0.0, 0.0, 1.0], grip: 0.5, engaged: true };
    inputs.packs.insert(hand, Pack { stamp: Frame(2), poses: vec![pose], hold: 1 });
    inputs.pushes.insert(b, [(Frame(3), 0.25)].into());
    let body = BodyState {
        linear: [0.5, 0.0, 0.0],
        angular: [0.0; 3],
        sleep_time: 0.0,
        awake: 1,
        _pad: [0; 3],
        digest: [4; 32],
    };
    w.insert(lamp, body).expect("live object");
    w.set_claim(lamp, Claim { hand, member: a, since: Frame(0), until: Frame(600) }).expect("live object");
    let v = Hash([7; 32]);
    let fields = vec![FieldDesc::new("period", FieldKind::F32, 0, 1), FieldDesc::new("phase", FieldKind::F32, 4, 1)];
    let id = script_component_id(&v, "Blink", fields.clone()).expect("a valid description");
    let blink = ComponentDesc::new(id, "Blink", fields).expect("a valid description");
    let tag = tag_desc(&v);
    let mut components = vec![tag.id(), blink.id()];
    components.sort();
    for d in [tag.clone(), blink] {
        Arc::make_mut(&mut w.script_ids).insert(d.id(), ASSET);
        w.store.register(d).expect("new ids");
    }
    w.store.insert_raw(tag.id(), lamp, &[]).expect("registered");
    let row: Vec<u8> = [2.0f32, 0.5].iter().flat_map(|f| f.to_le_bytes()).collect();
    w.store.insert_raw(id, lamp, &row).expect("registered");
    let module = ModuleRow {
        version: Version { entry: EntryRef(Hash([9; 32])), op: 1 },
        source: BlobRef { hash: v.0, len: 64 },
        toolchain: Hash([1; 32]),
        sdk: Hash([2; 32]),
        author_module: Hash([3; 32]),
        components,
        activate_at: Frame(1),
        faulted: None,
    };
    let pending = PendingCommit {
        version: Version { entry: EntryRef(Hash([10; 32])), op: 1 },
        author: a,
        source: BlobRef { hash: [8; 32], len: 64 },
        toolchain: module.toolchain,
        sdk: module.sdk,
        author_module: module.author_module,
        components: module.components.clone(),
        commit_frame: Frame(5),
        deadline: Frame(1205),
        voters: vec![a, b],
        attested: vec![(a, Frame(5))],
        failed: Vec::new(),
        void: None,
    };
    Arc::make_mut(&mut w.modules).insert(ASSET, module);
    Arc::make_mut(&mut w.pending).insert(ASSET, pending);
    w
}

/// Damages one record of `kind` in `w` and returns the leaf that now differs from an undamaged twin; `None` if `w`
/// has nothing of that kind. Each damage is small and keeps the record well-formed:
/// - `Field`: the first object with a `Transform` moves 1 mm along x;
/// - `Governance`: the first member's `perm::AGENT` bit flips (a member never leaves the table this way);
/// - `Claim`: the first claim's `until` grows by one frame;
/// - `LogicState`: the first row of the first sized script component, in modules order, has its first byte's lowest
///   bit flipped (a finite `f32` stays finite);
/// - `ModuleVersion`: the first modules row's version names the next op of its entry;
/// - `BodyState`: the first body's linear velocity gains 1 mm/s along x;
/// - `PendingAttestation`: the first pending commit's last attestation is dropped;
/// - `Registry`: the first script component's registering asset has its last byte's lowest bit flipped;
/// - `Cooldown`: the first cooldown ends one frame later;
/// - `Tombstone`: the first tombstone expires one frame later;
/// - `HandPack`: the first pending hand pack's stamp is one frame later;
/// - `PushWindow`: the first member's newest push grows by 0.001 N·s.
pub fn divergence(w: &mut World, kind: Kind) -> Option<LeafKey> {
    match kind {
        Kind::Field => {
            let id = *w.store.objects().iter().find(|&&id| w.store.get::<Transform>(id).is_some())?;
            let mut t = *w.store.get::<Transform>(id)?;
            t.pos[0] += 0.001;
            w.store.insert(id, t).ok()?;
            Some(LeafKey::Object(id))
        }
        Kind::Governance => {
            let (&key, &perms) = w.members.iter().next()?;
            Arc::make_mut(&mut w.members).insert(key, perms ^ perm::AGENT);
            Some(LeafKey::Table(table::GOVERNANCE))
        }
        Kind::Claim => {
            let object = *w.claims.keys().next()?;
            Arc::make_mut(&mut w.claims).get_mut(&object)?.until.0 += 1;
            Some(LeafKey::Table(table::CLAIMS))
        }
        Kind::LogicState => {
            let (c, id) = w.modules.values().flat_map(|m| m.components.iter()).find_map(|&c| {
                let (ids, _) = w.store.column_raw(c);
                ids.first().map(|&id| (c, id)).filter(|&(c, id)| w.store.get_raw(c, id).is_some_and(|r| !r.is_empty()))
            })?;
            let mut row = w.store.get_raw(c, id)?.to_vec();
            row[0] ^= 1;
            w.store.insert_raw(c, id, &row).ok()?;
            Some(LeafKey::Object(id))
        }
        Kind::ModuleVersion => {
            let asset = *w.modules.keys().next()?;
            let m = Arc::make_mut(&mut w.modules).get_mut(&asset)?;
            m.version.op = m.version.op.wrapping_add(1);
            Some(LeafKey::Table(table::MODULES))
        }
        Kind::BodyState => {
            let id = *w.store.objects().iter().find(|&&id| w.store.get::<BodyState>(id).is_some())?;
            let mut b = *w.store.get::<BodyState>(id)?;
            b.linear[0] += 0.001;
            w.store.insert(id, b).ok()?;
            Some(LeafKey::Object(id))
        }
        Kind::PendingAttestation => {
            let asset = *w.pending.iter().find(|(_, p)| !p.attested.is_empty())?.0;
            Arc::make_mut(&mut w.pending).get_mut(&asset)?.attested.pop();
            Some(LeafKey::Table(table::MODULES))
        }
        Kind::Registry => {
            let c = *w.script_ids.keys().next()?;
            Arc::make_mut(&mut w.script_ids).get_mut(&c)?[31] ^= 1;
            Some(LeafKey::Table(table::REGISTRY))
        }
        Kind::Cooldown => {
            let k = *w.inputs.cooldowns.keys().next()?;
            Arc::make_mut(&mut w.inputs).cooldowns.get_mut(&k)?.0 += 1;
            Some(LeafKey::Table(table::COOLDOWNS))
        }
        Kind::Tombstone => {
            let id = *w.tombstones.keys().next()?;
            Arc::make_mut(&mut w.tombstones).get_mut(&id)?.expires.0 += 1;
            Some(LeafKey::Table(table::TOMBSTONES))
        }
        Kind::HandPack => {
            let hand = *w.inputs.packs.keys().next()?;
            Arc::make_mut(&mut w.inputs).packs.get_mut(&hand)?.stamp.0 += 1;
            Some(LeafKey::Table(table::HAND_PACKS))
        }
        Kind::PushWindow => {
            let m = *w.inputs.pushes.iter().find(|(_, q)| !q.is_empty())?.0;
            Arc::make_mut(&mut w.inputs).pushes.get_mut(&m)?.back_mut()?.1 += 0.001;
            Some(LeafKey::Table(table::PUSH_WINDOWS))
        }
    }
}
