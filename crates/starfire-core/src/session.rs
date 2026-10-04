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

/// What a running [`StreamSession`] delivers to its consumer. Media datagrams
/// are stamped with their arrival time **on the receive thread**, so time spent
/// queued behind the consumer shows up in the latency numbers instead of hiding.
#[derive(Debug)]
pub enum SessionEvent {
    /// One video datagram (RTP + NV header + shard).
    Video {
        at: std::time::Instant,
        data: Vec<u8>,
    },
    /// One audio datagram (RTP + Opus, or audio FEC).
    Audio {
        at: std::time::Instant,
        data: Vec<u8>,
    },
    /// The control channel ended (host closed it, or it timed out). The session
    /// is over; drop it.
    Disconnected(String),
}

/// Receive-buffer size requested for the video socket. A keyframe is hundreds of
/// datagrams sent back to back; with the OS default (tens to a few hundred KB)
/// the tail of such a burst is dropped in the kernel before the application can
/// read it, which looks exactly like network loss. 4 MiB holds several of the
/// largest frames, so the host never has to slow its sending to protect us.
pub const VIDEO_RCVBUF: usize = 4 * 1024 * 1024;
/// Receive-buffer size requested for the audio socket.
pub const AUDIO_RCVBUF: usize = 256 * 1024;

/// Largest datagram we accept (well above the ~1.4 KB media packets).
const MAX_DATAGRAM: usize = 2048;
/// How often each media socket re-pings the host (keeps the return path open).
const PING_INTERVAL: std::time::Duration = std::time::Duration::from_millis(200);
/// Upper bound on how long a receive thread sleeps in `recv` before it checks
/// the stop flag and the ping timer.
const RECV_TICK: std::time::Duration = std::time::Duration::from_millis(50);
/// Idle wake-up of the control thread (ENet keepalive / retransmit service).
const CONTROL_TICK: std::time::Duration = std::time::Duration::from_millis(2);
/// Recycled datagram buffers kept for reuse.
const POOL_LIMIT: usize = 1024;

/// Network-side counters for a session (exact, lock-free).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct NetStats {
    pub video_packets: u64,
    pub video_bytes: u64,
    pub audio_packets: u64,
    /// Datagrams from an address other than the host — discarded.
    pub foreign_packets: u64,
    /// Receive-buffer sizes the OS actually granted (bytes).
    pub video_rcvbuf: usize,
    pub audio_rcvbuf: usize,
}

#[derive(Default)]
struct NetCounters {
    video_packets: std::sync::atomic::AtomicU64,
    video_bytes: std::sync::atomic::AtomicU64,
    audio_packets: std::sync::atomic::AtomicU64,
    foreign_packets: std::sync::atomic::AtomicU64,
}

enum ControlCmd {
    Send(Vec<u8>),
    Stop,
}

/// A cheap, cloneable handle for talking to the host over the control channel.
/// Messages are handed to a dedicated control thread, which wakes immediately
/// and sends them — input never waits behind video receive or decode.
#[derive(Clone)]
pub struct ControlHandle {
    tx: std::sync::mpsc::Sender<ControlCmd>,
    rtt_us: std::sync::Arc<std::sync::atomic::AtomicU32>,
}

impl ControlHandle {
    /// Send one encoded input message (see [`crate::input`]). Reliable, in order.
    pub fn send_input(&self, msg: Vec<u8>) {
        let _ = self.tx.send(ControlCmd::Send(msg));
    }

    /// Ask the host for an IDR keyframe — Sunshine `REQUEST_IDR_FRAME` (0x0302),
    /// `control_header_v2` framing (type LE u16 + len LE u16 + payload). Sent the
    /// moment a frame is known to be unrecoverable, so the host re-keys at once
    /// instead of the picture staying broken until its next scheduled keyframe.
    pub fn request_idr(&self) {
        let _ = self.tx.send(ControlCmd::Send(vec![0x02, 0x03, 0x00, 0x00]));
    }

    /// Send a periodic loss report — Sunshine `LOSS_STATS` (0x0201) with the
    /// `int32[4]{count, time_ms, reserved, lastGoodFrame}` payload.
    pub fn send_loss_stats(&self, count: i32, time_ms: i32, last_good: i32) {
        let mut m = Vec::with_capacity(20);
        m.extend_from_slice(&0x0201u16.to_le_bytes());
        m.extend_from_slice(&16u16.to_le_bytes());
        m.extend_from_slice(&count.to_le_bytes());
        m.extend_from_slice(&time_ms.to_le_bytes());
        m.extend_from_slice(&0i32.to_le_bytes()); // reserved
        m.extend_from_slice(&last_good.to_le_bytes());
        let _ = self.tx.send(ControlCmd::Send(m));
    }

