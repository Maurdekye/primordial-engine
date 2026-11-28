//! The friend test's menu (S3; @item:primordial-engine/friend-test-xr-eng-s-slices-vr-pointers-menu-and): open at
//! launch, toggled by a controller's menu button, the wrist button or the window's Tab key, and aimed at with a
//! pointer (xr.md 5.7) or the mouse.
//!
//! - **Option (D)** (@coordinator-opus): pe-app is always in a session. With no session flag it opens
//!   its own; New session and Join relaunch the process (`Relaunch`), at the very end of this one, after VR has shut
//!   down and the port is closed (Fable's condition 3).
//! - **The opener does everything in VR:** "Invite a friend" copies the invite and shows its time left; "Paste reply"
//!   reads a joiner's reply code and lets them in.
//! - **The joiner's code exchange is a desktop step**: its reply code is copied, saved to a file and
//!   shown on the console and the status line before VR starts.
//! - **Copy and paste** (ruling (b)): the clipboard, and every time a file beside the identity, with a
//!   console line; the menu says which worked.
//! - **The wrist button** sits on the back of the left wrist and is clicked with the right hand's
//!   pointer. It is the only menu toggle a tracked hand has.
//! - **Quit and Join ask first** (@coordinator-opus, after the friend test's attempts of 2026-11-26):
//!   the first press says what will happen, and only a second press of the same button within [`CONFIRM_S`] does it.
//!   Join's first press says that VR stays off until the opener lets this machine in, and that the reply code goes
//!   back from the desktop. Any other button, or closing the menu, takes the question back.
//! - **Every action is logged** (same ruling): each button pressed, the menu opened or closed, and what the button
//!   said, as `menu: …` lines in the run's log.
//! - **The Gnomon** (the user's pick D: "yes, D looks good, and those preliminary menu options
//!   work"; [`pe_render::gnomon`]): the menu is the dash, a sundial. Its dial holds the five sections on its hour
//!   lines, Leave · Quit, Settings, Sessions, My Domain and People, with the mic switch and Close at the ends of its
//!   diameter. The chosen section's page stands on the gnomon. Sessions opens first. Grabbing the gnomon's tip moves
//!   the dash, and it keeps that place relative to the player from then on. The spawn panel's test objects are on
//!   the My Domain page (the user renamed Inventory; @domain-eng owns what it holds). People and Settings say what they will hold, and the mic switch says voice isn't built yet
//!   [P: the placeholders' wording].

use glam::DVec3;
use pe_render::gnomon::{Arrangement, DialSpec, Glyph, Gnomon, Grip, Node, PageHit, Part, Place};
use pe_render::menu::{
    Button, MenuPanel, PanelSpec, Ui, UiInput, UiOutput, button_at, cursor_overlay, place_in_front, ray_overlay,
    wrist_place,
};

/// The dash's sections, on the dial's hour lines from the player's left to their right.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Section {
    Leave,
    Settings,
    Sessions,
    MyDomain,
    People,
}

impl Section {
    pub const ALL: [Section; 5] =
        [Section::Leave, Section::Settings, Section::Sessions, Section::MyDomain, Section::People];

    pub fn name(self) -> &'static str {
        match self {
            Section::Leave => "Leave · Quit",
            Section::Settings => "Settings",
            Section::Sessions => "Sessions",
            Section::MyDomain => "My Domain",
            Section::People => "People",
        }
    }

    /// Its node's id on the dial.
    pub fn node(self) -> u32 {
        id::SECTION + Section::ALL.iter().position(|s| *s == self).expect("listed") as u32
    }

    fn glyph(self) -> Glyph {
        match self {
            Section::Leave => Glyph::Exit,
            Section::Settings => Glyph::Settings,
            Section::Sessions => Glyph::Sessions,
            Section::MyDomain => Glyph::Inventory,
            Section::People => Glyph::People,
        }
    }
}

/// What this machine's session is, for the menu.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Role {
    /// Its own session: the invite, and its expiry (Unix seconds).
    Opener { invite: String, expires_unix_s: u64 },
    /// Joined, or joining: its reply code.
    Joiner { reply: String },
    /// No session (the world alone, as in tests).
    Alone,
}

/// What the menu asks the process to do after it ends.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Relaunch {
    /// A new session of its own.
    NewSession,
    /// Join this invite.
    Join(String),
}

/// How a copy went out (ruling (b)): to the clipboard and the file, or the file only.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Copied {
    Both(String),
    FileOnly(String),
}

/// What the menu's buttons act through: pe-app's, or a test's.
pub trait MenuHost: Send {
    /// Copy `text` out as `what` ("invite", "reply"): the clipboard, and always its file, with a console line.
    fn copy_out(&mut self, what: &str, text: &str) -> Result<Copied, String>;
    /// A code starting `prefix` pasted in (review L1): the first such word on the clipboard, else in the paste
    /// file's lines, newest first.
    fn paste_in(&mut self, prefix: &str) -> Result<String, String>;
    /// A joiner's reply code, to the opener's admission.
    fn submit_reply(&mut self, code: &str) -> Result<(), String>;
    /// Whether an invite can be joined: it decodes and hasn't expired (@engine-architect's L1 on S3). Join checks
    /// before it relaunches, since a relaunch that fails leaves the user with no session and no menu.
    fn check_invite(&self, code: &str) -> Result<(), String>;
    /// Leave this session for `r`: in-game, the session manager does it while the process runs (app.md 4.1);
    /// otherwise, or with `PE_MENU_RELAUNCH=1` (D7), end this run and start the process again after VR shut down.
    fn relaunch(&mut self, r: Relaunch);
    /// End the run.
    fn quit(&mut self);
    /// A line in the run's log (and on the console): each menu action.
    fn log(&mut self, line: &str);
    /// The wall clock, Unix seconds (an invite's time left).
    fn unix_now(&self) -> u64;
    /// Whether this world takes spawns (the plane's `/Spawned`; S4). No spawn panel without.
    fn can_spawn(&self) -> bool {
        false
    }
    /// Queue a spawn or a clear (crate::spawn): it is applied off the render thread.
    fn spawn(&mut self, _job: crate::spawn::Job) {}
    /// The last spawn's or clear's result, for the panel.
    fn spawn_status(&self) -> Option<String> {
        None
    }
    /// In-game sessions (app.md 4.1): what the session manager shows, `None` when a Join or New session restarts the
    /// process. The role in it replaces the menu's own.
    fn sessions(&self) -> Option<crate::session::View> {
        None
    }
    /// Cancel a swap under way (design d3, L2): `false` once a join said it is ready, or with no manager.
    fn cancel(&mut self) -> bool {
        false
    }
    /// The player read the last failure (D3): it leaves the menu.
    fn dismiss(&mut self) {}
    /// What the session says about a member's link (app.md 4.1): it dropped, or it closed and nobody can come back.
    fn link_note(&self) -> Option<crate::leave::LinkNote> {
        None
    }
}

/// Button ids.
pub mod id {
    pub const INVITE: u32 = 1;
    pub const PASTE_REPLY: u32 = 2;
    pub const JOIN: u32 = 3;
    pub const NEW_SESSION: u32 = 4;
    pub const LEAVE: u32 = 5;
    pub const COPY_REPLY: u32 = 6;
    pub const QUIT: u32 = 7;
    pub const CLOSE: u32 = 8;
    /// In-game sessions (app.md 4.1): stop the swap under way, and take the failure line down.
    pub const CANCEL: u32 = 9;
    pub const DISMISS: u32 = 10;
    /// The spawn panel's (S4): one per template, in `spawn::Kind::ALL`'s order, then Clear.
    pub const SPAWN: u32 = 20;
    pub const CLEAR: u32 = 30;
    /// The dial's: one per section, in `Section::ALL`'s order, then the mic switch (Close is [`CLOSE`]).
    pub const SECTION: u32 = 40;
    pub const MIC: u32 = 49;
    pub const WRIST: u32 = 100;
}

/// The menu: whether it is open, its panels, and its last message.
pub struct Menu {
    role: Role,
    host: Box<dyn MenuHost>,
    open: bool,
    /// The dash, placed in front of the head when the menu opens.
    dash: Option<Gnomon>,
    /// The open section.
    section: Section,
    /// Where the player last put the dash, from their head ([`Gnomon::rel_of`]); `None`: the arrangement's default.
    rel: Option<DVec3>,
    /// A pointer holding the gnomon's tip, and how far along its ray it took it.
    drag: Option<(usize, f64)>,
    /// The wrist button, placed on the left wrist each frame it is tracked.
    wrist: Option<MenuPanel>,
    /// The last thing a button did, shown on the panel: one line per `\n`.
    message: Option<String>,
    /// A press that waits for its second press (Quit, Join): the button, and the Unix second it lapses after.
    confirm: Option<(u32, u64)>,
    /// A relaunch or quit is under way: the buttons are off.
    leaving: bool,
    /// Where the head was when the main panel was placed.
    placed_at: Option<pe_seams::Pos>,
    /// The head this frame, for where a spawn appears.
    head: Option<pe_render::pose::WorldPose>,
    /// The hands' pointer origins this frame, as offsets from the origin: a spawn keeps clear of them.
    hands: Vec<[f64; 3]>,
    /// Per pointer, while its select is held: whether the press began on a panel (@ballpit-viz's M1 on S3). A press
    /// belongs to the menu, or not, from its start to its release, so a held grab sweeping across a panel isn't
    /// dropped, and a press dragged off a panel doesn't start one.
    latch: [Option<bool>; 3],
}

/// The wrist button's size against a main panel's (about 12 cm wide).
const WRIST_SCALE: f64 = 0.35;
/// A held tip moves only when its ray's point is this far from it, in metres at scale 1.
const DRAG_DEADBAND_M: f64 = 0.002;
/// A ray that meets nothing is drawn this long (metres at a rig scale of 1).
const RAY_LEN: f64 = 2.0;
/// How far the head may move from where it was when the main panel was placed before the panel follows (review L3).
const REPLACE_M: f64 = 1.0;
/// How long a first press of Quit or Join waits for the second, in seconds.
pub const CONFIRM_S: u64 = 10;
/// What Join's first press says (J0, @coordinator-opus, after the invite-only ruling):
/// the restart takes VR away until this machine is in the other session, a few seconds.
pub const JOIN_FIRST: &str = "Joining restarts the game: VR goes off,\n\
    usually for a few seconds, and comes back in their session.\n\
    To stop waiting, type quit or press Ctrl-C in the terminal.\n\
    Press Join again to go.";
