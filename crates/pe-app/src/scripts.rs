//! What each active script is doing on this machine, said where a person or an agent looks (M1 redo F5).
//!
//! In the M1 recording the agent's lamp script built on both machines and never ran: it bound the engine's `Light`
//! without listing it, the runner refused the bind, and the refusal reached nothing but a count. Now the simulation
//! thread compares, after each publication, the world's active scripts with what its replica's runner says
//! ([`pe_seams::replica::ScriptNotes`]), and logs each change:
//!
//! - **Runs:** `scripts: script <id> runs on this machine (active from frame F)`, an info line on stdout.
//! - **Doesn't run:** `pe-app: script <id> doesn't run on this machine (active since frame F): <why>` on stderr, why
//!   being the runner's: it won't bind and why. A module that isn't built here yet is the attester's to log
//!   (builds.rs), so it isn't logged twice.
//! - **Faulted:** `pe-app: script <id> faulted at frame F and stopped: <system and fault>` on stderr. The fault is
//!   world state (logic.md 3.6): it stops the script on every machine.
//!
//! The same states go to pe-api's `scripts_not_running` (session_info, and the objects a query returns), with the
//! attester's list.

use std::collections::BTreeMap;

use pe_seams::Frame;
use pe_seams::replica::ScriptNotes;
use pe_world::WorldSnapshot;
use pe_world::script::AssetId;

/// The runner's reason for a module it doesn't hold: the attester's case, which builds.rs logs.
const NOT_HELD: &str = pe_replica::logic::MODULE_NOT_HELD;

/// One active script's state here.
#[derive(Clone, Debug, PartialEq, Eq)]
enum State {
    Runs,
    /// Not run here, with the runner's reason. `held` is false for a module this machine doesn't hold.
    NotRun {
        why: String,
        held: bool,
    },
    Faulted {
        frame: Frame,
        what: String,
    },
}

/// A line for the console: stdout's info or stderr's warning.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Line {
    Out(String),
    Err(String),
}

/// The scripts' states as last logged (module doc).
#[derive(Default)]
pub struct ScriptWatch {
    /// Each active script's frame (its row's `activate_at`) and state, as the last call found them.
    seen: BTreeMap<AssetId, (Frame, State)>,
    /// What was last said of each script: its version V and state. Kept when a script stops being active, so a version
    /// that leaves and comes back in the same state (the author's prediction dropped by a rollback and resumed, with a
    /// new `activate_at`: F3's C4) says nothing again.
    said: BTreeMap<AssetId, (pe_seams::Hash, State)>,
}

impl ScriptWatch {
    /// Compares `snap`'s active scripts (a row whose `activate_at` is at or before its frame) with `notes`, and returns
    /// a line for each script whose version or state differs from what was last said of it. A script that stops being
    /// active (removed, or a rollback before its activation) is left out of the API's list without a line.
    pub fn update(&mut self, snap: &WorldSnapshot, notes: &ScriptNotes) -> Vec<Line> {
        let now = snap.frame();
        let mut next = BTreeMap::new();
        for (asset, row) in snap.modules() {
            if row.activate_at > now {
                continue;
            }
            let state = match (row.faulted, notes.not_run.get(asset)) {
                (Some((frame, code)), _) => {
                    let what = notes.faults.get(asset).cloned().unwrap_or_else(|| format!("fault code {code}"));
                    State::Faulted { frame, what }
                }
                (None, Some(why)) => State::NotRun { why: why.clone(), held: why != NOT_HELD },
                (None, None) => State::Runs,
            };
            next.insert(*asset, (row.activate_at, row.v(), state));
        }
        let mut lines = Vec::new();
        for (asset, (since, v, state)) in &next {
            let now_said = (*v, state.clone());
            if self.said.get(asset) == Some(&now_said) {
                continue;
            }
            self.said.insert(*asset, now_said);
            let id = short(asset);
            match state {
                State::Runs => {
                    lines.push(Line::Out(format!(
                        "scripts: script {id} runs on this machine (active from frame {})",
                        since.0
                    )));
                }
                State::NotRun { held: false, .. } => {}
                State::NotRun { why, held: true } => lines.push(Line::Err(format!(
                    "pe-app: script {id} doesn't run on this machine (active since frame {}): {why}",
                    since.0
                ))),
                State::Faulted { frame, what } => lines
                    .push(Line::Err(format!("pe-app: script {id} faulted at frame {} and stopped: {what}", frame.0))),
            }
        }
        self.seen = next.into_iter().map(|(a, (since, _, state))| (a, (since, state))).collect();
        lines
    }