    /// Network round-trip time to the host (ENet control-channel RTT), as of the
    /// control thread's last service pass. Zero until the first measurement.
    pub fn rtt(&self) -> std::time::Duration {
        let us = self.rtt_us.load(std::sync::atomic::Ordering::Relaxed);
        std::time::Duration::from_micros(us as u64)
    }
}

/// A live streaming session's data plane: it launches the app, walks the RTSP
/// handshake (which arms the host), connects the ENet control channel (which
/// sets the host's RTP source address), and opens/pings the media sockets — then
/// streams received datagrams to the caller as [`SessionEvent`]s. Assumes an
/// already-paired [`PairedClient`]. Cancels the host session on drop.
///
/// Threads (all owned by the session, all stopped on drop):
/// * **video receive** / **audio receive** — each blocks in `recv` on its own
///   socket, stamps the arrival time and forwards the datagram. Blocking receive
///   means a packet is handed on the moment the kernel has it; there is no
///   polling interval to wait out.
/// * **control** — owns the ENet channel; sends input and feedback the moment
///   they are queued and keeps ENet serviced.
///
/// [`PairedClient`]: crate::launch::PairedClient
pub struct StreamSession {
    client: crate::launch::PairedClient,
    control: ControlHandle,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    threads: Vec<std::thread::JoinHandle<()>>,
    pool: std::sync::Arc<std::sync::Mutex<Vec<Vec<u8>>>>,
    counters: std::sync::Arc<NetCounters>,
    rcvbuf: (usize, usize),
}

/// Bind a media socket on `port` (falling back to an ephemeral port if taken)
/// with a large receive buffer; returns the socket and the buffer size granted.
fn bind_media(port: u16, rcvbuf: usize) -> std::io::Result<(std::net::UdpSocket, usize)> {
    use socket2::{Domain, Protocol, Socket, Type};
    use std::net::{Ipv4Addr, SocketAddr};

    let make = |port: u16| -> std::io::Result<Socket> {
        let s = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
        s.bind(&SocketAddr::from((Ipv4Addr::UNSPECIFIED, port)).into())?;
        Ok(s)
    };
    let sock = make(port).or_else(|_| make(0))?;
    // Best effort: the OS may clamp it (macOS `kern.ipc.maxsockbuf`); report
    // what we actually got rather than what we asked for.
    let _ = sock.set_recv_buffer_size(rcvbuf);
    let granted = sock.recv_buffer_size().unwrap_or(0);
    let sock: std::net::UdpSocket = sock.into();
    sock.set_read_timeout(Some(RECV_TICK))?;
    Ok((sock, granted))
}

/// The media-port ping: Sunshine's 20-byte `SS_PING` (the session's 16-byte
/// payload + a big-endian sequence), or the legacy 4-byte `"PING"`.
fn ping_packet(payload: &[u8], seq: u32) -> Vec<u8> {
    if payload.len() == 16 {
        let mut pkt = Vec::with_capacity(20);
        pkt.extend_from_slice(payload);
        pkt.extend_from_slice(&seq.to_be_bytes());
        pkt
    } else {
        b"PING".to_vec()
    }
}

