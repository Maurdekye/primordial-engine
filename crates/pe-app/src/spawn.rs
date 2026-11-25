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
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex, OnceLock};

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
}

impl Kind {
    pub const ALL: [Kind; 4] = [Kind::Box, Kind::Sphere, Kind::Capsule, Kind::Stack];

    /// The template's name, which is also the copy's.
    pub fn template(self) -> &'static str {
        use pe_world::preset::plane_names as n;
        match self {
            Kind::Box => n::BOX,
            Kind::Sphere => n::SPHERE,
            Kind::Capsule => n::CAPSULE,
            Kind::Stack => n::STACK,
        }
    }

    /// The console's word for it.
    pub fn word(self) -> &'static str {
        match self {
            Kind::Box => "box",
            Kind::Sphere => "sphere",
            Kind::Capsule => "capsule",
            Kind::Stack => "stack",
        }
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
}

/// How often the worker looks for `/Spawned` while it hasn't found it.
pub const LOOK_AGAIN: std::time::Duration = std::time::Duration::from_millis(500);
/// The parent spawns go under (pe-world `preset::plane_names::SPAWNED`).
pub const SPAWNED: &str = "/Spawned";
/// Deletes per transaction while clearing, and the most rounds a clear takes.
const CLEAR_BATCH: usize = 32;
const CLEAR_ROUNDS: usize = 16;
/// The in-process caller's connection: far from pe-mcp's, which count up from 1.
const CONNECTION: ConnectionId = ConnectionId(u64::MAX - 7);
/// Where the console's `spawn` puts a thing: 1 m in front of the origin, 0.8 m up (a stack on the floor), each one
/// 0.3 m to the right of the last, five to a row.
const CONSOLE_AT: [f64; 3] = [-0.6, 0.8, -1.0];

/// Where the console's counted `spawn <kind> <count>` starts its grid: 3 m in front of the origin, past the single
/// spawns' rows, 0.8 m up (a stack on the floor), [`GRID_ACROSS`] to a row [`GRID_STEP`] apart, each row
/// [`GRID_STEP`] further ahead. A stack's boxes are 0.15 m across, so one that falls reaches its neighbours.
pub const GRID_AT: [f64; 3] = [-1.35, 0.8, -3.0];
pub const GRID_STEP: f64 = 0.3;
pub const GRID_ACROSS: u32 = 10;
/// The most one counted `spawn` queues: the world's shape limit (pe-world `MAX_SHAPES`, world-model.md 7.1), past
/// which every spawn is refused anyway.
pub const COUNT_MAX: u32 = 520;

