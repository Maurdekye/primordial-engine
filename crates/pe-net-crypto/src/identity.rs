//! Peer identity, invites and reply codes (transport.md 2.2, 2.4, 6.1).
//!
//! A member's Ed25519 key is its TLS raw public key (RFC 7250): the "certificate" is just the
//! SubjectPublicKeyInfo, so pinning is a byte comparison. Signatures in the handshake are checked with
//! `verify_strict` (the provider's only algorithm).
//!
//! Text formats, ASCII only, fields separated by `/` (which no socket address contains):
//! - invite:     `pe1:<id, 16 hex>/<key, 64 hex>/<expiry, unix seconds>/<addr>,<addr>.../<check, 8 hex>`
//! - reply code: `pe1r:<invite id, 16 hex>/<key, 64 hex>/<addr>,<addr>.../<check, 8 hex>`
//!
//! The check is the first 4 bytes of SHA-256 over everything before the last `/`: it catches typing and
//! copy errors, not forgery (the key is what the handshake proves).

use std::fmt;
use std::net::SocketAddr;
use std::sync::Arc;

use ed25519_dalek::pkcs8::{EncodePrivateKey, EncodePublicKey};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, verify_tls13_signature_with_raw_key};
use rustls::pki_types::{
    CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, SubjectPublicKeyInfoDer, UnixTime,
};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::sign::CertifiedKey;
use rustls::{DigitallySignedStruct, DistinguishedName, Error, SignatureScheme};
use sha2::{Digest, Sha256};

use crate::random::Random;

pub type PeerId = [u8; 32];

/// The most addresses an invite or reply code may carry.
pub const MAX_ADDRS: usize = 16;
/// The longest invite or reply code accepted.
pub const MAX_TEXT: usize = 2048;

pub struct Identity {
    signing: ed25519_dalek::SigningKey, // zeroized on drop
}

impl fmt::Debug for Identity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Identity({})", hex(&self.public()))
    }
}

impl Identity {
    pub fn generate(rng: &'static dyn Random) -> Result<Self, Error> {
        let mut seed = zeroize::Zeroizing::new([0u8; 32]);
        crate::random::fill(rng.as_secure_random(), seed.as_mut())?;
        Ok(Self::from_seed(&seed))
    }

    pub fn from_seed(seed: &[u8; 32]) -> Self {
        Self { signing: ed25519_dalek::SigningKey::from_bytes(seed) }
    }

    pub fn public(&self) -> PeerId {
        self.signing.verifying_key().to_bytes()
    }

    /// The raw public key as TLS sends it: SubjectPublicKeyInfo DER.
    pub fn spki(&self) -> Result<Vec<u8>, Error> {
        spki_of(&self.public())
    }

    /// rustls's signing key, loaded through `provider`.
    pub fn certified_key(&self, provider: &CryptoProvider) -> Result<Arc<CertifiedKey>, Error> {
        let pkcs8 = self.signing.to_pkcs8_der().map_err(|_| Error::General("pkcs8".into()))?;
        let key = provider
            .key_provider
            .load_private_key(PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(pkcs8.as_bytes().to_vec())))?;
        Ok(Arc::new(CertifiedKey::new(vec![CertificateDer::from(self.spki()?)], key)))
    }
}

/// Seam S1: entries are signed with the member key. pe-seams adds `SIGNING_PREFIX` to the bytes it passes,
/// so no entry signature can stand for a TLS CertificateVerify, or the other way round (transport.md 2.2).
impl pe_seams::Signer for Identity {
    fn key(&self) -> pe_seams::MemberKey {
        pe_seams::MemberKey(self.public())
    }

    fn sign(&self, msg: &[u8]) -> pe_seams::Signature {
        use ed25519_dalek::Signer as _;
        pe_seams::Signature(self.signing.sign(msg).to_bytes())
    }
}

/// Seam S1's verifier: Ed25519 `verify_strict` (small-order keys and malleable signatures refused).
#[derive(Debug, Default, Clone, Copy)]
pub struct StrictVerifier;