/// Everything one media receive thread needs.
struct RecvCtx {
    sock: std::net::UdpSocket,
    /// The host port this socket pings (its own stream's port).
    ping_addr: std::net::SocketAddr,
    host_ip: std::net::IpAddr,
    audio_port: u16,
    ping_payload: std::sync::Arc<Vec<u8>>,
    events: std::sync::mpsc::Sender<SessionEvent>,
    pool: std::sync::Arc<std::sync::Mutex<Vec<Vec<u8>>>>,
    counters: std::sync::Arc<NetCounters>,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

/// Receive loop for one media socket. Datagrams are classified by their
/// **source port** — video RTP always originates from the host's video port,
/// audio from its audio port — so a stream reaches the right consumer no matter
/// which local socket the host sent it to (Sunshine routes both onto one socket
/// when its ping registration races, or when client and host share an IP).
fn recv_loop(ctx: RecvCtx) {
    use std::sync::atomic::Ordering;
    use std::time::Instant;

    // A packet must be picked up the moment it arrives, however busy the machine.
    starfire_rt::promote_current_thread(starfire_rt::Role::Network);
    let mut last_ping = Instant::now() - PING_INTERVAL;
    let mut ping_seq = 0u32;
    let mut err_logged = false;
    while !ctx.stop.load(Ordering::Relaxed) {
        if last_ping.elapsed() >= PING_INTERVAL {
            let _ = ctx
                .sock
                .send_to(&ping_packet(&ctx.ping_payload, ping_seq), ctx.ping_addr);
            ping_seq = ping_seq.wrapping_add(1);
            last_ping = Instant::now();
        }
        let mut buf = ctx
            .pool
            .lock()
            .ok()
            .and_then(|mut p| p.pop())
            .unwrap_or_default();
        buf.resize(MAX_DATAGRAM, 0);
        match ctx.sock.recv_from(&mut buf) {
            Ok((n, src)) => {
                let at = Instant::now();
                buf.truncate(n);
                if src.ip() != ctx.host_ip {
                    // Not from the host: never let a stray or hostile datagram
                    // into the reassembler.
                    ctx.counters.foreign_packets.fetch_add(1, Ordering::Relaxed);
                    recycle_into(&ctx.pool, buf);
                    continue;
                }
                let event = if src.port() == ctx.audio_port {
                    ctx.counters.audio_packets.fetch_add(1, Ordering::Relaxed);
                    SessionEvent::Audio { at, data: buf }
                } else {
                    ctx.counters.video_packets.fetch_add(1, Ordering::Relaxed);
                    ctx.counters
                        .video_bytes
                        .fetch_add(n as u64, Ordering::Relaxed);
                    SessionEvent::Video { at, data: buf }
                };
                if ctx.events.send(event).is_err() {
                    return; // consumer is gone
                }
            }
            Err(e) => {
                recycle_into(&ctx.pool, buf);
                match e.kind() {
                    // The read timeout: nothing arrived this tick.
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut => {}
                    // Windows reports an ICMP "port unreachable" for an earlier
                    // ping as a reset on the next receive; the socket is fine.
                    std::io::ErrorKind::ConnectionReset => {}
                    _ => {
                        if !err_logged {
                            eprintln!("[stream] media recv error: {e} (kind {:?})", e.kind());
                            err_logged = true;
                        }
                        std::thread::sleep(std::time::Duration::from_millis(5));
                    }
                }
            }
        }
    }
}

fn recycle_into(pool: &std::sync::Mutex<Vec<Vec<u8>>>, buf: Vec<u8>) {
    if let Ok(mut p) = pool.lock() {
        if p.len() < POOL_LIMIT {
            p.push(buf);
        }
    }
}

/// The control thread: connect ENet (reporting the outcome through `ready`),
/// then send queued messages the moment they arrive and keep ENet serviced.
fn control_loop(
    addr: std::net::SocketAddr,
    connect_data: u32,
    ready: std::sync::mpsc::SyncSender<crate::Result<()>>,
    rx: std::sync::mpsc::Receiver<ControlCmd>,
    rtt_us: std::sync::Arc<std::sync::atomic::AtomicU32>,
    events: std::sync::mpsc::Sender<SessionEvent>,
) {
    use crate::control::{ControlChannel, ControlEvent};
    use std::sync::atomic::Ordering;
    use std::sync::mpsc::RecvTimeoutError;
    use std::time::{Duration, Instant};

    // Input must go out the moment it is queued.
    starfire_rt::promote_current_thread(starfire_rt::Role::Network);
    // The control channel must connect before any RTP can flow (the host
    // derives its send source address from this peer). Retry briefly.
    let deadline = Instant::now() + Duration::from_secs(6);
    let mut last_err = String::new();
    let mut control = None;
    while Instant::now() < deadline {
        match ControlChannel::connect(addr, connect_data, 1, 0, Duration::from_secs(1)) {
            Ok(c) => {
                control = Some(c);
                break;
            }
            Err(e) => last_err = e.to_string(),
        }
    }
    let Some(mut control) = control else {
        let _ = ready.send(Err(crate::Error::Protocol(format!(
            "ENet control connect failed: {last_err}"
        ))));
        return;
    };
    if ready.send(Ok(())).is_err() {
        return;
    }

    let mut batch: Vec<Vec<u8>> = Vec::new();
    loop {
        // Sleep until there is something to send, or the service tick.
        let mut stop = false;
        match rx.recv_timeout(CONTROL_TICK) {
            Ok(ControlCmd::Send(m)) => batch.push(m),
            Ok(ControlCmd::Stop) | Err(RecvTimeoutError::Disconnected) => stop = true,
            Err(RecvTimeoutError::Timeout) => {}
        }
        // Take everything else that is already queued. If relative mouse moves
        // have piled up (the link stalled), fold them into one message so the
        // host gets the same total motion in a single packet instead of a
        // backlog it must replay.
        while let Ok(cmd) = rx.try_recv() {
            match cmd {
                ControlCmd::Send(m) => {
                    let merged = batch
                        .last_mut()
                        .is_some_and(|last| crate::input::merge_mouse_rel(last, &m));
                    if !merged {
                        batch.push(m);
                    }
                }
                ControlCmd::Stop => stop = true,
            }
        }
        if !batch.is_empty() {
            for m in batch.drain(..) {
                if let Err(e) = control.queue(0, &m) {
                    let _ = events.send(SessionEvent::Disconnected(format!("control send: {e}")));
                    return;
                }
            }
            control.flush();
        }
        if stop {
            return;
        }
        // Service ENet until it has nothing more for us.
        loop {
            match control.poll() {
                Ok(Some(ControlEvent::Disconnected)) => {
                    let _ = events.send(SessionEvent::Disconnected(
                        "the host closed the control channel".into(),
                    ));
                    return;
                }
                Ok(Some(_)) => {}
                Ok(None) => break,
                Err(e) => {
                    let _ =
                        events.send(SessionEvent::Disconnected(format!("control channel: {e}")));
                    return;
                }
            }
        }
        rtt_us.store(
            control.rtt().as_micros().min(u32::MAX as u128) as u32,
            Ordering::Relaxed,
        );
    }
}

impl StreamSession {
    /// Bring up the data plane for `app_id` on `host` (an IP the host can reach
    /// back — not loopback) and start streaming datagrams into `events`.
    /// `client` must already be paired.
    pub fn start(
        client: crate::launch::PairedClient,
        host: &str,
        app_id: &str,
        launch: &crate::launch::LaunchConfig,
        announce: &crate::rtsp::AnnounceConfig,
        events: std::sync::mpsc::Sender<SessionEvent>,
    ) -> crate::Result<Self> {
        use crate::rtsp::RtspClient;
        use std::net::{SocketAddr, ToSocketAddrs};
        use std::sync::atomic::{AtomicBool, AtomicU32};
        use std::sync::{mpsc, Arc, Mutex};
        use std::time::Duration;

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

        // Bind the media ports we advertise in RTSP SETUP (X-GS-ClientPort=
        // 50000-50001) so the host streams to where we listen, whether it uses the
        // SETUP port or the ping source. Fall back to ephemeral if taken. Bound
        // before the handshake so the announce can state the receive buffer the OS
        // actually granted (a host that knows it can skip pacing its frames).
        let (video, video_rcvbuf) = bind_media(50000, VIDEO_RCVBUF)?;
        let (audio, audio_rcvbuf) = bind_media(50001, AUDIO_RCVBUF)?;
        let announce = crate::rtsp::AnnounceConfig {
            recv_buffer_bytes: video_rcvbuf.min(u32::MAX as usize) as u32,
            ..announce.clone()
        };
        let rs = rtsp.handshake(&announce)?; // OPTIONS..ANNOUNCE..PLAY — arms the host

        let resolve = |port: u16| -> crate::Result<SocketAddr> {
            (host, port)
                .to_socket_addrs()
                .map_err(|e| crate::Error::Protocol(format!("resolve {host}: {e}")))?
                .find(SocketAddr::is_ipv4)
                .ok_or_else(|| crate::Error::Protocol(format!("{host} has no IPv4 address")))
        };
        let control_addr = resolve(rs.ports.control_port)?;
        let video_addr = resolve(rs.ports.video_port)?;
        let audio_addr = resolve(rs.ports.audio_port)?;

        // Control thread: owns ENet from connect to teardown.
        let (cmd_tx, cmd_rx) = mpsc::channel::<ControlCmd>();
        let (ready_tx, ready_rx) = mpsc::sync_channel::<crate::Result<()>>(1);
        let rtt_us = Arc::new(AtomicU32::new(0));
        let mut threads = Vec::with_capacity(3);
        {
            let (rtt, ev) = (rtt_us.clone(), events.clone());
            let connect_data = rs.control_connect_data;
            threads.push(std::thread::spawn(move || {
                control_loop(control_addr, connect_data, ready_tx, cmd_rx, rtt, ev)
            }));
        }
        match ready_rx.recv() {
            Ok(Ok(())) => {}
            Ok(Err(e)) => return Err(e),
            Err(_) => return Err(crate::Error::Protocol("control thread exited".into())),
        }
        let control = ControlHandle { tx: cmd_tx, rtt_us };

        eprintln!(
            "[stream] video socket {:?} (host:{}, rcvbuf {} KB), audio socket {:?} (host:{}, rcvbuf {} KB)",
            video.local_addr().ok(),
            rs.ports.video_port,
            video_rcvbuf / 1024,
            audio.local_addr().ok(),
            rs.ports.audio_port,
            audio_rcvbuf / 1024,
        );

        // Each socket pings its OWN stream's port with the same session payload:
        // the host routes each stream's RTP back to the source address+port of the
        // ping that arrived on that stream's port. [SOURCE: observed Sunshine wire
        // behavior — distinct source ports ping distinct server ports with the same
        // 16-byte payload; verified against stock Sunshine from a separate client
        // machine, both video and audio routed correctly.]
        let stop = Arc::new(AtomicBool::new(false));
        let pool = Arc::new(Mutex::new(Vec::new()));
        let counters = Arc::new(NetCounters::default());
        let ping_payload = Arc::new(rs.ping_payload);
        for (sock, ping_addr) in [(video, video_addr), (audio, audio_addr)] {
            let ctx = RecvCtx {
                sock,
                ping_addr,
                host_ip: video_addr.ip(),
                audio_port: rs.ports.audio_port,
                ping_payload: ping_payload.clone(),
                events: events.clone(),
                pool: pool.clone(),
                counters: counters.clone(),
                stop: stop.clone(),
            };
            threads.push(std::thread::spawn(move || recv_loop(ctx)));
        }

        Ok(Self {
            client,
            control,
            stop,
            threads,
            pool,
            counters,
            rcvbuf: (video_rcvbuf, audio_rcvbuf),
        })
    }

