//! The session file every member shares, and each member's own key file.
//!
//! `session.json` pins the session: its id, scene, length, and each member's id, UDP address and
//! Ed25519 public key. A member's secret key lives in its own `member-<id>.key` (64 hex digits) and
//! never in the session file.

use std::net::SocketAddr;
use std::path::Path;

use ed25519_dalek::{SigningKey, VerifyingKey};
use pe_session::session::{KeyRing, PublicKeys, SceneKind, SessionId, SimConfig};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct MemberEntry {
    pub id: u32,
    pub addr: SocketAddr,
    /// Ed25519 public key, 64 hex digits.
    pub key: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct SessionFile {
    /// Random per session, in every datagram header: a quick filter for another session's traffic.
    pub session: u64,
    /// A fresh random 256-bit nonce (64 hex digits) in the genesis (design 10.3, review SX-1): two
    /// sessions with the same members and scene still get different ids.
    pub nonce: String,
    pub name: String,
    /// "pile" (size x size x 2 objects) or "ballpit" (size balls, one hand per member).
    pub scene: String,
    pub size: u32,
    /// The session's length in frames: each member stops once every frame below it is confirmed.
    pub frames: u64,
    pub window: u64,
    pub seed: u64,
    pub members: Vec<MemberEntry>,
}

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

pub fn unhex32(s: &str) -> Result<[u8; 32], String> {
    let s = s.trim();
    if s.len() != 64 {
        return Err("expected 64 hex digits".into());
    }
    let mut o = [0u8; 32];
    for (i, c) in s.as_bytes().chunks(2).enumerate() {
        o[i] = u8::from_str_radix(std::str::from_utf8(c).map_err(|e| e.to_string())?, 16).map_err(|e| e.to_string())?;
    }
    Ok(o)
}

pub fn new_key() -> SigningKey {
    let mut b = [0u8; 32];
    getrandom::fill(&mut b).expect("OS random number generator");
    SigningKey::from_bytes(&b)
}

pub fn read_key(path: &Path) -> Result<SigningKey, String> {
    let s = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(SigningKey::from_bytes(&unhex32(&s).map_err(|e| format!("{}: {e}", path.display()))?))
}

impl SessionFile {
    pub fn read(path: &Path) -> Result<SessionFile, String> {
        let s = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let f: SessionFile = serde_json::from_str(&s).map_err(|e| format!("{}: {e}", path.display()))?;
        f.validate()?;
        Ok(f)
    }

    pub fn validate(&self) -> Result<(), String> {
        if !(2..=8).contains(&self.members.len()) {
            return Err("a session has 2 to 8 members".into());
        }
        let mut ids: Vec<u32> = self.members.iter().map(|m| m.id).collect();
        ids.sort();
        ids.dedup();
        if ids.len() != self.members.len() || ids.contains(&0) {
            return Err("member ids must be distinct and non-zero (0 is world logic)".into());
        }
        for m in &self.members {
            VerifyingKey::from_bytes(&unhex32(&m.key)?).map_err(|e| format!("member {}: bad key: {e}", m.id))?;
        }
        unhex32(&self.nonce).map_err(|e| format!("nonce: {e}"))?;
        self.scene_kind()?;
        if self.frames == 0 || !(30..=600).contains(&self.window) {
            return Err("frames must be positive and window in 30..=600".into());
        }
        Ok(())
    }

    pub fn ids(&self) -> Vec<u32> {
        let mut v: Vec<u32> = self.members.iter().map(|m| m.id).collect();
        v.sort();
        v
    }

    pub fn scene_kind(&self) -> Result<SceneKind, String> {
        match self.scene.as_str() {
            "pile" if (1..=8).contains(&self.size) => Ok(SceneKind::Pile { side: self.size as usize, layers: 2 }),
            "ballpit" if (1..=500).contains(&self.size) => Ok(SceneKind::BallPit { balls: self.size as usize, hands: self.members.len() }),
            _ => Err(format!("scene {:?} size {}: use pile 1..=8 or ballpit 1..=500", self.scene, self.size)),
        }
    }

    pub fn sim_config(&self) -> SimConfig {
        SimConfig { window: self.window, suspect_after: self.window * 3 / 2, seed: self.seed, ..SimConfig::default() }
    }

    pub fn key_of(&self, id: u32) -> Option<VerifyingKey> {
        let m = self.members.iter().find(|m| m.id == id)?;
        VerifyingKey::from_bytes(&unhex32(&m.key).ok()?).ok()
    }

    /// The session's 32-byte id (design 10.3): the BLAKE3 hash of its genesis, this file's canonical
    /// JSON, which names the world (name and scene), the genesis members with their addresses and
    /// keys, the config and the 256-bit nonce. pe-session binds every entry signature to it, so no
    /// entry verifies in another session. In M1 the genesis is this file, handed out before the
    /// session, not a signed genesis entry (that comes with joins, phase 3).
    pub fn genesis_id(&self) -> SessionId {
        let mut h = blake3::Hasher::new();
        h.update(b"pe-app-genesis\0");
        h.update(&serde_json::to_vec(self).expect("serialisable"));
        h.finalize().into()
    }

    /// This member's key ring: its secret key and every member's pinned public key.
    pub fn key_ring(&self, secret: SigningKey) -> KeyRing {
        let public = PublicKeys::new(self.ids().into_iter().map(|m| (m, self.key_of(m).expect("validated key"))).collect());
        KeyRing::new(Some(secret), public, self.genesis_id())
    }

    pub fn addr_of(&self, id: u32) -> Option<SocketAddr> {
        self.members.iter().find(|m| m.id == id).map(|m| m.addr)
    }
}
