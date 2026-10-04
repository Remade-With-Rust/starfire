// SPDX-License-Identifier: Apache-2.0
//! Embeddable Starfire client — pair, stream, and hardware-decode a Sunshine/
//! Comet host into decoded `VideoFrame`s the caller renders. This crate **is**
//! the client pipeline: the reference app (`crates/app`) is one consumer of it
//! (winit + D3D11/Metal), an embedder is another.
//!
//! [`Client::connect`] drives pair → launch → stream → reassemble → decode on
//! background threads and leaves the most recently decoded frame in a shared
//! slot for the caller's render loop. It pulls in no UI or audio-output
//! dependencies (no winit/wgpu/cpal/opus); raw audio datagrams are forwarded on
//! a channel for the embedder to decode.
//!
//! # Threads
//! * **receive** (one per media socket) and **control** — owned by
//!   [`StreamSession`]: blocking socket reads with a large receive buffer, and
//!   an input/feedback thread that sends the moment something is queued.
//! * **pipeline** — reassembles frames (FEC, reorder window, loss detection),
//!   decodes, publishes the frame, and asks the host for a keyframe the moment
//!   a frame is known to be lost.
//!
//! # Measuring it
//! [`Client::stats`] returns a per-stage latency timeline (p50/p99/max): time
//! to receive a frame, queueing, decode, and — once the embedder reports
//! presents via [`Client::frame_presented`] — decode-to-present and the whole
//! first-packet-to-present path. See [`ClientStats`].
//!
//! # Example
//! ```no_run
//! use starfire_client::{Client, ClientEvent, StarfireConfig};
//!
//! let mut client = Client::connect(StarfireConfig {
//!     host: "192.168.0.224".into(),
//!     pin: "1234".into(),
//!     ..Default::default()
//! });
//!
//! // Optionally take the raw-audio receiver (decode with `starfire-audio`).
//! let _audio = client.take_audio();
//!
//! let latest = client.latest();
//! loop {
//!     match client.poll_event() {
//!         Some(ClientEvent::Frame) => {
//!             if let Ok(slot) = latest.lock() {
//!                 if let Some(frame) = slot.as_ref() {
//!                     // render the frame under the lock (e.g. with starfire-render);
//!                     // on macOS `VideoFrame` isn't Clone — don't move it out.
//!                     client.frame_presented(frame.pts);
//!                 }
//!             }
//!         }
//!         Some(ClientEvent::Stopped(msg)) => {
//!             eprintln!("session stopped: {msg}");
//!             break;
//!         }
//!         None => { /* no event this tick — pump your own loop */ }
//!     }
//!     // Forward input built with `starfire_core::input::*`:
//!     // client.send_input(my_encoded_input);
//! }
//! println!("{}", client.stats());
//! ```

use std::collections::VecDeque;
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use starfire_core::launch::LaunchConfig;
use starfire_core::metrics::{LatencySeries, Summary, TransitTracker};
use starfire_core::rtsp::AnnounceConfig;
use starfire_core::session::{self, ControlHandle, NetStats, SessionEvent, StreamSession};
use starfire_core::video::reassembly::{Depacketizer, ReassemblyStats};
use starfire_core::video::{AccessUnit, Codec, FrameMeta};
use starfire_decode::select::{create_decoder, Accel};
use starfire_decode::{Decoder, VideoFrame};

/// A shared D3D11 device threaded to the decoder on Windows (the zero-copy
/// path), so decoded textures need no cross-device sharing when the caller
/// renders with the D3D11 path. `Some` ⇒ zero-copy D3D11; `None`/`()` ⇒ the
/// portable wgpu path.
#[cfg(target_os = "windows")]
type Shared = Option<starfire_decode::win_device::SharedDevice>;
#[cfg(not(target_os = "windows"))]
type Shared = ();

/// Raw audio datagrams buffered for the embedder before the oldest are dropped
/// (about 2.5 s at the 5 ms audio cadence). Bounded so an embedder that never
/// reads audio cannot grow the process.
const AUDIO_QUEUE: usize = 512;

/// Consecutive decode failures after which the decoder is treated as stuck:
/// frames are withheld until a keyframe and one is requested.
const DECODE_ERROR_STREAK: u32 = 2;

/// How long the pipeline sleeps when nothing arrives before re-checking the
/// stop flag and its timers.
const IDLE_TICK: Duration = Duration::from_millis(100);

/// How often the loss report is sent to the host.
const LOSS_REPORT_INTERVAL: Duration = Duration::from_millis(500);

