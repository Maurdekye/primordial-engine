//! Fault injection: message rules compiled into the simulator's send filter, and crashes.

use pe_session::session::{Msg, Payload};
use pe_session_harness::{Fate, Filter, Sim};

use crate::scenario::{Crash, FaultRule};

pub const MSG_CLASSES: &[&str] =
    &["entry", "fetch", "batch", "join", "touch", "hand", "ack", "vote", "propose", "prevote", "precommit", "kick", "setrole", "definerole", "attest", "depart", "arrive", "verdict", "portal", "provenance", "ownership"];

/// Does `m` belong to message class `c` (see [`FaultRule::msg`])?
pub fn is_class(m: &Msg, c: &str) -> bool {
    match (m, c) {
        (Msg::Entry(_), "entry") | (Msg::Fetch { .. }, "fetch") | (Msg::Batch(_), "batch") | (Msg::Join { .. }, "join") => true,
        (Msg::Entry(e), _) => matches!(
            (&e.payload, c),
            (Payload::Touch { .. }, "touch")
                | (Payload::Hand { .. }, "hand")
                | (Payload::Ack { .. }, "ack")
                | (Payload::Vote { .. }, "vote")
                | (Payload::Propose { .. }, "propose")
                | (Payload::Prevote { .. }, "prevote")
                | (Payload::Precommit { .. }, "precommit")
                | (Payload::Kick { .. }, "kick")
                | (Payload::SetRole { .. }, "setrole")
                | (Payload::DefineRole { .. }, "definerole")
                | (Payload::Attest { .. }, "attest")
                | (Payload::Depart { .. }, "depart")
                | (Payload::Arrive { .. }, "arrive")
                | (Payload::Verdict { .. }, "verdict")
                | (Payload::Portal { .. }, "portal")
                | (Payload::Provenance { .. }, "provenance")
                | (Payload::Ownership { .. }, "ownership")
                | (Payload::Ban { .. }, "ban")
        ),
        _ => false,
    }
}

pub fn validate(r: &FaultRule) -> Result<(), String> {
    if [r.drop, r.delay_ms.is_some(), r.deliver].iter().filter(|&&x| x).count() != 1 {
        return Err(format!("fault rule needs exactly one of `drop = true`, `delay_ms` or `deliver = true`: {r:?}"));
    }
    if let Some(c) = &r.msg {
        if !MSG_CLASSES.contains(&c.as_str()) {
            return Err(format!("fault rule: unknown msg class `{c}` (one of {MSG_CLASSES:?})"));
        }
    }
    if (r.stamp.is_some() || r.seq.is_some()) && r.msg.as_deref().is_some_and(|c| c == "fetch" || c == "batch" || c == "join") {
        return Err(format!("fault rule: stamp and seq apply to chain entries only: {r:?}"));
    }
    if r.round.is_some() && !matches!(r.msg.as_deref(), Some("propose" | "prevote" | "precommit")) {
        return Err(format!("fault rule: round needs msg propose, prevote or precommit: {r:?}"));
    }
    if r.nil.is_some() && !matches!(r.msg.as_deref(), Some("prevote" | "precommit")) {
        return Err(format!("fault rule: nil needs msg prevote or precommit: {r:?}"));
    }
    if r.delay_grows && (r.delay_ms.is_none() || r.after_ms.is_none()) {
        return Err(format!("fault rule: delay_grows needs delay_ms and after_ms: {r:?}"));
    }
    Ok(())
}

