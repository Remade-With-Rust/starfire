// SPDX-License-Identifier: Apache-2.0
//! Embeddable Starfire client — pair, stream, and hardware-decode a Sunshine/
//! Comet host into decoded `VideoFrame`s the caller renders. The reference app
//! (`crates/app`) uses winit+wgpu/Metal; embedders use this `Client` and render
//! the latest frame with `starfire-render` (or their own surface).
//!
//! This crate is the headless equivalent of the reference app's `run_session`:
//! it drives the full pipeline (pair → launch → stream → depacketize →
//! hardware-decode) on a background thread, leaving the most-recently decoded
//! frame in a shared slot for the caller's render loop. It deliberately pulls in
//! no UI/audio-output dependencies (no winit/wgpu/cpal/opus); raw audio
//! datagrams are forwarded on a channel for the embedder to decode.
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
//!                 if let Some(_frame) = slot.as_ref() {
//!                     // render the frame under the lock (e.g. with starfire-render);
//!                     // on macOS `VideoFrame` isn't Clone — don't move it out.
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
//! ```

use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use starfire_core::launch::LaunchConfig;
use starfire_core::rtsp::AnnounceConfig;
use starfire_core::session::{self, StreamSession};
use starfire_core::video::reassembly::Depacketizer;
use starfire_core::video::Codec;
use starfire_decode::select::{create_decoder, Accel};
use starfire_decode::VideoFrame;

/// A shared D3D11 device threaded to the decoder on Windows (the zero-copy
/// path), so decoded textures need no cross-device sharing when the caller
/// renders with the D3D11 path. `Some` ⇒ zero-copy D3D11; `None`/`()` ⇒ the
/// portable wgpu path. Mirrors the reference app's `Shared` alias.
#[cfg(target_os = "windows")]
type Shared = Option<starfire_decode::win_device::SharedDevice>;
#[cfg(not(target_os = "windows"))]
type Shared = ();

/// All the stream knobs the reference app reads from `STARFIRE_*` env vars,
/// surfaced as a struct so embedders configure them in code.
pub struct StarfireConfig {
    /// Host IP the host can reach back on (not loopback).
    pub host: String,
    /// Pairing PIN (entered on the host out of band; this drives the ladder).
    pub pin: String,
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
}

impl Default for StarfireConfig {
    fn default() -> Self {
        Self {
            host: String::new(),
            pin: "1234".to_string(),
            app_name: "Desktop".to_string(),
            width: 1920,
            height: 1080,
            fps: 60,
            bitrate_kbps: 20000,
            slices: 1,
            fec_percent: 50,
            packet_size: 1392,
            audio: true,
        }
    }
}

/// Lifecycle wake-ups from the network/decode thread to the embedder's loop.
#[derive(Debug)]
pub enum ClientEvent {
    /// A new decoded frame is available in the [`Client::latest`] slot.
    Frame,
    /// The session ended (setup error or teardown); message is for the log.
    Stopped(String),
}

/// An embeddable Starfire client. [`Client::connect`] spawns the full pipeline
/// on a background thread; the caller polls events and presents [`latest`].
///
/// [`latest`]: Client::latest
pub struct Client {
    /// Most-recently decoded frame, shared with the render loop.
    latest: Arc<Mutex<Option<VideoFrame>>>,
    /// Lifecycle events (Frame produced / Stopped).
    event_rx: Receiver<ClientEvent>,
    /// Encoded input messages → network thread → control channel.
    input_tx: Sender<Vec<u8>>,
    /// Raw audio datagrams (taken once by the embedder, if `cfg.audio`).
    audio_rx: Option<Receiver<Vec<u8>>>,
    /// Background pipeline thread (kept alive for the client's lifetime).
    _thread: JoinHandle<()>,
}

