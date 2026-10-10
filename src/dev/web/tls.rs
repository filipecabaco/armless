//! A minimal TLS 1.3 server (RFC 8446), enough for the guest's HTTPS clients: one cipher suite
//! (TLS_AES_128_GCM_SHA256), X25519 or P-256 key shares, an ECDSA P-256 certificate minted for
//! the name the client asked for (SNI). Every current OpenSSL, Node, Python, Go, Rust and Java
//! client offers that. No resumption, no client certificates, no HelloRetryRequest.

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes128Gcm, Nonce};
use hmac::{Hmac, Mac};
use p256::ecdsa::signature::Signer;
use sha2::{Digest, Sha256};

use super::cert::{der, TlsKeys};
use super::Rng;

const CCS: u8 = 20;
const ALERT: u8 = 21;
const HANDSHAKE: u8 = 22;
const APPDATA: u8 = 23;
const CLIENT_HELLO: u8 = 1;
const SERVER_HELLO: u8 = 2;
const ENCRYPTED_EXTENSIONS: u8 = 8;
const CERTIFICATE: u8 = 11;
const CERTIFICATE_VERIFY: u8 = 15;
const FINISHED: u8 = 20;
const TLS13: u16 = 0x0304;
const AES_128_GCM_SHA256: u16 = 0x1301;
const ECDSA_P256_SHA256: u16 = 0x0403;
const X25519: u16 = 0x001d;
const SECP256R1: u16 = 0x0017;
const MAX_RECORD: usize = 16384;

fn sha256(parts: &[&[u8]]) -> [u8; 32] {
    let mut h = Sha256::new();
    for p in parts {
        h.update(p);
    }
    h.finalize().into()
}

fn hmac(key: &[u8], data: &[&[u8]]) -> [u8; 32] {
    let mut m = <Hmac<Sha256> as Mac>::new_from_slice(key).unwrap();
    for d in data {
        m.update(d);
    }
    m.finalize().into_bytes().into()
}

/// HKDF-Expand-Label; every output here fits in one SHA-256 block.
fn expand_label(secret: &[u8], label: &str, context: &[u8], length: usize) -> Vec<u8> {
    let full = format!("tls13 {label}");
    let mut info = vec![(length >> 8) as u8, length as u8, full.len() as u8];
    info.extend_from_slice(full.as_bytes());
    info.push(context.len() as u8);
    info.extend_from_slice(context);
    info.push(1);
    hmac(secret, &[&info])[..length].to_vec()
}

fn derive_secret(secret: &[u8], label: &str, transcript: &[u8]) -> Vec<u8> {
    expand_label(secret, label, &sha256(&[transcript]), 32)
}

struct Keys {
    aead: Aes128Gcm,
    iv: [u8; 12],
    seq: u64,
}

impl Keys {
    fn new(secret: &[u8]) -> Keys {
        let key = expand_label(secret, "key", &[], 16);
        let iv: [u8; 12] = expand_label(secret, "iv", &[], 12).try_into().unwrap();
        Keys {
            aead: Aes128Gcm::new_from_slice(&key).unwrap(),
            iv,
            seq: 0,
        }
    }

    fn nonce(&mut self) -> [u8; 12] {
        let mut n = self.iv;
        for (i, b) in self.seq.to_be_bytes().iter().enumerate() {
            n[4 + i] ^= b;
        }
        self.seq += 1;
        n
    }
}

fn handshake_msg(ty: u8, body: &[u8]) -> Vec<u8> {
    let n = body.len();
    let mut v = vec![ty, (n >> 16) as u8, (n >> 8) as u8, n as u8];
    v.extend_from_slice(body);
    v
}

fn be16(b: &[u8], o: usize) -> Option<u16> {
    Some(u16::from_be_bytes([*b.get(o)?, *b.get(o + 1)?]))
}

struct Hello {
    session_id: Vec<u8>,
    suites: Vec<u16>,
    versions: Vec<u16>,
    shares: Vec<(u16, Vec<u8>)>,
    host: Option<String>,
}

