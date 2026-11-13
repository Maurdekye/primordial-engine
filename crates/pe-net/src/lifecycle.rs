//! The connection lifecycle's messages (transport.md 6.3-6.6): the hello, the welcome, and the close reasons.
//!
//! These are pe-net's own bytes. Each travels alone on a unidirectional stream whose first byte names it, and each
//! is bounded: anything malformed or over a bound is a [`FrameError`], never a panic (2.5).

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use pe_seams::PeerId;

use crate::lanes::{FrameError, Reader, put_varint};

/// Format version of the hello and the welcome.
const VERSION: u8 = 1;
/// Bounds on what a peer may send (2.5's "no panics on a peer's input" also means no unbounded allocation).
pub const MAX_BUILD: usize = 64;
pub const MAX_TOKEN: usize = 64;
pub const MAX_ADDRS: usize = 16;
pub const MAX_MEMBERS: usize = 1024;
pub const MAX_AGREE: usize = 64 * 1024;
/// The largest hello and welcome on the wire.
pub const MAX_HELLO: usize = 4 * 1024;
pub const MAX_WELCOME: usize = 1 << 20;

/// What each side says first, after the handshake (6.3).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hello {
    pub protocol: u16,
    /// The engine build id.
    pub build: Vec<u8>,
    /// The physics self-test's digest, and when it ran (Unix seconds).
    pub selftest: [u8; 32],
    pub selftest_unix_s: u64,
    /// The line it means (`LineId`, `docs/spec/world-model.md` r9).
    pub line: [u8; 32],
    /// The invite id or token.
    pub token: Vec<u8>,
    pub addrs: Vec<SocketAddr>,
    /// Whether its clock is ready (5.4).
    pub clock_ready: bool,
}

/// The close reasons of 6.6, sent as QUIC application error codes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum CloseCode {
    Left = 0,
    Protocol = 1,
    Build = 2,
    SelfTest = 3,
    Line = 4,
    NotAdmitted = 5,
    Fault = 6,
    Clock = 7,
    Overloaded = 8,
}

impl CloseCode {
    pub fn from_u64(v: u64) -> Option<Self> {
        Some(match v {
            0 => Self::Left,
            1 => Self::Protocol,
            2 => Self::Build,
            3 => Self::SelfTest,
            4 => Self::Line,
            5 => Self::NotAdmitted,
            6 => Self::Fault,
            7 => Self::Clock,
            8 => Self::Overloaded,
            _ => return None,
        })
    }
}

/// A refusal: its code, and the two values that differ (or why), for the joiner to show.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refusal {
    pub code: CloseCode,
    pub detail: String,
}

impl Hello {
    /// The guards of 6.3, in order, between my hello and theirs: the first field that differs refuses; `None` if
    /// they all pass.
    pub fn guard(&self, theirs: &Hello) -> Option<Refusal> {
        let r = |code, what: &str, a: String, b: String| {
            Some(Refusal { code, detail: format!("{what}: ours {a}, theirs {b}") })
        };
        if self.protocol != theirs.protocol {
            return r(CloseCode::Protocol, "protocol", self.protocol.to_string(), theirs.protocol.to_string());
        }
        if self.build != theirs.build {
            return r(CloseCode::Build, "build", printable(&self.build), printable(&theirs.build));
        }
        if self.selftest != theirs.selftest {
            return r(CloseCode::SelfTest, "self-test", short(&self.selftest), short(&theirs.selftest));
        }
        if self.line != theirs.line {
            return r(CloseCode::Line, "line", short(&self.line), short(&theirs.line));
        }
        None
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut o = vec![VERSION];
        o.extend_from_slice(&self.protocol.to_le_bytes());
        put_bytes(&mut o, &self.build);
        o.extend_from_slice(&self.selftest);
        o.extend_from_slice(&self.selftest_unix_s.to_le_bytes());
        o.extend_from_slice(&self.line);
        put_bytes(&mut o, &self.token);
        put_addrs(&mut o, &self.addrs);
        o.push(u8::from(self.clock_ready));
        o
    }

