// SPDX-License-Identifier: Apache-2.0
//! The `/pair` ladder — docs/protocol/02-pairing-and-crypto.md §2.
//! HTTP GETs to `/pair` on 47989 carrying the cert, salt, and challenge blobs.
//! Derived from the public GameStream pairing protocol; each phase is validated
//! by the live host's accept/reject. Clean-room.
//!
//! Built bottom-up: phase 1 (`getservercert`, no PIN) lands first to confirm the
//! host accepts our cert; the PIN-keyed phases follow.

use std::collections::BTreeMap;
use std::time::Duration;

use crate::discovery::http_get;
use crate::hex;
use crate::pairing::crypto::{aes_ecb_decrypt, aes_ecb_encrypt, pin_key, random_bytes, sha256};
use crate::pairing::ClientIdentity;

/// Drives the pairing ladder against one host.
pub struct PairingClient {
    pub host: String,
    pub http_port: u16,
    pub identity: ClientIdentity,
    pub timeout: Duration,
}

/// The `<root>` envelope shared by every `/pair` phase (different child elements
/// per phase).
#[derive(Debug, Default, Clone)]
pub struct PairResponse {
    pub status_code: Option<u16>,
    pub paired: bool,
    pub plaincert: Option<Vec<u8>>,
    pub challenge_response: Option<Vec<u8>>,
    pub pairing_secret: Option<Vec<u8>>,
}

impl PairingClient {
    pub fn new(host: &str, http_port: u16, identity: ClientIdentity) -> Self {
        Self {
            host: host.to_string(),
            http_port,
            identity,
            timeout: Duration::from_secs(10),
        }
    }

    fn pair_get(&self, query: &str) -> crate::Result<PairResponse> {
        let path = format!("/pair?{query}");
        let resp = http_get(&self.host, self.http_port, &path, self.timeout)?;
        if resp.status != 200 {
            return Err(crate::Error::Protocol(format!(
                "/pair HTTP {}",
                resp.status
            )));
        }
        parse_pair_response(&resp.body)
    }

    /// Phase 1 — `getservercert`. Sends our cert + salt; returns the host cert.
    /// No PIN involved yet. `paired=0` here means the host rejected our cert or a
    /// pairing is already in progress.
    pub fn get_server_cert(&self, salt: &[u8; 16]) -> crate::Result<Vec<u8>> {
        let query = format!(
            "uniqueid={uid}&uuid={uuid}&devicename={name}&updateState=1\
             &phrase=getservercert&salt={salt}&clientcert={cert}",
            uid = self.identity.unique_id,
            uuid = self.identity.unique_id,
            name = self.identity.device_name,
            salt = hex::encode(salt),
            cert = hex::encode(self.identity.cert_pem.as_bytes()),
        );
        let r = self.pair_get(&query)?;
        if !r.paired {
            return Err(crate::Error::Protocol(
                "getservercert: host returned paired=0 (cert rejected or pairing busy)".into(),
            ));
        }
        r.plaincert.ok_or_else(|| {
            crate::Error::Protocol("getservercert: no <plaincert> in response".into())
        })
    }

    /// Phase 2 — `clientchallenge`. AES-ECB-encrypts a random challenge with the
    /// PIN-derived key; returns the host's (still-encrypted) challenge response.
    /// `paired=0` here means the PIN was wrong or not yet entered on the host.
    pub fn client_challenge(&self, key: &[u8; 16], challenge: &[u8; 16]) -> crate::Result<Vec<u8>> {
        let enc = aes_ecb_encrypt(key, challenge);
        let query = format!(
            "uniqueid={uid}&clientchallenge={c}",
            uid = self.identity.unique_id,
            c = hex::encode(&enc),
        );
        let r = self.pair_get(&query)?;
        if !r.paired {
            return Err(crate::Error::Protocol(
                "clientchallenge: paired=0 (wrong PIN or PIN not entered on host)".into(),
            ));
        }
        r.challenge_response
            .ok_or_else(|| crate::Error::Protocol("no <challengeresponse> in response".into()))
    }

    /// Phase 3 — `serverchallengeresp`. Commits to our `client_secret` via
    /// `SHA-256(server_challenge ‖ client_cert_sig ‖ client_secret)`,
    /// AES-ECB-encrypted; returns the host's `pairingsecret`
    /// (`server_secret ‖ server_signature`).
    pub fn server_challenge_resp(
        &self,
        key: &[u8; 16],
        server_challenge: &[u8; 16],
        client_secret: &[u8; 16],
    ) -> crate::Result<Vec<u8>> {
        let client_cert_sig = self.identity.cert_signature()?;
        let hash = sha256(&[server_challenge, &client_cert_sig, client_secret]);
        let enc = aes_ecb_encrypt(key, &hash);
        let query = format!(
            "uniqueid={uid}&serverchallengeresp={c}",
            uid = self.identity.unique_id,
            c = hex::encode(&enc),
        );
        let r = self.pair_get(&query)?;
        if !r.paired {
            return Err(crate::Error::Protocol(
                "serverchallengeresp: paired=0 (challenge hash rejected)".into(),
            ));
        }
        r.pairing_secret
            .ok_or_else(|| crate::Error::Protocol("no <pairingsecret> in response".into()))
    }

