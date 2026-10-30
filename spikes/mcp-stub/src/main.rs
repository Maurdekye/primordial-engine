//! Phase-1 stub MCP server: newline-delimited JSON-RPC 2.0 on stdin/stdout (the MCP stdio
//! transport). It owns one local peer's simulation and exposes:
//!
//!   tools/call world_get_state          read frame, state hash and object transforms
//!   tools/call world_step               advance the fixed-step simulation N frames
//!   tools/call world_propose_interaction submit an interaction; it enters the SAME input path
//!                                        (frame-stamped, soonest-wins, rollback) as a player's
//!   resources/read primordial://world/state
//!
//! This is a protocol-level stub written by hand against the MCP JSON-RPC shapes; it has not
//! been exercised by a third-party MCP client.

use std::io::{BufRead, Write};

use box3d_rust::body::{body_get_linear_velocity, body_get_position};
use pe_sim_core::*;
use serde_json::{json, Value};

const SEED: u64 = 0x5052_494D_4F52_4449;
const AI_PEER: u32 = 100; // the agent acts as its own peer identity

struct Server {
    sim: RollbackSim,
    next_seq: u32,
}

impl Server {
    fn state_json(&self, max: usize) -> Value {
        let w = &self.sim.state.scene.world;
        let bodies: Vec<Value> = self
            .sim
            .state
            .scene
            .bodies
            .iter()
            .take(max)
            .enumerate()
            .map(|(i, &b)| {
                let p = body_get_position(w, b);
                let v = body_get_linear_velocity(w, b);
                json!({"object": i, "position": [p.x, p.y, p.z], "linear_velocity": [v.x, v.y, v.z]})
            })
            .collect();
        let frame = self.sim.frame();
        json!({
            "frame": frame,
            "state_hash": self.sim.hashes.get(&frame.saturating_sub(1)).map(|h| format!("{h:016x}")),
            "object_count": self.sim.state.scene.bodies.len(),
            "claims": self.sim.state.claims.iter().map(|(o, (p, u))| json!({"object": o, "peer": p, "until_frame": u})).collect::<Vec<_>>(),
            "objects": bodies,
        })
    }