    pub fn decode(b: &[u8]) -> Result<Self, FrameError> {
        if b.len() > MAX_HELLO {
            return Err(FrameError::TooLong);
        }
        let mut r = Reader(b);
        version(&mut r)?;
        let protocol = u16::from_le_bytes(array(&mut r)?);
        let build = get_bytes(&mut r, MAX_BUILD)?;
        let selftest = array(&mut r)?;
        let selftest_unix_s = u64::from_le_bytes(array(&mut r)?);
        let line = array(&mut r)?;
        let token = get_bytes(&mut r, MAX_TOKEN)?;
        let addrs = get_addrs(&mut r)?;
        let clock_ready = match r.u8()? {
            0 => false,
            1 => true,
            k => return Err(FrameError::BadKind(k)),
        };
        end(&r)?;
        Ok(Self { protocol, build, selftest, selftest_unix_s, line, token, addrs, clock_ready })
    }
}

/// What admits a peer (6.4). The member list is only a hint: the joiner re-checks it against the start cut.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Welcome {
    pub line: [u8; 32],
    /// Frame 0's session time, in nanoseconds, and the world's tick rate (5.1).
    pub epoch_ns: i64,
    pub rate_hz: u32,
    /// The members, with their keys and last addresses (6.7).
    pub members: Vec<(PeerId, Vec<SocketAddr>)>,
    /// What pe-agree needs to start the join: pe-agree's bytes.
    pub agree: Vec<u8>,
}

impl Welcome {
    pub fn encode(&self) -> Vec<u8> {
        let mut o = vec![VERSION];
        o.extend_from_slice(&self.line);
        o.extend_from_slice(&self.epoch_ns.to_le_bytes());
        o.extend_from_slice(&self.rate_hz.to_le_bytes());
        let members = &self.members[..self.members.len().min(MAX_MEMBERS)];
        put_varint(&mut o, members.len() as u64);
        for (k, a) in members {
            o.extend_from_slice(&k.0);
            put_addrs(&mut o, a);
        }
        put_bytes(&mut o, &self.agree);
        o
    }

    pub fn decode(b: &[u8]) -> Result<Self, FrameError> {
        if b.len() > MAX_WELCOME {
            return Err(FrameError::TooLong);
        }
        let mut r = Reader(b);
        version(&mut r)?;
        let line = array(&mut r)?;
        let epoch_ns = i64::from_le_bytes(array(&mut r)?);
        let rate_hz = u32::from_le_bytes(array(&mut r)?);
        let n = bounded(&mut r, MAX_MEMBERS)?;
        let mut members = Vec::with_capacity(n.min(64));
        for _ in 0..n {
            let key = pe_seams::MemberKey(array(&mut r)?);
            members.push((key, get_addrs(&mut r)?));
        }
        let agree = get_bytes(&mut r, MAX_AGREE)?;
        end(&r)?;
        Ok(Self { line, epoch_ns, rate_hz, members, agree })
    }
}

fn version(r: &mut Reader<'_>) -> Result<(), FrameError> {
    match r.u8()? {
        VERSION => Ok(()),
        v => Err(FrameError::BadVersion(v)),
    }
}

fn end(r: &Reader<'_>) -> Result<(), FrameError> {
    if r.0.is_empty() { Ok(()) } else { Err(FrameError::TooLong) }
}

fn array<const N: usize>(r: &mut Reader<'_>) -> Result<[u8; N], FrameError> {
    r.bytes(N)?.try_into().map_err(|_| FrameError::Truncated)
}

fn bounded(r: &mut Reader<'_>, max: usize) -> Result<usize, FrameError> {
    let n = usize::try_from(r.varint()?).map_err(|_| FrameError::TooLong)?;
    if n > max { Err(FrameError::TooLong) } else { Ok(n) }
}

fn put_bytes(o: &mut Vec<u8>, b: &[u8]) {
    put_varint(o, b.len() as u64);
    o.extend_from_slice(b);
}

fn get_bytes(r: &mut Reader<'_>, max: usize) -> Result<Vec<u8>, FrameError> {
    let n = bounded(r, max)?;
    Ok(r.bytes(n)?.to_vec())
}

/// A rendezvous record (6.7): the key and addresses of a peer to dial.
pub fn encode_rendezvous(key: &PeerId, addrs: &[SocketAddr]) -> Vec<u8> {
    let mut o = key.0.to_vec();
    put_addrs(&mut o, addrs);
    o
}

pub fn decode_rendezvous(b: &[u8]) -> Result<(PeerId, Vec<SocketAddr>), FrameError> {
    let mut r = Reader(b);
    let key = pe_seams::MemberKey(array(&mut r)?);
    let addrs = get_addrs(&mut r)?;
    end(&r)?;
    Ok((key, addrs))
}

