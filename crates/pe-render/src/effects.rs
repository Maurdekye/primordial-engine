//! One-shot effects fire once (render.md 8, D6; divergence.md 8 as landed at 069e147d).
//!
//! - A key already played is never played again, across replays. Keys are kept for `W_fg` (2 s).
//! - A new effect from a first simulation plays. One from a re-simulation plays only if the entry whose arrival
//!   caused the re-simulation arrived within `T_effect_lag` (1 s) of the effect's frame; one with no newly arrived
//!   cause (a repair, a merge) never plays. So a remote push at 0.6 s one-way still sounds once, and a 60 s merge
//!   plays nothing.
//! - A played effect whose key lands in S5's retracted list is stopped (its sink fades it over `T_blend`).
//! - Own effects play at once; others wait `D_effect` (default 0 s).
//! - None plays before the drawn time reaches its frame: under Shown the simulation runs ahead of the moment drawn
//!   (app.md 3), so a sound or a haptic pulse waits for the frame that shows it (@coordinator-opus). The
//!   presenter passes the drawn time; under every other rule a frame view is never ahead of it, and nothing waits.
//!
//! pe-render plays particles itself; sounds and haptics go to their sinks (pe-app, pe-xr) already de-duplicated.

use std::collections::BTreeMap;
use std::time::Duration;

use pe_seams::{Effect, EffectKey, Frame, FrameView, HostTime};

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
    /// Stops sent at a gap, for every played effect the newest view doesn't list (see [`EffectGate::gap`]).
    pub gap_stops: u64,
}

#[derive(Default)]
pub struct EffectGate {
    settings: EffectSettings,
    /// Played keys, with their frame's host time (for `W_fg`).
    played: BTreeMap<EffectKey, HostTime>,
    /// Others' effects waiting `D_effect`: due time, the frame's time, the effect.
    waiting: Vec<(HostTime, HostTime, Effect)>,
    counters: EffectCounters,
    /// The `final_frame` of the newest view taken: effects of frames below it can no longer be retracted.
    last_final: Option<Frame>,
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

    /// A newly taken frame view's effects and retractions, at `now`, the display frame's drawn time. `sim_rate_hz` is
    /// the world's simulation rate.
    pub fn take(&mut self, view: &FrameView, sim_rate_hz: u32, now: HostTime) -> Vec<EffectCommand> {
        let mut out = Vec::new();
        self.last_final = view.final_frame.max(self.last_final);
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
            // Own effects at once and others after `D_effect`, but never before the drawn time reaches the frame.
            let due =
                if e.own || self.settings.d_effect.is_zero() { at } else { now.plus(self.settings.d_effect).max(at) };
            if due <= now {
                self.played.insert(e.key, at);
                self.counters.played += 1;
                out.push(EffectCommand::Play(e.clone()));
            } else {
                self.waiting.push((due, at, e.clone()));
            }
        }
        out.extend(self.due(now));
        // Forget keys past W_fg, by their frame's time.
        let w = self.settings.w_fg;
        self.played.retain(|_, t| t.plus(w) >= now);
        out
    }

    /// The next view follows a gap (render.md 3): the views between are lost, with any retraction in them. A played
    /// effect of a frame below the `final_frame` of the last view taken before the gap was final, so no lost view can
    /// have retracted it: it plays on (@coordinator-opus). A newer one, whose retraction may have been
    /// lost, is stopped (its sink fades it; a finished one-shot ignores it) unless the newest view lists it, and stays
    /// remembered, so a replay never plays it again. S5 lists effects as they are emitted, not what is on, so this is
    /// all a reader can know. Waiting effects go. The newest view's effects then pass through `take` as usual.
    pub fn gap(&mut self, newest: &FrameView) -> Vec<EffectCommand> {
        let listed: Vec<EffectKey> = newest.effects.iter().map(|e| e.key).collect();
        let fin = self.last_final;
        self.waiting.clear();
        let out: Vec<EffectCommand> = self
            .played
            .keys()
            .filter(|k| !listed.contains(k) && fin.is_none_or(|f| k.frame >= f))
            .map(|k| EffectCommand::Stop(*k))
            .collect();
        self.counters.gap_stops += out.len() as u64;
        out
    }

    /// Waiting effects that are due: others' after `D_effect`, and any whose frame the drawn time has reached.
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
