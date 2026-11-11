//! `KvReplica`: a small, deterministic, hashed state machine that stands in for seam S2 (DESIGN.md 3.3).
//!
//! It lets agreement scenarios, the probes and randsched run thousands of seeds a minute with no physics, and it
//! mocks S2 until pe-replica exists. Its control is that the same scenarios also run against pe-replica's ball pit,
//! so no property depends on this replica's simplicity.
//!
//! - **State:** objects with integer fields, plus a governance table (roles and the role of each member). Governance
//!   is inside the hash, as the new design requires (file 07, H3).
//! - **The hash tree:** one leaf per object, one for governance, and a root over the frame and all leaves.
//! - **Permissions:** role 0 is admin and holds every permission. Other roles hold what `DefineRole` gives them.
//! - **Rollback:** `save` keeps the state at the current frame; `restore` returns to a saved frame.
//! - **Faults:** `corrupt` changes one field outside any entry, for the repair tests and the checkers' controls.
//!
//! The ids, entries and outcomes here are local stand-ins until pe-seams v0.

use std::collections::BTreeMap;

pub type ObjectId = u128;
pub type Field = u16;
pub type MemberId = u32;
pub type Frame = u64;
pub type Role = u8;
pub type Hash = [u8; 32];

/// Permission bits. Role 0 (admin) holds all of them.
pub mod perm {
    pub const EDIT: u8 = 1;
    pub const ROLES: u8 = 2;
    pub const KICK: u8 = 4;
    pub const ALL: u8 = 0xFF;
}

pub const ADMIN: Role = 0;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Op {
    Create { obj: ObjectId },
    Delete { obj: ObjectId },
    Set { obj: ObjectId, field: Field, value: i64 },
    Add { obj: ObjectId, field: Field, delta: i64 },
    DefineRole { role: Role, perms: u8 },
    SetRole { member: MemberId, role: Role },
}

impl Op {
    fn needs(&self) -> u8 {
        match self {
            Op::Create { .. } | Op::Delete { .. } | Op::Set { .. } | Op::Add { .. } => perm::EDIT,
            Op::DefineRole { .. } | Op::SetRole { .. } => perm::ROLES,
        }
    }
}

/// An entry as the replica sees it: who, and what. Order and stamps are the agreement's business.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KvEntry {
    pub author: MemberId,
    pub op: Op,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reason {
    NoPermission,
    NotMember,
    NoSuchObject,
    AlreadyExists,
    UnknownRole,
    /// Admin cannot be redefined.
    AdminFixed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Applied,
    Refused(Reason),
}

/// The governance template a world starts from (probe-capture-close).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Template {
    /// Every member starts as admin: the old default.
    AllAdmin,
    /// The listed owners are admins; everyone else gets `joiner`, a role with the given permissions.
    Public { owners: Vec<MemberId>, joiner: Role, joiner_perms: u8 },
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct State {
    objects: BTreeMap<ObjectId, BTreeMap<Field, i64>>,
    roles: BTreeMap<Role, u8>,
    member_role: BTreeMap<MemberId, Role>,
}

#[derive(Clone, Debug)]
pub struct KvReplica {
    frame: Frame,
    state: State,
    saves: BTreeMap<Frame, State>,
}

fn hasher(domain: &str) -> blake3::Hasher {
    let mut h = blake3::Hasher::new();
    h.update(domain.as_bytes());
    h.update(&[0]);
    h
}

impl KvReplica {
    /// A world at frame 0 with the given members, governance and objects. Deterministic: the same inputs give the
    /// same root on any machine.
    pub fn new(members: &[MemberId], template: &Template, objects: &[ObjectId]) -> Self {
        let mut st = State::default();
        st.roles.insert(ADMIN, perm::ALL);
        match template {
            Template::AllAdmin => {
                for &m in members {
                    st.member_role.insert(m, ADMIN);
                }
            }
            Template::Public { owners, joiner, joiner_perms } => {
                st.roles.insert(*joiner, *joiner_perms);
                for &m in members {
                    st.member_role.insert(m, if owners.contains(&m) { ADMIN } else { *joiner });
                }
            }
        }
        for &o in objects {
            st.objects.insert(o, BTreeMap::new());
        }
        Self { frame: 0, state: st, saves: BTreeMap::new() }
    }

