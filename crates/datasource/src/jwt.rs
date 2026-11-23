//! RS256 for the warehouses that take a signed assertion instead of a
//! password. Snowflake's key-pair login is a JWT the client signs with its
//! own RSA key; BigQuery's service-account grant is the same machinery
//! with other claims. One signer serves both, on the `ring` already in
//! the tree for TLS.

use crate::{Result, SourceError};
use base64::Engine;
use ring::signature::{RsaKeyPair, RSA_PKCS1_SHA256};

pub struct Signer {
    key: RsaKeyPair,
    rng: ring::rand::SystemRandom,
}

impl Signer {
    /// From a PEM: PKCS#8 (`BEGIN PRIVATE KEY`, what `openssl genrsa` and
    /// Snowflake's own instructions produce) or PKCS#1 (`BEGIN RSA PRIVATE
    /// KEY`). An encrypted key is refused by name with the command that
    /// strips the passphrase, since a tool that runs unattended has
    /// nowhere to ask for one.
    pub fn from_pem(uid: &str, pem: &[u8]) -> Result<Signer> {
        use rustls::pki_types::{pem::PemObject, PrivateKeyDer};
        let fail = |what: String| SourceError::PrivateKey(uid.to_string(), what);
        if String::from_utf8_lossy(pem).contains("ENCRYPTED PRIVATE KEY") {
            return Err(fail(
                "the key is encrypted; write it out without the passphrase with `openssl pkcs8 -in <key> -out <key>.plain.p8 -nocrypt` and use that".into(),
            ));
        }
        let der = PrivateKeyDer::from_pem_slice(pem).map_err(|e| fail(e.to_string()))?;
        let key = match &der {
            PrivateKeyDer::Pkcs8(k) => RsaKeyPair::from_pkcs8(k.secret_pkcs8_der()),
            PrivateKeyDer::Pkcs1(k) => RsaKeyPair::from_der(k.secret_pkcs1_der()),
            _ => return Err(fail("not an RSA key".into())),
        }
        .map_err(|e| fail(e.to_string()))?;
        Ok(Signer { key, rng: ring::rand::SystemRandom::new() })
    }

    /// The public key as PKCS#1 `RSAPublicKey` DER.
    pub fn public_key_pkcs1(&self) -> &[u8] {
        self.key.public().as_ref()
    }

    /// SHA-256 of the public key as `SubjectPublicKeyInfo` DER, base64:
    /// what `openssl rsa -pubout -outform DER | openssl dgst -sha256
    /// -binary | openssl base64` prints, and what Snowflake shows as
    /// `RSA_PUBLIC_KEY_FP`.
    pub fn public_key_fingerprint(&self) -> String {
        let spki = spki(self.public_key_pkcs1());
        let digest = ring::digest::digest(&ring::digest::SHA256, &spki);
        base64::engine::general_purpose::STANDARD.encode(digest.as_ref())
    }

    /// A signed token carrying these claims.
    pub fn rs256(&self, claims: &serde_json::Value) -> Result<String> {
        let b64 = |b: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b);
        let signing = format!("{}.{}", b64(br#"{"alg":"RS256","typ":"JWT"}"#), b64(claims.to_string().as_bytes()));
        let mut sig = vec![0; self.key.public().modulus_len()];
        self.key
            .sign(&RSA_PKCS1_SHA256, &self.rng, signing.as_bytes(), &mut sig)
            .map_err(|e| SourceError::PrivateKey(String::new(), e.to_string()))?;
        Ok(format!("{signing}.{}", b64(&sig)))
    }
}

/// `RSAPublicKey` wrapped as `SubjectPublicKeyInfo`: a sequence of the
/// rsaEncryption algorithm identifier and the key as a bit string. Fixed
/// bytes plus two lengths, which is not worth a DER crate.
fn spki(pkcs1: &[u8]) -> Vec<u8> {
    const RSA_ENCRYPTION: [u8; 15] = [0x30, 0x0d, 0x06, 0x09, 0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x01, 0x05, 0x00];
    let mut bits = vec![0u8];
    bits.extend_from_slice(pkcs1);
    let mut inner = RSA_ENCRYPTION.to_vec();
    inner.extend(der(0x03, &bits));
    der(0x30, &inner)
}

/// One DER element: tag, length, content.
fn der(tag: u8, content: &[u8]) -> Vec<u8> {
    let mut out = vec![tag];
    let n = content.len();
    if n < 0x80 {
        out.push(n as u8);
    } else if n <= 0xff {
        out.extend([0x81, n as u8]);
    } else if n <= 0xffff {
        out.extend([0x82, (n >> 8) as u8, n as u8]);
    } else {
        out.extend([0x83, (n >> 16) as u8, (n >> 8) as u8, n as u8]);
    }
    out.extend_from_slice(content);
    out
}
