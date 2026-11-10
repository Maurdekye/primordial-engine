//! pe-net's security code (docs/spec/transport.md, section 2).
//!
//! - [`provider`]: a small rustls `CryptoProvider` over stable RustCrypto crates. One TLS 1.3 suite
//!   (TLS_AES_128_GCM_SHA256), one key exchange (X25519), one signature scheme (Ed25519, verified with
//!   `verify_strict`). Nothing else: no TLS 1.2, no RSA, no ECDSA.
//! - [`quic`]: the QUIC packet and header protection keys for that suite (RFC 9001 5).
//! - [`quinn`]: quinn-proto's crypto traits over `rustls::quic`, without ring or aws-lc-rs. A port of
//!   quinn-proto's own rustls adapter (MIT/Apache-2.0), with RustCrypto in place of ring.
//! - [`identity`]: Ed25519 keys as TLS raw public keys (RFC 7250), invites and reply codes, and the
//!   verifiers that pin a key.
//!
//! Every entry point that takes a peer's bytes returns an error rather than panicking (transport.md 2.5).
//! Its reviewer is @design-reviewer, not the author (coordinator ruling mc64f7a24509e).

#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod aead;
mod hash;
pub mod identity;
mod kx;
pub mod quic;
pub mod quinn;
pub mod random;
mod sign;

use std::sync::Arc;

use rustls::crypto::CryptoProvider;
use rustls::SupportedCipherSuite;

pub use random::Random;

/// The provider with the operating system's randomness.
pub fn provider() -> Arc<CryptoProvider> {
    provider_with(random::os())
}

/// The provider with a given randomness source (the deterministic harness passes a seeded one; see
/// [`random::seeded`], behind the `insecure-deterministic-rng` feature).
pub fn provider_with(rng: &'static dyn Random) -> Arc<CryptoProvider> {
    Arc::new(CryptoProvider {
        cipher_suites: vec![SupportedCipherSuite::Tls13(quic::tls13_aes_128_gcm_sha256())],
        kx_groups: vec![kx::x25519(rng)],
        signature_verification_algorithms: sign::ALGORITHMS,
        secure_random: rng.as_secure_random(),
        key_provider: &sign::KEY_PROVIDER,
    })
}
