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
    pub members: Vec<u32>,
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
    /// R14 genesis route list: the worlds this one trades objects with.
    #[serde(default)]
    pub routes: Vec<u32>,
    /// Each member's planned actions in this world, by member id.
    #[serde(default)]
    pub plan: BTreeMap<String, Vec<ActionSpec>>,
    /// Per-member replica settings in this world.
    #[serde(default)]
    pub member: BTreeMap<String, ReplicaCfg>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub enum SceneCfg {
    Pile { side: usize, layers: usize },
    Ballpit { balls: usize, hands: usize },
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReplicaCfg {
    /// Background world (design 22.3): advance only every `throttle_ms`, from `throttle_from_ms`.
    pub throttle_ms: Option<u64>,
    #[serde(default)]
    pub throttle_from_ms: u64,
    /// R18 congestion control on this member's uplink.
    #[serde(default)]
    pub adapt: bool,
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
    /// R18: one datagram per member, peer and tick (up to the MTU).
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
    /// Sender and receiver in different groups (members in no group are unaffected).
    pub partition: Option<Vec<Vec<u32>>>,
    /// Message class: `entry` (any chain entry), `fetch`, `batch`, or an entry payload: `touch`,
    /// `hand`, `ack`, `vote`, `propose`, `prevote`, `precommit`, `kick`, `setrole`, `definerole`,
    /// `attest`, `depart`, `arrive`, `verdict`, `portal`.
    pub msg: Option<String>,
    /// Chain entries only: this stamp, this sequence number.
    pub stamp: Option<u64>,
    pub seq: Option<u32>,
    /// Apply to at most this many messages, then stop matching.
    pub times: Option<u64>,
    #[serde(default)]
    pub drop: bool,
    pub delay_ms: Option<u64>,
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
    Verdict { honest: Vec<u32>, label: String },
    /// Membership, evictions, confirmation and paused frames per honest member.
    Membership { honest: Vec<u32>, label: String },
    /// A condition on member `at`'s replica of `world`: confirmed frame within bounds and/or this
    /// exact membership. Prints OK or FAIL.
    Expect {
        #[serde(default = "one")]
        world: u32,
        at: u32,
        conf_max: Option<u64>,
        conf_min: Option<u64>,
        members: Option<Vec<u32>>,
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
        .map(|(i, p)| p.try_into::<Scenario>().map_err(|e| format!("{} (part {}): {e}", path.display(), i + 1)))
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
    fn unknown_fields_are_errors() {
        let d = tmp("unknown", &[("u.toml", &format!("{BASE}\n[[fault]]\nfrom = 1\ndrop = true\ntypo = 3\n"))]);
        let e = load(&d.0.join("u.toml")).err().unwrap();
        assert!(e.contains("typo"), "{e}");
    }
}