fn parse_client_hello(b: &[u8]) -> Option<Hello> {
    let mut p = 34; // legacy_version, random
    let sid_len = *b.get(p)? as usize;
    let session_id = b.get(p + 1..p + 1 + sid_len)?.to_vec();
    p += 1 + sid_len;
    let n = be16(b, p)? as usize;
    let suites = (0..n / 2).filter_map(|i| be16(b, p + 2 + 2 * i)).collect();
    p += 2 + n;
    p += 1 + *b.get(p)? as usize; // compression methods
    let end = p + 2 + be16(b, p)? as usize;
    p += 2;
    let mut h = Hello {
        session_id,
        suites,
        versions: Vec::new(),
        shares: Vec::new(),
        host: None,
    };
    while p + 4 <= end.min(b.len()) {
        let ty = be16(b, p)?;
        let len = be16(b, p + 2)? as usize;
        let data = b.get(p + 4..p + 4 + len)?;
        p += 4 + len;
        match ty {
            // server_name: list, type 0 (host_name)
            0 if data.len() > 5 && data[2] == 0 => {
                let n = be16(data, 3)? as usize;
                h.host = Some(String::from_utf8_lossy(data.get(5..5 + n)?).to_ascii_lowercase());
            },
            43 => {
                let n = *data.first()? as usize;
                h.versions = (0..n / 2).filter_map(|i| be16(data, 1 + 2 * i)).collect();
            },
            51 => {
                let total = be16(data, 0)? as usize;
                let mut q = 2;
                while q + 4 <= (2 + total).min(data.len()) {
                    let group = be16(data, q)?;
                    let n = be16(data, q + 2)? as usize;
                    h.shares.push((group, data.get(q + 4..q + 4 + n)?.to_vec()));
                    q += 4 + n;
                }
            },
            _ => {},
        }
    }
    Some(h)
}

/// Key exchange: our public share and the shared secret.
fn key_share(group: u16, peer: &[u8], rng: &mut Rng) -> Option<(Vec<u8>, Vec<u8>)> {
    let mut seed = [0u8; 32];
    rng.fill(&mut seed);
    match group {
        X25519 => {
            let peer: [u8; 32] = peer.try_into().ok()?;
            let secret = x25519_dalek::StaticSecret::from(seed);
            let ours = x25519_dalek::PublicKey::from(&secret).as_bytes().to_vec();
            let shared = secret.diffie_hellman(&x25519_dalek::PublicKey::from(peer));
            if !shared.was_contributory() {
                return None;
            }
            Some((ours, shared.as_bytes().to_vec()))
        },
        SECP256R1 => {
            let peer = p256::PublicKey::from_sec1_bytes(peer).ok()?;
            let secret = p256::SecretKey::from_slice(&seed).ok()?;
            let ours = p256::EncodedPoint::from(secret.public_key());
            let shared = p256::ecdh::diffie_hellman(secret.to_nonzero_scalar(), peer.as_affine());
            Some((ours.as_bytes().to_vec(), shared.raw_secret_bytes().to_vec()))
        },
        _ => None,
    }
}

#[derive(PartialEq)]
enum State {
    Hello,
    /// Waiting for the client's Finished
    Finished,
    Open,
    Closed,
}

/// The TLS side of one connection.
pub struct Tls {
    state: State,
    incoming: Vec<u8>,
    handshake: Vec<u8>,
    read: Option<Keys>,
    write: Option<Keys>,
    client_app: Option<Keys>,
    /// The name the client asked for
    pub host: Option<String>,
}

/// What `receive` produced.
#[derive(Default)]
pub struct Received {
    /// Records for the client
    pub out: Vec<u8>,
    /// Decrypted application data
    pub data: Vec<u8>,
    /// The client closed (or the handshake failed): close the connection
    pub closed: bool,
}

impl Default for Tls {
    fn default() -> Self {
        Tls {
            state: State::Hello,
            incoming: Vec::new(),
            handshake: Vec::new(),
            read: None,
            write: None,
            client_app: None,
            host: None,
        }
    }
}

impl Tls {
    pub fn is_open(&self) -> bool {
        self.state == State::Open
    }

    pub fn receive(&mut self, bytes: &[u8], keys: Option<&mut TlsKeys>, rng: &mut Rng) -> Received {
        let mut r = Received::default();
        if self.state == State::Closed {
            r.closed = true;
            return r;
        }
        self.incoming.extend_from_slice(bytes);
        let mut keys = keys;
        while self.incoming.len() >= 5 {
            let len = be16(&self.incoming, 3).unwrap() as usize;
            if self.incoming.len() < 5 + len {
                break;
            }
            let header: [u8; 5] = self.incoming[..5].try_into().unwrap();
            let fragment = self.incoming[5..5 + len].to_vec();
            self.incoming.drain(..5 + len);
            if let Err(alert) = self.record(&header, &fragment, keys.as_deref_mut(), rng, &mut r) {
                if let Some(desc) = alert {
                    self.alert(desc, &mut r.out);
                }
                self.state = State::Closed;
                r.closed = true;
                return r;
            }
            if self.state == State::Closed {
                r.closed = true;
                return r;
            }
        }
        r
    }