    /// Phase 4 — `clientpairingsecret`. Reveals `client_secret` plus our signature
    /// over it; the host verifies both and adds our cert to its trusted set.
    pub fn client_pairing_secret(&self, client_secret: &[u8; 16]) -> crate::Result<()> {
        let signature = self.identity.sign(client_secret)?;
        let mut blob = client_secret.to_vec();
        blob.extend_from_slice(&signature);
        let query = format!(
            "uniqueid={uid}&clientpairingsecret={c}",
            uid = self.identity.unique_id,
            c = hex::encode(&blob),
        );
        let r = self.pair_get(&query)?;
        if !r.paired {
            return Err(crate::Error::Protocol(
                "clientpairingsecret: paired=0 (signature or commitment rejected)".into(),
            ));
        }
        Ok(())
    }

    /// Run the full HTTP ladder (phases 1–4). The PIN must be entered on the host
    /// out of band (auto-PIN) — `get_server_cert` blocks until it is. On success
    /// the host has added our cert to its trusted set.
    pub fn pair(&self, salt: &[u8; 16], pin: &str) -> crate::Result<()> {
        // Phase 1 — blocks until the PIN is entered on the host.
        let _server_cert = self.get_server_cert(salt)?;

        let key = pin_key(salt, pin);

        // Phase 2 — prove we know the PIN; recover the server challenge.
        let client_challenge = random_bytes::<16>()?;
        let enc_resp = self.client_challenge(&key, &client_challenge)?;
        let dec = aes_ecb_decrypt(&key, &enc_resp);
        if dec.len() < 48 {
            return Err(crate::Error::Protocol(format!(
                "challengeresponse too short: {} bytes (want 48)",
                dec.len()
            )));
        }
        let mut server_challenge = [0u8; 16];
        server_challenge.copy_from_slice(&dec[32..48]);

        // Phases 3–4 — commit to + reveal our secret.
        let client_secret = random_bytes::<16>()?;
        let _pairing_secret =
            self.server_challenge_resp(&key, &server_challenge, &client_secret)?;
        self.client_pairing_secret(&client_secret)?;
        Ok(())
    }
}

/// Parse a `/pair` response envelope, hex-decoding the binary fields.
pub fn parse_pair_response(xml: &[u8]) -> crate::Result<PairResponse> {
    let (status_code, fields) = leaf_fields(xml)?;
    let decode = |k: &str| -> crate::Result<Option<Vec<u8>>> {
        match fields.get(k) {
            Some(v) => {
                Ok(Some(hex::decode(v).map_err(|e| {
                    crate::Error::Protocol(format!("/pair {k}: {e}"))
                })?))
            }
            None => Ok(None),
        }
    };
    Ok(PairResponse {
        status_code,
        paired: fields.get("paired").map(|v| v == "1").unwrap_or(false),
        plaincert: decode("plaincert")?,
        challenge_response: decode("challengeresponse")?,
        pairing_secret: decode("pairingsecret")?,
    })
}