    pub fn frame(&self) -> Frame {
        self.frame
    }

    pub fn permits(&self, who: MemberId, needs: u8) -> bool {
        self.state.member_role.get(&who).and_then(|r| self.state.roles.get(r)).is_some_and(|p| p & needs == needs)
    }

    pub fn role_of(&self, who: MemberId) -> Option<Role> {
        self.state.member_role.get(&who).copied()
    }

    pub fn field(&self, obj: ObjectId, field: Field) -> Option<i64> {
        self.state.objects.get(&obj).and_then(|f| f.get(&field)).copied()
    }

    pub fn exists(&self, obj: ObjectId) -> bool {
        self.state.objects.contains_key(&obj)
    }

    /// Steps to `self.frame() + 1`, applying `entries` in the given (agreed) order. Every entry gets an outcome;
    /// nothing panics on any input.
    pub fn step(&mut self, entries: &[KvEntry]) -> Vec<Outcome> {
        self.frame += 1;
        entries.iter().map(|e| self.apply(e)).collect()
    }

    fn apply(&mut self, e: &KvEntry) -> Outcome {
        use Outcome::*;
        if !self.state.member_role.contains_key(&e.author) {
            return Refused(Reason::NotMember);
        }
        if !self.permits(e.author, e.op.needs()) {
            return Refused(Reason::NoPermission);
        }
        let st = &mut self.state;
        match e.op {
            Op::Create { obj } => {
                if st.objects.contains_key(&obj) {
                    return Refused(Reason::AlreadyExists);
                }
                st.objects.insert(obj, BTreeMap::new());
            }
            Op::Delete { obj } => {
                if st.objects.remove(&obj).is_none() {
                    return Refused(Reason::NoSuchObject);
                }
            }
            Op::Set { obj, field, value } => match st.objects.get_mut(&obj) {
                Some(f) => {
                    f.insert(field, value);
                }
                None => return Refused(Reason::NoSuchObject),
            },
            Op::Add { obj, field, delta } => match st.objects.get_mut(&obj) {
                Some(f) => {
                    let v = f.entry(field).or_insert(0);
                    *v = v.wrapping_add(delta);
                }
                None => return Refused(Reason::NoSuchObject),
            },
            Op::DefineRole { role, perms } => {
                if role == ADMIN {
                    return Refused(Reason::AdminFixed);
                }
                st.roles.insert(role, perms);
            }
            Op::SetRole { member, role } => {
                if !st.roles.contains_key(&role) {
                    return Refused(Reason::UnknownRole);
                }
                if !st.member_role.contains_key(&member) {
                    return Refused(Reason::NotMember);
                }
                st.member_role.insert(member, role);
            }
        }
        Applied
    }

    /// Keeps the state at the current frame, for a later `restore`.
    pub fn save(&mut self) {
        self.saves.insert(self.frame, self.state.clone());
    }

    /// Returns to a saved frame. Saves after it are dropped. Returns false if the frame was never saved.
    pub fn restore(&mut self, frame: Frame) -> bool {
        let Some(st) = self.saves.get(&frame).cloned() else {
            return false;
        };
        self.state = st;
        self.frame = frame;
        self.saves.retain(|&f, _| f <= frame);
        true
    }

    /// Drops saves below `frame`, as finality allows.
    pub fn forget_before(&mut self, frame: Frame) {
        self.saves.retain(|&f, _| f >= frame);
    }

    /// Test-only fault: sets a field outside any entry, as a corrupted machine would. Creates the object if needed.
    pub fn corrupt(&mut self, obj: ObjectId, field: Field, value: i64) {
        self.state.objects.entry(obj).or_default().insert(field, value);
    }

