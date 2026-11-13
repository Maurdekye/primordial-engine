//! The genesis record of a two-member M1 session: the interim until pe-agree's join from a cut (@coordinator-opus). The first machine (A) builds the world when the second's (B's) reply to the invite arrives, and
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

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use pe_agree::NodeConfig;
use pe_replica::{PeWorld, ReplicaConfig, ReplicaImpl};
use pe_seams::agree::{Cut, CutRef};
use pe_seams::{DomainId, Frame, FrameClock, Hash, LineId, MemberKey, Pos, Replica, SimRate};
use pe_world::preset::{self, Session};
use pe_world::{BlobStore, World};

/// The record's format: a change to its fields or their encoding bumps it.
pub const FORMAT: u16 = 1;
/// The one preset a record names in M1, and its version: a change to what `preset::m1_session` builds bumps it.
pub const PRESET: &str = "m1-ball-pit";
pub const PRESET_VERSION: u32 = 1;
/// Bounds a decoded record must keep.
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
    /// pe-agree's `NodeConfig::consensus_hash` of the configuration the members run.
    pub agree: Hash,
    pub rate: SimRate,
    pub cut_interval_ms: u32,
    pub gravity: [f32; 3],
    pub preset: Preset,
    /// Frame 0's session time, in nanoseconds (pe-net's `StartFirstMember`).
    pub epoch_ns: i64,
    /// The frame-0 world's root as built: pe-world's `root_hash` of `preset::m1_session`'s world, before a replica is
    /// made over it (which writes its physics into the world, the same on every machine).
    pub root: Hash,
}

/// What A chooses; the record follows from it ([`Genesis::ball_pit`]).
#[derive(Clone, Debug, PartialEq)]
pub struct Params {
    /// [`fresh_nonce`], once per session.
    pub nonce: [u8; 32],
    pub build: Vec<u8>,
    pub opener: MemberKey,
    pub joiner: MemberKey,
    pub agree: Hash,
    pub rate: SimRate,
    pub cut_interval: Duration,
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
            GenesisError::Preset { name, version } => write!(f, "unknown preset {name:?} version {version}"),
            GenesisError::Agree { .. } => write!(f, "other pe-agree settings than this machine's"),
            GenesisError::Root { built, record } => {
                write!(f, "the rebuilt world's root {built} is not the record's {record}")
            }
            GenesisError::World(why) => write!(f, "the world could not be built: {why}"),
        }
    }
}

impl Genesis {
    /// A's side: builds the frame-0 world from `params`, takes its root, and returns the record, A's replica (with
    /// `params.opener` as the local member) and pe-agree's node configuration.
    pub fn ball_pit(
        params: &Params,
        clock: Arc<dyn FrameClock>,
    ) -> Result<(Genesis, Box<dyn Replica>, NodeConfig), GenesisError> {
        if params.opener == params.joiner {
            return Err(GenesisError::Members("the two keys are the same"));
        }
        let cut_interval_ms = u32::try_from(params.cut_interval.as_millis())
            .ok()
            .filter(|&ms| ms > 0)
            .ok_or_else(|| GenesisError::World("the cut interval is out of range".into()))?;
        let mut members = vec![(params.opener, Role::Opener), (params.joiner, Role::Joiner)];
        members.sort_by_key(|m| m.0);
        let mut genesis = Genesis {
            format: FORMAT,
            nonce: params.nonce,
            build: params.build.clone(),
            members,
            agree: params.agree,
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
        };
        let world = genesis.world()?;
        genesis.root = world.root_hash(DomainId(0));
        let (replica, node) = genesis.start(world, params.opener, clock)?;
        Ok((genesis, replica, node))
    }

    /// B's side, before it builds anything: a record of this format, from this engine build (`build`), with pe-agree's
    /// settings this machine runs (`agree`), naming exactly this machine (`me`) as the joiner and the invite's key
    /// (`opener`) as the opener.
    pub fn check(&self, me: MemberKey, opener: MemberKey, build: &[u8], agree: Hash) -> Result<(), GenesisError> {
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
        if self.agree != agree {
            return Err(GenesisError::Agree { record: self.agree, mine: agree });
        }
        Ok(())
    }