    fn tools() -> Value {
        json!([
            {
                "name": "world_get_state",
                "description": "Read the current sim frame, the rollback-state hash and object transforms.",
                "inputSchema": {"type": "object", "properties": {"max_objects": {"type": "integer", "minimum": 0, "default": 8}}},
            },
            {
                "name": "world_step",
                "description": "Advance the fixed 60 Hz simulation by N frames (a local test harness; a live session advances on the clock).",
                "inputSchema": {"type": "object", "properties": {"frames": {"type": "integer", "minimum": 1, "maximum": 600}}, "required": ["frames"]},
            },
            {
                "name": "world_propose_interaction",
                "description": "Apply an impulse to an object. Stamped with the current frame and resolved exactly like a player's interaction (soonest wins; claimed objects reject).",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "object": {"type": "integer", "minimum": 0},
                        "impulse": {"type": "array", "items": {"type": "number"}, "minItems": 3, "maxItems": 3}
                    },
                    "required": ["object", "impulse"]
                },
            }
        ])
    }

    fn call_tool(&mut self, name: &str, args: &Value) -> Result<Value, Value> {
        match name {
            "world_get_state" => {
                let max = args.get("max_objects").and_then(Value::as_u64).unwrap_or(8) as usize;
                Ok(self.state_json(max))
            }
            "world_step" => {
                let n = args.get("frames").and_then(Value::as_u64).filter(|n| (1..=600).contains(n))
                    .ok_or_else(|| json!({"code": "invalid_argument", "field": "frames", "expected": "integer 1..=600"}))?;
                let target = self.sim.frame() + n;
                self.sim.advance_to(target);
                let verdicts: Vec<Value> = self.sim.verdicts.range(target - n..target).flat_map(|(f, v)| {
                    v.iter().map(move |(i, verdict)| json!({"frame": f, "peer": i.peer, "seq": i.seq, "object": i.body, "verdict": format!("{verdict:?}")}))
                }).collect();
                Ok(json!({"frame": self.sim.frame(), "state_hash": format!("{:016x}", self.sim.hashes[&(target - 1)]), "verdicts": verdicts}))
            }
            "world_propose_interaction" => {
                let object = args.get("object").and_then(Value::as_u64)
                    .ok_or_else(|| json!({"code": "invalid_argument", "field": "object", "expected": "non-negative integer"}))?;
                if object as usize >= self.sim.state.scene.bodies.len() {
                    return Err(json!({"code": "unknown_object", "object": object, "object_count": self.sim.state.scene.bodies.len()}));
                }
                let imp: Vec<f32> = args.get("impulse").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_f64).map(|x| x as f32).collect()).unwrap_or_default();
                if imp.len() != 3 || imp.iter().any(|x| !x.is_finite()) {
                    return Err(json!({"code": "invalid_argument", "field": "impulse", "expected": "3 finite numbers"}));
                }
                let i = Interaction { frame: self.sim.frame(), peer: AI_PEER, seq: self.next_seq, body: object as u32, impulse: [imp[0], imp[1], imp[2]] };
                self.next_seq += 1;
                match self.sim.add_input(i) {
                    Ok(_) => Ok(json!({"scheduled_frame": i.frame, "peer": AI_PEER, "seq": i.seq, "note": "resolved when this frame is simulated; see world_step verdicts"})),
                    Err(e) => Err(json!({"code": "rejected", "reason": format!("{e:?}")})),
                }
            }
            _ => Err(json!({"code": "unknown_tool", "name": name})),
        }
    }

    fn handle(&mut self, req: &Value) -> Option<Value> {
        let id = req.get("id").cloned();
        let method = req.get("method").and_then(Value::as_str).unwrap_or("");
        let params = req.get("params").cloned().unwrap_or(json!({}));
        let result: Result<Value, (i64, String)> = match method {
            "initialize" => {
                let asked = params.get("protocolVersion").and_then(Value::as_str).unwrap_or("2025-06-18");
                let v = if ["2025-11-25", "2025-06-18", "2025-03-26"].contains(&asked) { asked } else { "2025-06-18" };
                Ok(json!({
                    "protocolVersion": v,
                    "capabilities": {"tools": {"listChanged": false}, "resources": {"listChanged": false}},
                    "serverInfo": {"name": "primordial-engine-stub", "version": "0.0.1"},
                    "instructions": "Phase-1 stub: one local peer's deterministic simulation."
                }))
            }
            "notifications/initialized" | "notifications/cancelled" => return None,
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({"tools": Self::tools()})),
            "tools/call" => {
                let name = params.get("name").and_then(Value::as_str).unwrap_or("").to_string();
                let args = params.get("arguments").cloned().unwrap_or(json!({}));
                Ok(match self.call_tool(&name, &args) {
                    Ok(v) => json!({"content": [{"type": "text", "text": v.to_string()}], "structuredContent": v, "isError": false}),
                    Err(e) => json!({"content": [{"type": "text", "text": e.to_string()}], "structuredContent": e, "isError": true}),
                })
            }
            "resources/list" => Ok(json!({"resources": [{"uri": "primordial://world/state", "name": "world state", "mimeType": "application/json"}]})),
            "resources/read" => match params.get("uri").and_then(Value::as_str) {
                Some("primordial://world/state") => Ok(json!({"contents": [{"uri": "primordial://world/state", "mimeType": "application/json", "text": self.state_json(usize::MAX).to_string()}]})),
                other => Err((-32002, format!("resource not found: {other:?}"))),
            },
            _ => Err((-32601, format!("method not found: {method}"))),
        };
        let id = id?; // notifications get no response
        Some(match result {
            Ok(r) => json!({"jsonrpc": "2.0", "id": id, "result": r}),
            Err((code, message)) => json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}}),
        })
    }
}

fn main() {
    let mut server = Server { sim: RollbackSim::new(GameState::new(build_pile_scene(8, 4), SEED), 120), next_seq: 0 };
    let stdin = std::io::stdin();
    let mut out = std::io::stdout().lock();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let resp = match serde_json::from_str::<Value>(&line) {
            Ok(req) => server.handle(&req),
            Err(e) => Some(json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32700, "message": format!("parse error: {e}")}})),
        };
        if let Some(r) = resp {
            writeln!(out, "{r}").unwrap();
            out.flush().unwrap();
        }
    }
}
