// SPDX-License-Identifier: Apache-2.0
//! The `/pair` ladder — docs/protocol/02-pairing-and-crypto.md §2.
//! HTTP GETs to `/pair` on 47989 carrying the cert, salt, and challenge blobs.
//! Derived from the public GameStream pairing protocol; each phase is validated
//! by the live host's accept/reject. Clean-room.
//!
//! Built bottom-up: phase 1 (`getservercert`, no PIN) lands first to confirm the
//! host accepts our cert; the PIN-keyed phases follow.

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

    /// Phase 5 — `pairchallenge` over **mTLS** (HTTPS). Connects with the cert
    /// just added to the host's trust set, finalizing the pairing. Only after
    /// this does the host treat us as paired for authenticated requests.
    pub fn pair_challenge(
        &self,
        https: &crate::https::HttpsClient,
        https_port: u16,
    ) -> crate::Result<()> {
        let path = format!(
            "/pair?uniqueid={uid}&phrase=pairchallenge",
            uid = self.identity.unique_id
        );
        let resp = https.get(&self.host, https_port, &path, self.timeout)?;
        if resp.status != 200 {
            return Err(crate::Error::Protocol(format!(
                "pairchallenge HTTP {}",
                resp.status
            )));
        }
        let r = parse_pair_response(&resp.body)?;
        if !r.paired {
            return Err(crate::Error::Protocol(
                "pairchallenge: paired=0 (mTLS finalization rejected)".into(),
            ));
        }
        Ok(())
    }

    /// Run the full HTTP ladder (phases 1–4). The PIN must be entered on the host
    /// out of band (auto-PIN) — `get_server_cert` blocks until it is. On success
    /// the host has added our cert to its trusted set; returns the host's cert
    /// (PEM) so the caller can pin it for subsequent mTLS.
    pub fn pair(&self, salt: &[u8; 16], pin: &str) -> crate::Result<Vec<u8>> {
        // Phase 1 — blocks until the PIN is entered on the host.
        let server_cert = self.get_server_cert(salt)?;

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
        Ok(server_cert)
    }
}

