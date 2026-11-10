//! The one source of randomness for everything in this crate: TLS (via rustls's `SecureRandom`), X25519
//! key shares, and the stateless-reset and token keys. The operating system's by default; a seeded one
//! for the deterministic harness (transport.md 8), behind a feature that no shipped build enables.

use rustls::crypto::{GetRandomFailed, SecureRandom};

/// A randomness source usable both by rustls and by this crate's own keys.
pub trait Random: SecureRandom + Send + Sync + 'static {
    fn as_secure_random(&'static self) -> &'static dyn SecureRandom;
}

/// The operating system's randomness (getrandom).
#[derive(Debug)]
pub struct OsRandom;

impl SecureRandom for OsRandom {
    fn fill(&self, buf: &mut [u8]) -> Result<(), GetRandomFailed> {
        getrandom::fill(buf).map_err(|_| GetRandomFailed)
    }
}

impl Random for OsRandom {
    fn as_secure_random(&'static self) -> &'static dyn SecureRandom {
        self
    }
}

static OS: OsRandom = OsRandom;

pub fn os() -> &'static dyn Random {
    &OS
}

/// Fill `buf` from `rng`, or fail with rustls's error.
pub(crate) fn fill(rng: &dyn SecureRandom, buf: &mut [u8]) -> Result<(), rustls::Error> {
    rng.fill(buf).map_err(|_| rustls::Error::FailedToGetRandomBytes)
}

#[cfg(feature = "insecure-deterministic-rng")]
mod seeded_impl {
    use super::*;
    use chacha20::cipher::{KeyIvInit, StreamCipher};
    use std::sync::Mutex;

    /// ChaCha20 keystream from a 32-byte seed. Deterministic, so never secure: for the harness only.
    pub struct Seeded(Mutex<chacha20::ChaCha20>);

    impl std::fmt::Debug for Seeded {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("Seeded(insecure)")
        }
    }

    impl SecureRandom for Seeded {
        fn fill(&self, buf: &mut [u8]) -> Result<(), GetRandomFailed> {
            buf.fill(0);
            let mut c = self.0.lock().map_err(|_| GetRandomFailed)?;
            c.apply_keystream(buf);
            Ok(())
        }
    }

    impl Random for Seeded {
        fn as_secure_random(&'static self) -> &'static dyn SecureRandom {
            self
        }
    }

    /// A seeded source, leaked so it can live as long as the providers that use it (tests only).
    pub fn seeded(seed: [u8; 32]) -> &'static dyn Random {
        let c = chacha20::ChaCha20::new(&seed.into(), &[0u8; 12].into());
        Box::leak(Box::new(Seeded(Mutex::new(c))))
    }
}

#[cfg(feature = "insecure-deterministic-rng")]
pub use seeded_impl::seeded;