    /// A handle for sending input and feedback to the host. Clone it freely —
    /// e.g. hand one to the UI thread so input bypasses the video pipeline.
    pub fn control(&self) -> ControlHandle {
        self.control.clone()
    }

    /// Give a datagram buffer back for reuse once its contents are consumed, so
    /// steady-state receive does not allocate per packet.
    pub fn recycle(&self, buf: Vec<u8>) {
        recycle_into(&self.pool, buf);
    }

    /// Network-side counters since the session started.
    pub fn net_stats(&self) -> NetStats {
        use std::sync::atomic::Ordering::Relaxed;
        NetStats {
            video_packets: self.counters.video_packets.load(Relaxed),
            video_bytes: self.counters.video_bytes.load(Relaxed),
            audio_packets: self.counters.audio_packets.load(Relaxed),
            foreign_packets: self.counters.foreign_packets.load(Relaxed),
            video_rcvbuf: self.rcvbuf.0,
            audio_rcvbuf: self.rcvbuf.1,
        }
    }
}

impl Drop for StreamSession {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        let _ = self.control.tx.send(ControlCmd::Stop);
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
        // Bounded: a host that has gone away must not hold teardown hostage.
        let _ = self
            .client
            .cancel_with_timeout(std::time::Duration::from_secs(2));
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{mpsc, Arc, Mutex};
    use std::time::{Duration, Instant};

