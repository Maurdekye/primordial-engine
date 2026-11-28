//! The toy world the merge is checked on (docs/spec/divergence.md 9.5, "A sketch of a model that suffices"), and its
//! ordinary rules: one `apply` per entry, one `step` per frame. Everything here is the engine's ordinary semantics
//! in miniature; nothing in it knows about merging.
//!
//! - **Objects:** a parent `P` and a free object `X`, both with bodies in one physics domain, and two creation slots:
//!   `C` for the object a main member's entry creates, `D` for the returner's (created ids are per author and
//!   segment, so a returning create and a final create never collide). A created object is a child, or an orphan
//!   once its parent is gone. Each object has one field (0 or 1), a hold slot, a component slot and a version
//!   counter.
//! - **Rows:** the settings row `g` (gravity, 0 or 1), which every physics step reads, and the admin list (9.6).
//! - **The step** reads every body and `g`, and writes the bodies that move. It is truthful, with no domain rule built
//!   in: under gravity a raised body falls one unit a frame; a body written this frame moves; and when the frame's
//!   contact choice is on, a body that moved this frame moves the other one to its place. Contact is an input to the
//!   whole run, as the sketch asks: a nondeterministic choice the checker enumerates.

use std::fmt;

/// A member. 0 and 1 are on main; 2 is the returner; 3, when present, is a Byzantine member (n = 4).
pub type Member = u8;

/// The members that may hold seats in a run.
pub const MAIN: [Member; 2] = [0, 1];
pub const RETURNER: Member = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Obj {
    P,
    C,
    X,
    D,
}

pub const OBJS: [Obj; 4] = [Obj::P, Obj::C, Obj::X, Obj::D];

/// The creation slot an author's `Create` fills, and its `Reparent` moves.
pub fn slot(author: Member) -> Obj {
    if author == RETURNER { Obj::D } else { Obj::C }
}

/// Whether the object is a creation slot.
pub fn created(o: Obj) -> bool {
    matches!(o, Obj::C | Obj::D)
}

impl Obj {
    fn i(self) -> usize {
        self as usize
    }

