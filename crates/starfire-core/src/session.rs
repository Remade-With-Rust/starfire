// SPDX-License-Identifier: Apache-2.0
//! Session orchestration — the connection state machine that walks the protocol
//! lifecycle (docs/02-architecture.md §lifecycle): discover → pair → serverinfo →
//! launch → rtsp → control up → media ingest, with IDR/reconnect on loss and
//! clean teardown on quit. Drives the per-layer modules; owns no wire format.

/// Where we are in the connection lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Phase {
    #[default]
    Idle,
    Discovered,
    Paired,
    Negotiated,
    Launched,
    RtspReady,
    ControlUp,
    Streaming,
    TearingDown,
}

/// The session driver. Phase 1 wires the real layers behind this; today it only
/// models the phase progression so the state machine has a home.
#[derive(Debug, Default)]
pub struct Session {
    phase: Phase,
}

impl Session {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn phase(&self) -> Phase {
        self.phase
    }
}

/// Run the pairing ladder against `host` and return an authenticated client.
/// **Blocks until the PIN is entered on the host** (out of band) — callers that
/// auto-submit the PIN (e.g. via the host's web API) should do so concurrently
/// on another thread. `device_name` is shown in the host's client list.
pub fn pair(
    host: &str,
    device_name: &str,
    pin: &str,
) -> crate::Result<crate::launch::PairedClient> {
    use crate::https::{cert_pem_to_der, HttpsClient};
    use crate::launch::PairedClient;
    use crate::pairing::{ClientIdentity, PairingClient};

    let id = ClientIdentity::generate(device_name)?;
    let (cert, key) = (id.cert_pem.clone(), id.key_pem.clone());
    let mut salt = [0u8; 16];
    getrandom::getrandom(&mut salt).map_err(|e| crate::Error::Protocol(format!("rng: {e}")))?;

    let pairing = PairingClient::new(host, 47989, id);
    let host_pem = pairing.pair(&salt, pin)?; // blocks on the host-side PIN entry
    let der = cert_pem_to_der(&host_pem)?;
    let https = HttpsClient::new(&cert, &key, Some(der))?;
    pairing.pair_challenge(&https, 47984)?;
    let uid = pairing.identity.unique_id.clone();
    Ok(PairedClient::new(https, host, 47984, &uid))
}

/// A live streaming session's data plane: it launches the app, walks the RTSP
/// handshake (which arms the host), connects the ENet control channel (which
/// sets the host's RTP source address), and opens/pings the media sockets — then
/// hands the caller received video datagrams via [`poll_video`]. Assumes an
/// already-paired [`PairedClient`]. Cancels the host session on drop.
///
/// This is the post-pairing bring-up proven by the `live_explore_video` test,
/// lifted into a reusable driver the desktop app pumps each frame.
///
/// [`poll_video`]: StreamSession::poll_video
/// [`PairedClient`]: crate::launch::PairedClient
pub struct StreamSession {
    client: crate::launch::PairedClient,
    control: crate::control::ControlChannel,
    video: std::net::UdpSocket,
    audio: std::net::UdpSocket,
    host: String,
    video_port: u16,
    audio_port: u16,
    last_ping: std::time::Instant,
    recv_err_logged: bool,
}

impl StreamSession {
    /// Bring up the data plane for `app_id` on `host` (an IP the host can reach
    /// back — not loopback). `client` must already be paired.
    pub fn start(
        client: crate::launch::PairedClient,
        host: &str,
        app_id: &str,
        launch: &crate::launch::LaunchConfig,
        announce: &crate::rtsp::AnnounceConfig,
    ) -> crate::Result<Self> {
        use crate::control::ControlChannel;
        use crate::rtsp::RtspClient;
        use std::net::UdpSocket;
        use std::time::{Duration, Instant};

        // Clear any stale session first (e.g. a prior client that didn't tear
        // down cleanly) so launch doesn't 400 with "app already running". A
        // production client would `resume()` an owned session instead.
        let _ = client.cancel();
        let session = client.launch(app_id, launch)?;
        let mut rtsp = RtspClient::new(&session.rtsp_url, Duration::from_secs(10))?;
        let rs = rtsp.handshake(announce)?; // OPTIONS..ANNOUNCE..PLAY — arms the host

        // The control channel must connect before any RTP can flow (the host
        // derives its send source address from this peer). Retry briefly.
        let control_addr = format!("{host}:{}", rs.ports.control_port)
            .parse()
            .map_err(|e| crate::Error::Protocol(format!("control addr: {e}")))?;
        let mut control = None;
        let deadline = Instant::now() + Duration::from_secs(6);
        let mut last_err = String::new();
        while Instant::now() < deadline {
            match ControlChannel::connect(
                control_addr,
                rs.control_connect_data,
                1,
                0,
                Duration::from_secs(1),
            ) {
                Ok(c) => {
                    control = Some(c);
                    break;
                }
                Err(e) => last_err = e.to_string(),
            }
        }
        let control = control.ok_or_else(|| {
            crate::Error::Protocol(format!("ENet control connect failed: {last_err}"))
        })?;

        let video = UdpSocket::bind("0.0.0.0:0")?;
        let audio = UdpSocket::bind("0.0.0.0:0")?;
        // Video paces the pump (short blocking read); audio is drained
        // non-blocking each loop so it never adds latency to the video path.
        video.set_read_timeout(Some(Duration::from_millis(4)))?;
        audio.set_nonblocking(true)?;

        eprintln!(
            "[stream] video socket {:?} (host:{}), audio socket {:?} (host:{})",
            video.local_addr().ok(),
            rs.ports.video_port,
            audio.local_addr().ok(),
            rs.ports.audio_port
        );
        let mut s = Self {
            client,
            control,
            video,
            audio,
            host: host.to_string(),
            video_port: rs.ports.video_port,
            audio_port: rs.ports.audio_port,
            last_ping: Instant::now() - Duration::from_secs(1),
            recv_err_logged: false,
        };
        s.ping(); // open the return path immediately
        Ok(s)
    }