/// The jobs' queue and their last status.
pub struct Spawner {
    tx: Mutex<Sender<Job>>,
    state: Arc<Mutex<State>>,
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

static INSTALLED: OnceLock<Arc<Spawner>> = OnceLock::new();

/// The process's spawner over `api`, as `member`: a worker thread, and the handle the menu and the console use.
pub fn install(api: Arc<dyn WorldApi>, member: MemberKey) -> Option<Arc<Spawner>> {
    let s = Spawner::start(api, member)?;
    let _ = INSTALLED.set(s.clone());
    Some(s)
}

impl Spawner {
    /// A spawner and its worker, not installed (for a test, or a second world).
    pub fn start(api: Arc<dyn WorldApi>, member: MemberKey) -> Option<Arc<Spawner>> {
        let (tx, rx) = channel();
        let state = Arc::new(Mutex::new(State::default()));
        let s = Arc::new(Spawner { tx: Mutex::new(tx), state: state.clone() });
        let caller = person(member);
        let started =
            std::thread::Builder::new().name("pe-spawn".into()).spawn(move || work(&*api, &caller, &rx, &state));
        if let Err(e) = started {
            crate::err!("pe-app: the spawn buttons won't work: their thread didn't start ({e})");
            return None;
        }
        Some(s)
    }
}

/// The spawner [`install`] made, if any.
pub fn installed() -> Option<Arc<Spawner>> {
    INSTALLED.get().cloned()
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
    let kind =
        |w: &str| Kind::parse(w).ok_or_else(|| format!("`spawn {what}`: say box, sphere, capsule, stack or clear"));
    match words[..] {
        ["clear"] => Ok(Command::Clear),
        [w] => Ok(Command::One(kind(w)?)),
        [w, n] => {
            let k = kind(w)?;
            match n.parse::<u32>() {
                Ok(c) if (1..=COUNT_MAX).contains(&c) => Ok(Command::Many(k, c)),
                _ => Err(format!("`spawn {what}`: a count is a whole number from 1 to {COUNT_MAX}")),
            }
        }
        _ => Err(format!("`spawn {what}`: say box, sphere, capsule, stack [count] or clear")),
    }
}

/// The counted spawn's `n`-th place: [`GRID_ACROSS`] to a row from [`GRID_AT`], [`GRID_STEP`] apart, rows going
/// away from the origin; a stack on the floor.
pub fn grid_at(k: Kind, n: u32) -> [f64; 3] {
    let x = GRID_AT[0] + GRID_STEP * f64::from(n % GRID_ACROSS);
    let z = GRID_AT[2] - GRID_STEP * f64::from(n / GRID_ACROSS);
    [x, if k == Kind::Stack { FLOOR_GAP } else { GRID_AT[1] }, z]
}

/// The console's `n`-th spawn's place: a row of five in front of the origin, a stack on the floor.
pub fn console_at(k: Kind, n: u32) -> [f64; 3] {
    let x = CONSOLE_AT[0] + 0.3 * f64::from(n % 5);
    let z = CONSOLE_AT[2] - 0.4 * f64::from((n / 5) % 4);
    [x, if k == Kind::Stack { FLOOR_GAP } else { CONSOLE_AT[1] }, z]
}

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
    let (ahead0, y) = if k == Kind::Stack { (0.8, FLOOR_GAP) } else { (0.5, (head[1] - 0.2).max(0.3)) };
    let spot = |ahead: f64, left: f64| [head[0] + fx * ahead + lx * left, y, head[2] + fz * ahead + lz * left];
    let clear = |p: &[f64; 3]| {
        hands
            .iter()
            .all(|h| ((h[0] - p[0]).powi(2) + (h[1] - p[1]).powi(2) + (h[2] - p[2]).powi(2)).sqrt() > HAND_CLEAR_M)
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

fn work(api: &dyn WorldApi, c: &Caller, rx: &Receiver<Job>, state: &Mutex<State>) {
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
        let said = if !look(&mut found) {
            "This world has no Spawned folder: spawning needs the plane (--preset plane)".to_string()
        } else {
            match &job {
                Job::Spawn(k, at) => said(k.template(), api.apply(c, spawn_tx(*k, *at), &ApplyOpts::default())),
                Job::Clear => clear(api, c),
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
            let r = api.apply(c, clear_tx(batch), opts);
            if accepted(&r) {
                removed += batch.len();
            } else if is_held(&r) {
                for id in batch {
                    let one = api.apply(c, clear_tx(std::slice::from_ref(id)), opts);
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
        assert_eq!(parse_command("box  520"), Ok(Command::Many(Kind::Box, COUNT_MAX)));
        for bad in ["stack 0", "stack 521", "stack x", "stack -3", "clear 3", "stack 2 3", "ball 4", ""] {
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
        assert!((grid_at(Kind::Box, 10)[2] - (GRID_AT[2] - GRID_STEP)).abs() < 1e-9, "the second row");
        // The grid's last place is on the plane: the world takes a stack there.
        let me = MemberKey([1; 32]);
        let (api, c) = (plane_api(me, true), person(me));
        let dry = ApplyOpts { mode: pe_seams::world_api::Mode::DryRun, ..Default::default() };
        let o = api.apply(&c, spawn_tx(Kind::Stack, at[101]), &dry).expect("an answer");
        assert_eq!(o.status, Status::Accepted, "{:?}", o.refusal);
    }
}
