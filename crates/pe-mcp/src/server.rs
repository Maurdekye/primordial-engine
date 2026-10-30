//! The MCP server proper: the phase-2 tool catalogue (design 17.2, 23.4 P2-8) on rmcp.
//!
//! Tool names use underscores (`world_get_state`) where the design writes dots (`world.get_state`):
//! clients such as the Anthropic API accept only `[a-zA-Z0-9_-]` in tool names.
//!
//! Every call resolves its principal first (fixed for the embedded stdio bench, the connection's
//! token for a pipe, the request's token for HTTP), then checks the tool's scope. `tools/list`
//! shows only the tools the caller's scopes allow; calling another still fails with `forbidden`.

use std::sync::Arc;

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::*;
use rmcp::service::RequestContext;
use rmcp::{tool, tool_handler, tool_router, ErrorData, Json, RoleServer, ServerHandler};

use crate::auth::{Principal, TokenId, TokenStore};
use crate::engine::EngineHandle;
use crate::model::{self as m, ErrorCode, Scope, ToolError};

pub const INSTRUCTIONS: &str = "Primordial Engine peer (phase 2: one member of a simulated P2P session). \
You drive one member: world_propose_interaction authors an interaction exactly as a player's input is authored \
(stamped with the present frame, ordered soonest-wins, rolled back and re-simulated when earlier entries arrive). \
Time only moves when you call world_step. An interaction's state is provisional until `final` is true (its frame is \
confirmed). session_sync_status and session_members show this peer's view; scenario_* tools control the simulated \
session (load, faults, and an oracle check that every replica agrees). Errors come back with a machine-readable `code`.";

/// How a server instance learns who is calling.
#[derive(Clone)]
pub enum Auth {
    /// The embedded stdio bench: the channel belongs to the process that owns the engine.
    Fixed(Principal),
    /// A pipe or socket connection that authenticated with this token at connect time; re-checked
    /// on every call so revocation and expiry take effect at once.
    Connection(Arc<TokenStore>, TokenId),
    /// Loopback HTTP: the guard put the request's principal in the HTTP request extensions.
    Http(Arc<TokenStore>),
}

#[derive(Clone)]
pub struct PeerServer {
    engine: EngineHandle,
    auth: Auth,
    tool_router: ToolRouter<Self>,
    /// The advertised tool list, with schemas cleaned once.
    tools: Arc<Vec<Tool>>,
}

/// schemars marks integers with formats such as `uint32`, which are not JSON Schema formats;
/// validators in clients warn about each one. The `minimum` bounds stay.
fn clean_schema(v: &mut serde_json::Value) {
    const NUMERIC: [&str; 12] = ["int8", "int16", "int32", "int64", "uint8", "uint16", "uint32", "uint64", "int", "uint", "float", "double"];
    match v {
        serde_json::Value::Object(m) => {
            if m.get("format").and_then(|f| f.as_str()).is_some_and(|f| NUMERIC.contains(&f)) {
                m.remove("format");
            }
            m.values_mut().for_each(clean_schema);
        }
        serde_json::Value::Array(a) => a.iter_mut().for_each(clean_schema),
        _ => {}
    }
}

fn clean_object(o: &JsonObject) -> Arc<JsonObject> {
    let mut v = serde_json::Value::Object(o.clone());
    clean_schema(&mut v);
    let serde_json::Value::Object(m) = v else { unreachable!() };
    Arc::new(m)
}

fn tool_err(e: ToolError) -> CallToolResult {
    CallToolResult::structured_error(serde_json::to_value(&e).expect("serializable"))
}

/// Tool methods spell out `Result<Json<T>, CallToolResult>`: rmcp derives the output schema from that
/// literal return type, not through an alias.
type Out<T> = Result<Json<T>, CallToolResult>;

/// The scope each tool needs. Fails closed: a tool missing here needs `control`.
pub fn scope_of(tool: &str) -> Scope {
    match tool {
        "session_info" | "session_members" | "session_sync_status" | "world_get_state" | "world_get_object" | "world_hashes"
        | "world_interaction_status" => Scope::Read,
        "world_propose_interaction" => Scope::Interact,
        _ => Scope::Control,
    }
}