/// Everything needed to start a session.
#[derive(Debug, Clone)]
pub struct StarfireConfig {
    /// Host address the client dials (an IP the host can reach back on).
    pub host: String,
    /// Pairing PIN (entered on the host out of band; this drives the ladder).
    pub pin: String,
    /// Name shown in the host's list of paired clients.
    pub device_name: String,
    /// App title to launch from the host's `/applist` (default `"Desktop"`).
    pub app_name: String,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
    /// Encoder slices per frame (`videoEncoderSlicesPerFrame`).
    pub slices: u32,
    /// FEC repair overhead percent (`x-nv-vqos[0].fec.repairPercent`).
    pub fec_percent: u32,
    /// UDP payload size for video packets (`packetSize`).
    pub packet_size: u32,
    /// Forward raw audio datagrams on the audio channel if `true`.
    pub audio: bool,
    /// Force a video codec. `None` (the default) uses what the host advertises
    /// in `/serverinfo`, falling back to HEVC.
    pub codec: Option<Codec>,
    /// Windows: decode into D3D11 textures on a device shared with the renderer
    /// (see [`Client::shared_device`]). Ignored elsewhere.
    pub zero_copy: bool,
    /// Ask the OS to schedule the receive, control and pipeline threads ahead
    /// of ordinary work (default `true`). This is what keeps the latency tail
    /// short when the machine is busy; `false` is the measurement baseline.
    /// The switch is process-wide.
    pub realtime_threads: bool,
}

impl Default for StarfireConfig {
    fn default() -> Self {
        Self {
            host: String::new(),
            pin: "1234".to_string(),
            device_name: "Starfire".to_string(),
            app_name: "Desktop".to_string(),
            width: 1920,
            height: 1080,
            fps: 60,
            bitrate_kbps: 20000,
            slices: 1,
            fec_percent: 50,
            packet_size: 1392,
            audio: true,
            codec: None,
            zero_copy: true,
            realtime_threads: true,
        }
    }
}

/// Lifecycle wake-ups from the pipeline to the embedder's loop.
#[derive(Debug)]
pub enum ClientEvent {
    /// A new decoded frame is available in the [`Client::latest`] slot.
    Frame,
    /// The session ended (setup error or teardown); message is for the log.
    Stopped(String),
}

/// A snapshot of how the stream is doing. Every latency is a
/// [`Summary`](starfire_core::metrics::Summary) (p50 / p95 / p99 / max).
///
/// The client timeline for one frame, in order:
///
/// ```text
/// first packet ─receive─► decodable ─queue─► decoder ─decode─► in hand ─present─► on screen
/// └──────────────────────── pipeline ───────────────────────────┘
/// └──────────────────────────────── total ──────────────────────────────────────┘
/// ```
#[derive(Debug, Clone, Default)]
pub struct ClientStats {
    /// Time since the session began streaming (or since [`Client::reset_stats`]).
    pub elapsed: Duration,
    pub codec: Option<Codec>,
    /// Decoded frame size.
    pub resolution: (u32, u32),
    pub frames_decoded: u64,
    pub decode_errors: u64,
    /// Keyframe requests sent to the host (each one follows a lost frame).
    pub keyframe_requests: u64,
    /// Frames the decoder was holding back, by how many frames late each
    /// output was: `[on time, 1 late, 2 late, 3+ late]`. Anything outside the
    /// first bucket is latency the decoder is adding.
    pub decoder_lag: [u64; 4],
    /// Reassembly counters (FEC recoveries, lost / skipped frames, late packets).
    pub reassembly: ReassemblyStats,
    /// Socket-level counters and the receive-buffer sizes the OS granted.
    pub net: NetStats,
    /// Control-channel round-trip time.
    pub rtt: Duration,
    /// Raw audio datagrams dropped because the embedder was not reading them.
    pub audio_dropped: u64,
    /// First packet of a frame → frame decodable (network + pacing spread).
    pub receive: Summary,
    /// Frame decodable → handed to the decoder (waiting behind the pipeline).
    pub queue: Summary,
    /// Handed to the decoder → decoded frame in hand.
    pub decode: Summary,
    /// First packet → decoded frame published to the render slot.
    pub pipeline: Summary,
    /// Published → presented, as reported by [`Client::frame_presented`].
    pub present: Summary,
    /// First packet → presented: everything the client adds.
    pub total: Summary,
    /// One-way delay above the path's best case (queueing / Wi-Fi retries),
    /// measured from the host's per-frame send stamps — no clock sync needed.
    pub transit: Summary,
    /// Host capture → sent latency, as reported in each frame's header.
    pub host: Summary,
    /// Interval between published frames (pacing; ideal = 1000 / fps).
    pub interval: Summary,
}

impl ClientStats {
    /// Decoded frames per second over the measurement window.
    pub fn fps(&self) -> f64 {
        let secs = self.elapsed.as_secs_f64();
        if secs > 0.0 {
            self.frames_decoded as f64 / secs
        } else {
            0.0
        }
    }

    /// Received video bitrate in Mbit/s over the measurement window.
    pub fn mbps(&self) -> f64 {
        let secs = self.elapsed.as_secs_f64();
        if secs > 0.0 {
            self.net.video_bytes as f64 * 8.0 / secs / 1e6
        } else {
            0.0
        }
    }
}

