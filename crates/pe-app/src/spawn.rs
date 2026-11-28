//! The friend test's spawn buttons (S4; @item:primordial-engine/friend-test-xr-eng-s-slices-vr-pointers-menu-and):
//! Box, Sphere, Capsule and Box stack copy the plane's templates into `/Spawned`, and Clear deletes what is there.
//! Each is one pe-api `apply`, made as this machine's member, the same edit an agent or the inspector would make
//! (@world-model's interface: `create` from a template, which needs a `name`; app.md 4.2).
//!
//! - **Off the render thread:** the menu and the console only queue a job. A worker thread applies it and leaves a
//!   one-line status for the spawn panel and the console.
//! - **Where:** a button press gives the point in front of the head; the console's `spawn` uses a fixed point in
//!   front of the origin. A stack goes on the floor; the rest are dropped from where they appear. The console's
//!   `spawn <kind> <count>` (a self-test's way to fill the world: K4's R5', the shape limit) queues that many, each
//!   its own job, on a grid of their own ([`grid_at`]), so a hundred never land inside each other as the console's
//!   row of five, which repeats every 20, would put them.
//! - **Only on the plane:** the worker looks for `/Spawned` until it finds it, every [`LOOK_AGAIN`], and again before
//!   a job while it hasn't. A joiner's world is empty until the cut's state arrives, so a miss is never final
//!   (@engine-architect's M1 on S4). Until it is found (and in the ball pit, always) the panel isn't shown, and a job
//!   says that spawning needs the plane.
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use pe_seams::MemberKey;
use pe_seams::world_api::{
    ApplyOpts, Caller, ConnectionId, Outcome, Producer, ProducerKind, Query, RefusalCode, Scope, Status, Transaction,
    WorldApi,
};
use serde_json::{Value, json};

/// What a button or the console spawns: the plane's templates (pe-world `preset::plane_names`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Box,
    Sphere,
    Capsule,
    Stack,
    /// The mid-weight templates (pe-world `preset::MID`, after the 11-27 test): too heavy to lift, light enough to
    /// drag.
    Sack,
    Barrel,
    LowCrate,
    /// The heavy templates (pe-world `preset::HEAVY`, the friend test of 11-27): solid ground a player stands on.
    BigCrate,
    Platform,
    Slab,
}

impl Kind {
    /// Light to heavy, as the spawn panel lists them (@xr-eng).
    pub const ALL: [Kind; 10] = [
        Kind::Box,
        Kind::Sphere,
        Kind::Capsule,
        Kind::Stack,
        Kind::Sack,
        Kind::Barrel,
        Kind::LowCrate,
        Kind::BigCrate,
        Kind::Platform,
        Kind::Slab,
    ];

    /// The template's name, which is also the copy's.
    pub fn template(self) -> &'static str {
        use pe_world::preset::plane_names as n;
        match self {
            Kind::Box => n::BOX,
            Kind::Sphere => n::SPHERE,
            Kind::Capsule => n::CAPSULE,
            Kind::Stack => n::STACK,
            Kind::Sack => n::SACK,
            Kind::Barrel => n::BARREL,
            Kind::LowCrate => n::LOW_CRATE,
            Kind::BigCrate => n::BIG_CRATE,
            Kind::Platform => n::PLATFORM,
            Kind::Slab => n::SLAB,
        }
    }

    /// The console's word for it.
    pub fn word(self) -> &'static str {
        match self {
            Kind::Box => "box",
            Kind::Sphere => "sphere",
            Kind::Capsule => "capsule",
            Kind::Stack => "stack",
            Kind::Sack => "sack",
            Kind::Barrel => "barrel",
            Kind::LowCrate => "lowcrate",
            Kind::BigCrate => "crate",
            Kind::Platform => "platform",
            Kind::Slab => "slab",
        }
    }

    /// The half extents, m, of a template that stands on the floor: a heavy one's box (pe-world `preset::HEAVY`), or a
    /// mid-weight one's (`preset::MID`; a capsule's are its radius across and its radius plus half segment up). `None`
    /// for the small ones, which appear in the air.
    pub fn floor_half(self) -> Option<[f32; 3]> {
        use pe_world::components::shape_kind;
        let name = self.template();
        let heavy = pe_world::preset::HEAVY.iter().find(|h| h.0 == name).map(|h| h.1);
        heavy.or_else(|| {
            pe_world::preset::MID.iter().find(|m| m.0 == name).map(|&(_, kind, s, ..)| match kind {
                shape_kind::CAPSULE => [s[0], s[0] + s[1], s[0]],
                _ => s,
            })
        })
    }

    /// The root's height when it stands on the floor: a stack's root just above it, a floor kind's centre at its half
    /// height and the same gap; `None` for what appears in the air.
    pub fn on_floor(self) -> Option<f64> {
        match (self, self.floor_half()) {
            (Kind::Stack, _) => Some(FLOOR_GAP),
            (_, Some(h)) => Some(f64::from(h[1]) + FLOOR_GAP),
            _ => None,
        }
    }

    /// How far a counted spawn's next layer goes above the last (`grid_at`): a stack's or a floor kind's height and a
    /// little more, so it settles on the one below; a small one's [`GRID_STEP`], since it falls.
    pub fn layer_height(self) -> f64 {
        match (self, self.floor_half()) {
            (Kind::Stack, _) => STACK_LAYER,
            (_, Some(h)) => 2.0 * f64::from(h[1]) + 0.1,
            _ => GRID_STEP,
        }
    }

    /// How far apart two of it go on a grid or a row, m: the small ones' [`GRID_STEP`], a floor kind's width and
    /// 0.5 m more.
    pub fn spacing(self) -> f64 {
        self.floor_half().map_or(GRID_STEP, |h| 2.0 * f64::from(h[0].max(h[2])) + 0.5)
    }

    pub fn parse(w: &str) -> Option<Kind> {
        Kind::ALL.into_iter().find(|k| k.word() == w)
    }
}

/// One job for the worker.
#[derive(Clone, Debug, PartialEq)]
pub enum Job {
    /// A copy of the template with its root at this offset from the world's origin, in metres.
    Spawn(Kind, [f64; 3]),
    /// Delete everything under `/Spawned`.
    Clear,
    /// Set the world's shape limit (`/World settings`' `Limits.max_shapes`, world-model.md 7.1): an ordinary edit,
    /// so every member switches at its frame.
    Limit(u32),
}

/// How often the worker looks for `/Spawned` while it hasn't found it.
pub const LOOK_AGAIN: std::time::Duration = std::time::Duration::from_millis(500);
/// The parent spawns go under (pe-world `preset::plane_names::SPAWNED`).
pub const SPAWNED: &str = "/Spawned";
/// Deletes per transaction while clearing, and the most rounds a clear takes.
const CLEAR_BATCH: usize = 32;
const CLEAR_ROUNDS: usize = 16;
/// The longest the worker waits out pe-api's change rate for one change (api.md 9.3: 20 a second, a burst of 40) before
/// it gives up and says so: a counted spawn queues a hundred at once, and a clear of 500 is 16 changes (K4's R5', where
/// 43 of 102 stacks were refused `rate_limited`). A quick second press of a button can meet the same limit.
pub const RATE_PATIENCE: std::time::Duration = std::time::Duration::from_secs(5);
/// The longest one wait, whatever `retry_after_ms` says.
const RATE_WAIT_MAX: std::time::Duration = std::time::Duration::from_secs(1);
/// The in-process caller's connection: far from pe-mcp's, which count up from 1.
const CONNECTION: ConnectionId = ConnectionId(u64::MAX - 7);
/// Where the console's `spawn` puts a thing: 1 m in front of the origin, 0.8 m up (a stack on the floor), each one
/// 0.3 m to the right of the last, five to a row.
const CONSOLE_AT: [f64; 3] = [-0.6, 0.8, -1.0];

