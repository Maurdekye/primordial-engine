//! The genesis record of a two-member M1 session: the interim until pe-agree's join from a cut (@coordinator-opus
//! mb725d083d0ce). The first machine (A) builds the world when the second's (B's) reply to the invite arrives, and
//! sends this record in the welcome (pe-net's `agree` bytes, @shader-lang's net.rs). B checks that it names exactly
//! itself and the invite's key, rebuilds the world from it, and starts only if its root is the record's.
//!
//! - **What it carries:** the format and the engine build id (the bytes of pe-net's `Hello::build`); the two member
//!   keys, sorted, with their roles; pe-agree's consensus settings as `NodeConfig::consensus_hash` (@world-chunks);
//!   the world's rate, cut interval and gravity; the preset, its version and its arguments, seed and origin included;
//!   the epoch (frame 0's session time, as pe-net's `StartFirstMember` takes it); and the frame-0 world root.
//! - **The line** is pe-world's `preset::genesis_line` of the record's bytes without the root ([`Genesis::line`];
//!   world-model.md 2.4). A record A didn't send gives B another line, so no signature of A's verifies on B (pe-agree
//!   signs the line into entries, votes and proposals). The root can't be hashed into it: the world's config and its
//!   objects' ids carry the line.
//! - **A fresh nonce from the OS** is in those bytes ([`fresh_nonce`]), so two sessions with the same seed, members and
//!   settings never share a line, ids or signatures, and one session's signed entries can't replay into another
//!   (@world-model's reviewers' Low). B checks nothing about it beyond the line and the root it feeds.
//! - **What the root does not cover** (the cut interval, the epoch) the line still does: net.rs compares the welcome's
//!   line and epoch with the record's.
//! - **Admission** goes through `World::admit`, the function a join entry calls (pe-world's `preset::m1_session`).
//! - **Roles decide permissions** ([`Role::perms`]; @coordinator-opus m9571fcf519b9 and m615c8e76ba6a, world-model.md
//!   5.4 and the user's home-world ruling m76967fa60b43): the opener is admitted `ADMIN | EDIT | AGENT`, the joiner
//!   `EDIT | AGENT`, a Guest with agent powers, since M1's slice edits through an agent on either machine. The bits
//!   are in the root, not in the record: every build that passes the build check maps roles the same way.
//! - **Two roots.** The world's as built (pe-world's frame-0 root, pinned on Windows and Linux by pe-world's golden
//!   test), and the replica's at frame 0, after it writes its physics into the world: what the first cut's votes
//!   build on. B compares both, so if constructing the replica differed between the machines, B refuses at the
//!   welcome, naming it, instead of stalling at the first cut (@design-reviewer's Low 3).
//! - **The frame clock** must run at the record's rate ([`GenesisError::ClockRate`]). The clock speaks host time, so
//!   the epoch can't be compared through it: net.rs makes the clock from the record's epoch and refuses an epoch in
//!   the past or too far ahead (@shader-lang, @coordinator-opus m9571fcf519b9).
//! - **pe-agree's settings are checked on both machines** (@coordinator-opus m1be30a8c61b1, the first two-machine
//!   run's gate): `agree` is this build's [`consensus_hash`], the `NodeConfig::consensus_hash` of the configuration
//!   [`Genesis::start`] runs. B's [`Genesis::check`] refuses a record for other settings before building anything,
//!   and each machine's `start` compares the configuration it runs with the record's, so a member whose settings
//!   differ is refused by name ([`GenesisError::Agree`]) instead of refusing the other's entries, votes or cuts later.

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use pe_agree::NodeConfig;
use pe_replica::{PeWorld, ReplicaConfig, ReplicaImpl, ScriptRunner};
use pe_seams::agree::{Cut, CutRef};
use pe_seams::{DomainId, Frame, FrameClock, Hash, LineId, MemberKey, Pos, Replica, SimRate};
use pe_world::preset::{self, Session};
use pe_world::{BlobStore, World, perm};

/// The record's format: a change to its fields or their encoding bumps it.
pub const FORMAT: u16 = 1;
/// The one preset a record names in M1, and its version: a change to what `preset::m1_session` builds bumps it.
pub const PRESET: &str = "m1-ball-pit";
pub const PRESET_VERSION: u32 = 1;
/// Bounds a record keeps: `decode` refuses bytes past them, and `ball_pit` refuses values past them, so a record A
/// sends is one B can read.
const MAX_BUILD: usize = 256;
const MAX_MEMBERS: usize = 16;
const MAX_NAME: usize = 64;

/// A member's part in starting the session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    /// A: built the world and printed the invite.
    Opener,
    /// B: replied to the invite.
    Joiner,
}

impl Role {
    /// The permission bits a member with this role is admitted with (module doc): the opener is the world's creator
    /// and its admin, the joiner a Guest who may edit and act through an agent.
    pub fn perms(self) -> u32 {
        match self {
            Role::Opener => perm::ADMIN | perm::EDIT | perm::AGENT,
            Role::Joiner => perm::EDIT | perm::AGENT,
        }
    }
}

/// The preset the world is built from, and its arguments.
#[derive(Clone, Debug, PartialEq)]
pub struct Preset {
    pub name: String,
    pub version: u32,
    pub seed: u64,
    pub balls: u32,
    /// The pit's floor top and the domain's physics origin.
    pub origin: Pos,
}

