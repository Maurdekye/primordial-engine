//! Plain TLS 1.3 over this crate's provider, in memory: the handshake with raw keys both ways and application
//! data through the AES-128-GCM record layer (`src/aead.rs`), which QUIC never uses. A wrong pinned key fails, and a
//! tampered record is refused.

use std::io::{Read, Write};
use std::sync::Arc;

use pe_net_crypto::identity::{AnyPeerKey, Identity, PinnedKey};

fn pair(pin_right: bool) -> (rustls::ClientConnection, rustls::ServerConnection) {
    let rng = pe_net_crypto::random::os();
    let sid = Identity::from_seed(&[1; 32]);
    let cid = Identity::from_seed(&[2; 32]);
    let pin = if pin_right { sid.public() } else { Identity::from_seed(&[3; 32]).public() };
    let p = pe_net_crypto::provider_with(rng);
    let server = rustls::ServerConfig::builder_with_provider(p.clone())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .unwrap()
        .with_client_cert_verifier(Arc::new(AnyPeerKey))
        .with_cert_resolver(Arc::new(rustls::server::AlwaysResolvesServerRawPublicKeys::new(
            sid.certified_key(&p).unwrap(),
        )));
    let client = rustls::ClientConfig::builder_with_provider(p.clone())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .unwrap()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(PinnedKey::new(&pin).unwrap()))
        .with_client_cert_resolver(Arc::new(rustls::client::AlwaysResolvesClientRawPublicKeys::new(
            cid.certified_key(&p).unwrap(),
        )));
    let c = rustls::ClientConnection::new(Arc::new(client), "pe".try_into().unwrap()).unwrap();
    let s = rustls::ServerConnection::new(Arc::new(server)).unwrap();
    (c, s)
}

/// Moves TLS bytes from one side to the other; `$tamper` may change them. Evaluates to the receiver's result.
macro_rules! transfer {
    ($from:expr, $to:expr, $tamper:expr) => {{
        let mut buf = Vec::new();
        while $from.wants_write() {
            $from.write_tls(&mut buf).unwrap();
        }
        if buf.is_empty() {
            Ok(())
        } else {
            ($tamper)(&mut buf);
            let mut rd = &buf[..];
            while !rd.is_empty() {
                $to.read_tls(&mut rd).unwrap();
            }
            $to.process_new_packets().map(|_| ())
        }
    }};
}

fn none(_: &mut Vec<u8>) {}

fn handshake(c: &mut rustls::ClientConnection, s: &mut rustls::ServerConnection) -> Result<(), rustls::Error> {
    for _ in 0..10 {
        transfer!(c, s, none)?;
        transfer!(s, c, none)?;
        if !c.is_handshaking() && !s.is_handshaking() {
            return Ok(());
        }
    }
    Err(rustls::Error::General("handshake did not finish".into()))
}

#[test]
fn records_round_trip_with_raw_keys() {
    let (mut c, mut s) = pair(true);
    handshake(&mut c, &mut s).unwrap();
    assert_eq!(c.negotiated_cipher_suite().unwrap().suite(), rustls::CipherSuite::TLS13_AES_128_GCM_SHA256);
    c.writer().write_all(b"client to server").unwrap();
    transfer!(c, s, none).unwrap();
    let mut got = vec![0u8; 16];
    s.reader().read_exact(&mut got).unwrap();
    assert_eq!(got, b"client to server");
    s.writer().write_all(b"server to client, a little longer").unwrap();
    transfer!(s, c, none).unwrap();
    let mut got = vec![0u8; 33];
    c.reader().read_exact(&mut got).unwrap();
    assert_eq!(got, b"server to client, a little longer");
}

#[test]
fn wrong_pinned_key_fails() {
    let (mut c, mut s) = pair(false);
    assert!(handshake(&mut c, &mut s).is_err());
}

#[test]
fn a_tampered_record_is_refused() {
    let (mut c, mut s) = pair(true);
    handshake(&mut c, &mut s).unwrap();
    c.writer().write_all(b"do not change me").unwrap();
    let err = transfer!(c, s, |b: &mut Vec<u8>| {
        let n = b.len();
        b[n - 3] ^= 0x01;
    });
    assert!(err.is_err(), "a flipped bit in a record must fail its AEAD check");
}