/// Where the console's counted `spawn <kind> <count>` starts its grid: 3 m in front of the origin, past the single
/// spawns' rows, 0.8 m up (a stack on the floor), [`GRID_ACROSS`] to a row [`GRID_STEP`] apart, each row
/// [`GRID_STEP`] further ahead, [`grid_rows`] rows to a layer, and each layer [`GRID_STEP`] above the last (a stack's
/// layers [`STACK_LAYER`] apart). A stack's boxes are 0.15 m across, so one that falls reaches its neighbours. The layer
/// keeps to the ground @world-chunks keeps clear of the course (x -4.5 to 7.0, z -3 to -30), and to
/// the floor's own edge less 1 m.
pub const GRID_AT: [f64; 3] = [-4.35, 0.8, -3.0];
pub const GRID_STEP: f64 = 0.3;
pub const GRID_ACROSS: u32 = 38;
/// The course's clear ground ends 30 m ahead (@world-chunks).
const GRID_FAR: f64 = 30.0;
/// The most one counted `spawn` queues: the build's shape limit (pe-world `MAX_SHAPES`, world-model.md 7.1). A world
/// whose `Limits` row says less refuses the rest; one that says more takes them in several counts.
pub const COUNT_MAX: u32 = pe_world::edit::MAX_SHAPES as u32;

/// Rows to a layer of the counted spawn's grid, `step` apart: as far as the course's clear ground, and the floor's
/// edge less 1 m.
pub fn grid_rows(step: f64) -> u32 {
    let far = GRID_FAR.min(pe_world::preset::PLANE_SIDE / 2.0 - 1.0);
    (((far + GRID_AT[2]) / step).floor() as u32 + 1).max(1)
}

/// The jobs' queue and their last status.
pub struct Spawner {
    tx: Mutex<Sender<Job>>,
    state: Arc<Mutex<State>>,
    /// Set by [`Spawner::stop`]: the worker takes no further job, queued or not.
    stop: Arc<AtomicBool>,
    worker: Mutex<Option<std::thread::JoinHandle<()>>>,
    /// The world's limits now, as the console's `limit` says them (a read, on the caller's thread).
    limits: Box<dyn Fn() -> String + Send + Sync>,
}

#[derive(Default)]
struct State {
    /// `/Spawned` has been found in this world. Never reset: it is the plane's root, which no edit deletes.
    available: bool,
    /// The last job's result, for the panel and the console.
    status: Option<String>,
    /// Jobs queued and not yet done.
    pending: usize,
    /// Console spawns so far, for their row.
    console: u32,
    /// Counted console spawns so far, for their grid; back to 0 at a Clear.
    grid: u32,
}

/// The current session's spawner: a slot, since each session has its own over its own pe-api (app.md 4.1).
static INSTALLED: Mutex<Option<Arc<Spawner>>> = Mutex::new(None);

/// The session's spawner over `api`, as `member`: a worker thread, and the handle the menu and the console use. It
/// takes the place of any spawner installed before, which the caller has stopped ([`uninstall_if`]).
pub fn install(api: Arc<dyn WorldApi>, member: MemberKey) -> Option<Arc<Spawner>> {
    let s = Spawner::start(api, member)?;
    *INSTALLED.lock().unwrap_or_else(|e| e.into_inner()) = Some(s.clone());
    Some(s)
}

/// Takes the installed spawner out only if it is `mine`: a session ending leaves another session's in place. The app
/// runs one session at a time, so the slot is the ending one's; in tests two sessions share one process (an opener and
/// a joiner on loopback), and the later one's install replaced the earlier's. `true` if `mine` was the installed one.
pub fn uninstall_if(mine: &Arc<Spawner>) -> bool {
    let mut slot = INSTALLED.lock().unwrap_or_else(|e| e.into_inner());
    if slot.as_ref().is_some_and(|s| Arc::ptr_eq(s, mine)) {
        *slot = None;
        true
    } else {
        false
    }
}

impl Spawner {
    /// A spawner and its worker, not installed (for a test, or a second world).
    pub fn start(api: Arc<dyn WorldApi>, member: MemberKey) -> Option<Arc<Spawner>> {
        let (tx, rx) = channel();
        let state = Arc::new(Mutex::new(State::default()));
        let stop = Arc::new(AtomicBool::new(false));
        let caller = person(member);
        let limits: Box<dyn Fn() -> String + Send + Sync> = {
            let (api, caller) = (api.clone(), caller.clone());
            Box::new(move || limits_now(&*api, &caller))
        };
        let (st, halt) = (state.clone(), stop.clone());
        let started =
            std::thread::Builder::new().name("pe-spawn".into()).spawn(move || work(&*api, &caller, &rx, &st, &halt));
        let worker = match started {
            Ok(h) => h,
            Err(e) => {
                crate::err!("pe-app: the spawn buttons won't work: their thread didn't start ({e})");
                return None;
            }
        };
        Some(Arc::new(Spawner { tx: Mutex::new(tx), state, stop, worker: Mutex::new(Some(worker)), limits }))
    }

