//! quinn-proto's crypto traits over `rustls::quic`, without ring or aws-lc-rs.
//!
//! A port of quinn-proto 0.11's `src/crypto/rustls.rs` and `ring_like.rs` (MIT/Apache-2.0), which compile
//! only with its ring or aws-lc-rs features. The TLS session logic is the same. The four uses of ring
//! (the retry integrity tag, the stateless-reset HMAC, the token key's HKDF and AEAD) use RustCrypto.
//! Changes from the original (transport.md 2.1, 2.5):
//! - 0-RTT and session resumption are off: no early keys, and the configs refuse them.
//! - Nothing a peer sends can panic. A header that can't be unmasked is left as is, and its packet then
//!   fails the AEAD and is dropped. A retry tag for an unknown version is all zeros, and fails to verify.
//! - A config that rustls refuses is an error when the config is built, never when a session starts.

use std::{any::Any, io, sync::Arc};

use aes_gcm::aead::AeadInOut;
use aes_gcm::{Aes128Gcm, Aes256Gcm, KeyInit};
use bytes::BytesMut;
use hmac::{Hmac, Mac};
use quinn_proto::crypto::{self, CryptoError, ExportKeyingMaterialError, KeyPair, Keys, UnsupportedVersion};
use quinn_proto::transport_parameters::TransportParameters;
use quinn_proto::{ConnectError, ConnectionId, Side, TransportError, TransportErrorCode};
use rustls::pki_types::ServerName;
use rustls::quic::{Connection, KeyChange, Secrets, Suite, Version};
use sha2::Sha256;
use zeroize::Zeroizing;

use crate::random::Random;

fn rside(s: Side) -> rustls::Side {
    match s {
        Side::Client => rustls::Side::Client,
        Side::Server => rustls::Side::Server,
    }
}

struct TlsSession {
    version: Version,
    got_handshake_data: bool,
    next_secrets: Option<Secrets>,
    inner: Connection,
    suite: Suite,
}

impl TlsSession {
    fn side(&self) -> Side {
        match self.inner {
            Connection::Client(_) => Side::Client,
            Connection::Server(_) => Side::Server,
        }
    }
}

/// What `Connection::handshake_data()` returns: the negotiated ALPN protocol.
pub struct HandshakeData {
    pub protocol: Option<Vec<u8>>,
}

impl crypto::Session for TlsSession {
    fn initial_keys(&self, dst_cid: &ConnectionId, side: Side) -> Keys {
        initial_keys(self.version, *dst_cid, side, &self.suite)
    }

    fn handshake_data(&self) -> Option<Box<dyn Any>> {
        if !self.got_handshake_data {
            return None;
        }
        Some(Box::new(HandshakeData { protocol: self.inner.alpn_protocol().map(|x| x.into()) }))
    }

    /// The peer's raw public key (SubjectPublicKeyInfo DER), as `Vec<CertificateDer<'static>>`.
    fn peer_identity(&self) -> Option<Box<dyn Any>> {
        self.inner
            .peer_certificates()
            .map(|v| -> Box<dyn Any> { Box::new(v.iter().map(|c| c.clone().into_owned()).collect::<Vec<_>>()) })
    }

    fn early_crypto(&self) -> Option<(Box<dyn crypto::HeaderKey>, Box<dyn crypto::PacketKey>)> {
        None // 0-RTT is off (transport.md 2.1)
    }

    fn early_data_accepted(&self) -> Option<bool> {
        None
    }

    fn is_handshaking(&self) -> bool {
        self.inner.is_handshaking()
    }

