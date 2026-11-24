//! The headset test's hand-feel switch (the pull-ahead plan's 1.10; @coordinator-opus's ruling: console
//! commands on A, the pair on the status line, the defaults unchanged). Two console commands:
//!
//! - `hand-mode closed|reach` sets how both of this member's hands take part in physics: pe-world's `Input::HandMode`,
//!   "closed hand" (D18b: physical only while the hand is closed, the default) or "always within reach". Only the
//!   hand's owner may send it. The console thread queues the mode, and the next tick's step 2 sends it for both hands
//!   at that frame's stamp ([`crate::local::submit_hand_mode`]).
//! - `draw latched|simulated` sets where pe-render draws the own hands (xr.md 5.6, D4): at the late-latched tracked
//!   pose, the default, or where the simulation has them. The presenter reads it every frame
//!   (`pe_render::display::OwnHandsSwitch`).
//!
//! Once either command has been given, the status line shows the pair ([`HandFeel::status`]). Another status, such
//! as a join's catch-up, takes the line while it is up, and the pair comes back after it.
//!
//! The command is `hand-mode`, not `hands`, since the scripted hands (T1) own `hands <routine|file|stop>`.

use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};

use pe_render::display::{OwnHands, OwnHandsSwitch};
use pe_world::components::hand_mode;

/// No mode waits to be sent.
const NONE: u8 = u8::MAX;

/// What the console asked for, shared by the console thread, the simulation thread and the presenter.
#[derive(Debug)]
pub struct HandFeel {
    /// The mode asked for and not yet sent, or [`NONE`].
    pending: AtomicU8,
    /// The mode last asked for: "closed hand" until a command.
    mode: AtomicU8,
    /// Where the own hands are drawn: the presenter reads it each frame.
    pub draw: Arc<OwnHandsSwitch>,
    /// Whether a command was given: the status line shows the pair from then on.
    used: AtomicBool,
    /// Bumped at every command, so the simulation thread rewrites the status line only when the pair changed.
    version: AtomicU64,
}

impl Default for HandFeel {
    fn default() -> HandFeel {
        HandFeel {
            pending: AtomicU8::new(NONE),
            mode: AtomicU8::new(hand_mode::CLOSED_HAND),
            draw: Arc::new(OwnHandsSwitch::new(OwnHands::LateLatched)),
            used: AtomicBool::new(false),
            version: AtomicU64::new(0),
        }
    }
}

/// This process's switch: the console reads into it, and main.rs hands it to the simulation and the presenter.
pub fn console() -> Arc<HandFeel> {
    static FEEL: OnceLock<Arc<HandFeel>> = OnceLock::new();
    FEEL.get_or_init(Arc::default).clone()
}

impl HandFeel {
    /// One console line: `None` if it isn't one of the two commands; otherwise what to tell the console, or why the
    /// command is refused.
    pub fn command(&self, line: &str) -> Option<Result<String, String>> {
        let mut words = line.split_whitespace();
        let cmd = words.next()?;
        let arg = words.next();
        let one = words.next().is_none();
        match cmd {
            "hand-mode" => Some(match arg.filter(|_| one) {
                Some("closed") => Ok(self.set_mode(hand_mode::CLOSED_HAND)),
                Some("reach") => Ok(self.set_mode(hand_mode::WITHIN_REACH)),
                _ => Err("`hand-mode` takes `closed` (physical only while closed, the default) or `reach` (always \
                          physical within reach)"
                    .into()),
            }),
            "draw" => Some(match arg.filter(|_| one) {
                Some("latched") => Ok(self.set_draw(OwnHands::LateLatched)),
                Some("simulated") => Ok(self.set_draw(OwnHands::Simulated)),
                _ => Err("`draw` takes `latched` (the own hands at the tracked pose, the default) or `simulated` \
                          (where the simulation has them)"
                    .into()),
            }),
            _ => None,
        }
    }