    /// Stops the worker (a session ending): it takes no further job, a counted spawn's remaining ones included, and
    /// the job it is on ends within pe-api's wait for it. Waits up to `within` for the thread; `false` if it was left
    /// behind (it then ends by itself, its pe-api refusing as the session closes).
    pub fn stop(&self, within: Duration) -> bool {
        self.stop.store(true, Ordering::Release);
        // A dead sender in place of the queue's, so a worker waiting for a job wakes and ends.
        let (dead, _) = channel();
        drop(std::mem::replace(&mut *self.tx.lock().unwrap_or_else(|e| e.into_inner()), dead));
        let Some(h) = self.worker.lock().unwrap_or_else(|e| e.into_inner()).take() else { return true };
        let until = Instant::now() + within;
        while !h.is_finished() {
            if Instant::now() >= until {
                return false;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let _ = h.join();
        true
    }
}

/// The spawner [`install`] made for the current session, if any.
pub fn installed() -> Option<Arc<Spawner>> {
    INSTALLED.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

impl Spawner {
    /// Queue a job; the worker checks for `/Spawned` before it. False if the worker has stopped.
    pub fn ask(&self, job: Job) -> bool {
        if self.tx.lock().unwrap_or_else(|e| e.into_inner()).send(job.clone()).is_err() {
            self.state.lock().unwrap_or_else(|e| e.into_inner()).status = Some("The spawn thread has stopped".into());
            return false;
        }
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        st.pending += 1;
        let queued = if st.pending > 1 { format!(" ({} queued)", st.pending) } else { String::new() };
        st.status = Some(match &job {
            Job::Spawn(k, _) => format!("{}: spawning…{queued}", k.template()),
            Job::Clear => format!("Clearing…{queued}"),
            Job::Limit(n) => format!("Shape limit {n}: setting…{queued}"),
        });
        true
    }

    /// Whether the panel should show: `/Spawned` has been found.
    pub fn available(&self) -> bool {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).available
    }

    /// The last job's result.
    pub fn status(&self) -> Option<String> {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).status.clone()
    }

    /// The console's `limit` (the world's shape limit) and `limit shapes <n>` (a change to it, through the worker).
    pub fn on_limit(&self, args: &str) -> Result<String, String> {
        match parse_limit(args)? {
            Some(n) => {
                if self.ask(Job::Limit(n)) {
                    Ok(format!("limit shapes {n}: queued"))
                } else {
                    Err(self.status().unwrap_or_default())
                }
            }
            None => Ok((self.limits)()),
        }
    }

    /// The console's `spawn box|sphere|capsule|stack|clear` (@engine-architect), or `spawn <kind>
    /// <count>` for that many on the grid; `args` follows the word.
    pub fn on_command(&self, args: &str) -> Result<String, String> {
        let what = args.trim();
        let (jobs, said) = match parse_command(what)? {
            Command::Clear => {
                self.state.lock().unwrap_or_else(|e| e.into_inner()).grid = 0;
                (vec![Job::Clear], format!("spawn {what}: queued"))
            }
            Command::One(k) => {
                let n = {
                    let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
                    st.console += 1;
                    st.console - 1
                };
                (vec![Job::Spawn(k, console_at(k, n))], format!("spawn {what}: queued"))
            }
            Command::Many(k, count) => {
                let first = {
                    let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
                    st.grid += count;
                    st.grid - count
                };
                let jobs = (first..first + count).map(|n| Job::Spawn(k, grid_at(k, n))).collect();
                (jobs, format!("spawn {what}: queued {count} on the grid"))
            }
        };
        for job in jobs {
            if !self.ask(job) {
                return Err(self.status().unwrap_or_default());
            }
        }
        Ok(said)
    }
}

/// What the console's `spawn` was asked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    Clear,
    /// One, in the console's row.
    One(Kind),
    /// That many, 1 to [`COUNT_MAX`], on the grid.
    Many(Kind, u32),
}

/// The console's `spawn` arguments: `clear`, a kind, or a kind and a count.
pub fn parse_command(what: &str) -> Result<Command, String> {
    let words: Vec<&str> = what.split_whitespace().collect();
    let kind = |w: &str| Kind::parse(w).ok_or_else(|| format!("`spawn {what}`: say {} or clear", words_list()));
    match words[..] {
        ["clear"] => Ok(Command::Clear),
        [w] => Ok(Command::One(kind(w)?)),
        [w, n] => {
            let k = kind(w)?;
            match n.parse::<u32>() {
                Ok(c) if k.floor_half().is_some() && c > FLOOR_COUNT_MAX => Err(format!(
                    "`spawn {what}`: at most {FLOOR_COUNT_MAX} of a big kind at once, so they stay on the floor"
                )),
                Ok(c) if (1..=COUNT_MAX).contains(&c) => Ok(Command::Many(k, c)),
                _ => Err(format!("`spawn {what}`: a count is a whole number from 1 to {COUNT_MAX}")),
            }
        }
        _ => Err(format!("`spawn {what}`: say {} [count] or clear", words_list())),
    }
}

/// The kinds' console words, comma-separated.
fn words_list() -> String {
    Kind::ALL.map(Kind::word).join(", ")
}

/// The counted spawn's `n`-th place: [`GRID_ACROSS`] to a row from [`GRID_AT`], [`GRID_STEP`] apart, rows going
/// away from the origin; a stack on the floor.
pub fn grid_at(k: Kind, n: u32) -> [f64; 3] {
    let step = k.spacing();
    // A floor kind's rows are as wide as the small ones' (GRID_ACROSS × GRID_STEP), so fewer to a row; a layer
    // keeps to the clear ground (`grid_rows`), and the next layer goes above it.
    let across = ((f64::from(GRID_ACROSS) * GRID_STEP / step) as u32).max(1);
    let rows = grid_rows(step);
    let x = GRID_AT[0] + step * f64::from(n % across);
    let z = GRID_AT[2] - step * f64::from((n / across) % rows);
    let layer = f64::from(n / (across * rows));
    let y = match k.on_floor() {
        Some(y) => y + k.layer_height() * layer,
        None => GRID_AT[1] + GRID_STEP * layer,
    };
    [x, y, z]
}

/// A counted stack's layer height: a template stack stands about 0.77 m (five 0.15 m boxes and their gaps), so the
/// next layer starts clear of it and settles on top rather than inside (@design-reviewer's L1).
pub const STACK_LAYER: f64 = 0.8;

/// The console's `limit [shapes <n>]`: the world's shape limit now, or a change to it (an edit, agreed by every
/// member). `args` follows the word.
pub fn parse_limit(args: &str) -> Result<Option<u32>, String> {
    let words: Vec<&str> = args.split_whitespace().collect();
    match words[..] {
        [] => Ok(None),
        ["shapes", n] => match n.parse::<u32>() {
            Ok(v) if (1..=pe_world::components::SHAPES_CEILING).contains(&v) => Ok(Some(v)),
            _ => Err(format!("`limit shapes {n}`: a whole number from 1 to {}", pe_world::components::SHAPES_CEILING)),
        },
        _ => Err("say `limit` to see the world's limits, or `limit shapes <n>` to change the shape limit".into()),
    }
}

/// A shape-limit change's transaction: `/World settings`' `Limits.max_shapes`.
pub fn limit_tx(n: u32) -> Transaction {
    serde_json::from_value(json!({"label": format!("Shape limit {n}"), "ops": [{
        "op": "set", "id": SETTINGS, "field": "Limits.max_shapes", "value": n
    }]}))
    .expect("a limit change is a transaction")
}

/// The root that carries the world's `Limits` row (pe-world `preset::plane_names::SETTINGS`).
pub const SETTINGS: &str = "/World settings";

/// The world's shape limit and shape count now, for the console.
fn limits_now(api: &dyn WorldApi, c: &Caller) -> String {
    let q: Query = serde_json::from_value(json!({"where": {"path": SETTINGS}, "fields": ["Limits"]})).expect("a query");
    let shown = api
        .query(c, &q)
        .ok()
        .and_then(|r| serde_json::to_value(&r).ok())
        .and_then(|v| v["objects"][0]["components"]["Limits"]["max_shapes"].as_u64());
    match shown {
        Some(n) => format!("the world's shape limit is {n}"),
        None => format!("this world has no Limits row: its shape limit is the build's, {}", pe_world::edit::MAX_SHAPES),
    }
}