    fn read_handshake(&mut self, buf: &[u8]) -> Result<bool, TransportError> {
        self.inner.read_hs(buf).map_err(|e| match self.inner.alert() {
            Some(alert) => TransportError { code: TransportErrorCode::crypto(alert.into()), frame: None, reason: e.to_string() },
            None => TransportError { code: TransportErrorCode::PROTOCOL_VIOLATION, frame: None, reason: format!("TLS error: {e}") },
        })?;
        if !self.got_handshake_data {
            let have_server_name = match self.inner {
                Connection::Client(_) => false,
                Connection::Server(ref s) => s.server_name().is_some(),
            };
            if self.inner.alpn_protocol().is_some() || have_server_name || !self.is_handshaking() {
                self.got_handshake_data = true;
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn transport_parameters(&self) -> Result<Option<TransportParameters>, TransportError> {
        match self.inner.quic_transport_parameters() {
            None => Ok(None),
            Some(buf) => TransportParameters::read(self.side(), &mut io::Cursor::new(buf)).map(Some).map_err(Into::into),
        }
    }

    fn write_handshake(&mut self, buf: &mut Vec<u8>) -> Option<Keys> {
        let keys = match self.inner.write_hs(buf)? {
            KeyChange::Handshake { keys } => keys,
            KeyChange::OneRtt { keys, next } => {
                self.next_secrets = Some(next);
                keys
            }
        };
        Some(Keys {
            header: KeyPair { local: Box::new(Hk(keys.local.header)), remote: Box::new(Hk(keys.remote.header)) },
            packet: KeyPair { local: Box::new(Pk(keys.local.packet)), remote: Box::new(Pk(keys.remote.packet)) },
        })
    }

    fn next_1rtt_keys(&mut self) -> Option<KeyPair<Box<dyn crypto::PacketKey>>> {
        let keys = self.next_secrets.as_mut()?.next_packet_keys();
        Some(KeyPair { local: Box::new(Pk(keys.local)), remote: Box::new(Pk(keys.remote)) })
    }

    fn is_valid_retry(&self, orig_dst_cid: &ConnectionId, header: &[u8], payload: &[u8]) -> bool {
        let Some(tag_start) = payload.len().checked_sub(16) else { return false };
        let mut pseudo = Vec::with_capacity(header.len() + payload.len() + orig_dst_cid.len() + 1);
        pseudo.push(orig_dst_cid.len() as u8);
        pseudo.extend_from_slice(orig_dst_cid);
        pseudo.extend_from_slice(header);
        pseudo.extend_from_slice(&payload[..tag_start]);
        match retry_tag_for(self.version, &pseudo) {
            Some(tag) => tag.as_slice() == &payload[tag_start..],
            None => false,
        }
    }

    fn export_keying_material(&self, output: &mut [u8], label: &[u8], context: &[u8]) -> Result<(), ExportKeyingMaterialError> {
        self.inner.export_keying_material(output, label, Some(context)).map(|_| ()).map_err(|_| ExportKeyingMaterialError)
    }
}

const RETRY_INTEGRITY_KEY_DRAFT: [u8; 16] =
    [0xcc, 0xce, 0x18, 0x7e, 0xd0, 0x9a, 0x09, 0xd0, 0x57, 0x28, 0x15, 0x5a, 0x6c, 0xb9, 0x6b, 0xe1];
const RETRY_INTEGRITY_NONCE_DRAFT: [u8; 12] = [0xe5, 0x49, 0x30, 0xf9, 0x7f, 0x21, 0x36, 0xf0, 0x53, 0x0a, 0x8c, 0x1c];
const RETRY_INTEGRITY_KEY_V1: [u8; 16] =
    [0xbe, 0x0c, 0x69, 0x0b, 0x9f, 0x66, 0x57, 0x5a, 0x1d, 0x76, 0x6b, 0x54, 0xe3, 0x68, 0xc8, 0x4e];
const RETRY_INTEGRITY_NONCE_V1: [u8; 12] = [0x46, 0x15, 0x99, 0xd3, 0x5d, 0x63, 0x2b, 0xf2, 0x23, 0x98, 0x25, 0xbb];

/// RFC 9001 5.8: the AES-128-GCM tag over the retry pseudo-packet, with an empty plaintext. `None` for a
/// version without retry keys.
pub(crate) fn retry_tag_for(version: Version, pseudo_packet: &[u8]) -> Option<[u8; 16]> {
    let (nonce, key) = match version {
        Version::V1 => (RETRY_INTEGRITY_NONCE_V1, RETRY_INTEGRITY_KEY_V1),
        Version::V1Draft => (RETRY_INTEGRITY_NONCE_DRAFT, RETRY_INTEGRITY_KEY_DRAFT),
        _ => return None,
    };
    let aead = Aes128Gcm::new_from_slice(&key).ok()?;
    let tag = aead.encrypt_inout_detached(&nonce.into(), pseudo_packet, (&mut [][..]).into()).ok()?;
    Some(tag.into())
}

struct Hk(Box<dyn rustls::quic::HeaderProtectionKey>);

impl Hk {
    /// quinn-proto passes the whole packet and the packet number's offset; the sample starts 4 bytes after
    /// the offset (RFC 9001 5.4.2). `None` when the packet is too short: the caller leaves it alone.
    fn split(pn_offset: usize, sample_len: usize, packet: &mut [u8]) -> Option<(&mut u8, &mut [u8], &[u8])> {
        if pn_offset == 0 || packet.len() < pn_offset + 4 + sample_len {
            return None;
        }
        let (header, sample) = packet.split_at_mut(pn_offset + 4);
        let (first, rest) = header.split_at_mut(1);
        let pn_end = Ord::min(pn_offset + 3, rest.len());
        Some((&mut first[0], &mut rest[pn_offset - 1..pn_end], &sample[..sample_len]))
    }
}

impl crypto::HeaderKey for Hk {
    fn decrypt(&self, pn_offset: usize, packet: &mut [u8]) {
        if let Some((first, pn, sample)) = Self::split(pn_offset, self.0.sample_len(), packet) {
            // On error the header stays masked; the packet then fails its AEAD check and is dropped.
            let _ = self.0.decrypt_in_place(sample, first, pn);
        }
    }

    fn encrypt(&self, pn_offset: usize, packet: &mut [u8]) {
        if let Some((first, pn, sample)) = Self::split(pn_offset, self.0.sample_len(), packet) {
            let _ = self.0.encrypt_in_place(sample, first, pn);
        }
    }

    fn sample_size(&self) -> usize {
        self.0.sample_len()
    }
}

struct Pk(Box<dyn rustls::quic::PacketKey>);

impl crypto::PacketKey for Pk {
    fn encrypt(&self, packet: u64, buf: &mut [u8], header_len: usize) {
        let tag_len = self.0.tag_len();
        if buf.len() < header_len + tag_len {
            return; // quinn always reserves the tag; a short buffer would fail the peer's check
        }
        let (header, payload_tag) = buf.split_at_mut(header_len);
        let (payload, tag_storage) = payload_tag.split_at_mut(payload_tag.len() - tag_len);
        if let Ok(tag) = self.0.encrypt_in_place(packet, &*header, payload) {
            tag_storage.copy_from_slice(tag.as_ref());
        }
    }

    fn decrypt(&self, packet: u64, header: &[u8], payload: &mut BytesMut) -> Result<(), CryptoError> {
        let plain_len = self.0.decrypt_in_place(packet, header, payload.as_mut()).map_err(|_| CryptoError)?.len();
        payload.truncate(plain_len);
        Ok(())
    }

    fn tag_len(&self) -> usize {
        self.0.tag_len()
    }

    fn confidentiality_limit(&self) -> u64 {
        self.0.confidentiality_limit()
    }

    fn integrity_limit(&self) -> u64 {
        self.0.integrity_limit()
    }
}

fn initial_keys(version: Version, dst_cid: ConnectionId, side: Side, suite: &Suite) -> Keys {
    let keys = suite.keys(&dst_cid, rside(side), version);
    Keys {
        header: KeyPair { local: Box::new(Hk(keys.local.header)), remote: Box::new(Hk(keys.remote.header)) },
        packet: KeyPair { local: Box::new(Pk(keys.local.packet)), remote: Box::new(Pk(keys.remote.packet)) },
    }
}

fn interpret_version(version: u32) -> Result<Version, UnsupportedVersion> {
    match version {
        0xff00_001d..=0xff00_0020 => Ok(Version::V1Draft),
        0x0000_0001 | 0xff00_0021..=0xff00_0022 => Ok(Version::V1),
        _ => Err(UnsupportedVersion),
    }
}

fn to_vec(params: &TransportParameters) -> Vec<u8> {
    let mut bytes = Vec::new();
    params.write(&mut bytes);
    bytes
}

fn initial_suite() -> Suite {
    match crate::quic::tls13_aes_128_gcm_sha256().quic_suite() {
        Some(s) => s,
        None => unreachable!("the suite has QUIC keys"),
    }
}

/// A rustls config that QUIC can't use.
#[derive(Debug)]
pub struct ConfigError(pub String);

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "unusable TLS config for QUIC: {}", self.0)
    }
}

