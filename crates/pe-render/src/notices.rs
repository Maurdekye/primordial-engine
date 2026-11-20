//! Notices (render.md 8.1; D7: P3, P4, P5): one plain line telling the local player that something they did did not
//! happen. This module holds the lines (pure CPU), draws them into one image ([`strip`]) and places it
//! ([`window_overlay`], [`Panel`]); the scene renderer draws it as `Scene::overlay`.
//!
//! - **Refusals** of the local member's entries, from the frame view's `changes`, are told once the entry's frame is
//!   final (render.md R10): a refusal reversed by a replay is never told. While finality stalls, the refusal waits;
//!   if the verdict reverses, it is dropped.
//! - **A grab lost to a hand the player can see** is not told in words: the hands' slide and cue tell it, and the
//!   winner is in view (coordinator; api-surface.md 6.3). A grab refused for a reason the player can't
//!   see (held by a member with no hand in view, a lock, the claim cap, out of reach) is told with the reason.
//! - **Voids** (S3's `AgreeEvent::Voided`, handed in by pe-app) are told at once: a final cut decided them. A void
//!   of an entry whose refusal was already told replaces that line, so one entry is never told twice.
//! - **Only Input-class entries** (grabs, pushes, hand inputs) in M1 (coordinator): every edit
//!   entry carries the member's key, an agent's too, so an edit refusal can't be told as the player's. A void names
//!   no class, so it is told only for an entry this player's views stepped as Input (the local member's entries are
//!   stepped here first, so a void of one never comes unseen).
//! - **S5 v1.5's records** (@coordinator-opus's rulings): an `Outcome::Dropped` of the
//!   local member's Input entry is told as a void, at once, and only once whether it comes as a record or as S3's
//!   `Voided`. `Outcome::Unknown` (a repair replaced the state its verdict was stepped on) is told once per repair,
//!   not per entry. While the view's `health` is `Repairing` for longer than `resync_after_s`, a status line
//!   "Resyncing…" shows above the others.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use ab_glyph::{Font, PxScale, ScaleFont, point};
use glam::{DQuat, DVec3};
use pe_seams::{
    AgreeEvent, Class, CutRef, EntryRef, FrameView, Health, HostTime, MemberKey, Outcome, Reason, StepRecord,
    VoidReason,
};

use crate::camera::Camera;
use crate::labels::{font, text_width};
use crate::pose::{WorldPose, offset_vec};
use crate::scene::{Overlay, OverlayImage};

/// A line pe-app keeps up while a state lasts, such as "Joining: catching up…" while a joiner simulates on to the
/// present: set from the simulation thread, read by the render thread each frame ([`crate::present::Presenter::set_status`]).
#[derive(Debug, Default)]
pub struct StatusLine(Mutex<Option<String>>);

impl StatusLine {
    /// Put `text` up, or take the line down with `None`.
    pub fn set(&self, text: Option<String>) {
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = text;
    }

    pub fn get(&self) -> Option<String> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

/// When a refusal is told (render.md R10).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TellAt {
    /// Once the refused entry's frame is final: every member agrees on the verdict.
    #[default]
    Final,
    /// At the first refusal seen, even on the predicted line.
    AtOnce,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct NoticeSettings {
    pub tell_at: TellAt,
    /// Tell in words a grab lost to a hand in view too. Off: the hands' cue tells it.
    pub lost_grabs: bool,
    /// How long a line shows, seconds, fade included.
    pub life_s: f64,
    /// The fade at the end of its life, seconds.
    pub fade_s: f64,
    /// Lines shown at once; a newer one pushes the oldest out.
    pub max_lines: usize,
    /// How long a stepped entry's author is remembered, to name the winner of a `Stale` or `Deleted` refusal
    /// (`W_fg`), seconds. An older winner reads "someone".
    pub authors_s: f64,
    /// How long the view's health must stay `Repairing` before "Resyncing…" shows, seconds (about 1-2 s, ruling).
    pub resync_after_s: f64,
}

impl Default for NoticeSettings {
    fn default() -> Self {
        NoticeSettings {
            tell_at: TellAt::Final,
            lost_grabs: false,
            life_s: 5.0,
            fade_s: 0.5,
            max_lines: 3,
            authors_s: 2.0,
            resync_after_s: 1.5,
        }
    }
}

/// A line to draw now.
#[derive(Clone, Debug, PartialEq)]
pub struct Line {
    pub text: String,
    /// 1 until the fade, then down to 0.
    pub alpha: f32,
}

#[derive(Clone, Debug)]
struct Shown {
    text: String,
    /// A cooldown line: when the cooldown ends. Its seconds are counted when drawn, and all cooldown refusals share
    /// the one line (review Low on ab323229).
    cooldown_end: Option<HostTime>,
    /// The refused entries this line tells (a void of one of them retracts it from the line).
    entries: Vec<EntryRef>,
    count: u32,
    until: HostTime,
}

/// What an entry was told as.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Told {
    Refusal,
    /// A refusal judged not to need words (a grab lost to a hand in view).
    Silent,
    Void,
}

/// Entries remembered as told, so a re-stepped refusal or a repeated void is never told twice.
const TOLD_KEPT: usize = 4096;

/// The local player's notices.
pub struct Notices {
    settings: NoticeSettings,
    local: MemberKey,
    sim_rate_hz: u32,
    names: HashMap<MemberKey, String>,
    authors: VecDeque<(HostTime, EntryRef, MemberKey)>,
    /// The class of each of the local member's entries seen stepped, for voids (bounded like `told`).
    own_class: HashMap<EntryRef, Class>,
    own_order: VecDeque<EntryRef>,
    pending: HashMap<EntryRef, StepRecord>,
    told: HashMap<EntryRef, Told>,
    told_order: VecDeque<EntryRef>,
    shown: VecDeque<Shown>,
    told_total: u64,
    dropped_at_gaps: u64,
    /// Repairs already told ("Resynced…"), newest last, a few kept.
    resynced: VecDeque<CutRef>,
    /// Since when (host time) the views have said `Repairing`.
    repairing_since: Option<HostTime>,
}

impl Notices {
    pub fn new(settings: NoticeSettings, local: MemberKey, sim_rate_hz: u32) -> Notices {
        Notices {
            settings,
            local,
            sim_rate_hz: sim_rate_hz.max(1),
            names: HashMap::new(),
            authors: VecDeque::new(),
            own_class: HashMap::new(),
            own_order: VecDeque::new(),
            pending: HashMap::new(),
            told: HashMap::new(),
            told_order: VecDeque::new(),
            shown: VecDeque::new(),
            told_total: 0,
            dropped_at_gaps: 0,
            resynced: VecDeque::new(),
            repairing_since: None,
        }
    }

    pub fn settings(&self) -> &NoticeSettings {
        &self.settings
    }

    /// Notices told over the run, repeats included.
    pub fn told_total(&self) -> u64 {
        self.told_total
    }