impl Client {
    /// Pair + launch + stream on a background thread; returns immediately.
    pub fn connect(cfg: StarfireConfig) -> Client {
        let latest: Arc<Mutex<Option<VideoFrame>>> = Arc::new(Mutex::new(None));
        let (event_tx, event_rx) = std::sync::mpsc::channel::<ClientEvent>();
        let (input_tx, input_rx) = std::sync::mpsc::channel::<Vec<u8>>();
        let (audio_tx, audio_rx) = if cfg.audio {
            let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
            (Some(tx), Some(rx))
        } else {
            (None, None)
        };

        let latest_thread = latest.clone();
        let _thread = thread::spawn(move || {
            run_session(cfg, latest_thread, event_tx, input_rx, audio_tx);
        });

        Client {
            latest,
            event_rx,
            input_tx,
            audio_rx,
            _thread,
        }
    }

    /// Shared slot holding the most-recently decoded frame — lock it in your
    /// render loop and present it (e.g. with `starfire-render`). On macOS
    /// `VideoFrame` isn't Clone, so render under the lock; don't move it out.
    pub fn latest(&self) -> Arc<Mutex<Option<VideoFrame>>> {
        self.latest.clone()
    }

    /// Non-blocking: next lifecycle event (Frame produced / Stopped).
    pub fn poll_event(&self) -> Option<ClientEvent> {
        self.event_rx.try_recv().ok()
    }

    /// Send one encoded input message (build with `starfire_core::input::*`).
    pub fn send_input(&self, msg: Vec<u8>) {
        let _ = self.input_tx.send(msg);
    }

    /// Take the raw-audio-datagram receiver (Some once, if `cfg.audio`). Decode
    /// with `starfire-audio` on your side.
    pub fn take_audio(&mut self) -> Option<Receiver<Vec<u8>>> {
        self.audio_rx.take()
    }
}

/// Build the launch + announce configs from `cfg` — mirrors the reference app's
/// `stream_configs()`, with the `STARFIRE_*` env reads replaced by `cfg` fields.
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
    };
    let launch = LaunchConfig {
        width: cfg.width,
        height: cfg.height,
        fps: cfg.fps,
        ..LaunchConfig::default()
    };
    (launch, announce)
}

