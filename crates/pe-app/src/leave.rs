//! The graceful quit (app.md 4; agreement.md 8;
//! @item:primordial-engine/pe-app-m1-leave-a-graceful-quit-submits-leave-ke). [`crate::sim::Sim`] submits this
//! member's `Leave` and then nothing local, and keeps ticking, so the leaver still votes E's suffix
//! (@design-reviewer's (b)). The quit waits until every remaining member has told it
//! that E is final (S3's `leave_settled`), until no other member is connected, or until a bound passes. Quitting at
//! E's local finality instead stranded the other member in 3 of 9 pe-agree runs (agreement.md 8).
//!
//! The rulings (@coordinator-opus, with @fable-reviewer's refinements):
//! - **The bound adapts to the link:** [`VIEWS`] smoothed view times, clamped to [`MIN_WAIT`]..=[`MAX_WAIT`], and
//!   overridable locally. pe-app measures the view time itself, as the smoothed interval between S3's proposals
//!   ([`ViewTime`]), which tracks pe-agree's own (core.rs's `view_time_ns`, not exposed by S3). A second quit request
//!   ends the wait at once: Ctrl-C on the console ends the process, wherever the quit is.
//! - **"leaving…" on the console only**, in both modes, for M1.
//! - **The messages are in player words**, and say what happens to the world. M1 keeps no world past a quit (saves are
//!   M5), so a stranded member loses nothing a normal quit doesn't. The remaining member is told at once when the
//!   link drops with the leaver's signed `Leave` in hand, and a silent drop says the other may come back.

use std::time::Duration;

use pe_seams::{Frame, HostTime, MemberKey};

/// How many smoothed view times the quit waits for E's confirmations, at most (before the clamp).
pub const VIEWS: u32 = 8;
/// The shortest wait: pe-agree's leavers quit 355-558 ms after their `Leave` on simulated links (agreement.md 8), and
/// each remaining member repeats its confirmation once a second.
pub const MIN_WAIT: Duration = Duration::from_secs(5);
/// The longest wait, however slow the views.
pub const MAX_WAIT: Duration = Duration::from_secs(15);
/// How far around the present a remaining member looks for a member's signed `Leave` when that member's link drops:
/// past stamp-ahead's largest lead (3 s, `LeadConfig::MAX_LEAD`, once stamp-ahead lands) plus a second's margin, and
/// as far below the final frame. Once stamp-ahead is on main this follows its clamp (@design-reviewer's Low 3).
pub const SCAN: Duration = Duration::from_secs(4);

/// Where this member is in its graceful quit.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Leaving {
    #[default]
    Staying,
    /// Asked to leave: step 2 submits the `Leave` at its next tick.
    Asked,
    /// The `Leave` went out stamped `stamp`: the chain's last entry, so nothing local follows it.
    Left { stamp: Frame },
}

/// What the quit's wait sees.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LeaveStatus {
    /// Not asked to leave, or the `Leave` not submitted yet.
    Pending,
    /// The `Leave` is out, and not every remaining member has confirmed E yet.
    Waiting,
    /// Every remaining member has told this one that E is final there (S3's `leave_settled`): quitting strands no one.
    Settled,
    /// No other member is connected, so there is no one to wait for.
    Alone,
}

impl LeaveStatus {
    pub(crate) fn code(self) -> u8 {
        match self {
            LeaveStatus::Pending => 0,
            LeaveStatus::Waiting => 1,
            LeaveStatus::Settled => 2,
            LeaveStatus::Alone => 3,
        }
    }

    pub(crate) fn from_code(c: u8) -> LeaveStatus {
        match c {
            1 => LeaveStatus::Waiting,
            2 => LeaveStatus::Settled,
            3 => LeaveStatus::Alone,
            _ => LeaveStatus::Pending,
        }
    }
}

/// A smoothed view time, measured as the interval between successive proposals S3 reports (`AgreeEvent::Proposed`,
/// made or received), weighted 1/8 as pe-agree weights its own. A proposal opens each view, so the interval is a
/// view's whole time, the cut cadence included.
#[derive(Clone, Copy, Debug, Default)]
pub struct ViewTime {
    last: Option<HostTime>,
    ns: Option<i64>,
}

impl ViewTime {
    /// A proposal reported at host time `at`. Two in one tick give no interval.
    pub fn proposed(&mut self, at: HostTime) {
        if let Some(last) = self.last {
            let took = at.0.saturating_sub(last.0);
            if took > 0 {
                self.ns = Some(self.ns.map_or(took, |ns| (ns.saturating_mul(7).saturating_add(took)) / 8));
            }
        }
        self.last = Some(self.last.map_or(at, |l| l.max(at)));
    }