    /// B's side: rebuilds the frame-0 world from the record, refuses it unless its root is the record's, and returns B's
    /// replica (with `me` as the local member) and pe-agree's node configuration. Call [`Genesis::check`] first.
    pub fn build(
        &self,
        me: MemberKey,
        clock: Arc<dyn FrameClock>,
    ) -> Result<(Box<dyn Replica>, NodeConfig), GenesisError> {
        if !self.members.iter().any(|m| m.0 == me) {
            return Err(GenesisError::Members("this machine is not one of them"));
        }
        let world = self.world()?;
        let built = world.root_hash(DomainId(0));
        if built != self.root {
            return Err(GenesisError::Root { built, record: self.root });
        }
        self.start(world, me, clock)
    }

    /// The session's line: pe-world's `genesis_line` of the record's bytes without the root (module doc).
    pub fn line(&self) -> LineId {
        preset::genesis_line(&self.encode_params())
    }

    /// pe-agree's genesis cut: the line's first, at frame 0, with no heads.
    pub fn genesis_cut(&self) -> Cut {
        Cut { at: CutRef { line: self.line(), height: 0, frame: Frame(0) }, heads: Vec::new() }
    }

    /// The record's bytes: its parameters, then the root.
    pub fn encode(&self) -> Vec<u8> {
        let mut b = self.encode_params();
        b.extend_from_slice(&self.root.0);
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
        if !r.0.is_empty() {
            return Err(GenesisError::Decode("trailing bytes"));
        }
        let preset = Preset { name, version, seed, balls, origin };
        Ok(Genesis { format, nonce, build, members, agree, rate, cut_interval_ms, gravity, preset, epoch_ns, root })
    }