impl std::error::Error for ConfigError {}

pub struct QuicClientConfig {
    inner: Arc<rustls::ClientConfig>,
    initial: Suite,
}

impl QuicClientConfig {
    /// Wraps a rustls client config. Refuses 0-RTT, and a config rustls can't run QUIC with.
    pub fn new(inner: rustls::ClientConfig) -> Result<Self, ConfigError> {
        if inner.enable_early_data {
            return Err(ConfigError("0-RTT must stay off (transport.md 2.1)".into()));
        }
        let inner = Arc::new(inner);
        let probe = ServerName::try_from("pe").map_err(|e| ConfigError(e.to_string()))?;
        rustls::quic::ClientConnection::new(inner.clone(), Version::V1, probe, Vec::new()).map_err(|e| ConfigError(e.to_string()))?;
        Ok(Self { inner, initial: initial_suite() })
    }
}

impl crypto::ClientConfig for QuicClientConfig {
    fn start_session(self: Arc<Self>, version: u32, server_name: &str, params: &TransportParameters) -> Result<Box<dyn crypto::Session>, ConnectError> {
        let version = interpret_version(version)?;
        let name = ServerName::try_from(server_name).map_err(|_| ConnectError::InvalidServerName(server_name.into()))?.to_owned();
        let conn = rustls::quic::ClientConnection::new(self.inner.clone(), version, name, to_vec(params))
            .map_err(|_| ConnectError::InvalidServerName(server_name.into()))?;
        Ok(Box::new(TlsSession { version, got_handshake_data: false, next_secrets: None, inner: Connection::Client(conn), suite: self.initial }))
    }
}