/// The console's `n`-th spawn's place: a row of five in front of the origin, a stack on the floor.
pub fn console_at(k: Kind, n: u32) -> [f64; 3] {
    if k.floor_half().is_some() {
        // A floor kind goes 1.5 m further out than the small ones' rows, its width apart, so it never lands on them.
        // Its places repeat every 9 (3 by 3): the 10th of a kind lands on the 1st, fine for a test session.
        let step = k.spacing();
        let z = CONSOLE_AT[2] - 1.5 - step * (1.0 + f64::from((n / 3) % 3));
        return [CONSOLE_AT[0] + step * f64::from(n % 3), k.on_floor().unwrap_or(0.0), z];
    }
    let x = CONSOLE_AT[0] + 0.3 * f64::from(n % 5);
    let z = CONSOLE_AT[2] - 0.4 * f64::from((n / 5) % 4);
    [x, k.on_floor().unwrap_or(CONSOLE_AT[1]), z]
}

/// The most one counted `spawn` of a floor kind (heavy or mid-weight) queues: four rows of the widest (the platform,
/// three to a row, 2.5 m apart) from [`GRID_AT`] reach 10.5 m ahead, inside the plane's floor (@xr-eng's L1 on
/// a306b59c).
pub const FLOOR_COUNT_MAX: u32 = 12;

/// A stack's root above the floor's top (at the origin's height), so its bottom box starts just clear of it.
pub const FLOOR_GAP: f64 = 0.01;

/// How close a spawn may appear to either hand: a body made overlapping a kinematic hand is pushed out hard
/// (@design-reviewer's L1 on S4).
pub const HAND_CLEAR_M: f64 = 0.25;

/// A button's place, as an offset from the world's origin: 0.5 m ahead of the head along its level forward and
/// 0.2 m below it (never under 0.3 m above the floor), 0.25 m left of centre, since the hand aims at the spawn panel
/// on the right. If a hand (`hands`, offsets from the origin) is within [`HAND_CLEAR_M`], the next spot further away
/// is tried: further left, then further ahead, then to the right. A stack stands on the floor 0.8 m ahead.
///
/// The floor's top is the session's origin, and the world's origin is `Pos::ORIGIN` [I: every pe-app session opens
/// at `Pos::ORIGIN` today (@world-model's L1 on S4)]; a session elsewhere would need `/Plane`'s position read here.
pub fn in_front(head: [f64; 3], forward: [f64; 3], k: Kind, hands: &[[f64; 3]]) -> [f64; 3] {
    let (fx, fz) = (forward[0], forward[2]);
    let len = (fx * fx + fz * fz).sqrt();
    let (fx, fz) = if len > 1e-6 { (fx / len, fz / len) } else { (0.0, -1.0) };
    // Left of the level forward: for forward -Z, left is -X.
    let (lx, lz) = (fz, -fx);
    // A floor kind stands on the floor far enough ahead that its near side clears the player's body (0.25 m around the
    // head's spot over the floor) by 0.2 m.
    let (ahead0, y) = match (k, k.floor_half()) {
        (Kind::Stack, _) => (0.8, FLOOR_GAP),
        // The copy is axis-aligned, so its reach back toward the player along the level forward is hx|fx| + hz|fz|.
        (_, Some(h)) => {
            (0.45 + f64::from(h[0]) * fx.abs() + f64::from(h[2]) * fz.abs(), k.on_floor().unwrap_or(FLOOR_GAP))
        }
        _ => (0.5, (head[1] - 0.2).max(0.3)),
    };
    let spot = |ahead: f64, left: f64| [head[0] + fx * ahead + lx * left, y, head[2] + fz * ahead + lz * left];
    // A floor kind is big, so a hand is measured to its surface (a point-to-box distance), a small thing's to its
    // centre.
    let half = k.floor_half().map_or([0.0; 3], |h| h.map(f64::from));
    let clear = |p: &[f64; 3]| {
        hands.iter().all(|h| {
            let d: f64 = (0..3).map(|i| ((h[i] - p[i]).abs() - half[i]).max(0.0).powi(2)).sum();
            d.sqrt() > HAND_CLEAR_M
        })
    };
    let tries = [(ahead0, 0.25), (ahead0, 0.55), (ahead0 + 0.3, 0.25), (ahead0 + 0.3, 0.0), (ahead0, -0.55)];
    tries.iter().map(|&(a, l)| spot(a, l)).find(clear).unwrap_or_else(|| spot(ahead0 + 0.6, 0.0))
}

/// This machine's member as a person at the controls (the producer a hand, as the editor's would be).
fn person(member: MemberKey) -> Caller {
    Caller {
        connection: CONNECTION,
        principal: "person".into(),
        member: member.into(),
        scopes: [Scope::Read, Scope::Edit].into_iter().collect(),
        producer: Producer { kind: ProducerKind::Hand, harness: None, principal: None, requested_by: None },
        confirm: false,
    }
}

/// A spawn's transaction: one `create` from the template under `/Spawned`, its root placed from the world's origin.
pub fn spawn_tx(k: Kind, at: [f64; 3]) -> Transaction {
    let t = k.template();
    serde_json::from_value(json!({"label": format!("Spawn {t}"), "ops": [{
        "op": "create", "template": t, "name": t, "parent": SPAWNED, "as": "$s",
        "components": {"Transform": {"position": {"relative_to": "world_origin", "offset": at}}}
    }]}))
    .expect("a spawn is a transaction")
}

/// A clear's transaction: a delete for each id.
pub fn clear_tx(ids: &[String]) -> Transaction {
    let ops: Vec<Value> = ids.iter().map(|id| json!({"op": "delete", "id": id})).collect();
    serde_json::from_value(json!({"label": "Clear spawned", "ops": ops})).expect("a clear is a transaction")
}

fn work(api: &dyn WorldApi, c: &Caller, rx: &Receiver<Job>, state: &Mutex<State>, stop: &AtomicBool) {
    use std::sync::mpsc::RecvTimeoutError;
    let mut found = false;
    let mut told = false;
    // Look for `/Spawned`; true once found. The first miss is said once on the console.
    let mut look = |found: &mut bool| {
        if !*found {
            match has_spawned(api, c) {
                Ok(()) => {
                    *found = true;
                    state.lock().unwrap_or_else(|e| e.into_inner()).available = true;
                    if told {
                        crate::out!("spawn: the spawn buttons are on ({SPAWNED} is here now)");
                    }
                }
                Err(e) if !told => {
                    told = true;
                    crate::out!("spawn: no spawn buttons yet ({e}); looking again every {LOOK_AGAIN:?}");
                }
                Err(_) => {}
            }
        }
        *found
    };
    loop {
        if stop.load(Ordering::Acquire) {
            return;
        }
        look(&mut found);
        let job = if found {
            match rx.recv() {
                Ok(j) => j,
                Err(_) => return,
            }
        } else {
            match rx.recv_timeout(LOOK_AGAIN) {
                Ok(j) => j,
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => return,
            }
        };
        // Checked again before the job: queued jobs (a counted spawn's) aren't run once the session ends (L4).
        if stop.load(Ordering::Acquire) {
            return;
        }
        let said = if !look(&mut found) {
            "This world has no Spawned folder: spawning needs the plane (--preset plane)".to_string()
        } else {
            match &job {
                Job::Spawn(k, at) => said(k.template(), paced(api, c, &spawn_tx(*k, *at), &ApplyOpts::default())),
                Job::Clear => clear(api, c),
                Job::Limit(n) => match api.apply(c, limit_tx(*n), &ApplyOpts::default()) {
                    Ok(o) if matches!(o.status, Status::Accepted | Status::Final) => {
                        format!("Shape limit {n}: set (every member switches at this change's frame)")
                    }
                    r => said(&format!("Shape limit {n}"), r),
                },
            }
        };
        crate::out!("spawn: {said}");
        let mut st = state.lock().unwrap_or_else(|e| e.into_inner());
        st.pending = st.pending.saturating_sub(1);
        st.status = Some(said);
    }
}

