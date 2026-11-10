//! Ed25519 for TLS: signing (rustls's `SigningKey`, loaded from PKCS#8) and verification with
//! `verify_strict`, which refuses small-order keys and non-canonical signatures (transport.md 2.2).

use std::sync::Arc;

use ed25519_dalek::pkcs8::DecodePrivateKey;
use ed25519_dalek::Signer as _;
use rustls::crypto::{KeyProvider, WebPkiSupportedAlgorithms};
use rustls::pki_types::{alg_id, AlgorithmIdentifier, InvalidSignature, PrivateKeyDer, SignatureVerificationAlgorithm};
use rustls::sign::{Signer, SigningKey};
use rustls::{Error, SignatureAlgorithm, SignatureScheme};

#[derive(Debug)]
pub(crate) struct Ed25519Key(Arc<ed25519_dalek::SigningKey>); // zeroized on drop (the crate's `zeroize` feature)

impl SigningKey for Ed25519Key {
    fn choose_scheme(&self, offered: &[SignatureScheme]) -> Option<Box<dyn Signer>> {
        offered.contains(&SignatureScheme::ED25519).then(|| Box::new(Ed25519Signer(self.0.clone())) as Box<dyn Signer>)
    }

    fn algorithm(&self) -> SignatureAlgorithm {
        SignatureAlgorithm::ED25519
    }
}

#[derive(Debug)]
struct Ed25519Signer(Arc<ed25519_dalek::SigningKey>);

impl Signer for Ed25519Signer {
    fn sign(&self, message: &[u8]) -> Result<Vec<u8>, Error> {
        Ok(self.0.sign(message).to_bytes().to_vec())
    }

    fn scheme(&self) -> SignatureScheme {
        SignatureScheme::ED25519
    }
}

#[derive(Debug)]
pub(crate) struct Keys;

impl KeyProvider for Keys {
    fn load_private_key(&self, key: PrivateKeyDer<'static>) -> Result<Arc<dyn SigningKey>, Error> {
        match key {
            PrivateKeyDer::Pkcs8(der) => ed25519_dalek::SigningKey::from_pkcs8_der(der.secret_pkcs8_der())
                .map(|k| Arc::new(Ed25519Key(Arc::new(k))) as Arc<dyn SigningKey>)
                .map_err(|_| Error::General("not an Ed25519 PKCS#8 key".into())),
            _ => Err(Error::General("only Ed25519 PKCS#8 keys are supported".into())),
        }
    }
}

pub(crate) static KEY_PROVIDER: Keys = Keys;

#[derive(Debug)]
struct Ed25519Verify;

impl SignatureVerificationAlgorithm for Ed25519Verify {
    fn public_key_alg_id(&self) -> AlgorithmIdentifier {
        alg_id::ED25519
    }

    fn signature_alg_id(&self) -> AlgorithmIdentifier {
        alg_id::ED25519
    }

    fn verify_signature(&self, public_key: &[u8], message: &[u8], signature: &[u8]) -> Result<(), InvalidSignature> {
        verify_strict(public_key, message, signature).map_err(|_| InvalidSignature)
    }
}

/// Ed25519 `verify_strict` over a raw 32-byte key: refuses small-order keys, and non-canonical or
/// malleable signatures.
pub fn verify_strict(public_key: &[u8], message: &[u8], signature: &[u8]) -> Result<(), ()> {
    let key: &[u8; 32] = public_key.try_into().map_err(|_| ())?;
    let key = ed25519_dalek::VerifyingKey::from_bytes(key).map_err(|_| ())?;
    let sig = ed25519_dalek::Signature::from_slice(signature).map_err(|_| ())?;
    key.verify_strict(message, &sig).map_err(|_| ())
}

static ED25519: &dyn SignatureVerificationAlgorithm = &Ed25519Verify;

pub(crate) static ALGORITHMS: WebPkiSupportedAlgorithms = WebPkiSupportedAlgorithms {
    all: &[ED25519],
    mapping: &[(SignatureScheme::ED25519, &[ED25519])],
};

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
    }

    // RFC 8032 7.1, TEST 1 (empty message) and TEST 2 (one byte).
    #[test]
    fn rfc8032_7_1() {
        let cases = [
            ("9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60", "d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a", "",
             "e5564300c360ac729086e2cc806e828a84877f1eb8e5d974d873e065224901555fb8821590a33bacc61e39701cf9b46bd25bf5f0595bbe24655141438e7a100b"),
            ("4ccd089b28ff96da9db6c346ec114e0f5b8a319f35aba624da8cf6ed4fb8a6fb", "3d4017c3e843895a92b70aa74d1b7ebc9c982ccf2ec4968cc0cd55f12af4660c", "72",
             "92a009a9f0d4cab8720e820b5f642540a2b27b5416503f8fb3762223ebdb69da085ac1e43e15996e458f3613d0f11d8c387b2eaeb4302aeeb00d291612bb0c00"),
        ];
        for (sk, pk, msg, sig) in cases {
            let k = ed25519_dalek::SigningKey::from_bytes(&hex(sk).try_into().unwrap());
            assert_eq!(k.verifying_key().to_bytes().to_vec(), hex(pk));
            let s = Ed25519Signer(Arc::new(k)).sign(&hex(msg)).unwrap();
            assert_eq!(s, hex(sig));
            assert!(verify_strict(&hex(pk), &hex(msg), &s).is_ok());
            let mut bad = s.clone();
            bad[0] ^= 1;
            assert!(verify_strict(&hex(pk), &hex(msg), &bad).is_err());
            assert!(verify_strict(&hex(pk), b"other", &s).is_err());
        }
    }

    #[test]
    fn refuses_small_order_key() {
        // The identity point (y = 1) has order 1. A signature (R = identity, s = 0) "verifies" under the
        // permissive check for any message; verify_strict must refuse it.
        let mut key = [0u8; 32];
        key[0] = 1;
        let mut sig = [0u8; 64];
        sig[0] = 1;
        assert!(verify_strict(&key, b"anything", &sig).is_err());
    }
}