impl PeerServer {
    pub fn new(engine: EngineHandle, auth: Auth) -> Self {
        let tool_router = Self::tool_router();
        let tools = tool_router
            .list_all()
            .into_iter()
            .map(|mut t| {
                t.input_schema = clean_object(&t.input_schema);
                t.output_schema = t.output_schema.as_deref().map(clean_object);
                t
            })
            .collect();
        PeerServer { engine, auth, tool_router, tools: Arc::new(tools) }
    }

    fn principal(&self, ctx: &RequestContext<RoleServer>) -> Result<Principal, ToolError> {
        let unauth = || ToolError::new(ErrorCode::Forbidden, "the session token is no longer valid");
        match &self.auth {
            Auth::Fixed(p) => Ok(p.clone()),
            Auth::Connection(store, id) => store.check(*id).map_err(|_| unauth()),
            Auth::Http(store) => {
                let p = ctx
                    .extensions
                    .get::<axum::http::request::Parts>()
                    .and_then(|parts| parts.extensions.get::<Principal>())
                    .ok_or_else(unauth)?;
                // The guard authenticated this request; re-check for a revocation since.
                store.check(p.token.ok_or_else(unauth)?).map_err(|_| unauth())
            }
        }
    }

    fn require(&self, ctx: &RequestContext<RoleServer>, tool: &str) -> Result<Principal, CallToolResult> {
        let p = self.principal(ctx).map_err(tool_err)?;
        let need = scope_of(tool);
        if !p.has(need) {
            return Err(tool_err(
                ToolError::new(ErrorCode::Forbidden, format!("{tool} needs scope {}, which this token lacks", need.as_str()))
                    .with_details(serde_json::json!({ "need": need, "have": p.scopes })),
            ));
        }
        Ok(p)
    }

    async fn run<R: Send + 'static>(&self, f: impl FnOnce(&mut crate::engine::Engine) -> m::ToolResult<R> + Send + 'static) -> Out<R> {
        self.engine.call(f).await.map(Json).map_err(tool_err)
    }
}

#[tool_router]
impl PeerServer {
    #[tool(name = "session_info", description = "Who you are (principal, scopes, the member you drive) and the session: scenario, members, scene, clock, frame and confirmed frame.")]
    async fn session_info(&self, ctx: RequestContext<RoleServer>) -> Result<Json<m::SessionInfo>, CallToolResult> {
        let who = self.require(&ctx, "session_info")?;
        self.run(move |e| Ok(e.info(&who))).await
    }

    #[tool(name = "world_get_state", description = "The driven peer's world: frame, confirmed frame, state hash, a page of objects (position, velocity) and the claims in force.")]
    async fn world_get_state(&self, Parameters(p): Parameters<m::GetStateParams>, ctx: RequestContext<RoleServer>) -> Result<Json<m::WorldState>, CallToolResult> {
        self.require(&ctx, "world_get_state")?;
        self.run(move |e| e.state(&p)).await
    }

    #[tool(name = "world_get_object", description = "One object's full state: global id, kind, position, rotation, velocities, exact-state hash and claim.")]
    async fn world_get_object(&self, Parameters(p): Parameters<m::ObjectParams>, ctx: RequestContext<RoleServer>) -> Result<Json<m::ObjectState>, CallToolResult> {
        self.require(&ctx, "world_get_object")?;
        self.run(move |e| e.object(&p)).await
    }

    #[tool(name = "world_propose_interaction", description = "Author an interaction as the driven member, through the same path as a player's input: stamped with the present frame, ordered soonest-wins, rolled back when earlier entries arrive. touch/grab apply an impulse (grab renews its claim); hand moves a kinematic hand (ball-pit scenes). Returns a ticket; the outcome appears in world_step's `changed` and in world_interaction_status.")]
    async fn world_propose_interaction(&self, Parameters(p): Parameters<m::ProposeParams>, ctx: RequestContext<RoleServer>) -> Result<Json<m::Proposed>, CallToolResult> {
        let who = self.require(&ctx, "world_propose_interaction")?;
        self.run(move |e| e.propose(&who, &p)).await
    }