/// What Join's first press says in-game (app.md 4.1): nothing restarts, and VR stays on.
pub const JOIN_FIRST_INGAME: &str = "Joining leaves this session: the world stands still\n\
    for a few seconds, then you're in theirs. VR stays on.\n\
    Press Join again to go.";
/// A button's name for the log.
fn button_name(b: u32) -> String {
    match b {
        id::INVITE => "Invite a friend".into(),
        id::PASTE_REPLY => "Paste reply".into(),
        id::JOIN => "Join a session".into(),
        id::NEW_SESSION => "New session".into(),
        id::LEAVE => "Leave".into(),
        id::COPY_REPLY => "Copy my reply code".into(),
        id::QUIT => "Quit".into(),
        id::CLOSE => "Close menu".into(),
        id::CANCEL => "Cancel".into(),
        id::DISMISS => "OK".into(),
        id::CLEAR => "Clear".into(),
        id::MIC => "Mic".into(),
        b if (id::SECTION..id::SECTION + Section::ALL.len() as u32).contains(&b) => {
            format!("section {}", Section::ALL[(b - id::SECTION) as usize].name())
        }
        b if (id::SPAWN..id::SPAWN + crate::spawn::Kind::ALL.len() as u32).contains(&b) => {
            crate::spawn::Kind::ALL[(b - id::SPAWN) as usize].template().into()
        }
        b => format!("button {b}"),
    }
}

/// An invite's and a reply code's prefixes (pe-net-crypto's encodings).
const INVITE_PREFIX: &str = "pe1:";
const REPLY_PREFIX: &str = "pe1r:";

