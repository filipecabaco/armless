//! The CA the guest trusts (made for each site at build time; `tls.json`), and the
//! certificates minted from it for each host name the guest connects to.

use std::collections::HashMap;

use p256::ecdsa::signature::Signer;
use p256::ecdsa::{Signature, SigningKey};
use p256::pkcs8::DecodePrivateKey;

use super::{base64_decode, Rng};
use crate::dev::json::{self, Json};

pub struct TlsKeys {
    ca_subject: Vec<u8>,
    ca_key_id: Vec<u8>,
    /// SubjectPublicKeyInfo of the leaf key every minted certificate carries
    leaf_spki: Vec<u8>,
    ca_key: SigningKey,
    pub leaf_key: SigningKey,
    minted: HashMap<String, Vec<u8>>,
}

impl TlsKeys {
    /// From `tls.json`: base64 DER fields caSubject, caKeyId, leafSpki, caKey and leafKey
    /// (the keys as PKCS#8).
    pub fn from_json(text: &[u8]) -> Result<TlsKeys, String> {
        let doc = json::parse(text)?;
        let field = |k: &str| -> Result<Vec<u8>, String> {
            let s = doc
                .get(k)
                .and_then(Json::as_str)
                .ok_or_else(|| format!("tls.json: no {k}"))?;
            base64_decode(s).ok_or_else(|| format!("tls.json: {k} is not base64"))
        };
        let key = |k: &str| -> Result<SigningKey, String> {
            SigningKey::from_pkcs8_der(&field(k)?).map_err(|e| format!("tls.json: {k}: {e}"))
        };
        Ok(TlsKeys {
            ca_subject: field("caSubject")?,
            ca_key_id: field("caKeyId")?,
            leaf_spki: field("leafSpki")?,
            ca_key: key("caKey")?,
            leaf_key: key("leafKey")?,
            minted: HashMap::new(),
        })
    }

    /// A certificate for `host`, signed by the CA (DER).
    pub fn certificate(&mut self, host: &str, rng: &mut Rng) -> Vec<u8> {
        if let Some(c) = self.minted.get(host) {
            return c.clone();
        }
        let cert = self.mint(host, rng);
        self.minted.insert(host.to_string(), cert.clone());
        cert
    }

    fn mint(&self, host: &str, rng: &mut Rng) -> Vec<u8> {
        let name = seq(&[&der(
            0x31,
            &seq(&[&oid(&[0x55, 0x04, 0x03]), &der(0x0c, host.as_bytes())]),
        )]);
        let mut serial = [0u8; 16];
        rng.fill(&mut serial);
        serial[0] = 0x01;
        let ext = |id: &[u8], value: &[u8]| seq(&[&oid(id), &der(0x04, value)]);
        let extensions = der(
            0xa3,
            &seq(&[
                // subjectAltName: dNSName
                &ext(&[0x55, 0x1d, 0x11], &seq(&[&der(0x82, host.as_bytes())])),
                // authorityKeyIdentifier
                &ext(&[0x55, 0x1d, 0x23], &seq(&[&der(0x80, &self.ca_key_id)])),
                // extendedKeyUsage: serverAuth
                &ext(
                    &[0x55, 0x1d, 0x25],
                    &seq(&[&oid(&[0x2b, 0x06, 0x01, 0x05, 0x05, 0x07, 0x03, 0x01])]),
                ),
            ]),
        );
        let alg = ecdsa_with_sha256();
        // The guest's clock is wherever the snapshot left it, so validity can't track the date
        let validity = seq(&[&der(0x17, b"000101000000Z"), &der(0x17, b"491231235959Z")]);
        let tbs = seq(&[
            &der(0xa0, &der(0x02, &[2])),
            &integer(&serial),
            &alg,
            &self.ca_subject,
            &validity,
            &name,
            &self.leaf_spki,
            &extensions,
        ]);
        let sig: Signature = self.ca_key.sign(&tbs);
        let mut bits = vec![0u8];
        bits.extend_from_slice(sig.to_der().as_bytes());
        seq(&[&tbs, &alg, &der(0x03, &bits)])
    }
}

fn ecdsa_with_sha256() -> Vec<u8> {
    seq(&[&oid(&[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x04, 0x03, 0x02])])
}

pub fn der(tag: u8, content: &[u8]) -> Vec<u8> {
    let n = content.len();
    let mut out = vec![tag];
    if n < 128 {
        out.push(n as u8);
    } else if n < 256 {
        out.extend_from_slice(&[0x81, n as u8]);
    } else {
        out.extend_from_slice(&[0x82, (n >> 8) as u8, n as u8]);
    }
    out.extend_from_slice(content);
    out
}

pub fn seq(items: &[&[u8]]) -> Vec<u8> {
    der(0x30, &items.concat())
}

fn oid(bytes: &[u8]) -> Vec<u8> {
    der(0x06, bytes)
}

fn integer(bytes: &[u8]) -> Vec<u8> {
    let mut b = bytes;
    while b.len() > 1 && b[0] == 0 {
        b = &b[1..];
    }
    if b[0] & 0x80 != 0 {
        let mut v = vec![0];
        v.extend_from_slice(b);
        der(0x02, &v)
    } else {
        der(0x02, b)
    }
}