/// Parse a `/pair` response envelope, hex-decoding the binary fields.
pub fn parse_pair_response(xml: &[u8]) -> crate::Result<PairResponse> {
    let f = crate::xml::parse_flat(xml)?;
    let decode = |k: &str| -> crate::Result<Option<Vec<u8>>> {
        match f.get(k) {
            Some(v) => {
                Ok(Some(hex::decode(v).map_err(|e| {
                    crate::Error::Protocol(format!("/pair {k}: {e}"))
                })?))
            }
            None => Ok(None),
        }
    };
    Ok(PairResponse {
        status_code: f.status_code,
        paired: f.get("paired") == Some("1"),
        plaincert: decode("plaincert")?,
        challenge_response: decode("challengeresponse")?,
        pairing_secret: decode("pairingsecret")?,
    })
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
        let url = web_url("/api/pin");
        let _ = std::process::Command::new("curl")
            .args([
                "-sk",
                "--max-time",
                "8",
                "-u",
                "starfire:starfire-test-1",
                "-X",
                "POST",
                &url,
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

    /// Sunshine web-API URL on the stream host (same host as `test_host`).
    fn web_url(path: &str) -> String {
        format!("https://{}:47990{path}", test_host())
    }

    /// Reset the host's trusted-clients list (idempotent test setup).
    fn unpair_all() {
        curl(&["-X", "POST", web_url("/api/clients/unpair-all").as_str()]);
    }

    /// Live full pairing (phases 1–4), verified via Sunshine's trusted-clients
    /// API (over plain HTTP the host can't identify us, so `PairStatus` is always
    /// 0 — the clients list is the real signal). Resets pairing state first so it
    /// is idempotent. Run with `-- --ignored live_pair_full --nocapture`.
    #[test]
    #[ignore = "requires a running Sunshine host + web API on 47989/47990"]
    fn live_pair_full() {
        // Clean slate so accumulated pending pairings don't block getservercert.
        unpair_all();

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
        let host_cert = result.expect("full pairing ladder");
        println!(
            "pairing ladder OK for uid={uid}, host cert {} bytes",
            host_cert.len()
        );

        // The host must now list us as a trusted client.
        let clients = curl(&[web_url("/api/clients/list").as_str()]);
        println!("clients/list = {clients}");
        assert!(
            clients.contains(device),
            "host should list '{device}' as paired; got: {clients}"
        );
    }

    /// Live F3: pair, then query `/serverinfo` over **mTLS** with the same
    /// identity. `PairStatus=1` proves the host identified our client cert (only
    /// possible over mTLS). Also dumps the richer authenticated field set.
    /// Run with `-- --ignored live_serverinfo_https --nocapture`.
    #[test]
    #[ignore = "requires a running Sunshine host + web API on 47984/47989/47990"]
    fn live_serverinfo_https() {
        use crate::https::HttpsClient;
        use crate::serverinfo::ServerInfo;

        unpair_all();

        let pin = "1234";
        let id = ClientIdentity::generate("starfire-test").unwrap();
        let cert_pem = id.cert_pem.clone();
        let key_pem = id.key_pem.clone();
        let salt = random_bytes::<16>().unwrap();

        let pin_thread = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(800));
            submit_pin_via_curl(pin);
        });
        let client = PairingClient::new("127.0.0.1", 47989, id);
        let host_cert_pem = client.pair(&salt, pin).expect("pair");
        let _ = pin_thread.join();

        // Phase 5 — finalize over mTLS (host cert pinned), then authenticated
        // /serverinfo. Pinning the cert we learned at pairing is the trust model.
        let host_der = crate::https::cert_pem_to_der(&host_cert_pem).expect("host cert DER");
        let https = HttpsClient::new(&cert_pem, &key_pem, Some(host_der)).expect("https client");
        client.pair_challenge(&https, 47984).expect("pairchallenge");

        // Pairing state is keyed by uniqueid; include it on the serverinfo query.
        let path = format!("/serverinfo?uniqueid={}", client.identity.unique_id);
        let resp = https
            .get("127.0.0.1", 47984, &path, Duration::from_secs(5))
            .expect("https serverinfo");
        assert_eq!(resp.status, 200);
        let si = ServerInfo::parse(&resp.body).expect("parse https serverinfo");

        println!("HTTPS serverinfo PairStatus = {:?}", si.pair_status);
        let mut keys: Vec<&String> = si.fields.keys().collect();
        keys.sort();
        println!("authenticated fields ({}): {keys:?}", keys.len());
        assert_eq!(
            si.pair_status,
            Some(1),
            "mTLS /serverinfo should report paired"
        );
    }

    /// F4 exploration: pair, then dump the real `/applist` and a `/launch`
    /// attempt so we can read the exact XML + params. Prints, asserts little.
    /// Run with `-- --ignored live_explore_applist_launch --nocapture`.
    #[test]
    #[ignore = "requires a running Sunshine host + web API on 47984/47989/47990"]
    fn live_explore_applist_launch() {
        use crate::https::{cert_pem_to_der, HttpsClient};

        unpair_all();
        let pin = "1234";
        let id = ClientIdentity::generate("starfire-test").unwrap();
        let cert = id.cert_pem.clone();
        let key = id.key_pem.clone();
        let salt = random_bytes::<16>().unwrap();
        let t = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(800));
            submit_pin_via_curl(pin);
        });
        let client = PairingClient::new("127.0.0.1", 47989, id);
        let host_pem = client.pair(&salt, pin).expect("pair");
        let _ = t.join();
        let der = cert_pem_to_der(&host_pem).unwrap();
        let https = HttpsClient::new(&cert, &key, Some(der)).unwrap();
        client.pair_challenge(&https, 47984).unwrap();
        let uid = client.identity.unique_id.clone();

        let applist = https
            .get(
                "127.0.0.1",
                47984,
                &format!("/applist?uniqueid={uid}"),
                Duration::from_secs(5),
            )
            .unwrap();
        println!("===== APPLIST ({}) =====", applist.status);
        println!("{}", String::from_utf8_lossy(&applist.body));

        // Parse the first App's <ID> out of the applist body.
        let body = String::from_utf8_lossy(&applist.body);
        let appid = body
            .split("<ID>")
            .nth(1)
            .and_then(|s| s.split("</ID>").next())
            .unwrap_or("0")
            .to_string();
        println!("using appid={appid}");

        let rikey = crate::hex::encode(&random_bytes::<16>().unwrap());
        let launch_path = format!(
            "/launch?uniqueid={uid}&appid={appid}&mode=1920x1080x60&additionalStates=1&sops=0\
             &rikey={rikey}&rikeyid=12345&localAudioPlayMode=0&surroundAudioInfo=196610\
             &remoteControllersBitmap=0&gcmap=0&hdrMode=0"
        );
        let launch = https
            .get("127.0.0.1", 47984, &launch_path, Duration::from_secs(15))
            .unwrap();
        println!("===== LAUNCH ({}) =====", launch.status);
        println!("{}", String::from_utf8_lossy(&launch.body));

        // Tidy up so we don't leave a session running.
        let cancel = https
            .get(
                "127.0.0.1",
                47984,
                &format!("/cancel?uniqueid={uid}"),
                Duration::from_secs(5),
            )
            .map(|r| r.status);
        println!("===== CANCEL = {cancel:?} =====");
    }

    /// Live F4: pair, then drive the real `PairedClient` — applist, launch (assert
    /// an RTSP URL comes back), cancel. Run with
    /// `-- --ignored live_launch --nocapture`.
    #[test]
    #[ignore = "requires a running Sunshine host + web API on 47984/47989/47990"]
    fn live_launch() {
        use crate::https::{cert_pem_to_der, HttpsClient};
        use crate::launch::{LaunchConfig, PairedClient};

        unpair_all();
        let pin = "1234";
        let id = ClientIdentity::generate("starfire-test").unwrap();
        let cert = id.cert_pem.clone();
        let key = id.key_pem.clone();
        let salt = random_bytes::<16>().unwrap();
        let t = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(800));
            submit_pin_via_curl(pin);
        });
        let pairing = PairingClient::new("127.0.0.1", 47989, id);
        let host_pem = pairing.pair(&salt, pin).expect("pair");
        let _ = t.join();
        let der = cert_pem_to_der(&host_pem).unwrap();
        let https = HttpsClient::new(&cert, &key, Some(der)).unwrap();
        pairing.pair_challenge(&https, 47984).unwrap();
        let uid = pairing.identity.unique_id.clone();

        let client = PairedClient::new(https, "127.0.0.1", 47984, &uid);
        assert_eq!(client.server_info().unwrap().pair_status, Some(1));

        let apps = client.applist().expect("applist");
        println!("apps: {apps:?}");
        let desktop = apps
            .iter()
            .find(|a| a.title == "Desktop")
            .expect("Desktop app");

        let session = client
            .launch(&desktop.id, &LaunchConfig::default())
            .expect("launch");
        println!("session: {session:?}");
        assert!(session.rtsp_url.starts_with("rtsp://"));
        assert!(session.game_session);

        client.cancel().expect("cancel");
    }

    /// Host to run the live flow against. Defaults to loopback, but the data
    /// plane (encoder + media UDP ports) does NOT come up for a 127.0.0.1 client
    /// (Sunshine can't ARP a MAC for loopback), so the media tests set
    /// `STARFIRE_TEST_HOST=<LAN IP>`.
    fn test_host() -> String {
        std::env::var("STARFIRE_TEST_HOST").unwrap_or_else(|_| "127.0.0.1".to_string())
    }

    /// Pair + finalize against the test host, returning an authenticated client.
    /// Shared by the F4/F5/F6 live tests.
    fn pair_and_finalize() -> crate::launch::PairedClient {
        use crate::https::{cert_pem_to_der, HttpsClient};
        use crate::launch::PairedClient;

        let host = test_host();
        unpair_all();
        let pin = "1234";
        let id = ClientIdentity::generate("starfire-test").unwrap();
        let cert = id.cert_pem.clone();
        let key = id.key_pem.clone();
        let salt = random_bytes::<16>().unwrap();
        let t = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(800));
            submit_pin_via_curl(pin);
        });
        let pairing = PairingClient::new(&host, 47989, id);
        let host_pem = pairing.pair(&salt, pin).expect("pair");
        let _ = t.join();
        let der = cert_pem_to_der(&host_pem).unwrap();
        let https = HttpsClient::new(&cert, &key, Some(der)).unwrap();
        pairing.pair_challenge(&https, 47984).unwrap();
        let uid = pairing.identity.unique_id.clone();
        PairedClient::new(https, &host, 47984, &uid)
    }

    /// Live F5: pair, launch, then run the full RTSP handshake and assert the
    /// stream ports come back. Captures the DESCRIBE SDP fixture. Run with
    /// `-- --ignored live_rtsp_handshake --nocapture`.
    #[test]
    #[ignore = "requires a running Sunshine host + web API"]
    fn live_rtsp_handshake() {
        use crate::launch::LaunchConfig;
        use crate::rtsp::RtspClient;

        let client = pair_and_finalize();
        let apps = client.applist().expect("applist");
        let desktop = apps.iter().find(|a| a.title == "Desktop").expect("Desktop");
        let session = client
            .launch(&desktop.id, &LaunchConfig::default())
            .expect("launch");
        println!("rtsp url: {}", session.rtsp_url);

        let mut rtsp = RtspClient::new(&session.rtsp_url, Duration::from_secs(10)).expect("client");

        // Capture the DESCRIBE SDP for the golden test before the full handshake.
        let desc = rtsp
            .request("DESCRIBE", None, &[("X-GS-ClientVersion", "14")], b"")
            .expect("DESCRIBE");
        let base = format!("{}/../../tests/fixtures", env!("CARGO_MANIFEST_DIR"));
        std::fs::create_dir_all(format!("{base}/rtsp")).ok();
        std::fs::write(format!("{base}/rtsp/describe-sdp.bin"), &desc.body).unwrap();

        // Fresh client (CSeq restarts) for the real handshake.
        let mut rtsp =
            RtspClient::new(&session.rtsp_url, Duration::from_secs(10)).expect("client2");
        let rs = rtsp.handshake(&crate::rtsp::AnnounceConfig::default()).expect("rtsp handshake");
        println!("session: {rs:?}");
        assert_eq!(rs.ports.video_port, 47998);
        assert_eq!(rs.ports.audio_port, 48000);
        assert_eq!(rs.ports.control_port, 47999);
        assert!(!rs.session_id.is_empty());
        assert!(!rs.ping_payload.is_empty());
        assert!(rs.sdp.encryption_required());

        client.cancel().ok();
    }

    /// Hold a session open (pinging media ports) so the bound UDP ports can be
    /// inspected externally (netstat). Run with `-- --ignored live_hold_session`.
    #[test]
    #[ignore = "diagnostic: holds a live session ~25s"]
    fn live_hold_session() {
        use crate::launch::LaunchConfig;
        use crate::rtsp::RtspClient;
        use std::net::UdpSocket;
        use std::time::Instant;

        let client = pair_and_finalize();
        let apps = client.applist().expect("applist");
        let desktop = apps.iter().find(|a| a.title == "Desktop").expect("Desktop");
        let session = client
            .launch(&desktop.id, &LaunchConfig::default())
            .expect("launch");
        let mut rtsp = RtspClient::new(&session.rtsp_url, Duration::from_secs(10)).expect("client");
        let rs = rtsp.handshake(&crate::rtsp::AnnounceConfig::default()).expect("handshake");
        println!(
            "SESSION HELD: video={} control={} audio={} — holding 25s",
            rs.ports.video_port, rs.ports.control_port, rs.ports.audio_port
        );

        let host = test_host();
        // Ping from the client port advertised to the host in RTSP SETUP
        // (X-GS-ClientPort=50000-50001), so the host associates these pings.
        let punch = UdpSocket::bind("0.0.0.0:50000")
            .or_else(|_| UdpSocket::bind("0.0.0.0:0"))
            .unwrap();
        println!(
            "ping from local port {}",
            punch.local_addr().unwrap().port()
        );
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(25) {
            for p in [
                rs.ports.video_port,
                rs.ports.audio_port,
                rs.ports.control_port,
            ] {
                let _ = punch.send_to(&rs.ping_payload, format!("{host}:{p}"));
            }
            std::thread::sleep(Duration::from_millis(500));
        }
        client.cancel().ok();
    }

    /// F6+F7 full data plane: pair → launch → RTSP handshake (arms the session)
    /// → ENet control connect (this is what sets the host's `localAddress`, the
    /// source for RTP sends — without it every send fails WSAEINVAL) → ping the
    /// media ports with the legacy `"PING"` → receive plaintext HEVC video.
    /// Run with `STARFIRE_TEST_HOST=<lan-ip> -- --ignored live_explore_video --nocapture`.
    #[test]
    #[ignore = "requires a running Sunshine host reachable by LAN IP"]
    fn live_explore_video() {
        use crate::control::ControlChannel;
        use crate::launch::LaunchConfig;
        use crate::rtsp::RtspClient;
        use std::net::UdpSocket;
        use std::time::Instant;

        let host = test_host();
        let client = pair_and_finalize();
        let apps = client.applist().expect("applist");
        let desktop = apps.iter().find(|a| a.title == "Desktop").expect("Desktop");
        let session = client
            .launch(&desktop.id, &LaunchConfig::default())
            .expect("launch");
        let mut rtsp = RtspClient::new(&session.rtsp_url, Duration::from_secs(10)).expect("client");
        let rs = rtsp.handshake(&crate::rtsp::AnnounceConfig::default()).expect("handshake");
        println!(
            "host={host} video={} audio={} control={}",
            rs.ports.video_port, rs.ports.audio_port, rs.ports.control_port
        );

        // Connect the ENet control channel FIRST. The host derives its RTP source
        // address (localAddress) from this control peer; until it connects, the
        // encoder's sends fail with WSAEINVAL. Now that ANNOUNCE armed the
        // session, the connect should complete (it timed out pre-arming).
        let control_server = format!("{host}:{}", rs.ports.control_port).parse().unwrap();
        let mut ctrl = {
            let mut c = None;
            let start = Instant::now();
            while start.elapsed() < Duration::from_secs(6) {
                match ControlChannel::connect(
                    control_server,
                    rs.control_connect_data,
                    1,
                    0,
                    Duration::from_secs(1),
                ) {
                    Ok(ch) => {
                        println!("ENET control connected after {:?}", start.elapsed());
                        c = Some(ch);
                        break;
                    }
                    Err(e) => println!("control connect retry: {e}"),
                }
            }
            c.expect("ENet control connect (session armed but control did not connect)")
        };

        // Ephemeral socket per stream; ping is the legacy literal "PING" (4 bytes)
        // since featureFlags=0 → no ML_FF_SESSION_ID_V1. Both video AND audio must
        // be pinged or the audio timeout tears the session down.
        let video = UdpSocket::bind("0.0.0.0:0").expect("bind video");
        let audio = UdpSocket::bind("0.0.0.0:0").expect("bind audio");
        video
            .set_read_timeout(Some(Duration::from_millis(20)))
            .unwrap();
        let video_server = format!("{host}:{}", rs.ports.video_port);
        let audio_server = format!("{host}:{}", rs.ports.audio_port);

        // Optionally dump received datagrams (u16-LE length prefix + bytes) to a
        // fixture for offline depacketizer development. Capped so it stays small.
        let mut fixture: Option<Vec<u8>> = std::env::var("SF_VIDEO_FIXTURE").ok().map(|_| Vec::new());

        let mut buf = [0u8; 2048];
        let (mut got, mut hevc, mut bytes) = (0usize, false, 0usize);
        let mut last_ping = Instant::now() - Duration::from_secs(1);
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(8) {
            // Keep the control peer alive (drives ENet keepalive/acks).
            let _ = ctrl.poll();
            if last_ping.elapsed() > Duration::from_millis(200) {
                let _ = video.send_to(b"PING", &video_server);
                let _ = audio.send_to(b"PING", &audio_server);
                last_ping = Instant::now();
            }
            if let Ok((n, _)) = video.recv_from(&mut buf) {
                got += 1;
                bytes += n;
                if got <= 5 {
                    println!("RTP {n}B head={:02x?}", &buf[..n.min(40)]);
                }
                // HEVC VPS NAL header (0x40 0x01) appears in plaintext payload.
                if buf[..n].windows(2).any(|w| w == [0x40, 0x01]) {
                    hevc = true;
                }
                if let Some(f) = fixture.as_mut() {
                    if f.len() < 1_500_000 {
                        f.extend_from_slice(&(n as u16).to_le_bytes());
                        f.extend_from_slice(&buf[..n]);
                    }
                }
            }
        }
        if let (Some(f), Ok(path)) = (fixture, std::env::var("SF_VIDEO_FIXTURE")) {
            std::fs::write(&path, &f).expect("write fixture");
            println!("wrote {} bytes of video fixture to {path}", f.len());
        }
        println!("received {got} video packet(s), {bytes} bytes; hevc_seen={hevc}");
        client.cancel().ok();
        assert!(got > 0, "expected video RTP from the host");
    }

    /// F6 exploration: pair → launch → RTSP handshake → ENet connect to the
    /// control port, then poll for incoming (encrypted) control messages.
    /// Run with `-- --ignored live_explore_control --nocapture`.
    #[test]
    #[ignore = "requires a running Sunshine host + web API"]
    fn live_explore_control() {
        use crate::control::{ControlChannel, ControlEvent};
        use crate::launch::LaunchConfig;
        use crate::rtsp::RtspClient;
        use std::net::UdpSocket;
        use std::time::Instant;

        let client = pair_and_finalize();
        let apps = client.applist().expect("applist");
        let desktop = apps.iter().find(|a| a.title == "Desktop").expect("Desktop");
        let session = client
            .launch(&desktop.id, &LaunchConfig::default())
            .expect("launch");
        let mut rtsp = RtspClient::new(&session.rtsp_url, Duration::from_secs(10)).expect("client");
        let rs = rtsp.handshake(&crate::rtsp::AnnounceConfig::default()).expect("handshake");
        println!(
            "control_port={} connect_data={} ping_payload={:02x?}",
            rs.ports.control_port, rs.control_connect_data, rs.ping_payload
        );

        // The encoder + UDP ports take a few hundred ms to bind after PLAY.
        // Use the client ports advertised in SETUP (X-GS-ClientPort=50000-50001):
        // ping from 50000, ENet control from 50001.
        let host = test_host();
        let punch = UdpSocket::bind("0.0.0.0:50000")
            .or_else(|_| UdpSocket::bind("0.0.0.0:0"))
            .unwrap();
        let ping = |to: u16| {
            let _ = punch.send_to(&rs.ping_payload, format!("{host}:{to}"));
        };
        let server = format!("{host}:{}", rs.ports.control_port).parse().unwrap();

        // Retry: ping the media ports + try ENet connect for a few seconds.
        let mut connected = None;
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(8) {
            ping(rs.ports.video_port);
            ping(rs.ports.audio_port);
            match ControlChannel::connect(
                server,
                rs.control_connect_data,
                1,
                50001,
                Duration::from_secs(1),
            ) {
                Ok(c) => {
                    println!("ENET CONNECTED after {:?}", start.elapsed());
                    connected = Some(c);
                    break;
                }
                Err(e) => {
                    println!("connect retry ({:?}): {e}", start.elapsed());
                    std::thread::sleep(Duration::from_millis(400));
                }
            }
        }
        let mut ctrl = connected.expect("enet connect");

        // Poll for ~3s and dump any control messages (for encryption RE).
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut count = 0;
        while Instant::now() < deadline {
            match ctrl.poll().unwrap() {
                Some(ControlEvent::Message { channel, data }) => {
                    count += 1;
                    let head = &data[..data.len().min(48)];
                    println!("MSG ch{channel} len={} head={head:02x?}", data.len());
                }
                Some(other) => println!("EVENT {other:?}"),
                None => std::thread::sleep(Duration::from_millis(2)),
            }
        }
        println!("received {count} control message(s)");
        client.cancel().ok();
    }

    /// Focused F6/F7 arming check: pair → launch → full handshake (which now
    /// includes the ANNOUNCE that arms the session) and assert it reaches PLAY.
    /// The complete data plane (control + video) is covered by `live_explore_video`.
    /// Run with `STARFIRE_TEST_HOST=<lan-ip> -- --ignored live_explore_announce --nocapture`.
    #[test]
    #[ignore = "requires a running Sunshine host"]
    fn live_explore_announce() {
        use crate::launch::LaunchConfig;
        use crate::rtsp::{AnnounceConfig, RtspClient};

        let client = pair_and_finalize();
        let apps = client.applist().expect("applist");
        let desktop = apps.iter().find(|a| a.title == "Desktop").expect("Desktop");
        let session = client
            .launch(&desktop.id, &LaunchConfig::default())
            .expect("launch");

        let mut rtsp = RtspClient::new(&session.rtsp_url, Duration::from_secs(10)).expect("client");
        // handshake() runs OPTIONS/DESCRIBE/SETUP×3/ANNOUNCE/PLAY; ANNOUNCE 200 is
        // required for the host to arm — a missing mandatory SDP attr is a 400.
        let rs = rtsp
            .handshake(&AnnounceConfig::default())
            .expect("handshake (ANNOUNCE must be 200)");
        println!("ARMED: session={} ports={:?}", rs.session_id, rs.ports);
        client.cancel().ok();
    }
}