    #[tool(name = "world_interaction_status", description = "The current verdict on one of your interactions: pending, accepted, rejected (with reason) or void; `final` once its frame is confirmed.")]
    async fn world_interaction_status(&self, Parameters(p): Parameters<m::TicketParams>, ctx: RequestContext<RoleServer>) -> Result<Json<m::InteractionStatus>, CallToolResult> {
        let who = self.require(&ctx, "world_interaction_status")?;
        self.run(move |e| e.ticket(&who, &p)).await
    }

    #[tool(name = "world_step", description = "Advance the session's virtual clock by `ms` milliseconds (1..=10000) or `frames` frames (1..=600). Every member ticks, the network delivers, the driven peer rolls back as needed. Reports the new frame and confirmed frame, rollbacks, your interactions whose state changed, and protocol events.")]
    async fn world_step(&self, Parameters(p): Parameters<m::StepParams>, ctx: RequestContext<RoleServer>) -> Result<Json<m::StepReport>, CallToolResult> {
        let who = self.require(&ctx, "world_step")?;
        self.run(move |e| e.step(&who, &p)).await
    }

    #[tool(name = "world_hashes", description = "State hashes after each frame in [from, to) (at most 600 frames), plus the canonical check hashes compared with other members.")]
    async fn world_hashes(&self, Parameters(p): Parameters<m::HashesParams>, ctx: RequestContext<RoleServer>) -> Result<Json<m::Hashes>, CallToolResult> {
        self.require(&ctx, "world_hashes")?;
        self.run(move |e| e.hashes(&p)).await
    }

    #[tool(name = "session_members", description = "Membership as the driven peer sees it: members, evictions (cut seq, from frame), epoch, and vote counters.")]
    async fn session_members(&self, ctx: RequestContext<RoleServer>) -> Result<Json<m::Members>, CallToolResult> {
        self.require(&ctx, "session_members")?;
        self.run(|e| e.members()).await
    }

    #[tool(name = "session_sync_status", description = "Sync health of the driven peer: frame, clock, confirmed frame, speculation and its limit, paused, check hashes matched/mismatched, rollback counters.")]
    async fn session_sync_status(&self, ctx: RequestContext<RoleServer>) -> Result<Json<m::SyncStatus>, CallToolResult> {
        self.require(&ctx, "session_sync_status")?;
        self.run(|e| e.sync()).await
    }

    #[tool(name = "scenario_load", description = "Start a fresh simulated session (replacing the current one, tickets included): members, the member you drive, scene, link delay/jitter/loss, background touches by the other members, speculation window, seed. Same parameters and seed give the same run.")]
    async fn scenario_load(&self, Parameters(p): Parameters<m::LoadParams>, ctx: RequestContext<RoleServer>) -> Result<Json<m::SessionInfo>, CallToolResult> {
        let who = self.require(&ctx, "scenario_load")?;
        self.run(move |e| e.load(&who, &p)).await
    }

    #[tool(name = "scenario_fault", description = "Inject a fault into the simulated session: crash a member (permanent), set a link's delay/jitter/loss, partition members into groups, or heal the partition.")]
    async fn scenario_fault(&self, Parameters(p): Parameters<m::FaultParams>, ctx: RequestContext<RoleServer>) -> Result<Json<m::FaultReport>, CallToolResult> {
        self.require(&ctx, "scenario_fault")?;
        self.run(move |e| e.fault(&p)).await
    }

    #[tool(name = "scenario_check", description = "Oracle check over the confirmed prefix: every live member's state hashes against the driven peer's, and the driven peer's against an offline replay of its final log. Reports the first divergence.")]
    async fn scenario_check(&self, ctx: RequestContext<RoleServer>) -> Result<Json<m::CheckReport>, CallToolResult> {
        self.require(&ctx, "scenario_check")?;
        self.run(|e| e.check()).await
    }
}

const RESOURCES: [(&str, &str, &str); 3] = [
    ("primordial://world/state", "world state", "The driven peer's world state with every object (as world_get_state)."),
    ("primordial://session/members", "session members", "Membership as the driven peer sees it (as session_members)."),
    ("primordial://session/sync", "sync status", "The driven peer's sync status (as session_sync_status)."),
];