pub struct QuicServerConfig {
    inner: Arc<rustls::ServerConfig>,
    initial: Suite,
}

impl QuicServerConfig {
    /// Wraps a rustls server config. Refuses 0-RTT, and a config rustls can't run QUIC with.
    pub fn new(inner: rustls::ServerConfig) -> Result<Self, ConfigError> {
        if inner.max_early_data_size != 0 {
            return Err(ConfigError("0-RTT must stay off (transport.md 2.1)".into()));
        }
        let inner = Arc::new(inner);
        rustls::quic::ServerConnection::new(inner.clone(), Version::V1, Vec::new()).map_err(|e| ConfigError(e.to_string()))?;
        Ok(Self { inner, initial: initial_suite() })
    }
}

impl crypto::ServerConfig for QuicServerConfig {
    fn start_session(self: Arc<Self>, version: u32, params: &TransportParameters) -> Box<dyn crypto::Session> {
        // quinn-proto calls this only for a version `initial_keys` accepted, and `new` checked the config.
        let version = interpret_version(version).unwrap_or(Version::V1);
        match rustls::quic::ServerConnection::new(self.inner.clone(), version, to_vec(params)) {
            Ok(conn) => Box::new(TlsSession { version, got_handshake_data: false, next_secrets: None, inner: Connection::Server(conn), suite: self.initial }),
            Err(_) => unreachable!("QuicServerConfig::new checked that rustls accepts this config"),
        }
    }

    fn initial_keys(&self, version: u32, dst_cid: &ConnectionId) -> Result<Keys, UnsupportedVersion> {
        let version = interpret_version(version)?;
        Ok(initial_keys(version, *dst_cid, Side::Server, &self.initial))
    }

    fn retry_tag(&self, version: u32, orig_dst_cid: &ConnectionId, packet: &[u8]) -> [u8; 16] {
        let mut pseudo = Vec::with_capacity(packet.len() + orig_dst_cid.len() + 1);
        pseudo.push(orig_dst_cid.len() as u8);
        pseudo.extend_from_slice(orig_dst_cid);
        pseudo.extend_from_slice(packet);
        interpret_version(version).ok().and_then(|v| retry_tag_for(v, &pseudo)).unwrap_or([0; 16])
    }
}

/// HMAC-SHA256 for stateless reset tokens (as quinn's ring `hmac::Key`), keyed from `rng`.
pub struct ResetKey(Hmac<Sha256>);

