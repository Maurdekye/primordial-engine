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

use pe_render::menu::{
    Button, MenuPanel, PanelSpec, Ui, UiInput, UiOutput, button_at, cursor_overlay, place_beside, place_in_front,
    ray_overlay, wrist_place,
};

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
    /// End this run and start the process again (after VR has shut down).
    fn relaunch(&mut self, r: Relaunch);
    /// End the run.
    fn quit(&mut self);
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
    /// The spawn panel's (S4): one per template, in `spawn::Kind::ALL`'s order, then Clear.
    pub const SPAWN: u32 = 20;
    pub const CLEAR: u32 = 24;
    pub const WRIST: u32 = 100;
}

/// The menu: whether it is open, its panels, and its last message.
pub struct Menu {
    role: Role,
    host: Box<dyn MenuHost>,
    open: bool,
    /// The main panel, placed in front of the head when the menu opens.
    main: Option<MenuPanel>,
    /// The wrist button, placed on the left wrist each frame it is tracked.
    wrist: Option<MenuPanel>,
    /// The last thing a button did, shown on the panel.
    message: Option<String>,
    /// A relaunch or quit is under way: the buttons are off.
    leaving: bool,
    /// Where the head was when the main panel was placed.
    placed_at: Option<pe_seams::Pos>,
    /// The spawn panel, beside the main one while it is open and the world takes spawns (S4).
    side: Option<MenuPanel>,
    /// The head this frame, for where a spawn appears.
    head: Option<pe_render::pose::WorldPose>,
    /// The hands' pointer origins this frame, as offsets from the origin: a spawn keeps clear of them.
    hands: Vec<[f64; 3]>,
    /// Per pointer, while its select is held: whether the press began on a panel (@ballpit-viz's M1 on S3). A press
    /// belongs to the menu, or not, from its start to its release, so a held grab sweeping across a panel isn't
    /// dropped, and a press dragged off a panel doesn't start one.
    latch: [Option<bool>; 3],
}

/// Where the main panel opens from the head: 0.6 m ahead, 0.15 m down, facing the head, upright.
const AHEAD: f64 = 0.6;
const DOWN: f64 = 0.15;
/// The wrist button's size against a main panel's (about 12 cm wide).
const WRIST_SCALE: f64 = 0.35;
/// A ray that meets nothing is drawn this long (metres at a rig scale of 1).
const RAY_LEN: f64 = 2.0;
/// The spawn panel's gap from the main one, in metres at scale 1, and its turn toward the head.
const SIDE_GAP: f64 = 0.04;
const SIDE_TURN_DEG: f64 = 20.0;
/// How far the head may move from where it was when the main panel was placed before the panel follows (review L3).
const REPLACE_M: f64 = 1.0;
/// An invite's and a reply code's prefixes (pe-net-crypto's encodings).
const INVITE_PREFIX: &str = "pe1:";
const REPLY_PREFIX: &str = "pe1r:";