    #[test]
    fn new_session_is_idle() {
        assert_eq!(Session::new().phase(), Phase::Idle);
    }

    /// One receive thread wired to loopback sockets standing in for the host.
    struct Rig {
        events: mpsc::Receiver<SessionEvent>,
        stop: Arc<AtomicBool>,
        thread: Option<std::thread::JoinHandle<()>>,
        pool: Arc<Mutex<Vec<Vec<u8>>>>,
        counters: Arc<NetCounters>,
        /// Where the client's media socket listens.
        client_addr: SocketAddr,
        /// The "host video port": receives our pings, sends video.
        host_video: UdpSocket,
        /// The "host audio port".
        host_audio: UdpSocket,
        rcvbuf: usize,
    }

    impl Rig {
        fn new(ping_payload: &[u8]) -> Self {
            let lo = Ipv4Addr::LOCALHOST;
            let host_video = UdpSocket::bind((lo, 0)).unwrap();
            let host_audio = UdpSocket::bind((lo, 0)).unwrap();
            host_video
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let (sock, rcvbuf) = bind_media(0, VIDEO_RCVBUF).unwrap();
            let client_addr = SocketAddr::from((lo, sock.local_addr().unwrap().port()));
            let (tx, events) = mpsc::channel();
            let stop = Arc::new(AtomicBool::new(false));
            let pool = Arc::new(Mutex::new(Vec::new()));
            let counters = Arc::new(NetCounters::default());
            let ctx = RecvCtx {
                sock,
                ping_addr: host_video.local_addr().unwrap(),
                host_ip: IpAddr::V4(lo),
                audio_port: host_audio.local_addr().unwrap().port(),
                ping_payload: Arc::new(ping_payload.to_vec()),
                events: tx,
                pool: pool.clone(),
                counters: counters.clone(),
                stop: stop.clone(),
            };
            let thread = Some(std::thread::spawn(move || recv_loop(ctx)));
            Self {
                events,
                stop,
                thread,
                pool,
                counters,
                client_addr,
                host_video,
                host_audio,
                rcvbuf,
            }
        }

