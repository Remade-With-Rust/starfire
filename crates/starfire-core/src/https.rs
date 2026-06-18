// SPDX-License-Identifier: Apache-2.0
//! Minimal mTLS HTTPS client for the paired GameStream endpoints (F3+):
//! authenticated `/serverinfo`, `/applist`, `/launch` on 47984, and the host's
//! web API. Uses our client identity for TLS client auth (docs/protocol/02) and
//! **pins the host's certificate** (the one learned at pairing) rather than CA
//! validation — the right trust model for a self-signed, self-hosted peer.
//!
//! rustls with the `ring` provider; no global state (provider passed explicitly).

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::time::Duration;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use rustls::{ClientConfig, ClientConnection, DigitallySignedStruct, SignatureScheme, StreamOwned};

use crate::discovery::{parse_http_response, HttpResponse};

/// An mTLS HTTPS client bound to one client identity + (optionally) one pinned
/// host certificate.
pub struct HttpsClient {
    config: Arc<ClientConfig>,
}

impl HttpsClient {
    /// Build a client with TLS client auth from the identity PEMs. If
    /// `pinned_host_cert_der` is `Some`, the host must present exactly that leaf
    /// certificate; if `None`, any host cert is accepted (dev/bring-up only —
    /// always pin in production).
    pub fn new(
        client_cert_pem: &str,
        client_key_pem: &str,
        pinned_host_cert_der: Option<Vec<u8>>,
    ) -> crate::Result<Self> {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let certs = load_certs(client_cert_pem)?;
        let key = load_key(client_key_pem)?;

        let verifier = Arc::new(HostCertVerifier {
            pinned: pinned_host_cert_der.map(CertificateDer::from),
            schemes: provider
                .signature_verification_algorithms
                .supported_schemes(),
            provider: provider.clone(),
        });

        let mut config = ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .map_err(tls_err)?
            .dangerous()
            .with_custom_certificate_verifier(verifier)
            .with_client_auth_cert(certs, key)
            .map_err(tls_err)?;

        // Sunshine's TLS rejects resumed sessions (sends an InternalError alert on
        // the 2nd connection), so each request gets a fresh full handshake.
        config.resumption = rustls::client::Resumption::disabled();

        Ok(Self {
            config: Arc::new(config),
        })
    }

    /// Issue a one-shot `GET` over mTLS and read the full response.
    pub fn get(
        &self,
        host: &str,
        port: u16,
        path: &str,
        timeout: Duration,
    ) -> crate::Result<HttpResponse> {
        let server_name = ServerName::try_from(host.to_string())
            .map_err(|e| crate::Error::Protocol(format!("invalid server name {host}: {e}")))?;
        let conn = ClientConnection::new(self.config.clone(), server_name).map_err(tls_err)?;

        let sock = TcpStream::connect((host, port))?;
        sock.set_read_timeout(Some(timeout))?;
        sock.set_write_timeout(Some(timeout))?;
        let mut tls = StreamOwned::new(conn, sock);

        let request = format!(
            "GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\nUser-Agent: starfire\r\n\r\n"
        );
        tls.write_all(request.as_bytes())?;

        // A peer that closes without close_notify surfaces as UnexpectedEof; that
        // is a normal end-of-response here, so keep whatever we read.
        let mut raw = Vec::new();
        if let Err(e) = tls.read_to_end(&mut raw) {
            if raw.is_empty() {
                return Err(e.into());
            }
        }
        parse_http_response(&raw)
    }
}

/// Verifies the host cert by **pinning** (exact leaf match) when a pin is set,
/// and always verifies the handshake signature against the presented cert (so a
/// peer must actually possess the key, not merely replay a known public cert).
#[derive(Debug)]
struct HostCertVerifier {
    pinned: Option<CertificateDer<'static>>,
    schemes: Vec<SignatureScheme>,
    provider: Arc<CryptoProvider>,
}

impl ServerCertVerifier for HostCertVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        match &self.pinned {
            Some(p) if p.as_ref() != end_entity.as_ref() => Err(rustls::Error::General(
                "host certificate does not match the pinned certificate".into(),
            )),
            _ => Ok(ServerCertVerified::assertion()),
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.schemes.clone()
    }
}

/// Extract the first certificate's DER bytes from PEM — used to turn the host
/// cert learned at pairing (PEM) into the DER form [`HostCertVerifier`] pins.
pub fn cert_pem_to_der(pem: &[u8]) -> crate::Result<Vec<u8>> {
    use rustls::pki_types::pem::PemObject;
    let cert = CertificateDer::pem_slice_iter(pem)
        .next()
        .ok_or_else(|| crate::Error::Protocol("no certificate in host PEM".into()))?
        .map_err(|e| crate::Error::Protocol(format!("parse host cert PEM: {e}")))?;
    Ok(cert.as_ref().to_vec())
}

fn load_certs(pem: &str) -> crate::Result<Vec<CertificateDer<'static>>> {
    use rustls::pki_types::pem::PemObject;
    CertificateDer::pem_slice_iter(pem.as_bytes())
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| crate::Error::Protocol(format!("load client cert: {e}")))
}

fn load_key(pem: &str) -> crate::Result<PrivateKeyDer<'static>> {
    use rustls::pki_types::pem::PemObject;
    PrivateKeyDer::from_pem_slice(pem.as_bytes())
        .map_err(|e| crate::Error::Protocol(format!("load client key: {e}")))
}

fn tls_err(e: rustls::Error) -> crate::Error {
    crate::Error::Protocol(format!("TLS: {e}"))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn cert_pem_to_der_extracts_a_cert() {
        let id = crate::pairing::ClientIdentity::generate("starfire-test").unwrap();
        let der = cert_pem_to_der(id.cert_pem.as_bytes()).unwrap();
        assert!(!der.is_empty());
        assert_eq!(der[0], 0x30, "DER certificate starts with a SEQUENCE tag");
    }

    #[test]
    fn cert_pem_to_der_rejects_empty() {
        assert!(cert_pem_to_der(b"not a pem").is_err());
    }

    #[test]
    fn https_client_builds_with_and_without_pin() {
        let id = crate::pairing::ClientIdentity::generate("starfire-test").unwrap();
        assert!(HttpsClient::new(&id.cert_pem, &id.key_pem, None).is_ok());
        let der = cert_pem_to_der(id.cert_pem.as_bytes()).unwrap();
        assert!(HttpsClient::new(&id.cert_pem, &id.key_pem, Some(der)).is_ok());
    }
}
