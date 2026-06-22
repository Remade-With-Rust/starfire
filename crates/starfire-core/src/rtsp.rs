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

/// Stream parameters carried into the RTSP `ANNOUNCE` SDP. Mirrors the launch
/// `mode`; the values must be self-consistent with what `/launch` was given.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnnounceConfig {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
    /// Encoder slices per frame (`videoEncoderSlicesPerFrame`). >1 lets the host
    /// encode — and us decode — slices in parallel, trading a little coding
    /// efficiency for lower per-frame latency on multi-core. 1 = single slice.
    pub slices_per_frame: u32,
    /// FEC repair overhead percent (`x-nv-vqos[0].fec.repairPercent`). Higher =
    /// more redundancy (survives more loss) at the cost of bandwidth and packet
    /// count; on a clean LAN this can be dialled well below the 50 % default.
    pub fec_percent: u32,
    /// UDP payload size for video packets (`packetSize`). Larger packets mean
    /// fewer packets per frame (less overhead) but risk IP fragmentation above
    /// the path MTU. 1392 is the MTU-safe default.
    pub packet_size: u32,
    /// `x-ss-general.encryptionEnabled` bitmask. 0 = fully plaintext video +
    /// audio + control (only valid when the host's encryption mode is not
    /// MANDATORY for this client, e.g. `lan_encryption_mode = 0`).
    pub encryption_enabled: u32,
}

impl Default for AnnounceConfig {
    fn default() -> Self {
        Self {
            width: 1920,
            height: 1080,
            fps: 60,
            bitrate_kbps: 10_000,
            slices_per_frame: 1,
            fec_percent: 50,
            packet_size: 1392,
            encryption_enabled: 0,
        }
    }
}