/// Extract `<root status_code>` + a tag→text map of leaf elements. Shared XML
/// shape with `/serverinfo` (docs/protocol/03).
fn leaf_fields(xml: &[u8]) -> crate::Result<(Option<u16>, BTreeMap<String, String>)> {
    use quick_xml::events::Event;
    use quick_xml::Reader;

    let mut reader = Reader::from_reader(xml);
    let mut buf = Vec::new();
    let mut stack: Vec<String> = Vec::new();
    let mut fields = BTreeMap::new();
    let mut status_code = None;

    loop {
        match reader
            .read_event_into(&mut buf)
            .map_err(|e| crate::Error::Protocol(format!("/pair XML: {e}")))?
        {
            Event::Start(e) => {
                let name = String::from_utf8_lossy(e.name().into_inner()).to_string();
                if name == "root" {
                    status_code = e
                        .attributes()
                        .flatten()
                        .find(|a| a.key.into_inner() == b"status_code")
                        .and_then(|a| std::str::from_utf8(a.value.as_ref()).ok()?.parse().ok());
                }
                stack.push(name);
            }
            Event::Text(e) => {
                let text = e
                    .unescape()
                    .map_err(|e| crate::Error::Protocol(format!("/pair XML text: {e}")))?;
                let trimmed = text.trim();
                if !trimmed.is_empty() {
                    if let Some(cur) = stack.last() {
                        fields.insert(cur.clone(), trimmed.to_string());
                    }
                }
            }
            Event::End(_) => {
                stack.pop();
            }
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }
    Ok((status_code, fields))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::pairing::crypto::random_bytes;

    #[test]
    fn parses_getservercert_response_shape() {
        let xml = br#"<?xml version="1.0"?><root status_code="200"><paired>1</paired><plaincert>4142</plaincert></root>"#;
        let r = parse_pair_response(xml).unwrap();
        assert_eq!(r.status_code, Some(200));
        assert!(r.paired);
        assert_eq!(r.plaincert.as_deref(), Some(b"AB".as_ref()));
        assert!(r.challenge_response.is_none());
    }

    #[test]
    fn parses_unpaired_response() {
        let xml = br#"<root status_code="400"><paired>0</paired></root>"#;
        let r = parse_pair_response(xml).unwrap();
        assert!(!r.paired);
    }

    /// Submit a PIN to the local test Sunshine via its web API (auto-PIN), out of
    /// band, by shelling to curl. Sunshine *blocks* the `getservercert` response
    /// until a PIN is entered, so the live test fires this on a background thread.
    fn submit_pin_via_curl(pin: &str) {
        let body = format!("{{\"pin\":\"{pin}\",\"name\":\"starfire-test\"}}");
        let _ = std::process::Command::new("curl")
            .args([
                "-sk",
                "--max-time",
                "8",
                "-u",
                "starfire:starfire-test-1",
                "-X",
                "POST",
                "https://localhost:47990/api/pin",
                "-H",
                "Content-Type: application/json",
                "-d",
                &body,
            ])
            .output();
    }

    /// Live phases 1+2: getservercert (host accepts our cert) then clientchallenge
    /// (the PIN-derived AES key is correct). Run with
    /// `-- --ignored live_pair_phases_1_2 --nocapture` while Sunshine is up.
    #[test]
    #[ignore = "requires a running Sunshine host + web API on 47989/47990"]
    fn live_pair_phases_1_2() {
        let pin = "1234";
        let id = ClientIdentity::generate("starfire-test").unwrap();
        let uid = id.unique_id.clone();
        let salt = random_bytes::<16>().unwrap();

        // getservercert blocks until the PIN is entered: submit it shortly after
        // the request is in flight.
        let pin_thread = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(800));
            submit_pin_via_curl(pin);
        });

        let client = PairingClient::new("127.0.0.1", 47989, id);
        let server_cert = client.get_server_cert(&salt).expect("getservercert");
        let _ = pin_thread.join();
        println!(
            "phase1 getservercert OK: uid={uid} server_cert={} bytes",
            server_cert.len()
        );

        let key = crate::pairing::crypto::pin_key(&salt, pin);
        let challenge = random_bytes::<16>().unwrap();
        let resp = client
            .client_challenge(&key, &challenge)
            .expect("clientchallenge");
        println!("phase2 clientchallenge OK: response={} bytes", resp.len());
    }

    /// Run a Sunshine web-API call via curl, returning stdout.
    fn curl(args: &[&str]) -> String {
        let mut full = vec!["-sk", "--max-time", "8", "-u", "starfire:starfire-test-1"];
        full.extend_from_slice(args);
        let out = std::process::Command::new("curl")
            .args(&full)
            .output()
            .expect("run curl");
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    /// Live full pairing (phases 1–4), verified via Sunshine's trusted-clients
    /// API (over plain HTTP the host can't identify us, so `PairStatus` is always
    /// 0 — the clients list is the real signal). Resets pairing state first so it
    /// is idempotent. Run with `-- --ignored live_pair_full --nocapture`.
    #[test]
    #[ignore = "requires a running Sunshine host + web API on 47989/47990"]
    fn live_pair_full() {
        // Clean slate so accumulated pending pairings don't block getservercert.
        curl(&[
            "-X",
            "POST",
            "https://localhost:47990/api/clients/unpair-all",
        ]);

        let pin = "1234";
        let device = "starfire-test";
        let id = ClientIdentity::generate(device).unwrap();
        let uid = id.unique_id.clone();
        let salt = random_bytes::<16>().unwrap();

        let pin_thread = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(800));
            submit_pin_via_curl(pin);
        });

        let client = PairingClient::new("127.0.0.1", 47989, id);
        let result = client.pair(&salt, pin);
        let _ = pin_thread.join();
        result.expect("full pairing ladder");
        println!("pairing ladder OK for uid={uid}");

        // The host must now list us as a trusted client.
        let clients = curl(&["https://localhost:47990/api/clients/list"]);
        println!("clients/list = {clients}");
        assert!(
            clients.contains(device),
            "host should list '{device}' as paired; got: {clients}"
        );
    }
}