/// Whether this world has `/Spawned` (the plane), or why not.
fn has_spawned(api: &dyn WorldApi, c: &Caller) -> Result<(), String> {
    let q: Query = serde_json::from_value(json!({"where": {"path": SPAWNED}})).expect("a query");
    match api.query(c, &q) {
        Ok(r) if !r.objects.is_empty() => Ok(()),
        Ok(_) => Err(format!("no {SPAWNED}: spawning needs the plane (--preset plane)")),
        Err(e) => Err(format!("{e:?}")),
    }
}

/// `/Spawned`'s children's ids, one page.
fn children(api: &dyn WorldApi, c: &Caller) -> Result<Vec<String>, String> {
    let q: Query = serde_json::from_value(json!({"where": {"child_of": SPAWNED}, "limit": 256})).expect("a query");
    let r = api.query(c, &q).map_err(|e| format!("{e:?}"))?;
    let v = serde_json::to_value(&r).map_err(|e| e.to_string())?;
    Ok(v["objects"]
        .as_array()
        .map_or_else(Vec::new, |o| o.iter().filter_map(|o| o["id"].as_str().map(str::to_string)).collect()))
}

/// Delete `/Spawned`'s children: everyone's, as built (app.md 4.2). A batch refused because another player holds
/// something in it (pe-world refuses that delete, `Held`; the author's own holds end with the delete) is redone one
/// object at a time, and the held ones are left (@world-model's M1 on S4).
fn clear(api: &dyn WorldApi, c: &Caller) -> String {
    clear_with(api, c, &ApplyOpts::default(), CLEAR_ROUNDS)
}

/// [`clear`] with `opts` and at most `rounds` rounds (a test's dry run takes one: nothing is removed between rounds).
fn clear_with(api: &dyn WorldApi, c: &Caller, opts: &ApplyOpts, rounds: usize) -> String {
    let (mut removed, mut held) = (0, std::collections::BTreeSet::new());
    let accepted = |r: &Result<Outcome, pe_seams::world_api::ApiError>| matches!(r, Ok(o) if matches!(o.status, Status::Accepted | Status::Final));
    let is_held = |r: &Result<Outcome, pe_seams::world_api::ApiError>| matches!(r, Ok(Outcome { refusal: Some(f), .. }) if f.code == RefusalCode::Held);
    for _ in 0..rounds {
        let ids: Vec<String> = match children(api, c) {
            Ok(ids) => ids.into_iter().filter(|id| !held.contains(id)).collect(),
            Err(e) => return format!("Clear: couldn't read Spawned ({e})"),
        };
        if ids.is_empty() {
            break;
        }
        for batch in ids.chunks(CLEAR_BATCH) {
            let r = paced(api, c, &clear_tx(batch), opts);
            if accepted(&r) {
                removed += batch.len();
            } else if is_held(&r) {
                for id in batch {
                    let one = paced(api, c, &clear_tx(std::slice::from_ref(id)), opts);
                    if accepted(&one) {
                        removed += 1;
                    } else if is_held(&one) {
                        held.insert(id.clone());
                    } else {
                        return said("Clear", one);
                    }
                }
            } else {
                return said("Clear", r);
            }
        }
    }
    match (removed, held.len()) {
        (0, 0) => "Nothing to clear".into(),
        (n, 0) => format!("Cleared {n}"),
        (n, k) => format!("Cleared {n}, {k} held"),
    }
}

/// `api.apply`, asked again after each `rate_limited` answer once its `retry_after_ms` (at most [`RATE_WAIT_MAX`]) has
/// passed, for up to [`RATE_PATIENCE`]; any other answer, or the last `rate_limited`, is returned as it came.
fn paced(
    api: &dyn WorldApi,
    c: &Caller,
    tx: &Transaction,
    opts: &ApplyOpts,
) -> Result<Outcome, pe_seams::world_api::ApiError> {
    let give_up = std::time::Instant::now() + RATE_PATIENCE;
    loop {
        match api.apply(c, tx.clone(), opts) {
            Err(e) if e.code == pe_seams::world_api::ErrorCode::RateLimited && std::time::Instant::now() < give_up => {
                let ms = e.details.as_ref().and_then(|d| d["retry_after_ms"].as_u64()).unwrap_or(50);
                std::thread::sleep(std::time::Duration::from_millis(ms.max(1)).min(RATE_WAIT_MAX));
            }
            r => return r,
        }
    }
}

/// A one-line result for the panel.
fn said(what: &str, r: Result<Outcome, pe_seams::world_api::ApiError>) -> String {
    match r {
        Ok(o) => match o.status {
            Status::Accepted | Status::Final => format!("{what}: spawned"),
            Status::Queued => format!("{what}: queued"),
            _ => match o.refusal {
                Some(f) => format!("{what}: refused ({:?}: {})", f.code, f.message),
                None => format!("{what}: {:?}", o.status),
            },
        },
        Err(e) => format!("{what}: failed ({e:?})"),
    }
}

