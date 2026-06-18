// SPDX-License-Identifier: Apache-2.0
//! RTSP stream setup — docs/protocol/05-rtsp.md.
//! Derived from protocol observation against Sunshine 2026.516.143833. Clean-room.
//!
//! Sunshine's RTSP is a customized text dialect over TCP 48010 (plaintext) with
//! two confirmed quirks:
//!   - **one request per TCP connection** — the host closes after each response,
//!     so [`RtspClient`] opens a fresh socket per request (CSeq still increments);
//!   - **no `Content-Length`** on responses — the body runs to connection close,
//!     so we read to EOF.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

/// Per-stream UDP server ports learned from SETUP.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct StreamSetup {
    pub video_port: u16,
    pub audio_port: u16,
    pub control_port: u16,
}

/// Capability/encryption flags parsed from the DESCRIBE SDP (Sunshine's custom
/// `a=x-ss-general.*` attributes). Bit meanings of the encryption fields are
/// resolved with the media planes (F6/F7). [CAPTURE-LOCKED]
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SdpInfo {
    pub feature_flags: u32,
    pub encryption_supported: u32,
    pub encryption_requested: u32,
    /// `a=fmtp:97 surround-params=…` values (audio channel layouts).
    pub surround_params: Vec<String>,
}

impl SdpInfo {
    /// Parse the (non-standard, attribute-only) DESCRIBE SDP.
    pub fn parse(sdp: &[u8]) -> Self {
        let text = String::from_utf8_lossy(sdp);
        let mut out = SdpInfo::default();
        for line in text.lines() {
            let line = line.trim();
            if let Some(v) = line.strip_prefix("a=x-ss-general.featureFlags:") {
                out.feature_flags = v.trim().parse().unwrap_or(0);
            } else if let Some(v) = line.strip_prefix("a=x-ss-general.encryptionSupported:") {
                out.encryption_supported = v.trim().parse().unwrap_or(0);
            } else if let Some(v) = line.strip_prefix("a=x-ss-general.encryptionRequested:") {
                out.encryption_requested = v.trim().parse().unwrap_or(0);
            } else if let Some(v) = line.strip_prefix("a=fmtp:97 surround-params=") {
                out.surround_params.push(v.trim().to_string());
            }
        }
        out
    }

    /// Whether the host requests media encryption at all (any bit set).
    pub fn encryption_required(&self) -> bool {
        self.encryption_requested != 0
    }
}

/// The result of the RTSP handshake — everything the media planes (F6/F7/F8)
/// need to bind sockets and start streaming.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RtspSession {
    pub session_id: String,
    pub ports: StreamSetup,
    /// `X-SS-Ping-Payload` (decoded) — the bytes the client sends to each media
    /// UDP port to open the return path.
    pub ping_payload: Vec<u8>,
    /// `X-SS-Connect-Data` — the ENet control-channel connect token.
    pub control_connect_data: u32,
    pub sdp: SdpInfo,
}

/// A parsed RTSP response.
#[derive(Debug, Clone)]
pub struct RtspResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl RtspResponse {
    /// Case-insensitive header lookup.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// An RTSP client for one host. Each request opens a fresh connection (Sunshine
/// closes after every response); the CSeq counter spans the whole exchange.
pub struct RtspClient {
    host: String,
    port: u16,
    target: String,
    cseq: u32,
    timeout: Duration,
}

impl RtspClient {
    /// Build from an `rtsp://host:port` URL (the `sessionUrl0` from `/launch`).
    pub fn new(rtsp_url: &str, timeout: Duration) -> crate::Result<Self> {
        let rest = rtsp_url
            .strip_prefix("rtsp://")
            .ok_or_else(|| crate::Error::Protocol(format!("not an rtsp URL: {rtsp_url}")))?;
        let host_port = rest.split('/').next().unwrap_or(rest);
        let (host, port) = match host_port.rsplit_once(':') {
            Some((h, p)) => (
                h.to_string(),
                p.parse()
                    .map_err(|_| crate::Error::Protocol(format!("bad RTSP port in {rtsp_url}")))?,
            ),
            None => (host_port.to_string(), 48010u16),
        };
        Ok(Self {
            target: format!("rtsp://{host}:{port}"),
            host,
            port,
            cseq: 0,
            timeout,
        })
    }

    /// The base request URI (`rtsp://host:port`).
    pub fn target(&self) -> &str {
        &self.target
    }

    /// The next CSeq that will be used (1-based).
    pub fn next_cseq(&self) -> u32 {
        self.cseq + 1
    }

    /// Send one request on a fresh connection and read the response.
    pub fn request(
        &mut self,
        method: &str,
        uri: Option<&str>,
        extra_headers: &[(&str, &str)],
        body: &[u8],
    ) -> crate::Result<RtspResponse> {
        self.cseq += 1;
        let uri = uri.unwrap_or(&self.target).to_string();
        let mut req = format!("{method} {uri} RTSP/1.0\r\nCSeq: {}\r\n", self.cseq);
        for (k, v) in extra_headers {
            req.push_str(&format!("{k}: {v}\r\n"));
        }
        if !body.is_empty() {
            req.push_str(&format!("Content-Length: {}\r\n", body.len()));
        }
        req.push_str("\r\n");

        let mut stream = TcpStream::connect((self.host.as_str(), self.port))?;
        stream.set_read_timeout(Some(self.timeout))?;
        stream.set_write_timeout(Some(self.timeout))?;
        stream.write_all(req.as_bytes())?;
        if !body.is_empty() {
            stream.write_all(body)?;
        }
        read_response(&mut stream)
    }

