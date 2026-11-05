//! rustls configs with fingerprint pinning in place of CA validation.
//!
//! The verifiers do not accept "any cert". They accept exactly the end-entity
//! certificates whose SHA-256 is in the pin set, and still verify the
//! handshake signature with the real algorithms. An empty pin set rejects
//! every peer. Hostnames are ignored on purpose: the certificate is
//! self-signed and its identity is the pin, not a name.

use std::sync::Arc;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{verify_tls12_signature, verify_tls13_signature, CryptoProvider};
use rustls::pki_types::{CertificateDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{ClientConfig, DigitallySignedStruct, DistinguishedName, ServerConfig, SignatureScheme};

use crate::identity::{fingerprint, to_hex, Identity};
use crate::pins::PinSet;
use crate::NetError;

fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

fn check_pin(pins: &PinSet, end_entity: &CertificateDer<'_>) -> Result<(), rustls::Error> {
    let fp = fingerprint(end_entity);
    if pins.contains(&fp) {
        Ok(())
    } else {
        Err(rustls::Error::General(format!(
            "peer fingerprint {} not pinned",
            to_hex(&fp)
        )))
    }
}

#[derive(Debug)]
struct PinVerifier {
    pins: PinSet,
    provider: Arc<CryptoProvider>,
}

impl ServerCertVerifier for PinVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        check_pin(&self.pins, end_entity)?;
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider.signature_verification_algorithms.supported_schemes()
    }
}

#[derive(Debug)]
struct PinClientVerifier {
    pins: PinSet,
    provider: Arc<CryptoProvider>,
}

impl ClientCertVerifier for PinClientVerifier {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        check_pin(&self.pins, end_entity)?;
        Ok(ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider.signature_verification_algorithms.supported_schemes()
    }
}

fn key_of(id: &Identity) -> rustls::pki_types::PrivateKeyDer<'static> {
    PrivatePkcs8KeyDer::from(id.key_der.clone()).into()
}

fn chain_of(id: &Identity) -> Vec<CertificateDer<'static>> {
    vec![CertificateDer::from(id.cert_der.clone())]
}

pub fn client_config(id: &Identity, pins: PinSet) -> Result<Arc<ClientConfig>, NetError> {
    let p = provider();
    let verifier = Arc::new(PinVerifier { pins, provider: p.clone() });
    let cfg = ClientConfig::builder_with_provider(p)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(NetError::Tls)?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_client_auth_cert(chain_of(id), key_of(id))
        .map_err(NetError::Tls)?;
    Ok(Arc::new(cfg))
}

pub fn server_config(id: &Identity, pins: PinSet) -> Result<Arc<ServerConfig>, NetError> {
    let p = provider();
    let verifier = Arc::new(PinClientVerifier { pins, provider: p.clone() });
    let cfg = ServerConfig::builder_with_provider(p)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(NetError::Tls)?
        .with_client_cert_verifier(verifier)
        .with_single_cert(chain_of(id), key_of(id))
        .map_err(NetError::Tls)?;
    Ok(Arc::new(cfg))
}

// Pairing-only configs. They accept any certificate, but the handshake
// signature is still verified, so the peer must hold the key it presents.
// Nothing trusts that peer yet: the SPAKE2 exchange that runs over this
// channel binds both certificate fingerprints, and a peer is pinned only
// after its confirmation MAC checks out (see pair.rs). The normal link never
// uses these configs.

#[derive(Debug)]
struct PairingServerVerifier {
    provider: Arc<CryptoProvider>,
}

impl ServerCertVerifier for PairingServerVerifier {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider.signature_verification_algorithms.supported_schemes()
    }
}

#[derive(Debug)]
struct PairingClientVerifier {
    provider: Arc<CryptoProvider>,
}

impl ClientCertVerifier for PairingClientVerifier {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        Ok(ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider.signature_verification_algorithms.supported_schemes()
    }
}

/// Pairing client config: accepts any server certificate (see module note above).
pub fn pairing_client_config(id: &Identity) -> Result<Arc<ClientConfig>, NetError> {
    let p = provider();
    let verifier = Arc::new(PairingServerVerifier { provider: p.clone() });
    let cfg = ClientConfig::builder_with_provider(p)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(NetError::Tls)?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_client_auth_cert(chain_of(id), key_of(id))
        .map_err(NetError::Tls)?;
    Ok(Arc::new(cfg))
}

/// Pairing server config: requires a client certificate but accepts any one.
pub fn pairing_server_config(id: &Identity) -> Result<Arc<ServerConfig>, NetError> {
    let p = provider();
    let verifier = Arc::new(PairingClientVerifier { provider: p.clone() });
    let cfg = ServerConfig::builder_with_provider(p)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(NetError::Tls)?
        .with_client_cert_verifier(verifier)
        .with_single_cert(chain_of(id), key_of(id))
        .map_err(NetError::Tls)?;
    Ok(Arc::new(cfg))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configs_build_from_a_generated_identity() {
        let id = Identity::generate().unwrap();
        let pins = PinSet::new([fingerprint(&id.cert_der)]);
        client_config(&id, pins.clone()).expect("client config");
        server_config(&id, pins).expect("server config");
    }

    #[test]
    fn pin_check_accepts_only_pinned_certificates() {
        let a = Identity::generate().unwrap();
        let b = Identity::generate().unwrap();
        let pins = PinSet::new([fingerprint(&a.cert_der)]);
        let a_der = CertificateDer::from(a.cert_der.clone());
        let b_der = CertificateDer::from(b.cert_der.clone());
        assert!(check_pin(&pins, &a_der).is_ok());
        let err = check_pin(&pins, &b_der).unwrap_err();
        assert!(err.to_string().contains("not pinned"), "got: {err}");
        assert!(check_pin(&PinSet::default(), &a_der).is_err(), "empty pins trust nobody");
    }
}