impl Menu {
    pub fn new(role: Role, host: Box<dyn MenuHost>) -> Menu {
        Menu {
            role,
            host,
            open: true,
            dash: None,
            section: Section::Sessions,
            rel: None,
            drag: None,
            wrist: None,
            message: None,
            confirm: None,
            leaving: false,
            placed_at: None,
            head: None,
            hands: Vec::new(),
            latch: [None; 3],
        }
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    /// The session's role: the manager's while one runs the sessions (between two, the last one's), else the menu's.
    fn role(&self, view: Option<&crate::session::View>) -> Role {
        use crate::session::Role as S;
        match view.and_then(|v| v.role.as_ref()) {
            Some(S::Opener { invite, expires_unix_s, .. }) => {
                Role::Opener { invite: invite.clone(), expires_unix_s: *expires_unix_s }
            }
            Some(S::Joiner { .. }) => Role::Joiner { reply: String::new() },
            Some(S::Alone) => Role::Alone,
            None => self.role.clone(),
        }
    }

    /// The open section.
    pub fn section(&self) -> Section {
        self.section
    }

    /// The open section's page now.
    pub fn page(&self, now_unix: u64) -> PanelSpec {
        self.page_of(self.section, now_unix)
    }

    /// A section's page now: its title, its buttons, then its lines and the last message.
    pub fn page_of(&self, s: Section, now_unix: u64) -> PanelSpec {
        let mut spec = match s {
            Section::Sessions => return self.spec(now_unix),
            Section::Leave => {
                let view = self.host.sessions();
                let swapping = view.as_ref().is_some_and(|v| v.step.is_some());
                let mut b = Vec::new();
                if !swapping && matches!(self.role(view.as_ref()), Role::Joiner { .. }) {
                    b.push(Button::new(id::LEAVE, "Leave (back to my own session)"));
                }
                b.push(Button::new(id::QUIT, "Quit"));
                PanelSpec { title: None, buttons: b, lines: Vec::new() }
            }
            Section::MyDomain => self.side_spec(),
            Section::People => PanelSpec {
                title: None,
                buttons: Vec::new(),
                lines: vec!["Your contacts will be here:".into(), "friends, requests, confidants, blocked".into()],
            },
            Section::Settings => PanelSpec {
                title: None,
                buttons: Vec::new(),
                lines: vec!["Settings will be here: volume, voice,".into(), "comfort, controls, graphics".into()],
            },
        };
        spec.title = Some(s.name().into());
        self.confirming(&mut spec.buttons, now_unix);
        if self.leaving {
            spec.buttons.iter_mut().for_each(|x| x.enabled = false);
        }
        if let Some(m) = &self.message {
            spec.lines.extend(m.lines().map(String::from));
        }
        spec
    }

    /// A press waiting for its second says so on its button.
    fn confirming(&self, b: &mut [Button], now_unix: u64) {
        if let Some((c, until)) = self.confirm
            && now_unix <= until
            && let Some(x) = b.iter_mut().find(|x| x.id == c)
        {
            x.label = if c == id::QUIT { "Press again to quit".into() } else { "Press again to join".into() };
        }
    }

    /// The dial now.
    pub fn dial(&self) -> DialSpec {
        let places = [150.0, 120.0, 90.0, 60.0, 30.0];
        let mut nodes: Vec<Node> = Section::ALL
            .iter()
            .zip(places)
            .map(|(s, t)| Node { id: s.node(), label: s.name().into(), glyph: s.glyph(), place: Place::Hour(t) })
            .collect();
        nodes.push(Node { id: id::MIC, label: "Mic off".into(), glyph: Glyph::MicOff, place: Place::Left });
        nodes.push(Node { id: id::CLOSE, label: "Close".into(), glyph: Glyph::Close, place: Place::Right });
        DialSpec { nodes, selected: Some(self.section.node()) }
    }

    /// The Sessions page's content now.
    pub fn spec(&self, now_unix: u64) -> PanelSpec {
        let mut b = Vec::new();
        let mut lines = Vec::new();
        let view = self.host.sessions();
        // A swap under way (D2): its step and seconds, Cancel while it can be, Quit; nothing else to press.
        if let Some(step) = view.as_ref().and_then(|v| v.step.as_ref()) {
            lines.push(step.line(std::time::Instant::now()));
            let cancellable = matches!(
                step,
                crate::session::Step::Leaving { .. } | crate::session::Step::Joining { ready: false, .. }
            );
            if cancellable {
                b.push(Button::new(id::CANCEL, "Cancel"));
            }
            let asked = self.confirm.is_some_and(|(c, until)| c == id::QUIT && now_unix <= until);
            b.push(Button::new(id::QUIT, if asked { "Press again to quit" } else { "Quit" }));
            b.push(Button::new(id::CLOSE, "Close menu"));
            if self.leaving {
                for x in &mut b {
                    x.enabled = false;
                }
            }
            if let Some(m) = &self.message {
                lines.extend(m.lines().map(String::from));
            }
            return PanelSpec { title: Some(Section::Sessions.name().into()), buttons: b, lines };
        }
        // The last swap's failure stays until it is dismissed (D3).
        if let Some(f) = view.as_ref().and_then(|v| v.failure.as_ref()) {
            lines.extend(f.lines().map(String::from));
            b.push(Button::new(id::DISMISS, "OK"));
        }
        // D8: the port this session took instead, beside the invite.
        if let Some(crate::session::Role::Opener { warning: Some(w), .. }) = view.as_ref().and_then(|v| v.role.as_ref())
        {
            lines.push(w.clone());
        }
        match &self.role(view.as_ref()) {
            Role::Opener { expires_unix_s, .. } => {
                b.push(Button::new(id::INVITE, "Invite a friend"));
                b.push(Button::new(id::JOIN, "Join a session"));
                b.push(Button::new(id::NEW_SESSION, "New session"));
                let left = expires_unix_s.saturating_sub(now_unix);
                lines.push(if left == 0 {
                    "Your invite has expired: New session makes another".into()
                } else {
                    format!("Your session. Invite valid for {} min", left.div_ceil(60))
                });
            }
            Role::Joiner { .. } => {
                b.push(Button::new(id::LEAVE, "Leave (back to my own session)"));
                lines.push("In someone else's session".into());
            }
            Role::Alone => {
                b.push(Button::new(id::NEW_SESSION, "New session"));
                b.push(Button::new(id::JOIN, "Join a session"));
            }
        }
        // A member's link (@coordinator-opus): the note under the status. Once a link closed nobody can
        // come back into this session, so the opener's New session comes first, and its invite, which would be refused
        // now, and the invite's time left go (@xr-eng's Low).
        if let Some(note) = self.host.link_note() {
            if note.gone && matches!(self.role, Role::Opener { .. }) {
                b.retain(|x| x.id != id::INVITE);
                lines.clear();
                if let Some(i) = b.iter().position(|x| x.id == id::NEW_SESSION) {
                    let new = b.remove(i);
                    b.insert(0, new);
                }
            }
            lines.extend(note.lines);
        }
        self.confirming(&mut b, now_unix);
        if self.leaving {
            for x in &mut b {
                x.enabled = false;
            }
        }
        if let Some(m) = &self.message {
            lines.extend(m.lines().map(String::from));
        }
        PanelSpec { title: Some(Section::Sessions.name().into()), buttons: b, lines }
    }

    /// Press button `b`.
    pub fn press(&mut self, b: u32) {
        let now = self.host.unix_now();
        // The second press of a button that asked first; any other press takes the question back.
        let confirmed = self.confirm.take().is_some_and(|(c, until)| c == b && now <= until);
        let view = self.host.sessions();
        let ingame = view.is_some();
        let role = self.role(view.as_ref());
        let msg = match b {
            id::INVITE => match &role {
                Role::Opener { invite, expires_unix_s } if *expires_unix_s > now => {
                    let left = (expires_unix_s - now).div_ceil(60);
                    Some(match self.host.copy_out("invite", &invite.clone()) {
                        Ok(Copied::Both(p)) => format!("Invite copied ({left} min left); also saved to {p}"),
                        Ok(Copied::FileOnly(p)) => format!("Invite saved to {p} ({left} min left)"),
                        Err(e) => format!("Couldn't copy the invite: {e}"),
                    })
                }
                Role::Opener { .. } => Some("The invite has expired: New session makes another".into()),
                _ => None,
            },
            id::COPY_REPLY => match &role {
                Role::Joiner { reply } => Some(match self.host.copy_out("reply", &reply.clone()) {
                    Ok(Copied::Both(p)) => format!("Reply code copied; also saved to {p}"),
                    Ok(Copied::FileOnly(p)) => format!("Reply code saved to {p}"),
                    Err(e) => format!("Couldn't copy the reply code: {e}"),
                }),
                _ => None,
            },
            id::PASTE_REPLY => Some(match self.host.paste_in(REPLY_PREFIX) {
                Ok(code) if code.starts_with(REPLY_PREFIX) => match self.host.submit_reply(&code) {
                    Ok(()) => "Reply received: letting them in…".into(),
                    Err(e) => format!("That reply code was refused: {e}"),
                },
                Ok(_) => "No reply code to paste (it starts pe1r:)".into(),
                Err(e) => format!("No reply code to paste: {e}"),
            }),
            id::JOIN => Some(match self.host.paste_in(INVITE_PREFIX) {
                // Not the invite this machine just copied out (@api-eng's 2): that rejoins the session it leaves.
                Ok(code) if matches!(&role, Role::Opener { invite, .. } if *invite == code) => {
                    "That's your own invite: send it to your friend, and copy theirs to join them".into()
                }
                Ok(code) if code.starts_with(INVITE_PREFIX) => match self.host.check_invite(&code) {
                    Ok(()) if confirmed && ingame => {
                        // The manager's step replaces the message (D2).
                        self.host.relaunch(Relaunch::Join(code));
                        String::new()
                    }
                    Ok(()) if confirmed => {
                        self.leaving = true;
                        self.host.relaunch(Relaunch::Join(code));
                        "Joining: restarting…".into()
                    }
                    Ok(()) => {
                        self.confirm = Some((b, now + CONFIRM_S));
                        if ingame { JOIN_FIRST_INGAME.into() } else { JOIN_FIRST.into() }
                    }
                    Err(e) => format!("That invite can't be joined: {e}"),
                },
                Ok(_) => "Copy the invite you were sent first (it starts pe1:)".into(),
                Err(e) => format!("No invite to paste: {e}"),
            }),
            id::NEW_SESSION | id::LEAVE if ingame => {
                self.host.relaunch(Relaunch::NewSession);
                Some(String::new())
            }
            id::NEW_SESSION | id::LEAVE => {
                self.leaving = true;
                self.host.relaunch(Relaunch::NewSession);
                Some("Restarting in a new session of your own…".into())
            }
            id::CANCEL => Some(if self.host.cancel() {
                "Cancelled".into()
            } else {
                "They've let you in: this join can't be cancelled now".into()
            }),
            id::DISMISS => {
                self.host.dismiss();
                Some(String::new())
            }
            id::QUIT if confirmed => {
                self.leaving = true;
                self.host.quit();
                Some("Quitting…".into())
            }
            id::QUIT => {
                self.confirm = Some((b, now + CONFIRM_S));
                Some("Quit the game? Press Quit again to quit.".into())
            }
            id::CLOSE => {
                self.open = false;
                self.host.log("menu: closed");
                None
            }
            b if (id::SPAWN..id::SPAWN + crate::spawn::Kind::ALL.len() as u32).contains(&b) => {
                let k = crate::spawn::Kind::ALL[(b - id::SPAWN) as usize];
                if let Some(h) = self.head {
                    // The head's level forward, from the same helper that places the panel.
                    let f = place_in_front(&h, 1.0, 0.0, 1.0).pos - h.pos;
                    let at = crate::spawn::in_front((h.pos - pe_seams::Pos::ORIGIN).0, f.0, k, &self.hands);
                    self.host.spawn(crate::spawn::Job::Spawn(k, at));
                }
                None
            }
            id::CLEAR => {
                self.host.spawn(crate::spawn::Job::Clear);
                None
            }
            id::WRIST => {
                self.toggle();
                None
            }
            id::MIC => Some("Voice chat isn't built yet: this switch will mute you".into()),
            b if (id::SECTION..id::SECTION + Section::ALL.len() as u32).contains(&b) => {
                self.section = Section::ALL[(b - id::SECTION) as usize];
                self.message = None;
                None
            }
            _ => None,
        };
        if b != id::WRIST && b != id::CLOSE {
            let said = msg.as_deref().map(|m| format!(": {}", m.replace('\n', " "))).unwrap_or_default();
            self.host.log(&format!("menu: {}{said}", button_name(b)));
        }
        // An empty message clears the last one (the manager's lines say what is under way).
        match msg {
            Some(m) if m.is_empty() => self.message = None,
            Some(m) => self.message = Some(m),
            None => {}
        }
    }

    fn toggle(&mut self) {
        self.open = !self.open;
        self.host.log(if self.open { "menu: opened" } else { "menu: closed" });
        if self.open {
            self.dash = None; // placed again in front of the head
        } else {
            self.confirm = None;
            self.drag = None;
        }
    }

    /// The spawn panel: a button per template, Clear, and the last result.
    fn side_spec(&self) -> PanelSpec {
        let mut buttons: Vec<Button> = crate::spawn::Kind::ALL
            .iter()
            .enumerate()
            .map(|(i, k)| Button::new(id::SPAWN + i as u32, k.template()))
            .collect();
        buttons.push(Button::new(id::CLEAR, "Clear"));
        if self.leaving {
            buttons.iter_mut().for_each(|b| b.enabled = false);
        }
        if !self.host.can_spawn() {
            return PanelSpec {
                title: None,
                buttons: Vec::new(),
                lines: vec!["Nothing to take out in this domain".into()],
            };
        }
        PanelSpec { title: None, buttons, lines: self.host.spawn_status().into_iter().collect() }
    }

    /// The dash is placed again when the head is more than [`REPLACE_M`] from where it was at the placing (review
    /// L3): a first frame located before the head was tracked, or a walk away from it.
    fn stale_place(&self, head: pe_seams::Pos, scale: f64) -> bool {
        self.placed_at.is_some_and(|p| (head - p).length() > REPLACE_M * scale)
    }
}

impl Ui for Menu {
    fn frame(&mut self, input: &UiInput) -> UiOutput {
        if input.toggled {
            self.toggle();
        }
        let now = self.host.unix_now();
        let mut out = UiOutput::default();
        let head = input.head;
        let scale = input.scale;
        // The dash while open: placed where the player last put it, relative to their head, or by default in front.
        // The window draws no rays: it gets the flat arrangement.
        if self.open {
            let arrangement = if input.draw_rays { Arrangement::VR } else { Arrangement::WINDOW };
            if self.drag.is_none() && self.stale_place(head.pos, scale) {
                self.dash = None;
            }
            let (dial, page) = (self.dial(), self.page(now));
            match self.dash.as_mut() {
                Some(d) if d.arrangement == arrangement => {
                    d.dial = dial;
                    d.set_page(page);
                    if d.scale != scale {
                        let a = d.anchor;
                        d.set_anchor(a, scale);
                    }
                }
                _ => {
                    let rel = self.rel.unwrap_or_else(|| Gnomon::default_rel(&arrangement));
                    let anchor = Gnomon::anchor_at(&head, rel, scale);
                    self.dash = Some(Gnomon::new(dial, page, anchor, scale, arrangement));
                    self.placed_at = Some(head.pos);
                }
            }
        }
        self.head = Some(head);
        self.hands =
            input.rays.iter().filter(|r| r.pointer < 2).map(|r| (r.origin - pe_seams::Pos::ORIGIN).0).collect();
        // The wrist button follows the left hand; it is gone while that hand isn't tracked. The panel is kept while it
        // shows, so its image is made once, not every frame: a new image each frame was a new GPU texture each frame
        // (friend test 2026-11-26, the VR stall on NVIDIA 620.73 with the menu closed).
        match (input.wrist, self.wrist.as_mut()) {
            (Some(w), Some(p)) => {
                p.pose = wrist_place(&w, scale);
                p.scale = WRIST_SCALE * scale;
            }
            (Some(w), None) => {
                let spec = PanelSpec { title: None, buttons: vec![Button::new(id::WRIST, "Menu")], lines: Vec::new() };
                self.wrist = Some(MenuPanel::new(spec, wrist_place(&w, scale), WRIST_SCALE * scale));
            }
            (None, _) => self.wrist = None,
        }
        // The pointer holding the tip moves the dash first, so this frame's hits see it where it now is.
        if let Some((p, dist)) = self.drag {
            match input.rays.iter().find(|r| r.pointer == p) {
                Some(r) if r.pressed && self.open => {
                    // Aim jitter under 2 mm is ignored: a held trigger or pinch shakes the ray (@xr-eng).
                    if let Some(d) = self.dash.as_mut()
                        && (r.at(dist) - d.tip()).length() > DRAG_DEADBAND_M * scale
                    {
                        d.move_tip_to(r.at(dist), &head);
                    }
                }
                _ => {
                    self.drag = None;
                    if let Some(d) = &self.dash {
                        self.rel = Some(Gnomon::rel_of(&head, &d.anchor, scale));
                        self.placed_at = Some(head.pos);
                        self.host.log("menu: moved the dash");
                    }
                }
            }
        }
        // Each ray: the nearest part it meets, and its presses.
        let (mut dial_hover, mut page_hover, mut wrist_hover) = (None, None, None);
        let (mut dial_held, mut page_held, mut wrist_held) = (false, false, false);
        let mut grip = if self.drag.is_some() { Grip::Held } else { Grip::Plain };
        let mut presses = Vec::new();
        let mut hits = Vec::new();
        let mut seen = [false; 3];
        for r in &input.rays {
            /// What a ray meets: a part of the dash, or the wrist button and its button there.
            enum On {
                Dash(Part),
                Wrist(Option<u32>),
            }
            let mut best: Option<(On, f64, pe_seams::Pos)> = None;
            if self.open
                && let Some(d) = &self.dash
                && let Some((part, h)) = d.hit(r.origin, r.dir)
            {
                best = Some((On::Dash(part), h.distance, h.point));
            }
            // The left hand can't click its own wrist.
            if let Some(w) = self.wrist.as_ref().filter(|_| r.pointer != 0)
                && let Some(h) = w.hit(r.origin, r.dir)
                && best.as_ref().is_none_or(|b| h.distance < b.1)
            {
                best = Some((On::Wrist(button_at(&w.spec, h.uv)), h.distance, h.point));
            }
            // The dragging pointer is the dash's until it lets go, and its ray ends at the tip.
            if self.drag.is_some_and(|(p, _)| p == r.pointer)
                && let Some(d) = &self.dash
            {
                if r.pointer < 3 {
                    seen[r.pointer] = true;
                    out.took[r.pointer] = true;
                }
                hits.push((r, d.tip()));
                continue;
            }
            // Whose press this is: decided when it begins (a click, or a held select with no latch yet), kept
            // until its release.
            let mine = match r.pointer {
                p @ 0..3 => {
                    seen[p] = true;
                    if r.clicks > 0 || (r.pressed && self.latch[p].is_none()) {
                        self.latch[p] = Some(best.is_some());
                    }
                    let mine = self.latch[p] == Some(true);
                    if !r.pressed {
                        self.latch[p] = None;
                    }
                    out.took[p] = mine;
                    mine
                }
                _ => best.is_some(),
            };
            let held = r.pressed && mine;
            match best {
                Some((on, distance, point)) => {
                    hits.push((r, point));
                    let click = r.clicks > 0 && mine;
                    match on {
                        On::Dash(Part::Grip) => {
                            grip = if grip == Grip::Held { grip } else { Grip::Hover };
                            if click && self.drag.is_none() {
                                self.drag = Some((r.pointer, distance));
                                grip = Grip::Held;
                            }
                        }
                        On::Dash(Part::Page(b)) => {
                            page_hover = page_hover.or(b.map(PageHit::Button));
                            page_held |= held && b.is_some();
                            presses.extend(b.filter(|_| click));
                        }
                        // A custom page's point (`PageView`): no section sets one yet; My Domain's map will take it.
                        On::Dash(Part::PageAt(uv)) => {
                            page_hover = page_hover.or(Some(PageHit::Point(uv)));
                            page_held |= held;
                        }
                        On::Dash(Part::Dial(n)) => {
                            dial_hover = dial_hover.or(n);
                            dial_held |= held && n.is_some();
                            presses.extend(n.filter(|_| click));
                        }
                        On::Wrist(b) => {
                            wrist_hover = wrist_hover.or(b);
                            wrist_held |= held && b.is_some();
                            presses.extend(b.filter(|_| click));
                        }
                    }
                }
                // While the menu is open every ray shows; closed, only one that touches the wrist button.
                None if input.draw_rays && self.open => hits.push((r, r.at(RAY_LEN * scale))),
                None => {}
            }
        }
        // A pointer missing this frame (an untracked hand) has let go.
        for (p, seen) in seen.into_iter().enumerate() {
            if !seen {
                self.latch[p] = None;
            }
        }
        for b in presses {
            self.press(b);
        }
        // A press may have changed the page or the dial.
        let (dial, page) = (self.dial(), self.page(now));
        if self.open
            && let Some(d) = self.dash.as_mut()
        {
            d.dial = dial;
            d.set_page(page);
            out.overlays.extend(d.overlays(
                (dial_hover, dial_hover.filter(|_| dial_held)),
                (page_hover, page_held),
                grip,
            ));
        }
        if let Some(w) = self.wrist.as_mut() {
            out.overlays.push(w.overlay(wrist_hover, wrist_hover.filter(|_| wrist_held)));
        }
        if input.draw_rays {
            let eye = head.pos;
            for (r, end) in hits {
                if let Some(o) = ray_overlay(r.origin, end, eye, 0.003 * scale) {
                    out.overlays.push(o);
                }
                out.overlays.push(cursor_overlay(end, eye, 0.012 * scale));
            }
        }
        out
    }
}

// ---- pe-app's host: the clipboard and files, the opener's admission, the quit and the relaunch ---------------------

/// The menu's host in pe-app.
pub struct AppHost {
    /// Where copies and pastes go as files: `friend-test` beside the identity file.
    dir: std::path::PathBuf,
    /// The opener's admission: a pasted reply code goes where the console's does.
    replies: Option<std::sync::mpsc::Sender<pe_net_crypto::identity::Reply>>,
    quit: std::sync::Arc<crate::console::Quit>,
    /// In-game sessions (app.md 4.1): the manager's asker, and the current session's admission, which replaces
    /// `replies` from session to session.
    sessions: Option<(crate::session::Asker, crate::session::Replies)>,
    /// The simulation's note about members' links ([`crate::sim::Sim::with_link_notes`]).
    notes: Option<crate::leave::LinkNotes>,
}

impl AppHost {
    pub fn new(
        dir: std::path::PathBuf,
        replies: Option<std::sync::mpsc::Sender<pe_net_crypto::identity::Reply>>,
        quit: std::sync::Arc<crate::console::Quit>,
    ) -> AppHost {
        AppHost { dir, replies, quit, sessions: None, notes: None }
    }