    /// The next view follows a gap (render.md 3): the views between are lost, and with them perhaps a newer record of
    /// an entry held pending, so everything pending goes untold (@coordinator-opus's ruling). A refusal
    /// can be lost this way after a hitch of 250 ms or more; reading WorldApi's lossless `ChangeLog` fixes that after
    /// M1.
    pub fn gap(&mut self) {
        self.dropped_at_gaps += self.pending.len() as u64;
        self.pending.clear();
    }

    /// Refusals dropped untold at gaps, over the run.
    pub fn dropped_at_gaps(&self) -> u64 {
        self.dropped_at_gaps
    }

    /// A member's display name, from pe-app's roster.
    pub fn set_name(&mut self, member: MemberKey, name: impl Into<String>) {
        self.names.insert(member, name.into());
    }

    /// Read a new frame view's `changes` (and `final_frame`) at host time `now`.
    pub fn observe(&mut self, view: &FrameView, now: HostTime) {
        self.repairing_since = match view.health {
            Health::Repairing { .. } => Some(self.repairing_since.unwrap_or(now)),
            Health::Agreed => None,
        };
        let mut unknown: Vec<CutRef> = Vec::new();
        for rec in &view.changes {
            self.authors.push_back((now, rec.entry, rec.author));
            if rec.author != self.local {
                continue;
            }
            if self.own_class.insert(rec.entry, rec.class).is_none() {
                self.own_order.push_back(rec.entry);
                if self.own_order.len() > TOLD_KEPT {
                    let old = self.own_order.pop_front().expect("non-empty");
                    self.own_class.remove(&old);
                }
            }
            // S5 v1.5: a void decided by a final cut, and a verdict a repair couldn't know (Input-class only, like the
            // refusals).
            if rec.class == Class::Input {
                match &rec.outcome {
                    Outcome::Dropped { reason } => {
                        self.void(rec.entry, *reason, now);
                        continue;
                    }
                    Outcome::Unknown { repaired_at } => {
                        self.pending.remove(&rec.entry);
                        // Keep a void or refusal already told: a later void must still retract the refusal, and
                        // must not be told twice.
                        if !self.told.contains_key(&rec.entry) {
                            self.remember(rec.entry, Told::Silent);
                        }
                        unknown.push(*repaired_at);
                        continue;
                    }
                    _ => {}
                }
            }
            if self.told.contains_key(&rec.entry) {
                continue;
            }
            // A re-stepped entry appears again; the newest verdict wins.
            match &rec.outcome {
                Outcome::Refused { .. } if rec.class == Class::Input => {
                    self.pending.insert(rec.entry, rec.clone());
                }
                _ => {
                    self.pending.remove(&rec.entry);
                }
            }
        }
        for cut in unknown {
            if !self.resynced.contains(&cut) {
                self.resynced.push_back(cut);
                if self.resynced.len() > 16 {
                    self.resynced.pop_front();
                }
                self.tell("Resynced. Some of your last actions may not have happened.".to_string(), None, now);
            }
        }
        while self.authors.front().is_some_and(|a| now.secs_since(a.0) > self.settings.authors_s) {
            self.authors.pop_front();
        }
        let tell_at = self.settings.tell_at;
        let mut due: Vec<StepRecord> = Vec::new();
        self.pending.retain(|_, r| {
            let now_due = match tell_at {
                TellAt::AtOnce => true,
                // A cut at `f` covers the entries stamped below `f` (S3): one stamped at `f` can still change.
                TellAt::Final => view.final_frame.is_some_and(|f| r.frame < f),
            };
            if now_due {
                due.push(r.clone());
            }
            !now_due
        });
        due.sort_by_key(|r| (r.frame, r.entry.0.0));
        for rec in due {
            match self.refusal_text(&rec, view) {
                Some(text) => {
                    self.remember(rec.entry, Told::Refusal);
                    let cooldown_end = self.cooldown_end(&rec, view, now);
                    self.tell_line(text, cooldown_end, Some(rec.entry), now);
                }
                None => self.remember(rec.entry, Told::Silent),
            }
        }
    }

    /// An agreement event from pe-app: a void of one of the local member's entries is told at once (a final cut has
    /// decided it). If that entry's refusal was already told, the void replaces it.
    pub fn agreement(&mut self, e: &AgreeEvent, now: HostTime) {
        let AgreeEvent::Voided { entry, author, reason, .. } = e else { return };
        if *author != self.local || self.own_class.get(entry) != Some(&Class::Input) {
            return;
        }
        self.void(*entry, *reason, now);
    }

    /// A void of one of the local member's Input entries, from S3 or as a `Dropped` record: told once, replacing a
    /// refusal of it already told.
    fn void(&mut self, entry: EntryRef, reason: VoidReason, now: HostTime) {
        if self.told.get(&entry) == Some(&Told::Void) {
            return;
        }
        self.pending.remove(&entry);
        if self.told.get(&entry) == Some(&Told::Refusal) {
            self.retract(&entry);
        }
        self.remember(entry, Told::Void);
        let text = match reason {
            VoidReason::Late => "An action of yours arrived too late and was undone.",
            VoidReason::BackStamped => "An action of yours was undone: it arrived out of order.",
            VoidReason::AfterLeave => "An action of yours came after you left and was dropped.",
            VoidReason::LastMember => "You are the session's last member, so your leave wasn't applied.",
            VoidReason::NotAuthorized => "Your invite wasn't applied: you may not admit players.",
            VoidReason::NoChange => "That player is already in the session.",
        };
        self.tell(text.to_string(), None, now);
    }

    /// The lines to draw at `now`, oldest first, after "Resyncing…" while a repair has lasted `resync_after_s`.
    pub fn lines(&mut self, now: HostTime) -> Vec<Line> {
        self.shown.retain(|s| s.until.0 > now.0);
        let fade = self.settings.fade_s;
        let resyncing = self.repairing_since.filter(|t| now.secs_since(*t) >= self.settings.resync_after_s);
        let status = resyncing.map(|_| Line { text: "Resyncing…".to_string(), alpha: 1.0 });
        status
            .into_iter()
            .chain(self.shown.iter().map(|s| {
                let left = s.until.secs_since(now);
                let alpha = if fade > 0.0 { (left / fade).clamp(0.0, 1.0) } else { 1.0 };
                let text = match s.cooldown_end {
                    Some(end) => cooldown_text(end.secs_since(now)),
                    None => s.text.clone(),
                };
                let text = if s.count > 1 { format!("{text} (×{})", s.count) } else { text };
                Line { text, alpha: alpha as f32 }
            }))
            .collect()
    }

    fn remember(&mut self, e: EntryRef, how: Told) {
        if self.told.insert(e, how).is_none() {
            self.told_order.push_back(e);
            if self.told_order.len() > TOLD_KEPT {
                let old = self.told_order.pop_front().expect("non-empty");
                self.told.remove(&old);
            }
        }
    }