impl fmt::Display for ClientStats {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let r = &self.reassembly;
        writeln!(
            f,
            "========= STARFIRE CLIENT ({:.1}s) =========",
            self.elapsed.as_secs_f64()
        )?;
        writeln!(
            f,
            "stream        : {:?} {}x{}  {:.1} fps  {:.1} Mbps  rtt {:.1} ms",
            self.codec.unwrap_or(Codec::Hevc),
            self.resolution.0,
            self.resolution.1,
            self.fps(),
            self.mbps(),
            self.rtt.as_secs_f64() * 1000.0
        )?;
        writeln!(
            f,
            "frames        : decoded {}  lost {}  skipped {}  fec-recovered {} ({} shards)  decode errors {}  keyframe requests {}",
            self.frames_decoded,
            r.frames_lost,
            r.frames_skipped,
            r.frames_recovered,
            r.shards_recovered,
            self.decode_errors,
            self.keyframe_requests
        )?;
        writeln!(
            f,
            "packets       : {} video  {} after-complete  {} duplicate  {} malformed  {} foreign  (rcvbuf {} KB)",
            self.net.video_packets,
            r.late_packets,
            r.duplicate_packets,
            r.malformed_packets,
            self.net.foreign_packets,
            self.net.video_rcvbuf / 1024
        )?;
        writeln!(
            f,
            "decoder lag   : on time {}  1 frame late {}  2 late {}  3+ late {}",
            self.decoder_lag[0], self.decoder_lag[1], self.decoder_lag[2], self.decoder_lag[3]
        )?;
        writeln!(f, "host          : {}", self.host)?;
        writeln!(f, "net above min : {}", self.transit)?;
        writeln!(f, "receive       : {}", self.receive)?;
        writeln!(f, "queue         : {}", self.queue)?;
        writeln!(f, "decode        : {}", self.decode)?;
        writeln!(f, "pipeline      : {}", self.pipeline)?;
        writeln!(f, "present       : {}", self.present)?;
        writeln!(f, "client total  : {}", self.total)?;
        writeln!(f, "frame interval: {}", self.interval)?;
        write!(f, "=============================================")
    }
}

/// Mutable measurement state shared between the pipeline and the embedder.
struct StatsState {
    started: Instant,
    codec: Option<Codec>,
    resolution: (u32, u32),
    frames_decoded: u64,
    decode_errors: u64,
    keyframe_requests: u64,
    decoder_lag: [u64; 4],
    reassembly: ReassemblyStats,
    /// Counter baselines taken at the last reset (the sources are cumulative).
    reassembly_base: ReassemblyStats,
    net: NetStats,
    net_base: NetStats,
    rtt: Duration,
    audio_dropped: u64,
    receive: LatencySeries,
    queue: LatencySeries,
    decode: LatencySeries,
    pipeline: LatencySeries,
    present: LatencySeries,
    total: LatencySeries,
    transit: LatencySeries,
    host: LatencySeries,
    interval: LatencySeries,
    last_publish: Option<Instant>,
    /// Recently published frames awaiting their present report:
    /// `(pts, first packet, published)`.
    recent: VecDeque<(i64, Instant, Instant)>,
}

impl StatsState {
    fn new() -> Self {
        Self {
            started: Instant::now(),
            codec: None,
            resolution: (0, 0),
            frames_decoded: 0,
            decode_errors: 0,
            keyframe_requests: 0,
            decoder_lag: [0; 4],
            reassembly: ReassemblyStats::default(),
            reassembly_base: ReassemblyStats::default(),
            net: NetStats::default(),
            net_base: NetStats::default(),
            rtt: Duration::ZERO,
            audio_dropped: 0,
            receive: LatencySeries::new(),
            queue: LatencySeries::new(),
            decode: LatencySeries::new(),
            pipeline: LatencySeries::new(),
            present: LatencySeries::new(),
            total: LatencySeries::new(),
            transit: LatencySeries::new(),
            host: LatencySeries::new(),
            interval: LatencySeries::new(),
            last_publish: None,
            recent: VecDeque::with_capacity(RECENT_FRAMES),
        }
    }

    fn reset(&mut self) {
        let (codec, resolution, rtt) = (self.codec, self.resolution, self.rtt);
        let (reassembly, net) = (self.reassembly, self.net);
        *self = Self::new();
        self.codec = codec;
        self.resolution = resolution;
        self.rtt = rtt;
        self.reassembly = reassembly;
        self.reassembly_base = reassembly;
        self.net = net;
        self.net_base = net;
    }

    fn snapshot(&self) -> ClientStats {
        let (r, b) = (&self.reassembly, &self.reassembly_base);
        let (n, nb) = (&self.net, &self.net_base);
        ClientStats {
            elapsed: self.started.elapsed(),
            codec: self.codec,
            resolution: self.resolution,
            frames_decoded: self.frames_decoded,
            decode_errors: self.decode_errors,
            keyframe_requests: self.keyframe_requests,
            decoder_lag: self.decoder_lag,
            reassembly: ReassemblyStats {
                packets: r.packets - b.packets,
                frames_delivered: r.frames_delivered - b.frames_delivered,
                frames_recovered: r.frames_recovered - b.frames_recovered,
                shards_recovered: r.shards_recovered - b.shards_recovered,
                frames_lost: r.frames_lost - b.frames_lost,
                frames_skipped: r.frames_skipped - b.frames_skipped,
                late_packets: r.late_packets - b.late_packets,
                duplicate_packets: r.duplicate_packets - b.duplicate_packets,
                malformed_packets: r.malformed_packets - b.malformed_packets,
            },
            net: NetStats {
                video_packets: n.video_packets - nb.video_packets,
                video_bytes: n.video_bytes - nb.video_bytes,
                audio_packets: n.audio_packets - nb.audio_packets,
                foreign_packets: n.foreign_packets - nb.foreign_packets,
                video_rcvbuf: n.video_rcvbuf,
                audio_rcvbuf: n.audio_rcvbuf,
            },
            rtt: self.rtt,
            audio_dropped: self.audio_dropped,
            receive: self.receive.summary(),
            queue: self.queue.summary(),
            decode: self.decode.summary(),
            pipeline: self.pipeline.summary(),
            present: self.present.summary(),
            total: self.total.summary(),
            transit: self.transit.summary(),
            host: self.host.summary(),
            interval: self.interval.summary(),
        }
    }
}