/// The result of the RTSP handshake — everything the media planes (F6/F7/F8)
/// need to bind sockets and start streaming.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RtspSession {
    pub session_id: String,
    pub ports: StreamSetup,
    /// `X-SS-Ping-Payload` — the **literal** header string (NOT hex-decoded; the
    /// client sends these exact bytes + a 4-byte BE counter to each media UDP
    /// port to open the return path). Confirmed from a Moonlight capture.
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

        // Send headers + body in ONE write — Sunshine's RTSP reads the request in
        // a single pass, so a separate body write lands in a later segment and the
        // host sees an empty payload (confirmed via its debug log).
        let mut packet = req.into_bytes();
        packet.extend_from_slice(body);

        let mut stream = TcpStream::connect((self.host.as_str(), self.port))?;
        stream.set_read_timeout(Some(self.timeout))?;
        stream.set_write_timeout(Some(self.timeout))?;
        stream.write_all(&packet)?;
        read_response(&mut stream)
    }

    /// Build the `ANNOUNCE` SDP body. Sunshine's ANNOUNCE handler `.at()`s a fixed
    /// set of attributes (across the `x-nv-*`, `x-ml-*`, `x-ss-*` namespaces) and
    /// returns 400 if any are missing — this list is the complete mandatory set
    /// for Sunshine 2026.516. The `x-ss-general.encryptionEnabled` value selects
    /// the media-plane encryption (0 = plaintext). [SOURCE: Sunshine rtsp.cpp]
    fn build_announce_sdp(&self, cfg: &AnnounceConfig) -> String {
        let host = &self.host;
        let kbps = cfg.bitrate_kbps;
        [
            "v=0".to_string(),
            format!("o=android 0 14 IN IPv4 {host}"),
            "s=NVIDIA Streaming Client".to_string(),
            "t=0 0".to_string(),
            format!("a=x-nv-video[0].clientViewportWd:{}", cfg.width),
            format!("a=x-nv-video[0].clientViewportHt:{}", cfg.height),
            format!("a=x-nv-video[0].maxFPS:{}", cfg.fps),
            format!("a=x-nv-video[0].clientRefreshRateX100:{}", cfg.fps * 100),
            format!("a=x-nv-video[0].packetSize:{}", cfg.packet_size),
            "a=x-nv-video[0].rateControlMode:4".to_string(),
            "a=x-nv-video[0].timeoutLengthMs:7000".to_string(),
            "a=x-nv-video[0].framesWithInvalidRefThreshold:0".to_string(),
            format!("a=x-nv-video[0].initialBitrateKbps:{kbps}"),
            format!("a=x-nv-video[0].initialPeakBitrateKbps:{kbps}"),
            "a=x-nv-video[0].maxNumReferenceFrames:1".to_string(),
            format!(
                "a=x-nv-video[0].videoEncoderSlicesPerFrame:{}",
                cfg.slices_per_frame
            ),
            "a=x-nv-video[0].encoderCscMode:0".to_string(),
            "a=x-nv-video[0].dynamicRangeMode:0".to_string(),
            format!("a=x-nv-vqos[0].bw.minimumBitrateKbps:{kbps}"),
            format!("a=x-nv-vqos[0].bw.maximumBitrateKbps:{kbps}"),
            "a=x-nv-vqos[0].fec.enable:1".to_string(),
            "a=x-nv-vqos[0].fec.numSrcPackets:0".to_string(),
            format!("a=x-nv-vqos[0].fec.repairPercent:{}", cfg.fec_percent),
            "a=x-nv-vqos[0].fec.repairMaxPercent:100".to_string(),
            "a=x-nv-vqos[0].fec.minRequiredFecPackets:2".to_string(),
            "a=x-nv-vqos[0].drc.enable:0".to_string(),
            "a=x-nv-vqos[0].qosTrafficType:5".to_string(),
            "a=x-nv-vqos[0].bitStreamFormat:1".to_string(),
            "a=x-nv-aqos.qosTrafficType:4".to_string(),
            "a=x-nv-aqos.packetDuration:5".to_string(),
            "a=x-nv-aqos.coupledAq:1".to_string(),
            format!("a=x-nv-general.serverAddress:{host}"),
            "a=x-nv-general.featureFlags:135".to_string(),
            "a=x-nv-general.useReliableUdp:1".to_string(),
            "a=x-nv-clientSupportHevc:1".to_string(),
            "a=x-nv-audio.surround.numChannels:2".to_string(),
            "a=x-nv-audio.surround.channelMask:3".to_string(),
            "a=x-nv-audio.surround.enable:0".to_string(),
            "a=x-nv-audio.surround.AudioQuality:0".to_string(),
            "a=x-ml-general.featureFlags:0".to_string(),
            format!("a=x-ml-video.configuredBitrateKbps:{kbps}"),
            format!("a=x-ss-general.encryptionEnabled:{}", cfg.encryption_enabled),
            "a=x-ss-video[0].chromaSamplingType:0".to_string(),
            "a=x-ss-video[0].intraRefresh:0".to_string(),
            String::new(),
        ]
        .join("\r\n")
    }

    /// Send `ANNOUNCE` with the stream config SDP. Arms the host's session so it
    /// will start streaming once the client pings the media ports. Must run after
    /// SETUP (needs the session id) and before PLAY.
    pub fn announce(&mut self, session_id: &str, cfg: &AnnounceConfig) -> crate::Result<()> {
        const GS: (&str, &str) = ("X-GS-ClientVersion", "14");
        let sdp = self.build_announce_sdp(cfg);
        let resp = self.request(
            "ANNOUNCE",
            None,
            &[("Content-type", "application/sdp"), ("Session", session_id), GS],
            sdp.as_bytes(),
        )?;
        if resp.status != 200 {
            return Err(crate::Error::Protocol(format!(
                "ANNOUNCE: RTSP {} (SDP missing a mandatory attribute?)",
                resp.status
            )));
        }
        Ok(())
    }

    /// Walk the full RTSP handshake — `OPTIONS → DESCRIBE → SETUP×3 → ANNOUNCE →
    /// PLAY` — and return the session binding. The ANNOUNCE arms the host's
    /// session (without it, PLAY 200s but the host never streams). After this the
    /// host is ready to stream once the client pings the media ports.
    pub fn handshake(&mut self, cfg: &AnnounceConfig) -> crate::Result<RtspSession> {
        const GS: (&str, &str) = ("X-GS-ClientVersion", "14");
        let target = self.target.clone();

        self.request("OPTIONS", None, &[GS], b"")?;

        let describe = self.request("DESCRIBE", None, &[GS], b"")?;
        let sdp = SdpInfo::parse(&describe.body);

        let mut ports = StreamSetup::default();
        let mut session_id = String::new();
        let mut ping_payload = Vec::new();
        let mut control_connect_data = 0u32;

        // SETUP order matches Moonlight (video, audio, control). The host streams
        // to the address that pinged each stream's port, and pairs ports by SETUP
        // order — requesting audio first mis-binds video to the audio socket.
        for stream in ["video", "audio", "control"] {
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
                // The client sends the header's **literal ASCII bytes** to the
                // media UDP ports — NOT hex-decoded. moonlight-common-c does
                // `memcpy(SS_PING.payload, header, strlen(header))` with
                // `strlen == sizeof(payload)` (16); the host registers the session
                // under those exact bytes and matches the client's ping against
                // them. [SOURCE: moonlight-common-c RtspConnection.c — structure]
                ping_payload = p.trim().as_bytes().to_vec();
            }
            if let Some(c) = resp.header("X-SS-Connect-Data") {
                control_connect_data = c.trim().parse().unwrap_or(0);
            }
        }

        // ANNOUNCE arms the session (the host won't stream without it).
        self.announce(&session_id, cfg)?;

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
