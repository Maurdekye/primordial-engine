//! Scenario files: the data model and loading.
//!
//! A scenario is a TOML file. Two keys are resolved before the file is read as a [`Scenario`]:
//!
//! * `include = ["a.toml", ...]`: files (relative to this one) merged underneath it, in order.
//!   Tables merge key by key, recursively; any other value, arrays included, is replaced whole.
//! * `part = [{...}, {...}]`: the file holds several runs, printed one after another into one log
//!   (the spike's `backdate` ran two). Each part is merged on top of the rest of the file.
//!
//! Units: times are milliseconds of virtual time (`_ms`) unless a field says `_us`; frames are
//! frames of the world in question. Member and world ids are written as TOML keys or integers.

use std::collections::BTreeMap;
use std::path::Path;

use serde::Deserialize;
use toml::Value;

/// One run: worlds, peers, network, faults, Byzantine behaviour and what to report.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Scenario {
    #[serde(default)]
    pub description: String,
    /// The committed log, relative to the expected folder and without `.log` (default: the file
    /// stem), e.g. `p2-5-controls/r14-doublespend`.
    pub golden: Option<String>,
    /// A control run: its log is still compared, but it is expected to show failures (e.g. NOT
    /// CONSERVED with a protection switched off), so regress does not count them.
    #[serde(default)]
    pub control: bool,
    /// A control run must print this text; without it, at least one of regress's failure markers.
    pub control_marker: Option<String>,
    /// Templated lines printed before the run (see report.rs).
    #[serde(default)]
    pub intro: Vec<String>,
    /// Keep this part's finished run under this name, for a later part's report that compares
    /// runs (`handowner`). A kept part needs no checking report of its own.
    pub keep: Option<String>,
    /// Print nothing for a panic (the spike's `run_quiet`, for a run with a fix switched off,
    /// which may trip an assertion; the panic still ends the run and is reported).
    #[serde(default)]
    pub quiet_panics: bool,
    /// Last 1 ms step at or before this virtual time (`end_us` wins if both are given).
    pub end_ms: Option<u64>,
    pub end_us: Option<u64>,
    /// Peer order inside the simulator (it ticks peers in this order). Default: every world
    /// member, ascending.
    #[serde(default)]
    pub peers: Vec<u32>,
    #[serde(default)]
    pub peer: BTreeMap<String, PeerCfg>,
    pub world: BTreeMap<String, WorldCfg>,
    pub net: NetCfg,
    #[serde(default)]
    pub sim: SimOpts,
    /// Message rules, first match wins (see [`FaultRule`]).
    #[serde(default)]
    pub fault: Vec<FaultRule>,
    #[serde(default)]
    pub crash: Vec<Crash>,
    #[serde(default)]
    pub byzantine: Vec<Byzantine>,
    /// Join requests (P2-4, design 10.8), each sent once at its time.
    #[serde(default)]
    pub join: Vec<JoinReq>,
    /// Printed in order after the run.
    #[serde(default)]
    pub report: Vec<Report>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PeerCfg {
    /// Clock skew of this peer (microseconds, signed).
    #[serde(default)]
    pub skew_us: i64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorldCfg {
    /// The genesis members. `members_range = [a, b]` stands for `members = [a, ..., b]`.
    pub members: Vec<u32>,
    /// Peers that follow the world from frame 0 without being members (they may ask to join):
    /// they replicate it with `members` as its membership, and every member follows them.
    #[serde(default)]
    pub joiners: Vec<u32>,
    pub scene: SceneCfg,
    /// Virtual time of the world's frame 0.
    pub epoch_ms: u64,
    /// Frame rate; dt = 1 / hz (f32). Default 60.
    pub hz: Option<u64>,
    // SimConfig overrides (session.rs `SimConfig`); omitted fields keep `SimConfig::default()`.
    pub window: Option<u64>,
    pub seed: Option<u64>,
    pub horizon_slack: Option<u64>,
    pub check_every: Option<u64>,
    pub hand_predict: Option<u64>,
    pub suspect_after: Option<u64>,
    /// D80 quorum rule: `bft` (the default) or `majority`.
    pub quorum: Option<String>,
    /// Acks carry heads (default true; false measures the view change alone, design 25.5.2).
    pub ack_heads: Option<bool>,
    /// Every replica acks every this many frames (default: the replica's own default).
    pub ack_every_frames: Option<u64>,
    /// Every member follows the joiners (default true; false: they learn of a joiner only from
    /// its request).
    pub follow_joiners: Option<bool>,
    /// Review repros: these fixes switched off on every replica (`fix::VC2` etc., by name).
    #[serde(default)]
    pub fixes_off: Vec<String>,
    /// D80: define role `role` with no permissions and give it to every member and joiner, except
    /// `admins`, who keep ADMIN (the spike's `guests`).
    pub guests: Option<GuestsCfg>,
    /// D80 / V1-V2: roles defined at genesis, as `[role, perms]` pairs (after `guests`).
    #[serde(default)]
    pub role_defs: Vec<(u16, u64)>,
    /// D80 / V1-V2: members' roles at genesis, as `[member, role]` pairs (after `guests`).
    #[serde(default)]
    pub roles: Vec<(u32, u16)>,
    /// D80: the world's owners.
    #[serde(default)]
    pub owners: Vec<u32>,
    /// D80: the join rules (default: the replica's).
    pub join_rules: Option<JoinRulesCfg>,
    /// R14 genesis route list: the worlds this one trades objects with.
    #[serde(default)]
    pub routes: Vec<u32>,
    /// Each member's planned actions in this world, by member id.
    #[serde(default)]
    pub plan: BTreeMap<String, Vec<ActionSpec>>,
    /// Per-member replica settings in this world.
    #[serde(default)]
    pub member: BTreeMap<String, ReplicaCfg>,
    /// World ownership (design 12.5): the genesis certificate's owner keys (test identities, by
    /// number; all sign it), rule and nonce.
    pub ownership: Option<OwnersCfg>,
    /// 12.4: `[member, identity]` pairs: the member signs with test identity `identity`'s key (a
    /// player back under a new member id).
    #[serde(default)]
    pub keys: Vec<(u32, u32)>,
    /// 12.4: the world's saved perma-bans the session starts from (test identities).
    #[serde(default)]
    pub perma_bans: Vec<u32>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OwnersCfg {
    pub keys: Vec<u32>,
    /// `all` (rule A, the default), `any` (B), or a threshold such as `2` (C).
    #[serde(default = "all")]
    pub rule: String,
    #[serde(default)]
    pub nonce: u64,
}

fn all() -> String {
    "all".into()
}

/// An ownership certificate to publish: `change` is `add`, `leave`, `remove` (rule C only, with
/// `key`), `set_recovery` (`key` names the owner, `recovery` its backup key or none), `set_rule`
/// (`rule`) or `merge`; `signers` sign it (an owner's recovery key acts as the owner). Keys are test
/// identities, by number; member m's key is identity m.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OwnerSpec {
    pub change: String,
    pub key: Option<u32>,
    pub recovery: Option<u32>,
    pub rule: Option<String>,
    pub signers: Vec<u32>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub enum SceneCfg {
    Pile { side: usize, layers: usize },
    Ballpit { balls: usize, hands: usize },
    /// No bodies: a log-only world (`scene = "empty"`).
    Empty,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GuestsCfg {
    pub role: u16,
    pub admins: Vec<u32>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JoinRulesCfg {
    #[serde(default)]
    pub rules: Vec<JoinRuleCfg>,
    pub fallback: Option<u16>,
}

/// One join rule: a joiner with relation `rel` (`friend`, `confidant`, `invited`) to a `target`
/// (`player`, `admin`, `owner`), or to `all` of them, gets `role`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JoinRuleCfg {
    pub target: String,
    #[serde(default)]
    pub all: bool,
    pub rel: String,
    pub role: Option<u16>,
}

/// A join request: `member` sends `Msg::Join` to each of `to` at `at_ms`. Members listed in
/// `attests_to` get `attests`; the others get none (default: every recipient gets them).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JoinReq {
    pub at_ms: u64,
    #[serde(default = "one")]
    pub world: u32,
    pub member: u32,
    /// Sent by this member in `member`'s name (default: `member` itself).
    pub from: Option<u32>,
    /// The request's signature (default: `member`'s genuine one; 0 for a forged request).
    pub sig: Option<u64>,
    pub to: Vec<u32>,
    #[serde(default)]
    pub attests: Vec<AttestCfg>,
    pub attests_to: Option<Vec<u32>>,
    /// Right after the request, this member sends the joiner every entry it holds (so it catches
    /// up within a tick).
    pub log_from: Option<u32>,
}

/// An attestation of relation `rel` (`friend`, `confidant`, `invited`) between the joiner and
/// `with`, signed for `world`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AttestCfg {
    pub rel: String,
    pub with: u32,
    #[serde(default = "one")]
    pub world: u32,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReplicaCfg {
    /// Background world (design 22.3): advance only every `throttle_ms`, from `throttle_from_ms`.
    pub throttle_ms: Option<u64>,
    #[serde(default)]
    pub throttle_from_ms: u64,
    /// R18 congestion control on this member's uplink (on by default in pe-session; the tool's test
    /// IO opts in per member, as the harness does).
    #[serde(default)]
    pub adapt: bool,
    /// Ack every this many ms (default: the world's).
    pub ack_every_ms: Option<u64>,
    /// Test fixture (7.6 reduction 2, review VCR-12): ack only on the member's own interval, never at
    /// once on leaving idle (session-sim p24r3.rs `lagprecommit`).
    #[serde(default)]
    pub ack_grid_only: bool,
    /// Byzantine: casts no view-change entry (session.rs `withhold_vc`).
    #[serde(default)]
    pub withhold_vc: bool,
    /// Byzantine: backs every vote, prevotes and precommits every proposal, echoes nil votes and
    /// applies no decision (session.rs `byz_ally`).
    #[serde(default)]
    pub byz_ally: bool,
    /// Byzantine: prevotes but never precommits (session.rs `byz_no_precommit`).
    #[serde(default)]
    pub byz_no_precommit: bool,
    /// A joiner that authors no ack before it is admitted (session.rs `quiet_until_member`).
    #[serde(default)]
    pub quiet_until_member: bool,
}

/// A planned action: at frame `at`, or `count` times from `at` every `every` frames.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActionSpec {
    pub at: Option<u64>,
    pub every: Option<u64>,
    pub count: Option<u64>,
    pub touch: Option<TouchSpec>,
    pub hand: Option<HandSpec>,
    pub portal: Option<PortalSpec>,
    pub carry: Option<CarrySpec>,
    /// Byzantine: a touch stamped `back` frames before the present (design review H5).
    pub backdated: Option<BackdatedSpec>,
    /// The ball-pit hand gesture of session-sim's `ballpit_sweep` (no `at`).
    pub ballpit_sweep: Option<SweepSpec>,
    /// session-sim `handowner`'s hand gesture (no `at`): every frame from `from_frame` to
    /// `to_frame`, move hand `obj` in a straight line from x = `from_x` to `to_x` m (y `y_mm`,
    /// z `z` m), rounded to mm as the spike does.
    pub hand_path: Option<HandPathSpec>,
    /// D80 admin entries: kick a member, give a member a role, define a role.
    pub kick: Option<KickSpec>,
    pub set_role: Option<SetRoleSpec>,
    pub define_role: Option<DefineRoleSpec>,
    /// World ownership: publish a certificate.
    pub owner: Option<OwnerSpec>,
    /// 12.4: a ban entry.
    pub ban: Option<BanSpec>,
    /// Accept this action on an object the scene lacks (the protocol rejects it as gone). Only
    /// for reproducing a spike scenario that did this; say why next to it.
    #[serde(default)]
    pub missing_ok: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TouchSpec {
    pub obj: u32,
    pub impulse: [f32; 3],
    #[serde(default)]
    pub grab: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HandSpec {
    pub obj: u32,
    pub target_mm: [i32; 3],
    pub active: bool,
    #[serde(default)]
    pub ahead: u64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PortalSpec {
    pub open: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CarrySpec {
    pub transfer: u64,
    pub obj: u32,
    pub dest: u32,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackdatedSpec {
    pub obj: u32,
    pub impulse: [f32; 3],
    pub back: u64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HandPathSpec {
    pub obj: u32,
    pub from_frame: u64,
    pub to_frame: u64,
    pub from_x: f64,
    pub to_x: f64,
    pub y_mm: i32,
    pub z: f64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KickSpec {
    pub target: u32,
    /// 12.4 kick options: remove Admin (rejoin with this role), ban for the session, or perma-ban.
    pub remove_admin: Option<u16>,
    #[serde(default)]
    pub ban: bool,
    #[serde(default)]
    pub perma: bool,
}

/// 12.4: ban (or, `on = false`, lift the ban of) test identity `key`'s key, for the session or
/// (`perma`) the world.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BanSpec {
    pub key: u32,
    #[serde(default = "yes")]
    pub on: bool,
    #[serde(default)]
    pub perma: bool,
}

fn yes() -> bool {
    true
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetRoleSpec {
    pub member: u32,
    pub role: u16,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DefineRoleSpec {
    pub role: u16,
    pub perms: u64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SweepSpec {
    /// Input delay: entries stamped this many frames ahead.
    #[serde(default)]
    pub ahead: u64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetCfg {
    /// Seed of the link RNG (loss and jitter draws).
    pub seed: u64,
    /// Every directed link not listed in `links`.
    pub link: LinkCfg,
    /// Asymmetric or per-pair links.
    #[serde(default)]
    pub links: Vec<PairLink>,
    /// Bandwidth cap on every directed link not listed in `caps` (none = unlimited).
    pub cap: Option<CapCfg>,
    #[serde(default)]
    pub caps: Vec<PairCap>,
    /// R18: one datagram per member, peer and tick (up to the MTU). Opt-in for the test IO.
    #[serde(default)]
    pub batching: bool,
    /// Count traffic only in [from, to) ms (the ball-pit sweep window).
    pub count_ms: Option<[u64; 2]>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LinkCfg {
    pub delay_ms: u64,
    #[serde(default)]
    pub jitter_ms: u64,
    #[serde(default)]
    pub loss_pct: u32,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PairLink {
    pub from: u32,
    pub to: u32,
    pub delay_ms: u64,
    #[serde(default)]
    pub jitter_ms: u64,
    #[serde(default)]
    pub loss_pct: u32,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapCfg {
    pub kbps: u64,
    /// Bucket size; default 4000 B (as in the R18 runs).
    pub burst_bytes: Option<u64>,
    /// Tail-drop a datagram that would queue longer; default 500 ms.
    pub max_queue_ms: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PairCap {
    pub from: u32,
    pub to: u32,
    pub kbps: u64,
    pub burst_bytes: Option<u64>,
    pub max_queue_ms: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SimOpts {
    /// Tick peers in reverse order (order-independence check).
    #[serde(default)]
    pub reverse_order: bool,
    /// R14: a carrier's deadline in destination frames after its present. Default 600.
    pub carry_deadline_frames: Option<u64>,
    /// P2-5: arrivals check provenance chains (default true; false only for a control run).
    pub provenance: Option<bool>,
}

/// One or several member ids.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum Ids {
    One(u32),
    Many(Vec<u32>),
}

impl Ids {
    pub fn contains(&self, m: u32) -> bool {
        match self {
            Ids::One(x) => *x == m,
            Ids::Many(v) => v.contains(&m),
        }
    }
}

/// A message rule, checked when a message is sent. Every condition given must hold; the first
/// rule that matches (and has matches left) decides the message's fate.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FaultRule {
    /// Send time at or after (ms).
    pub after_ms: Option<u64>,
    /// Send time before (ms).
    pub before_ms: Option<u64>,
    pub world: Option<u32>,
    pub from: Option<Ids>,
    pub to: Option<Ids>,
    /// Sent by or to this member.
    pub involves: Option<Ids>,
    /// Chain entries authored by this member (whoever sends them, fetch replies included).
    pub author: Option<Ids>,
    /// Sender and receiver in different groups (members in no group are unaffected).
    pub partition: Option<Vec<Vec<u32>>>,
    /// Message class: `entry` (any chain entry), `fetch`, `batch`, or an entry payload: `touch`,
    /// `hand`, `ack`, `vote`, `propose`, `prevote`, `precommit`, `kick`, `setrole`, `definerole`,
    /// `attest`, `depart`, `arrive`, `verdict`, `portal`.
    pub msg: Option<String>,
    /// Chain entries only: this stamp, this sequence number.
    pub stamp: Option<u64>,
    pub seq: Option<u32>,
    /// `propose`, `prevote` or `precommit` entries of this round only.
    pub round: Option<u32>,
    /// `prevote` or `precommit` entries for nil (value 0), or (`false`) for a value.
    pub nil: Option<bool>,
    /// Apply to at most this many messages, then stop matching.
    pub times: Option<u64>,
    #[serde(default)]
    pub drop: bool,
    pub delay_ms: Option<u64>,
    /// With `delay_ms` and `after_ms`: the delay grows with the send time t, to
    /// t - after_ms + delay_ms (a link growing slower).
    #[serde(default)]
    pub delay_grows: bool,
    /// Deliver it as is (so a later rule does not match it).
    #[serde(default)]
    pub deliver: bool,
}

/// A peer stops (receives, ticks and sends nothing) at `at_ms`, and resumes at `recover_ms`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Crash {
    pub member: u32,
    pub at_ms: u64,
    pub recover_ms: Option<u64>,
}

/// When an equivocation fires.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub enum When {
    /// As soon as the entry exists.
    Exists,
    /// Once member `at`'s confirmed frame exceeds `frame`.
    ConfAbove { at: u32, frame: u64 },
    /// At this virtual time, or as soon after it as the entry exists.
    AtMs(u64),
}

/// Byzantine behaviour. `name` lets a later behaviour start only `after` an earlier one fired.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, tag = "kind", rename_all = "snake_case")]
pub enum Byzantine {
    /// `member` signs a second version of one of its own entries (payload replaced, no carried
    /// state) and sends it straight to `to`, once.
    Equivocate {
        name: Option<String>,
        after: Option<String>,
        #[serde(default = "one")]
        world: u32,
        member: u32,
        /// Which of its entries: `first` (any payload) or `first_touch`.
        entry: String,
        when: When,
        touch: TouchSpec,
        to: Vec<u32>,
        /// The second version never reaches these members (relayed or fetched), until
        /// `hide_until_ms` if given.
        #[serde(default)]
        hide_from: Vec<u32>,
        hide_until_ms: Option<u64>,
        /// Printed when it fires (see the template variables in report.rs).
        say: Option<String>,
    },
    /// `member` shows `to` a second chain of its own, of acks only, one every `every_ms`, and
    /// (with `hide_real`) `to` never receives its real chain from it.
    ForkChain {
        name: Option<String>,
        #[serde(default = "one")]
        world: u32,
        member: u32,
        to: Vec<u32>,
        every_ms: u64,
        #[serde(default)]
        hide_real: bool,
    },
    /// `member` re-signs its first vote with frozen head `frozen` and sends that copy to `to`;
    /// with `hide_real`, its genuine votes reach nobody.
    ForkVote {
        name: Option<String>,
        after: Option<String>,
        #[serde(default = "one")]
        world: u32,
        member: u32,
        frozen: [u64; 2],
        to: Vec<u32>,
        #[serde(default)]
        hide_real: bool,
        say: Option<String>,
    },
    /// `member`'s votes carry a forged frozen head (session.rs `lie_in_votes`).
    LieInVotes {
        #[serde(default = "one")]
        world: u32,
        member: u32,
    },
    /// `member` publishes ack horizons `frames` behind the truth (session.rs `horizon_lag`).
    LagHorizon {
        #[serde(default = "one")]
        world: u32,
        member: u32,
        frames: u64,
    },
    /// The R14 forgeries of reviews NEW-3..NEW-21, R3-4..R3-8, each at its time.
    R14Forgeries { attacks: Vec<R14Attack> },
    /// From `from_ms`, `member`'s acks reference no more of anyone's chain (session.rs
    /// `freeze_others`); at `until_ms` it references them again and, with `ally_until`, stops
    /// backing everything (`byz_ally` off).
    HideRefs {
        #[serde(default = "one")]
        world: u32,
        member: u32,
        from_ms: u64,
        until_ms: Option<u64>,
        #[serde(default)]
        ally_until: bool,
    },
    /// `member`, the first proposer, shows `to` a reset in place of its first proposal (epoch 0,
    /// round 0); `to` never receives its genuine proposals.
    ForgeProposal {
        #[serde(default = "one")]
        world: u32,
        member: u32,
        to: Vec<u32>,
    },
    /// Review VC-2's fork (session-sim p24r.rs `fork_run`): once `member`'s frame reaches
    /// `at_frame` it forks its chain. Branch A, its real chain, goes on to `a_to`; `b_to` gets
    /// branch B from the fork on (the same entries re-linked, its touches turned into no-ops) and,
    /// after `live_ms`, ten more acks on B only, so B is longer. From `live_ms` after the fork it is
    /// silent. With `backdate`, it signs, on A only, a touch backdated 90 frames 100 ms after the
    /// fork. With `detect_ms`, it does not relay B to `a_to`, and everything `b_to` sends them in
    /// the `detect_ms` after the fork arrives that late, so they find the fork late.
    ForkBranch {
        member: u32,
        at_frame: u64,
        a_to: Vec<u32>,
        b_to: u32,
        live_ms: u64,
        #[serde(default)]
        backdate: bool,
        #[serde(default)]
        detect_ms: u64,
    },
    /// From `from_ms` (while `member` is a member at `while_member_at`, if given), `member` signs a
    /// precommit for no proposal and sends it to `to`: once, in epoch 0, round 0, value `value`;
    /// or, with `every_epoch`, once in every epoch it enters, in that epoch, value `value ^ epoch`.
    /// Its boundary is `boundary`, or `ahead` frames past its clock.
    Precommit {
        member: u32,
        from_ms: u64,
        value: u64,
        boundary: Option<u64>,
        ahead: Option<u64>,
        to: Vec<u32>,
        #[serde(default)]
        every_epoch: bool,
        while_member_at: Option<u32>,
        /// The precommit's polka (prevote ids), as hex strings ("0xDEADBEEF00000001": TOML integers
        /// stop at i64): ids that need not exist (review VC6R-1).
        #[serde(default)]
        polka: Vec<String>,
    },
    /// From `from_ms` (while `member` is a member at `while_member_at`, if given), `member`'s acks
    /// claim epoch `epoch`, or `ahead` epochs past its own (session.rs `fake_ack_epoch`).
    FakeAckEpoch {
        member: u32,
        from_ms: u64,
        epoch: Option<u32>,
        ahead: Option<u32>,
        while_member_at: Option<u32>,
    },
    /// Review HO-1 (session-sim `handowner`): `member` sends `to` a grab of `hand` (another
    /// member's hand) at `grab_ms`, a departure of it at `carry_ms`, and at `plant_ms` a touch of
    /// ball 5 carrying a state for `hand` 2 m above where `member` sees it.
    HandForgery {
        member: u32,
        hand: u32,
        to: u32,
        grab_ms: u64,
        carry_ms: u64,
        plant_ms: u64,
    },
    /// At `at_ms`, `member` votes against `target` for a kick `kick` that does not exist, with
    /// `target`'s head as it holds it as the frozen head; sent to `to`.
    GhostVote {
        member: u32,
        at_ms: u64,
        target: u32,
        kick: u64,
        to: Vec<u32>,
    },
    /// Once `member` holds a kick against itself, it votes on it naming a head it never signed,
    /// `frozen`; sent to `to`.
    SelfKickVote {
        member: u32,
        frozen: [u64; 2],
        to: Vec<u32>,
    },
    /// From `from_ms`, `member` sends each of `to` only its own entries whose
    /// sequence number modulo the length of `to` is that member's index in it.
    RotateEntries {
        member: u32,
        from_ms: u64,
        to: Vec<u32>,
    },
    /// From `from_ms` until `until_ms` (while a member at `while_member_at`), in every epoch it is
    /// in, `member` votes against `target` (whom everyone hears), naming `target`'s head as it holds
    /// it, so an honest proposer answers with a reset; sent to `to`.
    ChurnVote {
        member: u32,
        target: u32,
        from_ms: u64,
        until_ms: Option<u64>,
        while_member_at: Option<u32>,
        to: Vec<u32>,
    },
    /// Review VC3R-3: at `at_ms`, `member` signs a SetRole giving `target` role `role`, backdated
    /// `back` frames (so every replica voids it), proposes a change listing it in every round it
    /// proposes (`byz_change`), and starts a view change with a nil prevote; sent to `to`.
    VoidAdmin {
        member: u32,
        at_ms: u64,
        target: u32,
        role: u16,
        back: u64,
        to: Vec<u32>,
    },
    /// From `from_ms`, `member`'s acks claim `n` more entries of `of`'s chain than it holds
    /// (session.rs `fake_refs`).
    FakeRefs {
        member: u32,
        from_ms: u64,
        of: u32,
        n: u32,
    },
    /// From `from_ms`, once `member`'s replica holds an entry of its own of kind `holds_own`
    /// (`prevote`, or `nonnil_prevote`), every message it sends is lost (the spike's crash after a
    /// prevote: it still receives and ticks).
    GoSilent {
        member: u32,
        from_ms: u64,
        holds_own: String,
    },
    /// Review VC5R-2: from `from_ms`, every `every_ms` `member` alternates between acks naming a
    /// fake id at its current seq of `of`'s chain (`fake_refs` with n = 0) and acks naming a stale
    /// real head; the stale heads are re-frozen (`freeze_others`) at a fake turn at least
    /// `refresh_ms` after the last freeze.
    MixRefs {
        member: u32,
        of: u32,
        from_ms: u64,
        every_ms: u64,
        refresh_ms: u64,
    },
    /// Review VC5R-1: at `at_ms`, faulty admin `member` forks one seq: a SetRole giving `target`
    /// role `role`, sent to `a_to`, and an Attest in its place, sent to `b_to` (who never receive
    /// the SetRole from it). It proposes a change listing the SetRole in every round it proposes
    /// (`byz_change`) and sends a nil prevote to `prevote_to`.
    AdminFork {
        member: u32,
        at_ms: u64,
        target: u32,
        role: u16,
        a_to: Vec<u32>,
        b_to: Vec<u32>,
        prevote_to: Vec<u32>,
    },
    /// Review VC4R-1: `member`, the round-0 proposer, equivocates its round-0 prevote once it holds
    /// it and its round-0 proposal (from `from_ms`): the proposal's value to `value_to`, nil to
    /// `nil_to`. Its genuine round-0 prevotes reach nobody.
    EquivocatePrevote {
        member: u32,
        from_ms: u64,
        value_to: Vec<u32>,
        nil_to: Vec<u32>,
        /// Review VC7R-2 (`slowcapper`): every delivery of the forked value-prevote to `slow_to`,
        /// from anyone, is delayed a further `slow_ms`.
        #[serde(default)]
        slow_to: Vec<u32>,
        #[serde(default)]
        slow_ms: u64,
    },
    /// Reviews VC7R-1 and VC8R-1 (session-sim p24r4.rs `voidfork` and `fork_segment_sim`): at
    /// `at_ms`, `member` (the round-0 proposer) forks its chain. Its real branch B goes to `others`
    /// first and to `p` at `swap_ms`; a forged branch A to `p` first and to `others` at `swap_ms`.
    /// `segment`:
    /// - "void_one": B is an Attest (fact 1); A a SetRole giving `p` role `role` at the same seq,
    ///   stamped `back` frames behind it (`voidfork`), listed in `member`'s change;
    /// - "void_role" / "valid_role": B is two Attests (facts 1, 2); A an ack declaring horizon
    ///   B1's stamp + 40 (for "valid_role" claiming epoch + 2, so it counts for nothing) and a
    ///   SetRole giving `p` role `role`, stamped B1's + 10, listed in `member`'s change;
    /// - "plain": B is two Attests; A one Attest (fact 3) at B1's seq; `member` proposes an empty
    ///   change.
    /// Every precommit it sends is lost (a fault rule's), and once every one of `others` holds its
    /// own precommit (or at `crash_by_ms`) it crashes: everything it sends is lost. Its own replica
    /// runs with `own_fixes_off` switched off as well (its code skips the admin checks).
    ForkSegment {
        member: u32,
        at_ms: u64,
        swap_ms: u64,
        p: u32,
        others: Vec<u32>,
        segment: String,
        #[serde(default)]
        role: u16,
        #[serde(default)]
        back: u64,
        #[serde(default)]
        crash_by_ms: Option<u64>,
        #[serde(default)]
        own_fixes_off: Vec<String>,
    },
    /// Review VC6R-4: from `from_ms` (while a member at `while_member_at`, if given), every
    /// `every_ms` `member` signs a no-op SetRole giving itself role `role`, sent to `to`.
    NoopSetrole {
        member: u32,
        from_ms: u64,
        every_ms: u64,
        role: u16,
        to: Vec<u32>,
        while_member_at: Option<u32>,
    },
    /// Review VC6R-3 (session-sim p24r4.rs `adminwedge`): at `at_ms`, `member` signs a SetRole giving
    /// itself role `role`, sent to `a_to`, and keeps a fork of it (an Attest at the same seq). Its
    /// precommits are a fault rule's. At the next millisecond on which every one of `others` holds its
    /// own precommit, every message it sends is lost (it crashes), apart from the fork, which it sends
    /// `part` at `part_ms` and `others` at `others_ms`, through any fault. `part` must be the
    /// spike's: 4, or 3 if 4 is the round-0 proposer.
    AdminWedge {
        member: u32,
        at_ms: u64,
        role: u16,
        a_to: Vec<u32>,
        part: u32,
        others: Vec<u32>,
        part_ms: u64,
        others_ms: u64,
    },
    /// Once `member`'s frame reaches `at_frame`, it signs a kick of `target` backdated `back`
    /// frames; sent to `to`.
    BackdatedKick {
        member: u32,
        at_frame: u64,
        target: u32,
        back: u64,
        to: Vec<u32>,
    },
}

/// Who a report covers: a list of members, or `{ members_at = M }`, the membership member M's
/// replica of world 1 holds at the end of the run, or `{ members_at = M, among = [...] }`, those of
/// `among` in it.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum Honest {
    List(Vec<u32>),
    Among { members_at: u32, among: Vec<u32> },
    At { members_at: u32 },
}

fn one() -> u32 {
    1
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct R14Attack {
    pub at_ms: u64,
    /// One of byzantine.rs `R14_ATTACKS`.
    pub attack: String,
}

/// What to print after the run, in order.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, tag = "kind", rename_all = "snake_case")]
pub enum Report {
    /// The phase-1 rule's failure that the scenario fixes (`c1` or `c2`), on the old RollbackSim.
    Phase1Rule { case: String },
    /// Every replica of `world` and an offline replay of its final log agree over `frames`.
    CheckWorld {
        #[serde(default = "one")]
        world: u32,
        frames: u64,
        label: String,
        #[serde(default)]
        skip: Vec<u32>,
        /// Also skip the members that member `skip_evicted_at` removed from the session, and
        /// compare only frames every remaining member has simulated (at most `frames`).
        skip_evicted_at: Option<u32>,
    },
    /// World ownership: each replica's owner set and outcomes against an offline reference built from
    /// the decided changes (heads, dispute, certificates never committed, fork proofs).
    Ownership {
        #[serde(default = "one")]
        world: u32,
        frames: u64,
        label: String,
        /// Also print the reference's view after the decisions up to these frames.
        #[serde(default)]
        at: Vec<u64>,
    },
    /// Rollbacks, re-sims, checks and so on per replica of `world`.
    Stats {
        #[serde(default = "one")]
        world: u32,
    },
    /// Events of `world` containing `filter`.
    Events {
        #[serde(default = "one")]
        world: u32,
        filter: String,
    },
    /// Acceptance checks of state-carrying entries (user ruling 09:31Z).
    Acceptance { label: String },
    /// Entry and ack bytes in `world`, and drops.
    Traffic {
        #[serde(default = "one")]
        world: u32,
    },
    /// Each replica's effective frame for `author`'s first entry.
    FirstEntryFrame {
        #[serde(default = "one")]
        world: u32,
        author: u32,
        text: String,
    },
    /// `author`'s touches as member `at` holds them: stamp, effective frame, void.
    Entries {
        #[serde(default = "one")]
        world: u32,
        at: u32,
        author: u32,
        prefix: String,
    },
    /// Members member `at` removed from the session, if any.
    Evicted {
        #[serde(default = "one")]
        world: u32,
        at: u32,
        label: String,
    },
    /// Accepted entries per member (`wins`), or accepted and rejected per member (`tally`).
    Outcomes {
        #[serde(default = "one")]
        world: u32,
        at: u32,
        mode: String,
        prefix: String,
    },
    /// `author`'s touches at each member of `at` (`{m}` in `text` is that member).
    Touches {
        #[serde(default = "one")]
        world: u32,
        author: u32,
        at: Vec<u32>,
        text: String,
    },
    /// Honest members agree with each other and an offline replay, no false checks, no panic.
    Verdict { honest: Honest, label: String },
    /// Membership, evictions, confirmation and paused frames per honest member.
    Membership { honest: Honest, label: String },
    /// P2-4: every decision each member took (epoch, frame, round, removed).
    ViewChanges { honest: Honest, label: String },
    /// P2-4 join: each member's admissions and `joiner`'s touches.
    Admitted { honest: Vec<u32>, joiner: u32 },
    /// D80: per member, the admin entries' outcomes (`RESULT {label}: m{m}: ...`) and/or its
    /// members, roles, admins and close frame.
    Admin {
        honest: Honest,
        label: String,
        #[serde(default)]
        outcomes: bool,
        #[serde(default)]
        roles: bool,
        /// 12.4: also each member's session bans and perma-bans.
        #[serde(default)]
        bans: bool,
    },
    /// Each member's confirmed and simulated frame on one line (`RESULT {label}: m1 conf ...`).
    Confs { honest: Vec<u32>, label: String },
    /// The line a run with its fix switched off ends with: whether it stopped, or every replica
    /// of `honest` equals the replay; their frames; and the first one's members and evictions.
    BeforeFix { honest: Honest, label: String, fix: String },
    /// `fork_branch`: where the fork fell and how long branch B is.
    ForkInfo,
    /// Review HO-1 (session-sim `handowner`), over three kept runs: with `member`'s forgeries of
    /// hand `hand` (`forged`), without them (`clean`), and with them and HO-1's fix switched off
    /// (`fixoff`). Every forgery refused at every replica, and the same history as without them.
    Handowner {
        forged: String,
        clean: String,
        fixoff: String,
        member: u32,
        hand: u32,
        label: String,
    },
    /// The spike's one-off checks, ported verbatim (report.rs `check`); each reads the fields it
    /// names and nothing else:
    /// * `bridge`: `label`, `text` (the verdict label if the session stayed whole), `honest`,
    ///   `sides` (two groups, judged on their own if it forked; the first of each is compared).
    /// * `kickwar`: `label`, `at` (whose membership is left), `members` (the two admins).
    /// * `kickpartition`: `label`, `members` (the two compared), `target`, `until_ms` (the heal).
    /// * `lastadmin`: `label`, `honest`, `members` (the admins expected at the end).
    /// * `adminclose`: `label`, `text`, `honest` (two members).
    /// * `joinrules`: `label`, `members` (friend, stranger, confidant, owner), `role` (guest).
    /// * `vcscale`: `silent` (the last members), `from_ms` (when their silence starts).
    /// * `selfkickhead`, `capchurn`: `label`, `at`, `target` (when `at` removed it).
    /// * `joinattests`: `label`, `at`, `target` (the joiner), `role`, `honest` (for the frames).
    /// * `slowadmin`: `label`, `honest` (each one's role definitions).
    /// * `kickbackdate`: `label`, `at`, `members` (the members expected to be left).
    /// * `lagprecommit`: `label`, `at`, `target` (the joiner), `honest` (for the frames).
    /// * `aheadlink`: `label`, `at` (whose votes), `members` (the members ahead, kept at the first
    ///   of `honest`), `honest`.
    /// * `onesided`: `members` (the two whose first decisions are compared).
    /// * `removed`: `label`, `at`, `target` (when `at` removed it), `text` (the word for success).
    /// * `strikeframe`: `text` (the timing), `at`, `target` (when `at` removed it), `members` (the
    ///   member that must be kept). `strikeframe_total`: `runs`, `at`, `members`.
    /// * `aheadshield`: `text` (the variant), `at`, `target`, `honest`, `expect` (`voted_out`, or
    ///   `harmless`: not removed and `at`'s confirmed frame above `conf_min`).
    /// * `fakeref`, `capstale`, `lagchurn`: `label`, `at`, `target`, `event`, `honest`.
    /// * `votechurn`, `votechurn_off`: `at`, `target`, `from_ms` and `until_ms` (the churn),
    ///   `honest`. `votechurn` samples `at`'s confirmation lag every 100 ms from 0.5 s.
    /// * `adminvoid`: `label`, `at`, `target` (whose role stays ADMIN), `honest`.
    /// * `joinskip`: `label`, `at`, `target` (the joiner), `honest`.
    Check {
        case: String,
        #[serde(default)]
        label: String,
        #[serde(default)]
        text: String,
        #[serde(default)]
        honest: Vec<u32>,
        #[serde(default)]
        sides: Vec<Vec<u32>>,
        at: Option<u32>,
        #[serde(default)]
        members: Vec<u32>,
        target: Option<u32>,
        until_ms: Option<u64>,
        role: Option<u16>,
        silent: Option<u32>,
        from_ms: Option<u64>,
        /// Kept runs (a part's `keep`) the check counts over.
        #[serde(default)]
        runs: Vec<String>,
        /// Text of an event the check counts at `at`.
        event: Option<String>,
        /// `aheadshield`: `voted_out` or `harmless`.
        expect: Option<String>,
        conf_min: Option<u64>,
    },
    /// A condition on member `at`'s replica of `world`: confirmed frame within bounds and/or this
    /// exact membership. Prints OK or FAIL.
    Expect {
        #[serde(default = "one")]
        world: u32,
        at: u32,
        conf_max: Option<u64>,
        conf_min: Option<u64>,
        members: Option<Vec<u32>>,
        /// 12.4: these members hold these roles, as `[member, role]`.
        roles: Option<Vec<(u32, u16)>>,
        /// 12.4: exactly these rejoin roles, as `[test key, role]`.
        rejoin_roles: Option<Vec<(u32, u16)>>,
        label: String,
    },
    /// One templated line (see report.rs).
    Line {
        #[serde(default = "one")]
        world: u32,
        text: String,
    },
    /// R14: outcome of each forged entry.
    R14Forged,
    /// R14: carrier and relayer job logs.
    R14Jobs,
    /// R14 crash: membership of each world at a surviving member, `[world, member]` pairs.
    R14Crash { at: Vec<[u32; 2]> },
    /// R14: object 40 is conserved (exactly one live copy, no pending escrow).
    R14Conservation { label: String },
    /// R14 forge (and doublespend): with `original`, the R3/NEW forged objects did not multiply;
    /// with `attacks`, that many forgeries were tried; then P2-5's dishonest-majority W check in
    /// the honest worlds A and B, and the fork proofs A and B hold.
    R14ForgeConservation {
        label: String,
        #[serde(default)]
        original: bool,
        attacks: Option<usize>,
        #[serde(default)]
        control: bool,
    },
    /// R14 chain (P2-5): every member's job logs.
    R14ChainJobs,
    /// R14 chain: each departure's and arrival's outcome at the carrier, in worlds A, B and C.
    R14ChainOutcomes,
    /// R14 chain: A:40 is back in A, one live copy, nothing pending.
    R14ChainConservation { label: String },
    /// R14 chain-epoch (P2-4 x P2-5): the job logs; A's epoch at member 2; each provenance hop
    /// relayed into B and C with its epoch, frame and epoch certificates; each arrival's chain and
    /// outcome; and A:40 conserved in C.
    R14ChainEpoch,
    /// R14: member 2's confirmation lag in A and how late check hashes come (sampled every 100 ms).
    R14Lag { label: String, throttled: bool },
    /// Ball pit: wire cost of the perturbation source vs naive state streaming, member 2's
    /// rollbacks during the sweep, and the end state.
    Ballpit { baseline_ms: [u64; 2] },
}

/// A scenario file resolved into its runs.
pub struct Loaded {
    pub parts: Vec<Scenario>,
}

/// Merge `top` onto `base`: tables key by key, recursively; anything else replaced.
pub fn merge(base: &mut Value, top: Value) {
    match (base, top) {
        (Value::Table(b), Value::Table(t)) => {
            for (k, v) in t {
                match b.get_mut(&k) {
                    Some(bv) if bv.is_table() && v.is_table() => merge(bv, v),
                    _ => {
                        b.insert(k, v);
                    }
                }
            }
        }
        (b, t) => *b = t,
    }
}

/// Read a file and its includes into one table (`part` still unresolved).
pub fn read_merged(path: &Path, depth: usize) -> Result<Value, String> {
    if depth > 16 {
        return Err(format!("{}: include nesting deeper than 16", path.display()));
    }
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut v: Value = toml::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
    let includes = match v.as_table_mut().and_then(|t| t.remove("include")) {
        None => vec![],
        Some(Value::Array(a)) => a.into_iter().map(|x| x.as_str().map(String::from).ok_or_else(|| format!("{}: include entries must be strings", path.display()))).collect::<Result<_, _>>()?,
        Some(_) => return Err(format!("{}: include must be an array of paths", path.display())),
    };
    let mut out = Value::Table(Default::default());
    let dir = path.parent().unwrap_or(Path::new("."));
    for inc in includes {
        merge(&mut out, read_merged(&dir.join(inc), depth + 1)?);
    }
    merge(&mut out, v);
    Ok(out)
}

/// `members_range = [a, b]` in a world table becomes `members = [a, ..., b]`.
fn expand_ranges(v: &mut Value) -> Result<(), String> {
    let Some(worlds) = v.get_mut("world").and_then(|w| w.as_table_mut()) else { return Ok(()) };
    for (k, w) in worlds.iter_mut() {
        let Some(t) = w.as_table_mut() else { continue };
        if let Some(r) = t.remove("members_range") {
            let r: [u32; 2] = r.try_into().map_err(|e| format!("world {k}: members_range: {e}"))?;
            if t.contains_key("members") {
                return Err(format!("world {k}: give members or members_range, not both"));
            }
            t.insert("members".into(), Value::Array((r[0]..=r[1]).map(|m| Value::Integer(m as i64)).collect()));
        }
    }
    Ok(())
}

pub fn load(path: &Path) -> Result<Loaded, String> {
    let mut v = read_merged(path, 0)?;
    let parts = match v.as_table_mut().and_then(|t| t.remove("part")) {
        None => vec![v],
        Some(Value::Array(a)) => a
            .into_iter()
            .map(|p| {
                let mut b = v.clone();
                merge(&mut b, p);
                b
            })
            .collect(),
        Some(_) => return Err(format!("{}: part must be an array of tables", path.display())),
    };
    let parts = parts
        .into_iter()
        .enumerate()
        .map(|(i, mut p)| {
            expand_ranges(&mut p).map_err(|e| format!("{} (part {}): {e}", path.display(), i + 1))?;
            p.try_into::<Scenario>().map_err(|e| format!("{} (part {}): {e}", path.display(), i + 1))
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Loaded { parts })
}

/// Parse a TOML key as an id.
pub fn id(k: &str) -> Result<u32, String> {
    k.parse().map_err(|_| format!("`{k}` is not a member or world id"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str, files: &[(&str, &str)]) -> PathBufGuard {
        let dir = std::env::temp_dir().join(format!("pe-session-sim-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for (f, text) in files {
            let p = dir.join(f);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, text).unwrap();
        }
        PathBufGuard(dir)
    }

    struct PathBufGuard(std::path::PathBuf);
    impl Drop for PathBufGuard {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    const BASE: &str = r#"
end_ms = 1000
[world.1]
members = [1, 2]
scene = { pile = { side = 2, layers = 1 } }
epoch_ms = 100
[world.1.plan]
1 = [{ at = 10, touch = { obj = 1, impulse = [0.0, 1.0, 0.0] } }]
2 = [{ at = 20, touch = { obj = 2, impulse = [0.0, 1.0, 0.0] } }]
[net]
seed = 1
link = { delay_ms = 30 }
"#;

    #[test]
    fn include_merges_tables_and_replaces_arrays() {
        let d = tmp(
            "include",
            &[
                ("common/base.toml", BASE),
                ("a.toml", "include = [\"common/base.toml\"]\n[world.1]\nmembers = [1, 2, 3]\n[world.1.plan]\n2 = []\n[net]\nseed = 9\n"),
            ],
        );
        let l = load(&d.0.join("a.toml")).unwrap();
        assert_eq!(l.parts.len(), 1);
        let s = &l.parts[0];
        let w = &s.world["1"];
        assert_eq!(w.members, vec![1, 2, 3]);
        assert_eq!(w.epoch_ms, 100, "kept from the include");
        assert_eq!(w.plan["1"].len(), 1, "member 1's plan kept");
        assert!(w.plan["2"].is_empty(), "member 2's plan replaced");
        assert_eq!((s.net.seed, s.net.link.delay_ms), (9, 30));
    }

    #[test]
    fn parts_are_merged_on_the_rest() {
        let d = tmp(
            "parts",
            &[("p.toml", &format!("{BASE}\n[[part]]\nend_ms = 2000\n[[part]]\nbyzantine = [{{ kind = \"lag_horizon\", member = 2, frames = 5 }}]\n"))],
        );
        let l = load(&d.0.join("p.toml")).unwrap();
        assert_eq!(l.parts.len(), 2);
        assert_eq!((l.parts[0].end_ms, l.parts[1].end_ms), (Some(2000), Some(1000)));
        assert!(l.parts[0].byzantine.is_empty());
        assert_eq!(l.parts[1].byzantine.len(), 1);
    }

    #[test]
    fn members_range_expands() {
        let d = tmp("range", &[("r.toml", &BASE.replace("members = [1, 2]", "members_range = [1, 5]"))]);
        assert_eq!(load(&d.0.join("r.toml")).unwrap().parts[0].world["1"].members, vec![1, 2, 3, 4, 5]);
        let d = tmp("range2", &[("r.toml", &BASE.replace("members = [1, 2]", "members = [1, 2]\nmembers_range = [1, 5]"))]);
        assert!(load(&d.0.join("r.toml")).err().unwrap().contains("not both"));
    }

    #[test]
    fn unknown_fields_are_errors() {
        let d = tmp("unknown", &[("u.toml", &format!("{BASE}\n[[fault]]\nfrom = 1\ndrop = true\ntypo = 3\n"))]);
        let e = load(&d.0.join("u.toml")).err().unwrap();
        assert!(e.contains("typo"), "{e}");
    }
}
