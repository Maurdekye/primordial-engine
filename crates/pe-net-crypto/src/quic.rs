//! The suite TLS_AES_128_GCM_SHA256, with its QUIC packet and header protection keys (RFC 9001 5).
//! QUIC's Initial packets always use this suite, so it is the minimum, and the only one here.

use aes::cipher::BlockCipherEncrypt;
use aes_gcm::aead::AeadInOut;
use aes_gcm::{Aes128Gcm, KeyInit};
use rustls::crypto::cipher::{AeadKey, Iv, Nonce};
use rustls::crypto::tls13::HkdfUsingHmac;
use rustls::crypto::CipherSuiteCommon;
use rustls::{quic, CipherSuite, Error, Tls13CipherSuite};

use crate::aead::Tls13Aes128Gcm;
use crate::hash::{HMAC_SHA256, SHA256};

const TAG: usize = 16;

static SUITE: Tls13CipherSuite = Tls13CipherSuite {
    common: CipherSuiteCommon {
        suite: CipherSuite::TLS13_AES_128_GCM_SHA256,
        hash_provider: &SHA256,
        // RFC 8446 5.5 leaves AES-GCM's record limit to the AEAD; rustls uses 2^24.5 records, rounded down.
        confidentiality_limit: 1 << 24,
    },
    hkdf_provider: &HkdfUsingHmac(&HMAC_SHA256),
    aead_alg: &Tls13Aes128Gcm,
    quic: Some(&Aes128GcmQuic),
};

/// The suite, for the provider and for the QUIC initial keys.
pub fn tls13_aes_128_gcm_sha256() -> &'static Tls13CipherSuite {
    &SUITE
}

/// QUIC keys for AES-128-GCM.
pub struct Aes128GcmQuic;

fn aead(key: &AeadKey) -> Aes128Gcm {
    match Aes128Gcm::new_from_slice(key.as_ref()) {
        Ok(c) => c,
        Err(_) => unreachable!("rustls supplies aead_key_len() = 16 bytes"),
    }
}

impl quic::Algorithm for Aes128GcmQuic {
    fn packet_key(&self, key: AeadKey, iv: Iv) -> Box<dyn quic::PacketKey> {
        Box::new(PacketKey { aead: aead(&key), iv })
    }

    fn header_protection_key(&self, key: AeadKey) -> Box<dyn quic::HeaderProtectionKey> {
        let hp = match aes::Aes128::new_from_slice(key.as_ref()) {
            Ok(c) => c,
            Err(_) => unreachable!("rustls supplies aead_key_len() = 16 bytes"),
        };
        Box::new(HeaderKey(hp))
    }

    fn aead_key_len(&self) -> usize {
        16
    }
}

struct PacketKey {
    aead: Aes128Gcm,
    iv: Iv,
}

impl quic::PacketKey for PacketKey {
    fn encrypt_in_place(&self, packet_number: u64, header: &[u8], payload: &mut [u8]) -> Result<quic::Tag, Error> {
        let nonce = Nonce::new(&self.iv, packet_number).0;
        let tag = self
            .aead
            .encrypt_inout_detached(&nonce.into(), header, payload.into())
            .map_err(|_| Error::EncryptError)?;
        Ok(quic::Tag::from(tag.as_slice()))
    }

    fn decrypt_in_place<'a>(&self, packet_number: u64, header: &[u8], payload: &'a mut [u8]) -> Result<&'a [u8], Error> {
        let len = payload.len();
        if len < TAG {
            return Err(Error::DecryptError);
        }
        let nonce = Nonce::new(&self.iv, packet_number).0;
        let (body, tag) = payload.split_at_mut(len - TAG);
        let tag: &aes_gcm::Tag = (&*tag).try_into().map_err(|_| Error::DecryptError)?;
        self.aead
            .decrypt_inout_detached(&nonce.into(), header, (&mut *body).into(), tag)
            .map_err(|_| Error::DecryptError)?;
        Ok(body)
    }

    fn tag_len(&self) -> usize {
        TAG
    }

    // RFC 9001 6.6.
    fn confidentiality_limit(&self) -> u64 {
        1 << 23
    }

    fn integrity_limit(&self) -> u64 {
        1 << 52
    }
}

/// AES header protection: mask = AES-ECB(hp, sample), RFC 9001 5.4.3.
struct HeaderKey(aes::Aes128);

impl HeaderKey {
    // RFC 9001 5.4.1, as rustls's ring-based implementation.
    fn xor_in_place(&self, sample: &[u8], first: &mut u8, packet_number: &mut [u8], masked: bool) -> Result<(), Error> {
        let mut block: aes::Block = sample.try_into().map_err(|_| Error::General("sample of invalid length".into()))?;
        self.0.encrypt_block(&mut block);
        let first_mask = block[0];
        let pn_mask = &block[1..5];
        if packet_number.len() > pn_mask.len() {
            return Err(Error::General("packet number too long".into()));
        }
        let bits = if *first & 0x80 == 0x80 { 0x0f } else { 0x1f };
        let first_plain = if masked { *first ^ (first_mask & bits) } else { *first };
        let pn_len = (first_plain & 0x03) as usize + 1;
        *first ^= first_mask & bits;
        for (dst, m) in packet_number.iter_mut().zip(pn_mask).take(pn_len) {
            *dst ^= m;
        }
        Ok(())
    }
}

impl quic::HeaderProtectionKey for HeaderKey {
    fn encrypt_in_place(&self, sample: &[u8], first: &mut u8, packet_number: &mut [u8]) -> Result<(), Error> {
        self.xor_in_place(sample, first, packet_number, false)
    }

    fn decrypt_in_place(&self, sample: &[u8], first: &mut u8, packet_number: &mut [u8]) -> Result<(), Error> {
        self.xor_in_place(sample, first, packet_number, true)
    }

    fn sample_len(&self) -> usize {
        16
    }
}
