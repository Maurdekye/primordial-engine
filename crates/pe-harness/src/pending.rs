//! The register of tests written before the code they test (DESIGN.md 9.4).
//!
//! Each entry names the milestone it waits for and **the one property it is expected to fail with**. The gate runs
//! every entry and requires:
//! - at least one violation of the expected property;
//! - no violation of any other property, and no failed expectation, because a failure for any other reason (a
//!   stub that panics, a broken schedule) must not pass as "failing as expected".
//!
//! When an entry starts passing, the gate says so, and it leaves the register in the same landing.

use crate::props::{Prop, Violation};

pub struct Pending {
    pub name: &'static str,
    pub milestone: &'static str,
    pub expect: Prop,
    pub run: fn() -> Vec<Violation>,
}

/// How a pending entry fared.
#[derive(Debug, PartialEq, Eq)]
pub enum Verdict {
    /// It failed with its expected property only: still pending, as it should be.
    FailsAsExpected,
    /// It passed: move it out of the register.
    NowPasses,
    /// It failed for another reason: a gate failure.
    WrongFailure(Vec<Violation>),
}

pub fn judge(p: &Pending) -> Verdict {
    let vs = (p.run)();
    if vs.is_empty() {
        return Verdict::NowPasses;
    }
    let others: Vec<Violation> = vs.iter().filter(|v| v.prop != p.expect).cloned().collect();
    if others.is_empty() { Verdict::FailsAsExpected } else { Verdict::WrongFailure(others) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn none() -> Vec<Violation> {
        Vec::new()
    }
    fn p8() -> Vec<Violation> {
        vec![Violation { prop: Prop::P8, at: 1, node: Some(1), detail: "paused".into() }]
    }
    fn p8_and_panic() -> Vec<Violation> {
        let mut v = p8();
        v.push(Violation { prop: Prop::P6, at: 2, node: Some(1), detail: "stub panicked".into() });
        v
    }

    #[test]
    fn the_register_tells_the_three_cases_apart() {
        let mk = |run| Pending { name: "t", milestone: "M3", expect: Prop::P8, run };
        assert_eq!(judge(&mk(p8)), Verdict::FailsAsExpected);
        assert_eq!(judge(&mk(none)), Verdict::NowPasses);
        assert!(matches!(judge(&mk(p8_and_panic)), Verdict::WrongFailure(v) if v.len() == 1 && v[0].prop == Prop::P6));
    }
}
