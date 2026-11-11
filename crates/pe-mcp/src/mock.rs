//! A small, fixed, in-memory WorldApi for pe-mcp's own tests and for trying a harness against the tool surface
//! (`pe-mcp serve-mock`). It is not a world: no simulation, no agreement, no rules beyond what the tests need.
//!
//! - Objects: a root `Table` with two lamps under it, and `balls` balls.
//! - `apply`: accepted, unless the transaction's label is `"refuse"` (then `stale_version` with a winner) or an op
//!   deletes a lamp named "Lamp (right)" (then `protected`). `dry_run` returns `dry_run: true` and changes nothing.
//! - `capture`: a 1x1 PNG.
//! - `code_check`: fails on source containing `compile_error!`.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use base64::Engine as _;
use pe_seams::world_api::*;
use serde_json::json;

const PNG_1X1: &str =
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==";

pub struct MockWorld {
    objects: Vec<ObjectView>,
    frame: Mutex<Frame>,
    /// Every transaction applied live, by label, for tests to read.
    pub applied: Mutex<Vec<String>>,
}

fn oid(n: u8) -> ObjectId {
    let mut b = [0u8; 16];
    b[15] = n;
    ObjectId(b)
}

fn obj(n: u8, name: &str, parent: Option<u8>, types: &[&str]) -> ObjectView {
    ObjectView {
        id: oid(n),
        name: name.into(),
        parent: parent.map(oid),
        version: Version(format!("v:{:016x}", n as u64)),
        creator: MemberId([1; 32]),
        held_by: None,
        protected_by: None,
        types: types.iter().map(|s| s.to_string()).collect(),
        components: BTreeMap::new(),
        children: vec![],
    }
}

impl MockWorld {
    pub fn new(balls: u8) -> Self {
        let mut objects = vec![
            obj(1, "Table", None, &["Transform", "Shape", "Body"]),
            obj(2, "Lamp (left)", Some(1), &["Transform", "Light"]),
            obj(3, "Lamp (right)", Some(1), &["Transform", "Light", "Protection"]),
        ];
        objects[2].protected_by = Some(MemberId([2; 32]));
        for i in 0..balls {
            objects.push(obj(
                10 + i,
                &format!("Ball {i}"),
                None,
                &["Transform", "Shape", "Body", "Appearance", "Ball"],
            ));
        }
        MockWorld { objects, frame: Mutex::new(1000), applied: Mutex::new(vec![]) }
    }

    fn frame(&self) -> Frame {
        *self.frame.lock().unwrap()
    }

    fn matches(&self, o: &ObjectView, s: &Selector) -> bool {
        match s {
            Selector::Id(r) => r.0 == o.id.to_string() || r.0 == format!("/{}", o.name),
            Selector::Ids(v) => v.iter().any(|r| r.0 == o.id.to_string()),
            Selector::Path(p) => p.trim_start_matches('/') == o.name,
            Selector::Name(n) => &o.name == n,
            Selector::NameLike(n) => o.name.to_lowercase().contains(&n.to_lowercase()),
            Selector::Has(t) => o.types.iter().any(|x| x == t),
            Selector::ChildOf(r) | Selector::Under(r) => o.parent.is_some_and(|p| p.to_string() == r.0),
            Selector::Near(_) => true,
            Selector::All(v) => v.iter().all(|s| self.matches(o, s)),
        }
    }

    fn by_ref(&self, r: &ObjRef) -> Option<&ObjectView> {
        self.objects
            .iter()
            .find(|o| o.id.to_string() == r.0 || format!("/{}", o.name) == r.0 || o.name == r.0.trim_start_matches('/'))
    }

    fn refusal(&self, code: RefusalCode, message: &str, op: u32, o: Option<&ObjectView>) -> Outcome {
        Outcome {
            dry_run: false,
            status: Status::Refused,
            is_final: false,
            change: None,
            frame: None,
            final_frame: self.frame() - 8,
            created: BTreeMap::new(),
            changed: vec![],
            count: 0,
            refusal: Some(Refusal {
                code,
                message: message.into(),
                op: Some(op),
                object: o.map(|o| ObjectBrief { id: o.id, name: o.name.clone() }),
                field: None,
                read: o.map(|o| o.version.clone()),
                now: Some(Version("v:ffffffffffffffff".into())),
                winner: (code == RefusalCode::StaleVersion).then(|| Winner {
                    change: TxId([9; 32]),
                    frame: self.frame() - 3,
                    by: MemberBrief { member: MemberId([2; 32]), name: "Bob".into() },
                    producer: Producer { kind: ProducerKind::Hand, harness: None, principal: None, requested_by: None },
                    label: Some("Moved Lamp (left)".into()),
                    value: None,
                }),
                next: Some("Read it again with world_query and decide whether your change still applies.".into()),
                details: None,
            }),
            diff: None,
        }
    }
}