    /// The scripts not running here as pe-api lists them (S6's `scripts_not_running`): (asset, since, why). A module
    /// not held here is left out: the attester's list has it, with the build's reason.
    pub fn not_running(&self) -> Vec<(AssetId, Frame, String)> {
        (self.seen.iter())
            .filter_map(|(asset, (since, state))| match state {
                State::Runs | State::NotRun { held: false, .. } => None,
                State::NotRun { why, held: true } => Some((*asset, *since, why.clone())),
                State::Faulted { frame, what } => Some((*asset, *frame, format!("it faulted and stopped: {what}"))),
            })
            .collect()
    }
}

/// The first four bytes in hex, as builds.rs's lines name a script.
fn short(asset: &AssetId) -> String {
    asset.iter().take(4).map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use pe_world::script::LogicReport;
    use pe_world::test_util::{ASSET, five_kinds};

    use super::*;

    fn notes(not_run: &[(AssetId, &str)], faults: &[(AssetId, &str)]) -> ScriptNotes {
        ScriptNotes {
            not_run: not_run.iter().map(|(a, w)| (*a, w.to_string())).collect(),
            faults: faults.iter().map(|(a, w)| (*a, w.to_string())).collect(),
        }
    }

    /// Each change of an active script's state is one line, said once; the API's list has the scripts that don't run.
    #[test]
    fn each_change_of_a_script_s_state_is_said_once() {
        // five_kinds' script is active from frame 1.
        let mut w = five_kinds();
        w.set_frame(Frame(10));
        let snap = w.snapshot(Frame(10));
        let id = short(&ASSET);
        let mut watch = ScriptWatch::default();

        let why = "it won't bind: system blink uses pe.Light, which the script doesn't declare";
        let lines = watch.update(&snap, &notes(&[(ASSET, why)], &[]));
        let err = format!("pe-app: script {id} doesn't run on this machine (active since frame 1): {why}");
        assert_eq!(lines, [Line::Err(err)]);
        assert_eq!(watch.not_running(), [(ASSET, Frame(1), why.to_string())]);
        assert!(watch.update(&snap, &notes(&[(ASSET, why)], &[])).is_empty(), "said once");

        // It binds: it runs.
        let lines = watch.update(&snap, &notes(&[], &[]));
        assert_eq!(lines, [Line::Out(format!("scripts: script {id} runs on this machine (active from frame 1)"))]);
        assert!(watch.not_running().is_empty());
        // A rollback before its activation and back (as when the author's prediction drops and resumes): said once.
        let mut before = five_kinds();
        before.set_frame(Frame(0));
        assert!(watch.update(&before.snapshot(Frame(0)), &notes(&[], &[])).is_empty());
        assert!(watch.update(&snap, &notes(&[], &[])).is_empty(), "the same version, running, isn't said again");

        // A module not held here is the attester's to say: no line, not in this list.
        assert!(watch.update(&snap, &notes(&[(ASSET, NOT_HELD)], &[])).is_empty());
        assert!(watch.not_running().is_empty());

        // It faults: world state, with the runner's detail.
        w.logic_stage(|_, _, _, _| ());
        w.after_logic(Frame(10), &LogicReport { faults: vec![(ASSET, 5)], ..LogicReport::default() });
        let snap = w.snapshot(Frame(10));
        let what = "system blink faulted at frame 10: DivideByZero";
        let lines = watch.update(&snap, &notes(&[], &[(ASSET, what)]));
        assert_eq!(lines, [Line::Err(format!("pe-app: script {id} faulted at frame 10 and stopped: {what}"))]);
        assert_eq!(watch.not_running(), [(ASSET, Frame(10), format!("it faulted and stopped: {what}"))]);
    }

    /// A script not yet active has no state, and says nothing.
    #[test]
    fn a_script_not_yet_active_says_nothing() {
        let mut w = five_kinds();
        w.set_frame(Frame(0));
        let snap = w.snapshot(Frame(0));
        let mut watch = ScriptWatch::default();
        assert!(watch.update(&snap, &notes(&[], &[])).is_empty());
        assert!(watch.not_running().is_empty());
    }
}