#[tool_handler(router = self.tool_router)]
impl ServerHandler for PeerServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().enable_resources().build())
            .with_server_info(Implementation::new("primordial-engine", env!("CARGO_PKG_VERSION")))
            .with_instructions(INSTRUCTIONS)
    }

    async fn call_tool(&self, request: CallToolRequestParams, ctx: RequestContext<RoleServer>) -> Result<CallToolResponse, ErrorData> {
        let tcc = rmcp::handler::server::tool::ToolCallContext::new(self, request, ctx);
        let mut r = self.tool_router.call(tcc).await?;
        // rmcp reports arguments that fail to deserialize as a plain-text tool error. Give them the
        // same machine-readable shape as every other error.
        if let CallToolResponse::Complete(res) = &mut r {
            if res.is_error == Some(true) && res.structured_content.is_none() {
                let text = res.content.iter().filter_map(|c| c.as_text().map(|t| t.text.clone())).collect::<Vec<_>>().join("
");
                *res = tool_err(ToolError::new(ErrorCode::InvalidArgument, text));
            }
        }
        Ok(r)
    }

    async fn list_tools(&self, _request: Option<PaginatedRequestParams>, ctx: RequestContext<RoleServer>) -> Result<ListToolsResult, ErrorData> {
        let p = self.principal(&ctx).map_err(|e| ErrorData::invalid_request(e.message, None))?;
        let tools = self.tools.iter().filter(|t| p.has(scope_of(&t.name))).cloned().collect();
        let mut r = ListToolsResult::with_all_items(tools);
        // The list depends on the caller's token.
        if ctx.protocol_version().is_some_and(|v| v >= ProtocolVersion::V_2026_07_28) {
            r.ttl_ms = Some(0);
            r.cache_scope = Some(CacheScope::Private);
        }
        Ok(r)
    }

    async fn list_resources(&self, _request: Option<PaginatedRequestParams>, ctx: RequestContext<RoleServer>) -> Result<ListResourcesResult, ErrorData> {
        let p = self.principal(&ctx).map_err(|e| ErrorData::invalid_request(e.message, None))?;
        let resources = if p.has(Scope::Read) {
            RESOURCES
                .iter()
                .map(|(uri, name, desc)| {
                    let mut r = Resource::new(*uri, *name);
                    r.description = Some((*desc).into());
                    r.mime_type = Some("application/json".into());
                    r
                })
                .collect()
        } else {
            vec![]
        };
        Ok(ListResourcesResult::with_all_items(resources))
    }

    async fn read_resource(&self, request: ReadResourceRequestParams, ctx: RequestContext<RoleServer>) -> Result<ReadResourceResponse, ErrorData> {
        let p = self.principal(&ctx).map_err(|e| ErrorData::invalid_request(e.message, None))?;
        if !p.has(Scope::Read) {
            return Err(ErrorData::invalid_request("this token lacks scope read", None));
        }
        let uri = request.uri.clone();
        let value: Result<serde_json::Value, ToolError> = match uri.as_str() {
            "primordial://world/state" => {
                self.engine.call(|e| e.state(&m::GetStateParams { offset: None, limit: Some(256) }).map(|s| serde_json::to_value(s).unwrap())).await
            }
            "primordial://session/members" => self.engine.call(|e| e.members().map(|s| serde_json::to_value(s).unwrap())).await,
            "primordial://session/sync" => self.engine.call(|e| e.sync().map(|s| serde_json::to_value(s).unwrap())).await,
            _ => return Err(ErrorData::resource_not_found(format!("no resource {uri}"), None)),
        };
        let v = value.map_err(|e| ErrorData::internal_error(e.message, Some(serde_json::to_value(&e.code).unwrap())))?;
        let mut c = ResourceContents::text(v.to_string(), uri);
        if let ResourceContents::TextResourceContents { mime_type, .. } = &mut c {
            *mime_type = Some("application/json".into());
        }
        Ok(ReadResourceResponse::Complete(ReadResourceResult::new(vec![c])))
    }
}
