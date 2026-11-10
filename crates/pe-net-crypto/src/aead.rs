//! The TLS 1.3 record layer's AES-128-GCM (RFC 8446 5.2), as rustls's own ring-based encrypter does it.
//! QUIC carries the handshake in CRYPTO frames and never uses TLS records, but the suite must still be a
//! whole TLS 1.3 suite; the loopback test in `tests/tls.rs` exercises this path.

use aes_gcm::aead::AeadInOut;
use aes_gcm::{Aes128Gcm, KeyInit};
use rustls::crypto::cipher::{
    make_tls13_aad, AeadKey, InboundOpaqueMessage, InboundPlainMessage, Iv, MessageDecrypter, MessageEncrypter, Nonce,
    OutboundOpaqueMessage, OutboundPlainMessage, PrefixedPayload, Tls13AeadAlgorithm, UnsupportedOperationError,
};
use rustls::{ConnectionTrafficSecrets, ContentType, Error, ProtocolVersion};

const TAG: usize = 16;

pub(crate) struct Tls13Aes128Gcm;

fn cipher(key: &AeadKey) -> Aes128Gcm {
    // rustls derives the key at the length key_len() asks for, so this is 16 bytes.
    match Aes128Gcm::new_from_slice(key.as_ref()) {
        Ok(c) => c,
        Err(_) => unreachable!("rustls supplies a 16-byte key"),
    }
}

impl Tls13AeadAlgorithm for Tls13Aes128Gcm {
    fn encrypter(&self, key: AeadKey, iv: Iv) -> Box<dyn MessageEncrypter> {
        Box::new(Records { aead: cipher(&key), iv })
    }

    fn decrypter(&self, key: AeadKey, iv: Iv) -> Box<dyn MessageDecrypter> {
        Box::new(Records { aead: cipher(&key), iv })
    }

    fn key_len(&self) -> usize {
        16
    }

    fn extract_keys(&self, key: AeadKey, iv: Iv) -> Result<ConnectionTrafficSecrets, UnsupportedOperationError> {
        Ok(ConnectionTrafficSecrets::Aes128Gcm { key, iv })
    }
}

struct Records {
    aead: Aes128Gcm,
    iv: Iv,
}

impl MessageEncrypter for Records {
    fn encrypt(&mut self, msg: OutboundPlainMessage<'_>, seq: u64) -> Result<OutboundOpaqueMessage, Error> {
        let total_len = self.encrypted_payload_len(msg.payload.len());
        let mut payload = PrefixedPayload::with_capacity(total_len);
        payload.extend_from_chunks(&msg.payload);
        payload.extend_from_slice(&msg.typ.to_array());
        let nonce = Nonce::new(&self.iv, seq).0;
        let aad = make_tls13_aad(total_len);
        let tag = self
            .aead
            .encrypt_inout_detached(&nonce.into(), &aad, payload.as_mut().into())
            .map_err(|_| Error::EncryptError)?;
        payload.extend_from_slice(&tag);
        Ok(OutboundOpaqueMessage::new(ContentType::ApplicationData, ProtocolVersion::TLSv1_2, payload))
    }

    fn encrypted_payload_len(&self, payload_len: usize) -> usize {
        payload_len + 1 + TAG
    }
}

impl MessageDecrypter for Records {
    fn decrypt<'a>(&mut self, mut msg: InboundOpaqueMessage<'a>, seq: u64) -> Result<InboundPlainMessage<'a>, Error> {
        let payload = &mut msg.payload;
        let len = payload.len();
        if len < TAG {
            return Err(Error::DecryptError);
        }
        let nonce = Nonce::new(&self.iv, seq).0;
        let aad = make_tls13_aad(len);
        let (body, tag) = payload.split_at_mut(len - TAG);
        let tag: &aes_gcm::Tag = (&*tag).try_into().map_err(|_| Error::DecryptError)?;
        self.aead
            .decrypt_inout_detached(&nonce.into(), &aad, body.into(), tag)
            .map_err(|_| Error::DecryptError)?;
        payload.truncate(len - TAG);
        msg.into_tls13_unpadded_message()
    }
}