/// The largest rendezvous record: a key and `MAX_ADDRS` IPv6 addresses.
pub const MAX_RENDEZVOUS: usize = 32 + 8 + MAX_ADDRS * 19;

fn put_addrs(o: &mut Vec<u8>, a: &[SocketAddr]) {
    let a = &a[..a.len().min(MAX_ADDRS)];
    put_varint(o, a.len() as u64);
    for s in a {
        match s.ip() {
            IpAddr::V4(ip) => {
                o.push(4);
                o.extend_from_slice(&ip.octets());
            }
            IpAddr::V6(ip) => {
                o.push(6);
                o.extend_from_slice(&ip.octets());
            }
        }
        o.extend_from_slice(&s.port().to_le_bytes());
    }
}

fn get_addrs(r: &mut Reader<'_>) -> Result<Vec<SocketAddr>, FrameError> {
    let n = bounded(r, MAX_ADDRS)?;
    let mut v = Vec::with_capacity(n);
    for _ in 0..n {
        let ip = match r.u8()? {
            4 => IpAddr::V4(Ipv4Addr::from(array::<4>(r)?)),
            6 => IpAddr::V6(Ipv6Addr::from(array::<16>(r)?)),
            k => return Err(FrameError::BadKind(k)),
        };
        v.push(SocketAddr::new(ip, u16::from_le_bytes(array(r)?)));
    }
    Ok(v)
}

fn short(h: &[u8; 32]) -> String {
    h[..6].iter().map(|b| format!("{b:02x}")).collect()
}

fn printable(b: &[u8]) -> String {
    String::from_utf8_lossy(b).chars().map(|c| if c.is_control() { '?' } else { c }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hello() -> Hello {
        Hello {
            protocol: 1,
            build: b"pe 0.1".to_vec(),
            selftest: [7; 32],
            selftest_unix_s: 1_800_000_000,
            line: [9; 32],
            token: vec![1, 2, 3],
            addrs: vec!["192.0.2.1:4433".parse().unwrap(), "[2001:db8::1]:4433".parse().unwrap()],
            clock_ready: true,
        }
    }

    #[test]
    fn hello_and_welcome_round_trip() {
        let h = hello();
        assert_eq!(Hello::decode(&h.encode()).unwrap(), h);
        let w = Welcome {
            line: [9; 32],
            epoch_ns: -5,
            rate_hz: 60,
            members: vec![(pe_seams::MemberKey([3; 32]), h.addrs.clone())],
            agree: vec![0xaa; 100],
        };
        assert_eq!(Welcome::decode(&w.encode()).unwrap(), w);
    }

    #[test]
    fn the_guards_refuse_in_order() {
        let a = hello();
        assert_eq!(a.guard(&a), None);
        let mut b = hello();
        b.line = [8; 32];
        b.build = b"pe 0.2".to_vec();
        let r = a.guard(&b).unwrap();
        assert_eq!(r.code, CloseCode::Build, "the build is checked before the line");
        assert!(r.detail.contains("pe 0.1") && r.detail.contains("pe 0.2"), "{}", r.detail);
        b.build = a.build.clone();
        assert_eq!(a.guard(&b).unwrap().code, CloseCode::Line);
        b.protocol = 2;
        assert_eq!(a.guard(&b).unwrap().code, CloseCode::Protocol);
    }

    #[test]
    fn malformed_bytes_are_errors_never_panics() {
        let h = hello().encode();
        for n in 0..h.len() {
            assert!(Hello::decode(&h[..n]).is_err(), "truncated at {n}");
        }
        let mut long = h.clone();
        long.push(0);
        assert!(Hello::decode(&long).is_err());
        // A member count far past the bound is refused before any allocation.
        let mut w = vec![VERSION];
        w.extend_from_slice(&[0; 44]);
        w.extend_from_slice(&[0xc0, 0, 0, 0, 0x40, 0, 0, 0]); // 2^30, in its minimal 8-byte form
        assert_eq!(Welcome::decode(&w), Err(FrameError::TooLong));
        // Every single-byte change: an error or a value, never a panic.
        for i in 0..h.len() {
            for v in [0u8, 0x3f, 0x80, 0xff] {
                let mut m = h.clone();
                m[i] = v;
                let _ = Hello::decode(&m);
            }
        }
    }
}