        fn next(&self) -> SessionEvent {
            self.events
                .recv_timeout(Duration::from_secs(2))
                .expect("an event within 2 s")
        }
    }

    impl Drop for Rig {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            if let Some(t) = self.thread.take() {
                let _ = t.join();
            }
        }
    }

    /// The receive thread opens the return path by pinging its stream's host
    /// port with the session payload + a big-endian sequence, and keeps pinging.
    #[test]
    fn receive_thread_pings_the_host_with_the_session_payload() {
        let payload = *b"0123456789ABCDEF";
        let rig = Rig::new(&payload);
        let mut buf = [0u8; 64];
        let (n, from) = rig.host_video.recv_from(&mut buf).expect("first ping");
        assert_eq!(n, 20);
        assert_eq!(&buf[..16], &payload);
        assert_eq!(&buf[16..20], &0u32.to_be_bytes());
        assert_eq!(
            from.port(),
            rig.client_addr.port(),
            "pinged from the media socket"
        );
        let (n, _) = rig.host_video.recv_from(&mut buf).expect("second ping");
        assert_eq!(
            (n, &buf[16..20]),
            (20, &1u32.to_be_bytes()[..]),
            "sequence advances"
        );
    }

    /// Without a 16-byte session payload the legacy 4-byte ping is used.
    #[test]
    fn legacy_hosts_get_the_four_byte_ping() {
        let rig = Rig::new(b"");
        let mut buf = [0u8; 64];
        let (n, _) = rig.host_video.recv_from(&mut buf).expect("ping");
        assert_eq!(&buf[..n], b"PING");
    }

    /// Datagrams are routed by the host port they came FROM, not by the local
    /// socket they arrived on, and each is stamped with its arrival time.
    #[test]
    fn datagrams_are_classified_by_source_port_and_timestamped() {
        let rig = Rig::new(b"");
        let before = Instant::now();
        rig.host_video
            .send_to(b"video-bytes", rig.client_addr)
            .unwrap();
        match rig.next() {
            SessionEvent::Video { at, data } => {
                assert_eq!(data, b"video-bytes");
                assert!(at >= before && at <= Instant::now());
            }
            other => panic!("expected video, got {other:?}"),
        }
        // Audio arriving on the SAME local socket is still audio.
        rig.host_audio
            .send_to(b"audio-bytes", rig.client_addr)
            .unwrap();
        match rig.next() {
            SessionEvent::Audio { data, .. } => assert_eq!(data, b"audio-bytes"),
            other => panic!("expected audio, got {other:?}"),
        }
        assert_eq!(rig.counters.video_packets.load(Ordering::Relaxed), 1);
        assert_eq!(rig.counters.video_bytes.load(Ordering::Relaxed), 11);
        assert_eq!(rig.counters.audio_packets.load(Ordering::Relaxed), 1);
    }