/// The genesis record (module doc).
#[derive(Clone, Debug, PartialEq)]
pub struct Genesis {
    pub format: u16,
    /// Fresh from the OS for each session ([`fresh_nonce`]).
    pub nonce: [u8; 32],
    /// The engine build id: the same bytes as pe-net's `Hello::build`.
    pub build: Vec<u8>,
    /// Exactly two, sorted by key: one opener and one joiner.
    pub members: Vec<(MemberKey, Role)>,
    /// pe-agree's `NodeConfig::consensus_hash` of the configuration the members run: [`consensus_hash`].
    pub agree: Hash,
    pub rate: SimRate,
    /// pe-agree's cut interval, this build's consensus setting ([`settings`]), in whole milliseconds. The line covers
    /// it; the replica's cut grid comes from the settings themselves ([`replica_config`]), and `start` checks the two
    /// agree.
    pub cut_interval_ms: u32,
    pub gravity: [f32; 3],
    pub preset: Preset,
    /// Frame 0's session time, in nanoseconds (pe-net's `StartFirstMember`).
    pub epoch_ns: i64,
    /// The frame-0 world's root as built: pe-world's `root_hash` of `preset::m1_session`'s world, before a replica is
    /// made over it.
    pub root: Hash,
    /// The replica's root at frame 0: the same world after the replica writes its physics into it.
    pub replica_root: Hash,
}

/// What A chooses; the record follows from it ([`Genesis::ball_pit`]).
#[derive(Clone, Debug, PartialEq)]
pub struct Params {
    /// [`fresh_nonce`], once per session.
    pub nonce: [u8; 32],
    pub build: Vec<u8>,
    pub opener: MemberKey,
    pub joiner: MemberKey,
    pub rate: SimRate,
    pub gravity: [f32; 3],
    pub seed: u64,
    pub balls: u32,
    pub origin: Pos,
    pub epoch_ns: i64,
}

/// Why a record is refused, naming the field.
#[derive(Clone, Debug, PartialEq)]
pub enum GenesisError {
    /// The bytes are not a record: what was wrong with them.
    Decode(&'static str),
    Format {
        record: u16,
        mine: u16,
    },
    Build {
        record: Vec<u8>,
        mine: Vec<u8>,
    },
    /// The members are not exactly this machine (the joiner) and the invite's key (the opener): what was wrong.
    Members(&'static str),
    /// The world's governance members are not the record's, the members pe-agree's configuration names: 6.2a's
    /// voter snapshot takes the voters from the world, so in M1, where every member votes, the two must be one set.
    Governance {
        world: Vec<MemberKey>,
        agree: Vec<MemberKey>,
    },
    Preset {
        name: String,
        version: u32,
    },
    Agree {
        record: Hash,
        mine: Hash,
    },
    /// The rebuilt world's root is not the record's: some setting or argument the root covers differs.
    Root {
        built: Hash,
        record: Hash,
    },
    /// The rebuilt world's root is the record's, but the replica's frame-0 root is not: constructing the replica (its
    /// physics) differs between the two machines.
    ReplicaRoot {
        built: Hash,
        record: Hash,
    },
    /// A value past the record's bound: `field`'s length, and the bound.
    TooLong {
        field: &'static str,
        len: usize,
        max: usize,
    },
    /// The frame clock runs at another rate than the record's.
    ClockRate {
        clock: u32,
        record: u32,
    },
    /// The world or the replica could not be built.
    World(String),
}

impl fmt::Display for GenesisError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GenesisError::Decode(what) => write!(f, "not a genesis record: {what}"),
            GenesisError::Format { record, mine } => write!(f, "record format {record}, this build reads {mine}"),
            GenesisError::Build { .. } => write!(f, "another engine build than this one"),
            GenesisError::Members(what) => write!(f, "members: {what}"),
            GenesisError::Governance { .. } => write!(f, "the world's governance members are not agreement's"),
            GenesisError::Preset { name, version } => write!(f, "unknown preset {name:?} version {version}"),
            GenesisError::Agree { .. } => write!(f, "other pe-agree settings than this machine's"),
            GenesisError::Root { built, record } => {
                write!(f, "the rebuilt world's root {built} is not the record's {record}")
            }
            GenesisError::ReplicaRoot { built, record } => write!(
                f,
                "the world matches the record, but the replica's frame-0 root {built} is not the record's {record}: \
                 its physics differs between the machines"
            ),
            GenesisError::TooLong { field, len, max } => {
                write!(f, "the {field} is {len} long, past the record's bound of {max}")
            }
            GenesisError::ClockRate { clock, record } => {
                write!(f, "the frame clock runs at {clock} Hz, the record at {record} Hz")
            }
            GenesisError::World(why) => write!(f, "the world could not be built: {why}"),
        }
    }
}