impl pe_seams::Verifier for StrictVerifier {
    fn verify(&self, key: &pe_seams::MemberKey, msg: &[u8], sig: &pe_seams::Signature) -> bool {
        crate::sign::verify_strict(&key.0, msg, &sig.0).is_ok()
    }
}

/// SubjectPublicKeyInfo DER for an Ed25519 public key; an error for bytes that aren't a valid point.
pub fn spki_of(public: &PeerId) -> Result<Vec<u8>, Error> {
    let key =
        ed25519_dalek::VerifyingKey::from_bytes(public).map_err(|_| Error::General("not an Ed25519 key".into()))?;
    Ok(key.to_public_key_der().map_err(|_| Error::General("spki".into()))?.as_bytes().to_vec())
}

/// The Ed25519 key inside an SPKI that `spki_of` would produce, or `None`.
pub fn peer_id_of(spki: &[u8]) -> Option<PeerId> {
    if spki.len() != 44 {
        return None;
    }
    let key: PeerId = spki[12..].try_into().ok()?;
    (spki_of(&key).ok()?.as_slice() == spki).then_some(key)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TextError {
    TooLong,
    NotAscii,
    BadPrefix,
    BadFields,
    BadHex,
    BadKey,
    BadAddr,
    NoAddrs,
    TooManyAddrs,
    BadExpiry,
    BadCheck,
}

impl fmt::Display for TextError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self, f)
    }
}

impl std::error::Error for TextError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invite {
    pub id: [u8; 8],
    pub key: PeerId,
    pub expiry_unix_s: u64,
    pub addrs: Vec<SocketAddr>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reply {
    pub invite_id: [u8; 8],
    pub key: PeerId,
    pub addrs: Vec<SocketAddr>,
}

impl Invite {
    pub fn encode(&self) -> String {
        with_check(format!(
            "pe1:{}/{}/{}/{}",
            hex(&self.id),
            hex(&self.key),
            self.expiry_unix_s,
            addrs_text(&self.addrs)
        ))
    }

    pub fn decode(s: &str) -> Result<Self, TextError> {
        let body = checked_body(s, "pe1:")?;
        let f: Vec<&str> = body.split('/').collect();
        let [id, key, expiry, addrs] = f[..] else { return Err(TextError::BadFields) };
        Ok(Self {
            id: unhex_n(id)?,
            key: parse_key(key)?,
            expiry_unix_s: parse_u64(expiry).ok_or(TextError::BadExpiry)?,
            addrs: parse_addrs(addrs)?,
        })
    }

    pub fn is_expired(&self, now_unix_s: u64) -> bool {
        now_unix_s >= self.expiry_unix_s
    }
}

impl Reply {
    pub fn encode(&self) -> String {
        with_check(format!("pe1r:{}/{}/{}", hex(&self.invite_id), hex(&self.key), addrs_text(&self.addrs)))
    }

    pub fn decode(s: &str) -> Result<Self, TextError> {
        let body = checked_body(s, "pe1r:")?;
        let f: Vec<&str> = body.split('/').collect();
        let [id, key, addrs] = f[..] else { return Err(TextError::BadFields) };
        Ok(Self { invite_id: unhex_n(id)?, key: parse_key(key)?, addrs: parse_addrs(addrs)? })
    }
}

fn check_of(body: &str) -> String {
    hex(&Sha256::digest(body.as_bytes())[..4])
}

fn with_check(body: String) -> String {
    let c = check_of(&body);
    format!("{body}/{c}")
}

/// The text between the prefix and the check, after the length, ASCII, prefix and check tests.
fn checked_body<'a>(s: &'a str, prefix: &str) -> Result<&'a str, TextError> {
    let s = s.trim();
    if s.len() > MAX_TEXT {
        return Err(TextError::TooLong);
    }
    if !s.is_ascii() {
        return Err(TextError::NotAscii);
    }
    if !s.starts_with(prefix) {
        return Err(TextError::BadPrefix);
    }
    let (body, check) = s.rsplit_once('/').ok_or(TextError::BadFields)?;
    if check.len() != 8 || check_of(body) != check.to_ascii_lowercase() {
        return Err(TextError::BadCheck);
    }
    Ok(&body[prefix.len()..])
}

