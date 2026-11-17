//! api.md 14's agent demo script, the CI form (app.md 6 has the two-process run): two members over the in-process
//! network of sim.rs's tests, each machine with pe-api's ports. An agent on B speaks MCP to pe-mcp's `WorldServer` over
//! B's `WorldApi`, as `pe-app --mcp-pipe` serves it. A person on A edits through A's `WorldApi`. Each step is checked
//! by a query on both machines.
//!
//! The test thread ticks both machines on hub time; the agent runs on its own thread, so its calls wait as an agent's
//! do while the simulation steps. Every wait is bounded in hub frames.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use pe_agree::{Node, NodeConfig};
use pe_mcp::auth::{ALL_SCOPES, Grant, Principal};
use pe_mcp::server::{Auth, WorldServer};
use pe_seams::agree::{Cut, CutRef};
use pe_seams::test_util::TestKeys;
use pe_seams::world_api::{
    ApplyOpts, Caller, ConnectionId, MemberId, Producer, ProducerKind, Query, Transaction, Wait, WorldApi,
};
use pe_seams::{Frame, FrameClock, HostTime, MemberKey, Pos};
use rmcp::model::{CallToolRequestParams, CallToolResult};
use rmcp::service::RunningService;
use rmcp::{RoleClient, ServiceExt};
use serde_json::{Value, json};

use crate::clock::LocalClock;
use crate::sim::tests::{Hub, Port, RATE, StillHands, runner};
use crate::sim::{Budget, Sim};
use pe_render::present::FrameSlot;

/// The longest any wait may take: both this many hub frames (20 s of simulated time) and [`STEP_WALL`] must pass
/// before it fails, so a loaded machine that starves the agent's thread while the hub runs on fails nothing.
const STEP_FRAMES: u64 = 20 * 60;
const STEP_WALL: Duration = Duration::from_secs(30);
/// The whole script's bound, past which `run` stops ticking and fails with the step it had reached.
const SCRIPT_WALL: Duration = Duration::from_secs(300);

/// The hub's present frame, which the ticking thread advances and the agent's waits read, and the step the agent
/// is at, for a failure's message.
#[derive(Clone, Default)]
struct HubFrame(Arc<AtomicU64>, Arc<std::sync::Mutex<String>>);

impl HubFrame {
    fn now(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }

    /// Notes the step the agent is at.
    fn step(&self, what: &str) {
        *self.1.lock().expect("the step") = what.to_string();
    }

    /// Waits until `done` is `Ok`, failing with `what` and the last thing `done` saw once both [`STEP_FRAMES`] hub
    /// frames and [`STEP_WALL`] have passed.
    fn until(&self, what: &str, mut done: impl FnMut() -> Result<(), String>) {
        self.step(what);
        let (end, wall) = (self.now() + STEP_FRAMES, Instant::now() + STEP_WALL);
        while let Err(saw) = done() {
            assert!(
                self.now() < end || Instant::now() < wall,
                "{what}: not within {STEP_FRAMES} hub frames and {STEP_WALL:?}; last saw {saw}"
            );
            thread::sleep(Duration::from_millis(1));
        }
    }
}

/// `Ok` if `got` is `want`, else what was got.
fn is(got: Value, want: Value) -> Result<(), String> {
    if got == want { Ok(()) } else { Err(got.to_string()) }
}

/// A person on `member`'s machine, as the editor calls pe-api.
fn person(member: MemberKey) -> Caller {
    Caller {
        connection: ConnectionId(1),
        principal: "person".into(),
        member: member.into(),
        scopes: ALL_SCOPES.iter().copied().collect(),
        producer: Producer { kind: ProducerKind::Hand, harness: None, principal: None, requested_by: None },
        confirm: false,
    }
}

/// One query on `api` as `c`, as JSON.
fn query(api: &dyn WorldApi, c: &Caller, q: Value) -> Value {
    let q: Query = serde_json::from_value(q).expect("a query");
    serde_json::to_value(api.query(c, &q).expect("the query")).expect("serializable")
}

