// SPDX-License-Identifier: Apache-2.0
//! App list & session launch — docs/protocol/04-applist-and-launch.md.
//! Derived from protocol observation against Sunshine 2026.516.143833; the launch
//! parameters + response shape are validated by a live session starting. Clean-room.
//!
//! [`PairedClient`] is the authenticated (mTLS) client: once paired (F2/F3) it
//! does `/serverinfo`, `/applist`, `/launch`, `/resume`, `/cancel`.

use std::time::Duration;

use crate::hex;
use crate::https::HttpsClient;
use crate::pairing::crypto::random_bytes;
use crate::serverinfo::ServerInfo;

/// A launchable app from `/applist`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct App {
    pub id: String,
    pub title: String,
    pub hdr_supported: bool,
}

/// The streaming config carried into `/launch` (the `mode` + media options).
/// Resolution/FPS/HDR are the *client's* choice, bounded by the host's
/// `MaxLumaPixelsHEVC` (docs/protocol/03) — Sunshine does not advertise them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchConfig {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub hdr: bool,
    /// `surroundAudioInfo` (channel layout encoding). `0x00060003` ≈ stereo here;
    /// confirm per layout in F8 audio. [CAPTURE-LOCKED]
    pub surround_audio_info: u32,
}

impl Default for LaunchConfig {
    fn default() -> Self {
        Self {
            width: 1920,
            height: 1080,
            fps: 60,
            hdr: false,
            surround_audio_info: 196610, // stereo (validated live)
        }
    }
}

/// A launched (or resumed) session. `rtsp_url` (from `sessionUrl0`) feeds RTSP
/// setup (F5); `rikey`/`rikey_id` are the session crypto material (the RI key)
/// the control/input/video planes consume (F6+).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    pub rtsp_url: String,
    pub game_session: bool,
    pub rikey: [u8; 16],
    pub rikey_id: i32,
}

/// Authenticated GameStream client over mTLS for a single paired host.
pub struct PairedClient {
    https: HttpsClient,
    host: String,
    https_port: u16,
    uniqueid: String,
    timeout: Duration,
}

impl PairedClient {
    pub fn new(https: HttpsClient, host: &str, https_port: u16, uniqueid: &str) -> Self {
        Self {
            https,
            host: host.to_string(),
            https_port,
            uniqueid: uniqueid.to_string(),
            timeout: Duration::from_secs(15),
        }
    }

    fn get(&self, path: &str) -> crate::Result<crate::discovery::HttpResponse> {
        let resp = self
            .https
            .get(&self.host, self.https_port, path, self.timeout)?;
        if resp.status != 200 {
            return Err(crate::Error::Protocol(format!(
                "{path}: HTTP {}",
                resp.status
            )));
        }
        Ok(resp)
    }

    /// Authenticated `/serverinfo` (reports `PairStatus=1` when paired).
    pub fn server_info(&self) -> crate::Result<ServerInfo> {
        let resp = self.get(&format!("/serverinfo?uniqueid={}", self.uniqueid))?;
        ServerInfo::parse(&resp.body)
    }

    /// `/applist` — the apps the host can launch.
    pub fn applist(&self) -> crate::Result<Vec<App>> {
        let resp = self.get(&format!("/applist?uniqueid={}", self.uniqueid))?;
        parse_applist(&resp.body)
    }

    /// `/launch` — start a fresh session for `app_id`. Generates the RI key/IV and
    /// returns the session (RTSP URL + the crypto material to reuse downstream).
    pub fn launch(&self, app_id: &str, config: &LaunchConfig) -> crate::Result<Session> {
        let rikey: [u8; 16] = random_bytes()?;
        let rikey_id = i32::from_le_bytes(random_bytes::<4>()?);
        let path = launch_query("/launch", &self.uniqueid, app_id, config, &rikey, rikey_id);
        let resp = self.get(&path)?;
        parse_session(&resp.body, rikey, rikey_id)
    }

    /// `/resume` — rejoin a session already running on the host.
    pub fn resume(&self, config: &LaunchConfig) -> crate::Result<Session> {
        let rikey: [u8; 16] = random_bytes()?;
        let rikey_id = i32::from_le_bytes(random_bytes::<4>()?);
        // /resume omits appid; same crypto/mode params otherwise.
        let path = launch_query("/resume", &self.uniqueid, "", config, &rikey, rikey_id);
        let resp = self.get(&path)?;
        parse_session(&resp.body, rikey, rikey_id)
    }

    /// `/cancel` — terminate the running session.
    pub fn cancel(&self) -> crate::Result<()> {
        self.get(&format!("/cancel?uniqueid={}", self.uniqueid))?;
        Ok(())
    }
}