impl WorldApi for MockWorld {
    fn session(&self, c: &Caller) -> SessionInfo {
        SessionInfo {
            world: WorldBrief { line: LineId([5; 32]), name: "Ball pit (mock)".into() },
            me: Me {
                member: c.member,
                name: "Alice".into(),
                principal: c.principal.clone(),
                producer: c.producer.clone(),
                role: "Admin".into(),
                may: vec!["edit".into()],
                scopes: c.scopes.iter().copied().collect(),
                scale: 1.0,
            },
            members: vec![MemberInfo { member: c.member, name: "Alice".into(), role: "Admin".into(), here: true }],
            frame: self.frame(),
            final_frame: self.frame() - 8,
            tick_hz: 60,
            link: Link::Fine,
            caught_up: true,
            kept: true,
            limits: Limits {
                tx_per_s: 20,
                tx_burst: 40,
                ops_per_tx: 4096,
                objects_per_tx: 4096,
                payload_bytes_per_tx: 1 << 20,
                query_limit_max: 256,
                watch_wait_ms_max: 30_000,
                capture_size_max: [1920, 1080],
            },
        }
    }

    fn schema(&self, _c: &Caller, q: &SchemaQuery) -> Result<Schema, ApiError> {
        let version = "s:mock".to_string();
        if q.since.as_deref() == Some("s:mock") {
            return Ok(Schema::Unchanged { version });
        }
        match &q.type_name {
            None => Ok(Schema::List {
                version,
                types: ["Transform", "Shape", "Body", "Appearance", "Light", "Ball", "Protection"]
                    .iter()
                    .map(|n| Brief { name: n.to_string(), summary: format!("{n} (mock)") })
                    .collect(),
                templates: vec![Brief { name: "box".into(), summary: "a dynamic 0.2 m box".into() }],
            }),
            Some(t) if t == "Appearance" => Ok(Schema::Type {
                version,
                name: t.clone(),
                summary: "How it looks".into(),
                fields: BTreeMap::from([(
                    "color".to_string(),
                    json!({"type": "string", "x-unit": "srgb", "x-sync": "synced"}),
                )]),
            }),
            Some(t) => Err(ApiError::arg("type", format!("no component type named {t:?}"))),
        }
    }

    fn query(&self, _c: &Caller, q: &Query) -> Result<QueryResult, ApiError> {
        let limit = q.limit.unwrap_or(50);
        if limit > 256 {
            return Err(ApiError::arg("limit", "at most 256"));
        }
        let all: Vec<&ObjectView> = self
            .objects
            .iter()
            .filter(|o| match &q.select {
                Some(s) => self.matches(o, s),
                None => o.parent.is_none(),
            })
            .collect();
        let start: usize = q.cursor.as_ref().and_then(|c| c.0.strip_prefix("p:")?.parse().ok()).unwrap_or(0);
        let page: Vec<ObjectView> = all.iter().skip(start).take(limit as usize).map(|o| (*o).clone()).collect();
        let more = (start + page.len() < all.len()).then(|| Cursor(format!("p:{}", start + page.len())));
        Ok(QueryResult {
            frame: self.frame(),
            final_frame: self.frame() - 8,
            count: q.count_only.then_some(all.len() as u64),
            objects: if q.count_only { vec![] } else { page },
            more,
        })
    }

    fn apply(&self, _c: &Caller, tx: Transaction, opts: &ApplyOpts) -> Result<Outcome, ApiError> {
        if tx.label == "refuse" {
            return Ok(self.refusal(
                RefusalCode::StaleVersion,
                "Bob moved \"Lamp (left)\" after you read it.",
                0,
                self.objects.get(1),
            ));
        }
        let mut count = 0u64;
        for (i, op) in tx.ops.iter().enumerate() {
            if let Op::Delete { target } = op
                && let Some(o) = target.id.as_ref().and_then(|r| self.by_ref(r))
                && o.protected_by.is_some()
            {
                return Ok(self.refusal(
                    RefusalCode::Protected,
                    "\"Lamp (right)\" is protected by Bob.",
                    i as u32,
                    Some(o),
                ));
            }
            count += match op {
                Op::Set { target, .. } if target.select.is_some() => {
                    let s = target.select.as_ref().unwrap();
                    let n = self.objects.iter().filter(|o| self.matches(o, s)).count() as u64;
                    if target.max.is_none_or(|m| n > m as u64) {
                        return Ok(self.refusal(
                            RefusalCode::TooMany,
                            "The selector matched more objects than max.",
                            i as u32,
                            None,
                        ));
                    }
                    n
                }
                _ => 1,
            };
        }
        let dry = opts.mode == Mode::DryRun;
        if !dry {
            self.applied.lock().unwrap().push(tx.label.clone());
            *self.frame.lock().unwrap() += 1;
        }
        Ok(Outcome {
            dry_run: dry,
            status: Status::Accepted,
            is_final: false,
            change: (!dry).then_some(TxId([7; 32])),
            frame: (!dry).then(|| self.frame()),
            final_frame: self.frame() - 8,
            created: BTreeMap::new(),
            changed: vec![],
            count,
            refusal: None,
            diff: dry.then(Diff::default),
        })
    }