/// The objects a query on `api` finds, by name.
fn names(api: &dyn WorldApi, c: &Caller, select: Value) -> Vec<String> {
    let r = query(api, c, json!({"where": select}));
    let mut n: Vec<String> =
        r["objects"].as_array().expect("objects").iter().map(|o| o["name"].as_str().unwrap_or("").into()).collect();
    n.sort();
    n
}

/// One field of the object `id` names, read on `api`.
fn field(api: &dyn WorldApi, c: &Caller, id: &str, component: &str, name: &str) -> Value {
    let r = query(api, c, json!({"where": {"id": id}, "fields": [format!("{component}.{name}")]}));
    r["objects"][0]["components"][component][name].clone()
}

type Client = RunningService<RoleClient, ()>;

async fn call(c: &Client, name: &str, args: Value) -> CallToolResult {
    let mut p = CallToolRequestParams::new(name.to_string());
    if let Value::Object(m) = args {
        p = p.with_arguments(m);
    }
    c.call_tool(p).await.unwrap_or_else(|e| panic!("{name}: transport error {e}"))
}

/// A tool call's structured result, success or not.
async fn tool(c: &Client, name: &str, args: Value) -> Value {
    call(c, name, args).await.structured_content.unwrap_or_else(|| panic!("{name}: no structured content"))
}

/// The two machines: A at index 0, B at 1, both admins of one ball pit (pe-app's `world::ball_pit`).
struct Machines {
    sims: Vec<Sim>,
    apis: Vec<Arc<dyn WorldApi>>,
    members: [MemberKey; 2],
    hub: Hub,
    clock: Arc<dyn FrameClock>,
}

fn machines() -> Machines {
    let clock: Arc<dyn FrameClock> = Arc::new(LocalClock::new(HostTime(0), RATE));
    let keys = TestKeys::new(17, 2);
    let members = keys.sorted_keys();
    let line = pe_world::preset::m1_line();
    let hub = Hub::default();
    let (mut sims, mut apis) = (Vec::new(), Vec::new());
    for (i, feed) in [[-0.5f32, 1.0, -0.4], [0.5, 1.1, -0.3]].into_iter().enumerate() {
        let me = keys.key(i);
        let genesis = Cut { at: CutRef { line, height: 0, frame: Frame(0) }, heads: Vec::new() };
        let node = Node::new(
            NodeConfig::new(line, members.clone(), genesis),
            Box::new(keys.signer(i)),
            Box::new(keys.clone()),
            HostTime(0),
        );
        let world = pe_world::preset::ball_pit_with(5, pe_world::BlobStore::default(), Pos::ORIGIN, 8);
        let replica = crate::world::ball_pit(world, &members, me, RATE, clock.clone(), runner()).expect("a replica");
        let sim = Sim::new(Box::new(node), replica, clock.clone(), Arc::new(FrameSlot::new()), Budget::new(RATE))
            .with_local(me, keys.nonce(i, 0), Arc::new(StillHands(feed)));
        let ports = sim.ports("Pit", || {}).expect("pe-api's ports");
        apis.push(ports.api());
        sims.push(sim.with_api(ports).with_transport(Box::new(Port::new(me, hub.clone())), None));
    }
    Machines { sims, apis, members: [keys.key(0), keys.key(1)], hub, clock }
}

/// Runs `script` on its own thread while this one ticks both machines on hub time, until the script returns; past
/// [`SCRIPT_WALL`], fails with the step the script had reached (a call that never returns).
fn run(m: &mut Machines, frame: HubFrame, script: impl FnOnce() + Send + 'static) {
    let script = thread::spawn(script);
    let mut f = frame.now();
    let wall = Instant::now() + SCRIPT_WALL;
    while !script.is_finished() {
        assert!(
            Instant::now() < wall,
            "the script didn't finish within {SCRIPT_WALL:?}, at hub frame {f}; last step: {}",
            frame.1.lock().expect("the step")
        );
        f += 1;
        let now = m.clock.host_time_of(Frame(f));
        m.hub.now.store(now.0, Ordering::SeqCst);
        for s in &mut m.sims {
            s.tick(now).expect("ticked");
        }
        frame.0.store(f, Ordering::SeqCst);
        // Let the agent's thread and pe-mcp's runtime in between frames.
        thread::sleep(Duration::from_micros(500));
    }
    if let Err(e) = script.join() {
        std::panic::resume_unwind(e);
    }
}

