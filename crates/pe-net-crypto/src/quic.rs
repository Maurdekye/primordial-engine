//! The suite TLS_AES_128_GCM_SHA256, with its QUIC packet and header protection keys (RFC 9001 5).
//! QUIC's Initial packets always use this suite, so it is the minimum, and the only one here.

use aes::cipher::BlockCipherEncrypt;
use aes_gcm::aead::AeadInOut;
use aes_gcm::{Aes128Gcm, KeyInit};
use rustls::crypto::CipherSuiteCommon;
use rustls::crypto::cipher::{AeadKey, Iv, Nonce};
use rustls::crypto::tls13::HkdfUsingHmac;
use rustls::{CipherSuite, Error, Tls13CipherSuite, quic};

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
        let tag =
            self.aead.encrypt_inout_detached(&nonce.into(), header, payload.into()).map_err(|_| Error::EncryptError)?;
        Ok(quic::Tag::from(tag.as_slice()))
    }

    fn decrypt_in_place<'a>(
        &self,
        packet_number: u64,
        header: &[u8],
        payload: &'a mut [u8],
    ) -> Result<&'a [u8], Error> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use rustls::Side;
    use rustls::quic::{HeaderProtectionKey as _, PacketKey as _, Version};

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
    }

    fn vector(name: &str) -> Vec<u8> {
        let data = include_str!("../tests/data/rfc9001-appendix-a.txt");
        let line = data.lines().find(|l| l.starts_with(&format!("{name} "))).unwrap();
        hex(line.split_once(' ').unwrap().1)
    }

    const DCID: [u8; 8] = [0x83, 0x94, 0xc8, 0xf0, 0x3e, 0x51, 0x57, 0x08];

    /// Protect `payload` under `header` (packet number `pn`, its length taken from the header) and
    /// compare the whole packet with RFC 9001's.
    fn protect(side: Side, header: &[u8], pn: u64, pn_len: usize, payload: &[u8]) -> Vec<u8> {
        let keys = tls13_aes_128_gcm_sha256().quic_suite().unwrap().keys(&DCID, side, Version::V1);
        let mut body = payload.to_vec();
        let tag = keys.local.packet.encrypt_in_place(pn, header, &mut body).unwrap();
        let mut packet = header.to_vec();
        packet.extend_from_slice(&body);
        packet.extend_from_slice(tag.as_ref());
        let pn_offset = header.len() - pn_len;
        let sample: Vec<u8> = packet[pn_offset + 4..pn_offset + 20].to_vec();
        let (first, rest) = packet.split_at_mut(1);
        keys.local
            .header
            .encrypt_in_place(&sample, &mut first[0], &mut rest[pn_offset - 1..pn_offset - 1 + pn_len])
            .unwrap();
        packet
    }

    #[test]
    fn rfc9001_a3_server_initial() {
        let packet = protect(Side::Server, &vector("a3_header"), 1, 2, &vector("a3_payload"));
        assert_eq!(packet, vector("a3_protected"));
    }

    #[test]
    fn rfc9001_a2_client_initial() {
        let mut payload = vector("a2_crypto_frame");
        payload.resize(1162, 0); // PADDING frames
        let packet = protect(Side::Client, &vector("a2_header"), 2, 4, &payload);
        assert_eq!(packet, vector("a2_protected"));
    }

    #[test]
    fn rfc9001_a3_unprotect() {
        // The server's packet, as the client opens it: header protection off, then the AEAD.
        let keys = tls13_aes_128_gcm_sha256().quic_suite().unwrap().keys(&DCID, Side::Client, Version::V1);
        let mut packet = vector("a3_protected");
        let pn_offset = 18;
        let sample: Vec<u8> = packet[pn_offset + 4..pn_offset + 20].to_vec();
        {
            let (first, rest) = packet.split_at_mut(1);
            keys.remote
                .header
                .decrypt_in_place(&sample, &mut first[0], &mut rest[pn_offset - 1..pn_offset + 3])
                .unwrap();
        }
        let header = vector("a3_header");
        assert_eq!(&packet[..20], &header[..]);
        let (h, body) = packet.split_at_mut(20);
        let plain = keys.remote.packet.decrypt_in_place(1, h, body).unwrap();
        assert_eq!(plain, &vector("a3_payload")[..]);
        // Tampered: one flipped bit anywhere in the protected payload fails.
        let mut t = vector("a3_protected");
        t[60] ^= 0x01;
        let (_masked_header, body) = t.split_at_mut(20);
        assert!(keys.remote.packet.decrypt_in_place(1, &header, body).is_err());
    }

    #[test]
    fn wycheproof_aes_gcm_through_the_packet_key() {
        let data: serde_json::Value =
            serde_json::from_str(include_str!("../tests/data/wycheproof-aes-gcm-128-iv96.json")).unwrap();
        let tests = data["tests"].as_array().unwrap();
        assert!(tests.len() >= 60);
        let (mut valid, mut invalid) = (0, 0);
        for t in tests {
            let s = |k: &str| hex(t[k].as_str().unwrap());
            let key: [u8; 16] = s("key").try_into().unwrap();
            let iv: [u8; 12] = s("iv").try_into().unwrap();
            // Packet number 0, so the nonce is the IV itself (RFC 9001 5.3).
            let pk = PacketKey { aead: Aes128Gcm::new_from_slice(&key).unwrap(), iv: Iv::from(iv) };
            let mut ct = s("ct");
            ct.extend_from_slice(&s("tag"));
            let opened = pk.decrypt_in_place(0, &s("aad"), &mut ct).map(|p| p.to_vec());
            match t["result"].as_str().unwrap() {
                "valid" => {
                    assert_eq!(opened.unwrap(), s("msg"), "tcId {}", t["tcId"]);
                    let mut m = s("msg");
                    let tag = pk.encrypt_in_place(0, &s("aad"), &mut m).unwrap();
                    assert_eq!(m, s("ct"), "tcId {}", t["tcId"]);
                    assert_eq!(tag.as_ref(), &s("tag")[..], "tcId {}", t["tcId"]);
                    valid += 1;
                }
                _ => {
                    assert!(opened.is_err(), "tcId {} must fail", t["tcId"]);
                    invalid += 1;
                }
            }
        }
        assert!(valid > 0 && invalid > 0);
    }

    #[test]
    fn short_or_odd_inputs_are_errors() {
        let pk = PacketKey { aead: Aes128Gcm::new_from_slice(&[0; 16]).unwrap(), iv: Iv::from([0u8; 12]) };
        assert!(pk.decrypt_in_place(0, b"", &mut [0u8; 15]).is_err());
        let hk = HeaderKey(aes::Aes128::new_from_slice(&[0; 16]).unwrap());
        let mut first = 0xc3u8;
        assert!(hk.encrypt_in_place(&[0; 15], &mut first, &mut [0; 4]).is_err());
        assert!(hk.encrypt_in_place(&[0; 16], &mut first, &mut [0; 5]).is_err());
    }
}
