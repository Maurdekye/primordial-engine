//! Per-session bearer tokens (api.md 6.1, 10.2; kept from the archived pe-mcp): 256 random bits each, compared in
//! constant time, never logged. Every token maps to one principal: its scopes and its grant (the member it acts for,
//! its producer label, `confirm`). Rights come from that mapping, never from tool arguments. Tokens expire after an
//! idle TTL, on revocation, and when the session ends.

use std::collections::BTreeSet;
use std::fmt;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use subtle::ConstantTimeEq;

use pe_seams::world_api::{Caller, ConnectionId, MemberId, Producer, Scope};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TokenId(pub u64);

/// Every scope, for a token that may do everything (api.md 10.1).
pub const ALL_SCOPES: [Scope; 2] = [Scope::Read, Scope::Edit];

pub fn scope_name(s: Scope) -> &'static str {
    match s {
        Scope::Read => "read",
        Scope::Edit => "edit",
    }
}

pub fn parse_scope(s: &str) -> Option<Scope> {
    match s {
        "read" => Some(Scope::Read),
        "edit" => Some(Scope::Edit),
        _ => None,
    }
}

/// Who is calling and what they may do. Built only by the token store (or, for the embedded stdio
/// test bench, by the process that owns the engine).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Principal {
    /// None for an embedded in-process channel, which belongs to the process that owns the world.
    pub token: Option<TokenId>,
    pub name: String,
    pub scopes: BTreeSet<Scope>,
    pub grant: Grant,
}

/// What a token acts as in the world (api.md 10.2): set by pe-app when it issues the token.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Grant {
    pub member: MemberId,
    pub producer: Producer,
    /// Hold each transaction for the approval of the person whose agent it is (C11: off by default).
    pub confirm: bool,
}

impl Principal {
    pub fn local(name: &str, scopes: &[Scope], grant: Grant) -> Principal {
        Principal { token: None, name: name.into(), scopes: scopes.iter().copied().collect(), grant }
    }
    pub fn has(&self, s: Scope) -> bool {
        self.scopes.contains(&s)
    }
    /// The WorldApi caller. A token is one connection for the table of versions seen (api.md 4.2): one harness
    /// launch gets one token. An embedded channel is connection 0.
    pub fn caller(&self) -> Caller {
        Caller {
            connection: ConnectionId(self.token.map_or(0, |t| t.0)),
            principal: self.name.clone(),
            member: self.grant.member,
            scopes: self.scopes.clone(),
            producer: self.grant.producer.clone(),
            confirm: self.grant.confirm,
        }
    }
}

/// A token's secret. Debug and Display never show it.
pub struct Secret(String);

impl Secret {
    /// The value to put after "Bearer " or send in the pipe handshake.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuthError {
    /// No live token matches (wrong, expired or revoked: deliberately indistinguishable).
    Invalid,
}

struct Entry {
    id: TokenId,
    secret: [u8; 32],
    name: String,
    scopes: BTreeSet<Scope>,
    grant: Grant,
    idle_ttl: Duration,
    last_used: Instant,
}

#[derive(Default)]
struct Inner {
    entries: Vec<Entry>,
    next: u64,
}

#[derive(Default)]
pub struct TokenStore {
    inner: Mutex<Inner>,
}

fn decode(s: &str) -> Option<[u8; 32]> {
    let b = s.as_bytes();
    if b.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, pair) in b.chunks(2).enumerate() {
        let d = |c: u8| (c as char).to_digit(16);
        out[i] = (d(pair[0])? * 16 + d(pair[1])?) as u8;
    }
    Some(out)
}