    /// A datagram from any address other than the host never reaches the
    /// consumer. (Needs a second loopback address; skipped where the OS has
    /// only 127.0.0.1 configured.)
    #[test]
    fn datagrams_from_other_addresses_are_discarded() {
        let rig = Rig::new(b"");
        let Ok(stranger) = UdpSocket::bind((Ipv4Addr::new(127, 0, 0, 2), 0)) else {
            eprintln!("skipped: 127.0.0.2 is not bindable on this OS");
            return;
        };
        stranger.send_to(b"injected", rig.client_addr).unwrap();
        rig.host_video.send_to(b"genuine", rig.client_addr).unwrap();
        match rig.next() {
            SessionEvent::Video { data, .. } => assert_eq!(data, b"genuine"),
            other => panic!("expected the genuine datagram, got {other:?}"),
        }
        assert_eq!(rig.counters.foreign_packets.load(Ordering::Relaxed), 1);
        assert!(rig.events.try_recv().is_err(), "nothing else was delivered");
    }

    /// A burst far larger than a default socket buffer survives intact while
    /// the consumer is not reading — the reason for the large receive buffer.
    #[test]
    fn a_keyframe_sized_burst_is_not_dropped_by_the_socket() {
        let rig = Rig::new(b"");
        assert!(
            rig.rcvbuf >= 1024 * 1024,
            "expected >= 1 MiB of receive buffer, the OS granted {} bytes",
            rig.rcvbuf
        );
        // 600 x 1392 bytes = 835 KB sent back to back, with nobody draining the
        // event channel in the meantime.
        let pkt = [0xA5u8; 1392];
        for _ in 0..600 {
            rig.host_video.send_to(&pkt, rig.client_addr).unwrap();
        }
        let mut got = 0;
        let deadline = Instant::now() + Duration::from_secs(3);
        while got < 600 && Instant::now() < deadline {
            if let Ok(SessionEvent::Video { data, .. }) =
                rig.events.recv_timeout(Duration::from_millis(200))
            {
                assert_eq!(data.len(), 1392);
                got += 1;
            }
        }
        assert_eq!(got, 600, "every datagram of the burst must arrive");
    }

    /// Returned buffers are reused, so steady-state receive does not allocate.
    #[test]
    fn recycled_buffers_are_reused() {
        let rig = Rig::new(b"");
        rig.host_video.send_to(b"one", rig.client_addr).unwrap();
        let SessionEvent::Video { data, .. } = rig.next() else {
            panic!("expected video")
        };
        let ptr = data.as_ptr();
        recycle_into(&rig.pool, data);
        // The receive thread may already hold a fresh buffer for its current
        // `recv`; the recycled one is picked up within the next two datagrams.
        let mut reused = false;
        for _ in 0..3 {
            rig.host_video.send_to(b"next", rig.client_addr).unwrap();
            let SessionEvent::Video { data, .. } = rig.next() else {
                panic!("expected video")
            };
            reused |= data.as_ptr() == ptr;
            recycle_into(&rig.pool, data);
        }
        assert!(
            reused,
            "the recycled buffer should have been handed out again"
        );
    }

    /// The control thread reports a failed connect instead of hanging, and a
    /// `ControlHandle` whose thread is gone swallows sends without panicking.
    #[test]
    fn control_thread_reports_connect_failure() {
        // Nothing listens on this port: ENet's connect must time out.
        let dead = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let addr = dead.local_addr().unwrap();
        let (cmd_tx, cmd_rx) = mpsc::channel::<ControlCmd>();
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let rtt = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let (ev_tx, _ev_rx) = mpsc::channel();
        let rtt2 = rtt.clone();
        let t = std::thread::spawn(move || control_loop(addr, 7, ready_tx, cmd_rx, rtt2, ev_tx));
        let outcome = ready_rx
            .recv_timeout(Duration::from_secs(15))
            .expect("the control thread must report");
        assert!(outcome.is_err(), "connecting to a dead port must fail");
        t.join().unwrap();
        let handle = ControlHandle {
            tx: cmd_tx,
            rtt_us: rtt,
        };
        handle.request_idr(); // thread is gone: must be a silent no-op
        handle.send_input(vec![1, 2, 3]);
        assert_eq!(handle.rtt(), Duration::ZERO);
    }
}