fn launch_query(
    endpoint: &str,
    uniqueid: &str,
    app_id: &str,
    config: &LaunchConfig,
    rikey: &[u8; 16],
    rikey_id: i32,
) -> String {
    let appid = if app_id.is_empty() {
        String::new()
    } else {
        format!("&appid={app_id}")
    };
    format!(
        "{endpoint}?uniqueid={uniqueid}{appid}&mode={w}x{h}x{fps}\
         &additionalStates=1&sops=0&rikey={rikey}&rikeyid={rikey_id}\
         &localAudioPlayMode=0&surroundAudioInfo={surround}\
         &remoteControllersBitmap=0&gcmap=0&hdrMode={hdr}",
        w = config.width,
        h = config.height,
        fps = config.fps,
        rikey = hex::encode(rikey),
        surround = config.surround_audio_info,
        hdr = u8::from(config.hdr),
    )
}

/// Parse a `/launch` or `/resume` response into a [`Session`].
fn parse_session(xml: &[u8], rikey: [u8; 16], rikey_id: i32) -> crate::Result<Session> {
    let f = crate::xml::parse_flat(xml)?;
    if f.status_code != Some(200) {
        return Err(crate::Error::Protocol(format!(
            "launch failed: status {:?} {}",
            f.status_code,
            f.status_message.as_deref().unwrap_or("")
        )));
    }
    let rtsp_url = f
        .get("sessionUrl0")
        .ok_or_else(|| crate::Error::Protocol("launch: no <sessionUrl0> in response".into()))?
        .to_string();
    Ok(Session {
        rtsp_url,
        game_session: f.get("gamesession") == Some("1"),
        rikey,
        rikey_id,
    })
}

/// Parse `/applist` — a `<root>` of repeated `<App>` records.
pub fn parse_applist(xml: &[u8]) -> crate::Result<Vec<App>> {
    use quick_xml::events::Event;
    use quick_xml::Reader;

    let mut reader = Reader::from_reader(xml);
    let mut buf = Vec::new();
    let mut apps = Vec::new();
    let mut in_app = false;
    let mut cur_tag: Option<String> = None;
    let mut title = String::new();
    let mut id = String::new();
    let mut hdr = false;

    loop {
        match reader
            .read_event_into(&mut buf)
            .map_err(|e| crate::Error::Protocol(format!("/applist XML: {e}")))?
        {
            Event::Start(e) => {
                let name = crate::xml::local_name(e.name().into_inner());
                if name == "App" {
                    in_app = true;
                    title.clear();
                    id.clear();
                    hdr = false;
                }
                cur_tag = Some(name);
            }
            Event::Text(e) if in_app => {
                let text = e
                    .unescape()
                    .map_err(|e| crate::Error::Protocol(format!("/applist text: {e}")))?;
                let t = text.trim();
                if !t.is_empty() {
                    match cur_tag.as_deref() {
                        Some("AppTitle") => title = t.to_string(),
                        Some("ID") => id = t.to_string(),
                        Some("IsHdrSupported") => hdr = t == "1",
                        _ => {}
                    }
                }
            }
            Event::End(e) => {
                if crate::xml::local_name(e.name().into_inner()) == "App" {
                    in_app = false;
                    if !id.is_empty() {
                        apps.push(App {
                            id: id.clone(),
                            title: title.clone(),
                            hdr_supported: hdr,
                        });
                    }
                }
                cur_tag = None;
            }
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }
    Ok(apps)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn load(name: &str) -> Vec<u8> {
        let path = format!("{}/../../tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
        std::fs::read(path).expect("read fixture")
    }

    #[test]
    fn parses_real_applist_fixture() {
        let apps = parse_applist(&load("applist/desktop-steam.bin")).unwrap();
        assert_eq!(apps.len(), 2);
        assert_eq!(apps[0].title, "Desktop");
        assert_eq!(apps[0].id, "881448767");
        assert!(apps[0].hdr_supported);
        assert_eq!(apps[1].title, "Steam Big Picture");
        assert_eq!(apps[1].id, "1093255277");
    }

    #[test]
    fn parses_real_launch_session_fixture() {
        let s = parse_session(&load("launch/desktop-session.bin"), [9u8; 16], 42).unwrap();
        assert_eq!(s.rtsp_url, "rtsp://127.0.0.1:48010");
        assert!(s.game_session);
        assert_eq!(s.rikey, [9u8; 16]);
        assert_eq!(s.rikey_id, 42);
    }

    #[test]
    fn launch_failure_is_an_error() {
        let xml = br#"<root status_code="404" status_message="Failed to start the specified application"><gamesession>0</gamesession></root>"#;
        assert!(parse_session(xml, [0u8; 16], 0).is_err());
    }

    #[test]
    fn launch_query_has_expected_shape() {
        let q = launch_query(
            "/launch",
            "ABCD",
            "881448767",
            &LaunchConfig::default(),
            &[0xab; 16],
            12345,
        );
        assert!(q.starts_with("/launch?uniqueid=ABCD&appid=881448767&mode=1920x1080x60"));
        assert!(q.contains("&rikey=abababababababababababababababab"));
        assert!(q.contains("&rikeyid=12345"));
        assert!(q.contains("&hdrMode=0"));
    }
}