impl Genesis {
    /// A's side: builds the frame-0 world from `params`, takes its root and the replica's, and returns the record, A's
    /// replica (with `params.opener` as the local member) and pe-agree's node configuration. Refuses a value past the
    /// record's bounds ([`GenesisError::TooLong`]) and a clock at another rate. net.rs may change the configuration's
    /// local settings (the cut lag, fetch retry, retention, relay), never one every member must share
    /// (`frame_tolerance_s`, the rate budget, `Timing`): `agree` describes those.
    pub fn ball_pit(
        params: &Params,
        clock: Arc<dyn FrameClock>,
        runner: Box<dyn ScriptRunner>,
    ) -> Result<(Genesis, Box<dyn Replica>, NodeConfig), GenesisError> {
        if params.opener == params.joiner {
            return Err(GenesisError::Members("the two keys are the same"));
        }
        let cut_interval_ms = interval_ms(&settings())?;
        let mut members = vec![(params.opener, Role::Opener), (params.joiner, Role::Joiner)];
        members.sort_by_key(|m| m.0);
        let mut genesis = Genesis {
            format: FORMAT,
            nonce: params.nonce,
            build: params.build.clone(),
            members,
            agree: consensus_hash(),
            rate: params.rate,
            cut_interval_ms,
            gravity: params.gravity,
            preset: Preset {
                name: PRESET.into(),
                version: PRESET_VERSION,
                seed: params.seed,
                balls: params.balls,
                origin: params.origin,
            },
            epoch_ns: params.epoch_ns,
            root: Hash([0; 32]),
            replica_root: Hash([0; 32]),
        };
        genesis.bounds()?;
        let world = genesis.world()?;
        genesis.root = world.root_hash(DomainId(0));
        let (replica, node, replica_root) = genesis.start(world, params.opener, clock, runner)?;
        genesis.replica_root = replica_root;
        Ok((genesis, replica, node))
    }

    /// B's side, before it builds anything: a record of this format, from this engine build (`build`), with the
    /// pe-agree settings this machine runs ([`consensus_hash`]), naming exactly this machine (`me`) as the joiner and
    /// the invite's key (`opener`) as the opener.
    pub fn check(&self, me: MemberKey, opener: MemberKey, build: &[u8]) -> Result<(), GenesisError> {
        if self.format != FORMAT {
            return Err(GenesisError::Format { record: self.format, mine: FORMAT });
        }
        if self.build != build {
            return Err(GenesisError::Build { record: self.build.clone(), mine: build.to_vec() });
        }
        if self.preset.name != PRESET || self.preset.version != PRESET_VERSION {
            return Err(GenesisError::Preset { name: self.preset.name.clone(), version: self.preset.version });
        }
        match self.members.as_slice() {
            [a, b] if a.0 < b.0 => {
                let role_of = |k: MemberKey| [a, b].into_iter().find(|m| m.0 == k).map(|m| m.1);
                if role_of(me) != Some(Role::Joiner) {
                    return Err(GenesisError::Members("this machine is not the joiner"));
                }
                if role_of(opener) != Some(Role::Opener) {
                    return Err(GenesisError::Members("the opener is not the invite's key"));
                }
            }
            [_, _] => return Err(GenesisError::Members("not sorted by key, or one key twice")),
            _ => return Err(GenesisError::Members("not exactly two")),
        }
        let mine = consensus_hash();
        if self.agree != mine {
            return Err(GenesisError::Agree { record: self.agree, mine });
        }
        Ok(())
    }

    /// B's side: rebuilds the frame-0 world from the record, refuses it unless its root and then the replica's are the
    /// record's, and returns B's replica (with `me` as the local member) and pe-agree's node configuration, which net.rs
    /// changes only as [`Genesis::ball_pit`] says. Call [`Genesis::check`] first.
    pub fn build(
        &self,
        me: MemberKey,
        clock: Arc<dyn FrameClock>,
        runner: Box<dyn ScriptRunner>,
    ) -> Result<(Box<dyn Replica>, NodeConfig), GenesisError> {
        if !self.members.iter().any(|m| m.0 == me) {
            return Err(GenesisError::Members("this machine is not one of them"));
        }
        let world = self.world()?;
        let built = world.root_hash(DomainId(0));
        if built != self.root {
            return Err(GenesisError::Root { built, record: self.root });
        }
        let (replica, node, built) = self.start(world, me, clock, runner)?;
        if built != self.replica_root {
            return Err(GenesisError::ReplicaRoot { built, record: self.replica_root });
        }
        Ok((replica, node))
    }

    /// The session's line: pe-world's `genesis_line` of the record's bytes without the roots (module doc).
    ///
    /// # Panics
    ///
    /// If a field is past the record's bounds, which [`Genesis::ball_pit`] and [`Genesis::decode`] refuse.
    pub fn line(&self) -> LineId {
        preset::genesis_line(&self.encode_params())
    }

    /// pe-agree's genesis cut: the line's first, at frame 0, with no heads.
    pub fn genesis_cut(&self) -> Cut {
        Cut { at: CutRef { line: self.line(), height: 0, frame: Frame(0) }, heads: Vec::new() }
    }

    /// The record's bytes: its parameters, then the two roots.
    ///
    /// # Panics
    ///
    /// As [`Genesis::line`].
    pub fn encode(&self) -> Vec<u8> {
        let mut b = self.encode_params();
        b.extend_from_slice(&self.root.0);
        b.extend_from_slice(&self.replica_root.0);
        b
    }