    fn tell(&mut self, text: String, entry: Option<EntryRef>, now: HostTime) {
        self.tell_line(text, None, entry, now);
    }

    /// Show a line, or refresh the one showing the same text (any cooldown line, for a cooldown) and count it up.
    fn tell_line(&mut self, text: String, cooldown_end: Option<HostTime>, entry: Option<EntryRef>, now: HostTime) {
        let until = now.plus_nanos((self.settings.life_s * 1e9) as i64);
        self.told_total += 1;
        self.shown.retain(|s| s.until.0 > now.0);
        let same = |s: &Shown| match (s.cooldown_end, cooldown_end) {
            (Some(_), Some(_)) => true,
            (None, None) => s.text == text,
            _ => false,
        };
        if let Some(i) = self.shown.iter().position(same) {
            let mut s = self.shown.remove(i).expect("found");
            s.count += 1;
            s.until = until;
            s.cooldown_end = cooldown_end;
            s.text = text;
            s.entries.extend(entry);
            self.shown.push_back(s);
            return;
        }
        self.shown.push_back(Shown { text, cooldown_end, entries: entry.into_iter().collect(), count: 1, until });
        while self.shown.len() > self.settings.max_lines.max(1) {
            self.shown.pop_front();
        }
    }

    /// Take a told refusal back off its line: the line's count drops, and the line goes at zero.
    fn retract(&mut self, e: &EntryRef) {
        let Some(i) = self.shown.iter().position(|s| s.entries.contains(e)) else { return };
        let s = &mut self.shown[i];
        s.entries.retain(|x| x != e);
        s.count = s.count.saturating_sub(1);
        if s.count == 0 {
            self.shown.remove(i);
        }
        self.told_total = self.told_total.saturating_sub(1);
    }

    /// A member's name, starting a sentence.
    fn name(&self, m: &MemberKey) -> String {
        if *m == self.local {
            return "You".to_string();
        }
        self.names.get(m).cloned().unwrap_or_else(|| "Another player".to_string())
    }

    /// The author of a recently stepped entry, starting a sentence.
    fn author_of(&self, e: &EntryRef) -> String {
        match self.authors.iter().rev().find(|a| a.1 == *e) {
            Some((_, _, m)) => self.name(m),
            None => "Someone".to_string(),
        }
    }

    /// A cooldown's end in host time: its frames left past the telling view's frame, from `now`.
    fn cooldown_end(&self, rec: &StepRecord, view: &FrameView, now: HostTime) -> Option<HostTime> {
        let Outcome::Refused { reason: Reason::Cooldown { until }, .. } = &rec.outcome else { return None };
        let frames = until.0.saturating_sub(view.frame.0);
        Some(now.plus_nanos((frames as f64 / self.sim_rate_hz as f64 * 1e9) as i64))
    }

    /// The words for a refusal, or `None` when it needs none: a grab lost to a hand in view.
    fn refusal_text(&self, rec: &StepRecord, view: &FrameView) -> Option<String> {
        let Outcome::Refused { reason, .. } = &rec.outcome else { return None };
        let what = match rec.class {
            Class::Input => "action",
            Class::Edit => "edit",
            Class::Governance | Class::Protocol => "change",
        };
        Some(match reason {
            Reason::Held { by } => {
                let in_view = *by == self.local || view.objects.iter().any(|o| o.hand_of.is_some_and(|h| h.0 == *by));
                if in_view && !self.settings.lost_grabs {
                    return None;
                }
                format!("{} is holding it.", self.name(by))
            }
            Reason::Stale { winner, .. } => {
                format!("Your {what} was refused: {} changed it first.", inside(&self.author_of(winner)))
            }
            Reason::Deleted { by, .. } => {
                format!("Your {what} was refused: {} deleted it.", inside(&self.author_of(by)))
            }
            Reason::Protected { owner } => format!("Refused: {} protects it.", inside(&self.name(owner))),
            Reason::NoPermission => "Refused: you don't have permission for that.".to_string(),
            // Counted from the frame of the view that tells it, not the frame the grab was stepped at; with the
            // finality delay it may already have ended (review L2).
            // `lines` redraws it as the cooldown runs down (see `cooldown_end`).
            Reason::Cooldown { until } => {
                cooldown_text(until.0.saturating_sub(view.frame.0) as f64 / self.sim_rate_hz as f64)
            }
            Reason::TooFast { .. } => "Your push was refused: too fast.".to_string(),
            Reason::OutOfReach => "Out of reach.".to_string(),
            Reason::TooManyClaims { limit } => format!("You can hold {limit} things at once."),
            other => match plain_reason(other) {
                Some(why) => format!("Your {what} was refused: {why}."),
                None => format!("Your {what} was refused."),
            },
        })
    }
}

/// A cooldown with `left` seconds to go, counted up to whole seconds: "now" once it has ended.
fn cooldown_text(left: f64) -> String {
    if left <= 0.0 {
        "You can grab it again now.".to_string()
    } else {
        format!("You can grab it again in {} s.", left.ceil() as u64)
    }
}

/// A name that started a sentence, placed inside one: "You", "Someone" and "Another player" go lower case; a
/// member's own name is kept.
fn inside(s: &str) -> String {
    match s {
        "You" | "Someone" | "Another player" => s.to_lowercase(),
        other => other.to_string(),
    }
}

/// The remaining reasons in plain words; `None` for one this version can't name.
fn plain_reason(r: &Reason) -> Option<&'static str> {
    Some(match r {
        Reason::Malformed | Reason::NewIndexOverlap => "it was malformed",
        Reason::Unknown(_) => "it no longer exists",
        Reason::Exists(_) => "it already exists",
        Reason::Invalid { .. } => "a value wasn't allowed",
        Reason::OutOfRange => "too far from the world's centre",
        Reason::Cycle => "it would put something inside itself",
        Reason::TooDeep { .. } => "it would nest things too deep",
        Reason::TooLarge { .. } => "too many new things at once",
        Reason::NotYourHand => "that isn't your hand",
        _ => return None,
    })
}

// ---- the strip: the lines as an image ------------------------------------------------------------------------------

/// The box behind each line: black at this opacity (times the line's fade).
const STRIP_BOX: f32 = 0.6;