    pub fn object_hash(&self, obj: ObjectId) -> Option<Hash> {
        self.state.objects.get(&obj).map(|f| Self::leaf(obj, f))
    }

    fn leaf(obj: ObjectId, fields: &BTreeMap<Field, i64>) -> Hash {
        let mut h = hasher("pe.kv.obj");
        h.update(&obj.to_le_bytes());
        for (k, v) in fields {
            h.update(&k.to_le_bytes());
            h.update(&v.to_le_bytes());
        }
        *h.finalize().as_bytes()
    }

    pub fn governance_hash(&self) -> Hash {
        let mut h = hasher("pe.kv.gov");
        h.update(&(self.state.roles.len() as u64).to_le_bytes());
        for (r, p) in &self.state.roles {
            h.update(&[*r, *p]);
        }
        for (m, r) in &self.state.member_role {
            h.update(&m.to_le_bytes());
            h.update(&[*r]);
        }
        *h.finalize().as_bytes()
    }

    /// The root over the frame, every object leaf (in id order) and the governance leaf.
    pub fn root_hash(&self) -> Hash {
        let mut h = hasher("pe.kv.root");
        h.update(&self.frame.to_le_bytes());
        h.update(&self.governance_hash());
        h.update(&(self.state.objects.len() as u64).to_le_bytes());
        for (o, f) in &self.state.objects {
            h.update(&Self::leaf(*o, f));
        }
        *h.finalize().as_bytes()
    }