    /// Send the legacy `"PING"` hole-punch to both media ports (the host learns
    /// our return address from it and starts/keeps streaming).
    fn ping(&mut self) {
        let _ = self.video.send_to(b"PING", (self.host.as_str(), self.video_port));
        let _ = self.audio.send_to(b"PING", (self.host.as_str(), self.audio_port));
        self.last_ping = std::time::Instant::now();
    }

    /// Network round-trip time to the host (ENet control-channel RTT).
    pub fn rtt(&mut self) -> std::time::Duration {
        self.control.rtt()
    }

    /// Send one encoded input control message (see [`crate::input`]) to the host
    /// over the ENet control channel (channel 0, reliable). Call immediately per
    /// input event for lowest latency.
    pub fn send_input(&mut self, msg: &[u8]) -> crate::Result<()> {
        self.control.send(0, msg)
    }

    /// Ask the host for an IDR keyframe — Sunshine `REQUEST_IDR_FRAME` (0x0302),
    /// `control_header_v2` framing (type LE u16 + len LE u16 + payload). Sent on
    /// unrecoverable video loss so the host re-keys immediately instead of waiting
    /// out the GOP. Backwards-compatible: Sunshine handles this type; a host that
    /// doesn't simply ignores it.
    pub fn request_idr(&mut self) -> crate::Result<()> {
        self.control.send(0, &[0x02, 0x03, 0x00, 0x00]) // type=0x0302, len=0
    }

    /// Send a periodic loss report — Sunshine `LOSS_STATS` (0x0201) with the
    /// `int32[4]{count, time_ms, reserved, lastGoodFrame}` payload. Feeds the
    /// host's bitrate estimator (a host that ignores it is unaffected).
    pub fn send_loss_stats(&mut self, count: i32, time_ms: i32, last_good: i32) -> crate::Result<()> {
        let mut m = Vec::with_capacity(20);
        m.extend_from_slice(&0x0201u16.to_le_bytes());
        m.extend_from_slice(&16u16.to_le_bytes());
        m.extend_from_slice(&count.to_le_bytes());
        m.extend_from_slice(&time_ms.to_le_bytes());
        m.extend_from_slice(&0i32.to_le_bytes()); // reserved
        m.extend_from_slice(&last_good.to_le_bytes());
        self.control.send(0, &m)
    }

    /// Pump the session once and return one received video datagram's length (in
    /// `buf`) if available. Keeps the control peer alive and re-pings periodically.
    /// Call in a tight loop on the network thread.
    pub fn poll_video(&mut self, buf: &mut [u8]) -> Option<usize> {
        let _ = self.control.poll();
        if self.last_ping.elapsed() > std::time::Duration::from_millis(200) {
            self.ping();
        }
        match self.video.recv_from(buf) {
            Ok((n, _)) => Some(n),
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => None,
            Err(e) => {
                if !self.recv_err_logged {
                    eprintln!("[stream] video recv error: {e} (kind {:?})", e.kind());
                    self.recv_err_logged = true;
                }
                None
            }
        }
    }

    /// Drain one audio datagram (non-blocking) into `buf`, returning its length.
    /// Independent of the video path — audio never gates video. Returns `None`
    /// when no audio packet is queued. The keepalive ping is driven by
    /// [`poll_video`], so audio can be fully ignored (muted) without the host
    /// tearing the session down.
    ///
    /// [`poll_video`]: StreamSession::poll_video
    pub fn poll_audio(&mut self, buf: &mut [u8]) -> Option<usize> {
        self.audio.recv_from(buf).ok().map(|(n, _)| n)
    }
}

impl Drop for StreamSession {
    fn drop(&mut self) {
        let _ = self.client.cancel();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_session_is_idle() {
        assert_eq!(Session::new().phase(), Phase::Idle);
    }
}