/// The lines as one image: each line centred on its own black box (60 % opaque), white text, the whole faded by
/// each line's alpha; transparent elsewhere. `px` is the text height in pixels. `None` with no lines.
pub fn strip(lines: &[Line], px: f32) -> Option<OverlayImage> {
    if lines.is_empty() {
        return None;
    }
    let f = font().as_scaled(PxScale::from(px));
    let pad = (px * 0.4).ceil();
    let line_h = (f.ascent() - f.descent()).ceil() + 2.0 * pad;
    let widths: Vec<f32> = lines.iter().map(|l| text_width(&l.text, px).ceil() + 2.0 * pad).collect();
    let w = widths.iter().cloned().fold(0.0, f32::max) as usize;
    let h = (line_h * lines.len() as f32) as usize;
    let mut box_a = vec![0f32; w * h];
    let mut text_a = vec![0f32; w * h];
    for (i, (l, lw)) in lines.iter().zip(&widths).enumerate() {
        let top = i as f32 * line_h;
        let left = ((w as f32 - lw) / 2.0).floor();
        for y in top as usize..(top + line_h) as usize {
            for x in left as usize..(left + lw) as usize {
                box_a[y * w + x] = STRIP_BOX * l.alpha;
            }
        }
        let mut pen = point(left + pad, top + pad + f.ascent());
        let mut prev = None;
        for c in l.text.chars() {
            let id = f.glyph_id(c);
            if let Some(p) = prev {
                pen.x += f.kern(p, id);
            }
            let g = id.with_scale_and_position(px, pen);
            pen.x += f.h_advance(id);
            prev = Some(id);
            if let Some(o) = f.outline_glyph(g) {
                let b = o.px_bounds();
                o.draw(|gx, gy, cov| {
                    let (x, y) = (b.min.x as i64 + gx as i64, b.min.y as i64 + gy as i64);
                    if x >= 0 && y >= 0 && (x as usize) < w && (y as usize) < h {
                        let k = y as usize * w + x as usize;
                        text_a[k] = text_a[k].max(cov.clamp(0.0, 1.0) * l.alpha);
                    }
                });
            }
        }
    }
    let mut rgba = vec![0u8; w * h * 4];
    for k in 0..w * h {
        // White text over the black box, straight alpha: colour = text / total (linear), then sRGB.
        let (t, b) = (text_a[k], box_a[k]);
        let a = t + b * (1.0 - t);
        let c = if a > 0.0 { srgb_byte(t / a) } else { 0 };
        rgba[k * 4..k * 4 + 4].copy_from_slice(&[c, c, c, (a * 255.0).round() as u8]);
    }
    Some(OverlayImage { width: w as u32, height: h as u32, rgba })
}

fn srgb_byte(linear: f32) -> u8 {
    let c = linear.clamp(0.0, 1.0);
    let s = if c <= 0.003_130_8 { c * 12.92 } else { 1.055 * c.powf(1.0 / 2.4) - 0.055 };
    (s * 255.0).round() as u8
}

// ---- where the strip goes -----------------------------------------------------------------------------------------

/// The window's text height: 1/30 of its height, at least 14 px.
pub fn window_text_px(window_height: u32) -> f32 {
    (window_height as f32 / 30.0).max(14.0)
}

/// The strip in the window: centred at the bottom, at its pixel size on screen, 3 % of the height above the bottom
/// edge, its corners on whole pixels so the text stays sharp (review nit). Placed in front of `cam` (ten times its
/// near plane, so it is never clipped), facing it.
pub fn window_overlay(cam: &Camera, window: [u32; 2], image: Arc<OverlayImage>) -> Overlay {
    let d = cam.near * 10.0;
    let [tl, tr, tu, td] = [cam.fov.left, cam.fov.right, cam.fov.up, cam.fov.down].map(|a| (a as f64).tan());
    let (ww, wh) = (window[0].max(1) as f64, window[1].max(1) as f64);
    let (w, h) = (image.width as f64, image.height as f64);
    // The strip's pixel rectangle: left and bottom edges on whole pixels.
    let x0 = ((ww - w) / 2.0).floor();
    let y1 = wh - (0.03 * wh).round();
    let (cx, cy) = (x0 + w / 2.0, y1 - h / 2.0);
    // Pixels to the view plane at distance d, per axis.
    let (per_x, per_y) = (d * (tr - tl) / ww, d * (tu - td) / wh);
    let centre = DVec3::new(d * tl + cx * per_x, d * tu - cy * per_y, -d);
    let size = [(w * per_x) as f32, (h * per_y) as f32];
    Overlay { image, pose: cam.pose.moved(cam.pose.rot * centre), size }
}

/// The strip's image, rebuilt only when the lines change. A fading line's alpha is rounded to 1/16, so a fade
/// rebuilds the image 16 times, not every display frame.
#[derive(Default)]
pub struct StripCache {
    key: Option<(Vec<Line>, u32)>,
    image: Option<Arc<OverlayImage>>,
}

impl StripCache {
    /// The image for `lines` at `px`, or `None` with no lines.
    pub fn get(&mut self, lines: &[Line], px: f32) -> Option<Arc<OverlayImage>> {
        if lines.is_empty() {
            self.key = None;
            self.image = None;
            return None;
        }
        let rounded: Vec<Line> =
            lines.iter().map(|l| Line { text: l.text.clone(), alpha: (l.alpha * 16.0).ceil() / 16.0 }).collect();
        let key = (rounded, px.to_bits());
        if self.key.as_ref() != Some(&key) {
            self.image = strip(&key.0, px).map(Arc::new);
            self.key = Some(key);
        }
        self.image.clone()
    }
}

/// Where a notice sits in the headset (render.md R11).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PanelMode {
    /// Locked to the head: always `distance` ahead and `below` under the head's forward direction.
    #[default]
    HeadLocked,
    /// Fixed in the world; it glides back to the head-locked place once the head has turned more than
    /// `follow_deg` away from it.
    LazyFollow,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PanelSettings {
    pub mode: PanelMode,
    /// Metres ahead of the head, in the rig frame (times the rig's scale in the world).
    pub distance: f64,
    /// Degrees below the head's forward direction.
    pub below_deg: f64,
    /// The text's height in metres at `distance` (rig frame): 3 cm at 1 m is about 1.7°.
    pub text_height: f64,
    /// The strip's text size in pixels when rasterized for the headset.
    pub px: f32,
    /// Lazy follow: how far the head may turn from the panel before it moves, degrees.
    pub follow_deg: f64,
    /// Lazy follow: the glide's time constant, seconds.
    pub follow_s: f64,
}

impl Default for PanelSettings {
    fn default() -> Self {
        PanelSettings {
            mode: PanelMode::HeadLocked,
            distance: 1.0,
            below_deg: 20.0,
            text_height: 0.03,
            px: 32.0,
            follow_deg: 25.0,
            follow_s: 0.3,
        }
    }
}

/// The head between two eyes: the eyes' midpoint, their mean orientation.
pub fn head_of(eyes: &[Camera; 2]) -> WorldPose {
    let half = offset_vec(eyes[1].pose.pos - eyes[0].pose.pos) * 0.5;
    WorldPose { pos: eyes[0].pose.moved(half).pos, rot: eyes[0].pose.rot.slerp(eyes[1].pose.rot, 0.5) }
}

