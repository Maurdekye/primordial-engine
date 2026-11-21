//! The MCP tools over the real `pe_api::api::Api` and a pe-world snapshot, not the mock: the path an agent takes in
//! M1 (api.md 6, 9.2). A thread stands in for pe-app's simulation thread: it drains the edit queue, settles the
//! entry agreement would give, and records the step in the change log.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pe_api::api::{Api, SessionFacts, WorldSource};
use pe_api::changes::ChangeLog;
use pe_api::queue::{EditQueue, Receipt};
use pe_core::{ChainId, EntryRef, Frame, Hash, LineId, MemberKey, Offset, Pos};
use pe_mcp::auth::{Grant, Principal};
use pe_mcp::server::{Auth, WorldServer};
use pe_seams::world_api::{Link, MemberId, ObjectId, Producer, ProducerKind, WorldApi};
use pe_sim::Transform;
use pe_world::components::Name;
use pe_world::edit::Transaction;
use pe_world::{BlobStore, Draft, World, WorldSnapshot, perm};
use rmcp::model::{CallToolRequestParams, CallToolResult};
use rmcp::service::RunningService;
use rmcp::{RoleClient, ServiceExt};
use serde_json::{Value, json};

const ALICE: MemberKey = MemberKey([1; 32]);

fn id(n: u128) -> pe_core::ObjectId {
    pe_core::ObjectId::from_u128(n)
}

/// pe-app's side, for one published frame: a Table with a Lamp on it; Alice is an admin.
struct Src {
    snap: Arc<WorldSnapshot>,
    edits: Mutex<HashMap<EntryRef, Transaction>>,
}

impl WorldSource for Src {
    fn latest(&self) -> Arc<WorldSnapshot> {
        self.snap.clone()
    }
    fn final_(&self) -> Arc<WorldSnapshot> {
        self.snap.clone()
    }
    fn at(&self, frame: u64) -> Option<Arc<WorldSnapshot>> {
        (frame == self.snap.frame().0).then(|| self.snap.clone())
    }
    fn head_of(&self, _: MemberId) -> Option<ObjectId> {
        None
    }
    fn draft_for(&self, member: MemberId) -> Draft {
        Draft { author: member.into(), line: LineId(Hash([7; 32])), chain: ChainId(Hash([8; 32])), seq: 1 }
    }
    fn put_blobs(&self, _: &[Vec<u8>]) {}
    fn edit(&self, entry: EntryRef) -> Option<std::sync::Arc<Transaction>> {
        self.edits.lock().unwrap().get(&entry).cloned().map(std::sync::Arc::new)
    }
    fn member_name(&self, m: MemberId) -> Option<String> {
        (m == ALICE.into()).then(|| "Alice".into())
    }
    fn session(&self) -> SessionFacts {
        SessionFacts {
            world_name: "Kitchen".into(),
            line: LineId(Hash([7; 32])),
            here: vec![ALICE.into()],
            link: Link::Fine,
            caught_up: true,
            tick_hz: 60,
            agents_move_user: false,
            scripts_not_running: vec![],
            joining: false,
        }
    }
}

fn world() -> Arc<WorldSnapshot> {
    let mut w = World::new(1, BlobStore::default());
    w.set_member(ALICE, perm::EDIT | perm::ADMIN);
    for (n, name, parent) in [(10u128, "Table", None), (20, "Lamp", Some(10))] {
        let text = w.blobs().put(name.as_bytes());
        let v = pe_seams::Version { entry: EntryRef(Hash([n as u8; 32])), op: 0 };
        w.create(id(n), parent.map(id), ALICE, v).unwrap();
        let t = match parent {
            None => Transform::at(Pos::ORIGIN.checked_add(Offset([1.0, 0.0, 0.0])).unwrap()),
            Some(_) => Transform::local([0.0, 1.0, 0.0]),
        };
        w.insert(id(n), t).unwrap();
        w.insert(id(n), Name { text }).unwrap();
    }
    w.set_frame(Frame(4));
    w.snapshot(Frame(4))
}

type Client = RunningService<RoleClient, ()>;

async fn call(c: &Client, name: &str, args: Value) -> CallToolResult {
    let mut p = CallToolRequestParams::new(name.to_string());
    if let Value::Object(m) = args {
        p = p.with_arguments(m);
    }
    c.call_tool(p).await.unwrap_or_else(|e| panic!("{name}: transport error {e}"))
}