    /// The body index of a body object (`P` and `X` have bodies; `C` has none).
    fn body(self) -> Option<usize> {
        match self {
            Obj::P => Some(0),
            Obj::X => Some(1),
            Obj::C | Obj::D => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ObjState {
    pub exists: bool,
    pub field: u8,
    pub hold: Option<Member>,
    pub comp: bool,
    pub version: u8,
    pub parent: Option<Obj>,
}

impl ObjState {
    const GONE: ObjState = ObjState { exists: false, field: 0, hold: None, comp: false, version: 0, parent: None };
    const NEW: ObjState = ObjState { exists: true, field: 0, hold: None, comp: false, version: 0, parent: None };
}

/// The world's state at a frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct World {
    pub objs: [ObjState; 4],
    /// The settings row.
    pub g: u8,
    /// The admin list, as a member bit set.
    pub admins: u8,
    /// Each body's height (0 = resting), `P`'s then `X`'s.
    pub pos: [u8; 2],
}

/// The heights a body can take.
pub const MAX_POS: u8 = 2;

impl World {
    /// The base state `S_b`: `P` and `X` exist and rest, `C` and `D` do not exist yet, gravity is off, and the three
    /// seated members are admins.
    pub fn base() -> World {
        World { objs: [ObjState::NEW, ObjState::GONE, ObjState::NEW, ObjState::GONE], g: 0, admins: 0b111, pos: [0, 0] }
    }

    pub fn obj(&self, o: Obj) -> &ObjState {
        &self.objs[o.i()]
    }

    fn obj_mut(&mut self, o: Obj) -> &mut ObjState {
        &mut self.objs[o.i()]
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Op {
    Grab(Obj),
    Release(Obj),
    Write(Obj, u8),
    /// A write with a precondition: the object's version is the one given.
    WriteIf(Obj, u8, u8),
    /// Deletes the object and its children.
    Delete(Obj),
    /// Creates the author's slot (`C`, or `D` for the returner) as a child of the object (an object add-on).
    Create(Obj),
    /// Moves the author's slot under the object (an object add-on too).
    Reparent(Obj),
    /// Attaches a component to the object (a component add-on).
    Attach(Obj),
    WriteG(u8),
    AddAdmin(Member),
    RemoveAdmin(Member),
}

/// Why the ordinary rules refused an entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Reason {
    /// The object does not exist (deleted first, or never created).
    Gone,
    /// Another member holds it.
    Held,
    /// A release by a member that does not hold it.
    NotHolder,
    /// A precondition's version did not match.
    Precondition,
    /// The author's creation slot exists already.
    Exists,
    /// A seat or admin change by a member that is not an admin.
    NotAdmin,
    /// A grab of a body in motion (raised above its rest): out of the hand's reach, as pe-world's reach rule refuses
    /// a grab of a ball that has moved away. It makes a final grab's outcome depend on the physics, so a returning
    /// push that moves a body by contact can change it (9.5's example: a push that hits a resting ball a final grab
    /// holds).
    Moving,
}

/// An applied entry's state-dependent outcome under the ordinary rules (9.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Outcome {
    Applied,
    /// A grab the rules granted.
    Granted,
    /// An object add-on (created under, or reparented to, an object) whose parent was deleted first: kept, with no
    /// parent (9.2, the delete rule).
    Orphaned,
    /// A delete of an object already gone.
    AlreadyDeleted,
    Refused(Reason),
}

/// The ordinary rules: `op` by `author` on `w`, in place, with its outcome.
pub fn apply(w: &mut World, author: Member, op: Op, moved: &mut [bool; 2]) -> Outcome {
    let held_by_other = |s: &ObjState| s.hold.is_some_and(|h| h != author);
    match op {
        Op::Grab(o) => {
            if o.body().is_some_and(|b| w.pos[b] > 0) && w.obj(o).exists {
                return Outcome::Refused(Reason::Moving);
            }
            let s = w.obj_mut(o);
            if !s.exists {
                return Outcome::Refused(Reason::Gone);
            }
            if held_by_other(s) {
                return Outcome::Refused(Reason::Held);
            }
            s.hold = Some(author);
            Outcome::Granted
        }
        Op::Release(o) => {
            let s = w.obj_mut(o);
            if !s.exists {
                return Outcome::Refused(Reason::Gone);
            }
            if s.hold != Some(author) {
                return Outcome::Refused(Reason::NotHolder);
            }
            s.hold = None;
            Outcome::Applied
        }
        Op::Write(o, v) | Op::WriteIf(o, _, v) => {
            let s = *w.obj(o);
            if !s.exists {
                return Outcome::Refused(Reason::Gone);
            }
            if held_by_other(&s) {
                return Outcome::Refused(Reason::Held);
            }
            if let Op::WriteIf(_, version, _) = op
                && s.version != version
            {
                return Outcome::Refused(Reason::Precondition);
            }
            let s = w.obj_mut(o);
            s.field = v;
            s.version = (s.version + 1) % 3;
            if let Some(b) = o.body() {
                // A write to a body places it: field 1 raises it to the top.
                w.pos[b] = if v == 1 { MAX_POS } else { 0 };
                moved[b] = true;
            }
            Outcome::Applied
        }
        Op::Delete(o) => {
            let s = *w.obj(o);
            if !s.exists {
                return Outcome::AlreadyDeleted;
            }
            if held_by_other(&s) {
                return Outcome::Refused(Reason::Held);
            }
            *w.obj_mut(o) = ObjState::GONE;
            if let Some(b) = o.body() {
                w.pos[b] = 0;
            }
            // The subtree: each created object that is this object's child.
            for c in [Obj::C, Obj::D] {
                if c != o && w.obj(c).exists && w.obj(c).parent == Some(o) {
                    *w.obj_mut(c) = ObjState::GONE;
                }
            }
            Outcome::Applied
        }
        Op::Create(under) => {
            let c = slot(author);
            if w.obj(c).exists {
                return Outcome::Refused(Reason::Exists);
            }
            if created(under) {
                return Outcome::Refused(Reason::Gone);
            }
            // The user's rule: if the delete came first, the add-on is orphaned.
            let parent_there = w.obj(under).exists;
            *w.obj_mut(c) = ObjState { parent: parent_there.then_some(under), ..ObjState::NEW };
            if parent_there { Outcome::Applied } else { Outcome::Orphaned }
        }
        Op::Reparent(under) => {
            let c = slot(author);
            let s = *w.obj(c);
            if !s.exists || created(under) {
                return Outcome::Refused(Reason::Gone);
            }
            if held_by_other(&s) {
                return Outcome::Refused(Reason::Held);
            }
            // Reparented to an object deleted first: orphaned (9.2's delete rule, world-model.md 7.5).
            if !w.obj(under).exists {
                w.obj_mut(c).parent = None;
                return Outcome::Orphaned;
            }
            w.obj_mut(c).parent = Some(under);
            Outcome::Applied
        }
        Op::Attach(o) => {
            // A component add-on on an object deleted first is refused, and its author is told (9.2, file 14 9.2).
            let s = w.obj_mut(o);
            if !s.exists {
                return Outcome::Refused(Reason::Gone);
            }
            s.comp = true;
            Outcome::Applied
        }
        Op::WriteG(v) => {
            w.g = v;
            Outcome::Applied
        }
        Op::AddAdmin(m) | Op::RemoveAdmin(m) => {
            if w.admins & (1 << author) == 0 {
                return Outcome::Refused(Reason::NotAdmin);
            }
            if let Op::AddAdmin(_) = op {
                w.admins |= 1 << m;
            } else {
                w.admins &= !(1 << m);
            }
            Outcome::Applied
        }
    }
}

/// One physics step at the end of a frame, after its entries: reads every body and `g`, writes the bodies that move.
/// `contact` is the frame's contact choice; `moved` says which bodies an entry moved this frame.
pub fn step(w: &mut World, contact: bool, moved: [bool; 2]) {
    let exists = [w.obj(Obj::P).exists, w.obj(Obj::X).exists];
    let mut now = moved;
    for b in 0..2 {
        if exists[b] && w.g == 1 && w.pos[b] > 0 && !moved[b] {
            w.pos[b] -= 1;
            now[b] = true;
        }
    }
    if contact && exists[0] && exists[1] && now[0] != now[1] {
        let (from, to) = if now[0] { (0, 1) } else { (1, 0) };
        w.pos[to] = w.pos[from];
    }
}

impl fmt::Display for Obj {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}",
            match self {
                Obj::P => "p",
                Obj::C => "c",
                Obj::X => "x",
                Obj::D => "d",
            }
        )
    }
}