/// The headset's notice panel. The mode can change at any time (a switch in the headset, render.md R11).
#[derive(Clone, Debug, Default)]
pub struct Panel {
    pub settings: PanelSettings,
    placed: Option<WorldPose>,
    gliding: bool,
    last: Option<HostTime>,
}

impl Panel {
    pub fn new(settings: PanelSettings) -> Panel {
        Panel { settings, ..Panel::default() }
    }

    /// Switch between head-locked and lazy follow; a lazy panel starts where the head-locked one is now.
    pub fn set_mode(&mut self, mode: PanelMode) {
        self.settings.mode = mode;
        self.placed = None;
        self.gliding = false;
    }

    /// The head-locked place for `head`, in a rig of scale `scale`.
    pub fn head_locked(&self, head: &WorldPose, scale: f64) -> WorldPose {
        let rot = head.rot * DQuat::from_rotation_x(-self.settings.below_deg.to_radians());
        WorldPose { pos: head.moved(rot * DVec3::new(0.0, 0.0, -self.settings.distance * scale)).pos, rot }
    }

    /// The strip's overlay this display frame at `now`, for `head` in a rig of scale `scale`.
    pub fn place(&mut self, head: &WorldPose, scale: f64, image: Arc<OverlayImage>, now: HostTime) -> Overlay {
        let target = self.head_locked(head, scale);
        let dt = self.last.map_or(0.0, |l| now.secs_since(l).max(0.0));
        self.last = Some(now);
        let pose = match self.settings.mode {
            PanelMode::HeadLocked => target,
            PanelMode::LazyFollow => {
                let cur = *self.placed.get_or_insert(target);
                let dir = |p: &WorldPose| offset_vec(p.pos - head.pos).normalize_or_zero();
                let off = dir(&cur).angle_between(dir(&target)).to_degrees();
                if off > self.settings.follow_deg {
                    self.gliding = true;
                }
                let next = if self.gliding {
                    let k = if self.settings.follow_s > 0.0 { 1.0 - (-dt / self.settings.follow_s).exp() } else { 1.0 };
                    let pos = cur.moved(offset_vec(target.pos - cur.pos) * k).pos;
                    let p = WorldPose { pos, rot: cur.rot.slerp(target.rot, k) };
                    if dir(&p).angle_between(dir(&target)).to_degrees() < 1.0 {
                        self.gliding = false;
                    }
                    p
                } else {
                    cur
                };
                self.placed = Some(next);
                next
            }
        };
        let per_px = self.settings.text_height * scale / self.settings.px.max(1.0) as f64;
        let size = [image.width as f64 * per_px, image.height as f64 * per_px].map(|v| v as f32);
        Overlay { image, pose, size }
    }
}

#[cfg(test)]
mod tests {
    use pe_seams::{
        CutRef, DomainId, Frame, Hand, Hash, LineId, ObjectClass, ObjectId, Pos, RigTransform, Version, ViewCounters,
        ViewFlags, ViewObject,
    };

    use super::*;
    use crate::camera::{DepthRange, Fov};

    const ME: MemberKey = MemberKey([1; 32]);
    const BOB: MemberKey = MemberKey([2; 32]);

    fn entry(n: u8) -> EntryRef {
        EntryRef(pe_seams::Hash([n; 32]))
    }

    fn t(ms: i64) -> HostTime {
        HostTime::from_millis(ms)
    }

    fn rec(n: u8, author: MemberKey, class: Class, frame: u64, outcome: Outcome) -> StepRecord {
        StepRecord { entry: entry(n), author, class, frame: Frame(frame), outcome }
    }

    fn input(n: u8, frame: u64, reason: Reason) -> StepRecord {
        rec(n, ME, Class::Input, frame, refused(reason))
    }

    fn refused(reason: Reason) -> Outcome {
        Outcome::Refused { op: None, reason }
    }

    fn applied() -> Outcome {
        Outcome::Applied { created: vec![], overwrote: vec![], notes: vec![], inverse: None }
    }

    fn view(frame: u64, final_frame: Option<u64>, changes: Vec<StepRecord>) -> FrameView {
        FrameView {
            frame: Frame(frame),
            domain: DomainId::default(),
            sim_time: HostTime(0),
            final_frame: final_frame.map(Frame),
            health: pe_seams::Health::Agreed,
            local_rig: RigTransform { pos: Pos::ORIGIN, rotation: [0.0, 0.0, 0.0, 1.0], scale: 1.0 },
            objects: vec![],
            effects: vec![],
            retracted: vec![],
            counters: ViewCounters::default(),
            changes,
            activations: Vec::new(),
            activations_retracted: Vec::new(),
        }
    }

    /// A view that also shows `member`'s right hand.
    fn with_hand_of(mut v: FrameView, member: MemberKey) -> FrameView {
        v.objects.push(ViewObject {
            id: ObjectId::from_u128(77),
            class: ObjectClass::World,
            pos: Pos::ORIGIN,
            rotation: [0.0, 0.0, 0.0, 1.0],
            scale: [1.0; 3],
            linear_velocity: [0.0; 3],
            angular_velocity: [0.0; 3],
            shape: None,
            appearance: None,
            light: None,
            radius: 0.09,
            hand_of: Some((member, Hand::Right)),
            held_by: None,
            flags: ViewFlags::default(),
            version: None,
        });
        v
    }

    fn void(e: u8, author: MemberKey, reason: VoidReason) -> AgreeEvent {
        AgreeEvent::Voided {
            entry: entry(e),
            author,
            reason,
            cut: CutRef { line: LineId(Hash([0; 32])), height: 1, frame: Frame(5) },
        }
    }

    fn texts(n: &mut Notices, now: HostTime) -> Vec<String> {
        n.lines(now).into_iter().map(|l| l.text).collect()
    }

    fn notices() -> Notices {
        let mut n = Notices::new(NoticeSettings::default(), ME, 60);
        n.set_name(BOB, "Bob");
        n
    }

    #[test]
    fn a_refusal_is_told_once_final_and_a_reversed_one_never() {
        let mut n = notices();
        // Predicted: out of reach at frame 10. Not final yet: nothing told.
        n.observe(&view(12, Some(5), vec![input(1, 10, Reason::OutOfReach)]), t(0));
        assert!(texts(&mut n, t(0)).is_empty());
        // A replay re-steps it: applied after all. Then frame 10 goes final: still nothing.
        n.observe(&view(14, Some(8), vec![rec(1, ME, Class::Input, 10, applied())]), t(30));
        n.observe(&view(16, Some(12), vec![]), t(60));
        assert!(texts(&mut n, t(60)).is_empty());
        // Another, stamped at frame 20: told when a cut covers it. A cut at 20 covers the entries below 20 (S3), so
        // it waits for the next one.
        n.observe(&view(22, Some(15), vec![input(2, 20, Reason::OutOfReach)]), t(100));
        assert!(texts(&mut n, t(100)).is_empty());
        n.observe(&view(26, Some(20), vec![]), t(200));
        assert!(texts(&mut n, t(200)).is_empty(), "final_frame 20 doesn't cover frame 20");
        n.observe(&view(26, Some(21), vec![]), t(205));
        assert_eq!(texts(&mut n, t(205)), ["Out of reach."]);
        // Re-stepped after being told: never told again.
        n.observe(&view(27, Some(21), vec![input(2, 20, Reason::OutOfReach)]), t(210));
        assert_eq!(n.told_total(), 1);
    }