/// The full client bring-up + ingest loop, run on its own thread by
/// [`Client::connect`]. Mirrors the reference app's `run_session`, minus the
/// bench/audio-playback paths: decoded frames land in `latest`; each lifecycle
/// event is delivered via `event_tx`; raw audio datagrams are forwarded on
/// `audio_tx` when present.
fn run_session(
    cfg: StarfireConfig,
    latest: Arc<Mutex<Option<VideoFrame>>>,
    event_tx: Sender<ClientEvent>,
    input_rx: Receiver<Vec<u8>>,
    audio_tx: Option<Sender<Vec<u8>>>,
) {
    // Create the shared D3D11 device up front (Windows zero-copy path), so it
    // exists before the decoder is built; `()` elsewhere.
    #[cfg(target_os = "windows")]
    let shared: Shared = starfire_decode::win_device::SharedDevice::create().ok();
    #[cfg(not(target_os = "windows"))]
    let shared: Shared = ();
    #[cfg(not(target_os = "windows"))]
    let _ = &shared;

    macro_rules! stop {
        ($($arg:tt)*) => {{
            let _ = event_tx.send(ClientEvent::Stopped(format!($($arg)*)));
            return;
        }};
    }

    let client = match session::pair(&cfg.host, "Starfire", &cfg.pin) {
        Ok(c) => c,
        Err(e) => stop!("pair: {e}"),
    };
    let apps = match client.applist() {
        Ok(a) => a,
        Err(e) => stop!("applist: {e}"),
    };
    let Some(app) = apps
        .iter()
        .find(|a| a.title == cfg.app_name)
        .map(|a| a.id.clone())
    else {
        stop!(
            "app {:?} not found in {:?}",
            cfg.app_name,
            apps.iter().map(|a| &a.title).collect::<Vec<_>>()
        );
    };

    let (launch_cfg, announce_cfg) = stream_configs(&cfg);
    let mut sess = match StreamSession::start(client, &cfg.host, &app, &launch_cfg, &announce_cfg) {
        Ok(s) => s,
        Err(e) => stop!("session start: {e}"),
    };

    // On Windows, build the decoder on the shared D3D11 device (zero-copy
    // textures); otherwise the portable factory. Mirrors the reference app.
    #[cfg(target_os = "windows")]
    let made = match &shared {
        Some(dev) => {
            starfire_decode::backend::mediafoundation::MediaFoundationDecoder::with_device(
                Codec::Hevc,
                dev.clone(),
            )
            .map(|d| Box::new(d) as Box<dyn starfire_decode::Decoder>)
        }
        None => create_decoder(Codec::Hevc, Accel::PreferHardware),
    };
    #[cfg(not(target_os = "windows"))]
    let made = create_decoder(Codec::Hevc, Accel::PreferHardware);
    let mut decoder = match made {
        Ok(d) => d,
        Err(e) => stop!("no video decoder on this platform: {e}"),
    };
    let mut dep = Depacketizer::new(Codec::Hevc);

    let mut buf = [0u8; 2048];
    let mut abuf = [0u8; 2048];
    // Loss feedback to the host (Sunshine-compatible): gaps in delivered frame
    // indices mean a frame was lost beyond FEC recovery, so request an IDR
    // (cooldown-limited) and accumulate the loss for the periodic LOSS_STATS the
    // host's bitrate estimator consumes.
    let mut last_decoded_idx: Option<u32> = None;
    let mut loss_window = 0i32;
    let mut consec_errs = 0u32;
    let mut last_idr_req = std::time::Instant::now() - Duration::from_secs(2);
    let mut last_loss_report = std::time::Instant::now();

    loop {
        // Send any captured input immediately (drained first for lowest latency).
        while let Ok(msg) = input_rx.try_recv() {
            let _ = sess.send_input(&msg);
        }
        // Periodic LOSS_STATS to the host (drives its bitrate estimator).
        if last_loss_report.elapsed() > Duration::from_millis(500) {
            let dt = last_loss_report.elapsed().as_millis() as i32;
            let _ = sess.send_loss_stats(loss_window, dt, last_decoded_idx.unwrap_or(0) as i32);
            loss_window = 0;
            last_loss_report = std::time::Instant::now();
        }
        // Drain audio every iteration (non-blocking) so it never gates video.
        while let Some(an) = sess.poll_audio(&mut abuf) {
            if let Some(tx) = &audio_tx {
                let _ = tx.send(abuf[..an].to_vec());
            }
        }
        let Some(n) = sess.poll_video(&mut buf) else {
            continue;
        };
        let Some(au) = dep.push(&buf[..n]) else {
            continue;
        };
        let frame_idx = au.frame_index;
        match decoder.push(&au) {
            Ok(Some(frame)) => {
                consec_errs = 0;
                // Loss for the host's BWE = gaps between *decoded* frames (transit
                // loss AND decode-failure cascades), the user-visible damage.
                if let Some(last) = last_decoded_idx {
                    if frame_idx > last + 1 {
                        loss_window += (frame_idx - last - 1) as i32;
                    }
                }
                last_decoded_idx = Some(frame_idx);
                if let Ok(mut slot) = latest.lock() {
                    *slot = Some(frame);
                }
                if event_tx.send(ClientEvent::Frame).is_err() {
                    return; // embedder dropped the client — tear down.
                }
            }
            Ok(None) => {}
            Err(_e) => {
                consec_errs += 1;
                // Only ask for an IDR when the decoder is genuinely STUCK — a run
                // of consecutive failures — and at most once a second.
                if consec_errs >= 5 && last_idr_req.elapsed() > Duration::from_secs(1) {
                    let _ = sess.request_idr();
                    last_idr_req = std::time::Instant::now();
                    consec_errs = 0;
                }
            }
        }
    }
}