    /// The smoothed view time, once two proposals have been seen.
    pub fn get(&self) -> Option<Duration> {
        self.ns.map(|ns| Duration::from_nanos(u64::try_from(ns).unwrap_or(0)))
    }
}

/// The quit's bound for a smoothed view time `view`: [`VIEWS`] of it, clamped to [`MIN_WAIT`]..=[`MAX_WAIT`];
/// [`MIN_WAIT`] before any view was measured.
pub fn bound(view: Option<Duration>) -> Duration {
    view.map_or(MIN_WAIT, |v| v.saturating_mul(VIEWS).clamp(MIN_WAIT, MAX_WAIT))
}

/// A member as the console names it: "player" and the first four bytes of its key, as pe-app's start-up line does.
pub fn player(m: MemberKey) -> String {
    let hex: String = m.0[..4].iter().map(|b| format!("{b:02x}")).collect();
    format!("player {hex}")
}

fn capitalized(s: String) -> String {
    let mut c = s.chars();
    c.next().map_or(String::new(), |f| f.to_uppercase().chain(c).collect())
}

/// The remaining member, when `m`'s link drops while `m` is still in its epoch and its signed `Leave` is in hand.
pub fn stranded(m: MemberKey) -> String {
    capitalized(format!(
        "{} left before the session could confirm it. You can keep playing, but changes from now on won't be shared, \
         and this world ends when you quit.",
        player(m)
    ))
}

/// The remaining member, when `m`'s link drops with no `Leave` from it in hand.
pub fn dropped(m: MemberKey) -> String {
    format!(
        "{}'s connection dropped. They may come back; until then, changes wait to be confirmed.",
        capitalized(player(m))
    )
}

/// The remaining member, when `m`'s link drops after its leave was confirmed.
pub fn left(m: MemberKey) -> String {
    capitalized(format!("{} left the session.", player(m)))
}

/// The members named for the console: "player 1a2b3c4d and player …", or "the other players" for none.
fn players(others: &[MemberKey]) -> String {
    let names: Vec<String> = others.iter().map(|&m| player(m)).collect();
    if names.is_empty() { "the other players".to_string() } else { names.join(" and ") }
}

/// The leaver, when its bound passed before every remaining member in `others` confirmed its leave.
pub fn left_unconfirmed(others: &[MemberKey]) -> String {
    let who = players(others);
    format!(
        "Left before {who} confirmed it. {} can keep playing, but their changes won't be shared from now on.",
        capitalized(who.clone())
    )
}

/// The leaver, when its bound passed before agreement ever took its `Leave` (@design-reviewer's Low 1): the others
/// see only a dropped connection, and may wait for this member to come back.
pub fn leave_not_sent(others: &[MemberKey]) -> String {
    let who = players(others);
    format!("Couldn't send your leave, so {who} will see your connection drop and may wait for you to come back.")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn the_bound_is_eight_view_times_between_5_and_15_s() {
        assert_eq!(bound(None), MIN_WAIT, "before any view");
        assert_eq!(bound(Some(ms(300))), MIN_WAIT, "a LAN's views: 2.4 s, so the floor");
        assert_eq!(bound(Some(ms(1_000))), ms(8_000));
        assert_eq!(bound(Some(ms(2_600))), MAX_WAIT, "a 1.2 s link's views: 20.8 s, so the cap");
    }

    #[test]
    fn the_view_time_is_the_smoothed_interval_between_proposals() {
        let mut v = ViewTime::default();
        v.proposed(HostTime(0));
        assert_eq!(v.get(), None, "one proposal gives no interval");
        v.proposed(HostTime(800_000_000));
        assert_eq!(v.get(), Some(ms(800)));
        v.proposed(HostTime(800_000_000));
        assert_eq!(v.get(), Some(ms(800)), "two in one tick give no interval");
        v.proposed(HostTime(1_600_000_000 + 800_000_000));
        assert_eq!(v.get(), Some(ms((7 * 800 + 1_600) / 8)), "weighted 1/8");
    }

    #[test]
    fn the_messages_name_the_player() {
        let m = MemberKey([0xab; 32]);
        assert!(stranded(m).starts_with("Player abababab left before the session could confirm it."));
        assert!(dropped(m).starts_with("Player abababab's connection dropped. They may come back"));
        assert_eq!(left(m), "Player abababab left the session.");
        assert!(left_unconfirmed(&[m]).starts_with("Left before player abababab confirmed it. Player abababab can"));
        assert!(leave_not_sent(&[m]).starts_with("Couldn't send your leave, so player abababab will see"));
    }
}