/// Published frames remembered for present-time matching.
const RECENT_FRAMES: usize = 32;

/// How lifecycle events reach the embedder.
enum Notify {
    /// Polled through [`Client::poll_event`]. `frame_pending` collapses a run of
    /// frame wake-ups into one queued event, so an embedder that polls slowly
    /// cannot grow the queue.
    Channel {
        tx: Sender<ClientEvent>,
        frame_pending: Arc<AtomicBool>,
    },
    /// Called on the pipeline thread (e.g. to wake a UI event loop).
    Callback(Box<dyn Fn(ClientEvent) + Send>),
}

impl Notify {
    /// Returns `false` when the embedder is gone.
    fn frame(&self) -> bool {
        match self {
            Notify::Channel { tx, frame_pending } => {
                if frame_pending.swap(true, Ordering::AcqRel) {
                    true // a wake-up is already queued
                } else {
                    tx.send(ClientEvent::Frame).is_ok()
                }
            }
            Notify::Callback(f) => {
                f(ClientEvent::Frame);
                true
            }
        }
    }

    fn stopped(&self, why: String) {
        match self {
            Notify::Channel { tx, .. } => {
                let _ = tx.send(ClientEvent::Stopped(why));
            }
            Notify::Callback(f) => f(ClientEvent::Stopped(why)),
        }
    }
}