async fn ok(c: &Client, name: &str, args: Value) -> Value {
    let r = call(c, name, args.clone()).await;
    assert_ne!(r.is_error, Some(true), "{name} {args} failed: {:?}", r.structured_content);
    r.structured_content.unwrap_or_else(|| panic!("{name}: no structured content"))
}

#[tokio::test(flavor = "multi_thread")]
async fn an_agent_reads_edits_and_watches_a_pe_world_through_mcp() {
    let queue = Arc::new(EditQueue::new(16, || {}));
    let log = Arc::new(ChangeLog::new());
    let src = Src { snap: world(), edits: Mutex::new(HashMap::new()) };
    let api: Arc<dyn WorldApi> = Arc::new(Api::new(src, queue.clone(), log.clone()));
    let grant = Grant {
        member: ALICE.into(),
        producer: Producer {
            kind: ProducerKind::Agent,
            harness: Some("claude-code".into()),
            principal: None,
            requested_by: None,
        },
        confirm: false,
    };
    let (s, c) = tokio::io::duplex(1 << 20);
    let server = WorldServer::new(api, Auth::Fixed(Principal::local("agent", &pe_mcp::auth::ALL_SCOPES, grant)));
    tokio::spawn(async move {
        if let Ok(r) = server.serve(s).await {
            let _ = r.waiting().await;
        }
    });
    let c: Client = ().serve(c).await.expect("client handshake");

    // Who and where.
    let s = ok(&c, "session_info", json!({})).await;
    assert_eq!(
        (s["world"]["name"].clone(), s["me"]["role"].clone(), s["kept"].clone()),
        (json!("Kitchen"), json!("admin"), json!(true))
    );
    // Read.
    let q = ok(&c, "world_query", json!({"where": {"name": "Lamp"}})).await;
    assert_eq!(q["objects"][0]["name"], "Lamp", "{q}");
    let schema = ok(&c, "world_schema", json!({})).await;
    assert!(schema["types"].as_array().unwrap().iter().any(|t| t["name"] == "Transform"), "{schema}");
    // A dry run changes nothing and names what it would create.
    let cup = json!([{"op": "create", "as": "$cup", "parent": "/Table", "name": "Cup"}]);
    let d = ok(&c, "world_apply", json!({"label": "a cup", "mode": "dry_run", "ops": cup})).await;
    assert_eq!((d["dry_run"].clone(), d["status"].clone()), (json!(true), json!("accepted")), "{d}");
    assert!(d["created"]["$cup"]["id"].is_string(), "{d}");
    assert!(queue.is_empty());
    // Live: the stand-in simulation thread takes it, agreement names it, and the step is recorded.
    let start = ok(&c, "world_watch", json!({})).await["cursor"].clone();
    let (q2, log2) = (queue.clone(), log.clone());
    let entry = EntryRef(Hash([3; 32]));
    let sim = std::thread::spawn(move || {
        for _ in 0..400 {
            if let Some((t, _)) = q2.drain(1).pop() {
                q2.settle(
                    t,
                    Receipt::Submitted { change: entry.into(), frame: 5, chain: ChainId(Hash([1; 32])), seq: 1 },
                );
                let outcome = pe_seams::Outcome::Applied {
                    created: vec![id(99)],
                    overwrote: vec![],
                    notes: vec![],
                    inverse: None,
                };
                log2.record(
                    &[pe_seams::StepRecord {
                        entry,
                        author: ALICE,
                        class: pe_seams::Class::Edit,
                        frame: Frame(5),
                        outcome,
                    }],
                    None,
                );
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    });
    let o = ok(&c, "world_apply", json!({"label": "a cup", "ops": cup})).await;
    sim.join().unwrap();
    assert_eq!((o["status"].clone(), o["frame"].clone()), (json!("accepted"), json!(5)), "{o}");
    // The agent's own change comes back through watch.
    let w = ok(&c, "world_watch", json!({"cursor": start})).await;
    let mine: Vec<_> = w["mine"].as_array().unwrap().iter().map(|o| o["status"].clone()).collect();
    assert_eq!(mine, vec![json!("queued"), json!("accepted")], "{w}");
    // And in history.
    let h = ok(&c, "history_list", json!({})).await;
    assert_eq!(h["entries"].as_array().unwrap().len(), 1, "{h}");
}