impl Menu {
    pub fn new(role: Role, host: Box<dyn MenuHost>) -> Menu {
        Menu {
            role,
            host,
            open: true,
            main: None,
            wrist: None,
            message: None,
            leaving: false,
            placed_at: None,
            side: None,
            head: None,
            hands: Vec::new(),
            latch: [None; 3],
        }
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    /// The main panel's content now.
    pub fn spec(&self, now_unix: u64) -> PanelSpec {
        let mut b = Vec::new();
        let mut lines = Vec::new();
        match &self.role {
            Role::Opener { expires_unix_s, .. } => {
                b.push(Button::new(id::INVITE, "Invite a friend"));
                b.push(Button::new(id::PASTE_REPLY, "Paste reply"));
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
                b.push(Button::new(id::COPY_REPLY, "Copy my reply code"));
                b.push(Button::new(id::LEAVE, "Leave (back to my own session)"));
                lines.push("In someone else's session".into());
            }
            Role::Alone => {
                b.push(Button::new(id::NEW_SESSION, "New session"));
                b.push(Button::new(id::JOIN, "Join a session"));
            }
        }
        b.push(Button::new(id::QUIT, "Quit"));
        b.push(Button::new(id::CLOSE, "Close menu"));
        if self.leaving {
            for x in &mut b {
                x.enabled = false;
            }
        }
        if let Some(m) = &self.message {
            lines.push(m.clone());
        }
        PanelSpec { title: Some("Primordial Engine".into()), buttons: b, lines }
    }

    /// Press button `b`.
    pub fn press(&mut self, b: u32) {
        let now = self.host.unix_now();
        let msg = match b {
            id::INVITE => match &self.role {
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
            id::COPY_REPLY => match &self.role {
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
                Ok(code) if matches!(&self.role, Role::Opener { invite, .. } if *invite == code) => {
                    "That's your own invite: send it to your friend, and copy theirs to join them".into()
                }
                Ok(code) if code.starts_with(INVITE_PREFIX) => match self.host.check_invite(&code) {
                    Ok(()) => {
                        self.leaving = true;
                        self.host.relaunch(Relaunch::Join(code));
                        "Joining: restarting…".into()
                    }
                    Err(e) => format!("That invite can't be joined: {e}"),
                },
                Ok(_) => "Copy the invite you were sent first (it starts pe1:)".into(),
                Err(e) => format!("No invite to paste: {e}"),
            }),
            id::NEW_SESSION | id::LEAVE => {
                self.leaving = true;
                self.host.relaunch(Relaunch::NewSession);
                Some("Restarting in a new session of your own…".into())
            }
            id::QUIT => {
                self.leaving = true;
                self.host.quit();
                Some("Quitting…".into())
            }
            id::CLOSE => {
                self.open = false;
                None
            }
            b if (id::SPAWN..id::SPAWN + 4).contains(&b) => {
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
            _ => None,
        };
        if msg.is_some() {
            self.message = msg;
        }
    }

    fn toggle(&mut self) {
        self.open = !self.open;
        if self.open {
            self.main = None; // placed again in front of the head
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
        PanelSpec { title: Some("Spawn".into()), buttons, lines: self.host.spawn_status().into_iter().collect() }
    }

    /// The main panel is placed again when the head is more than [`REPLACE_M`] from where it was at the placing (review
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
        // The panels this frame: the main one while open, the wrist button while the wrist is tracked.
        if self.open {
            let spec = self.spec(now);
            let head = input.head;
            let scale = input.scale;
            if self.stale_place(head.pos, scale) {
                self.main = None;
            }
            if self.main.is_none() {
                self.placed_at = Some(head.pos);
            }
            let placed = self.main.is_none();
            let p = self
                .main
                .get_or_insert_with(|| MenuPanel::new(spec.clone(), place_in_front(&head, AHEAD, DOWN, scale), scale));
            p.spec = spec;
            let main_pose = p.pose;
            // The spawn panel sits to the main one's right, turned toward the head, placed with it.
            if self.host.can_spawn() {
                let spec = self.side_spec();
                match self.side.as_mut() {
                    Some(s) if !placed => s.spec = spec,
                    _ => {
                        let across =
                            (pe_render::menu::PANEL_PX as f64 * pe_render::menu::METRES_PER_PX + SIDE_GAP) * scale;
                        self.side = Some(MenuPanel::new(spec, place_beside(&main_pose, across, SIDE_TURN_DEG), scale));
                    }
                }
            } else {
                self.side = None;
            }
        }
        self.head = Some(input.head);
        self.hands =
            input.rays.iter().filter(|r| r.pointer < 2).map(|r| (r.origin - pe_seams::Pos::ORIGIN).0).collect();
        // The wrist button follows the left hand; it is gone while that hand isn't tracked.
        self.wrist = input.wrist.map(|w| {
            let spec = PanelSpec { title: None, buttons: vec![Button::new(id::WRIST, "Menu")], lines: Vec::new() };
            MenuPanel::new(spec, wrist_place(&w, input.scale), WRIST_SCALE * input.scale)
        });
        // Each ray: the nearest panel it meets, its button, and its presses.
        let mut hover: [Option<u32>; 3] = [None; 3];
        // A panel's button shows pressed only while a ray on that panel holds its select.
        let mut held = [false; 3];
        let mut presses = Vec::new();
        let mut hits = Vec::new();
        let mut seen = [false; 3];
        for r in &input.rays {
            let mut best: Option<(usize, f64, Option<u32>, pe_seams::Pos)> = None;
            let main = self.main.as_ref().filter(|_| self.open);
            let side = self.side.as_ref().filter(|_| self.open);
            // The left hand can't click its own wrist.
            let wrist = self.wrist.as_ref().filter(|_| r.pointer != 0);
            for (k, p) in [(0usize, main), (1, wrist), (2, side)] {
                if let Some(p) = p
                    && let Some(h) = p.hit(r.origin, r.dir)
                    && best.is_none_or(|b| h.distance < b.1)
                {
                    best = Some((k, h.distance, button_at(&p.spec, h.uv), h.point));
                }
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
            if let Some((k, _, b, point)) = best {
                hover[k] = hover[k].or(b);
                held[k] |= r.pressed && mine && b.is_some();
                hits.push((r, point));
                if r.clicks > 0
                    && mine
                    && let Some(b) = b
                {
                    presses.push(b);
                }
            } else if input.draw_rays && self.open {
                // While the menu is open every ray shows; closed, only one that touches the wrist button.
                hits.push((r, r.at(RAY_LEN * input.scale)));
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
        if self.open
            && let Some(p) = self.main.as_mut()
        {
            out.overlays.push(p.overlay(hover[0], hover[0].filter(|_| held[0])));
        }
        if self.open
            && let Some(s) = self.side.as_mut()
        {
            out.overlays.push(s.overlay(hover[2], hover[2].filter(|_| held[2])));
        }
        if let Some(w) = self.wrist.as_mut() {
            out.overlays.push(w.overlay(hover[1], hover[1].filter(|_| held[1])));
        }
        if input.draw_rays {
            let eye = input.head.pos;
            for (r, end) in hits {
                if let Some(o) = ray_overlay(r.origin, end, eye, 0.003 * input.scale) {
                    out.overlays.push(o);
                }
                out.overlays.push(cursor_overlay(end, eye, 0.012 * input.scale));
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
}

impl AppHost {
    pub fn new(
        dir: std::path::PathBuf,
        replies: Option<std::sync::mpsc::Sender<pe_net_crypto::identity::Reply>>,
        quit: std::sync::Arc<crate::console::Quit>,
    ) -> AppHost {
        AppHost { dir, replies, quit }
    }
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
        Ok(())
    }

    fn submit_reply(&mut self, code: &str) -> Result<(), String> {
        let reply = pe_net_crypto::identity::Reply::decode(code).map_err(|e| format!("{e:?}"))?;
        let Some(tx) = &self.replies else { return Err("this machine isn't letting anyone in".into()) };
        tx.send(reply).map_err(|_| "the admission has ended".to_string())
    }

    fn relaunch(&mut self, r: Relaunch) {
        ask_relaunch(&r);
        self.quit.request_from_app("the menu: restarting");
    }

    fn quit(&mut self) {
        self.quit.request_from_app("the menu's Quit");
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
    match cmd.spawn() {
        Ok(child) => crate::out!("pe-app: restarted as process {} with {}", child.id(), args.join(" ")),
        Err(e) => crate::err!("pe-app: couldn't restart ({e}): start it again by hand"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pe_render::menu::{UiRay, layout};
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
    }

    fn opener(paste: Result<String, String>, clipboard: bool) -> (Menu, Log) {
        let log = Log::default();
        let host = FakeHost { log: log.clone(), clipboard, paste, now: 1_000, spawns: false };
        (Menu::new(Role::Opener { invite: "pe1:abc".into(), expires_unix_s: 1_000 + 52 * 60 }, Box::new(host)), log)
    }

    fn calls(l: &Log) -> Vec<String> {
        l.0.lock().unwrap().clone()
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
        assert_eq!(ids, [id::INVITE, id::PASTE_REPLY, id::JOIN, id::NEW_SESSION, id::QUIT, id::CLOSE]);
        assert!(s.lines[0].contains("52 min"), "{:?}", s.lines);
        assert!(m.spec(1_000 + 3_600).lines[0].contains("expired"));
        assert!(m.is_open(), "open at launch");
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
        assert_eq!(calls(&log), ["relaunch Join(\"pe1:friend\")"], "a code inside a sentence");
        let (mut x, log) = opener(Ok("pe1:expired".into()), true);
        x.press(id::JOIN);
        assert!(calls(&log).is_empty(), "an invite that can't be joined never relaunches");
        assert!(x.spec(1_000).lines.last().unwrap().starts_with("That invite can't be joined: it has expired"));
        assert!(x.spec(1_000).buttons.iter().all(|b| b.enabled), "and the session stays");
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
        let host = FakeHost { log: log.clone(), clipboard: true, paste: Err("x".into()), now: 0, spawns: false };
        let mut m = Menu::new(Role::Joiner { reply: "pe1r:me".into() }, Box::new(host));
        let ids: Vec<u32> = m.spec(0).buttons.iter().map(|b| b.id).collect();
        assert_eq!(ids, [id::COPY_REPLY, id::LEAVE, id::QUIT, id::CLOSE]);
        m.press(id::COPY_REPLY);
        m.press(id::LEAVE);
        assert_eq!(calls(&log), ["copy reply pe1r:me", "relaunch NewSession"]);
    }

    fn at(d: [f64; 3]) -> Pos {
        Pos::ORIGIN.checked_add(Offset(d)).unwrap()
    }

    /// The head at 1.6 m looking along -Z, and (with `b`) a ray from the head at the main panel's button `b`.
    fn input(menu: &Menu, b: Option<usize>, clicks: u32, toggled: bool, wrist: bool) -> UiInput {
        let rig = RigTransform { pos: Pos::ORIGIN, rotation: [0.0, 0.0, 0.0, 1.0], scale: 1.0 };
        let head = pe_render::pose::rig_to_world(
            &rig,
            &RigPose { position: [0.0, 1.6, 0.0], orientation: [0.0, 0.0, 0.0, 1.0] },
        );
        let mut rays = Vec::new();
        if let Some(b) = b {
            let l = layout(&menu.spec(1_000));
            let r = l.buttons[b];
            let h = l.size[1] as f64 * pe_render::menu::METRES_PER_PX;
            let y = 1.6 - DOWN + h * (0.5 - (r[1] + r[3]) as f64 / 2.0 / l.size[1] as f64);
            let dir = pe_render::pose::offset_vec(at([0.0, y, -AHEAD]) - at([0.0, 1.6, 0.0])).normalize();
            rays.push(UiRay { pointer: 1, origin: at([0.0, 1.6, 0.0]), dir, pressed: clicks > 0, clicks });
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

    #[test]
    fn a_click_presses_the_button_the_ray_points_at_and_the_toggle_closes_the_menu() {
        let (mut m, log) = opener(Ok("pe1r:abc".into()), true);
        // Hover only: the panel, the ray and its cursor; nothing pressed.
        let out = m.frame(&input(&m, Some(1), 0, false, false));
        assert_eq!(out.overlays.len(), 3);
        assert!(calls(&log).is_empty() && !out.took[1]);
        // A click on Paste reply, the second button.
        let out = m.frame(&input(&m, Some(1), 1, false, false));
        assert!(out.took[1], "the press went to the menu");
        assert_eq!(calls(&log), ["reply pe1r:abc"]);
        // The menu button closes it: no panel, and a ray that meets nothing is not drawn while closed.
        let out = m.frame(&input(&m, Some(1), 0, true, false));
        assert!(!m.is_open());
        assert!(out.overlays.is_empty(), "{}", out.overlays.len());
    }

    #[test]
    fn the_panel_follows_a_head_that_moved_more_than_a_metre_since_it_was_placed() {
        let (mut m, _) = opener(Err("x".into()), true);
        // The first frame located before tracking: the head at the rig's origin.
        let mut first = input(&m, None, 0, false, false);
        first.head.pos = at([0.0, 0.0, 0.0]);
        m.frame(&first);
        let low = m.main.as_ref().unwrap().pose.pos;
        // Tracked a frame later, 1.6 m up: placed again in front of the real head.
        m.frame(&input(&m, None, 0, false, false));
        let placed = m.main.as_ref().unwrap().pose.pos;
        assert!((placed - low).length() > 1.0, "re-placed");
        assert!((placed - at([0.0, 1.6 - DOWN, -AHEAD])).length() < 1e-6, "the rig pose is f32");
        // A small move of the head leaves it where it is.
        let mut near = input(&m, None, 0, false, false);
        near.head.pos = at([0.3, 1.6, 0.0]);
        m.frame(&near);
        assert_eq!(m.main.as_ref().unwrap().pose.pos, placed);
    }

    #[test]
    fn a_press_belongs_to_the_menu_or_not_from_its_start_to_its_release() {
        let (mut m, log) = opener(Err("x".into()), true);
        let held = |m: &Menu, on: bool, clicks: u32| {
            let mut i = input(m, Some(1), 1, false, false);
            let r = &mut i.rays[0];
            if !on {
                r.dir = -r.dir; // behind the head, away from the panel
            }
            r.clicks = clicks;
            i
        };
        // A press that begins off the panel and is dragged onto it, held: never the menu's.
        assert!(!m.frame(&held(&m, false, 1)).took[1]);
        assert!(!m.frame(&held(&m, true, 0)).took[1], "a held grab crossing the panel stays the hand's");
        assert!(calls(&log).is_empty(), "and presses no button");
        // Released (a frame without the select), then a press that begins on the panel and is dragged off: the menu's.
        let mut up = held(&m, false, 0);
        up.rays[0].pressed = false;
        assert!(!m.frame(&up).took[1]);
        assert!(m.frame(&held(&m, true, 1)).took[1]);
        assert!(m.frame(&held(&m, false, 0)).took[1], "dragged off, it still starts no grab");
    }

    #[test]
    fn the_spawn_panel_sits_beside_the_menu_and_spawns_in_front_of_the_head() {
        let log = Log::default();
        let host = FakeHost { log: log.clone(), clipboard: true, paste: Err("x".into()), now: 1_000, spawns: true };
        let mut m = Menu::new(Role::Opener { invite: "pe1:abc".into(), expires_unix_s: 9_000 }, Box::new(host));
        let out = m.frame(&input(&m, None, 0, false, false));
        assert_eq!(out.overlays.len(), 2, "the main panel and the spawn panel");
        let side = m.side.as_ref().expect("the spawn panel");
        let labels: Vec<&str> = side.spec.buttons.iter().map(|b| b.label.as_str()).collect();
        assert_eq!(labels, ["Box", "Sphere", "Capsule", "Box stack", "Clear"]);
        assert_eq!(side.spec.lines, ["Box: spawned"], "the last result shows");
        let main = m.main.as_ref().unwrap().pose.pos;
        assert!((side.pose.pos - main).0[0] > 0.3, "to the right of the main panel");
        // The head is at (0, 1.6, 0) facing -Z: a box 0.5 m ahead and 0.2 m down, a stack 0.8 m ahead on the floor.
        m.press(id::SPAWN);
        m.press(id::SPAWN + 3);
        m.press(id::CLEAR);
        let c = calls(&log);
        assert_eq!(c.len(), 3);
        assert!(c[0].starts_with("Spawn(Box, [-0.25, 1.4") && c[0].contains(", -0.5"), "{}", c[0]);
        assert!(c[1].starts_with("Spawn(Stack, [-0.25, 0.01, -0.8"), "{}", c[1]);
        assert_eq!(c[2], "Clear");
        // Closed: no spawn panel.
        let out = m.frame(&input(&m, None, 0, true, false));
        assert!(out.overlays.is_empty());
    }

    #[test]
    fn no_spawn_panel_where_the_world_takes_no_spawns() {
        let (mut m, _) = opener(Err("x".into()), true);
        let out = m.frame(&input(&m, None, 0, false, false));
        assert_eq!(out.overlays.len(), 1);
        assert!(m.side.is_none());
    }

    #[test]
    fn the_wrist_button_is_there_while_the_left_hand_is_tracked() {
        let (mut m, _) = opener(Err("x".into()), true);
        let out = m.frame(&input(&m, None, 0, false, true));
        assert_eq!(out.overlays.len(), 2, "the panel and the wrist button");
        let out = m.frame(&input(&m, None, 0, false, false));
        assert_eq!(out.overlays.len(), 1, "no wrist: no wrist button");
    }
}
