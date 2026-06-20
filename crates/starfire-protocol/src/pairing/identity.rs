// SPDX-License-Identifier: Apache-2.0
//! Client identity — docs/protocol/02-pairing-and-crypto.md §1.
//! A self-signed certificate + a stable unique id. The cert is added to the
//! host's trusted set during pairing and used for mTLS thereafter.
//!
//! Key type is being validated against live Sunshine: we start with rcgen's
//! default (ECDSA P-256). If the host rejects it in `getservercert`, switch to
//! RSA-2048 (the Moonlight-compatible choice). [CAPTURE-LOCKED]

use rcgen::{CertificateParams, DistinguishedName, DnType, KeyPair};

use crate::hex;
use crate::pairing::crypto::random_bytes;

/// A generated client identity. `cert_pem`/`key_pem` are PEM; the pairing ladder
/// sends the cert (hex-encoded) and signs with the key.
#[derive(Debug, Clone)]
pub struct ClientIdentity {
    /// Hex client id Sunshine keys pairing state by (stable per install).
    pub unique_id: String,
    pub device_name: String,
    pub cert_pem: String,
    pub key_pem: String,
}

impl ClientIdentity {
    /// Generate a fresh identity (new key + self-signed cert + random unique id).
    pub fn generate(device_name: &str) -> crate::Result<Self> {
        let key_pair = KeyPair::generate().map_err(cert_err)?;
        let mut params = CertificateParams::new(Vec::<String>::new()).map_err(cert_err)?;
        let mut dn = DistinguishedName::new();
        dn.push(DnType::CommonName, "Starfire");
        params.distinguished_name = dn;
        let cert = params.self_signed(&key_pair).map_err(cert_err)?;

        Ok(Self {
            unique_id: hex::encode_upper(&random_bytes::<8>()?),
            device_name: device_name.to_string(),
            cert_pem: cert.pem(),
            key_pem: key_pair.serialize_pem(),
        })
    }

    /// The X.509 signature bytes of our own cert (used in the pairing hash chain).
    pub fn cert_signature(&self) -> crate::Result<Vec<u8>> {
        cert_signature_of(self.cert_pem.as_bytes())
    }

    /// ECDSA-sign `msg` with our private key (P-256, SHA-256), DER-encoded — the
    /// form OpenSSL (Sunshine) verifies against our cert's public key.
    pub fn sign(&self, msg: &[u8]) -> crate::Result<Vec<u8>> {
        use p256::ecdsa::signature::Signer;
        use p256::ecdsa::{Signature, SigningKey};
        use p256::pkcs8::DecodePrivateKey;

        let key = SigningKey::from_pkcs8_pem(&self.key_pem)
            .map_err(|e| crate::Error::Protocol(format!("load signing key: {e}")))?;
        let sig: Signature = key.sign(msg);
        Ok(sig.to_der().as_bytes().to_vec())
    }
}

/// Extract the X.509 `signatureValue` bytes from a cert (PEM or DER). These feed
/// the pairing challenge hash chain (docs/protocol/02 §3).
pub fn cert_signature_of(cert_bytes: &[u8]) -> crate::Result<Vec<u8>> {
    use x509_parser::prelude::FromDer;

    let der_owned;
    let der: &[u8] = match x509_parser::pem::parse_x509_pem(cert_bytes) {
        Ok((_, pem)) => {
            der_owned = pem.contents;
            &der_owned
        }
        Err(_) => cert_bytes, // already DER
    };
    let (_, cert) = x509_parser::certificate::X509Certificate::from_der(der)
        .map_err(|e| crate::Error::Protocol(format!("parse cert: {e}")))?;
    Ok(cert.signature_value.data.to_vec())
}

fn cert_err(e: rcgen::Error) -> crate::Error {
    crate::Error::Protocol(format!("client cert: {e}"))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn generates_distinct_identities() {
        let a = ClientIdentity::generate("starfire-test").unwrap();
        let b = ClientIdentity::generate("starfire-test").unwrap();
        assert!(a.cert_pem.contains("BEGIN CERTIFICATE"));
        assert!(a.key_pem.contains("PRIVATE KEY"));
        assert_eq!(a.unique_id.len(), 16); // 8 bytes -> 16 hex chars
        assert_ne!(a.unique_id, b.unique_id);
        assert_ne!(a.cert_pem, b.cert_pem);
    }
}