    #[test]
    fn at_once_tells_on_the_predicted_line() {
        let mut n = Notices::new(NoticeSettings { tell_at: TellAt::AtOnce, ..NoticeSettings::default() }, ME, 60);
        n.observe(&view(12, None, vec![input(1, 10, Reason::OutOfReach)]), t(0));
        assert_eq!(texts(&mut n, t(0)), ["Out of reach."]);
    }

    /// A grab lost to a hand in view is the cue's alone; held by a member with no hand in view, it is told.
    #[test]
    fn a_grab_lost_to_a_visible_hand_is_not_told_in_words() {
        let mut n = notices();
        n.observe(&with_hand_of(view(30, Some(30), vec![input(1, 10, Reason::Held { by: BOB })]), BOB), t(0));
        assert!(texts(&mut n, t(0)).is_empty());
        n.observe(&view(31, Some(31), vec![input(2, 11, Reason::Held { by: BOB })]), t(10));
        assert_eq!(texts(&mut n, t(10)), ["Bob is holding it."]);
        let mut all = Notices::new(NoticeSettings { lost_grabs: true, ..NoticeSettings::default() }, ME, 60);
        all.observe(&with_hand_of(view(30, Some(30), vec![input(1, 10, Reason::Held { by: BOB })]), BOB), t(0));
        assert_eq!(texts(&mut all, t(0)), ["Another player is holding it."]);
    }

    /// M1: only the local member's Input-class entries (coordinator).
    #[test]
    fn only_my_input_entries_are_told() {
        let mut n = notices();
        let stale = Reason::Stale { precondition: 0, current: Version { entry: entry(9), op: 0 }, winner: entry(9) };
        n.observe(
            &view(
                30,
                Some(30),
                vec![
                    rec(1, BOB, Class::Input, 10, refused(Reason::OutOfReach)),
                    rec(2, ME, Class::Edit, 11, refused(stale)),
                    rec(3, ME, Class::Governance, 12, refused(Reason::NoPermission)),
                    input(4, 13, Reason::TooFast { limit: 5000 }),
                ],
            ),
            t(0),
        );
        assert_eq!(texts(&mut n, t(0)), ["Your push was refused: too fast."]);
        // A void of my edit (an agent's, as far as this player knows) is not told either.
        n.agreement(&void(2, ME, VoidReason::Late), t(10));
        assert_eq!(n.told_total(), 1);
    }

    #[test]
    fn a_deleted_object_names_the_deleting_entry_s_author() {
        let mut n = notices();
        n.observe(&view(10, Some(9), vec![rec(7, BOB, Class::Edit, 9, applied())]), t(0));
        n.observe(
            &view(12, Some(12), vec![input(8, 11, Reason::Deleted { object: ObjectId::NONE, by: entry(7) })]),
            t(100),
        );
        // The author is forgotten after `authors_s`: "someone".
        n.observe(
            &view(400, Some(400), vec![input(9, 300, Reason::Deleted { object: ObjectId::NONE, by: entry(7) })]),
            t(5000),
        );
        assert_eq!(
            texts(&mut n, t(5000)),
            ["Your action was refused: Bob deleted it.", "Your action was refused: someone deleted it."]
        );
    }

    #[test]
    fn a_void_is_told_at_once_and_repeats_count_up() {
        let mut n = notices();
        n.observe(&view(10, None, vec![input(1, 5, Reason::OutOfReach), rec(2, ME, Class::Input, 6, applied())]), t(0));
        n.agreement(&void(1, BOB, VoidReason::Late), t(0));
        assert!(texts(&mut n, t(0)).is_empty(), "someone else's void");
        n.agreement(&void(1, ME, VoidReason::Late), t(0));
        n.agreement(&void(1, ME, VoidReason::Late), t(10)); // the same entry again: told once
        n.agreement(&void(2, ME, VoidReason::Late), t(1000));
        assert_eq!(texts(&mut n, t(1000)), ["An action of yours arrived too late and was undone. (×2)"]);
        assert_eq!(n.told_total(), 2);
        // The void decided the first entry: its pending refusal is never told.
        n.observe(&view(12, Some(12), vec![]), t(1100));
        assert_eq!(n.told_total(), 2);
    }

    /// Review L1: a refusal told once final, then voided (the void arrived after the final view): one line, the
    /// void's.
    #[test]
    fn a_void_replaces_a_told_refusal_of_the_same_entry() {
        let mut n = notices();
        n.observe(&view(12, Some(12), vec![input(1, 10, Reason::OutOfReach)]), t(0));
        assert_eq!(texts(&mut n, t(0)), ["Out of reach."]);
        n.agreement(&void(1, ME, VoidReason::BackStamped), t(50));
        assert_eq!(texts(&mut n, t(50)), ["An action of yours was undone: it arrived out of order."]);
        assert_eq!(n.told_total(), 1);
    }

    #[test]
    fn lines_fade_expire_and_cap_at_three() {
        let mut n = notices();
        let reasons = [Reason::OutOfReach, Reason::TooFast { limit: 1 }, Reason::NotYourHand, Reason::Cycle];
        for (i, r) in reasons.into_iter().enumerate() {
            n.observe(&view(10, Some(10), vec![input(i as u8, 5, r)]), t(i as i64 * 100));
        }
        let l = n.lines(t(300));
        assert_eq!(l.len(), 3, "the oldest was pushed out");
        assert_eq!(l[0].text, "Your push was refused: too fast.");
        assert_eq!(l[2].text, "Your action was refused: it would put something inside itself.");
        // The first remaining line was told at 100 ms: full until 4.6 s, half at 4.85 s, gone at 5.1 s; the others
        // (told at 200 and 300 ms) go at 5.2 and 5.3 s.
        assert_eq!(n.lines(t(4600))[0].alpha, 1.0);
        assert!((n.lines(t(4850))[0].alpha - 0.5).abs() < 1e-6);
        assert_eq!(n.lines(t(5150)).len(), 2);
        assert!(n.lines(t(5300)).is_empty());
    }

    #[test]
    fn a_cooldown_counts_from_the_telling_view() {
        let mut n = notices();
        n.observe(&view(100, Some(100), vec![input(1, 90, Reason::Cooldown { until: Frame(161) })]), t(0));
        // One that ended while the refusal waited for finality (review L2).
        assert_eq!(texts(&mut n, t(0)), ["You can grab it again in 2 s."]);
        n.observe(&view(200, Some(200), vec![input(2, 150, Reason::Cooldown { until: Frame(180) })]), t(10));
        // It shares the line, which now says so.
        assert_eq!(texts(&mut n, t(10)), ["You can grab it again now. (×2)"]);
    }