    fn watch(&self, _c: &Caller, _f: &Filter, cursor: Option<&Cursor>, wait: Duration) -> Result<Changes, ApiError> {
        if let Some(c) = cursor {
            if c.0 != "c:0.0" {
                return Err(ApiError {
                    code: ErrorCode::CursorExpired,
                    message: "that cursor has expired; re-read by query".into(),
                    field: Some("cursor".into()),
                    details: Some(json!({"fresh": "c:0.0"})),
                });
            }
            std::thread::sleep(wait.min(Duration::from_millis(50)));
        }
        Ok(Changes {
            cursor: Cursor("c:0.0".into()),
            changes: vec![],
            mine: vec![],
            session: vec![],
            motion: vec![],
            more: false,
        })
    }

    fn history(&self, _c: &Caller, _q: &HistoryQuery) -> Result<History, ApiError> {
        let entries = self
            .applied
            .lock()
            .unwrap()
            .iter()
            .rev()
            .map(|l| HistoryEntry {
                change: TxId([7; 32]),
                by: MemberBrief { member: MemberId([1; 32]), name: "Alice".into() },
                producer: Producer {
                    kind: ProducerKind::Agent,
                    harness: Some("test".into()),
                    principal: None,
                    requested_by: None,
                },
                label: Some(l.clone()),
                frame: self.frame(),
                is_final: false,
                status: Status::Accepted,
                summary: l.clone(),
            })
            .collect();
        Ok(History { entries, more: None })
    }

    fn revert(&self, c: &Caller, _change: TxId, opts: &ApplyOpts) -> Result<Outcome, ApiError> {
        self.apply(c, Transaction { label: "Revert".into(), base: At::default(), ops: vec![] }, opts)
    }

    fn capture(&self, _c: &Caller, r: &CaptureRequest, deadline: Instant) -> Result<Capture, ApiError> {
        if Instant::now() > deadline {
            return Err(ApiError::new(ErrorCode::Timeout, "the capture did not finish before the deadline"));
        }
        if let Some([w, h]) = r.size
            && (w > 1920 || h > 1080 || w == 0 || h == 0)
        {
            return Err(ApiError::arg("size", "at most [1920, 1080]"));
        }
        Ok(Capture {
            png: base64::engine::general_purpose::STANDARD.decode(PNG_1X1).expect("valid"),
            frame: self.frame(),
            camera: CameraUsed {
                at: json!({"relative_to": "me", "offset": [0, 0, 0]}),
                look_at: json!({"relative_to": "me", "offset": [0, 0, -1]}),
                fov_deg: 60.0,
            },
            visible: r.visible_list.then(|| vec![Visible { id: oid(1), name: "Table".into(), rect: [0, 0, 1, 1] }]),
            visible_approx: r.visible_list,
        })
    }

    fn code_check(&self, _c: &Caller, r: &CodeCheck) -> Result<Diagnostics, ApiError> {
        if r.source.contains("compile_error!") {
            return Ok(Diagnostics {
                ok: false,
                diagnostics: vec![Diagnostic {
                    kind: DiagnosticKind::Rustc,
                    severity: Severity::Error,
                    message: "compile_error! invoked".into(),
                    file: Some("lib.rs".into()),
                    line: Some(1),
                    column: Some(1),
                    code: None,
                }],
                module: None,
                systems: vec![],
                components: vec![],
                build_ms: Some(1),
            });
        }
        Ok(Diagnostics {
            ok: true,
            diagnostics: vec![],
            module: Some("h:mock".into()),
            systems: vec!["spin".into()],
            components: vec![],
            build_ms: Some(1),
        })
    }
}