    /// The record in `bytes`, all of them. It checks the encoding and the values' ranges, not who the members are:
    /// that is [`Genesis::check`].
    pub fn decode(bytes: &[u8]) -> Result<Genesis, GenesisError> {
        let mut r = Reader(bytes);
        let format = r.u16()?;
        if format != FORMAT {
            return Err(GenesisError::Format { record: format, mine: FORMAT });
        }
        let nonce = r.array()?;
        let build_len = r.u16()? as usize;
        let build = r.bytes(build_len, MAX_BUILD, "build id too long")?.to_vec();
        let count = r.u8()? as usize;
        if count > MAX_MEMBERS {
            return Err(GenesisError::Decode("too many members"));
        }
        let mut members = Vec::with_capacity(count);
        for _ in 0..count {
            let key = MemberKey(r.array()?);
            let role = match r.u8()? {
                0 => Role::Opener,
                1 => Role::Joiner,
                _ => return Err(GenesisError::Decode("unknown role")),
            };
            members.push((key, role));
        }
        let agree = Hash(r.array()?);
        let rate = SimRate::new(r.u32()?).ok_or(GenesisError::Decode("rate out of range"))?;
        let cut_interval_ms = r.u32()?;
        if cut_interval_ms == 0 {
            return Err(GenesisError::Decode("cut interval of zero"));
        }
        let gravity = [f32::from_bits(r.u32()?), f32::from_bits(r.u32()?), f32::from_bits(r.u32()?)];
        if !gravity.iter().all(|g| g.is_finite()) {
            return Err(GenesisError::Decode("gravity not finite"));
        }
        let name_len = r.u8()? as usize;
        let name = std::str::from_utf8(r.bytes(name_len, MAX_NAME, "preset name too long")?)
            .map_err(|_| GenesisError::Decode("preset name not UTF-8"))?
            .to_owned();
        let version = r.u32()?;
        let seed = r.u64()?;
        let balls = r.u32()?;
        let (origin, used) = Pos::decode(r.0).map_err(|_| GenesisError::Decode("origin"))?;
        r.0 = &r.0[used..];
        let epoch_ns = r.u64()? as i64;
        let root = Hash(r.array()?);
        let replica_root = Hash(r.array()?);
        if !r.0.is_empty() {
            return Err(GenesisError::Decode("trailing bytes"));
        }
        let preset = Preset { name, version, seed, balls, origin };
        Ok(Genesis {
            format,
            nonce,
            build,
            members,
            agree,
            rate,
            cut_interval_ms,
            gravity,
            preset,
            epoch_ns,
            root,
            replica_root,
        })
    }

    /// Refuses a value past the record's bounds: the ones `decode` refuses, so nothing is ever cut to fit.
    fn bounds(&self) -> Result<(), GenesisError> {
        for (field, len, max) in [
            ("build id", self.build.len(), MAX_BUILD),
            ("member list", self.members.len(), MAX_MEMBERS),
            ("preset name", self.preset.name.len(), MAX_NAME),
        ] {
            if len > max {
                return Err(GenesisError::TooLong { field, len, max });
            }
        }
        Ok(())
    }

    fn encode_params(&self) -> Vec<u8> {
        if let Err(e) = self.bounds() {
            panic!("a genesis record past its bounds ({e}): ball_pit and decode refuse them");
        }
        let mut b = Vec::new();
        b.extend_from_slice(&self.format.to_le_bytes());
        b.extend_from_slice(&self.nonce);
        b.extend_from_slice(&(self.build.len() as u16).to_le_bytes());
        b.extend_from_slice(&self.build);
        b.push(self.members.len() as u8);
        for (key, role) in &self.members {
            b.extend_from_slice(&key.0);
            b.push(match role {
                Role::Opener => 0,
                Role::Joiner => 1,
            });
        }
        b.extend_from_slice(&self.agree.0);
        b.extend_from_slice(&self.rate.hz().to_le_bytes());
        b.extend_from_slice(&self.cut_interval_ms.to_le_bytes());
        for g in self.gravity {
            b.extend_from_slice(&g.to_bits().to_le_bytes());
        }
        b.push(self.preset.name.len() as u8);
        b.extend_from_slice(self.preset.name.as_bytes());
        b.extend_from_slice(&self.preset.version.to_le_bytes());
        b.extend_from_slice(&self.preset.seed.to_le_bytes());
        b.extend_from_slice(&self.preset.balls.to_le_bytes());
        self.preset.origin.encode(&mut b);
        b.extend_from_slice(&self.epoch_ns.to_le_bytes());
        b
    }

    /// The frame-0 world the record describes, on its line.
    fn world(&self) -> Result<World, GenesisError> {
        if self.preset.name != PRESET || self.preset.version != PRESET_VERSION {
            return Err(GenesisError::Preset { name: self.preset.name.clone(), version: self.preset.version });
        }
        let session = Session {
            seed: self.preset.seed,
            origin: self.preset.origin,
            balls: self.preset.balls as usize,
            line: self.line(),
            rate: self.rate,
            gravity: self.gravity,
            agree: self.agree.0.to_vec(),
            members: self.members.iter().map(|&(key, role)| (key, role.perms())).collect(),
        };
        preset::m1_session(&session, BlobStore::default()).map_err(|e| GenesisError::World(format!("{e:?}")))
    }