impl TokenStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Issue a fresh token (one per harness launch or resume).
    pub fn issue(&self, name: &str, scopes: &[Scope], grant: Grant, idle_ttl: Duration) -> (TokenId, Secret) {
        let mut secret = [0u8; 32];
        getrandom::fill(&mut secret).expect("OS random number generator");
        let text: String = secret.iter().map(|b| format!("{b:02x}")).collect();
        let mut g = self.inner.lock().unwrap();
        g.next += 1;
        let id = TokenId(g.next);
        g.entries.push(Entry {
            id,
            secret,
            name: name.into(),
            scopes: scopes.iter().copied().collect(),
            grant,
            idle_ttl,
            last_used: Instant::now(),
        });
        (id, Secret(text))
    }

    fn prune(g: &mut Inner, now: Instant) {
        g.entries.retain(|e| now.duration_since(e.last_used) < e.idle_ttl);
    }

    /// Map a presented secret to its principal. Compares against every live token in constant time
    /// per token, without stopping at a match.
    pub fn authenticate(&self, presented: &str) -> Result<Principal, AuthError> {
        let now = Instant::now();
        let mut g = self.inner.lock().unwrap();
        Self::prune(&mut g, now);
        // A malformed value is compared too (against zeros), so its timing matches a wrong one.
        let (p, well_formed) = match decode(presented.trim()) {
            Some(p) => (p, true),
            None => ([0u8; 32], false),
        };
        let mut hit: Option<usize> = None;
        for (i, e) in g.entries.iter().enumerate() {
            if bool::from(e.secret.ct_eq(&p)) && well_formed {
                hit = Some(i);
            }
        }
        let i = hit.ok_or(AuthError::Invalid)?;
        let e = &mut g.entries[i];
        e.last_used = now;
        Ok(Principal { token: Some(e.id), name: e.name.clone(), scopes: e.scopes.clone(), grant: e.grant.clone() })
    }

    /// Re-check a token that authenticated earlier (every call on a pipe connection), refreshing
    /// its idle timer.
    pub fn check(&self, id: TokenId) -> Result<Principal, AuthError> {
        let now = Instant::now();
        let mut g = self.inner.lock().unwrap();
        Self::prune(&mut g, now);
        let e = g.entries.iter_mut().find(|e| e.id == id).ok_or(AuthError::Invalid)?;
        e.last_used = now;
        Ok(Principal { token: Some(e.id), name: e.name.clone(), scopes: e.scopes.clone(), grant: e.grant.clone() })
    }

    pub fn revoke(&self, id: TokenId) {
        self.inner.lock().unwrap().entries.retain(|e| e.id != id);
    }

    /// The session ended: every token goes.
    pub fn revoke_all(&self) {
        self.inner.lock().unwrap().entries.clear();
    }

    pub fn live(&self) -> usize {
        let mut g = self.inner.lock().unwrap();
        Self::prune(&mut g, Instant::now());
        g.entries.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pe_seams::world_api::ProducerKind;

    const ALL: &[Scope] = &ALL_SCOPES;

    fn g() -> Grant {
        Grant {
            member: MemberId([7; 32]),
            producer: Producer {
                kind: ProducerKind::Agent,
                harness: Some("test".into()),
                principal: None,
                requested_by: None,
            },
            confirm: false,
        }
    }

    #[test]
    fn issue_authenticate_revoke() {
        let s = TokenStore::new();
        let (id, secret) = s.issue("agent", ALL, g(), Duration::from_secs(60));
        assert_eq!(secret.expose().len(), 64);
        let p = s.authenticate(secret.expose()).unwrap();
        assert_eq!((p.token, p.name.as_str()), (Some(id), "agent"));
        assert!(p.has(Scope::Edit));
        assert_eq!(p.caller().connection, ConnectionId(id.0));
        assert_eq!(p.caller().member, MemberId([7; 32]));
        s.revoke(id);
        assert_eq!(s.authenticate(secret.expose()), Err(AuthError::Invalid));
        assert_eq!(s.check(id), Err(AuthError::Invalid));
    }

    #[test]
    fn wrong_malformed_and_other_tokens() {
        let s = TokenStore::new();
        let (_, a) = s.issue("a", &[Scope::Read], g(), Duration::from_secs(60));
        let (_, b) = s.issue("b", ALL, g(), Duration::from_secs(60));
        assert_eq!(s.authenticate(a.expose()).unwrap().name, "a");
        assert_eq!(s.authenticate(b.expose()).unwrap().name, "b");
        let mut wrong = a.expose().to_string();
        let last = if wrong.ends_with('0') { "1" } else { "0" };
        wrong.replace_range(63.., last);
        assert_eq!(s.authenticate(&wrong), Err(AuthError::Invalid));
        for bad in ["", "zz", &"g".repeat(64), &a.expose()[..63], &format!("{}0", a.expose())] {
            assert_eq!(s.authenticate(bad), Err(AuthError::Invalid), "{bad:?}");
        }
    }

    #[test]
    fn idle_ttl_expires_and_use_refreshes() {
        let s = TokenStore::new();
        // Wide margins: tests run at low priority on a loaded machine.
        let (id, t) = s.issue("a", ALL, g(), Duration::from_millis(1500));
        std::thread::sleep(Duration::from_millis(900));
        assert!(s.check(id).is_ok()); // refreshes
        std::thread::sleep(Duration::from_millis(900));
        assert!(s.authenticate(t.expose()).is_ok()); // 1800 ms since issue, 900 since the refresh
        std::thread::sleep(Duration::from_millis(1600));
        assert_eq!(s.authenticate(t.expose()), Err(AuthError::Invalid));
        assert_eq!(s.live(), 0);
    }

    #[test]
    fn revoke_all_ends_the_session() {
        let s = TokenStore::new();
        let (_, a) = s.issue("a", ALL, g(), Duration::from_secs(60));
        s.issue("b", ALL, g(), Duration::from_secs(60));
        s.revoke_all();
        assert_eq!(s.live(), 0);
        assert!(s.authenticate(a.expose()).is_err());
    }

    #[test]
    fn secrets_are_distinct_and_redacted() {
        let s = TokenStore::new();
        let (_, a) = s.issue("a", ALL, g(), Duration::from_secs(60));
        let (_, b) = s.issue("a", ALL, g(), Duration::from_secs(60));
        assert_ne!(a.expose(), b.expose());
        assert_eq!(format!("{a:?}"), "Secret(<redacted>)");
    }
}