/// api.md 14: the agent puts a red cube in the pit, makes the balls blue, doubles the cube and removes the lamp. Its
/// first delete reads the lamp before a person on A changes it, and is refused as `stale_version` naming A's change.
/// Every step is seen on both machines.
#[test]
fn the_agent_demo_script_runs_against_two_machines() {
    let mut m = machines();
    let (a, b) = (m.apis[0].clone(), m.apis[1].clone());
    let [ma, mb] = m.members;
    let (pa, pb) = (person(ma), person(mb));
    let frame = HubFrame::default();
    let hub_frame = frame.clone();
    run(&mut m, frame, move || {
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().expect("a runtime");
        rt.block_on(async move {
            let grant = Grant {
                member: MemberId::from(mb),
                producer: Producer {
                    kind: ProducerKind::Agent,
                    harness: Some("demo".into()),
                    principal: None,
                    requested_by: None,
                },
                confirm: false,
            };
            let (s, c) = tokio::io::duplex(1 << 20);
            let server = WorldServer::new(b.clone(), Auth::Fixed(Principal::local("agent", &ALL_SCOPES, grant)));
            tokio::spawn(async move {
                if let Ok(r) = server.serve(s).await {
                    let _ = r.waiting().await;
                }
            });
            let agent: Client = ().serve(c).await.expect("the client's handshake");
            let both = [(&a, &pa, "A"), (&b, &pb, "B")];
            let seen = |what: &str, check: &dyn Fn(&dyn WorldApi, &Caller) -> Result<(), String>| {
                for (api, c, who) in both {
                    hub_frame.until(&format!("{what} on {who}"), || check(&**api, c));
                }
            };

            // Where things are.
            let pit = tool(&agent, "world_query", json!({"where": {"name": "Ball pit"}})).await;
            let pit = pit["objects"][0]["id"].as_str().expect("the pit's id").to_string();

            hub_frame.step("1: the red cube");
            // 1. "Put a red cube in the pit."
            let o = tool(
                &agent,
                "world_apply",
                json!({"label": "Put a red cube in the pit", "wait": "final", "ops": [
                    {"op": "create", "as": "$cube", "parent": pit, "name": "Red cube", "components": {
                        "Transform": {"position": {"relative_to": "parent", "offset": [0, 0.5, 0]}},
                        "Shape": {"kind": 1, "density": 500, "size": [0.1, 0.1, 0.1]},
                        "Appearance": {"base": [0.65, 0.03, 0.03, 1.0]}}}
                ]}),
            )
            .await;
            assert_eq!(o["status"], "final", "{o}");
            let cube = o["created"]["$cube"]["id"].as_str().expect("the cube's id").to_string();
            seen("the red cube", &|api, c| is(field(api, c, &cube, "Appearance", "base"), json!([0.65, 0.03, 0.03, 1.0])));

            hub_frame.step("2: the blue balls");
            // 2. "Make the balls blue."
            let balls = json!({"all": [{"name_like": "Ball "}, {"has": "Body"}]});
            let o = tool(
                &agent,
                "world_apply",
                json!({"label": "Make the balls blue", "wait": "final", "ops": [
                    {"op": "set", "where": balls, "max": 100, "field": "Appearance.base", "value": [0.02, 0.1, 0.65, 1.0]}
                ]}),
            )
            .await;
            assert_eq!((o["status"].clone(), o["count"].clone()), (json!("final"), json!(8)), "{o}");
            seen("eight blue balls", &|api, c| {
                let r = query(api, c, json!({"where": balls, "fields": ["Appearance.base"]}));
                let objs = r["objects"].as_array().expect("objects");
                let blue = objs.iter().filter(|o| o["components"]["Appearance"]["base"] == json!([0.02, 0.1, 0.65, 1.0]));
                is(json!(blue.count()), json!(8)).map_err(|_| r.to_string())
            });

            hub_frame.step("3: doubling the cube");
            // 3. "Double the cube."
            let o = tool(
                &agent,
                "world_apply",
                json!({"label": "Double the cube", "wait": "final", "ops": [
                    {"op": "duplicate", "id": cube, "as": "$copy"},
                    {"op": "set", "id": "$copy", "field": "Transform.position",
                     "value": {"relative_to": cube, "offset": [0.3, 0, 0]}}
                ]}),
            )
            .await;
            assert_eq!(o["status"], "final", "{o}");
            let copy = o["created"]["$copy"]["id"].as_str().expect("the copy's id").to_string();
            seen("the copy, red", &|api, c| is(field(api, c, &copy, "Appearance", "base"), json!([0.65, 0.03, 0.03, 1.0])));
            // By name on both machines: a created object's name rides inside its entry (world-model.md 3.4).
            seen("two red cubes, by name", &|api, c| is(json!(names(api, c, json!({"name": "Red cube"})).len()), json!(2)));

            hub_frame.step("4: the stale case");
            // 4. The stale case: the agent reads the lamp, a person on A dims it, and the agent's delete of what it
            // read is refused, naming A's change.
            let lamp = tool(&agent, "world_query", json!({"where": {"name": "Lamp"}})).await;
            let (lamp_id, read) = (
                lamp["objects"][0]["id"].as_str().expect("the lamp's id").to_string(),
                lamp["objects"][0]["version"].clone(),
            );
            let dim: Transaction = serde_json::from_value(json!({"label": "Dim the lamp", "ops": [
                {"op": "set", "id": lamp_id, "field": "Light.intensity", "value": 0.25}
            ]}))
            .expect("a transaction");
            let o = a.apply(&pa, dim, &ApplyOpts { wait: Wait::Final, ..Default::default() }).expect("A's apply");
            assert!(o.status == pe_seams::world_api::Status::Final && o.is_final, "{o:?}");
            seen("the dimmed lamp", &|api, c| is(field(api, c, &lamp_id, "Light", "intensity"), json!(0.25)));
            let o = tool(
                &agent,
                "world_apply",
                json!({"label": "Remove the lamp", "wait": "final", "ops": [
                    {"op": "delete", "id": lamp_id, "if_version": read}
                ]}),
            )
            .await;
            // Over MCP a refusal is the error envelope (api.md 6.3): code and message on top, the rest in details.
            assert_eq!((o["code"].clone(), o["details"]["status"].clone()), (json!("stale_version"), json!("refused")), "{o}");
            let winner = &o["details"]["winner"];
            assert_eq!(winner["by"]["member"], json!(MemberId::from(ma)), "the refusal names A's change: {o}");
            assert_eq!((winner["label"].clone(), winner["producer"]["kind"].clone()), (json!("Dim the lamp"), json!("hand")), "{o}");

            hub_frame.step("5: removing the lamp");
            // 5. "Remove the lamp", after reading it again.
            let now = tool(&agent, "world_query", json!({"where": {"id": lamp_id}})).await["objects"][0]["version"].clone();
            let o = tool(
                &agent,
                "world_apply",
                json!({"label": "Remove the lamp", "wait": "final", "ops": [
                    {"op": "delete", "id": lamp_id, "if_version": now}
                ]}),
            )
            .await;
            assert_eq!(o["status"], "final", "{o}");
            seen("no lamp", &|api, c| is(json!(names(api, c, json!({"name": "Lamp"}))), json!([])));

            // The agent's changes are in its history, labelled as an agent's.
            let h = tool(&agent, "history_list", json!({"mine": true})).await;
            let labels: Vec<_> = h["entries"].as_array().expect("entries").iter().map(|e| e["label"].clone()).collect();
            for l in ["Put a red cube in the pit", "Make the balls blue", "Double the cube", "Remove the lamp"] {
                assert!(labels.contains(&json!(l)), "{l:?} in {labels:?}");
            }
            agent.cancel().await.expect("the client closes");
        });
    });
}