fn matches(r: &FaultRule, t_us: u64, w: u32, from: u32, to: u32, m: &Msg) -> bool {
    let entry = match m {
        Msg::Entry(e) => Some(e),
        _ => None,
    };
    r.after_ms.is_none_or(|a| t_us >= a * 1000)
        && r.before_ms.is_none_or(|b| t_us < b * 1000)
        && r.world.is_none_or(|x| x == w)
        && r.from.as_ref().is_none_or(|x| x.contains(from))
        && r.to.as_ref().is_none_or(|x| x.contains(to))
        && r.involves.as_ref().is_none_or(|x| x.contains(from) || x.contains(to))
        && r.partition.as_ref().is_none_or(|groups| {
            let g = |m: u32| groups.iter().position(|v| v.contains(&m));
            matches!((g(from), g(to)), (Some(a), Some(b)) if a != b)
        })
        && r.msg.as_ref().is_none_or(|c| is_class(m, c))
        && r.stamp.is_none_or(|s| entry.is_some_and(|e| e.stamp == s))
        && r.seq.is_none_or(|s| entry.is_some_and(|e| e.seq == s))
        && r.author.as_ref().is_none_or(|x| entry.is_some_and(|e| x.contains(e.member)))
        && r.round.is_none_or(|x| {
            entry.is_some_and(|e| matches!(e.payload, Payload::Propose { round, .. } | Payload::Prevote { round, .. } | Payload::Precommit { round, .. } if round == x))
        })
        && r.nil.is_none_or(|x| entry.is_some_and(|e| matches!(e.payload, Payload::Prevote { value, .. } | Payload::Precommit { value, .. } if (value == 0) == x)))
}

/// The scripted faults as pings see them (admin indicators, design 10.8): only the rules that apply
/// to every message on a link (no message kind, author, stamp, seq, round or nil, and no `times`
/// limit), first match wins. Stateless, so the real filter's counters are untouched.
pub fn ping_filter(rules: &[FaultRule]) -> Option<pe_session_harness::PingFilter> {
    let link: Vec<FaultRule> = rules
        .iter()
        .filter(|r| r.msg.is_none() && r.author.is_none() && r.stamp.is_none() && r.seq.is_none() && r.round.is_none() && r.nil.is_none() && r.times.is_none())
        .cloned()
        .collect();
    if link.is_empty() {
        return None;
    }
    Some(Box::new(move |t, from, to| {
        for r in &link {
            let on = r.after_ms.is_none_or(|a| t >= a * 1000)
                && r.before_ms.is_none_or(|b| t < b * 1000)
                && r.from.as_ref().is_none_or(|x| x.contains(from))
                && r.to.as_ref().is_none_or(|x| x.contains(to))
                && r.involves.as_ref().is_none_or(|x| x.contains(from) || x.contains(to))
                && r.partition.as_ref().is_none_or(|groups| {
                    let g = |m: u32| groups.iter().position(|v| v.contains(&m));
                    matches!((g(from), g(to)), (Some(a), Some(b)) if a != b)
                });
            if on {
                return match (r.drop, r.delay_ms) {
                    (true, _) => Fate::Drop,
                    (false, Some(d)) if r.delay_grows => Fate::Delay(t - r.after_ms.unwrap_or(0) * 1000 + d * 1000),
                    (false, Some(d)) => Fate::Delay(d * 1000),
                    (false, None) => Fate::Deliver,
                };
            }
        }
        Fate::Deliver
    }))
}

/// A filter from a Byzantine behaviour: `Some(fate)` decides the message.
pub type Hook = Box<dyn FnMut(u64, u32, u32, u32, &Msg) -> Option<Fate>>;

/// The send filter: Byzantine hooks first (a member hiding its real messages wins over any
/// rule), then the rules in file order; anything unmatched is delivered.
pub fn filter(rules: Vec<FaultRule>, hooks: Vec<Hook>) -> Option<Filter> {
    if rules.is_empty() && hooks.is_empty() {
        return None;
    }
    let mut left: Vec<Option<u64>> = rules.iter().map(|r| r.times).collect();
    let mut hooks = hooks;
    Some(Box::new(move |t, w, from, to, m| {
        for h in hooks.iter_mut() {
            if let Some(f) = h(t, w, from, to, m) {
                return f;
            }
        }
        for (r, n) in rules.iter().zip(left.iter_mut()) {
            if *n == Some(0) || !matches(r, t, w, from, to, m) {
                continue;
            }
            if let Some(k) = n.as_mut() {
                *k -= 1;
            }
            return match (r.drop, r.delay_ms) {
                (true, _) => Fate::Drop,
                // delay_grows: the delay grows with the send time (a link growing slower).
                (false, Some(d)) if r.delay_grows => Fate::Delay(t - r.after_ms.unwrap_or(0) * 1000 + d * 1000),
                (false, Some(d)) => Fate::Delay(d * 1000),
                (false, None) => Fate::Deliver,
            };
        }
        Fate::Deliver
    }))
}