    /// A held grab against a cooldown is one line, not a scroll, and its seconds count down (review Low on ab323229).
    #[test]
    fn cooldown_refusals_share_one_line_that_counts_down() {
        let mut n = notices();
        n.observe(&view(100, Some(100), vec![input(1, 90, Reason::Cooldown { until: Frame(220) })]), t(0));
        assert_eq!(texts(&mut n, t(0)), ["You can grab it again in 2 s."]);
        assert_eq!(texts(&mut n, t(1100)), ["You can grab it again in 1 s."], "the same line, counting down");
        n.observe(&view(130, Some(130), vec![input(2, 125, Reason::Cooldown { until: Frame(220) })]), t(500));
        n.observe(&view(160, Some(160), vec![input(3, 155, Reason::Cooldown { until: Frame(230) })]), t(1000));
        // The newest refusal sets the end: frame 230 is 70 frames past view 160's, so 1.167 s after 1.0 s.
        assert_eq!(texts(&mut n, t(1100)), ["You can grab it again in 2 s. (×3)"]);
        assert_eq!(texts(&mut n, t(2100)), ["You can grab it again in 1 s. (×3)"]);
        assert_eq!(texts(&mut n, t(2200)), ["You can grab it again now. (×3)"]);
        // Another refusal is its own line beside it.
        n.observe(&view(170, Some(170), vec![input(4, 165, Reason::OutOfReach)]), t(2300));
        assert_eq!(texts(&mut n, t(2300)), ["You can grab it again now. (×3)", "Out of reach."]);
    }

    #[test]
    fn the_strip_cache_rebuilds_only_when_the_lines_change() {
        let mut c = StripCache::default();
        let line = |alpha: f32| vec![Line { text: "Out of reach.".into(), alpha }];
        let a = c.get(&line(1.0), 20.0).expect("a line");
        assert!(Arc::ptr_eq(&a, &c.get(&line(1.0), 20.0).unwrap()), "same lines: the same image");
        // Within one sixteenth of a fade: the same image; past it, a new one.
        let b = c.get(&line(0.97), 20.0).unwrap();
        assert!(Arc::ptr_eq(&a, &b));
        assert!(!Arc::ptr_eq(&b, &c.get(&line(0.9), 20.0).unwrap()));
        assert!(c.get(&[], 20.0).is_none());
    }

    #[test]
    fn the_strip_is_text_on_boxes_and_clear_around_a_short_line() {
        let lines = [
            Line { text: "A much longer line of notice text".into(), alpha: 1.0 },
            Line { text: "Short".into(), alpha: 0.5 },
        ];
        let img = strip(&lines, 20.0).expect("two lines");
        let (w, h) = (img.width as usize, img.height as usize);
        assert!(w > 200 && h >= 40, "{w} x {h}");
        let px = |x: usize, y: usize| &img.rgba[(y * w + x) * 4..(y * w + x) * 4 + 4];
        // Beside the short line, outside its box: clear.
        assert_eq!(px(1, h - 2)[3], 0);
        // The long line's box corner: black at 60 %.
        assert_eq!(px(0, 0), [0, 0, 0, 153]);
        // White text somewhere in each line. The second is faded to half: its text (0.5) over its box (0.3) reads
        // 0.5 + 0.3 x 0.5 = 0.65.
        let top_max = (0..h / 2).flat_map(|y| (0..w).map(move |x| (x, y))).map(|(x, y)| px(x, y)[3]).max().unwrap();
        let bot_max = (h / 2..h).flat_map(|y| (0..w).map(move |x| (x, y))).map(|(x, y)| px(x, y)[3]).max().unwrap();
        assert!(top_max >= 250, "{top_max}");
        assert!((163..=169).contains(&bot_max), "{bot_max}");
        assert!(strip(&[], 20.0).is_none());
    }

    #[test]
    fn the_window_strip_sits_at_the_bottom_at_its_pixel_size() {
        let cam = Camera::placed(
            WorldPose { pos: Pos::ORIGIN, rot: DQuat::from_rotation_y(0.7) },
            Fov::symmetric(1.0, 16.0 / 9.0),
            DepthRange::DESKTOP,
        );
        let img = Arc::new(OverlayImage { width: 300, height: 40, rgba: vec![0; 300 * 40 * 4] });
        let o = window_overlay(&cam, [1280, 720], img);
        // Project its corners: 300 x 40 px, centred, its bottom 3 % of the height above the window's bottom.
        let proj = |dx: f64, dy: f64| {
            let p = o.pose.moved(o.pose.rot * DVec3::new(dx * o.size[0] as f64, dy * o.size[1] as f64, 0.0)).pos;
            let l = cam.relative(p);
            let c = cam.projection() * glam::Vec4::new(l.x as f32, l.y as f32, l.z as f32, 1.0);
            let n = c / c.w;
            ((n.x as f64 * 0.5 + 0.5) * 1280.0, (0.5 - n.y as f64 * 0.5) * 720.0)
        };
        let (x0, y1) = proj(-0.5, -0.5);
        let (x1, y0) = proj(0.5, 0.5);
        assert!((x1 - x0 - 300.0).abs() < 0.5 && (y1 - y0 - 40.0).abs() < 0.5, "{x0} {x1} {y0} {y1}");
        assert!(((x0 + x1) / 2.0 - 640.0).abs() < 0.5);
        assert!((720.0 - y1 - 0.03 * 720.0).abs() < 0.5, "bottom at {y1}");
    }

    #[test]
    fn the_head_locked_panel_is_ahead_and_below_and_lazy_follow_waits() {
        let img = Arc::new(OverlayImage { width: 320, height: 48, rgba: vec![0; 320 * 48 * 4] });
        let head = WorldPose {
            pos: Pos::ORIGIN.checked_add(pe_seams::Offset([0.0, 1.6, 0.0])).unwrap(),
            rot: DQuat::IDENTITY,
        };
        let mut p = Panel::new(PanelSettings::default());
        let o = p.place(&head, 1.0, img.clone(), t(0));
        let d = offset_vec(o.pose.pos - head.pos);
        assert!((d.length() - 1.0).abs() < 1e-9);
        assert!((d.y / d.length() + 20f64.to_radians().sin()).abs() < 1e-9, "20 degrees down: {d:?}");
        // 3 cm text at 32 px: 320 px wide is 0.3 m.
        assert!((o.size[0] - 0.3).abs() < 1e-6 && (o.size[1] - 0.045).abs() < 1e-6);

        // Lazy follow: a 15 degree turn leaves it; a 40 degree turn starts a glide that ends at the new place.
        p.set_mode(PanelMode::LazyFollow);
        let start = p.place(&head, 1.0, img.clone(), t(0)).pose;
        let turned = |deg: f64| WorldPose { pos: head.pos, rot: DQuat::from_rotation_y(deg.to_radians()) };
        assert_eq!(p.place(&turned(15.0), 1.0, img.clone(), t(100)).pose, start);
        let mut ms = 100;
        let mut last = start;
        for _ in 0..125 {
            ms += 16;
            last = p.place(&turned(40.0), 1.0, img.clone(), t(ms)).pose;
        }
        let want = p.head_locked(&turned(40.0), 1.0);
        assert!(offset_vec(last.pos - want.pos).length() < 0.02, "glided to the new place");
    }

