//! SHA-256 and HMAC-SHA256 for rustls (the suite's hash and its HKDF).

use hmac::{KeyInit, Mac};
use rustls::crypto::{hash, hmac as rhmac};
use sha2::{Digest, Sha256};

pub(crate) struct Sha256Hash;

impl hash::Hash for Sha256Hash {
    fn start(&self) -> Box<dyn hash::Context> {
        Box::new(Sha256Context(Sha256::new()))
    }

    fn hash(&self, data: &[u8]) -> hash::Output {
        hash::Output::new(&Sha256::digest(data)[..])
    }

    fn output_len(&self) -> usize {
        32
    }

    fn algorithm(&self) -> hash::HashAlgorithm {
        hash::HashAlgorithm::SHA256
    }
}

struct Sha256Context(Sha256);

impl hash::Context for Sha256Context {
    fn fork_finish(&self) -> hash::Output {
        hash::Output::new(&self.0.clone().finalize()[..])
    }

    fn fork(&self) -> Box<dyn hash::Context> {
        Box::new(Sha256Context(self.0.clone()))
    }

    fn finish(self: Box<Self>) -> hash::Output {
        hash::Output::new(&self.0.finalize()[..])
    }

    fn update(&mut self, data: &[u8]) {
        self.0.update(data);
    }
}

pub(crate) struct HmacSha256;

impl rhmac::Hmac for HmacSha256 {
    fn with_key(&self, key: &[u8]) -> Box<dyn rhmac::Key> {
        // HMAC takes a key of any length, so this cannot fail.
        match hmac::Hmac::<Sha256>::new_from_slice(key) {
            Ok(k) => Box::new(HmacKey(k)),
            #[allow(clippy::unreachable, reason = "HMAC accepts a key of any length")]
            Err(_) => unreachable!("HMAC accepts any key length"),
        }
    }

    fn hash_output_len(&self) -> usize {
        32
    }
}

struct HmacKey(hmac::Hmac<Sha256>);

impl rhmac::Key for HmacKey {
    fn sign_concat(&self, first: &[u8], middle: &[&[u8]], last: &[u8]) -> rhmac::Tag {
        let mut m = self.0.clone();
        m.update(first);
        for x in middle {
            m.update(x);
        }
        m.update(last);
        rhmac::Tag::new(&m.finalize().into_bytes()[..])
    }

    fn tag_len(&self) -> usize {
        32
    }
}

pub(crate) static SHA256: Sha256Hash = Sha256Hash;
pub(crate) static HMAC_SHA256: HmacSha256 = HmacSha256;