fn addrs_text(addrs: &[SocketAddr]) -> String {
    addrs.iter().map(|a| a.to_string()).collect::<Vec<_>>().join(",")
}

fn parse_addrs(s: &str) -> Result<Vec<SocketAddr>, TextError> {
    let v: Vec<SocketAddr> = s
        .split(',')
        .filter(|x| !x.is_empty())
        .map(|x| x.parse().map_err(|_| TextError::BadAddr))
        .collect::<Result<_, _>>()?;
    if v.is_empty() {
        return Err(TextError::NoAddrs);
    }
    if v.len() > MAX_ADDRS {
        return Err(TextError::TooManyAddrs);
    }
    Ok(v)
}

fn parse_key(s: &str) -> Result<PeerId, TextError> {
    let k: PeerId = unhex_n(s)?;
    ed25519_dalek::VerifyingKey::from_bytes(&k).map_err(|_| TextError::BadKey)?;
    Ok(k)
}

fn parse_u64(s: &str) -> Option<u64> {
    if s.is_empty() || s.len() > 20 || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Hex of exactly N bytes. The input is ASCII by now; anything else is an error, never a panic.
fn unhex_n<const N: usize>(s: &str) -> Result<[u8; N], TextError> {
    let b = s.as_bytes();
    if b.len() != 2 * N {
        return Err(TextError::BadHex);
    }
    let nib = |c: u8| match c {
        b'0'..=b'9' => Ok(c - b'0'),
        b'a'..=b'f' => Ok(c - b'a' + 10),
        b'A'..=b'F' => Ok(c - b'A' + 10),
        _ => Err(TextError::BadHex),
    };
    let mut out = [0u8; N];
    for (i, o) in out.iter_mut().enumerate() {
        *o = (nib(b[2 * i])? << 4) | nib(b[2 * i + 1])?;
    }
    Ok(out)
}

fn verify_sig(
    message: &[u8],
    cert: &CertificateDer<'_>,
    dss: &DigitallySignedStruct,
) -> Result<HandshakeSignatureValid, Error> {
    verify_tls13_signature_with_raw_key(
        message,
        &SubjectPublicKeyInfoDer::from(cert.as_ref()),
        dss,
        &crate::sign::ALGORITHMS,
    )
}

/// The joiner's check of the side it dials: the raw key must be the invite's (or the reply code's).
#[derive(Debug)]
pub struct PinnedKey {
    spki: Vec<u8>,
}

impl PinnedKey {
    pub fn new(key: &PeerId) -> Result<Self, Error> {
        Ok(Self { spki: spki_of(key)? })
    }
}

impl ServerCertVerifier for PinnedKey {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        _name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, Error> {
        if intermediates.is_empty() && end_entity.as_ref() == self.spki.as_slice() {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(Error::General("peer key is not the pinned key".into()))
        }
    }

    fn verify_tls12_signature(
        &self,
        _: &[u8],
        _: &CertificateDer<'_>,
        _: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        Err(Error::General("TLS 1.2 is not used".into()))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        verify_sig(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![SignatureScheme::ED25519]
    }

    fn requires_raw_public_keys(&self) -> bool {
        true
    }
}

/// The dialled side's check of a dialler: any well-formed Ed25519 raw key. Admission is decided above
/// the transport, from the agreed governance, and sent as the welcome (transport.md 2.3).
#[derive(Debug, Default)]
pub struct AnyPeerKey;

impl ClientCertVerifier for AnyPeerKey {
    /// Said outright rather than left to rustls's defaults: every dialler must present a key and prove it
    /// (@design-reviewer's Medium on pe-net-crypto; tests/tls.rs).
    fn offer_client_auth(&self) -> bool {
        true
    }

    fn client_auth_mandatory(&self) -> bool {
        true
    }

    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, Error> {
        if intermediates.is_empty() && peer_id_of(end_entity.as_ref()).is_some() {
            Ok(ClientCertVerified::assertion())
        } else {
            Err(Error::General("not an Ed25519 raw public key".into()))
        }
    }

    fn verify_tls12_signature(
        &self,
        _: &[u8],
        _: &CertificateDer<'_>,
        _: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        Err(Error::General("TLS 1.2 is not used".into()))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        verify_sig(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![SignatureScheme::ED25519]
    }

    fn requires_raw_public_keys(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn invite() -> Invite {
        Invite {
            id: [1, 2, 3, 4, 5, 6, 7, 8],
            key: Identity::from_seed(&[9; 32]).public(),
            expiry_unix_s: 1_800_000_000,
            addrs: vec!["192.168.0.10:47000".parse().unwrap(), "[fd00::1]:47000".parse().unwrap()],
        }
    }

    #[test]
    fn invite_round_trip_and_expiry() {
        let i = invite();
        let s = i.encode();
        assert!(s.starts_with("pe1:0102030405060708/"));
        assert_eq!(Invite::decode(&s).unwrap(), i);
        assert_eq!(Invite::decode(&format!("  {s}\n")).unwrap(), i);
        assert!(!i.is_expired(1_799_999_999));
        assert!(i.is_expired(1_800_000_000));
    }

    #[test]
    fn reply_round_trip() {
        let r = Reply {
            invite_id: [8; 8],
            key: Identity::from_seed(&[3; 32]).public(),
            addrs: vec!["10.0.0.2:5000".parse().unwrap()],
        };
        assert_eq!(Reply::decode(&r.encode()).unwrap(), r);
        assert_eq!(Invite::decode(&r.encode()), Err(TextError::BadPrefix));
    }

    #[test]
    fn malformed_text_is_an_error_never_a_panic() {
        let s = invite().encode();
        // A non-ASCII character anywhere (the M0 parser panicked on this; @chunks-reviewer).
        assert_eq!(Invite::decode(&s.replacen('0', "é", 1)), Err(TextError::NotAscii));
        // A typing error is caught by the check.
        let mut t = s.clone().into_bytes();
        let k = t.len() - 12;
        t[k] = if t[k] == b'1' { b'2' } else { b'1' };
        assert_eq!(Invite::decode(std::str::from_utf8(&t).unwrap()), Err(TextError::BadCheck));
        // Cut anywhere, the text fails cleanly.
        for n in 0..s.len() {
            let _ = Invite::decode(&s[..n]);
        }
        assert_eq!(Invite::decode(&"x".repeat(MAX_TEXT + 1)), Err(TextError::TooLong));
        // Every byte value at every position: errors only.
        for pos in 0..s.len() {
            for b in 0u8..=127 {
                let mut v = s.clone().into_bytes();
                v[pos] = b;
                if let Ok(t) = std::str::from_utf8(&v) {
                    let _ = Invite::decode(t);
                }
            }
        }
    }

    #[test]
    fn s1_signer_and_strict_verifier() {
        use pe_seams::{Signer, Verifier};
        let id = Identity::from_seed(&[6; 32]);
        let msg = [pe_seams::entry::SIGNING_PREFIX, b"entry bytes"].concat();
        let sig = id.sign(&msg);
        assert_eq!(id.key().0, id.public());
        assert!(StrictVerifier.verify(&id.key(), &msg, &sig));
        assert!(!StrictVerifier.verify(&id.key(), b"entry bytes", &sig), "the prefix is part of what is signed");
        let mut bad = pe_seams::Signature(sig.0);
        bad.0[10] ^= 1;
        assert!(!StrictVerifier.verify(&id.key(), &msg, &bad));
        let other = Identity::from_seed(&[7; 32]);
        assert!(!StrictVerifier.verify(&other.key(), &msg, &sig));
        // The identity point as a key: refused whatever the signature.
        let mut weak = [0u8; 32];
        weak[0] = 1;
        let mut s0 = [0u8; 64];
        s0[0] = 1;
        assert!(!StrictVerifier.verify(&pe_seams::MemberKey(weak), &msg, &pe_seams::Signature(s0)));
    }

    #[test]
    fn spki_round_trip() {
        let id = Identity::from_seed(&[5; 32]);
        let spki = id.spki().unwrap();
        assert_eq!(peer_id_of(&spki), Some(id.public()));
        let mut bad = spki.clone();
        bad[0] ^= 1;
        assert_eq!(peer_id_of(&bad), None);
    }
}