    fn dropped(n: u8, frame: u64, reason: VoidReason) -> StepRecord {
        rec(n, ME, Class::Input, frame, Outcome::Dropped { reason })
    }

    fn cut(height: u64) -> CutRef {
        CutRef { line: pe_seams::LineId::default(), height, frame: Frame(height * 6) }
    }

    fn repairing(mut v: FrameView, since: u64) -> FrameView {
        v.health = Health::Repairing { since: Frame(since) };
        v
    }

    /// S5 v1.5: a `Dropped` record is a void, told at once and once, whether S3's `Voided` comes too or not; it
    /// replaces a refusal of the same entry already told, and drops one still pending.
    #[test]
    fn a_dropped_record_is_told_as_a_void_once() {
        let mut n = notices();
        n.observe(&view(12, Some(5), vec![input(1, 10, Reason::OutOfReach)]), t(0));
        n.observe(&view(13, Some(5), vec![dropped(1, 10, VoidReason::Late)]), t(10));
        assert_eq!(texts(&mut n, t(10)), ["An action of yours arrived too late and was undone."]);
        let void = AgreeEvent::Voided { entry: entry(1), author: ME, reason: VoidReason::Late, cut: cut(2) };
        n.agreement(&void, t(20));
        n.observe(&view(30, Some(30), vec![]), t(30));
        assert_eq!(n.told_total(), 1, "told once, never as refused");
        // Told as refused first, then dropped: the void replaces it.
        n.observe(&view(40, Some(40), vec![input(2, 35, Reason::OutOfReach)]), t(100));
        assert_eq!(texts(&mut n, t(100)).last().unwrap(), "Out of reach.");
        n.observe(&view(41, Some(41), vec![dropped(2, 35, VoidReason::BackStamped)]), t(110));
        let l = texts(&mut n, t(110));
        assert!(!l.iter().any(|x| x == "Out of reach."), "{l:?}");
        assert_eq!(l.last().unwrap(), "An action of yours was undone: it arrived out of order.");
        // Another member's dropped entry is theirs to hear about.
        n.observe(
            &view(42, Some(42), vec![rec(3, BOB, Class::Input, 41, Outcome::Dropped { reason: VoidReason::Late })]),
            t(120),
        );
        assert_eq!(n.told_total(), 2);
    }

    /// `Unknown` is told once per repair, not per entry, and only for the local member's own entries.
    #[test]
    fn unknown_verdicts_are_told_once_per_repair() {
        let mut n = notices();
        let unknown = |e: u8, f: u64, c: CutRef| rec(e, ME, Class::Input, f, Outcome::Unknown { repaired_at: c });
        n.observe(&view(50, Some(48), vec![unknown(1, 40, cut(8)), unknown(2, 41, cut(8))]), t(0));
        n.observe(&view(51, Some(48), vec![unknown(3, 42, cut(8))]), t(10));
        assert_eq!(texts(&mut n, t(10)), ["Resynced. Some of your last actions may not have happened."]);
        assert_eq!(n.told_total(), 1);
        let theirs = rec(4, BOB, Class::Input, 43, Outcome::Unknown { repaired_at: cut(9) });
        n.observe(&view(60, Some(58), vec![theirs]), t(20));
        assert_eq!(n.told_total(), 1, "another member's entry");
        n.observe(&view(70, Some(68), vec![unknown(5, 66, cut(11))]), t(30));
        assert_eq!(n.told_total(), 2, "a second repair");
        // A refusal of an entry later reported Unknown is never told.
        n.observe(&view(80, Some(70), vec![input(6, 75, Reason::OutOfReach)]), t(40));
        n.observe(&view(81, Some(70), vec![unknown(6, 75, cut(13))]), t(50));
        n.observe(&view(90, Some(90), vec![]), t(60));
        assert!(!texts(&mut n, t(60)).iter().any(|x| x == "Out of reach."));
    }

    /// Under `TellAt::AtOnce` a refusal told before an `Unknown` stays recorded as told: a later void still retracts
    /// it, and is told once (shader-reviewer's nit on e0598ab9).
    #[test]
    fn unknown_keeps_a_refusal_already_told() {
        let mut n = Notices::new(NoticeSettings { tell_at: TellAt::AtOnce, ..NoticeSettings::default() }, ME, 60);
        n.observe(&view(12, None, vec![input(1, 10, Reason::OutOfReach)]), t(0));
        assert_eq!(texts(&mut n, t(0)), ["Out of reach."]);
        let unknown = rec(1, ME, Class::Input, 10, Outcome::Unknown { repaired_at: cut(3) });
        n.observe(&view(20, Some(15), vec![unknown]), t(10));
        n.observe(&view(21, Some(15), vec![dropped(1, 10, VoidReason::Late)]), t(20));
        let l = texts(&mut n, t(20));
        assert!(!l.iter().any(|x| x == "Out of reach."), "the void retracts the refusal: {l:?}");
        assert_eq!(l.last().unwrap(), "An action of yours arrived too late and was undone.");
        let told = n.told_total();
        let void = AgreeEvent::Voided { entry: entry(1), author: ME, reason: VoidReason::Late, cut: cut(4) };
        n.agreement(&void, t(30));
        assert_eq!(n.told_total(), told, "the void is told once");
    }

    /// "Resyncing…" shows after the views have said `Repairing` for 1.5 s, above the other lines, until `Agreed`.
    #[test]
    fn resyncing_shows_while_a_repair_lasts() {
        let mut n = notices();
        n.observe(&repairing(view(100, Some(90), vec![]), 95), t(0));
        n.observe(&repairing(view(130, Some(90), vec![input(1, 120, Reason::OutOfReach)]), 95), t(1000));
        assert!(texts(&mut n, t(1000)).is_empty(), "under 1.5 s: nothing, and the refusal waits for finality");
        n.observe(&repairing(view(160, Some(90), vec![]), 95), t(1600));
        assert_eq!(texts(&mut n, t(1600)), ["Resyncing…"]);
        n.observe(&view(170, Some(168), vec![]), t(1800));
        assert_eq!(texts(&mut n, t(1800)), ["Out of reach."], "agreed again: the status goes, the refusal is told");
    }
}