    /// Show the simulation's note about members' links: the same `notes` as [`crate::sim::Sim::with_link_notes`].
    pub fn with_link_notes(mut self, notes: crate::leave::LinkNotes) -> AppHost {
        self.notes = Some(notes);
        self
    }

    /// Join, New session and Leave go to the session manager instead of a restart, unless `PE_MENU_RELAUNCH=1` (D7).
    pub fn with_sessions(mut self, asker: crate::session::Asker, replies: crate::session::Replies) -> AppHost {
        if relaunch_kept() {
            crate::out!("menu: PE_MENU_RELAUNCH=1: Join and New session restart the process");
        } else {
            self.sessions = Some((asker, replies));
        }
        self
    }
}

/// D7: `PE_MENU_RELAUNCH=1` keeps the restart for Join and New session, for the first VR test of the in-game join.
pub fn relaunch_kept() -> bool {
    std::env::var_os("PE_MENU_RELAUNCH").is_some_and(|v| v == "1")
}

/// The clipboard, kept for the run: on X11 and Wayland the copying process serves it, so it must outlive the copy.
static CLIPBOARD: std::sync::Mutex<Option<arboard::Clipboard>> = std::sync::Mutex::new(None);

fn with_clipboard<T>(f: impl FnOnce(&mut arboard::Clipboard) -> Result<T, arboard::Error>) -> Result<T, String> {
    let mut c = CLIPBOARD.lock().unwrap_or_else(|e| e.into_inner());
    if c.is_none() {
        *c = Some(arboard::Clipboard::new().map_err(|e| e.to_string())?);
    }
    f(c.as_mut().expect("just made")).map_err(|e| e.to_string())
}

/// Copy `text` out as `what`: always to `<dir>/<what>.txt` with a console line, and to the clipboard when it takes it.
pub fn copy_out(dir: &std::path::Path, what: &str, text: &str) -> Result<Copied, String> {
    let path = save(dir, what, text)?;
    match with_clipboard(|c| c.set_text(text.to_string())) {
        Ok(()) => {
            crate::out!("{what}: copied to the clipboard");
            Ok(Copied::Both(path))
        }
        Err(e) => {
            crate::err!("pe-app: the clipboard refused the {what} ({e}); it is in {path}");
            Ok(Copied::FileOnly(path))
        }
    }
}

/// The folder for the menu's files beside the identity file at `identity`.
pub fn files_dir(identity: &std::path::Path) -> std::path::PathBuf {
    identity.parent().map_or_else(|| std::path::PathBuf::from("friend-test"), |d| d.join("friend-test"))
}

/// Write `text` to `<dir>/<what>.txt` and say so on the console, with the text (ruling (b): every time, since a
/// clipboard can fail silently). The file's path.
pub fn save(dir: &std::path::Path, what: &str, text: &str) -> Result<String, String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let path = dir.join(format!("{what}.txt"));
    std::fs::write(&path, format!("{text}\n")).map_err(|e| format!("{}: {e}", path.display()))?;
    crate::out!("{what}: {text}");
    crate::out!("{what}: saved to {}", path.display());
    Ok(path.display().to_string())
}

impl MenuHost for AppHost {
    fn copy_out(&mut self, what: &str, text: &str) -> Result<Copied, String> {
        copy_out(&self.dir, what, text)
    }

    fn paste_in(&mut self, prefix: &str) -> Result<String, String> {
        let path = self.dir.join("paste.txt");
        let file = std::fs::read_to_string(&path).unwrap_or_default();
        pick(with_clipboard(|c| c.get_text()), &file, &path.display().to_string(), prefix)
    }

    fn check_invite(&self, code: &str) -> Result<(), String> {
        let invite = pe_net_crypto::identity::Invite::decode(code).map_err(|e| format!("it doesn't decode ({e:?})"))?;
        if invite.expiry_unix_s <= self.unix_now() {
            return Err("it has expired: ask for a new one".into());
        }
        // D1 (design d3): checked before this session is left. Their session is the one this machine is in.
        if let Some((asker, _)) = &self.sessions
            && let Some(crate::session::Role::Joiner { opener }) = asker.view().role
            && opener.0 == invite.key
        {
            return Err("you're in their session already".into());
        }
        Ok(())
    }

    fn submit_reply(&mut self, code: &str) -> Result<(), String> {
        let reply = pe_net_crypto::identity::Reply::decode(code).map_err(|e| format!("{e:?}"))?;
        if let Some((_, slot)) = &self.sessions {
            let slot = slot.lock().unwrap_or_else(|e| e.into_inner());
            let Some(tx) = slot.as_ref() else { return Err("this machine isn't letting anyone in".into()) };
            return tx.send(reply).map_err(|_| "the admission has ended".to_string());
        }
        let Some(tx) = &self.replies else { return Err("this machine isn't letting anyone in".into()) };
        tx.send(reply).map_err(|_| "the admission has ended".to_string())
    }

    fn relaunch(&mut self, r: Relaunch) {
        if let Some((asker, _)) = &self.sessions {
            let want = match r {
                Relaunch::NewSession => crate::session::Want::New,
                Relaunch::Join(code) => match pe_net_crypto::identity::Invite::decode(&code) {
                    Ok(i) => crate::session::Want::Join(i),
                    // check_invite decoded it before the press got here.
                    Err(e) => return crate::err!("menu: the invite doesn't decode ({e:?})"),
                },
            };
            if !asker.ask(want) {
                crate::err!("menu: the session manager has ended");
            }
            return;
        }
        ask_relaunch(&r);
        self.quit.request_from_app("the menu: restarting");
    }

    fn sessions(&self) -> Option<crate::session::View> {
        self.sessions.as_ref().map(|(a, _)| a.view())
    }

    fn cancel(&mut self) -> bool {
        self.sessions.as_ref().is_some_and(|(a, _)| a.ask(crate::session::Want::Cancel))
    }

