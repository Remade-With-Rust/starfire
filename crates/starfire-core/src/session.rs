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
    /// Sunshine's `X-SS-Ping-Payload` token. The host only accepts a media-port
    /// ping that carries this exact payload (it identifies the session and learns
    /// our return address from it). Empty ⇒ fall back to the legacy `"PING"`.
    ping_payload: Vec<u8>,
    /// `SS_PING.sequenceNumber` — bumped per ping. The host ignores it for the
    /// payload match, but it's part of the 20-byte struct the host expects.
    ping_seq: u32,
    /// Demultiplexed receive queues. Both media sockets are drained together and
    /// each datagram is sorted by its **source port** — video RTP always originates
    /// from `video_port`, audio from `audio_port` — so a stream is delivered to the
    /// right consumer no matter which local socket the host actually sent it to.
    /// This makes ingest immune to Sunshine routing both streams onto one socket
    /// (which it does when its shared-payload ping registration races, or when the
    /// client and host share an IP). See [`pump`](StreamSession::pump).
    video_q: std::collections::VecDeque<Vec<u8>>,
    audio_q: std::collections::VecDeque<Vec<u8>>,
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
        // The launch response's `rtsp_url` carries the HOST's own view of its
        // address — e.g. a VM's internal 192.168.122.x behind the box's NAT, which
        // the client can't reach. Reconnect RTSP at the address we actually dialed
        // (`host`), keeping the host's advertised port. Without this the RTSP
        // handshake hangs on the unreachable internal IP and video never starts.
        let rtsp_port = session
            .rtsp_url
            .rsplit(':')
            .next()
            .and_then(|p| p.trim_end_matches('/').parse::<u16>().ok())
            .unwrap_or(48010);
        let rtsp_url = format!("rtsp://{host}:{rtsp_port}");
        let mut rtsp = RtspClient::new(&rtsp_url, Duration::from_secs(10))?;
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

        // Bind the media ports we advertised in RTSP SETUP (X-GS-ClientPort=
        // 50000-50001) so the host streams to where we listen, whether it uses the
        // SETUP port or the ping source. Fall back to ephemeral if taken.
        let video = UdpSocket::bind("0.0.0.0:50000").or_else(|_| UdpSocket::bind("0.0.0.0:0"))?;
        let audio = UdpSocket::bind("0.0.0.0:50001").or_else(|_| UdpSocket::bind("0.0.0.0:0"))?;
        // Both sockets are non-blocking; [`pump`] drains both and sorts datagrams
        // by source port. poll_video adds the only pacing wait (a short sleep when
        // no video is queued) so the caller's loop doesn't busy-spin.
        video.set_nonblocking(true)?;
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
            ping_payload: rs.ping_payload,
            ping_seq: 0,
            video_q: std::collections::VecDeque::new(),
            audio_q: std::collections::VecDeque::new(),
            last_ping: Instant::now() - Duration::from_secs(1),
            recv_err_logged: false,
        };
        s.ping(); // open the return path immediately
        Ok(s)
    }

    /// Ping both media ports so the host learns our return address per stream and
    /// starts / keeps streaming. Stock Sunshine requires its per-session
    /// `X-SS-Ping-Payload` token; older hosts accept the legacy `"PING"`.
    ///
    /// **Each socket pings its OWN stream's port** with the same session payload:
    /// the host routes each stream's RTP back to the source address+port of the
    /// ping that arrived on that stream's port. Video pings `video_port` from the
    /// video socket; audio pings `audio_port` from the audio socket. This mirrors
    /// Moonlight and gives independent per-stream return paths. [SOURCE: observed
    /// Sunshine wire behavior — distinct source ports ping distinct server ports
    /// with the same 16-byte payload; verified against stock Sunshine from a
    /// separate client machine, both video and audio routed correctly.]
    ///
    /// If Sunshine's shared-payload ping registration races (or the client and
    /// host share an IP) it can route both streams onto one socket — [`pump`] makes
    /// that harmless by demultiplexing on the source port, so we always ping both.
    ///
    /// [`pump`]: StreamSession::pump
    fn ping(&mut self) {
        if self.ping_payload.len() == 16 {
            // SS_PING (packed, 20 bytes): the 16-byte literal payload + a u32
            // big-endian sequence. The host matches the payload (ignoring the
            // sequence) and learns our per-stream return address.
            let mut pkt = [0u8; 20];
            pkt[..16].copy_from_slice(&self.ping_payload);
            pkt[16..20].copy_from_slice(&self.ping_seq.to_be_bytes());
            self.ping_seq = self.ping_seq.wrapping_add(1);
            let _ = self.video.send_to(&pkt, (self.host.as_str(), self.video_port));
            let _ = self.audio.send_to(&pkt, (self.host.as_str(), self.audio_port));
        } else {
            // Legacy 4-byte hole-punch (older hosts match by source address).
            let _ = self.video.send_to(b"PING", (self.host.as_str(), self.video_port));
            let _ = self.audio.send_to(b"PING", (self.host.as_str(), self.audio_port));
        }
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

    /// Drain **both** media sockets (non-blocking) into the per-stream queues,
    /// classifying each datagram by its **source port**: a packet from
    /// `audio_port` is audio, anything else (i.e. `video_port`) is video. This is
    /// the heart of the demux — it recovers the correct stream regardless of which
    /// local socket the host routed it to, so a Sunshine ping-registration race
    /// (both streams on one socket) or a co-located client can't break ingest.
    fn pump(&mut self) {
        let mut buf = [0u8; 2048];
        // Drain video socket then audio socket fully; order within a single source
        // stream is preserved because each stream arrives on exactly one socket.
        for which in 0..2 {
            loop {
                let sock = if which == 0 { &self.video } else { &self.audio };
                match sock.recv_from(&mut buf) {
                    Ok((n, src)) => {
                        let pkt = buf[..n].to_vec();
                        if src.port() == self.audio_port {
                            self.audio_q.push_back(pkt);
                        } else {
                            self.video_q.push_back(pkt);
                        }
                    }
                    Err(e)
                        if matches!(
                            e.kind(),
                            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                        ) =>
                    {
                        break
                    }
                    Err(e) => {
                        if !self.recv_err_logged {
                            eprintln!("[stream] media recv error: {e} (kind {:?})", e.kind());
                            self.recv_err_logged = true;
                        }
                        break;
                    }
                }
            }
        }
    }

    /// Return one demultiplexed video datagram's length (copied into `buf`) if one
    /// is available. Keeps the control peer alive, re-pings periodically, and adds
    /// the loop's only pacing wait — a short sleep when no video is queued — so the
    /// caller's tight loop doesn't busy-spin. Call in a loop on the network thread.
    pub fn poll_video(&mut self, buf: &mut [u8]) -> Option<usize> {
        let _ = self.control.poll();
        if self.last_ping.elapsed() > std::time::Duration::from_millis(200) {
            self.ping();
        }
        self.pump();
        if self.video_q.is_empty() {
            // No video queued — wait briefly (the pacing the blocking read used to
            // provide) then drain once more, so idle iterations don't busy-spin.
            std::thread::sleep(std::time::Duration::from_millis(2));
            self.pump();
        }
        self.video_q.pop_front().map(|pkt| {
            let n = pkt.len().min(buf.len());
            buf[..n].copy_from_slice(&pkt[..n]);
            n
        })
    }

    /// Drain one demultiplexed audio datagram into `buf`, returning its length, or
    /// `None` when none is queued. Independent of the video path — audio never
    /// gates video. The keepalive ping is driven by [`poll_video`], so audio can be
    /// fully ignored (muted) without the host tearing the session down.
    ///
    /// [`poll_video`]: StreamSession::poll_video
    pub fn poll_audio(&mut self, buf: &mut [u8]) -> Option<usize> {
        if self.audio_q.is_empty() {
            self.pump();
        }
        self.audio_q.pop_front().map(|pkt| {
            let n = pkt.len().min(buf.len());
            buf[..n].copy_from_slice(&pkt[..n]);
            n
        })
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