/// An embeddable Starfire client. [`Client::connect`] spawns the full pipeline
/// on background threads; the caller polls events and presents [`latest`].
///
/// [`latest`]: Client::latest
pub struct Client {
    /// Most-recently decoded frame, shared with the render loop.
    latest: Arc<Mutex<Option<VideoFrame>>>,
    /// Lifecycle events (`None` when delivered through a callback instead).
    event_rx: Option<Receiver<ClientEvent>>,
    frame_pending: Arc<AtomicBool>,
    /// The session's control channel, once it is up.
    control: Arc<OnceLock<ControlHandle>>,
    /// Raw audio datagrams (taken once by the embedder, if `cfg.audio`).
    audio_rx: Option<Receiver<Vec<u8>>>,
    /// The decode device (Windows D3D11 zero-copy path), shared with the caller's
    /// renderer via [`shared_device`](Client::shared_device). `()` elsewhere.
    shared: Shared,
    stats: Arc<Mutex<StatsState>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Client {
    /// Pair + launch + stream in the background; returns immediately. Events are
    /// read with [`poll_event`](Client::poll_event).
    pub fn connect(cfg: StarfireConfig) -> Client {
        let (tx, rx) = std::sync::mpsc::channel::<ClientEvent>();
        let frame_pending = Arc::new(AtomicBool::new(false));
        let notify = Notify::Channel {
            tx,
            frame_pending: frame_pending.clone(),
        };
        Self::spawn(cfg, notify, Some(rx), frame_pending)
    }

    /// Like [`connect`](Client::connect), but each event is delivered by calling
    /// `on_event` on the pipeline thread — the way to wake a UI event loop the
    /// instant a frame is decoded, with no polling interval. Keep the callback
    /// short: it runs between decode and the next packet.
    pub fn connect_with(
        cfg: StarfireConfig,
        on_event: impl Fn(ClientEvent) + Send + 'static,
    ) -> Client {
        let notify = Notify::Callback(Box::new(on_event));
        Self::spawn(cfg, notify, None, Arc::new(AtomicBool::new(false)))
    }

    fn spawn(
        cfg: StarfireConfig,
        notify: Notify,
        event_rx: Option<Receiver<ClientEvent>>,
        frame_pending: Arc<AtomicBool>,
    ) -> Client {
        starfire_rt::set_enabled(cfg.realtime_threads);
        let latest: Arc<Mutex<Option<VideoFrame>>> = Arc::new(Mutex::new(None));
        let (audio_tx, audio_rx) = if cfg.audio {
            let (tx, rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(AUDIO_QUEUE);
            (Some(tx), Some(rx))
        } else {
            (None, None)
        };

        // Create the decode device up front so it can be shared with both the
        // decoder (on the pipeline thread) and the caller's renderer.
        #[cfg(target_os = "windows")]
        let shared: Shared = if cfg.zero_copy {
            starfire_decode::win_device::SharedDevice::create().ok()
        } else {
            None
        };
        #[cfg(not(target_os = "windows"))]
        let shared: Shared = ();

        let control = Arc::new(OnceLock::new());
        let stats = Arc::new(Mutex::new(StatsState::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let pipeline = Pipeline {
            cfg,
            latest: latest.clone(),
            notify,
            audio_tx,
            shared: shared.clone(),
            control: control.clone(),
            stats: stats.clone(),
            stop: stop.clone(),
        };
        let thread = Some(thread::spawn(move || pipeline.run()));

        Client {
            latest,
            event_rx,
            frame_pending,
            control,
            audio_rx,
            shared,
            stats,
            stop,
            thread,
        }
    }

    /// Shared slot holding the most-recently decoded frame — lock it in your
    /// render loop and present it (e.g. with `starfire-render`). On macOS
    /// `VideoFrame` isn't Clone, so render under the lock; don't move it out.
    pub fn latest(&self) -> Arc<Mutex<Option<VideoFrame>>> {
        self.latest.clone()
    }

    /// Non-blocking: next lifecycle event (Frame produced / Stopped). Always
    /// `None` for a client built with [`connect_with`](Client::connect_with).
    pub fn poll_event(&self) -> Option<ClientEvent> {
        let ev = self.event_rx.as_ref()?.try_recv().ok()?;
        if matches!(ev, ClientEvent::Frame) {
            self.frame_pending.store(false, Ordering::Release);
        }
        Some(ev)
    }

    /// Send one encoded input message (build with `starfire_core::input::*`). It
    /// goes straight to the control thread and onto the wire; it does not wait
    /// behind video. Dropped if the session is not up yet.
    pub fn send_input(&self, msg: Vec<u8>) {
        if let Some(control) = self.control.get() {
            control.send_input(msg);
        }
    }

    /// Ask the host for a keyframe now (the pipeline already does this by itself
    /// when it detects a lost frame).
    pub fn request_keyframe(&self) {
        if let Some(control) = self.control.get() {
            control.request_idr();
        }
    }

    /// Tell the client that the frame with this `pts` has just been presented.
    /// Feeds the `present` and `total` latency series in [`stats`](Client::stats);
    /// call it right after your present/swap returns.
    pub fn frame_presented(&self, pts: i64) {
        let now = Instant::now();
        if let Ok(mut st) = self.stats.lock() {
            if let Some(pos) = st.recent.iter().position(|&(p, _, _)| p == pts) {
                if let Some((_, first_packet, published)) = st.recent.remove(pos) {
                    st.present.record_span(published, now);
                    st.total.record_span(first_packet, now);
                }
                // Anything older was superseded before it could be shown.
                st.recent.retain(|&(p, _, _)| p > pts);
            }
        }
    }

    /// A snapshot of the stream's counters and latency timeline.
    pub fn stats(&self) -> ClientStats {
        match self.stats.lock() {
            Ok(st) => st.snapshot(),
            Err(_) => ClientStats::default(),
        }
    }

    /// Start a fresh measurement window (e.g. after the stream has settled).
    pub fn reset_stats(&self) {
        if let Ok(mut st) = self.stats.lock() {
            st.reset();
        }
    }

    /// Take the raw-audio-datagram receiver (Some once, if `cfg.audio`). Decode
    /// with `starfire-audio` on your side. The queue is bounded: if it is not
    /// drained, the oldest audio is dropped rather than buffered without limit.
    pub fn take_audio(&mut self) -> Option<Receiver<Vec<u8>>> {
        self.audio_rx.take()
    }

    /// The Windows D3D11 decode device (`None` if it couldn't be created). On
    /// Windows decoded frames are GPU textures on this device, so build the
    /// zero-copy renderer on the same device:
    /// `starfire_render::new_d3d11_for_window(&window, client.shared_device()?, w, h)`.
    /// (macOS needs no equivalent — its frames are IOSurface-backed and import
    /// into any Metal device, so `starfire_render::new_for_window` just works.)
    #[cfg(target_os = "windows")]
    pub fn shared_device(&self) -> Option<starfire_decode::win_device::SharedDevice> {
        self.shared.clone()
    }

    /// End the session and wait for the pipeline to finish tearing down (which
    /// tells the host to stop the stream). Dropping the client also ends the
    /// session, but without waiting.
    pub fn stop(mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
    }
}

/// Build the launch + announce configs from `cfg`.
fn stream_configs(cfg: &StarfireConfig) -> (LaunchConfig, AnnounceConfig) {
    let ad = AnnounceConfig::default();
    let announce = AnnounceConfig {
        width: cfg.width,
        height: cfg.height,
        fps: cfg.fps,
        bitrate_kbps: cfg.bitrate_kbps,
        slices_per_frame: cfg.slices,
        fec_percent: cfg.fec_percent,
        packet_size: cfg.packet_size,
        encryption_enabled: ad.encryption_enabled,
        recv_buffer_bytes: 0, // the session fills in the size the OS granted
    };
    let launch = LaunchConfig {
        width: cfg.width,
        height: cfg.height,
        fps: cfg.fps,
        ..LaunchConfig::default()
    };
    (launch, announce)
}

/// A frame handed to the decoder and not yet seen coming out.
struct Submitted {
    frame_index: u32,
    meta: FrameMeta,
    host_latency_tenths_ms: u16,
    submitted_at: Instant,
}

/// Decides when to ask the host for a keyframe. A request goes out the moment
/// one is needed, and is repeated only if no keyframe has arrived after the
/// time it should have taken (a round trip plus a couple of frames) — so a
/// lost request or a lost keyframe is retried, but a keyframe already in flight
/// is not requested twice.
struct KeyframeRequester {
    needed: bool,
    last: Option<Instant>,
    frame_interval: Duration,
}

impl KeyframeRequester {
    fn new(fps: u32) -> Self {
        Self {
            needed: false,
            last: None,
            frame_interval: Duration::from_secs_f64(1.0 / fps.max(1) as f64),
        }
    }

    fn need(&mut self) {
        self.needed = true;
    }

    fn satisfied(&mut self) {
        self.needed = false;
    }

    /// `true` if a request should be sent now.
    fn due(&mut self, now: Instant, rtt: Duration) -> bool {
        if !self.needed {
            return false;
        }
        let retry = (rtt * 2 + self.frame_interval * 3).max(Duration::from_millis(50));
        match self.last {
            Some(t) if now.saturating_duration_since(t) < retry => false,
            _ => {
                self.last = Some(now);
                true
            }
        }
    }
}

/// The pipeline thread's state: pair → launch → stream → reassemble → decode.
struct Pipeline {
    cfg: StarfireConfig,
    latest: Arc<Mutex<Option<VideoFrame>>>,
    notify: Notify,
    audio_tx: Option<SyncSender<Vec<u8>>>,
    shared: Shared,
    control: Arc<OnceLock<ControlHandle>>,
    stats: Arc<Mutex<StatsState>>,
    stop: Arc<AtomicBool>,
}

impl Pipeline {
    fn run(self) {
        // Reassembly + decode is bounded per-frame work that must not sit behind
        // whatever else the machine is doing.
        starfire_rt::promote_current_thread(starfire_rt::Role::Frame);
        if let Err(why) = self.stream() {
            self.notify.stopped(why);
        }
    }

    fn make_decoder(&self, codec: Codec) -> Result<Box<dyn Decoder>, String> {
        // On Windows, build the decoder on the shared D3D11 device (zero-copy
        // textures the D3D11 renderer can sample); otherwise the portable factory.
        #[cfg(target_os = "windows")]
        let made = match &self.shared {
            Some(dev) => {
                starfire_decode::backend::mediafoundation::MediaFoundationDecoder::with_device(
                    codec,
                    dev.clone(),
                )
                .map(|d| Box::new(d) as Box<dyn Decoder>)
            }
            None => create_decoder(codec, Accel::PreferHardware),
        };
        #[cfg(not(target_os = "windows"))]
        let made = {
            let _ = &self.shared;
            create_decoder(codec, Accel::PreferHardware)
        };
        made.map_err(|e| format!("no video decoder on this platform: {e}"))
    }

    /// Runs until the session ends; `Err` carries the reason for the embedder.
    fn stream(&self) -> Result<(), String> {
        let cfg = &self.cfg;
        let client = session::pair(&cfg.host, &cfg.device_name, &cfg.pin)
            .map_err(|e| format!("pair: {e}"))?;
        let apps = client.applist().map_err(|e| format!("applist: {e}"))?;
        let app = apps
            .iter()
            .find(|a| a.title == cfg.app_name)
            .map(|a| a.id.clone())
            .ok_or_else(|| {
                format!(
                    "app {:?} not found in {:?}",
                    cfg.app_name,
                    apps.iter().map(|a| &a.title).collect::<Vec<_>>()
                )
            })?;

        // The host advertises (via `<VideoCodec>`) the codec it will actually
        // send — HEVC normally, H264 when it has no working hardware HEVC
        // encoder. An explicit `cfg.codec` overrides; unknown → HEVC.
        let codec = cfg
            .codec
            .or_else(|| client.server_info().ok().and_then(|i| i.negotiated_codec()))
            .unwrap_or(Codec::Hevc);

        let (launch_cfg, announce_cfg) = stream_configs(cfg);
        let (events_tx, events) = std::sync::mpsc::channel::<SessionEvent>();
        let sess = StreamSession::start(
            client,
            &cfg.host,
            &app,
            &launch_cfg,
            &announce_cfg,
            events_tx,
        )
        .map_err(|e| format!("session start: {e}"))?;
        let control = sess.control();
        let _ = self.control.set(control.clone());

        let mut decoder = self.make_decoder(codec)?;
        // Gate on keyframes: start at the first keyframe, and after a lost frame
        // withhold everything that depends on it until the next keyframe.
        let mut dep = Depacketizer::new(codec).gate_on_keyframes(true);

        if let Ok(mut st) = self.stats.lock() {
            st.reset();
            st.codec = Some(codec);
        }

        let mut submitted: VecDeque<Submitted> = VecDeque::with_capacity(8);
        let mut transit = TransitTracker::new();
        let mut keyframes = KeyframeRequester::new(cfg.fps);
        let mut consecutive_errors = 0u32;
        let mut last_decoded: Option<u32> = None;
        let mut loss_window = 0i32;
        let mut last_loss_report = Instant::now();
        let mut audio_dropped = 0u64;

        loop {
            if self.stop.load(Ordering::Acquire) {
                return Ok(());
            }
            let event = match events.recv_timeout(IDLE_TICK) {
                Ok(ev) => Some(ev),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => None,
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    return Err("the stream ended".into());
                }
            };
            match event {
                Some(SessionEvent::Video { at, data }) => {
                    let au = dep.push_at(&data, at);
                    sess.recycle(data);
                    if let Some(loss) = dep.take_loss() {
                        loss_window = loss_window.saturating_add(loss.frames() as i32);
                        keyframes.need();
                    }
                    if let Some(au) = au {
                        if au.is_keyframe {
                            keyframes.satisfied();
                        }
                        let ok = self.decode(
                            &mut *decoder,
                            au,
                            &mut submitted,
                            &mut transit,
                            &mut last_decoded,
                        );
                        match ok {
                            DecodeOutcome::Published => {
                                consecutive_errors = 0;
                                if !self.notify.frame() {
                                    return Ok(()); // embedder dropped the client
                                }
                            }
                            DecodeOutcome::NoOutput => consecutive_errors = 0,
                            DecodeOutcome::Failed => {
                                consecutive_errors += 1;
                                if consecutive_errors >= DECODE_ERROR_STREAK {
                                    // The decoder has lost its references: stop
                                    // feeding it predicted frames and re-key.
                                    dep.require_keyframe();
                                    keyframes.need();
                                    consecutive_errors = 0;
                                }
                            }
                        }
                    } else if dep.awaiting_keyframe() && dep.stats().frames_skipped > 0 {
                        // Complete frames are being withheld: only a keyframe
                        // gets the picture moving again.
                        keyframes.need();
                    }
                }
                Some(SessionEvent::Audio { data, .. }) => {
                    if let Some(tx) = &self.audio_tx {
                        match tx.try_send(data) {
                            Ok(()) => {}
                            Err(TrySendError::Full(_)) => audio_dropped += 1,
                            Err(TrySendError::Disconnected(_)) => {}
                        }
                    }
                }
                Some(SessionEvent::Disconnected(why)) => return Err(why),
                None => {}
            }

            let now = Instant::now();
            let rtt = control.rtt();
            if keyframes.due(now, rtt) {
                control.request_idr();
                if let Ok(mut st) = self.stats.lock() {
                    st.keyframe_requests += 1;
                }
            }
            // Periodic LOSS_STATS to the host (drives its link estimator) and a
            // refresh of the cumulative counters the snapshot reads.
            let since = now.saturating_duration_since(last_loss_report);
            if since >= LOSS_REPORT_INTERVAL {
                control.send_loss_stats(
                    loss_window,
                    since.as_millis() as i32,
                    last_decoded.unwrap_or(0) as i32,
                );
                loss_window = 0;
                last_loss_report = now;
                if let Ok(mut st) = self.stats.lock() {
                    st.reassembly = dep.stats();
                    st.net = sess.net_stats();
                    st.rtt = rtt;
                    st.audio_dropped = audio_dropped;
                }
            }
        }
    }

    /// Decode one access unit, publish the resulting frame, record its timeline.
    fn decode(
        &self,
        decoder: &mut dyn Decoder,
        au: AccessUnit,
        submitted: &mut VecDeque<Submitted>,
        transit: &mut TransitTracker,
        last_decoded: &mut Option<u32>,
    ) -> DecodeOutcome {
        let submitted_at = Instant::now();
        // Network delay above the path floor, taken when the frame became
        // decodable (its last needed packet). The sender-side reference is the
        // moment the host packetized the frame: its capture stamp plus the host
        // latency it reports for this frame (0.1 ms = 9 ticks of the 90 kHz
        // clock). Using the capture stamp alone would fold the host's own
        // encode-time variation into what is reported as network delay.
        let sent_stamp = au
            .meta
            .rtp_timestamp
            .wrapping_add(au.host_latency_tenths_ms as u32 * 9);
        let above_floor = au
            .meta
            .complete_at
            .map(|at| transit.observe(sent_stamp, at));
        if submitted.len() >= 8 {
            submitted.pop_front(); // the decoder never returned it
        }
        submitted.push_back(Submitted {
            frame_index: au.frame_index,
            meta: au.meta,
            host_latency_tenths_ms: au.host_latency_tenths_ms,
            submitted_at,
        });

        let frame = match decoder.push(&au) {
            Ok(Some(frame)) => frame,
            Ok(None) => {
                if let (Ok(mut st), Some(d)) = (self.stats.lock(), above_floor) {
                    st.transit.record(d);
                }
                return DecodeOutcome::NoOutput;
            }
            Err(e) => {
                submitted.pop_back();
                if let Ok(mut st) = self.stats.lock() {
                    st.decode_errors += 1;
                    if st.decode_errors <= 3 {
                        eprintln!("[starfire] decode error on frame {}: {e}", au.frame_index);
                    }
                }
                return DecodeOutcome::Failed;
            }
        };
        let decoded_at = Instant::now();

        // Match the output to the access unit it was decoded from (its pts is
        // that frame index). A decoder that buffers returns an older frame.
        while submitted
            .front()
            .is_some_and(|s| (s.frame_index as i64) < frame.pts)
        {
            submitted.pop_front();
        }
        let source = match submitted.front() {
            Some(s) if s.frame_index as i64 == frame.pts => submitted.pop_front(),
            _ => None,
        };
        let lag = (au.frame_index as i64 - frame.pts).clamp(0, 3) as usize;
        let (pts, dims) = (frame.pts, (frame.width, frame.height));
        *last_decoded = Some(au.frame_index);

        if let Ok(mut slot) = self.latest.lock() {
            *slot = Some(frame);
        }
        let published_at = Instant::now();

        if let Ok(mut st) = self.stats.lock() {
            st.frames_decoded += 1;
            st.resolution = dims;
            if pts >= 0 {
                st.decoder_lag[lag] += 1; // a negative pts means the decoder lost the stamp
            }
            if let Some(d) = above_floor {
                st.transit.record(d);
            }
            if let Some(last) = st.last_publish {
                st.interval.record_span(last, published_at);
            }
            st.last_publish = Some(published_at);
            if let Some(src) = source {
                st.host.record_us(src.host_latency_tenths_ms as u64 * 100);
                st.decode.record_span(src.submitted_at, decoded_at);
                if let (Some(first), Some(complete)) =
                    (src.meta.first_packet_at, src.meta.complete_at)
                {
                    st.receive.record_span(first, complete);
                    st.queue.record_span(complete, src.submitted_at);
                    st.pipeline.record_span(first, published_at);
                    if st.recent.len() >= RECENT_FRAMES {
                        st.recent.pop_front();
                    }
                    st.recent.push_back((pts, first, published_at));
                }
            }
        }
        DecodeOutcome::Published
    }
}

enum DecodeOutcome {
    Published,
    NoOutput,
    Failed,
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn keyframe_is_requested_at_once_then_only_after_it_is_overdue() {
        let mut k = KeyframeRequester::new(60);
        let t0 = Instant::now();
        let rtt = Duration::from_millis(10);
        assert!(!k.due(t0, rtt), "nothing needed yet");
        k.need();
        assert!(k.due(t0, rtt), "first request is immediate");
        // A keyframe takes a round trip plus a few frames: 2*10 + 3*16.7 = 70 ms.
        assert!(
            !k.due(t0 + Duration::from_millis(40), rtt),
            "still in flight"
        );
        assert!(
            k.due(t0 + Duration::from_millis(71), rtt),
            "overdue: ask again"
        );
        k.satisfied();
        assert!(!k.due(t0 + Duration::from_secs(5), rtt), "keyframe arrived");
    }

    #[test]
    fn keyframe_retry_never_drops_below_the_floor() {
        let mut k = KeyframeRequester::new(240);
        let t0 = Instant::now();
        k.need();
        assert!(k.due(t0, Duration::ZERO));
        // 2*0 + 3*4.2 ms = 12.5 ms would hammer the host; the floor is 50 ms.
        assert!(!k.due(t0 + Duration::from_millis(30), Duration::ZERO));
        assert!(k.due(t0 + Duration::from_millis(50), Duration::ZERO));
    }

    /// A slow poller sees one queued frame wake-up, not one per decoded frame.
    #[test]
    fn frame_wakeups_are_coalesced_for_polling_embedders() {
        let (tx, rx) = std::sync::mpsc::channel();
        let pending = Arc::new(AtomicBool::new(false));
        let n = Notify::Channel {
            tx,
            frame_pending: pending.clone(),
        };
        for _ in 0..1000 {
            assert!(n.frame());
        }
        assert!(matches!(rx.try_recv(), Ok(ClientEvent::Frame)));
        assert!(
            rx.try_recv().is_err(),
            "1000 frames queued exactly one wake-up"
        );
        // Once the embedder has taken it, the next frame queues a new one.
        pending.store(false, Ordering::Release);
        assert!(n.frame());
        assert!(matches!(rx.try_recv(), Ok(ClientEvent::Frame)));
        // A stop is never coalesced away.
        n.stopped("bye".into());
        assert!(matches!(rx.try_recv(), Ok(ClientEvent::Stopped(m)) if m == "bye"));
    }

    #[test]
    fn frame_wakeup_reports_a_departed_embedder() {
        let (tx, rx) = std::sync::mpsc::channel();
        drop(rx);
        let n = Notify::Channel {
            tx,
            frame_pending: Arc::new(AtomicBool::new(false)),
        };
        assert!(!n.frame());
    }

    /// Resetting starts a new window for the cumulative counters too.
    #[test]
    fn reset_rebaselines_cumulative_counters() {
        let mut st = StatsState::new();
        st.reassembly.frames_lost = 7;
        st.net.video_packets = 1000;
        st.net.video_rcvbuf = 4096;
        st.frames_decoded = 50;
        st.receive.record_us(900);
        st.reset();
        st.reassembly.frames_lost = 9; // two more since the reset
        st.net.video_packets = 1300;
        let snap = st.snapshot();
        assert_eq!(snap.reassembly.frames_lost, 2);
        assert_eq!(snap.net.video_packets, 300);
        assert_eq!(snap.net.video_rcvbuf, 4096, "a size, not a counter");
        assert_eq!(snap.frames_decoded, 0);
        assert_eq!(snap.receive.count, 0);
    }

    #[test]
    fn stats_report_renders() {
        let st = StatsState::new();
        let text = st.snapshot().to_string();
        assert!(text.contains("client total"));
        assert!(text.contains("decoder lag"));
    }
}