    fn encode_params(&self) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&self.format.to_le_bytes());
        b.extend_from_slice(&self.nonce);
        let build = &self.build[..self.build.len().min(MAX_BUILD)];
        b.extend_from_slice(&(build.len() as u16).to_le_bytes());
        b.extend_from_slice(build);
        let members = &self.members[..self.members.len().min(MAX_MEMBERS)];
        b.push(members.len() as u8);
        for (key, role) in members {
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
        let name = &self.preset.name.as_bytes()[..self.preset.name.len().min(MAX_NAME)];
        b.push(name.len() as u8);
        b.extend_from_slice(name);
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
            members: self.members.iter().map(|m| m.0).collect(),
        };
        preset::m1_session(&session, BlobStore::default()).map_err(|e| GenesisError::World(format!("{e:?}")))
    }

    /// The replica over `world` with `me` local, and pe-agree's configuration: pe-agree's defaults, which `agree` hashes.
    fn start(
        &self,
        world: World,
        me: MemberKey,
        clock: Arc<dyn FrameClock>,
    ) -> Result<(Box<dyn Replica>, NodeConfig), GenesisError> {
        let mut config = ReplicaConfig::new(self.rate, Duration::from_millis(self.cut_interval_ms.into()), clock);
        config.local = Some(me);
        let replica = ReplicaImpl::new(PeWorld::new(world, DomainId(0)), config)
            .map_err(|e| GenesisError::World(format!("{e:?}")))?;
        let keys = self.members.iter().map(|m| m.0).collect();
        Ok((Box::new(replica), NodeConfig::new(self.line(), keys, self.genesis_cut())))
    }
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
    use pe_seams::HostTime;

    const A: MemberKey = MemberKey([0xa1; 32]);
    const B: MemberKey = MemberKey([0x0b; 32]);
    const BUILD: &[u8] = b"pe test build";
    const AGREE: Hash = Hash([7; 32]);

    fn clock() -> Arc<dyn FrameClock> {
        Arc::new(LocalClock::new(HostTime(0), SimRate::DEFAULT))
    }

    fn params() -> Params {
        Params {
            nonce: [5; 32],
            build: BUILD.to_vec(),
            opener: A,
            joiner: B,
            agree: AGREE,
            rate: SimRate::DEFAULT,
            cut_interval: Duration::from_millis(133),
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
        let (g, mut a, node_a) = Genesis::ball_pit(&params(), clock()).expect("A's genesis");
        assert_eq!(g.members, vec![(B, Role::Joiner), (A, Role::Opener)], "sorted by key, with roles");
        let bytes = g.encode();
        let got = Genesis::decode(&bytes).expect("a record");
        assert_eq!(got, g);
        got.check(B, A, BUILD, AGREE).expect("B's check");
        let (mut b, node_b) = got.build(B, clock()).expect("B's genesis");
        let mut source = crate::standin::SoloAgreement::new(g.line());
        let ra = a.root_at_cut(&g.genesis_cut(), &mut source).expect("A's root at frame 0");
        let rb = b.root_at_cut(&g.genesis_cut(), &mut source).expect("B's root at frame 0");
        // The record's root is the world's as built; constructing a replica writes into it (its physics), the same on
        // both machines.
        assert_eq!(ra, rb, "both replicas start from the same state");
        assert_eq!((node_b.line, node_b.members.clone()), (node_a.line, node_a.members.clone()));
        assert_eq!((node_a.line, node_a.members.clone()), (g.line(), vec![B, A]));
        assert_eq!(node_a.genesis.at, CutRef { line: g.line(), height: 0, frame: Frame(0) });
        let mine = |r: &dyn Replica, k| r.view().objects.iter().filter(|o| o.hand_of.is_some_and(|h| h.0 == k)).count();
        assert_eq!((mine(&*a, A), mine(&*b, B)), (2, 2), "both members' hands are in both worlds");
    }

    /// The line is the record without its root: the root doesn't move it, every parameter does.
    #[test]
    fn the_line_is_the_hash_of_the_record_without_its_root() {
        let (g, _, _) = Genesis::ball_pit(&params(), clock()).expect("A's genesis");
        let bytes = g.encode();
        let want = preset::genesis_line(&bytes[..bytes.len() - 32]);
        assert_eq!(g.line(), want);
        let mut other = g.clone();
        other.root = Hash([1; 32]);
        assert_eq!(other.line(), g.line(), "the root is not in the line");
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

    /// A record with one field altered is refused, naming the field, or, for what the root covers, both roots
    /// (@coordinator-opus).
    #[test]
    fn a_record_with_one_field_altered_is_refused_naming_it() {
        let (g, _, _) = Genesis::ball_pit(&params(), clock()).expect("A's genesis");
        let refused = |change: &dyn Fn(&mut Genesis)| {
            let mut altered = g.clone();
            change(&mut altered);
            let altered = Genesis::decode(&altered.encode()).expect("still a record");
            altered.check(B, A, BUILD, AGREE).and_then(|()| altered.build(B, clock()).map(|_| ()))
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
        let mut format = g.encode();
        format[0] ^= 1;
        assert!(matches!(Genesis::decode(&format), Err(GenesisError::Format { .. })));
    }

    /// Each session's nonce is fresh: two draws differ.
    #[test]
    fn each_session_gets_a_fresh_nonce() {
        assert_ne!(fresh_nonce().expect("the OS's randomness"), fresh_nonce().expect("the OS's randomness"));
    }

    /// Bytes that are not a whole record are refused, never a panic: cut short at every length, or with a byte more.
    #[test]
    fn bytes_that_are_not_a_record_are_refused() {
        let (g, _, _) = Genesis::ball_pit(&params(), clock()).expect("A's genesis");
        let bytes = g.encode();
        for n in 0..bytes.len() {
            assert!(Genesis::decode(&bytes[..n]).is_err(), "cut at {n}");
        }
        let mut longer = bytes.clone();
        longer.push(0);
        assert_eq!(Genesis::decode(&longer), Err(GenesisError::Decode("trailing bytes")));
    }
}