    fn dismiss(&mut self) {
        if let Some((a, _)) = &self.sessions {
            a.dismiss();
        }
    }

    fn quit(&mut self) {
        self.quit.request_from_app("the menu's Quit");
    }

    fn log(&mut self, line: &str) {
        crate::out!("{line}");
    }

    fn unix_now(&self) -> u64 {
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
    }

    fn can_spawn(&self) -> bool {
        crate::spawn::installed().is_some_and(|s| s.available())
    }

    fn spawn(&mut self, job: crate::spawn::Job) {
        if let Some(s) = crate::spawn::installed() {
            s.ask(job);
        }
    }

    fn spawn_status(&self) -> Option<String> {
        crate::spawn::installed().and_then(|s| s.status())
    }

    fn link_note(&self) -> Option<crate::leave::LinkNote> {
        self.notes.as_ref().and_then(|n| n.lock().unwrap_or_else(|e| e.into_inner()).clone())
    }
}

// ---- the relaunch (option D) ----------------------------------------------------------------------------------------

static RELAUNCH: std::sync::Mutex<Option<Vec<String>>> = std::sync::Mutex::new(None);

/// Set on a relaunched process (@engine-architect's L3 on S3): its port check waits up to [`RELAUNCH_PORT_WAIT`] for
/// the old process to let go of the port.
pub const RELAUNCHED: &str = "PE_RELAUNCHED";
pub const RELAUNCH_PORT_WAIT: std::time::Duration = std::time::Duration::from_secs(5);

/// Remember to start pe-app again as `r` asks, once this run has ended.
pub fn ask_relaunch(r: &Relaunch) {
    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let args = relaunch_args(std::env::args().skip(1), r, stamp);
    *RELAUNCH.lock().unwrap_or_else(|e| e.into_inner()) = Some(args);
}

/// This run's arguments for the next: the session flags (`--open`, `--alone`, `--join <invite>`) replaced by `r`'s,
/// and `--log`'s file renamed with `-r<stamp>` (a log is never overwritten). The rest is kept. Everything from
/// `--agent` on is the agent's command (@api-eng's 1), so it is copied through as it was, after the session flags.
pub fn relaunch_args(args: impl Iterator<Item = String>, r: &Relaunch, stamp: u64) -> Vec<String> {
    let mut out = Vec::new();
    let mut agent = Vec::new();
    let mut it = args;
    while let Some(a) = it.next() {
        match a.as_str() {
            "--agent" => {
                agent.push(a);
                agent.extend(it.by_ref());
            }
            "--open" | "--alone" => {}
            "--join" => {
                it.next();
            }
            "--log" => {
                if let Some(p) = it.next() {
                    out.push(a);
                    out.push(renamed(&p, stamp));
                }
            }
            _ => out.push(a),
        }
    }
    match r {
        Relaunch::NewSession => out.push("--open".into()),
        Relaunch::Join(invite) => {
            out.push("--join".into());
            out.push(invite.clone());
        }
    }
    out.extend(agent);
    out
}

/// A pasted code starting `prefix`: from the clipboard's text, else from the paste file's lines, newest first (ruling
/// (b): the file also serves while the clipboard holds another program's text). The error names both sources.
pub fn pick(clip: Result<String, String>, file: &str, file_name: &str, prefix: &str) -> Result<String, String> {
    if let Ok(t) = &clip
        && let Some(code) = code_in(t, prefix)
    {
        return Ok(code);
    }
    if let Some(code) = file.lines().rev().find_map(|l| code_in(l, prefix)) {
        return Ok(code);
    }
    let clip = match clip {
        Ok(t) if t.trim().is_empty() => "the clipboard is empty".to_string(),
        Ok(_) => format!("the clipboard holds no {prefix} code"),
        Err(e) => format!("the clipboard can't be read ({e})"),
    };
    Err(format!("{clip}, and {file_name} has none"))
}

/// The first whitespace-separated word of `text` that starts with `prefix`, its trailing punctuation dropped: a code
/// pasted inside a sentence ("here: pe1:…") still works.
pub fn code_in(text: &str, prefix: &str) -> Option<String> {
    text.split_whitespace()
        .find(|w| w.starts_with(prefix))
        .map(|w| w.trim_end_matches(['.', ',', ';', ')', '"', '\'']).to_string())
}

/// `p` with `-r<stamp>` before its file name's extension. Edited as text, so the path keeps its own separators.
pub fn renamed(p: &str, stamp: u64) -> String {
    let name_at = p.rfind(['/', '\\']).map_or(0, |i| i + 1);
    match p[name_at..].rfind('.').filter(|&i| i > 0) {
        Some(i) => format!("{}-r{stamp}{}", &p[..name_at + i], &p[name_at + i..]),
        None => format!("{p}-r{stamp}"),
    }
}