    fn set_mode(&self, mode: u8) -> String {
        self.mode.store(mode, Ordering::Relaxed);
        self.pending.store(mode, Ordering::Relaxed);
        self.touched();
        format!("hand-mode: {} for both hands, from the next frame", mode_name(mode))
    }

    fn set_draw(&self, own: OwnHands) -> String {
        self.draw.set(own);
        self.touched();
        format!("draw: the own hands {}, from the next display frame", draw_name(own))
    }

    fn touched(&self) {
        self.used.store(true, Ordering::Relaxed);
        self.version.fetch_add(1, Ordering::Relaxed);
    }

    /// The mode the console asked for and not yet sent: the simulation thread takes it once.
    pub fn take_pending(&self) -> Option<u8> {
        let m = self.pending.swap(NONE, Ordering::Relaxed);
        (m != NONE).then_some(m)
    }

    /// Puts a taken mode back, when no hand of this member was in the view to send it to; a newer command wins.
    pub fn put_back(&self, mode: u8) {
        let _ = self.pending.compare_exchange(NONE, mode, Ordering::Relaxed, Ordering::Relaxed);
    }

    /// Changes at every command: the simulation thread compares it to rewrite the status line only then.
    pub fn version(&self) -> u64 {
        self.version.load(Ordering::Relaxed)
    }

    /// The status line's pair, once a command was given.
    pub fn status(&self) -> Option<String> {
        self.used.load(Ordering::Relaxed).then(|| {
            format!("Hands: {} · drawn {}", mode_name(self.mode.load(Ordering::Relaxed)), draw_name(self.draw.get()))
        })
    }
}

fn mode_name(mode: u8) -> &'static str {
    if mode == hand_mode::WITHIN_REACH { "always within reach" } else { "closed hand" }
}

fn draw_name(own: OwnHands) -> &'static str {
    match own {
        OwnHands::LateLatched => "at the tracked pose",
        OwnHands::Simulated => "where simulated",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_commands_set_the_mode_and_the_drawing_and_refuse_anything_else() {
        let f = HandFeel::default();
        assert_eq!(f.status(), None, "no pair before a command");
        assert_eq!(f.take_pending(), None);
        assert_eq!(f.command("quit"), None, "not ours");
        assert_eq!(f.command("hands sweep"), None, "the scripted hands' command, not ours");
        let v = f.version();
        assert!(f.command("hand-mode reach").expect("ours").is_ok());
        assert!(f.version() > v);
        assert_eq!(f.take_pending(), Some(hand_mode::WITHIN_REACH));
        assert_eq!(f.take_pending(), None, "taken once");
        assert_eq!(f.status().as_deref(), Some("Hands: always within reach · drawn at the tracked pose"));
        assert!(f.command("draw simulated").expect("ours").is_ok());
        assert_eq!(f.draw.get(), OwnHands::Simulated);
        assert_eq!(f.status().as_deref(), Some("Hands: always within reach · drawn where simulated"));
        for bad in ["hand-mode", "hand-mode open", "hand-mode reach now", "draw", "draw sideways"] {
            assert!(f.command(bad).expect("ours").is_err(), "{bad}");
        }
        assert_eq!(f.take_pending(), None, "a refused command queues nothing");
        assert!(f.command("hand-mode closed").expect("ours").is_ok());
        assert!(f.command("draw latched").expect("ours").is_ok());
        assert_eq!(f.status().as_deref(), Some("Hands: closed hand · drawn at the tracked pose"));
    }

    #[test]
    fn a_mode_put_back_waits_but_a_newer_command_wins() {
        let f = HandFeel::default();
        f.command("hand-mode reach");
        let m = f.take_pending().expect("queued");
        f.put_back(m);
        assert_eq!(f.take_pending(), Some(hand_mode::WITHIN_REACH), "kept for a later frame");
        f.command("hand-mode reach");
        let m = f.take_pending().expect("queued");
        f.command("hand-mode closed");
        f.put_back(m);
        assert_eq!(f.take_pending(), Some(hand_mode::CLOSED_HAND), "the newer command stays");
    }
}