    /// Err(Some(alert)) / Err(None): fail the connection (with an alert, or silently).
    fn record(
        &mut self,
        header: &[u8; 5],
        fragment: &[u8],
        keys: Option<&mut TlsKeys>,
        rng: &mut Rng,
        r: &mut Received,
    ) -> Result<(), Option<u8>> {
        match header[0] {
            CCS => return Ok(()), // middlebox compatibility; meaningless in TLS 1.3
            ALERT if self.read.is_none() => {
                self.state = State::Closed;
                return Ok(());
            },
            HANDSHAKE if self.state == State::Hello => {
                self.handshake.extend_from_slice(fragment);
                if self.handshake.len() < 4 {
                    return Ok(());
                }
                let n = (self.handshake[1] as usize) << 16
                    | (self.handshake[2] as usize) << 8
                    | self.handshake[3] as usize;
                if self.handshake.len() < 4 + n {
                    return Ok(());
                }
                if self.handshake[0] != CLIENT_HELLO {
                    return Err(Some(10)); // unexpected_message
                }
                let hello = self.handshake[..4 + n].to_vec();
                let keys = keys.ok_or(Some(40u8))?;
                return self.server_handshake(&hello, keys, rng, &mut r.out);
            },
            APPDATA if self.read.is_some() => {},
            _ => return Err(Some(10)),
        }
        let read = self.read.as_mut().unwrap();
        let nonce = read.nonce();
        let plain = read
            .aead
            .decrypt(
                &Nonce::from(nonce),
                Payload {
                    msg: fragment,
                    aad: header,
                },
            )
            .map_err(|_| Some(20u8))?; // bad_record_mac
                                       // The last non-zero byte is the real content type
        let end = plain.iter().rposition(|&b| b != 0).ok_or(Some(10u8))?;
        let inner = plain[end];
        let content = &plain[..end];
        match inner {
            HANDSHAKE if self.state == State::Finished && content.first() == Some(&FINISHED) => {
                // The client's Finished: from here on it sends with its application keys
                self.read = self.client_app.take();
                self.state = State::Open;
            },
            APPDATA if self.state == State::Open => r.data.extend_from_slice(content),
            ALERT => self.state = State::Closed,
            // Post-handshake messages (tickets, key updates we don't ask for): ignored
            _ => {},
        }
        Ok(())
    }

