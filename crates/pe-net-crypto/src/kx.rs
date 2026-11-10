//! X25519 key exchange (RFC 7748). The private scalar comes from the crate's one randomness source, so
//! the harness can seed it. A shared secret that isn't contributory (the peer sent a low-order point) is
//! refused, as RFC 8446 7.4.2 allows and RFC 7748 6.1 recommends.

use rustls::crypto::{ActiveKeyExchange, SharedSecret, SupportedKxGroup};
use rustls::{Error, NamedGroup, PeerMisbehaved};

use crate::random::Random;

#[derive(Debug)]
pub(crate) struct X25519 {
    rng: &'static dyn Random,
}

/// The X25519 group drawing on `rng`. Leaked: a provider lives for the program.
pub(crate) fn x25519(rng: &'static dyn Random) -> &'static dyn SupportedKxGroup {
    Box::leak(Box::new(X25519 { rng }))
}

impl SupportedKxGroup for X25519 {
    fn start(&self) -> Result<Box<dyn ActiveKeyExchange>, Error> {
        let mut seed = zeroize::Zeroizing::new([0u8; 32]);
        crate::random::fill(self.rng.as_secure_random(), seed.as_mut())?;
        let secret = x25519_dalek::StaticSecret::from(*seed);
        let public = x25519_dalek::PublicKey::from(&secret);
        Ok(Box::new(X25519Exchange { secret, public }))
    }

    fn name(&self) -> NamedGroup {
        NamedGroup::X25519
    }
}

struct X25519Exchange {
    secret: x25519_dalek::StaticSecret, // zeroized on drop (the crate's `zeroize` feature)
    public: x25519_dalek::PublicKey,
}

impl ActiveKeyExchange for X25519Exchange {
    fn complete(self: Box<Self>, peer: &[u8]) -> Result<SharedSecret, Error> {
        let peer: [u8; 32] = peer.try_into().map_err(|_| Error::from(PeerMisbehaved::InvalidKeyShare))?;
        let shared = self.secret.diffie_hellman(&x25519_dalek::PublicKey::from(peer));
        if !shared.was_contributory() {
            return Err(Error::from(PeerMisbehaved::InvalidKeyShare));
        }
        Ok(SharedSecret::from(&shared.as_bytes()[..]))
    }

    fn pub_key(&self) -> &[u8] {
        self.public.as_bytes()
    }

    fn group(&self) -> NamedGroup {
        NamedGroup::X25519
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
    }

    // RFC 7748 6.1: Alice's and Bob's keys and their shared secret.
    #[test]
    fn rfc7748_6_1() {
        let a: [u8; 32] = hex("77076d0a7318a57d3c16c17251b26645df4c2f87ebc0992ab177fba51db92c2a").try_into().unwrap();
        let b: [u8; 32] = hex("5dab087e624a8a4b79e17f8b83800ee66f3bb1292618b6fd1c2f8b27ff88e0eb").try_into().unwrap();
        let sa = x25519_dalek::StaticSecret::from(a);
        let sb = x25519_dalek::StaticSecret::from(b);
        let pa = x25519_dalek::PublicKey::from(&sa);
        let pb = x25519_dalek::PublicKey::from(&sb);
        assert_eq!(pa.as_bytes().to_vec(), hex("8520f0098930a754748b7ddcb43ef75a0dbf3a0d26381af4eba4a98eaa9b4e6a"));
        assert_eq!(pb.as_bytes().to_vec(), hex("de9edb7d7b7dc1b4d35b61c2ece435373f8343c85b78674dadfc7e146f882b4f"));
        let ea = Box::new(X25519Exchange { secret: sa, public: pa });
        let k = ea.complete(pb.as_bytes()).unwrap();
        assert_eq!(k.secret_bytes().to_vec(), hex("4a5d9d5ba4ce2de1728e3bf480350f25e07e21c947d19e3376f09b3c1e161742"));
    }

    #[test]
    fn refuses_low_order_and_bad_length() {
        let ex = Box::new(X25519Exchange { secret: x25519_dalek::StaticSecret::from([7u8; 32]), public: x25519_dalek::PublicKey::from([9u8; 32]) });
        assert!(ex.complete(&[0u8; 32]).is_err()); // the all-zero point gives a non-contributory secret
        let ex = Box::new(X25519Exchange { secret: x25519_dalek::StaticSecret::from([7u8; 32]), public: x25519_dalek::PublicKey::from([9u8; 32]) });
        assert!(ex.complete(&[1u8; 31]).is_err());
    }
}