    /// Walk the full RTSP handshake — `OPTIONS → DESCRIBE → SETUP×3 → PLAY` — and
    /// return the session binding. Validated live against Sunshine; ANNOUNCE is
    /// not required (the host uses the `/launch` config). After this the host is
    /// ready to stream once the client pings the media ports.
    pub fn handshake(&mut self) -> crate::Result<RtspSession> {
        const GS: (&str, &str) = ("X-GS-ClientVersion", "14");
        let target = self.target.clone();

        self.request("OPTIONS", None, &[GS], b"")?;

        let describe = self.request("DESCRIBE", None, &[GS], b"")?;
        let sdp = SdpInfo::parse(&describe.body);

        let mut ports = StreamSetup::default();
        let mut session_id = String::new();
        let mut ping_payload = Vec::new();
        let mut control_connect_data = 0u32;

        for stream in ["audio", "video", "control"] {
            let uri = format!("{target}/streamid={stream}");
            let resp = self.request(
                "SETUP",
                Some(&uri),
                &[("Transport", "unicast;X-GS-ClientPort=50000-50001"), GS],
                b"",
            )?;
            if resp.status != 200 {
                return Err(crate::Error::Protocol(format!(
                    "SETUP {stream}: RTSP {}",
                    resp.status
                )));
            }
            let port = parse_server_port(resp.header("Transport"))?;
            match stream {
                "audio" => ports.audio_port = port,
                "video" => ports.video_port = port,
                _ => ports.control_port = port,
            }
            if let Some(s) = resp.header("Session") {
                session_id = s.split(';').next().unwrap_or(s).trim().to_string();
            }
            if let Some(p) = resp.header("X-SS-Ping-Payload") {
                ping_payload = crate::hex::decode(p).unwrap_or_default();
            }
            if let Some(c) = resp.header("X-SS-Connect-Data") {
                control_connect_data = c.trim().parse().unwrap_or(0);
            }
        }

        let play = self.request("PLAY", None, &[("Session", session_id.as_str()), GS], b"")?;
        if play.status != 200 {
            return Err(crate::Error::Protocol(format!(
                "PLAY: RTSP {}",
                play.status
            )));
        }

        Ok(RtspSession {
            session_id,
            ports,
            ping_payload,
            control_connect_data,
            sdp,
        })
    }
}

/// Parse `server_port=<n>` from an RTSP `Transport` header.
fn parse_server_port(transport: Option<&str>) -> crate::Result<u16> {
    let t = transport.ok_or_else(|| crate::Error::Protocol("SETUP: no Transport header".into()))?;
    t.split(';')
        .find_map(|p| p.trim().strip_prefix("server_port="))
        .and_then(|v| v.trim().parse().ok())
        .ok_or_else(|| crate::Error::Protocol(format!("SETUP: no server_port in {t:?}")))
}

fn read_response(stream: &mut TcpStream) -> crate::Result<RtspResponse> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];

    let header_end = loop {
        if let Some(pos) = find_subslice(&buf, b"\r\n\r\n") {
            break pos + 4;
        }
        let n = stream.read(&mut tmp)?;
        if n == 0 {
            return Err(crate::Error::Protocol(
                "RTSP: connection closed before headers".into(),
            ));
        }
        buf.extend_from_slice(&tmp[..n]);
    };

    let header_text = String::from_utf8_lossy(&buf[..header_end]).into_owned();
    let mut lines = header_text.split("\r\n");
    let status_line = lines.next().unwrap_or("");
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| crate::Error::Protocol(format!("RTSP: bad status line {status_line:?}")))?;

    let mut headers = Vec::new();
    let mut content_length: Option<usize> = None;
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            let (k, v) = (k.trim().to_string(), v.trim().to_string());
            if k.eq_ignore_ascii_case("Content-Length") {
                content_length = v.parse().ok();
            }
            headers.push((k, v));
        }
    }

    // Sunshine omits Content-Length and closes the connection, so when it is
    // absent we read until EOF; when present we read exactly that many bytes.
    let mut body = buf[header_end..].to_vec();
    loop {
        if let Some(len) = content_length {
            if body.len() >= len {
                break;
            }
        }
        let n = stream.read(&mut tmp)?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&tmp[..n]);
    }
    Ok(RtspResponse {
        status,
        headers,
        body,
    })
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn new_rejects_non_rtsp_url() {
        assert!(RtspClient::new("http://x", Duration::from_secs(1)).is_err());
    }

    #[test]
    fn new_parses_host_port() {
        let c = RtspClient::new("rtsp://127.0.0.1:48010", Duration::from_secs(1)).unwrap();
        assert_eq!(c.target(), "rtsp://127.0.0.1:48010");
        assert_eq!(c.next_cseq(), 1);
    }

    #[test]
    fn parse_server_port_from_transport() {
        assert_eq!(parse_server_port(Some("server_port=48000")).unwrap(), 48000);
        assert_eq!(
            parse_server_port(Some("unicast;server_port=47998")).unwrap(),
            47998
        );
        assert!(parse_server_port(None).is_err());
        assert!(parse_server_port(Some("unicast")).is_err());
    }

    /// Golden test: the real captured DESCRIBE SDP parses to the expected flags.
    #[test]
    fn parses_real_describe_sdp_fixture() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../tests/fixtures/rtsp/describe-sdp.bin"
        );
        let fx = starfire_testkit::Fixture::load(path).expect("load sdp fixture");
        assert_eq!(fx.meta.layer, "rtsp/describe-sdp");

        let sdp = SdpInfo::parse(&fx.bytes);
        assert_eq!(sdp.feature_flags, 3);
        assert_eq!(sdp.encryption_supported, 5);
        assert_eq!(sdp.encryption_requested, 1);
        assert!(sdp.encryption_required());
        assert_eq!(sdp.surround_params.len(), 6);
        assert_eq!(sdp.surround_params[0], "21101");
    }
}