    fn server_handshake(
        &mut self,
        hello: &[u8],
        keys: &mut TlsKeys,
        rng: &mut Rng,
        out: &mut Vec<u8>,
    ) -> Result<(), Option<u8>> {
        let h = parse_client_hello(&hello[4..]).ok_or(Some(50u8))?; // decode_error
        if !h.versions.contains(&TLS13) || !h.suites.contains(&AES_128_GCM_SHA256) {
            return Err(Some(70)); // protocol_version
        }
        // An IP literal sends no SNI: there is no name to make a certificate for
        let host = h.host.clone().ok_or(Some(112u8))?; // unrecognized_name
        self.host = Some(host.clone());
        let (group, ours, shared) = [X25519, SECP256R1]
            .iter()
            .find_map(|&g| {
                let (_, peer) = h.shares.iter().find(|(gg, _)| *gg == g)?;
                key_share(g, peer, rng).map(|(o, s)| (g, o, s))
            })
            .ok_or(Some(40u8))?; // handshake_failure

        let mut ext = Vec::new();
        ext.extend_from_slice(&[0, 43, 0, 2, 0x03, 0x04]);
        ext.extend_from_slice(&[0, 51]);
        ext.extend_from_slice(&((4 + ours.len()) as u16).to_be_bytes());
        ext.extend_from_slice(&group.to_be_bytes());
        ext.extend_from_slice(&(ours.len() as u16).to_be_bytes());
        ext.extend_from_slice(&ours);
        let mut random = [0u8; 32];
        rng.fill(&mut random);
        let mut body = vec![0x03, 0x03];
        body.extend_from_slice(&random);
        body.push(h.session_id.len() as u8);
        body.extend_from_slice(&h.session_id);
        body.extend_from_slice(&AES_128_GCM_SHA256.to_be_bytes());
        body.push(0);
        body.extend_from_slice(&(ext.len() as u16).to_be_bytes());
        body.extend_from_slice(&ext);
        let server_hello = handshake_msg(SERVER_HELLO, &body);
        out.extend_from_slice(&[HANDSHAKE, 3, 3]);
        out.extend_from_slice(&(server_hello.len() as u16).to_be_bytes());
        out.extend_from_slice(&server_hello);
        if !h.session_id.is_empty() {
            out.extend_from_slice(&[CCS, 3, 3, 0, 1, 1]);
        }

        let zeros = [0u8; 32];
        let mut transcript = [hello, &server_hello[..]].concat();
        let early = hmac(&zeros, &[&zeros]);
        let hs_secret = hmac(&derive_secret(&early, "derived", &[]), &[&shared]);
        let client_hs = derive_secret(&hs_secret, "c hs traffic", &transcript);
        let server_hs = derive_secret(&hs_secret, "s hs traffic", &transcript);
        self.read = Some(Keys::new(&client_hs));
        self.write = Some(Keys::new(&server_hs));

        let cert = keys.certificate(&host, rng);
        let ee = handshake_msg(ENCRYPTED_EXTENSIONS, &[0, 0]);
        let mut cbody = vec![0];
        let entry = cert.len() + 5;
        cbody.extend_from_slice(&[(entry >> 16) as u8, (entry >> 8) as u8, entry as u8]);
        cbody.extend_from_slice(&[
            (cert.len() >> 16) as u8,
            (cert.len() >> 8) as u8,
            cert.len() as u8,
        ]);
        cbody.extend_from_slice(&cert);
        cbody.extend_from_slice(&[0, 0]);
        let cert_msg = handshake_msg(CERTIFICATE, &cbody);
        transcript.extend_from_slice(&ee);
        transcript.extend_from_slice(&cert_msg);

        let mut signed = vec![0x20u8; 64];
        signed.extend_from_slice(b"TLS 1.3, server CertificateVerify\0");
        signed.extend_from_slice(&sha256(&[&transcript]));
        let sig: p256::ecdsa::Signature = keys.leaf_key.sign(&signed);
        let sig = sig.to_der();
        let mut vbody = ECDSA_P256_SHA256.to_be_bytes().to_vec();
        vbody.extend_from_slice(&(sig.as_bytes().len() as u16).to_be_bytes());
        vbody.extend_from_slice(sig.as_bytes());
        let verify = handshake_msg(CERTIFICATE_VERIFY, &vbody);
        transcript.extend_from_slice(&verify);

        let finished_key = expand_label(&server_hs, "finished", &[], 32);
        let finished = handshake_msg(FINISHED, &hmac(&finished_key, &[&sha256(&[&transcript])]));
        transcript.extend_from_slice(&finished);
        self.seal(HANDSHAKE, &[ee, cert_msg, verify, finished].concat(), out);

        let master = hmac(&derive_secret(&hs_secret, "derived", &[]), &[&zeros]);
        self.client_app = Some(Keys::new(&derive_secret(
            &master,
            "c ap traffic",
            &transcript,
        )));
        self.write = Some(Keys::new(&derive_secret(
            &master,
            "s ap traffic",
            &transcript,
        )));
        self.state = State::Finished;
        let _ = der; // (certificates are DER; see cert.rs)
        Ok(())
    }

    fn seal(&mut self, ty: u8, data: &[u8], out: &mut Vec<u8>) {
        let w = self.write.as_mut().unwrap();
        for chunk in data
            .chunks(MAX_RECORD)
            .chain(data.is_empty().then_some(&[][..]))
        {
            let mut inner = chunk.to_vec();
            inner.push(ty);
            let len = inner.len() + 16;
            let header = [APPDATA, 3, 3, (len >> 8) as u8, len as u8];
            let nonce = w.nonce();
            let sealed = w
                .aead
                .encrypt(
                    &Nonce::from(nonce),
                    Payload {
                        msg: &inner,
                        aad: &header,
                    },
                )
                .unwrap();
            out.extend_from_slice(&header);
            out.extend_from_slice(&sealed);
        }
    }

    /// Application data for the client.
    pub fn send(&mut self, data: &[u8], out: &mut Vec<u8>) {
        if self.write.is_some() && self.state != State::Closed && !data.is_empty() {
            self.seal(APPDATA, data, out);
        }
    }

    /// close_notify.
    pub fn close(&mut self, out: &mut Vec<u8>) {
        if self.write.is_some() && self.state != State::Closed {
            self.seal(ALERT, &[1, 0], out);
        }
        self.state = State::Closed;
    }

    fn alert(&mut self, desc: u8, out: &mut Vec<u8>) {
        if self.write.is_some() && self.state != State::Hello {
            self.seal(ALERT, &[2, desc], out);
        } else {
            out.extend_from_slice(&[ALERT, 3, 3, 0, 2, 2, desc]);
        }
    }
}