    /// The objects whose leaves differ from `other`'s, including objects only one side has. A stand-in for the
    /// subtree-hash exchange S2 will have; the repair tests use it until then.
    pub fn differing(&self, other: &KvReplica) -> Vec<ObjectId> {
        let mut ids: Vec<ObjectId> = self.state.objects.keys().chain(other.state.objects.keys()).copied().collect();
        ids.sort_unstable();
        ids.dedup();
        ids.retain(|&o| self.object_hash(o) != other.object_hash(o));
        ids
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(author: MemberId, op: Op) -> KvEntry {
        KvEntry { author, op }
    }

    #[test]
    fn same_inputs_same_root_and_order_matters() {
        let mk = || KvReplica::new(&[1, 2], &Template::AllAdmin, &[10, 11]);
        let (mut a, mut b) = (mk(), mk());
        assert_eq!(a.root_hash(), b.root_hash());
        let xs = [e(1, Op::Set { obj: 10, field: 0, value: 5 }), e(2, Op::Add { obj: 10, field: 0, delta: 3 })];
        a.step(&xs);
        b.step(&xs);
        assert_eq!(a.root_hash(), b.root_hash());
        assert_eq!(a.field(10, 0), Some(8));
        let mut c = mk();
        c.step(&[xs[1].clone(), xs[0].clone()]);
        assert_eq!(c.field(10, 0), Some(5));
        assert_ne!(c.root_hash(), a.root_hash(), "a different order is a different state");
    }

    #[test]
    fn the_root_covers_frame_fields_and_governance() {
        let base = KvReplica::new(&[1, 2], &Template::AllAdmin, &[10]);
        let mut stepped = base.clone();
        stepped.step(&[]);
        assert_ne!(base.root_hash(), stepped.root_hash(), "frame");
        let mut field = base.clone();
        field.corrupt(10, 1, 0);
        assert_ne!(base.root_hash(), field.root_hash(), "a field, even set to zero");
        let mut gov = base.clone();
        gov.step(&[e(1, Op::DefineRole { role: 1, perms: 0 })]);
        let mut gov2 = base.clone();
        gov2.step(&[]);
        assert_ne!(gov.root_hash(), gov2.root_hash(), "governance is in the hash");
        assert_eq!(gov.differing(&gov2), Vec::<ObjectId>::new(), "and not in any object");
    }

    #[test]
    fn public_template_refuses_a_joiner_s_role_capture() {
        // probe-capture-close's public variant, at the replica level: member 4 joined without admin rights.
        let t = Template::Public { owners: vec![1], joiner: 1, joiner_perms: perm::EDIT };
        let mut r = KvReplica::new(&[1, 2, 3, 4], &t, &[10]);
        let before = r.governance_hash();
        let out = r.step(&[
            e(4, Op::DefineRole { role: 2, perms: 0 }),
            e(4, Op::SetRole { member: 1, role: 2 }),
            e(4, Op::SetRole { member: 2, role: 2 }),
            e(4, Op::SetRole { member: 3, role: 2 }),
            e(4, Op::Set { obj: 10, field: 0, value: 1 }),
        ]);
        assert_eq!(&out[..4], &[Outcome::Refused(Reason::NoPermission); 4]);
        assert_eq!(out[4], Outcome::Applied, "a joiner may still edit");
        assert_eq!(r.governance_hash(), before);
        assert_eq!(r.role_of(1), Some(ADMIN));
    }

    #[test]
    fn all_admin_template_lets_one_member_capture_the_roles() {
        // The old default (probe-g3): the capture succeeds. The ruled answer for this case is WS-gov's guarded mode.
        let mut r = KvReplica::new(&[1, 2, 3, 4], &Template::AllAdmin, &[]);
        let out = r.step(&[e(4, Op::DefineRole { role: 1, perms: 0 }), e(4, Op::SetRole { member: 1, role: 1 })]);
        assert_eq!(out, vec![Outcome::Applied; 2]);
        assert!(!r.permits(1, perm::ROLES));
    }

    #[test]
    fn refusals_never_panic() {
        let mut r = KvReplica::new(&[1], &Template::AllAdmin, &[10]);
        let out = r.step(&[
            e(9, Op::Create { obj: 1 }),
            e(1, Op::Create { obj: 10 }),
            e(1, Op::Delete { obj: 99 }),
            e(1, Op::Set { obj: 99, field: 0, value: 0 }),
            e(1, Op::Add { obj: 10, field: 0, delta: i64::MAX }),
            e(1, Op::Add { obj: 10, field: 0, delta: i64::MAX }),
            e(1, Op::DefineRole { role: ADMIN, perms: 0 }),
            e(1, Op::SetRole { member: 1, role: 77 }),
            e(1, Op::SetRole { member: 55, role: ADMIN }),
        ]);
        use Outcome::*;
        use Reason::*;
        assert_eq!(
            out,
            vec![
                Refused(NotMember),
                Refused(AlreadyExists),
                Refused(NoSuchObject),
                Refused(NoSuchObject),
                Applied,
                Applied,
                Refused(AdminFixed),
                Refused(UnknownRole),
                Refused(NotMember),
            ]
        );
        assert_eq!(r.field(10, 0), Some(-2), "wrapping, not overflow");
    }

    #[test]
    fn save_and_restore() {
        let mut r = KvReplica::new(&[1], &Template::AllAdmin, &[10]);
        r.save();
        let root0 = r.root_hash();
        r.step(&[e(1, Op::Set { obj: 10, field: 0, value: 1 })]);
        r.save();
        let root1 = r.root_hash();
        r.step(&[e(1, Op::Delete { obj: 10 })]);
        assert!(r.restore(1));
        assert_eq!((r.frame(), r.root_hash()), (1, root1));
        assert!(r.restore(0));
        assert_eq!((r.frame(), r.root_hash()), (0, root0));
        assert!(!r.restore(1), "saves after a restored frame are dropped");
        r.forget_before(1);
        assert!(!r.restore(0));
    }

    #[test]
    fn corruption_is_found_object_by_object() {
        let mk = || KvReplica::new(&[1], &Template::AllAdmin, &[10, 11, 12]);
        let (good, mut bad) = (mk(), mk());
        bad.corrupt(11, 3, 42);
        assert_ne!(good.root_hash(), bad.root_hash());
        assert_eq!(good.differing(&bad), vec![11]);
        bad.corrupt(13, 0, 1);
        assert_eq!(good.differing(&bad), vec![11, 13]);
    }
}