/// Crash and recovery events, applied before the step of their millisecond.
pub fn apply_crashes(sim: &mut Sim, crashes: &[Crash]) {
    let now = sim.net.now_us;
    for c in crashes {
        let set = if now == c.at_ms * 1000 {
            Some(true)
        } else if c.recover_ms.is_some_and(|r| now == r * 1000) {
            Some(false)
        } else {
            None
        };
        if let Some(v) = set {
            if let Some(p) = sim.peers.iter_mut().find(|p| p.id == c.member) {
                p.crashed = v;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pe_session::session::{Entry, EntryId};

    fn rule(t: &str) -> FaultRule {
        toml::from_str(t).unwrap()
    }

    fn touch(member: u32, seq: u32, stamp: u64) -> Msg {
        Msg::Entry(pe_session::sign::test_sign(1, Entry { member, seq, prev: EntryId::NONE, stamp, payload: Payload::Touch { obj: 1, impulse: [0.0; 3], grab: false }, carried: None }))
    }

    fn ack(member: u32) -> Msg {
        Msg::Entry(pe_session::sign::test_sign(1, Entry { member, seq: 1, prev: EntryId::NONE, stamp: 0, payload: Payload::Ack { horizon: 0, heads: vec![], check: None, epoch: 0 }, carried: None }))
    }

    fn fate(f: &mut Filter, t_ms: u64, from: u32, to: u32, m: &Msg) -> String {
        match f(t_ms * 1000, 1, from, to, m) {
            Fate::Deliver => "deliver".into(),
            Fate::Drop => "drop".into(),
            Fate::Delay(d) => format!("delay {}", d / 1000),
        }
    }

    #[test]
    fn first_matching_rule_wins_and_times_runs_out() {
        let mut f = filter(
            vec![rule("from = 3\nto = 1\ndelay_ms = 10\ntimes = 1"), rule("from = 3\nmsg = \"touch\"\nstamp = 5\ndrop = true"), rule("from = 3\nafter_ms = 100\nbefore_ms = 200\ndelay_ms = 7")],
            vec![],
        )
        .unwrap();
        assert_eq!(fate(&mut f, 0, 3, 1, &touch(3, 1, 5)), "delay 10");
        assert_eq!(fate(&mut f, 0, 3, 1, &touch(3, 1, 5)), "drop", "rule 1 used up");
        assert_eq!(fate(&mut f, 0, 3, 2, &ack(3)), "deliver");
        assert_eq!(fate(&mut f, 150, 3, 2, &ack(3)), "delay 7");
        assert_eq!(fate(&mut f, 200, 3, 2, &ack(3)), "deliver", "before_ms is exclusive");
    }

    #[test]
    fn partition_and_involves() {
        let mut f = filter(vec![rule("partition = [[1, 2], [3]]\ndrop = true"), rule("involves = 4\ndelay_ms = 1")], vec![]).unwrap();
        assert_eq!(fate(&mut f, 0, 1, 3, &ack(1)), "drop");
        assert_eq!(fate(&mut f, 0, 3, 2, &ack(3)), "drop");
        assert_eq!(fate(&mut f, 0, 1, 2, &ack(1)), "deliver");
        assert_eq!(fate(&mut f, 0, 4, 1, &ack(4)), "delay 1", "member 4 is in no group");
        assert_eq!(fate(&mut f, 0, 2, 4, &ack(2)), "delay 1");
    }

    #[test]
    fn author_and_deliver() {
        // As p2-4-aheadlink: 4's own acks pass, every other entry to 3 is delayed.
        let mut f = filter(vec![rule("to = 3\nfrom = 4\nauthor = 4\nmsg = \"ack\"\ndeliver = true"), rule("to = 3\nmsg = \"entry\"\ndelay_ms = 5")], vec![]).unwrap();
        assert_eq!(fate(&mut f, 0, 4, 3, &ack(4)), "deliver");
        assert_eq!(fate(&mut f, 0, 4, 3, &ack(1)), "delay 5", "relayed: authored by 1");
        assert_eq!(fate(&mut f, 0, 4, 3, &touch(4, 1, 0)), "delay 5", "not an ack");
        assert_eq!(fate(&mut f, 0, 4, 2, &touch(4, 1, 0)), "deliver");
    }

    fn vote(member: u32, payload: Payload) -> Msg {
        Msg::Entry(pe_session::sign::test_sign(1, Entry { member, seq: 1, prev: EntryId::NONE, stamp: 0, payload, carried: None }))
    }

    #[test]
    fn join_round_nil_and_growing_delay() {
        let mut f = filter(
            vec![
                rule("to = 3\nmsg = \"join\"\ndrop = true"),
                rule("from = 9\nmsg = \"prevote\"\nround = 0\nnil = false\ndrop = true"),
                rule("from = 8\nafter_ms = 2000\ndelay_ms = 30\ndelay_grows = true"),
            ],
            vec![],
        )
        .unwrap();
        let join = Msg::Join { member: 5, attests: vec![], sig: [0; 64], expires: u64::MAX };
        assert_eq!(fate(&mut f, 0, 5, 3, &join), "drop");
        assert_eq!(fate(&mut f, 0, 5, 2, &join), "deliver");
        assert_eq!(fate(&mut f, 0, 9, 1, &vote(9, Payload::Prevote { epoch: 0, round: 0, value: 7 })), "drop");
        assert_eq!(fate(&mut f, 0, 9, 1, &vote(9, Payload::Prevote { epoch: 0, round: 0, value: 0 })), "deliver", "nil");
        assert_eq!(fate(&mut f, 0, 9, 1, &vote(9, Payload::Prevote { epoch: 0, round: 1, value: 7 })), "deliver", "round 1");
        assert_eq!(fate(&mut f, 1999, 8, 1, &ack(8)), "deliver");
        assert_eq!(fate(&mut f, 2000, 8, 1, &ack(8)), "delay 30");
        assert_eq!(fate(&mut f, 3000, 8, 1, &ack(8)), "delay 1030", "slowref: sent at t, arrives near 2t - 2 s");
    }

    #[test]
    fn validation() {
        assert!(validate(&rule("from = 1")).is_err(), "needs an action");
        assert!(validate(&rule("drop = true\ndelay_ms = 3")).is_err(), "one action only");
        assert!(validate(&rule("deliver = true\ndrop = true")).is_err(), "one action only");
        assert!(validate(&rule("deliver = true")).is_ok());
        assert!(validate(&rule("msg = \"tuch\"\ndrop = true")).is_err());
        assert!(validate(&rule("msg = \"fetch\"\nseq = 1\ndrop = true")).is_err());
        assert!(validate(&rule("msg = \"vote\"\ndrop = true")).is_ok());
        assert!(validate(&rule("msg = \"vote\"\nround = 0\ndrop = true")).is_err());
        assert!(validate(&rule("msg = \"propose\"\nnil = true\ndrop = true")).is_err());
        assert!(validate(&rule("delay_ms = 5\ndelay_grows = true")).is_err(), "needs after_ms");
        assert!(validate(&rule("msg = \"join\"\nseq = 1\ndrop = true")).is_err());
    }
}
