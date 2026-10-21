//! One trust store and one client configuration per [`SslMode`], for
//! every adapter that speaks TLS through rustls here.
//!
//! The roots are the platform's: the OS trust store, which is where an
//! enterprise's private CA already lives, with the Mozilla bundle as the
//! fallback when the platform has none to offer, and `ca_file` appended
//! for a CA that lives nowhere else. `ring` is installed as the process's
//! default provider once, because tiberius brings rustls's default
//! provider, aws-lc, in beside it, and with two compiled in a bare
//! `ClientConfig::builder()` would otherwise pick one unspecified.

use crate::{Result, SourceError, SslMode};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, RootCertStore, SignatureScheme};
use std::path::Path;
use std::sync::{Arc, Once};

static RING: Once = Once::new();

/// `ring`, once, as the process default. Idempotent; a second call is a
/// no-op, and a call after another provider was installed is ignored.
pub fn install_ring() -> Arc<rustls::crypto::CryptoProvider> {
    RING.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
    Arc::new(rustls::crypto::ring::default_provider())
}

/// The platform's roots, the bundle if the platform has none, and
/// `ca_file` on top. Loaded on every call: a catalogue read opens one
/// connection, and a stale store is worse than a slow one.
pub fn roots(uid: &str, ca_file: Option<&Path>) -> Result<RootCertStore> {
    let mut store = RootCertStore::empty();
    let native = rustls_native_certs::load_native_certs();
    if native.certs.is_empty() {
        store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    } else {
        let (_, _) = store.add_parsable_certificates(native.certs);
    }
    if let Some(path) = ca_file {
        use rustls::pki_types::pem::PemObject;
        let fail = |e: String| SourceError::CaFile(format!("{uid}: {}", path.display()), e);
        let mut added = 0;
        for cert in CertificateDer::pem_file_iter(path).map_err(|e| fail(e.to_string()))? {
            let cert = cert.map_err(|e| fail(e.to_string()))?;
            store.add(cert).map_err(|e| fail(e.to_string()))?;
            added += 1;
        }
        if added == 0 {
            return Err(fail("no certificates in the file".into()));
        }
    }
    Ok(store)
}

/// The client configuration this mode calls for. `Disable` has none.
pub fn client_config(uid: &str, mode: SslMode, ca_file: Option<&Path>) -> Result<rustls::ClientConfig> {
    let provider = install_ring();
    let builder = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .expect("ring provides the default protocol versions");
    let config = match mode {
        SslMode::Disable | SslMode::Prefer | SslMode::Require => {
            builder.dangerous().with_custom_certificate_verifier(Arc::new(Unverified { provider })).with_no_client_auth()
        }
        SslMode::VerifyCa => {
            let inner = rustls::client::WebPkiServerVerifier::builder_with_provider(Arc::new(roots(uid, ca_file)?), provider)
                .build()
                .map_err(|e| SourceError::CaFile(uid.to_string(), e.to_string()))?;
            builder.dangerous().with_custom_certificate_verifier(Arc::new(ChainOnly(inner))).with_no_client_auth()
        }
        SslMode::VerifyFull => builder.with_root_certificates(roots(uid, ca_file)?).with_no_client_auth(),
    };
    Ok(config)
}

/// `require`: the channel is encrypted and the peer is whoever answered.
/// libpq's `require` is exactly this, and it is named, not a flag.
#[derive(Debug)]
struct Unverified {
    provider: Arc<rustls::crypto::CryptoProvider>,
}

impl ServerCertVerifier for Unverified {
    fn verify_server_cert(
        &self,
        _: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: &ServerName<'_>,
        _: &[u8],
        _: UnixTime,
    ) -> std::result::Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }
    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider.signature_verification_algorithms.supported_schemes()
    }
}

/// `verify-ca`: the chain is checked against the store, the name is not,
/// which is libpq's and MySQL's rung between `require` and the full check.
#[derive(Debug)]
struct ChainOnly(Arc<rustls::client::WebPkiServerVerifier>);

impl ServerCertVerifier for ChainOnly {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp: &[u8],
        now: UnixTime,
    ) -> std::result::Result<ServerCertVerified, rustls::Error> {
        match self.0.verify_server_cert(end_entity, intermediates, server_name, ocsp, now) {
            Err(rustls::Error::InvalidCertificate(rustls::CertificateError::NotValidForName))
            | Err(rustls::Error::InvalidCertificate(rustls::CertificateError::NotValidForNameContext { .. })) => {
                Ok(ServerCertVerified::assertion())
            }
            other => other,
        }
    }
    fn verify_tls12_signature(
        &self,
        m: &[u8],
        c: &CertificateDer<'_>,
        d: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        self.0.verify_tls12_signature(m, c, d)
    }
    fn verify_tls13_signature(
        &self,
        m: &[u8],
        c: &CertificateDer<'_>,
        d: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        self.0.verify_tls13_signature(m, c, d)
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.supported_verify_schemes()
    }
}