impl ResetKey {
    pub fn new(rng: &'static dyn Random) -> Result<Self, rustls::Error> {
        let mut k = Zeroizing::new([0u8; 64]);
        crate::random::fill(rng.as_secure_random(), k.as_mut())?;
        <Hmac<Sha256> as hmac::KeyInit>::new_from_slice(k.as_ref()).map(Self).map_err(|_| rustls::Error::General("hmac key".into()))
    }
}

impl crypto::HmacKey for ResetKey {
    fn sign(&self, data: &[u8], out: &mut [u8]) {
        let mut m = self.0.clone();
        m.update(data);
        let tag = m.finalize().into_bytes();
        let n = out.len().min(tag.len());
        out[..n].copy_from_slice(&tag[..n]);
    }

    fn signature_len(&self) -> usize {
        32
    }

    fn verify(&self, data: &[u8], signature: &[u8]) -> Result<(), CryptoError> {
        let mut m = self.0.clone();
        m.update(data);
        m.verify_slice(signature).map_err(|_| CryptoError)
    }
}

/// The address-validation token key: an HKDF-SHA256 PRK from a random master key, expanded with each
/// token's random bytes into a one-use AES-256-GCM key, sealed with a zero nonce (as quinn's ring
/// `hkdf::Prk`; @chunks-reviewer's reading of the M0 port).
pub struct TokenKey(hkdf::Hkdf<Sha256>);

impl TokenKey {
    pub fn new(rng: &'static dyn Random) -> Result<Self, rustls::Error> {
        let mut k = Zeroizing::new([0u8; 64]);
        crate::random::fill(rng.as_secure_random(), k.as_mut())?;
        Ok(Self(hkdf::Hkdf::<Sha256>::new(None, k.as_ref())))
    }
}

impl crypto::HandshakeTokenKey for TokenKey {
    fn aead_from_hkdf(&self, random_bytes: &[u8]) -> Box<dyn crypto::AeadKey> {
        let mut key = Zeroizing::new([0u8; 32]);
        // 32 bytes is always a valid HKDF-SHA256 output length.
        let _ = self.0.expand(random_bytes, key.as_mut());
        match Aes256Gcm::new_from_slice(key.as_ref()) {
            Ok(c) => Box::new(TokenAead(c)),
            Err(_) => unreachable!("a 32-byte key"),
        }
    }
}

struct TokenAead(Aes256Gcm);

impl crypto::AeadKey for TokenAead {
    fn seal(&self, data: &mut Vec<u8>, additional_data: &[u8]) -> Result<(), CryptoError> {
        let tag = self.0.encrypt_inout_detached(&[0u8; 12].into(), additional_data, data.as_mut_slice().into()).map_err(|_| CryptoError)?;
        data.extend_from_slice(&tag);
        Ok(())
    }

    fn open<'a>(&self, data: &'a mut [u8], additional_data: &[u8]) -> Result<&'a mut [u8], CryptoError> {
        let n = data.len().checked_sub(16).ok_or(CryptoError)?;
        let (body, tag) = data.split_at_mut(n);
        let tag: &aes_gcm::Tag = (&*tag).try_into().map_err(|_| CryptoError)?;
        self.0.decrypt_inout_detached(&[0u8; 12].into(), additional_data, (&mut *body).into(), tag).map_err(|_| CryptoError)?;
        Ok(body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
    }

    // RFC 9001 A.4: the Retry packet for the A.2 client's DCID; its last 16 bytes are the tag.
    #[test]
    fn rfc9001_a4_retry_tag() {
        let data = include_str!("../tests/data/rfc9001-appendix-a.txt");
        let retry = hex(data.lines().find(|l| l.starts_with("a4_retry ")).unwrap().split_once(' ').unwrap().1);
        let odcid = [0x83, 0x94, 0xc8, 0xf0, 0x3e, 0x51, 0x57, 0x08];
        let (packet, tag) = retry.split_at(retry.len() - 16);
        let mut pseudo = vec![odcid.len() as u8];
        pseudo.extend_from_slice(&odcid);
        pseudo.extend_from_slice(packet);
        assert_eq!(retry_tag_for(Version::V1, &pseudo).unwrap().to_vec(), tag);
        assert!(retry_tag_for(Version::V2, &pseudo).is_none());
    }
}
