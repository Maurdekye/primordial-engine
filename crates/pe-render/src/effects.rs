//! One-shot effects fire once (render.md 8, D6; divergence.md 8 as landed at 069e147d).
//!
//! - A key already played is never played again, across replays. Keys are kept for `W_fg` (2 s).
//! - A new effect from a first simulation plays. One from a re-simulation plays only if the entry whose arrival
//!   caused the re-simulation arrived within `T_effect_lag` (1 s) of the effect's frame; one with no newly arrived
//!   cause (a repair, a merge) never plays. So a remote push at 0.6 s one-way still sounds once, and a 60 s merge
//!   plays nothing.
//! - A played effect whose key lands in S5's retracted list is stopped (its sink fades it over `T_blend`).
//! - Own effects play at once; others wait `D_effect` (default 0 s).
//!
//! pe-render plays particles itself; sounds and haptics go to their sinks (pe-app, pe-xr) already de-duplicated.

use std::collections::BTreeMap;
use std::time::Duration;

use pe_seams::{Effect, EffectKey, FrameView, HostTime};

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EffectSettings {
    /// `T_effect_lag`, 1 s.
    pub t_effect_lag: Duration,
    /// `W_fg`, 2 s: how long a played key is remembered.
    pub w_fg: Duration,
    /// `D_effect`, 0 s: the wait before others' effects.
    pub d_effect: Duration,
}

impl Default for EffectSettings {
    fn default() -> Self {
        EffectSettings { t_effect_lag: Duration::from_secs(1), w_fg: Duration::from_secs(2), d_effect: Duration::ZERO }
    }
}

/// What the gate tells the sinks.
#[derive(Clone, Debug, PartialEq)]
pub enum EffectCommand {
    Play(Effect),
    /// Stop a played effect, fading.
    Stop(EffectKey),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EffectCounters {
    pub played: u64,
    /// A key seen again (a replay) and not replayed.
    pub replays_suppressed: u64,
    /// Never played: re-simulated with no newly arrived cause, or with its cause later than `T_effect_lag`.
    pub stale_dropped: u64,
    pub stopped: u64,
}

#[derive(Default)]
pub struct EffectGate {
    settings: EffectSettings,
    /// Played keys, with their frame's host time (for `W_fg`).
    played: BTreeMap<EffectKey, HostTime>,
    /// Others' effects waiting `D_effect`: due time, the frame's time, the effect.
    waiting: Vec<(HostTime, HostTime, Effect)>,
    counters: EffectCounters,
}

impl EffectGate {
    pub fn new(settings: EffectSettings) -> EffectGate {
        EffectGate { settings, ..Default::default() }
    }

    pub fn counters(&self) -> EffectCounters {
        self.counters
    }

    /// The host time of an effect's frame, from the view's own frame and time at the world's simulation rate.
    fn frame_time(view: &FrameView, e: &Effect, sim_rate_hz: u32) -> HostTime {
        let back = view.frame.0.saturating_sub(e.key.frame.0);
        if sim_rate_hz == 0 {
            return view.sim_time;
        }
        let ns = (back as u128 * 1_000_000_000).div_ceil(sim_rate_hz as u128);
        HostTime(view.sim_time.0.saturating_sub(i64::try_from(ns).unwrap_or(i64::MAX)))
    }

    /// A newly taken frame view's effects and retractions, at `now`. `sim_rate_hz` is the world's simulation rate.
    pub fn take(&mut self, view: &FrameView, sim_rate_hz: u32, now: HostTime) -> Vec<EffectCommand> {
        let mut out = Vec::new();
        for k in &view.retracted {
            if self.played.contains_key(k) {
                out.push(EffectCommand::Stop(*k));
                self.counters.stopped += 1;
            }
            self.waiting.retain(|(_, _, e)| e.key != *k);
        }
        for e in &view.effects {
            if self.played.contains_key(&e.key) || self.waiting.iter().any(|(_, _, w)| w.key == e.key) {
                self.counters.replays_suppressed += 1;
                continue;
            }
            let at = Self::frame_time(view, e, sim_rate_hz);
            if e.resimulated {
                let fresh =
                    e.trigger_arrived.is_some_and(|t| t.secs_since(at) <= self.settings.t_effect_lag.as_secs_f64());
                if !fresh {
                    self.counters.stale_dropped += 1;
                    continue;
                }
            }
            if e.own || self.settings.d_effect.is_zero() {
                self.played.insert(e.key, at);
                self.counters.played += 1;
                out.push(EffectCommand::Play(e.clone()));
            } else {
                self.waiting.push((now.plus(self.settings.d_effect), at, e.clone()));
            }
        }
        out.extend(self.due(now));
        // Forget keys past W_fg, by their frame's time.
        let w = self.settings.w_fg;
        self.played.retain(|_, t| t.plus(w) >= now);
        out
    }

    /// Others' effects whose `D_effect` wait is over.
    pub fn due(&mut self, now: HostTime) -> Vec<EffectCommand> {
        let mut out = Vec::new();
        let mut i = 0;
        while i < self.waiting.len() {
            if self.waiting[i].0 <= now {
                let (_, at, e) = self.waiting.remove(i);
                self.played.insert(e.key, at);
                self.counters.played += 1;
                out.push(EffectCommand::Play(e));
            } else {
                i += 1;
            }
        }
        out
    }
}