/// If the menu asked, start pe-app again now: called as this run ends, after VR has shut down and the port is
/// closed (Fable's condition 3). The environment's per-process log paths are renamed as `--log`'s is.
pub fn relaunch_if_asked() {
    let Some(args) = RELAUNCH.lock().unwrap_or_else(|e| e.into_inner()).take() else { return };
    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let exe = match std::env::current_exe() {
        Ok(e) => e,
        Err(e) => {
            crate::err!("pe-app: couldn't restart ({e}): start it again by hand");
            return;
        }
    };
    let mut cmd = std::process::Command::new(&exe);
    cmd.args(&args).env(RELAUNCHED, "1");
    for var in [
        "PE_CUT_LOG",
        "PE_TICK_TRACE",
        #[cfg(feature = "steam-audio-trial")]
        "PE_STEAM_AUDIO_LOG",
    ] {
        if let Some(p) = std::env::var_os(var).filter(|p| !p.is_empty()) {
            cmd.env(var, renamed(&p.to_string_lossy(), stamp));
        }
    }
    // On unix the new run replaces this process (@coordinator-opus): the same process id, so the
    // terminal's run.sh still waits for it, Ctrl-C still reaches it, and nothing is left running when it ends. Every
    // file and socket of this run is close-on-exec. Windows has no exec: the new process shares this console, and
    // run-windows.ps1 waits for every pe-app.exe of its kit.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        crate::out!("pe-app: restarting in place (process {}) with {}", std::process::id(), args.join(" "));
        let e = cmd.exec();
        crate::err!("pe-app: couldn't restart ({e}): start it again by hand");
    }
    #[cfg(not(unix))]
    match cmd.spawn() {
        Ok(child) => crate::out!("pe-app: restarted as process {} with {}", child.id(), args.join(" ")),
        Err(e) => crate::err!("pe-app: couldn't restart ({e}): start it again by hand"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pe_render::menu::UiRay;
    use pe_seams::{HostTime, Offset, Pos, RigPose, RigTransform};
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    struct Log(Arc<Mutex<Vec<String>>>);

    struct FakeHost {
        log: Log,
        clipboard: bool,
        paste: Result<String, String>,
        now: u64,
        /// The world takes spawns (S4's panel shows).
        spawns: bool,
        note: Option<crate::leave::LinkNote>,
    }

    impl MenuHost for FakeHost {
        fn copy_out(&mut self, what: &str, text: &str) -> Result<Copied, String> {
            self.log.0.lock().unwrap().push(format!("copy {what} {text}"));
            let path = format!("/tmp/{what}.txt");
            Ok(if self.clipboard { Copied::Both(path) } else { Copied::FileOnly(path) })
        }
        fn paste_in(&mut self, prefix: &str) -> Result<String, String> {
            self.paste.clone().map(|t| code_in(&t, prefix).unwrap_or(t))
        }
        fn submit_reply(&mut self, code: &str) -> Result<(), String> {
            self.log.0.lock().unwrap().push(format!("reply {code}"));
            Ok(())
        }
        fn check_invite(&self, code: &str) -> Result<(), String> {
            if code.contains("expired") { Err("it has expired: ask for a new one".into()) } else { Ok(()) }
        }
        fn relaunch(&mut self, r: Relaunch) {
            self.log.0.lock().unwrap().push(format!("relaunch {r:?}"));
        }
        fn quit(&mut self) {
            self.log.0.lock().unwrap().push("quit".into());
        }
        fn log(&mut self, line: &str) {
            self.log.0.lock().unwrap().push(line.into());
        }
        fn unix_now(&self) -> u64 {
            self.now
        }
        fn can_spawn(&self) -> bool {
            self.spawns
        }
        fn spawn(&mut self, job: crate::spawn::Job) {
            self.log.0.lock().unwrap().push(format!("{job:?}"));
        }
        fn spawn_status(&self) -> Option<String> {
            self.spawns.then(|| "Box: spawned".into())
        }
        fn link_note(&self) -> Option<crate::leave::LinkNote> {
            self.note.clone()
        }
    }

    fn opener(paste: Result<String, String>, clipboard: bool) -> (Menu, Log) {
        let log = Log::default();
        let host = FakeHost { log: log.clone(), clipboard, paste, now: 1_000, spawns: false, note: None };
        (Menu::new(Role::Opener { invite: "pe1:abc".into(), expires_unix_s: 1_000 + 52 * 60 }, Box::new(host)), log)
    }

    /// What the menu did to its host, without its log lines.
    fn calls(l: &Log) -> Vec<String> {
        l.0.lock().unwrap().iter().filter(|c| !c.starts_with("menu: ")).cloned().collect()
    }

    /// The menu's log lines.
    fn logged(l: &Log) -> Vec<String> {
        l.0.lock().unwrap().iter().filter(|c| c.starts_with("menu: ")).cloned().collect()
    }

    /// A host whose sessions a manager runs (app.md 4.1): `FakeHost`, with the manager's view and its answers.
    struct InGame {
        inner: FakeHost,
        view: Arc<Mutex<crate::session::View>>,
    }

    impl MenuHost for InGame {
        fn copy_out(&mut self, what: &str, text: &str) -> Result<Copied, String> {
            self.inner.copy_out(what, text)
        }
        fn paste_in(&mut self, prefix: &str) -> Result<String, String> {
            self.inner.paste_in(prefix)
        }
        fn submit_reply(&mut self, code: &str) -> Result<(), String> {
            self.inner.submit_reply(code)
        }
        fn check_invite(&self, code: &str) -> Result<(), String> {
            self.inner.check_invite(code)
        }
        fn relaunch(&mut self, r: Relaunch) {
            self.inner.relaunch(r)
        }
        fn quit(&mut self) {
            self.inner.quit()
        }
        fn log(&mut self, line: &str) {
            self.inner.log(line)
        }
        fn unix_now(&self) -> u64 {
            self.inner.unix_now()
        }
        fn sessions(&self) -> Option<crate::session::View> {
            Some(self.view.lock().unwrap().clone())
        }
        fn cancel(&mut self) -> bool {
            let ready =
                matches!(self.view.lock().unwrap().step, Some(crate::session::Step::Joining { ready: true, .. }));
            self.inner.log.0.lock().unwrap().push(format!("cancel {}", !ready));
            !ready
        }
        fn dismiss(&mut self) {
            self.view.lock().unwrap().failure = None;
            self.inner.log.0.lock().unwrap().push("dismiss".into());
        }
    }

    /// A menu in-game: the manager's view says it is in its own session, whose invite is `pe1:mine`.
    fn in_game(paste: Result<String, String>) -> (Menu, Log, Arc<Mutex<crate::session::View>>) {
        let log = Log::default();
        let inner = FakeHost { log: log.clone(), clipboard: true, paste, now: 1_000, spawns: false, note: None };
        let view = Arc::new(Mutex::new(crate::session::View {
            role: Some(crate::session::Role::Opener {
                invite: "pe1:mine".into(),
                expires_unix_s: 1_000 + 30 * 60,
                warning: None,
            }),
            ..Default::default()
        }));
        // The menu's own role is stale on purpose: the manager's replaces it.
        let host = InGame { inner, view: view.clone() };
        (Menu::new(Role::Alone, Box::new(host)), log, view)
    }

    fn ids(m: &Menu) -> Vec<u32> {
        m.spec(1_000).buttons.iter().map(|b| b.id).collect()
    }

    /// In-game (app.md 4.1), Join asks first in words that say VR stays on, then asks the manager, and the menu stays
    /// usable: nothing restarts, so no button goes off.
    #[test]
    fn in_game_join_asks_the_manager_and_leaves_the_buttons_on() {
        let (mut m, log, _) = in_game(Ok("pe1:friend".into()));
        // Quit is on the Leave page and Close on the dial (the Gnomon dash), outside a swap.
        assert_eq!(ids(&m), [id::INVITE, id::JOIN, id::NEW_SESSION], "the manager's role");
        let leave: Vec<u32> = m.page_of(Section::Leave, 1_000).buttons.iter().map(|b| b.id).collect();
        assert_eq!(leave, [id::QUIT], "Quit is on the Leave page");
        assert!(m.spec(1_000).lines[0].contains("30 min"), "{:?}", m.spec(1_000).lines);
        m.press(id::JOIN);
        assert!(calls(&log).is_empty());
        assert_eq!(m.spec(1_000).lines[1..], JOIN_FIRST_INGAME.lines().collect::<Vec<_>>()[..]);
        m.press(id::JOIN);
        assert_eq!(calls(&log), ["relaunch Join(\"pe1:friend\")"]);
        let s = m.spec(1_000);
        assert!(s.buttons.iter().all(|b| b.enabled), "nothing restarts");
        assert_eq!(s.lines.len(), 1, "the first press's words are gone: {:?}", s.lines);
        // The own invite is the manager's session's, not the menu's stale role.
        let (mut o, log, _) = in_game(Ok("pe1:mine".into()));
        o.press(id::JOIN);
        assert!(calls(&log).is_empty());
        assert!(o.spec(1_000).lines.last().unwrap().starts_with("That's your own invite"));
        // New session and Leave too.
        let (mut n, log, _) = in_game(Err("x".into()));
        n.press(id::NEW_SESSION);
        assert_eq!(calls(&log), ["relaunch NewSession"]);
        assert!(n.spec(1_000).buttons.iter().all(|b| b.enabled));
    }

    /// During a swap (D2) the menu shows its step, Cancel while it can be, Quit and Close, and nothing else; Cancel
    /// goes to the manager, and once the join said it is ready, Cancel is gone.
    #[test]
    fn during_a_swap_the_menu_shows_its_step_and_cancel_until_the_join_is_ready() {
        let (mut m, log, view) = in_game(Err("x".into()));
        let who = pe_seams::MemberKey([0xcd; 32]);
        let since = std::time::Instant::now();
        view.lock().unwrap().step = Some(crate::session::Step::Joining { who, since, ready: false });
        assert_eq!(ids(&m), [id::CANCEL, id::QUIT, id::CLOSE]);
        assert!(m.spec(1_000).lines[0].starts_with("Joining "), "{:?}", m.spec(1_000).lines);
        m.press(id::CANCEL);
        assert_eq!(calls(&log), ["cancel true"]);
        view.lock().unwrap().step = Some(crate::session::Step::Joining { who, since, ready: true });
        assert_eq!(ids(&m), [id::QUIT, id::CLOSE], "no Cancel once they've let this machine in");
        assert!(m.spec(1_000).lines[0].contains("can't be cancelled"));
        view.lock().unwrap().step = Some(crate::session::Step::Leaving { since, bound: Some(crate::leave::MAX_WAIT) });
        assert_eq!(ids(&m), [id::CANCEL, id::QUIT, id::CLOSE], "a leave can be cut short");
        assert!(m.spec(1_000).lines[0].contains("up to 15 s"), "{:?}", m.spec(1_000).lines);
        view.lock().unwrap().step = Some(crate::session::Step::Opening { since });
        assert_eq!(ids(&m), [id::QUIT, id::CLOSE]);
        // Quit still asks first, and its button says so.
        m.press(id::QUIT);
        assert_eq!(m.spec(1_000).buttons[0].label, "Press again to quit");
        m.press(id::QUIT);
        assert_eq!(calls(&log), ["cancel true", "quit"]);
    }

    /// A failure stays on the menu until OK (D3); the joined role offers Leave; D8's warning sits beside the invite.
    #[test]
    fn a_failure_stays_until_ok_and_the_role_follows_the_manager() {
        let (mut m, log, view) = in_game(Err("x".into()));
        view.lock().unwrap().failure = Some("Couldn't join: timed out waiting for the welcome".into());
        let s = m.spec(1_000);
        assert_eq!(s.lines[0], "Couldn't join: timed out waiting for the welcome");
        assert_eq!(s.buttons[0].id, id::DISMISS);
        m.press(id::DISMISS);
        assert_eq!(calls(&log), ["dismiss"]);
        assert!(!m.spec(1_000).buttons.iter().any(|b| b.id == id::DISMISS));
        view.lock().unwrap().role = Some(crate::session::Role::Joiner { opener: pe_seams::MemberKey([1; 32]) });
        assert_eq!(ids(&m), [id::LEAVE]);
        let leave: Vec<u32> = m.page_of(Section::Leave, 1_000).buttons.iter().map(|b| b.id).collect();
        assert_eq!(leave, [id::LEAVE, id::QUIT], "the Leave page follows the manager too");
        view.lock().unwrap().role = Some(crate::session::Role::Opener {
            invite: "pe1:mine".into(),
            expires_unix_s: 1_000 + 60,
            warning: Some("Port 4000 was busy: this session took 4001".into()),
        });
        assert!(m.spec(1_000).lines.iter().any(|l| l.contains("Port 4000 was busy")), "{:?}", m.spec(1_000).lines);
    }

    #[test]
    fn a_relaunch_keeps_the_options_swaps_the_session_and_renames_the_log() {
        let args = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let was = args(&["--vr", "--preset", "plane", "--join", "pe1:old", "--log", "/l/run.log", "--alone"]);
        assert_eq!(
            relaunch_args(was.clone().into_iter(), &Relaunch::NewSession, 7),
            args(&["--vr", "--preset", "plane", "--log", "/l/run-r7.log", "--open"])
        );
        assert_eq!(
            relaunch_args(args(&["--vr", "--open"]).into_iter(), &Relaunch::Join("pe1:new".into()), 7),
            args(&["--vr", "--join", "pe1:new"])
        );
        // The agent's command is its own (@api-eng's 1): copied through, after the session flags.
        let with_agent = args(&["--vr", "--open", "--agent", "python", "x.py", "--log", "a", "--open"]);
        assert_eq!(
            relaunch_args(with_agent.into_iter(), &Relaunch::Join("pe1:new".into()), 7),
            args(&["--vr", "--join", "pe1:new", "--agent", "python", "x.py", "--log", "a", "--open"])
        );
        assert_eq!(renamed("trace", 9), "trace-r9");
        assert_eq!(renamed("/a/b/cuts.txt", 9), "/a/b/cuts-r9.txt");
        assert_eq!(renamed(r"C:\a.b\run.log", 9), r"C:\a.b\run-r9.log");
        assert_eq!(renamed(r"C:\a.b\run", 9), r"C:\a.b\run-r9");
        assert_eq!(renamed("/a/.hidden", 9), "/a/.hidden-r9");
    }

    #[test]
    fn a_copy_always_writes_its_file() {
        let dir = std::env::temp_dir().join(format!("pe-app-menu-copy-{}", std::process::id()));
        let path = save(&dir, "invite", "pe1:abc").expect("saved");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "pe1:abc\n");
        assert_eq!(files_dir(std::path::Path::new("/x/identity.seed")), std::path::Path::new("/x/friend-test"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn the_opener_menu_offers_invite_paste_join_and_new_and_shows_the_time_left() {
        let (m, _) = opener(Err("empty".into()), true);
        let s = m.spec(1_000);
        let ids: Vec<u32> = s.buttons.iter().map(|b| b.id).collect();
        // Paste reply is the console's alone now, the fallback (invite-only, the user's ruling).
        assert_eq!(ids, [id::INVITE, id::JOIN, id::NEW_SESSION]);
        assert!(s.lines[0].contains("52 min"), "{:?}", s.lines);
        assert!(m.spec(1_000 + 3_600).lines[0].contains("expired"));
        assert!(m.is_open(), "open at launch");
    }

    /// @ticket:primordial-engine/friend-test-rejoining-after-a-drop-is-refused-wi (@coordinator-opus): a
    /// member's link note shows under the status; once the link closed, the opener's New session comes first and its
    /// Invite goes. A
    /// joiner's buttons stay as they are.
    #[test]
    fn a_link_note_shows_and_a_closed_link_puts_new_session_first() {
        let note = |gone| crate::leave::LinkNote { lines: vec![format!("note {gone}")], gone };
        let host = |n| FakeHost {
            log: Log::default(),
            clipboard: true,
            paste: Err("x".into()),
            now: 1_000,
            spawns: false,
            note: Some(n),
        };
        let role = Role::Opener { invite: "pe1:abc".into(), expires_unix_s: 1_000 + 52 * 60 };
        let ids = |s: &PanelSpec| s.buttons.iter().map(|b| b.id).collect::<Vec<u32>>();
        let dropped = Menu::new(role.clone(), Box::new(host(note(false)))).spec(1_000);
        assert_eq!(ids(&dropped), [id::INVITE, id::JOIN, id::NEW_SESSION], "dropped: as before");
        assert_eq!(dropped.lines, ["Your session. Invite valid for 52 min", "note false"]);
        let gone = Menu::new(role, Box::new(host(note(true)))).spec(1_000);
        assert_eq!(ids(&gone), [id::NEW_SESSION, id::JOIN], "closed: New session first, no invite");
        assert_eq!(gone.lines, ["note true"], "the invite's time left goes too");
        let joiner = Menu::new(Role::Joiner { reply: "pe1r:x".into() }, Box::new(host(note(true)))).spec(1_000);
        assert_eq!(ids(&joiner), [id::LEAVE]);
        assert_eq!(joiner.lines.last().map(String::as_str), Some("note true"));
    }

    #[test]
    fn invite_copies_and_says_where_it_went() {
        let (mut m, log) = opener(Err("empty".into()), true);
        m.press(id::INVITE);
        assert_eq!(calls(&log), ["copy invite pe1:abc"]);
        assert!(m.spec(1_000).lines.last().unwrap().starts_with("Invite copied (52 min left)"));
        let (mut f, _) = opener(Err("empty".into()), false);
        f.press(id::INVITE);
        assert!(f.spec(1_000).lines.last().unwrap().starts_with("Invite saved to /tmp/invite.txt"));
    }

    #[test]
    fn paste_reply_submits_a_reply_code_and_refuses_anything_else() {
        let (mut m, log) = opener(Ok(" pe1r:xyz\n".into()), true);
        m.press(id::PASTE_REPLY);
        assert_eq!(calls(&log), ["reply pe1r:xyz"]);
        let (mut j, log) = opener(Ok("hello".into()), true);
        j.press(id::PASTE_REPLY);
        assert!(calls(&log).is_empty());
        assert!(j.spec(1_000).lines.last().unwrap().starts_with("No reply code to paste"));
    }

    #[test]
    fn join_relaunches_with_the_pasted_invite_and_then_the_buttons_are_off() {
        let (mut m, log) = opener(Ok("pe1:friend".into()), true);
        m.press(id::JOIN);
        assert!(calls(&log).is_empty(), "the first press only says what Join does");
        assert_eq!(m.spec(1_000).lines[1..], JOIN_FIRST.lines().collect::<Vec<_>>()[..]);
        m.press(id::JOIN);
        assert_eq!(calls(&log), ["relaunch Join(\"pe1:friend\")"]);
        assert!(m.spec(1_000).buttons.iter().all(|b| !b.enabled), "leaving: nothing more to press");
        let (mut n, log) = opener(Ok("pe1r:not-an-invite".into()), true);
        n.press(id::JOIN);
        assert!(calls(&log).is_empty(), "a reply code is no invite");
        let (mut o, log) = opener(Ok("pe1:abc".into()), true);
        o.press(id::JOIN);
        assert!(calls(&log).is_empty(), "its own invite rejoins the session it leaves");
        assert!(o.spec(1_000).lines.last().unwrap().starts_with("That's your own invite"));
        let (mut q, log) = opener(Ok("join me: pe1:friend.".into()), true);
        q.press(id::JOIN);
        q.press(id::JOIN);
        assert_eq!(calls(&log), ["relaunch Join(\"pe1:friend\")"], "a code inside a sentence");
        let (mut x, log) = opener(Ok("pe1:expired".into()), true);
        x.press(id::JOIN);
        assert!(calls(&log).is_empty(), "an invite that can't be joined never relaunches");
        assert!(x.spec(1_000).lines.last().unwrap().starts_with("That invite can't be joined: it has expired"));
        assert!(x.spec(1_000).buttons.iter().all(|b| b.enabled), "and the session stays");
    }

    /// Quit and Join ask first: a second press of the same button within `CONFIRM_S` does it, and
    /// its button says so meanwhile; a lapse, another button or closing the menu takes the question back.
    #[test]
    fn quit_and_join_ask_first_and_a_lapse_or_another_button_takes_it_back() {
        let (mut m, log) = opener(Ok("pe1:friend".into()), true);
        m.press(id::QUIT);
        assert!(calls(&log).is_empty());
        let quit = |m: &Menu, now| {
            m.page_of(Section::Leave, now).buttons.iter().find(|b| b.id == id::QUIT).unwrap().label.clone()
        };
        assert_eq!(quit(&m, 1_000), "Press again to quit");
        assert_eq!(quit(&m, 1_000 + CONFIRM_S + 1), "Quit", "lapsed");
        m.press(id::QUIT);
        assert_eq!(calls(&log), ["quit"]);
        assert!(m.spec(1_000).buttons.iter().all(|b| !b.enabled));

        let (mut n, log) = opener(Ok("pe1:friend".into()), true);
        n.press(id::QUIT);
        n.press(id::INVITE);
        n.press(id::QUIT);
        assert_eq!(calls(&log), ["copy invite pe1:abc"], "another button took the question back");
        n.press(id::JOIN);
        n.press(id::QUIT);
        n.press(id::JOIN);
        assert_eq!(calls(&log), ["copy invite pe1:abc"], "Quit took Join's back, and Join Quit's");
        n.toggle();
        n.toggle();
        n.press(id::JOIN);
        assert_eq!(calls(&log), ["copy invite pe1:abc"], "closing the menu took it back");
        n.press(id::JOIN);
        assert_eq!(calls(&log), ["copy invite pe1:abc", "relaunch Join(\"pe1:friend\")"]);
    }

    /// A first press that lapsed is a first press again (FakeHost's clock: `now`).
    #[test]
    fn a_lapsed_question_asks_again() {
        let log = Log::default();
        let host = FakeHost {
            log: log.clone(),
            clipboard: true,
            paste: Err("x".into()),
            now: 2_000,
            spawns: false,
            note: None,
        };
        let mut m = Menu::new(Role::Alone, Box::new(host));
        m.confirm = Some((id::QUIT, 2_000 - 1));
        m.press(id::QUIT);
        assert!(calls(&log).is_empty(), "lapsed: this press asks again");
        m.press(id::QUIT);
        assert_eq!(calls(&log), ["quit"]);
    }

    /// Every action is logged: the button, and what it said on one line.
    #[test]
    fn each_menu_action_is_logged() {
        let (mut m, log) = opener(Ok("pe1:friend".into()), true);
        m.press(id::INVITE);
        m.press(id::JOIN);
        m.press(id::CLOSE);
        m.toggle();
        m.press(id::QUIT);
        let l = logged(&log);
        assert!(l[0].starts_with("menu: Invite a friend: Invite copied (52 min left)"), "{l:?}");
        assert_eq!(l[1], format!("menu: Join a session: {}", JOIN_FIRST.replace('\n', " ")));
        assert_eq!(l[2..], ["menu: closed", "menu: opened", "menu: Quit: Quit the game? Press Quit again to quit."]);
    }

    #[test]
    fn a_paste_takes_the_code_from_the_file_when_the_clipboard_holds_something_else() {
        // AppHost::paste_in's lookup, given the clipboard's text and the file's.
        let file = "pe1:older\nsome note\npe1:friend\n\n";
        let p =
            |clip: Result<&str, &str>, prefix| pick(clip.map(Into::into).map_err(Into::into), file, "p.txt", prefix);
        assert_eq!(p(Ok("hello"), "pe1:"), Ok("pe1:friend".into()), "the newest line with an invite");
        assert_eq!(p(Ok("see pe1:clip, thanks"), "pe1:"), Ok("pe1:clip".into()), "the clipboard first");
        assert_eq!(p(Err("no X11"), "pe1:"), Ok("pe1:friend".into()), "a clipboard that can't be read");
        assert_eq!(p(Ok("hello"), "pe1r:"), Err("the clipboard holds no pe1r: code, and p.txt has none".into()));
        assert_eq!(p(Ok(" "), "pe1r:"), Err("the clipboard is empty, and p.txt has none".into()));
    }

    #[test]
    fn a_joiner_copies_its_reply_and_leaves_into_a_new_session() {
        let log = Log::default();
        let host =
            FakeHost { log: log.clone(), clipboard: true, paste: Err("x".into()), now: 0, spawns: false, note: None };
        let mut m = Menu::new(Role::Joiner { reply: "pe1r:me".into() }, Box::new(host));
        let ids: Vec<u32> = m.spec(0).buttons.iter().map(|b| b.id).collect();
        // No reply to copy: the invite let it in. The press still works, for the fallback.
        assert_eq!(ids, [id::LEAVE]);
        m.press(id::COPY_REPLY);
        m.press(id::LEAVE);
        assert_eq!(calls(&log), ["copy reply pe1r:me", "relaunch NewSession"]);
    }

    fn at(d: [f64; 3]) -> Pos {
        Pos::ORIGIN.checked_add(Offset(d)).unwrap()
    }

    /// The head at 1.6 m looking along -Z, and (with `to`) a ray from the head to that point.
    fn input(to: Option<Pos>, clicks: u32, toggled: bool, wrist: bool) -> UiInput {
        let rig = RigTransform { pos: Pos::ORIGIN, rotation: [0.0, 0.0, 0.0, 1.0], scale: 1.0 };
        let head = pe_render::pose::rig_to_world(
            &rig,
            &RigPose { position: [0.0, 1.6, 0.0], orientation: [0.0, 0.0, 0.0, 1.0] },
        );
        let mut rays = Vec::new();
        if let Some(to) = to {
            let dir = pe_render::pose::offset_vec(to - head.pos).normalize();
            rays.push(UiRay { pointer: 1, origin: head.pos, dir, pressed: clicks > 0, clicks });
        }
        let grip = RigPose { position: [-0.2, 1.1, -0.3], orientation: [0.0, 0.0, 0.0, 1.0] };
        UiInput {
            now: HostTime(0),
            head,
            scale: 1.0,
            rays,
            toggled,
            wrist: wrist.then(|| pe_render::pose::rig_to_world(&rig, &grip)),
            draw_rays: true,
        }
    }

    fn button(m: &Menu, b: u32) -> Option<Pos> {
        m.dash.as_ref().and_then(|d| d.button_point(b))
    }

    fn node(m: &Menu, n: u32) -> Option<Pos> {
        m.dash.as_ref().and_then(|d| d.node_point(n))
    }

    /// The dash's four parts: the dial, the gnomon, the page and the grip.
    const DASH: usize = 4;

    #[test]
    fn a_click_presses_the_button_the_ray_points_at_and_the_toggle_closes_the_menu() {
        let (mut m, log) = opener(Ok("pe1r:abc".into()), true);
        let out = m.frame(&input(None, 0, false, false));
        assert_eq!(out.overlays.len(), DASH, "open at launch, on Sessions");
        assert_eq!(m.section(), Section::Sessions);
        // Hover only: the dash, the ray and its cursor; nothing pressed.
        let invite = button(&m, id::INVITE);
        let out = m.frame(&input(invite, 0, false, false));
        assert_eq!(out.overlays.len(), DASH + 2);
        assert!(calls(&log).is_empty() && !out.took[1]);
        // A click on Invite a friend.
        let out = m.frame(&input(invite, 1, false, false));
        assert!(out.took[1], "the press went to the menu");
        assert_eq!(calls(&log), ["copy invite pe1:abc"]);
        // The menu button closes it: no dash, and a ray that meets nothing is not drawn while closed.
        let out = m.frame(&input(invite, 0, true, false));
        assert!(!m.is_open());
        assert!(out.overlays.is_empty(), "{}", out.overlays.len());
    }

    #[test]
    fn the_dial_opens_each_section_and_close_closes_the_dash() {
        let (mut m, log) = opener(Err("x".into()), true);
        m.frame(&input(None, 0, false, false));
        for s in Section::ALL {
            let n = node(&m, s.node());
            m.frame(&input(n, 1, false, false));
            assert_eq!(m.section(), s);
            m.frame(&input(None, 0, false, false));
            assert_eq!(m.dash.as_ref().unwrap().page.spec.title.as_deref(), Some(s.name()), "its page shows");
        }
        // Leave · Quit holds Quit, which still asks first.
        m.frame(&input(node(&m, Section::Leave.node()), 1, false, false));
        m.frame(&input(None, 0, false, false));
        let quit = button(&m, id::QUIT);
        m.frame(&input(quit, 1, false, false));
        m.frame(&input(None, 0, false, false));
        assert_eq!(m.page(1_000).buttons[0].label, "Press again to quit");
        // Another section takes the question back.
        m.frame(&input(node(&m, Section::People.node()), 1, false, false));
        assert!(m.confirm.is_none());
        // The mic switch says voice isn't built yet, on the open page.
        m.frame(&input(node(&m, id::MIC), 1, false, false));
        assert!(m.page(1_000).lines.last().unwrap().contains("Voice chat isn't built yet"));
        // Close, at the diameter's right end.
        m.frame(&input(node(&m, id::CLOSE), 1, false, false));
        assert!(!m.is_open());
        assert!(logged(&log).iter().any(|l| l == "menu: section My Domain"), "{:?}", logged(&log));
    }

    #[test]
    fn grabbing_the_tip_moves_the_dash_and_it_keeps_that_place_from_the_head() {
        let (mut m, log) = opener(Err("x".into()), true);
        m.frame(&input(None, 0, false, false));
        let tip = m.dash.as_ref().unwrap().tip();
        let out = m.frame(&input(Some(tip), 1, false, false));
        assert!(out.took[1] && m.drag.is_some(), "the press took the tip");
        // Held, the ray swings 0.3 m to the right at the same distance: the tip follows it.
        let d = (tip - at([0.0, 1.6, 0.0])).length();
        let mut swung = input(Some(tip), 0, false, false);
        swung.rays[0].pressed = true;
        let dir = pe_render::pose::offset_vec(tip - at([0.0, 1.6, 0.0])) + glam::DVec3::new(0.3, 0.0, 0.0);
        swung.rays[0].dir = dir.normalize();
        let out = m.frame(&swung);
        assert!(out.took[1], "dragging: still the dash's");
        let moved = m.dash.as_ref().unwrap().tip();
        assert!((moved - swung.rays[0].at(d)).length() < 1e-6, "the rig pose is f32");
        assert!((moved - tip).0[0] > 0.2, "to the right");
        // Let go: the place is kept. Closed and opened again, the dash comes back there.
        m.frame(&input(None, 0, false, false));
        assert!(m.drag.is_none() && m.rel.is_some());
        assert!(logged(&log).iter().any(|l| l == "menu: moved the dash"));
        m.frame(&input(None, 0, true, false));
        m.frame(&input(None, 0, true, false));
        assert!((m.dash.as_ref().unwrap().tip() - moved).length() < 1e-6);
    }

    #[test]
    fn the_dash_follows_a_head_that_moved_more_than_a_metre_since_it_was_placed() {
        let (mut m, _) = opener(Err("x".into()), true);
        // The first frame located before tracking: the head at the rig's origin.
        let mut first = input(None, 0, false, false);
        first.head.pos = at([0.0, 0.0, 0.0]);
        m.frame(&first);
        let low = m.dash.as_ref().unwrap().anchor.pos;
        // Tracked a frame later, 1.6 m up: placed again in front of the real head.
        m.frame(&input(None, 0, false, false));
        let placed = m.dash.as_ref().unwrap().anchor.pos;
        assert!((placed - low).length() > 1.0, "re-placed");
        let a = Arrangement::VR;
        assert!((placed - at([0.0, 1.6 - a.down, -a.ahead])).length() < 1e-6, "the rig pose is f32");
        // A small move of the head leaves it where it is.
        let mut near = input(None, 0, false, false);
        near.head.pos = at([0.3, 1.6, 0.0]);
        m.frame(&near);
        assert_eq!(m.dash.as_ref().unwrap().anchor.pos, placed);
    }

    #[test]
    fn the_window_gets_the_flat_arrangement() {
        let (mut m, _) = opener(Err("x".into()), true);
        let mut i = input(None, 0, false, false);
        i.draw_rays = false;
        m.frame(&i);
        assert_eq!(m.dash.as_ref().unwrap().arrangement, Arrangement::WINDOW);
    }

    #[test]
    fn a_press_belongs_to_the_menu_or_not_from_its_start_to_its_release() {
        let (mut m, log) = opener(Err("x".into()), true);
        m.frame(&input(None, 0, false, false));
        let join = button(&m, id::JOIN);
        let held = |on: bool, clicks: u32| {
            let mut i = input(join, 1, false, false);
            let r = &mut i.rays[0];
            if !on {
                r.dir = -r.dir; // behind the head, away from the dash
            }
            r.clicks = clicks;
            i
        };
        // A press that begins off the dash and is dragged onto it, held: never the menu's.
        assert!(!m.frame(&held(false, 1)).took[1]);
        assert!(!m.frame(&held(true, 0)).took[1], "a held grab crossing the dash stays the hand's");
        assert!(calls(&log).is_empty(), "and presses no button");
        // Released (a frame without the select), then a press that begins on the dash and is dragged off: the menu's.
        let mut up = held(false, 0);
        up.rays[0].pressed = false;
        assert!(!m.frame(&up).took[1]);
        assert!(m.frame(&held(true, 1)).took[1]);
        assert!(m.frame(&held(false, 0)).took[1], "dragged off, it still starts no grab");
    }

    #[test]
    fn the_my_domain_page_holds_the_test_objects_and_spawns_in_front_of_the_head() {
        let log = Log::default();
        let host = FakeHost {
            log: log.clone(),
            clipboard: true,
            paste: Err("x".into()),
            now: 1_000,
            spawns: true,
            note: None,
        };
        let mut m = Menu::new(Role::Opener { invite: "pe1:abc".into(), expires_unix_s: 9_000 }, Box::new(host));
        m.press(Section::MyDomain.node());
        let page = m.page(1_000);
        let labels: Vec<&str> = page.buttons.iter().map(|b| b.label.as_str()).collect();
        // The spawn buttons' ids end before Clear's: 10 kinds fill 20..30 exactly, so an 11th needs Clear moved.
        assert!(id::SPAWN + crate::spawn::Kind::ALL.len() as u32 <= id::CLEAR);
        // Light to heavy, then Clear: 11 buttons.
        assert_eq!(
            labels,
            [
                "Box",
                "Sphere",
                "Capsule",
                "Box stack",
                "Sack",
                "Barrel",
                "Low crate",
                "Big crate",
                "Platform",
                "Slab",
                "Clear"
            ]
        );
        assert_eq!(page.lines, ["Box: spawned"], "the last result shows");
        m.frame(&input(None, 0, false, false));
        // The head is at (0, 1.6, 0) facing -Z: a box 0.5 m ahead and 0.2 m down, a stack 0.8 m ahead on the floor.
        m.press(id::SPAWN);
        m.press(id::SPAWN + 3);
        m.press(id::CLEAR);
        let c = calls(&log);
        assert_eq!(c.len(), 3);
        assert!(c[0].starts_with("Spawn(Box, [-0.25, 1.4") && c[0].contains(", -0.5"), "{}", c[0]);
        assert!(c[1].starts_with("Spawn(Stack, [-0.25, 0.01, -0.8"), "{}", c[1]);
        assert_eq!(c[2], "Clear");
    }

    #[test]
    fn the_my_domain_page_says_so_where_the_world_takes_no_spawns() {
        let (mut m, _) = opener(Err("x".into()), true);
        m.press(Section::MyDomain.node());
        let page = m.page(1_000);
        assert!(page.buttons.is_empty());
        assert_eq!(page.lines, ["Nothing to take out in this domain"]);
    }

    #[test]
    fn the_wrist_button_is_there_while_the_left_hand_is_tracked() {
        let (mut m, _) = opener(Err("x".into()), true);
        let out = m.frame(&input(None, 0, false, true));
        assert_eq!(out.overlays.len(), DASH + 1, "the dash and the wrist button");
        let out = m.frame(&input(None, 0, false, false));
        assert_eq!(out.overlays.len(), DASH, "no wrist: no wrist button");
    }
}
