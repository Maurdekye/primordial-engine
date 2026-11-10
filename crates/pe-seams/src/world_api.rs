//! Seam S6, WorldApi (feature `world-api`): the trait and its request and result types, from @api-eng's proposal
//! (`docs/spec/api.md` section 11; `crates/pe-api/src/s6.rs` at ws/api 001dffec, moved here unchanged except as
//! noted below). The JSON these types serialize to is the MCP surface (spec sections 4-8).
//!
//! Field values cross the seam as JSON (`Json`), in the forms of spec 4.4. pe-api parses them against the world's
//! schema into pe-world's binary types, once, on the author's machine. This keeps pe-seams free of pe-world.
//!
//! **The ids here are S6's text forms** (`"o:…"`, `"e:…"`, `"x:…"`, `"m:…"`, `"l:…"`): the wire spelling of S1's
//! binary ids, one spelling per id, so ids compare as text. Convert with `From` in both directions; an object id's
//! 16 bytes are its `u128` in big-endian order, so text order is numeric order.
//!
//! Changes from s6.rs (v0): the JSON Schema pattern of each id now asks for twice as many hex digits as the id has
//! bytes (it asked for as many, so a schema-validating client would have refused every real id); `From`
//! conversions to and from S1's types; `MemberId` is no longer a stand-in.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::time::{Duration, Instant};

use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// A field value or a position, in the JSON forms of spec 4.4.
pub type Json = serde_json::Value;
pub type Frame = u64;

// ---- ids -----------------------------------------------------------------------------------------------------------

macro_rules! hex_id {
    ($(#[$m:meta])* $name:ident, $bytes:expr, $hex:literal, $prefix:literal) => {
        $(#[$m])*
        #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(pub [u8; $bytes]);

        impl $name {
            pub const PREFIX: &'static str = $prefix;
            pub fn parse(s: &str) -> Option<Self> {
                let h = s.strip_prefix($prefix)?;
                if h.len() != 2 * $bytes {
                    return None;
                }
                let mut out = [0u8; $bytes];
                // Lowercase hex only, no sign: one spelling per id, so ids compare as text.
                if !h.bytes().all(|c| matches!(c, b'0'..=b'9' | b'a'..=b'f')) {
                    return None;
                }
                for (i, o) in out.iter_mut().enumerate() {
                    *o = u8::from_str_radix(h.get(2 * i..2 * i + 2)?, 16).ok()?;
                }
                Some($name(out))
            }
        }
        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str($prefix)?;
                for b in self.0 {
                    write!(f, "{b:02x}")?;
                }
                Ok(())
            }
        }
        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                fmt::Display::fmt(self, f)
            }
        }
        impl Serialize for $name {
            fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                s.collect_str(self)
            }
        }
        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                let s = String::deserialize(d)?;
                $name::parse(&s).ok_or_else(|| {
                    serde::de::Error::custom(concat!("expected ", $prefix, " and ", stringify!($bytes), " bytes of hex"))
                })
            }
        }
        impl JsonSchema for $name {
            fn schema_name() -> std::borrow::Cow<'static, str> {
                stringify!($name).into()
            }
            fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
                schemars::json_schema!({
                    "type": "string",
                    "pattern": concat!("^", $prefix, "[0-9a-f]{", stringify!($hex), "}$"),
                })
            }
        }
    };
}

hex_id!(
    /// A world-scoped object id (S1: 128 bits), `"o:"` and 32 hex digits.
    ObjectId, 16, 32, "o:"
);
hex_id!(
    /// An entry id (S1: a 256-bit hash), `"e:"` and 64 hex digits.
    EntryId, 32, 64, "e:"
);
hex_id!(
    /// A transaction ("change") id: the id of its edit entry, `"x:"` and 64 hex digits.
    TxId, 32, 64, "x:"
);
hex_id!(
    /// A member's public key (S1's `MemberKey`), `"m:"` and 64 hex digits.
    MemberId, 32, 64, "m:"
);
hex_id!(
    /// A world's line id (world-model.md 2.3, `LineId`), `"l:"` and 64 hex digits.
    LineId, 32, 64, "l:"
);

// ---- conversions to and from S1's ids -------------------------------------------------------------------------------

impl core::convert::From<crate::ObjectId> for ObjectId {
    fn from(id: crate::ObjectId) -> Self {
        ObjectId(id.to_u128().to_be_bytes())
    }
}

