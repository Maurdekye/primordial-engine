//! Layer 1b: the witness statements (docs/spec/divergence.md 9.3, "Witnessed"; R-M1), checked exhaustively at
//! `n = 3`, and at `n = 4` with one Byzantine member who signs any statement (9.5's sketch).
//!
//! Layer 1 takes the witnessed set as a free choice of its inputs, so the merge is checked for every set. What is
//! left is the rule that decides the set: a statement `(m, heads, f)` witnesses a returning entry `e` (author `a`,
//! stamp `s`) when it is signed by `m != a`, its heads cover `e`, and `f - s <= B`, with `f` the frame at which `m`
//! first held `e`. This checks that rule against the ground truth it stands for: some honest member other than the
//! author held `e` within `B` of its stamp, so main should have had it in time.
//!
//! **Modelled:** members 0 and 1 on main and the returner 2, all honest, plus the Byzantine member 3 in the `n = 4`
//! run. Each honest member other than the author first holds `e` at any frame from `s` to `s + B + 2`, or never; the
//! author holds it at `s`. A statement is signed by its signer (B relays it and can't forge it). An honest member's
//! statement carries its true heads and receipt frame, or, if it never held `e`, heads that don't cover it and any
//! frame. The Byzantine member's statement carries any heads and any frame.
//!
//! **Properties:** *sound*: a valid statement implies the ground truth, at `n = 3`; *the residual is the only gap*:
//! at `n = 4`, a valid statement implies the ground truth or a Byzantine signer, and that residual is reached;
//! *complete*: an honest non-author's statement for an entry it held within `B` is valid.

use std::collections::BTreeSet;

/// The back-dating bound `B`, in frames (9.5's sketch: 1).
pub const BOUND: i16 = 1;
/// Stamps enumerated: `0..STAMPS`.
pub const STAMPS: i16 = 4;
/// The returner.
pub const RETURNER: u8 = 2;
/// The Byzantine member, present at `n = 4`.
pub const BYZANTINE: u8 = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Mutation {
    None,
    /// Accept a statement signed by the entry's author.
    WitnessByAuthor,
    /// Drop `f - s <= B`.
    NoBackdateBound,
    /// Accept a statement whose heads don't cover the entry.
    NoHeadsCheck,
}

/// A carried statement.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Statement {
    pub signer: u8,
    pub covers: bool,
    pub f: i16,
}

/// One case: the entry's author and stamp, each honest member's first receipt frame, and the statement carried.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Case {
    pub author: u8,
    pub stamp: i16,
    pub held: Vec<Option<i16>>,
    pub statement: Statement,
}

/// 9.3's rule.
pub fn valid(c: &Case, m: Mutation) -> bool {
    let st = c.statement;
    (m == Mutation::WitnessByAuthor || st.signer != c.author)
        && (m == Mutation::NoHeadsCheck || st.covers)
        && (m == Mutation::NoBackdateBound || st.f - c.stamp <= BOUND)
}

/// The ground truth: an honest member other than the author held the entry within `B` of its stamp.
pub fn on_time(c: &Case) -> bool {
    c.held.iter().enumerate().any(|(i, r)| i as u8 != c.author && r.is_some_and(|r| r - c.stamp <= BOUND))
}

/// Every case at `n = 3`, or `n = 4` with the Byzantine member.
pub fn for_each(byzantine: bool, mut f: impl FnMut(&Case)) {
    let frames: Vec<i16> = (0..STAMPS + BOUND + 3).collect();
    for author in 0..=RETURNER {
        for stamp in 0..STAMPS {
            let options: Vec<Option<i16>> =
                std::iter::once(None).chain((stamp..=stamp + BOUND + 2).map(Some)).collect();
            // Receipt frames of the two honest non-authors.
            let others: Vec<u8> = (0..=RETURNER).filter(|&i| i != author).collect();
            for &r0 in &options {
                for &r1 in &options {
                    let mut held = vec![None; usize::from(RETURNER) + 1];
                    held[usize::from(author)] = Some(stamp);
                    held[usize::from(others[0])] = r0;
                    held[usize::from(others[1])] = r1;
                    let mut statements = Vec::new();
                    for signer in 0..=RETURNER {
                        match held[usize::from(signer)] {
                            Some(r) => statements.push(Statement { signer, covers: true, f: r }),
                            None => statements.extend(frames.iter().map(|&f| Statement { signer, covers: false, f })),
                        }
                    }
                    if byzantine {
                        for covers in [false, true] {
                            statements.extend(frames.iter().map(|&f| Statement { signer: BYZANTINE, covers, f }));
                        }
                    }
                    for statement in statements {
                        f(&Case { author, stamp, held: held.clone(), statement });
                    }
                }
            }
        }
    }
}

pub const SOUND: &str = "sound: a valid statement means an honest non-author held the entry within B";
pub const RESIDUAL_ONLY: &str = "the residual is the only gap: a valid statement is on time or Byzantine-signed";
pub const COMPLETE: &str = "complete: an honest non-author's statement for an entry it held within B is valid";
pub const RESIDUAL_REACHED: &str = "the collusion residual is reached: a Byzantine statement admits a late entry";

/// The properties a case breaks, and whether it reaches the residual.
pub fn check(c: &Case, byzantine: bool, m: Mutation) -> (Vec<&'static str>, bool) {
    let v = valid(c, m);
    let truth = on_time(c);
    let st = c.statement;
    let mut broke = Vec::new();
    if !byzantine && v && !truth {
        broke.push(SOUND);
    }
    if byzantine && v && !truth && st.signer != BYZANTINE {
        broke.push(RESIDUAL_ONLY);
    }
    let honest = st.signer != BYZANTINE;
    let held_in_time = honest && c.held[usize::from(st.signer)].is_some_and(|r| r - c.stamp <= BOUND) && st.covers;
    if honest && st.signer != c.author && held_in_time && !v {
        broke.push(COMPLETE);
    }
    (broke, byzantine && v && !truth && st.signer == BYZANTINE)
}

/// Runs one configuration: the properties broken, the number of cases, and whether the residual was reached.
pub fn run(byzantine: bool, m: Mutation) -> (BTreeSet<&'static str>, u64, bool) {
    let mut broke = BTreeSet::new();
    let mut cases = 0;
    let mut residual = false;
    for_each(byzantine, |c| {
        cases += 1;
        let (b, r) = check(c, byzantine, m);
        broke.extend(b);
        residual |= r;
    });
    (broke, cases, residual)
}

/// The property each control must break (at `n = 3`).
pub fn expected(m: Mutation) -> Option<&'static str> {
    match m {
        Mutation::None => None,
        Mutation::WitnessByAuthor | Mutation::NoBackdateBound | Mutation::NoHeadsCheck => Some(SOUND),
    }
}