    /// The replica over `world` with `me` local and `runner` running its scripts, its root at frame 0, and pe-agree's
    /// configuration ([`node_config`]). Refuses a clock at another rate than the record's, and a configuration whose
    /// consensus hash isn't the record's `agree`: on A that can't happen, since A's record takes `agree` from the same
    /// function; on B it holds even if [`Genesis::check`] was skipped.
    fn start(
        &self,
        world: World,
        me: MemberKey,
        clock: Arc<dyn FrameClock>,
        runner: Box<dyn ScriptRunner>,
    ) -> Result<(Box<dyn Replica>, NodeConfig, Hash), GenesisError> {
        if clock.rate_hz() != self.rate.hz() {
            return Err(GenesisError::ClockRate { clock: clock.rate_hz(), record: self.rate.hz() });
        }
        let keys: Vec<MemberKey> = self.members.iter().map(|m| m.0).collect();
        // The world's governance members are agreement's, the voters: 6.2a's voter snapshot takes them from the world,
        // and in M1 every member votes, so it is checked, never assumed (@design-reviewer m1a02a35a8a8e,
        // @coordinator-opus m800c0992d30d). Both lists are in key order.
        let snapshot = world.snapshot(Frame(0));
        let governance: Vec<MemberKey> = snapshot.members().map(|(k, _)| k).collect();
        if governance != keys {
            return Err(GenesisError::Governance { world: governance, agree: keys });
        }
        let node = node_config(self.line(), keys, self.genesis_cut());
        let mine = node.consensus_hash();
        if mine != self.agree {
            return Err(GenesisError::Agree { record: self.agree, mine });
        }
        if interval_ms(&node)? != self.cut_interval_ms {
            return Err(GenesisError::World(format!(
                "the record's cut interval, {} ms, isn't its consensus settings' {} s",
                self.cut_interval_ms, node.cut_interval_s
            )));
        }
        let mut config = replica_config(&node, self.rate, clock);
        config.local = Some(me);
        let replica = ReplicaImpl::new(PeWorld::new(world, DomainId(0)), config)
            .map_err(|e| GenesisError::World(format!("{e:?}")))?
            .with_runner(runner);
        let root = replica.live_root(Frame(0)).ok_or_else(|| GenesisError::World("no root at frame 0".into()))?;
        Ok((Box::new(replica), node, root))
    }
}

/// The `agree` hash this build writes into a record and expects in one: pe-agree's `NodeConfig::consensus_hash` of
/// the configuration [`Genesis::start`] runs. That hash leaves out the line, the members and the genesis cut (the
/// record carries them, and the line derives from the record, `agree` included), so it is taken here from a
/// configuration with placeholders for them, before a line exists; `start` compares it with the real one.
pub fn consensus_hash() -> Hash {
    settings().consensus_hash()
}

/// This build's consensus settings (pe-agree's defaults), with placeholders for the line, the members and the genesis
/// cut, which [`consensus_hash`] leaves out: the source of a record's `agree` and cut interval, and of a solo world's
/// cut grid (world.rs).
pub(crate) fn settings() -> NodeConfig {
    let line = LineId(Hash([0; 32]));
    let genesis = Cut { at: CutRef { line, height: 0, frame: Frame(0) }, heads: Vec::new() };
    node_config(line, Vec::new(), genesis)
}

/// The replica's settings at `rate` on `node`'s cut grid. `cut_every` is `NodeConfig::cut_every`, the frames between
/// cuts that pe-agree's voters require (they refuse a cut off the grid), so the replica's live roots and keyframes
/// sit on the cuts' frames and serve every vote without re-stepping (@world-chunks's cut grid). `ReplicaConfig::new`
/// rounds the interval up instead, which differs wherever it isn't a whole number of frames: 0.1 s is 7.2 frames at
/// 72 Hz, a grid of 7 for pe-agree and 8 for the rounding up.
pub(crate) fn replica_config(node: &NodeConfig, rate: SimRate, clock: Arc<dyn FrameClock>) -> ReplicaConfig {
    let mut config = ReplicaConfig::new(rate, Duration::from_secs_f64(node.cut_interval_s), clock);
    config.cut_every = node.cut_every(rate.hz());
    config
}

/// A configuration's cut interval in whole milliseconds, as a record carries it: at least 1 and within `u32`.
fn interval_ms(node: &NodeConfig) -> Result<u32, GenesisError> {
    let ms = (node.cut_interval_s * 1000.0).round();
    if (1.0..=u32::MAX as f64).contains(&ms) {
        Ok(ms as u32)
    } else {
        Err(GenesisError::World("the cut interval is out of range".into()))
    }
}

/// pe-agree's configuration of a session: every setting the members must share is this build's, pe-agree's
/// defaults. net.rs may change only the local ones ([`Genesis::ball_pit`]), which `consensus_hash` leaves out, and
/// today changes none. One function for [`consensus_hash`] and [`Genesis::start`], so the hash a record carries and
/// the configuration a machine runs can't drift apart.
fn node_config(line: LineId, members: Vec<MemberKey>, genesis: Cut) -> NodeConfig {
    NodeConfig::new(line, members, genesis)
}