impl core::convert::From<ObjectId> for crate::ObjectId {
    fn from(id: ObjectId) -> Self {
        crate::ObjectId::from_u128(u128::from_be_bytes(id.0))
    }
}

impl core::convert::From<crate::EntryRef> for EntryId {
    fn from(e: crate::EntryRef) -> Self {
        EntryId(e.0.0)
    }
}

impl core::convert::From<EntryId> for crate::EntryRef {
    fn from(e: EntryId) -> Self {
        crate::EntryRef(crate::Hash(e.0))
    }
}

impl core::convert::From<crate::EntryRef> for TxId {
    fn from(e: crate::EntryRef) -> Self {
        TxId(e.0.0)
    }
}

impl core::convert::From<TxId> for crate::EntryRef {
    fn from(t: TxId) -> Self {
        crate::EntryRef(crate::Hash(t.0))
    }
}

impl core::convert::From<crate::MemberKey> for MemberId {
    fn from(k: crate::MemberKey) -> Self {
        MemberId(k.0)
    }
}

impl core::convert::From<MemberId> for crate::MemberKey {
    fn from(m: MemberId) -> Self {
        crate::MemberKey(m.0)
    }
}

impl core::convert::From<crate::LineId> for LineId {
    fn from(l: crate::LineId) -> Self {
        LineId(l.0.0)
    }
}

impl core::convert::From<LineId> for crate::LineId {
    fn from(l: LineId) -> Self {
        crate::LineId(crate::Hash(l.0))
    }
}

/// An object's version (spec 4.2): opaque to callers. pe-api issues `"v:"` and 16 hex digits, a prefix of the id of
/// the last edit entry that changed the object, and sends the full expected entry id in the edit entry.
/// `"any"` in an `if_version` makes a plain write.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
pub struct Version(pub String);

impl Version {
    pub const ANY: &'static str = "any";
    pub fn is_any(&self) -> bool {
        self.0 == Self::ANY
    }
}

/// An object named by id (`"o:…"`), path (`"/Table/Lamp (left)"`), a `$ref` created in the same transaction, or
/// `"me"` (the player the caller acts for; positions and `near` only). Resolved by pe-api against a snapshot.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
pub struct ObjRef(pub String);

/// An opaque position in this process's change stream, or a page token.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
pub struct Cursor(pub String);

/// One MCP connection, or one in-process client. pe-api keeps its table of versions seen per connection.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ConnectionId(pub u64);

// ---- callers and attribution (spec 4.5, 10) -----------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    Read,
    Edit,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ProducerKind {
    /// A person's own direct actions: hands, the inspector, in-game tools (pe-world's `Actor::Person`).
    Hand,
    /// pe-world's `Actor::Agent`.
    Agent,
}

/// The engine-written producer label (spec 4.5). Set from the connection, never from arguments.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Producer {
    pub kind: ProducerKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub principal: Option<String>,
    /// A guest who prompted an owner's agent (M5 panels).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested_by: Option<MemberId>,
}

/// Who is calling, built by the adapter from the token (spec 10.2). Rights come from here, never from arguments.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Caller {
    pub connection: ConnectionId,
    pub principal: String,
    pub member: MemberId,
    pub scopes: BTreeSet<Scope>,
    pub producer: Producer,
    /// Hold each transaction for the owner's approval (C11: off by default).
    pub confirm: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct MemberBrief {
    pub member: MemberId,
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ObjectBrief {
    pub id: ObjectId,
    pub name: String,
}

// ---- snapshots ------------------------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AtName {
    /// The newest published frame (predicted).
    Latest,
    /// The newest final cut.
    Final,
}

/// Which snapshot to read: `"latest"`, `"final"`, or a frame still retained.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum At {
    Named(AtName),
    Frame(Frame),
}

impl Default for At {
    fn default() -> Self {
        At::Named(AtName::Latest)
    }
}