/// The slot is one per process, and the lib's tests open sessions in parallel, each installing its spawner there
/// (session.rs, vr.rs): a test that opens a session holds this for reading, and a test that checks the slot is empty
/// at its end holds it for writing.
#[cfg(test)]
pub(crate) mod test_slot {
    use std::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard};

    static SLOT: RwLock<()> = RwLock::new(());

    pub(crate) fn opens_sessions() -> RwLockReadGuard<'static, ()> {
        SLOT.read().unwrap_or_else(|e| e.into_inner())
    }

    pub(crate) fn checks_the_slot() -> RwLockWriteGuard<'static, ()> {
        SLOT.write().unwrap_or_else(|e| e.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_spawn_is_one_create_from_the_template_under_spawned() {
        let v = serde_json::to_value(spawn_tx(Kind::Stack, [0.5, 0.01, -0.8])).unwrap();
        let op = &v["ops"][0];
        assert_eq!(op["op"], "create");
        assert_eq!((op["template"].clone(), op["name"].clone()), (json!("Box stack"), json!("Box stack")));
        assert_eq!(op["parent"], SPAWNED);
        let p = &op["components"]["Transform"]["position"];
        assert_eq!((p["relative_to"].clone(), p["offset"].clone()), (json!("world_origin"), json!([0.5, 0.01, -0.8])));
    }

    #[test]
    fn a_clear_deletes_each_id() {
        let v = serde_json::to_value(clear_tx(&["a".into(), "b".into()])).unwrap();
        let ops = v["ops"].as_array().unwrap();
        assert_eq!(ops.len(), 2);
        assert_eq!((ops[1]["op"].clone(), ops[1]["id"].clone()), (json!("delete"), json!("b")));
    }

    #[test]
    fn things_appear_in_front_of_the_head_and_a_stack_on_the_floor() {
        let at = in_front([0.0, 1.6, 0.0], [0.0, -0.5, -1.0], Kind::Box, &[]);
        assert!((at[0] + 0.25).abs() < 1e-9, "left of centre: {at:?}");
        assert!((at[2] + 0.5).abs() < 1e-9 && (at[1] - 1.4).abs() < 1e-9, "{at:?}");
        let s = in_front([1.0, 1.6, 0.0], [1.0, 0.0, 0.0], Kind::Stack, &[]);
        assert!(
            (s[0] - 1.8).abs() < 1e-9 && s[1] == FLOOR_GAP && (s[2] + 0.25).abs() < 1e-9,
            "facing +X, left is -Z: {s:?}"
        );
        let low = in_front([0.0, 0.2, 0.0], [0.0, 0.0, -1.0], Kind::Sphere, &[]);
        assert_eq!(low[1], 0.3, "never below the floor");
        let up = in_front([0.0, 1.6, 0.0], [0.0, 1.0, 0.0], Kind::Box, &[]);
        assert!((up[0] + 0.25).abs() < 1e-9 && (up[2] + 0.5).abs() < 1e-9, "looking straight up: ahead is -Z");
    }

    /// The floor kinds (the heavy and mid-weight templates) stand on the floor, their near side clear of the player's
    /// body, and a grid or a row of them never overlaps.
    #[test]
    fn floor_things_stand_on_the_floor_clear_of_the_player() {
        for k in [Kind::Sack, Kind::Barrel, Kind::LowCrate, Kind::BigCrate, Kind::Platform, Kind::Slab] {
            let h = k.floor_half().expect("a floor kind");
            assert_eq!(Kind::parse(k.word()), Some(k));
            let at = in_front([0.0, 1.6, 0.0], [0.0, 0.0, -1.0], k, &[]);
            assert!((at[1] - (f64::from(h[1]) + FLOOR_GAP)).abs() < 1e-9, "{k:?} on the floor: {at:?}");
            let near = -at[2] - f64::from(h[2]);
            assert!(near >= 0.25 + 0.2 - 1e-9, "{k:?}'s near side is {near} m ahead");
            let (a, b) = (grid_at(k, 0), grid_at(k, 1));
            assert!((b[0] - a[0]).abs() >= 2.0 * f64::from(h[0]) + 0.5 - 1e-9, "{k:?} grid {a:?} {b:?}");
            let (c, d) = (console_at(k, 0), console_at(k, 1));
            assert!((d[0] - c[0]).abs() >= 2.0 * f64::from(h[0]) + 0.5 - 1e-9 && c[1] == a[1], "{k:?} row {c:?} {d:?}");
        }
        assert!(parse_command("nonsense").unwrap_err().contains("crate, platform, slab"));
        // Facing diagonally the axis-aligned platform's near corner still clears the body (@xr-eng's M1).
        let r = std::f64::consts::FRAC_1_SQRT_2;
        let at = in_front([0.0, 1.6, 0.0], [r, 0.0, -r], Kind::Platform, &[]);
        let h = Kind::Platform.floor_half().unwrap().map(f64::from);
        let corner = (at[0] - h[0]).max(0.0).hypot((-at[2] - h[2]).max(0.0));
        assert!(corner >= 0.25 + 0.2 - 1e-9 || (at[0] - h[0]) * r + (-at[2] - h[2]) * r >= 0.45 - 1e-9, "{at:?}");
        // A hand in front at chest height moves the crate on to a spot clear of it (@xr-eng's M2).
        let hand = [0.0, 1.0, -0.6];
        let c = in_front([0.0, 1.6, 0.0], [0.0, 0.0, -1.0], Kind::BigCrate, &[hand]);
        let hc = Kind::BigCrate.floor_half().unwrap().map(f64::from);
        let d: f64 = (0..3).map(|i| ((hand[i] - c[i]).abs() - hc[i]).max(0.0).powi(2)).sum();
        assert!(d.sqrt() > HAND_CLEAR_M, "the crate at {c:?} is within reach of the hand");
        // A floor-kind count stays on the floor, and the console's fourth crate isn't inside its first (L1, L2).
        assert!(parse_command(&format!("crate {}", FLOOR_COUNT_MAX + 1)).is_err());
        assert!(parse_command(&format!("crate {FLOOR_COUNT_MAX}")).is_ok());
        let last = grid_at(Kind::Platform, FLOOR_COUNT_MAX - 1);
        assert!(-last[2] < 11.0, "{last:?}");
        assert_ne!(console_at(Kind::BigCrate, 0), console_at(Kind::BigCrate, 3));
    }

    /// The plane at frame 4 with `me` an editor, behind pe-app's own ports (no simulation: dry runs and reads).
    fn plane_api(me: MemberKey, plane: bool) -> Arc<dyn WorldApi> {
        let line = pe_seams::LineId(pe_seams::Hash([7; 32]));
        let blobs = pe_world::BlobStore::default();
        let mut w = if plane {
            pe_world::preset::plane_on(line, 5, blobs, pe_seams::Pos::ORIGIN)
        } else {
            pe_world::preset::ball_pit_with(5, blobs, pe_seams::Pos::ORIGIN, 8)
        };
        w.set_member(me, pe_world::perm::EDIT | pe_world::perm::ADMIN);
        w.set_frame(pe_seams::Frame(4));
        let facts =
            crate::api::Facts { world_name: "Plane".into(), line, rate: pe_seams::SimRate::DEFAULT, me: Some(me) };
        crate::api::Ports::new(w.snapshot(pe_seams::Frame(4)), facts, || {}).api()
    }

    /// The console's `limit` reads the plane's `Limits` row, and `limit shapes <n>` is an edit pe-api accepts from an
    /// admin and refuses from anyone else (the friend test 11-27; world-model.md 7.1).
    #[test]
    fn the_shape_limit_is_read_and_changed_through_the_api() {
        let me = MemberKey([1; 32]);
        let (api, c) = (plane_api(me, true), person(me));
        assert_eq!(limits_now(&*api, &c), format!("the world's shape limit is {}", pe_world::edit::MAX_SHAPES));
        let dry = ApplyOpts { mode: pe_seams::world_api::Mode::DryRun, ..Default::default() };
        let o = api.apply(&c, limit_tx(600), &dry).expect("an answer");
        assert_eq!(o.status, Status::Accepted, "{:?}", o.refusal);
        // A member without admin.
        let guest = MemberKey([2; 32]);
        let line = pe_seams::LineId(pe_seams::Hash([7; 32]));
        let mut w = pe_world::preset::plane_on(line, 5, pe_world::BlobStore::default(), pe_seams::Pos::ORIGIN);
        w.set_member(guest, pe_world::perm::EDIT);
        w.set_frame(pe_seams::Frame(4));
        let facts =
            crate::api::Facts { world_name: "Plane".into(), line, rate: pe_seams::SimRate::DEFAULT, me: Some(guest) };
        let theirs = crate::api::Ports::new(w.snapshot(pe_seams::Frame(4)), facts, || {}).api();
        let o = theirs.apply(&person(guest), limit_tx(600), &dry).expect("an answer");
        assert_eq!(o.status, Status::Refused);
        assert_eq!(
            parse_limit(" shapes 0"),
            Err(format!("`limit shapes 0`: a whole number from 1 to {}", pe_world::components::SHAPES_CEILING))
        );
        assert_eq!((parse_limit(""), parse_limit(" shapes 700")), (Ok(None), Ok(Some(700))));
    }

    #[test]
    fn every_spawn_is_accepted_on_the_plane_and_the_pit_has_no_spawned() {
        let me = MemberKey([1; 32]);
        let (api, c) = (plane_api(me, true), person(me));
        assert_eq!(children(&*api, &c), Ok(vec![]), "the plane starts with nothing spawned");
        let dry = ApplyOpts { mode: pe_seams::world_api::Mode::DryRun, ..Default::default() };
        for k in Kind::ALL {
            let at = in_front([0.0, 1.6, 0.0], [0.0, 0.0, -1.0], k, &[]);
            let o = api.apply(&c, spawn_tx(k, at), &dry).expect("an answer");
            assert_eq!(o.status, Status::Accepted, "{k:?}: {:?}", o.refusal);
            assert!(o.created.contains_key("$s"), "{k:?}");
        }
        assert_eq!(has_spawned(&*api, &c), Ok(()));
        let pit = plane_api(me, false);
        assert!(has_spawned(&*pit, &c).is_err(), "no Spawned in the ball pit");
    }

    #[test]
    fn a_joiner_whose_world_arrives_after_the_start_gets_the_spawn_panel() {
        // The joiner's ports open on an empty world; the plane comes with the cut's state, a little later.
        let me = MemberKey([1; 32]);
        let line = pe_seams::LineId(pe_seams::Hash([7; 32]));
        let mut empty = pe_world::World::new(5, pe_world::BlobStore::default());
        empty.set_frame(pe_seams::Frame(0));
        let facts =
            crate::api::Facts { world_name: "Plane".into(), line, rate: pe_seams::SimRate::DEFAULT, me: Some(me) };
        let ports = crate::api::Ports::new(empty.snapshot(pe_seams::Frame(0)), facts, || {});
        let s = Spawner::start(ports.api(), me).expect("the worker");
        std::thread::sleep(LOOK_AGAIN * 2);
        assert!(!s.available(), "no Spawned in the empty world");
        let mut w = pe_world::preset::plane_on(line, 5, pe_world::BlobStore::default(), pe_seams::Pos::ORIGIN);
        w.set_member(me, pe_world::perm::EDIT | pe_world::perm::ADMIN);
        w.set_frame(pe_seams::Frame(4));
        ports.shared.keep(w.snapshot(pe_seams::Frame(4)));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !s.available() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(s.available(), "the panel comes up once the plane is here");
    }

    /// A session's end (app.md 4.1; design d3's L4): with pe-api's ports closed first, as the session's stop does,
    /// the spawn worker stops within its bound: the job it is on is answered at once, and the counted spawn's other
    /// jobs, queued behind it, are never run. The menu and the console then see no spawner.
    #[test]
    fn a_stopped_spawner_runs_none_of_its_queued_jobs_and_ends_at_once() {
        let me = MemberKey([1; 32]);
        let line = pe_seams::LineId(pe_seams::Hash([7; 32]));
        let mut w = pe_world::preset::plane_on(line, 5, pe_world::BlobStore::default(), pe_seams::Pos::ORIGIN);
        w.set_member(me, pe_world::perm::EDIT | pe_world::perm::ADMIN);
        w.set_frame(pe_seams::Frame(4));
        let facts =
            crate::api::Facts { world_name: "Plane".into(), line, rate: pe_seams::SimRate::DEFAULT, me: Some(me) };
        // No simulation drains these ports; the worker still runs jobs at the change rate, so some are left at the stop.
        let ports = crate::api::Ports::new(w.snapshot(pe_seams::Frame(4)), facts, || {});
        // Not installed: the process's slot is the session tests' too, and they run in parallel.
        let s = Spawner::start(ports.api(), me).expect("the worker");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !s.available() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(s.available());
        assert!(s.on_command(" box 100").is_ok());
        std::thread::sleep(std::time::Duration::from_millis(200));
        let asked = std::time::Instant::now();
        ports.close();
        assert!(s.stop(Duration::from_secs(6)), "joined within the bound");
        assert!(asked.elapsed() < Duration::from_secs(2), "at once: {:?}", asked.elapsed());
        let left = s.state.lock().unwrap().pending;
        assert!(left > 0, "the stop came before the counted spawn ended");
        std::thread::sleep(std::time::Duration::from_millis(300));
        assert_eq!(s.state.lock().unwrap().pending, left, "no queued job ran after the stop");
        assert!(!s.ask(Job::Clear), "a stopped spawner takes no job");
    }

    #[test]
    fn a_clear_leaves_what_the_other_player_holds_and_removes_the_rest() {
        // Two members on the plane (@coordinator-opus's ruling (c)): A spawned three boxes, B holds the
        // second. A's clear removes the other two and leaves the held one. pe-world decides the hold (apply.rs, `Held`),
        // through pe-api's dry run on the same snapshot, so one round stands for the whole clear.
        use pe_seams::test_util::{EntryBuilder, TestKeys};
        use pe_world::edit::{Actor, Op, Target};
        use pe_world::preset::{Preset, Session, find, m1_session, plane_names as n};
        let keys = TestKeys::new(3, 2);
        let (ka, kb) = (keys.key(0), keys.key(1));
        let line = pe_seams::LineId(pe_seams::Hash([7; 32]));
        let s = Session {
            seed: 5,
            origin: pe_seams::Pos::ORIGIN,
            preset: Preset::Plane,
            line,
            rate: pe_seams::SimRate::DEFAULT,
            gravity: pe_world::GRAVITY,
            agree: b"settings".to_vec(),
            members: vec![(ka, pe_world::perm::ADMIN | pe_world::perm::EDIT), (kb, pe_world::perm::EDIT)],
        };
        let mut w = m1_session(&s, pe_world::BlobStore::default()).expect("the plane");
        let (template, spawned, plane) =
            (find(&w, n::BOX).unwrap(), find(&w, n::SPAWNED).unwrap(), find(&w, n::ROOT).unwrap());
        let mut author = EntryBuilder::on_line(&keys, 0, line);
        let mut copies = Vec::new();
        for f in 1..=3 {
            let copy = Op::Copy {
                source: Target::Existing(template),
                new: 0,
                parent: Target::Existing(spawned),
                index: None,
                size: 1,
            };
            let t = pe_world::edit::Transaction {
                inline: vec![],
                actor: Actor::Person,
                label: None,
                preconditions: vec![],
                ops: vec![copy],
                blobs: vec![],
            };
            let e = author.next(pe_seams::Class::Edit, pe_seams::Frame(f), bytes::Bytes::from(t.encode()));
            match w.apply_edit(pe_seams::Frame(f), &e).0 {
                pe_seams::Outcome::Applied { created, .. } => copies.push(created[0]),
                r => panic!("the copy was refused: {r:?}"),
            }
        }
        let claim =
            pe_world::Claim { hand: plane, member: kb, since: pe_seams::Frame(0), until: pe_seams::Frame(u64::MAX) };
        w.set_claim(copies[1], claim).unwrap();
        w.set_frame(pe_seams::Frame(4));
        let facts =
            crate::api::Facts { world_name: "Plane".into(), line, rate: pe_seams::SimRate::DEFAULT, me: Some(ka) };
        let api = crate::api::Ports::new(w.snapshot(pe_seams::Frame(4)), facts, || {}).api();
        let (a, dry) = (person(ka), ApplyOpts { mode: pe_seams::world_api::Mode::DryRun, ..Default::default() });
        assert_eq!(children(&*api, &a).unwrap().len(), 3);
        assert_eq!(clear_with(&*api, &a, &dry, 1), "Cleared 2, 1 held");
        // Unheld, all three go in one batch.
        w.set_claim(copies[1], pe_world::Claim { member: ka, ..claim }).unwrap();
        let facts =
            crate::api::Facts { world_name: "Plane".into(), line, rate: pe_seams::SimRate::DEFAULT, me: Some(ka) };
        let api = crate::api::Ports::new(w.snapshot(pe_seams::Frame(4)), facts, || {}).api();
        assert_eq!(clear_with(&*api, &a, &dry, 1), "Cleared 3", "A's own hold ends with the delete");
    }

    #[test]
    fn a_spawn_never_appears_at_a_hand() {
        let d =
            |a: [f64; 3], b: [f64; 3]| ((a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2) + (a[2] - b[2]).powi(2)).sqrt();
        let (head, fwd) = ([0.0, 1.6, 0.0], [0.0, 0.0, -1.0]);
        let first = in_front(head, fwd, Kind::Box, &[]);
        // A hand right where the first spot is: the next spot, further left, and clear of it.
        let moved = in_front(head, fwd, Kind::Box, &[first]);
        assert!(d(moved, first) > HAND_CLEAR_M && moved[0] < first[0], "{moved:?}");
        // Both hands across the left spots: further ahead.
        let hands = [first, in_front(head, fwd, Kind::Box, &[first])];
        let ahead = in_front(head, fwd, Kind::Box, &hands);
        assert!(hands.iter().all(|&h| d(ahead, h) > HAND_CLEAR_M), "{ahead:?}");
        // A hand far off changes nothing.
        assert_eq!(in_front(head, fwd, Kind::Box, &[[3.0, 1.0, 3.0]]), first);
    }

    #[test]
    fn the_console_words_and_rows() {
        assert_eq!(Kind::ALL.map(|k| Kind::parse(k.word())), Kind::ALL.map(Some));
        assert_eq!(Kind::parse("ball"), None);
        assert_eq!(console_at(Kind::Box, 0), [-0.6, 0.8, -1.0]);
        assert_eq!(console_at(Kind::Stack, 6)[1], FLOOR_GAP);
        assert!((console_at(Kind::Box, 6)[2] + 1.4).abs() < 1e-9, "the second row");
    }

    /// `spawn <kind> <count>` (K4's R5', the shape limit): that many on a grid of their own, every place its own, past
    /// the single spawns' rows; a count from 1 to the world's shape limit.
    #[test]
    fn a_counted_spawn_fills_a_grid_of_its_own() {
        assert_eq!(parse_command("clear"), Ok(Command::Clear));
        assert_eq!(parse_command("stack"), Ok(Command::One(Kind::Stack)));
        assert_eq!(parse_command("stack 102"), Ok(Command::Many(Kind::Stack, 102)));
        assert_eq!(parse_command("box  52000"), Ok(Command::Many(Kind::Box, COUNT_MAX)));
        for bad in ["stack 0", "stack 52001", "stack x", "stack -3", "clear 3", "stack 2 3", "ball 4", ""] {
            assert!(parse_command(bad).is_err(), "`{bad}` refused");
        }
        assert_eq!(grid_at(Kind::Box, 0), GRID_AT);
        assert_eq!(grid_at(Kind::Stack, 0)[1], FLOOR_GAP);
        // 102 stacks (510 boxes, the plane's room under 520 shapes): every place apart from the others, and from the
        // single spawns' rows (z -1.0 to -2.2).
        let at: Vec<[f64; 3]> = (0..102).map(|n| grid_at(Kind::Stack, n)).collect();
        for (i, a) in at.iter().enumerate() {
            assert!(a[2] <= -3.0 + 1e-9, "past the rows: {a:?}");
            for b in &at[i + 1..] {
                let d = ((a[0] - b[0]).powi(2) + (a[2] - b[2]).powi(2)).sqrt();
                assert!(d >= GRID_STEP - 1e-9, "{a:?} and {b:?} {d} m apart");
            }
        }
        assert!((grid_at(Kind::Box, GRID_ACROSS)[2] - (GRID_AT[2] - GRID_STEP)).abs() < 1e-9, "the second row");
        // A full layer on, the same place a layer up: a stack clear of the one below (@design-reviewer's L1).
        let layer = GRID_ACROSS * grid_rows(GRID_STEP);
        let (low, high) = (grid_at(Kind::Stack, 0), grid_at(Kind::Stack, layer));
        assert_eq!((low[0], low[2]), (high[0], high[2]));
        assert!((high[1] - low[1] - STACK_LAYER).abs() < 1e-9, "{low:?} {high:?}");
        assert!((grid_at(Kind::Box, layer)[1] - GRID_AT[1] - GRID_STEP).abs() < 1e-9);
        // Every place a count allows is on the plane's floor, a metre in from its edge, carried over or not
        // (@xr-eng's L1: 10 to a row went off the floor past 240).
        let edge = pe_world::preset::PLANE_SIDE / 2.0 - 1.0;
        for n in 0..COUNT_MAX {
            let [x, _, z] = grid_at(Kind::Box, n);
            assert!(x.abs() <= edge && z.abs() <= edge, "place {n} at {x}, {z}: off the floor");
        }
        // The grid's last place is on the plane: the world takes a stack there.
        let me = MemberKey([1; 32]);
        let (api, c) = (plane_api(me, true), person(me));
        let dry = ApplyOpts { mode: pe_seams::world_api::Mode::DryRun, ..Default::default() };
        let o = api.apply(&c, spawn_tx(Kind::Stack, at[101]), &dry).expect("an answer");
        assert_eq!(o.status, Status::Accepted, "{:?}", o.refusal);
    }

    /// pe-api limits changes per second (api.md 9.3), dry runs too: 60 spawns asked back to back,
    /// as a counted spawn's jobs come, are each taken once the rate allows, never refused `rate_limited` (K4's R5',
    /// where 43 of 102 were).
    #[test]
    fn spawns_asked_faster_than_the_change_rate_wait_their_turn() {
        let me = MemberKey([1; 32]);
        let (api, c) = (plane_api(me, true), person(me));
        let dry = ApplyOpts { mode: pe_seams::world_api::Mode::DryRun, ..Default::default() };
        let tx = spawn_tx(Kind::Box, grid_at(Kind::Box, 0));
        for i in 0..60 {
            let o = paced(&*api, &c, &tx, &dry).unwrap_or_else(|e| panic!("spawn {i}: {e:?}"));
            assert_eq!(o.status, Status::Accepted, "spawn {i}: {:?}", o.refusal);
        }
        // Since the friend test 11-27 the bucket is 2,000 a second with a burst of 4,000 (pe-api rate.rs), which a
        // test asking back to back can't spend; pe-api's own test (the_rate_limit_applies_per_principal) shows the
        // bucket refusing at a lower rate.
    }
}