/// A fresh session nonce from the operating system's randomness (getrandom, the source pe-net-crypto uses).
pub fn fresh_nonce() -> Result<[u8; 32], GenesisError> {
    let mut nonce = [0; 32];
    getrandom::fill(&mut nonce).map_err(|_| GenesisError::World("no randomness from the OS".into()))?;
    Ok(nonce)
}

/// A cursor over a record's bytes.
struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn bytes(&mut self, n: usize, max: usize, too_long: &'static str) -> Result<&'a [u8], GenesisError> {
        if n > max {
            return Err(GenesisError::Decode(too_long));
        }
        if self.0.len() < n {
            return Err(GenesisError::Decode("cut short"));
        }
        let (head, rest) = self.0.split_at(n);
        self.0 = rest;
        Ok(head)
    }
    fn array<const N: usize>(&mut self) -> Result<[u8; N], GenesisError> {
        let b = self.bytes(N, N, "cut short")?;
        Ok(b.try_into().expect("N bytes"))
    }
    fn u8(&mut self) -> Result<u8, GenesisError> {
        Ok(self.array::<1>()?[0])
    }
    fn u16(&mut self) -> Result<u16, GenesisError> {
        Ok(u16::from_le_bytes(self.array()?))
    }
    fn u32(&mut self) -> Result<u32, GenesisError> {
        Ok(u32::from_le_bytes(self.array()?))
    }
    fn u64(&mut self) -> Result<u64, GenesisError> {
        Ok(u64::from_le_bytes(self.array()?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::LocalClock;
    use pe_replica::logic::LogicRunner;
    use pe_seams::HostTime;

    const A: MemberKey = MemberKey([0xa1; 32]);
    const B: MemberKey = MemberKey([0x0b; 32]);
    const BUILD: &[u8] = b"pe test build";

    fn clock() -> Arc<dyn FrameClock> {
        Arc::new(LocalClock::new(HostTime(0), SimRate::DEFAULT))
    }

    fn runner() -> Box<dyn ScriptRunner> {
        let run = pe_logic::run::RunSettings::default();
        Box::new(LogicRunner::new(pe_logic::Settings::default(), run).expect("a runner"))
    }

    fn params() -> Params {
        Params {
            nonce: [5; 32],
            build: BUILD.to_vec(),
            opener: A,
            joiner: B,
            rate: SimRate::DEFAULT,
            gravity: pe_world::GRAVITY,
            seed: 11,
            balls: 24,
            origin: Pos::ORIGIN,
            epoch_ns: 2_000_000_000,
        }
    }

    /// B rebuilds A's frame-0 world from the record alone: the same root, the same line and pe-agree configuration,
    /// and each machine's own member marked local. The record survives its bytes.
    #[test]
    fn the_second_machine_rebuilds_the_first_machine_s_world_from_the_record() {
        let (g, mut a, node_a) = Genesis::ball_pit(&params(), clock(), runner()).expect("A's genesis");
        assert_eq!(g.members, vec![(B, Role::Joiner), (A, Role::Opener)], "sorted by key, with roles");
        let bytes = g.encode();
        let got = Genesis::decode(&bytes).expect("a record");
        assert_eq!(got, g);
        got.check(B, A, BUILD).expect("B's check");
        let (mut b, node_b) = got.build(B, clock(), runner()).expect("B's genesis");
        let mut source = crate::standin::SoloAgreement::new(g.line());
        let ra = a.root_at_cut(&g.genesis_cut(), &mut source).expect("A's root at frame 0");
        let rb = b.root_at_cut(&g.genesis_cut(), &mut source).expect("B's root at frame 0");
        // The record's root is the world's as built; constructing a replica writes into it (its physics), the same on
        // both machines, and the record's replica root is what each votes at the genesis cut.
        assert_eq!((ra, rb), (g.replica_root, g.replica_root), "both replicas start from the record's state");
        assert_ne!(g.replica_root, g.root, "the replica's physics is in its root");
        let world = got.world().expect("the record's world");
        let snap = world.snapshot(world.frame());
        assert_eq!(
            (snap.perms(A), snap.perms(B)),
            (Some(perm::ADMIN | perm::EDIT | perm::AGENT), Some(perm::EDIT | perm::AGENT)),
            "the opener is the admin, the joiner a Guest with agent powers"
        );
        assert_eq!((node_b.line, node_b.members.clone()), (node_a.line, node_a.members.clone()));
        assert_eq!((node_a.line, node_a.members.clone()), (g.line(), vec![B, A]));
        assert_eq!(node_a.genesis.at, CutRef { line: g.line(), height: 0, frame: Frame(0) });
        let mine = |r: &dyn Replica, k| r.view().objects.iter().filter(|o| o.hand_of.is_some_and(|h| h.0 == k)).count();
        assert_eq!((mine(&*a, A), mine(&*b, B)), (2, 2), "both members' hands are in both worlds");
    }

    /// The line is the record without its roots: they don't move it, every parameter does.
    #[test]
    fn the_line_is_the_hash_of_the_record_without_its_root() {
        let (g, _, _) = Genesis::ball_pit(&params(), clock(), runner()).expect("A's genesis");
        let bytes = g.encode();
        let want = preset::genesis_line(&bytes[..bytes.len() - 64]);
        assert_eq!(g.line(), want);
        let mut other = g.clone();
        other.root = Hash([1; 32]);
        other.replica_root = Hash([2; 32]);
        assert_eq!(other.line(), g.line(), "the roots are not in the line");
        let mut other = g.clone();
        other.cut_interval_ms += 1;
        assert_ne!(other.line(), g.line(), "the cut interval, which the root doesn't cover, is in the line");
        let mut other = g.clone();
        other.epoch_ns += 1;
        assert_ne!(other.line(), g.line(), "so is the epoch");
        let mut other = g.clone();
        other.nonce[0] ^= 1;
        assert_ne!(other.line(), g.line(), "and the nonce, so two sessions never share a line");
    }

    /// At 72 Hz, pe-agree's 0.1 s grid is 7 frames (7.2, rounded), where `ReplicaConfig::new`'s rounding up gives 8.
    /// The replica takes its roots on the cuts' grid, solo and in a session, so every cut a voter accepts lands on a
    /// live root (@world-chunks's cut grid).
    #[test]
    fn the_replica_s_cut_grid_is_pe_agree_s_at_72_hz() {
        let node = settings();
        let rate = SimRate::new(72).expect("72 Hz is a supported rate");
        assert_eq!(node.cut_every(72), 7, "0.1 s at 72 Hz is 7.2 frames");
        assert_eq!(replica_config(&node, rate, clock()).cut_every, node.cut_every(72));
        let rounded_up = ReplicaConfig::new(rate, Duration::from_secs_f64(node.cut_interval_s), clock()).cut_every;
        assert_eq!(rounded_up, 8, "the rule replica_config replaces");
    }

    /// A record's cut interval is this build's consensus setting, and `start` refuses a record whose interval isn't
    /// the one its `agree` hash stands for: the line covers the interval, and the grid comes from the settings.
    #[test]
    fn the_record_s_cut_interval_is_the_settings_and_start_refuses_another() {
        let (g, _, _) = Genesis::ball_pit(&params(), clock(), runner()).expect("A's genesis");
        assert_eq!(g.cut_interval_ms, 100, "pe-agree's 0.1 s");
        let mut other = g.clone();
        other.cut_interval_ms += 1;
        let world = other.world().expect("the record's world");
        match other.start(world, A, clock(), runner()) {
            Err(GenesisError::World(m)) => assert!(m.contains("cut interval"), "{m}"),
            r => panic!("expected the cut interval refused, got {:?}", r.map(|_| ())),
        }
    }

    /// A record with one field altered is refused, naming the field, or, for what the root covers, both roots
    /// (@coordinator-opus mb725d083d0ce).
    #[test]
    fn a_record_with_one_field_altered_is_refused_naming_it() {
        let (g, _, _) = Genesis::ball_pit(&params(), clock(), runner()).expect("A's genesis");
        let refused = |change: &dyn Fn(&mut Genesis)| {
            let mut altered = g.clone();
            change(&mut altered);
            let altered = Genesis::decode(&altered.encode()).expect("still a record");
            altered.check(B, A, BUILD).and_then(|()| altered.build(B, clock(), runner()).map(|_| ()))
        };
        assert!(matches!(refused(&|r| r.build = b"another build".to_vec()), Err(GenesisError::Build { .. })));
        assert!(matches!(refused(&|r| r.preset.version += 1), Err(GenesisError::Preset { .. })));
        assert!(matches!(refused(&|r| r.agree = Hash([8; 32])), Err(GenesisError::Agree { .. })));
        // Keys that keep the list sorted: one below both members' keys, one above.
        let (low, high) = (MemberKey([0x01; 32]), MemberKey([0xcc; 32]));
        assert_eq!(refused(&|r| r.members[0].0 = low), Err(GenesisError::Members("this machine is not the joiner")));
        assert_eq!(
            refused(&|r| r.members = vec![(B, Role::Joiner), (high, Role::Opener)]),
            Err(GenesisError::Members("the opener is not the invite's key"))
        );
        assert_eq!(
            refused(&|r| r.members.reverse()),
            Err(GenesisError::Members("not sorted by key, or one key twice"))
        );
        assert_eq!(refused(&|r| r.members.truncate(1)), Err(GenesisError::Members("not exactly two")));
        for (what, change) in [
            ("seed", &(|r: &mut Genesis| r.preset.seed += 1) as &dyn Fn(&mut Genesis)),
            ("balls", &|r: &mut Genesis| r.preset.balls += 1),
            ("gravity", &|r: &mut Genesis| r.gravity[1] = -3.0),
            ("rate", &|r: &mut Genesis| r.rate = SimRate::new(90).expect("90 Hz")),
            ("root", &|r: &mut Genesis| r.root = Hash([3; 32])),
            ("nonce", &|r: &mut Genesis| r.nonce[0] ^= 1),
        ] {
            assert!(matches!(refused(change), Err(GenesisError::Root { .. })), "{what}: the roots differ");
        }
        // The world as built matches, the replica's start doesn't: named apart from the world's root.
        assert!(matches!(refused(&|r| r.replica_root = Hash([4; 32])), Err(GenesisError::ReplicaRoot { .. })));
        let mut format = g.encode();
        format[0] ^= 1;
        assert!(matches!(Genesis::decode(&format), Err(GenesisError::Format { .. })));
    }

    /// The record names the pe-agree settings both machines run: A's record carries the consensus hash of the
    /// configuration A starts, and B starts one with the same hash (gate item 1, @coordinator-opus m1be30a8c61b1).
    #[test]
    fn the_record_names_the_consensus_settings_both_machines_run() {
        let (g, _, node_a) = Genesis::ball_pit(&params(), clock(), runner()).expect("A's genesis");
        assert_eq!(g.agree, node_a.consensus_hash(), "A's record names the settings A runs");
        assert_eq!(g.agree, consensus_hash(), "this build's hash");
        let g = Genesis::decode(&g.encode()).expect("a record");
        g.check(B, A, BUILD).expect("B's check");
        let (_, node_b) = g.build(B, clock(), runner()).expect("B's world");
        assert_eq!(node_b.consensus_hash(), g.agree, "and the settings B runs");
    }

    /// Each machine's start compares the configuration it runs with the record's, `check` or not: a record for other
    /// settings, its world right for its own line, is refused there, naming both hashes.
    #[test]
    fn a_record_for_other_consensus_settings_is_refused_at_start() {
        let (g, _, _) = Genesis::ball_pit(&params(), clock(), runner()).expect("A's genesis");
        let mut other = g.clone();
        other.agree = Hash([8; 32]);
        let world = other.world().expect("its world");
        let refused = other.start(world, B, clock(), runner()).map(|_| ()).expect_err("other settings");
        assert_eq!(refused, GenesisError::Agree { record: Hash([8; 32]), mine: consensus_hash() });
    }

    /// The world's governance members are agreement's, checked at start (@design-reviewer m1a02a35a8a8e): a record
    /// whose members aren't those of the world it is started with is refused, naming both lists.
    #[test]
    fn a_world_whose_governance_members_are_not_agreement_s_is_refused() {
        let (g, _, _) = Genesis::ball_pit(&params(), clock(), runner()).expect("A's genesis");
        let world = g.world().expect("its world");
        let c = MemberKey([0xcc; 32]);
        let mut other = g.clone();
        other.members = vec![(B, Role::Joiner), (c, Role::Opener)];
        let refused = other.start(world, B, clock(), runner()).map(|_| ()).expect_err("other members");
        assert_eq!(refused, GenesisError::Governance { world: vec![B, A], agree: vec![B, c] });
    }

    /// A value past the record's bounds is refused, never cut to fit (@chunks-reviewer's L2).
    #[test]
    fn a_value_past_the_record_s_bounds_is_refused() {
        let mut p = params();
        p.build = vec![b'x'; MAX_BUILD + 1];
        assert_eq!(
            Genesis::ball_pit(&p, clock(), runner()).map(|_| ()),
            Err(GenesisError::TooLong { field: "build id", len: MAX_BUILD + 1, max: MAX_BUILD })
        );
        p.build = vec![b'x'; MAX_BUILD];
        let (g, _, _) = Genesis::ball_pit(&p, clock(), runner()).expect("a build id at the bound");
        assert_eq!(Genesis::decode(&g.encode()).expect("a record"), g);
    }

    /// A record made by hand past its bounds never encodes: no line over cut bytes.
    #[test]
    #[should_panic(expected = "past its bounds")]
    fn a_record_past_its_bounds_does_not_encode() {
        let (mut g, _, _) = Genesis::ball_pit(&params(), clock(), runner()).expect("A's genesis");
        g.preset.name = "x".repeat(MAX_NAME + 1);
        g.encode();
    }

    /// The frame clock must run at the record's rate, on either machine (@design-reviewer's Low 2).
    #[test]
    fn a_clock_at_another_rate_is_refused() {
        let fast: Arc<dyn FrameClock> = Arc::new(LocalClock::new(HostTime(0), SimRate::new(90).expect("90 Hz")));
        let refused = Err(GenesisError::ClockRate { clock: 90, record: SimRate::DEFAULT.hz() });
        assert_eq!(Genesis::ball_pit(&params(), fast.clone(), runner()).map(|_| ()), refused);
        let (g, _, _) = Genesis::ball_pit(&params(), clock(), runner()).expect("A's genesis");
        assert_eq!(g.build(B, fast, runner()).map(|_| ()), refused);
    }

    /// Each session's nonce is fresh: two draws differ.
    #[test]
    fn each_session_gets_a_fresh_nonce() {
        assert_ne!(fresh_nonce().expect("the OS's randomness"), fresh_nonce().expect("the OS's randomness"));
    }

    /// Bytes that are not a whole record are refused, never a panic: cut short at every length, or with a byte more.
    #[test]
    fn bytes_that_are_not_a_record_are_refused() {
        let (g, _, _) = Genesis::ball_pit(&params(), clock(), runner()).expect("A's genesis");
        let bytes = g.encode();
        for n in 0..bytes.len() {
            assert!(Genesis::decode(&bytes[..n]).is_err(), "cut at {n}");
        }
        let mut longer = bytes.clone();
        longer.push(0);
        assert_eq!(Genesis::decode(&longer), Err(GenesisError::Decode("trailing bytes")));
    }
}