// ---- session (spec 5.1) --------------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Link {
    Fine,
    Thin,
    SomeActionsDropped,
    CatchingUp,
    WatchingOnly,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Limits {
    pub tx_per_s: u32,
    pub tx_burst: u32,
    pub ops_per_tx: u32,
    pub objects_per_tx: u32,
    pub payload_bytes_per_tx: u32,
    pub query_limit_max: u32,
    pub watch_wait_ms_max: u32,
    pub capture_size_max: [u32; 2],
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct WorldBrief {
    pub line: LineId,
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Me {
    pub member: MemberId,
    pub name: String,
    pub principal: String,
    pub producer: Producer,
    pub role: String,
    /// The role's permissions, in the role system's own words.
    pub may: Vec<String>,
    pub scopes: Vec<Scope>,
    /// The player's own scale (metres are the world's).
    pub scale: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct MemberInfo {
    pub member: MemberId,
    pub name: String,
    pub role: String,
    pub here: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct SessionInfo {
    pub world: WorldBrief,
    pub me: Me,
    pub members: Vec<MemberInfo>,
    pub frame: Frame,
    pub final_frame: Frame,
    pub tick_hz: u32,
    pub link: Link,
    /// False while this machine is catching up; `apply` then refuses with `not_ready`.
    pub caught_up: bool,
    /// Whether an admin of the world is present: with none, the session keeps nothing (user ruling, file 18).
    pub kept: bool,
    pub limits: Limits,
}

// ---- schema (spec 5.6) ---------------------------------------------------------------------------------------------

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct SchemaQuery {
    /// One component type by name.
    #[serde(default, rename = "type", skip_serializing_if = "Option::is_none")]
    pub type_name: Option<String>,
    /// One template by name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub template: Option<String>,
    /// Returns `unchanged` when the registry still has this version.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Brief {
    pub name: String,
    pub summary: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Schema {
    Unchanged {
        version: String,
    },
    List {
        version: String,
        types: Vec<Brief>,
        templates: Vec<Brief>,
    },
    /// A JSON Schema per field, with `x-unit`, `x-sync`, `x-protected` and `x-read-only` annotations.
    Type {
        version: String,
        name: String,
        summary: String,
        fields: BTreeMap<String, Json>,
    },
    Template {
        version: String,
        name: String,
        summary: String,
        holds: Json,
    },
}

// ---- selectors and queries (spec 5.2) ------------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Near {
    pub of: ObjRef,
    pub within_m: f64,
}

/// A selector. Its JSON is one key: `{"has": "Ball"}`, `{"all": [ ... ]}`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Selector {
    Id(ObjRef),
    Ids(Vec<ObjRef>),
    /// `*` matches one level, `**` any depth.
    Path(String),
    Name(String),
    /// Case-insensitive substring.
    NameLike(String),
    Has(String),
    Near(Near),
    /// Direct children, in their order.
    ChildOf(ObjRef),
    /// The whole subtree.
    Under(ObjRef),
    All(Vec<Selector>),
}

fn is_false(b: &bool) -> bool {
    !*b
}
fn is_zero(n: &u32) -> bool {
    *n == 0
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Query {
    #[serde(default)]
    pub at: At,
    /// Omitted: the root's children.
    #[serde(default, rename = "where", skip_serializing_if = "Option::is_none")]
    pub select: Option<Selector>,
    /// Component names or `Component.field`; `"*"` for everything. Omitted: id, name, parent, version and the
    /// component types only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fields: Option<Vec<String>>,
    /// The frame positions are given in: `"parent"` (default), an object, `"me"`, or `"world_origin"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frame: Option<ObjRef>,
    /// Levels of children to inline.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub children: u32,
    /// Default 50, at most 256.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<Cursor>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub count_only: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ObjectView {
    pub id: ObjectId,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<ObjectId>,
    pub version: Version,
    /// Who created or imported it (write-once; world-model.md 7.8).
    pub creator: MemberId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub held_by: Option<MemberId>,
    /// The owner of a `Protection` on it or an ancestor.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protected_by: Option<MemberId>,
    /// The component types, when no fields were asked for.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub types: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub components: BTreeMap<String, BTreeMap<String, Json>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<ObjectView>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct QueryResult {
    pub frame: Frame,
    pub final_frame: Frame,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub count: Option<u64>,
    pub objects: Vec<ObjectView>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub more: Option<Cursor>,
}

// ---- transactions (spec 5.3) ---------------------------------------------------------------------------------------

/// One object by `id`, or many by `where` with a required `max` (selector ops are plain writes).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Target {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<ObjRef>,
    #[serde(default, rename = "where", skip_serializing_if = "Option::is_none")]
    pub select: Option<Selector>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max: Option<u32>,
    /// Default: this connection's last read of the object, else its version at the base. `"any"`: a plain write.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub if_version: Option<Version>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Keep {
    #[default]
    WorldPose,
    LocalPose,
}

pub type ComponentValues = BTreeMap<String, BTreeMap<String, Json>>;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Op {
    Create {
        #[serde(rename = "as", default, skip_serializing_if = "Option::is_none")]
        as_ref: Option<String>,
        parent: ObjRef,
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        template: Option<String>,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        components: ComponentValues,
    },
    Set {
        #[serde(flatten)]
        target: Target,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        field: Option<String>,
        /// `null` resets to the default.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        value: Option<Json>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        fields: Option<BTreeMap<String, Json>>,
    },
    Add {
        #[serde(flatten)]
        target: Target,
        component: String,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        fields: BTreeMap<String, Json>,
    },
    Remove {
        #[serde(flatten)]
        target: Target,
        component: String,
    },
    Reparent {
        #[serde(flatten)]
        target: Target,
        parent: ObjRef,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        index: Option<u32>,
        #[serde(default)]
        keep: Keep,
    },
    Duplicate {
        #[serde(flatten)]
        target: Target,
        #[serde(rename = "as", default, skip_serializing_if = "Option::is_none")]
        as_ref: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent: Option<ObjRef>,
    },
    Delete {
        #[serde(flatten)]
        target: Target,
    },
    /// The logic edit of U1 (spec 7): WS-logic's semantics. `target.id` is the script asset id, or a `$ref` for a
    /// new script; its version is the hash of its source tree.
    Code {
        #[serde(flatten)]
        target: Target,
        /// The text of `lib.rs` (M1), or a blob tree hash `"b:…"` (later).
        source: String,
        /// Objects to bind the script to, through a `Script` component.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        attach: Vec<ObjRef>,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Transaction {
    /// Free text for history, shown to players.
    pub label: String,
    /// The snapshot selectors, paths and frames resolve against; the fallback for versions.
    #[serde(default)]
    pub base: At,
    pub ops: Vec<Op>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    #[default]
    Live,
    DryRun,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Wait {
    None,
    /// Until this machine has applied or refused it.
    #[default]
    Local,
    /// Until it is final, refused or dropped.
    Final,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ApplyOpts {
    #[serde(default)]
    pub mode: Mode,
    #[serde(default)]
    pub wait: Wait,
    /// Default 2,000, at most 10,000.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u32>,
    /// For `revert`: revert even if the objects changed since (a plain write).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub if_version: Option<Version>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Queued,
    Accepted,
    Final,
    Refused,
    Dropped,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RefusalCode {
    StaleVersion,
    NotFound,
    Ambiguous,
    Invalid,
    InChange,
    TooMany,
    NotPermitted,
    Protected,
    Held,
    TooLarge,
    RateLimited,
    NotReady,
    Compile,
    Dropped,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Winner {
    pub change: TxId,
    pub frame: Frame,
    pub by: MemberBrief,
    pub producer: Producer,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<Json>,
}

/// One envelope for every refusal (spec 5.3.3).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Refusal {
    pub code: RefusalCode,
    /// One engine-written sentence, safe to show a player; peer text in it is quoted.
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub op: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub object: Option<ObjectBrief>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub field: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read: Option<Version>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub now: Option<Version>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub winner: Option<Winner>,
    /// What an agent should do next.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next: Option<String>,
    /// The rest, by code: `matches`, `rule`, `given`, `count`, `max`, `permission`, `owner`, `retry_after_ms`, ….
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Json>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct IdVersion {
    pub id: ObjectId,
    pub version: Version,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct FieldDiff {
    /// An id, or a `$ref` in a dry run.
    pub id: ObjRef,
    pub field: String,
    pub before: Json,
    pub after: Json,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Diff {
    pub created: Vec<ObjRef>,
    pub deleted: Vec<ObjectBrief>,
    pub set: Vec<FieldDiff>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Outcome {
    /// First, so a transcript never mistakes a dry run for a change.
    #[serde(default, skip_serializing_if = "is_false")]
    pub dry_run: bool,
    pub status: Status,
    /// For `refused`: whether the refusal is final.
    #[serde(rename = "final", default, skip_serializing_if = "is_false")]
    pub is_final: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub change: Option<TxId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frame: Option<Frame>,
    pub final_frame: Frame,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub created: BTreeMap<String, IdVersion>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub changed: Vec<IdVersion>,
    pub count: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refusal: Option<Refusal>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff: Option<Diff>,
}

// ---- watch (spec 5.4) ----------------------------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Motion {
    /// Default 250.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub every_ms: Option<u32>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Filter {
    #[serde(default, rename = "where", skip_serializing_if = "Option::is_none")]
    pub select: Option<Selector>,
    /// Edits by anyone (default true).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub changes: Option<bool>,
    /// Fields whose new values are included with `set` items.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fields: Vec<String>,
    /// Status changes of these transactions as well as the caller's own.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub of: Vec<TxId>,
    /// Joins, leaves, catching up, finality, schema changes (default true).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<bool>,
    /// Sampled poses; needs a `where` of at most 16 ids.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub motion: Option<Motion>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub final_only: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct SetItem {
    pub id: ObjectId,
    pub field: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<Json>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ChangeItem {
    pub change: TxId,
    pub by: MemberBrief,
    pub producer: Producer,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    pub frame: Frame,
    #[serde(rename = "final")]
    pub is_final: bool,
    /// A re-step undid this change; the items delivered earlier stand as they were.
    #[serde(default, skip_serializing_if = "is_false")]
    pub retracted: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub created: Vec<IdVersion>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deleted: Vec<ObjectId>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reparented: Vec<IdVersion>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub set: Vec<SetItem>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum SessionEvent {
    Joined { member: MemberBrief },
    Left { member: MemberBrief },
    CatchingUp,
    CaughtUp,
    Final { frame: Frame },
    SchemaChanged { version: String },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct MotionSample {
    pub id: ObjectId,
    pub frame: Frame,
    pub position: Json,
    pub rotation_deg: [f64; 3],
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Changes {
    pub cursor: Cursor,
    pub changes: Vec<ChangeItem>,
    /// Status changes of the caller's own transactions (and of `Filter::of`).
    pub mine: Vec<Outcome>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub session: Vec<SessionEvent>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub motion: Vec<MotionSample>,
    /// More items wait past `limit`.
    pub more: bool,
}

// ---- history (spec 5.5) --------------------------------------------------------------------------------------------

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct HistoryQuery {
    #[serde(default, rename = "where", skip_serializing_if = "Option::is_none")]
    pub select: Option<Selector>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub member: Option<MemberId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub producer: Option<ProducerKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_frame: Option<Frame>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to_frame: Option<Frame>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub mine: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<Cursor>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct HistoryEntry {
    pub change: TxId,
    pub by: MemberBrief,
    pub producer: Producer,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    pub frame: Frame,
    #[serde(rename = "final")]
    pub is_final: bool,
    pub status: Status,
    /// Engine-written, capped: "Created Red cube under Table".
    pub summary: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct History {
    pub entries: Vec<HistoryEntry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub more: Option<Cursor>,
}

// ---- capture (spec 8) ----------------------------------------------------------------------------------------------

/// The camera. Its JSON is told apart by its first key: `player`, `orbit` or `at`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum From {
    Player {
        /// `"me"` or a member.
        player: String,
    },
    Orbit {
        orbit: ObjRef,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        yaw_deg: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pitch_deg: Option<f64>,
        /// Omitted: the object's bounds frame it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        distance_m: Option<f64>,
    },
    Placed {
        at: Json,
        look_at: Json,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        fov_deg: Option<f64>,
    },
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct CaptureRequest {
    /// Default: `{"player": "me"}`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<From>,
    /// Default `[768, 432]`, at most `[1920, 1080]`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<[u32; 2]>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub highlight: Vec<ObjRef>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub label: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub visible_list: bool,
    #[serde(default)]
    pub at: At,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct CameraUsed {
    pub at: Json,
    pub look_at: Json,
    pub fov_deg: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Visible {
    pub id: ObjectId,
    pub name: String,
    /// Pixels: x, y, width, height.
    pub rect: [u32; 4],
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Capture {
    /// The image; pe-mcp sends it as an MCP image content block, not inside the JSON.
    #[serde(skip)]
    pub png: Vec<u8>,
    pub frame: Frame,
    pub camera: CameraUsed,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub visible: Option<Vec<Visible>>,
    /// True when `visible` came from projected bounds without occlusion.
    #[serde(default, skip_serializing_if = "is_false")]
    pub visible_approx: bool,
}

// ---- code (spec 7) -------------------------------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct CodeCheck {
    /// Rust source of one script.
    pub source: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Error,
    Warning,
    Note,
}

/// rustc's own diagnostics, or pe-logic's refusals (logic.md 6).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticKind {
    Rustc,
    Import,
    Feature,
    Manifest,
    ReservedName,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Diagnostic {
    pub kind: DiagnosticKind,
    pub severity: Severity,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub column: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Diagnostics {
    pub ok: bool,
    pub diagnostics: Vec<Diagnostic>,
    /// On success: the module hash, the declared systems and component types, and the build time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub module: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub systems: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub components: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build_ms: Option<u32>,
}

// ---- errors of the call itself (spec 6.3) --------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    InvalidArgument,
    Forbidden,
    CursorExpired,
    SnapshotGone,
    CaptureUnavailable,
    RateLimited,
    Timeout,
    SessionEnded,
    Internal,
}

/// A failure of the call itself. A refused transaction is an `Outcome`, not an `ApiError`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ApiError {
    pub code: ErrorCode,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub field: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Json>,
}

impl ApiError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        ApiError { code, message: message.into(), field: None, details: None }
    }
    pub fn arg(field: &str, message: impl Into<String>) -> Self {
        ApiError { code: ErrorCode::InvalidArgument, message: message.into(), field: Some(field.into()), details: None }
    }
}

// ---- the trait (spec 5) --------------------------------------------------------------------------------------------

/// The one API over the live world. Callable from many threads at once. Only `apply` with a wait, `watch` with a
/// wait and `capture` block, each until the caller's deadline; none ever blocks the simulation thread.
pub trait WorldApi: Send + Sync {
    fn session(&self, c: &Caller) -> SessionInfo;
    fn schema(&self, c: &Caller, q: &SchemaQuery) -> Result<Schema, ApiError>;
    fn query(&self, c: &Caller, q: &Query) -> Result<QueryResult, ApiError>;
    fn apply(&self, c: &Caller, tx: Transaction, opts: &ApplyOpts) -> Result<Outcome, ApiError>;
    fn watch(&self, c: &Caller, f: &Filter, cursor: Option<&Cursor>, wait: Duration) -> Result<Changes, ApiError>;
    fn history(&self, c: &Caller, q: &HistoryQuery) -> Result<History, ApiError>;
    fn revert(&self, c: &Caller, change: TxId, opts: &ApplyOpts) -> Result<Outcome, ApiError>;
    fn capture(&self, c: &Caller, r: &CaptureRequest, deadline: Instant) -> Result<Capture, ApiError>;
    fn code_check(&self, c: &Caller, r: &CodeCheck) -> Result<Diagnostics, ApiError>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn ids_round_trip_and_reject_bad_hex() {
        let id = ObjectId([0xab; 16]);
        let s = serde_json::to_string(&id).unwrap();
        assert_eq!(s, format!("\"o:{}\"", "ab".repeat(16)));
        assert_eq!(serde_json::from_str::<ObjectId>(&s).unwrap(), id);
        assert!(ObjectId::parse("o:zz").is_none());
        assert!(ObjectId::parse(&format!("e:{}", "ab".repeat(16))).is_none());
        // One spelling per id: no sign, no uppercase.
        assert!(ObjectId::parse(&format!("o:+f{}", "ab".repeat(15))).is_none());
        assert!(ObjectId::parse(&format!("o:{}", "AB".repeat(16))).is_none());
        assert!(ObjectId::parse("o:é").is_none());
    }

    #[test]
    fn the_spec_example_transaction_parses() {
        // docs/spec/api.md 5.3.
        let tx: Transaction = serde_json::from_value(json!({
            "label": "Put a red cube on the table",
            "ops": [
                {"op": "create", "as": "$cube", "parent": "/Table", "name": "Red cube", "template": "box",
                 "components": {"Transform": {"position": {"relative_to": "parent", "offset": [0, 0.8, 0]}},
                                "Appearance": {"color": "#d22b2b"}}},
                {"op": "set", "where": {"has": "Ball"}, "max": 1000, "field": "Appearance.color", "value": "#2b5bd2"},
                {"op": "duplicate", "id": "$cube", "as": "$copy", "parent": "/Table"},
                {"op": "set", "id": "$copy", "field": "Transform.position",
                 "value": {"relative_to": "$cube", "offset": [0.3, 0, 0]}},
                {"op": "delete", "id": "o:00000000000000000000000000000001", "if_version": "v:0123456789abcdef"}
            ]
        }))
        .unwrap();
        assert_eq!(tx.base, At::Named(AtName::Latest));
        assert_eq!(tx.ops.len(), 5);
        match &tx.ops[1] {
            Op::Set { target, .. } => {
                assert_eq!(target.select, Some(Selector::Has("Ball".into())));
                assert_eq!(target.max, Some(1000));
            }
            o => panic!("{o:?}"),
        }
    }

    #[test]
    fn selectors_and_at_have_the_spec_json() {
        let q: Query = serde_json::from_value(json!({
            "at": 1234,
            "where": {"all": [{"name_like": "lamp"}, {"near": {"of": "me", "within_m": 2.0}}]},
            "fields": ["Transform"]
        }))
        .unwrap();
        assert_eq!(q.at, At::Frame(1234));
        assert!(matches!(q.select, Some(Selector::All(ref v)) if v.len() == 2));
        assert_eq!(serde_json::to_value(At::Named(AtName::Final)).unwrap(), json!("final"));
    }

    #[test]
    fn capture_cameras_are_told_apart() {
        let r: CaptureRequest = serde_json::from_value(json!({"from": {"orbit": "$cube", "yaw_deg": 30}})).unwrap();
        assert!(matches!(r.from, Some(From::Orbit { .. })));
        let r: CaptureRequest = serde_json::from_value(json!({"from": {"player": "me"}})).unwrap();
        assert!(matches!(r.from, Some(From::Player { .. })));
    }

    #[test]
    fn a_refused_outcome_serializes_flat_enough_to_read() {
        let o = Outcome {
            dry_run: false,
            status: Status::Refused,
            is_final: false,
            change: None,
            frame: Some(18350),
            final_frame: 18340,
            created: BTreeMap::new(),
            changed: vec![],
            count: 0,
            refusal: Some(Refusal {
                code: RefusalCode::StaleVersion,
                message: "Bob moved \"Lamp (left)\" after you read it.".into(),
                op: Some(4),
                object: None,
                field: Some("Transform.position".into()),
                read: Some(Version("v:0123456789abcdef".into())),
                now: Some(Version("v:fedcba9876543210".into())),
                winner: None,
                next: None,
                details: None,
            }),
            diff: None,
        };
        let v = serde_json::to_value(&o).unwrap();
        assert_eq!(v["status"], "refused");
        assert_eq!(v["refusal"]["code"], "stale_version");
        assert!(v.get("dry_run").is_none());
    }

    #[test]
    fn schemas_generate_for_every_tool_input() {
        // pe-mcp advertises these; generating them must not fail.
        for s in [
            schemars::schema_for!(Query),
            schemars::schema_for!(Transaction),
            schemars::schema_for!(ApplyOpts),
            schemars::schema_for!(Filter),
            schemars::schema_for!(HistoryQuery),
            schemars::schema_for!(CaptureRequest),
            schemars::schema_for!(SchemaQuery),
            schemars::schema_for!(CodeCheck),
        ] {
            assert!(serde_json::to_string(&s).unwrap().len() > 2);
        }
    }
    #[test]
    fn id_schemas_ask_for_two_hex_digits_per_byte() {
        let schema = schemars::schema_for!(ObjectId);
        let v = serde_json::to_value(&schema).unwrap();
        assert_eq!(v["pattern"], json!("^o:[0-9a-f]{32}$"));
        let schema = schemars::schema_for!(EntryId);
        let v = serde_json::to_value(&schema).unwrap();
        assert_eq!(v["pattern"], json!("^e:[0-9a-f]{64}$"));
    }

    #[test]
    fn s1_ids_convert_both_ways_and_keep_their_order() {
        let a = crate::ObjectId::from_u128(1);
        let b = crate::ObjectId::from_u128(1 << 100);
        // `From` is this module's history enum here, so the conversions are spelled with `into`.
        let (ta, tb): (ObjectId, ObjectId) = (a.into(), b.into());
        assert_eq!(ta.to_string(), format!("o:{}01", "0".repeat(30)));
        assert!(ta.to_string() < tb.to_string());
        let back: crate::ObjectId = tb.into();
        assert_eq!(back, b);
        let k = crate::MemberKey([7; 32]);
        let m: MemberId = k.into();
        let k2: crate::MemberKey = m.into();
        assert_eq!(k2, k);
        let l = crate::LineId::origin();
        let t: LineId = l.into();
        let l2: crate::LineId = t.into();
        assert_eq!(l2, l);
    }
}
