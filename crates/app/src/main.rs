// SPDX-License-Identifier: Apache-2.0
//! Starfire desktop client — the end-to-end "picture on screen" path.
//!
//! Pairs with a Sunshine host, launches an app, brings up the data plane
//! ([`StreamSession`]), and pumps received RTP video through the depacketizer →
//! [`starfire_decode`] (OS-native HW decode) → [`starfire_render`] (wgpu) to a
//! window. The protocol/decode work runs on a network thread; the main thread is
//! the winit event loop (where the window + GPU surface must live).
//!
//! Run (on the client machine, in a GUI session):
//! ```text
//! STARFIRE_HOST=192.168.0.224 \
//! STARFIRE_WEB_USER=starfire STARFIRE_WEB_PASS=... STARFIRE_PIN=1234 \
//! cargo run -p starfire-app
//! ```
//! `STARFIRE_HOST` is required (an IP the host can reach back — not loopback).
//! The web creds let it auto-enter the pairing PIN via the host's web API; omit
//! them to enter the PIN on the host yourself.

use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use starfire_core::input::{self, MouseButton as SfButton};
use starfire_core::launch::LaunchConfig;
use starfire_core::rtsp::AnnounceConfig;
use starfire_core::session::{self, StreamSession};
use starfire_core::video::reassembly::Depacketizer;
use starfire_core::video::Codec;
use starfire_audio::{CpalPlayer, OpusAudioDecoder};
use starfire_decode::select::{create_decoder, Accel};
use starfire_decode::VideoFrame;
use starfire_render::{new_for_window, ActiveRenderer, Renderer};
use std::sync::mpsc::Sender;
use winit::application::ApplicationHandler;
use winit::event::{DeviceEvent, DeviceId, ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::keyboard::{KeyCode, PhysicalKey};
use winit::window::{CursorGrabMode, Fullscreen, Window, WindowId};

/// Wake-ups sent from the network/decode thread to the render loop.
enum AppEvent {
    /// A new decoded frame is available in the shared slot.
    Frame,
    /// The session ended (error or teardown); the message is for the log.
    Stopped(String),
}

fn env(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.is_empty())
}

/// Read a `u32` stream knob from the environment, falling back to `default`.
fn env_u32(key: &str, default: u32) -> u32 {
    env(key).and_then(|v| v.parse().ok()).unwrap_or(default)
}

/// Build the launch + announce configs from environment knobs so a benchmark
/// sweep can vary resolution / fps / bitrate / slices / FEC without rebuilding:
/// `STARFIRE_W`, `STARFIRE_H`, `STARFIRE_FPS`, `STARFIRE_BITRATE` (kbps),
/// `STARFIRE_SLICES`, `STARFIRE_FEC` (repair %), `STARFIRE_PKT` (payload bytes).
fn stream_configs() -> (LaunchConfig, AnnounceConfig) {
    let ad = AnnounceConfig::default();
    let (w, h, fps) = (
        env_u32("STARFIRE_W", ad.width),
        env_u32("STARFIRE_H", ad.height),
        env_u32("STARFIRE_FPS", ad.fps),
    );
    let announce = AnnounceConfig {
        width: w,
        height: h,
        fps,
        bitrate_kbps: env_u32("STARFIRE_BITRATE", ad.bitrate_kbps),
        slices_per_frame: env_u32("STARFIRE_SLICES", ad.slices_per_frame),
        fec_percent: env_u32("STARFIRE_FEC", ad.fec_percent),
        packet_size: env_u32("STARFIRE_PKT", ad.packet_size),
        encryption_enabled: ad.encryption_enabled,
    };
    let launch = LaunchConfig {
        width: w,
        height: h,
        fps,
        ..LaunchConfig::default()
    };
    (launch, announce)
}

/// Keep the process at full speed for the whole run: disable macOS **App Nap**
/// (which suspends an unfocused/background GUI app and would stutter — or stall —
/// the stream) plus idle display sleep, and mark the work latency-critical.
///
/// Raw Objective-C runtime FFI (no binding crate, matching the decode backend's
/// clean-room style): `[[NSProcessInfo processInfo] beginActivityWithOptions:…
/// reason:…]`, whose returned activity we retain for process lifetime.
#[cfg(target_os = "macos")]
fn keep_awake() {
    use std::ffi::c_void;
    use std::os::raw::c_char;
    type Id = *const c_void;
    type Sel = *const c_void;

    // NSActivityOptions (Foundation): keep App Nap off + screen on + low latency.
    const USER_INITIATED: u64 = 0x00FF_FFFF;
    const LATENCY_CRITICAL: u64 = 0xFF_0000_0000;
    const IDLE_DISPLAY_SLEEP_DISABLED: u64 = 1 << 40;
    let options = USER_INITIATED | LATENCY_CRITICAL | IDLE_DISPLAY_SLEEP_DISABLED;

    #[link(name = "objc", kind = "dylib")]
    #[link(name = "Foundation", kind = "framework")]
    extern "C" {
        fn objc_getClass(name: *const c_char) -> Id;
        fn sel_registerName(name: *const c_char) -> Sel;
        fn objc_msgSend();
    }

    // SAFETY: standard objc runtime calls; objc_msgSend is transmuted to the
    // concrete signature per call site (the normal pattern on arm64/x86_64).
    unsafe {
        let send: extern "C" fn(Id, Sel) -> Id = std::mem::transmute(objc_msgSend as *const ());
        let send_str: extern "C" fn(Id, Sel, *const c_char) -> Id =
            std::mem::transmute(objc_msgSend as *const ());
        let send_begin: extern "C" fn(Id, Sel, u64, Id) -> Id =
            std::mem::transmute(objc_msgSend as *const ());

        let pi_cls = objc_getClass(c"NSProcessInfo".as_ptr());
        let ns_cls = objc_getClass(c"NSString".as_ptr());
        if pi_cls.is_null() || ns_cls.is_null() {
            return;
        }
        let pi = send(pi_cls, sel_registerName(c"processInfo".as_ptr()));
        let reason = send_str(
            ns_cls,
            sel_registerName(c"stringWithUTF8String:".as_ptr()),
            c"Starfire streaming".as_ptr(),
        );
        let activity = send_begin(
            pi,
            sel_registerName(c"beginActivityWithOptions:reason:".as_ptr()),
            options,
            reason,
        );
        // Retain + intentionally leak so the activity lives for the whole run.
        let _ = send(activity, sel_registerName(c"retain".as_ptr()));
    }
}

#[cfg(not(target_os = "macos"))]
fn keep_awake() {}

/// Auto-submit the pairing PIN to the host's web API (so pairing completes
/// without touching the host). No-op if web creds aren't provided.
fn submit_pin(host: &str, pin: &str) {
    let (Some(user), Some(pass)) = (env("STARFIRE_WEB_USER"), env("STARFIRE_WEB_PASS")) else {
        eprintln!("[pair] no STARFIRE_WEB_USER/PASS — enter PIN {pin} on the host");
        return;
    };
    let _ = std::process::Command::new("curl")
        .args([
            "-sk",
            "--max-time",
            "8",
            "-u",
            &format!("{user}:{pass}"),
            "-X",
            "POST",
            &format!("https://{host}:47990/api/pin"),
            "-H",
            "Content-Type: application/json",
            "-d",
            &format!("{{\"pin\":\"{pin}\",\"name\":\"starfire\"}}"),
        ])
        .output();
}

/// Benchmark accumulator — the same metrics Moonlight's perf overlay shows, so
/// back-to-back runs under identical host settings are directly comparable.
struct BenchStats {
    start: std::time::Instant,
    secs: f64,
    bytes: u64,
    packets: u64,
    decode_us: Vec<u32>,
    interval_us: Vec<u32>,
    host_lat_tenths: Vec<u16>,
    rtt_ms: Vec<u32>,
    last_frame: Option<std::time::Instant>,
    last_idx: Option<u32>,
    dropped: u64,
    width: u32,
    height: u32,
}

impl BenchStats {
    fn new(secs: f64) -> Self {
        Self {
            start: std::time::Instant::now(),
            secs,
            bytes: 0,
            packets: 0,
            decode_us: Vec::new(),
            interval_us: Vec::new(),
            host_lat_tenths: Vec::new(),
            rtt_ms: Vec::new(),
            last_frame: None,
            last_idx: None,
            dropped: 0,
            width: 0,
            height: 0,
        }
    }

    fn packet(&mut self, n: usize) {
        self.packets += 1;
        self.bytes += n as u64;
    }

    fn frame(&mut self, decode_us: u32, host_lat_tenths: u16, rtt_ms: u32, idx: u32, w: u32, h: u32) {
        self.decode_us.push(decode_us);
        self.host_lat_tenths.push(host_lat_tenths);
        self.rtt_ms.push(rtt_ms);
        self.width = w;
        self.height = h;
        let now = std::time::Instant::now();
        if let Some(last) = self.last_frame {
            self.interval_us.push(now.duration_since(last).as_micros() as u32);
        }
        self.last_frame = Some(now);
        if let Some(li) = self.last_idx {
            if idx > li + 1 {
                self.dropped += (idx - li - 1) as u64;
            }
        }
        self.last_idx = Some(idx);
    }

    fn done(&self) -> bool {
        self.start.elapsed().as_secs_f64() >= self.secs
    }

    fn report(&self) {
        let dur = self.start.elapsed().as_secs_f64();
        let n = self.decode_us.len();
        let pct = |v: &[u32], p: f64| -> f64 {
            if v.is_empty() {
                return 0.0;
            }
            let mut s = v.to_vec();
            s.sort_unstable();
            s[((s.len() - 1) as f64 * p) as usize] as f64 / 1000.0
        };
        let avg = |v: &[u32]| -> f64 {
            if v.is_empty() {
                0.0
            } else {
                v.iter().map(|&x| x as f64).sum::<f64>() / v.len() as f64 / 1000.0
            }
        };
        let iv_avg = avg(&self.interval_us);
        let iv_jitter = {
            if self.interval_us.len() < 2 {
                0.0
            } else {
                let m = iv_avg * 1000.0;
                let var = self
                    .interval_us
                    .iter()
                    .map(|&x| (x as f64 - m).powi(2))
                    .sum::<f64>()
                    / self.interval_us.len() as f64;
                var.sqrt() / 1000.0
            }
        };
        let host_avg = if self.host_lat_tenths.is_empty() {
            0.0
        } else {
            self.host_lat_tenths.iter().map(|&x| x as f64).sum::<f64>()
                / self.host_lat_tenths.len() as f64
                / 10.0
        };
        let mbps = self.bytes as f64 * 8.0 / dur / 1e6;
        let drop_pct = 100.0 * self.dropped as f64 / (n as f64 + self.dropped as f64).max(1.0);

        eprintln!("\n========= STARFIRE BENCHMARK ({dur:.1}s) =========");
        eprintln!("resolution      : {}x{}", self.width, self.height);
        eprintln!("frames decoded  : {n}   |   FPS: {:.1}", n as f64 / dur);
        eprintln!(
            "decode time     : avg {:.2} ms   p99 {:.2} ms   max {:.2} ms",
            avg(&self.decode_us),
            pct(&self.decode_us, 0.99),
            pct(&self.decode_us, 1.0)
        );
        eprintln!(
            "frame pacing    : avg {iv_avg:.2} ms   jitter(stdev) {iv_jitter:.2} ms   p99 {:.2} ms",
            pct(&self.interval_us, 0.99)
        );
        eprintln!("host latency    : avg {host_avg:.2} ms   (encoder, from frame header)");
        let rtt_avg = if self.rtt_ms.is_empty() {
            0.0
        } else {
            self.rtt_ms.iter().map(|&x| x as f64).sum::<f64>() / self.rtt_ms.len() as f64
        };
        eprintln!("network RTT     : avg {rtt_avg:.1} ms   (ENet control channel)");
        eprintln!("recv bitrate    : {mbps:.1} Mbps");
        eprintln!(
            "packets         : {}   ({:.0}/s)",
            self.packets,
            self.packets as f64 / dur
        );
        eprintln!("dropped frames  : {}   ({drop_pct:.2}%)", self.dropped);
        eprintln!("=================================================\n");
    }
}

/// The full client bring-up + ingest loop, run on its own thread so the main
/// thread can own the winit event loop. Decoded frames land in `latest`; each
/// event (new frame / teardown) is delivered via `emit` — the windowed path
/// forwards it to the event loop, the headless path just logs.
fn run_session(
    latest: Arc<Mutex<Option<VideoFrame>>>,
    emit: impl Fn(AppEvent),
    input_rx: std::sync::mpsc::Receiver<Vec<u8>>,
) {
    macro_rules! stop {
        ($($arg:tt)*) => {{
            emit(AppEvent::Stopped(format!($($arg)*)));
            return;
        }};
    }

    let Some(host) = env("STARFIRE_HOST") else {
        stop!("STARFIRE_HOST not set");
    };
    let pin = env("STARFIRE_PIN").unwrap_or_else(|| "1234".to_string());
    let app_name = env("STARFIRE_APP").unwrap_or_else(|| "Desktop".to_string());

    // Pairing blocks until the PIN is entered on the host; submit it concurrently.
    {
        let (host, pin) = (host.clone(), pin.clone());
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(900));
            submit_pin(&host, &pin);
        });
    }

    eprintln!("[starfire] pairing with {host} …");
    let client = match session::pair(&host, "Starfire", &pin) {
        Ok(c) => c,
        Err(e) => stop!("pair: {e}"),
    };
    let apps = match client.applist() {
        Ok(a) => a,
        Err(e) => stop!("applist: {e}"),
    };
    let Some(app) = apps.iter().find(|a| a.title == app_name).map(|a| a.id.clone()) else {
        stop!(
            "app {app_name:?} not found in {:?}",
            apps.iter().map(|a| &a.title).collect::<Vec<_>>()
        );
    };

    let (launch_cfg, announce_cfg) = stream_configs();
    eprintln!(
        "[starfire] launching {app_name:?} @ {}x{}x{} {}kbps slices={} fec={}% pkt={} …",
        announce_cfg.width,
        announce_cfg.height,
        announce_cfg.fps,
        announce_cfg.bitrate_kbps,
        announce_cfg.slices_per_frame,
        announce_cfg.fec_percent,
        announce_cfg.packet_size,
    );
    let mut sess = match StreamSession::start(client, &host, &app, &launch_cfg, &announce_cfg) {
        Ok(s) => s,
        Err(e) => stop!("session start: {e}"),
    };

    let mut decoder = match create_decoder(Codec::Hevc, Accel::PreferHardware) {
        Ok(d) => d,
        Err(e) => stop!("no video decoder on this platform: {e}"),
    };
    let mut dep = Depacketizer::new(Codec::Hevc);

    eprintln!("[starfire] streaming — decoding frames …");
    let mut buf = [0u8; 2048];
    let mut abuf = [0u8; 2048];
    let (mut frames, mut pkts, mut aus, mut errs) = (0u64, 0u64, 0u64, 0u64);
    let mut last_report = std::time::Instant::now();
    // Loss feedback to the host (Sunshine-compatible): a gap in delivered frame
    // indices means a frame was lost beyond FEC recovery, so request an IDR
    // (cooldown-limited) and accumulate the loss for the periodic LOSS_STATS the
    // host's bitrate estimator consumes.
    let mut last_decoded_idx: Option<u32> = None;
    let mut loss_window = 0i32;
    let mut consec_errs = 0u32;
    let mut last_idr_req = std::time::Instant::now() - Duration::from_secs(2);
    let mut last_loss_report = std::time::Instant::now();

    // Benchmark mode: measure for STARFIRE_BENCH_SECS (default 20), print a
    // report, and exit. Run headless for clean numbers.
    let mut bench = env("STARFIRE_BENCH").map(|_| {
        let secs = env("STARFIRE_BENCH_SECS")
            .and_then(|s| s.parse().ok())
            .unwrap_or(20.0);
        eprintln!("[starfire] benchmarking for {secs}s …");
        BenchStats::new(secs)
    });

    // One-shot audio fixture capture (datagrams: u16-LE len prefix + bytes), then
    // exit — for offline Opus-decoder development. Set STARFIRE_AUDIO_FIXTURE=path.
    let mut afix: Option<(String, Vec<u8>)> =
        env("STARFIRE_AUDIO_FIXTURE").map(|p| (p, Vec::new()));
    let mut apkts = 0u64;

    // Audio runs on its own thread (decoupled — never gates video). Mute with
    // STARFIRE_AUDIO=off; the session still pings the audio port (keepalive) via
    // poll_video, so muting doesn't tear the session down.
    let audio_on = !matches!(
        env("STARFIRE_AUDIO").as_deref(),
        Some("off") | Some("0") | Some("false") | Some("no")
    );
    let audio_tx = if audio_on && afix.is_none() {
        let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
        thread::spawn(move || audio_thread(rx));
        Some(tx)
    } else {
        if !audio_on {
            eprintln!("[starfire] audio disabled (STARFIRE_AUDIO=off) — video-only, lowest latency");
        }
        None
    };

    loop {
        // Send any captured input immediately (drained first for lowest latency).
        while let Ok(msg) = input_rx.try_recv() {
            let _ = sess.send_input(&msg);
        }
        // Benchmark: finish + report + exit once the window elapses.
        if let Some(b) = &bench {
            if b.done() {
                b.report();
                std::process::exit(0);
            }
        }
        // Periodic pipeline health report (helps diagnose where frames stall).
        if last_report.elapsed() > Duration::from_secs(2) {
            eprintln!("[starfire] rx_pkts={pkts} apkts={apkts} aus={aus} decoded={frames} decode_errs={errs}");
            last_report = std::time::Instant::now();
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
            apkts += 1;
            if let Some((path, data)) = afix.as_mut() {
                data.extend_from_slice(&(an as u16).to_le_bytes());
                data.extend_from_slice(&abuf[..an]);
                if apkts >= 600 {
                    std::fs::write(path.as_str(), &*data).ok();
                    eprintln!("[starfire] wrote {} audio bytes to {path}", data.len());
                    std::process::exit(0);
                }
            } else if let Some(tx) = &audio_tx {
                let _ = tx.send(abuf[..an].to_vec());
            }
        }
        let Some(n) = sess.poll_video(&mut buf) else {
            continue;
        };
        pkts += 1;
        if let Some(b) = bench.as_mut() {
            b.packet(n);
        }
        let Some(au) = dep.push(&buf[..n]) else {
            continue;
        };
        aus += 1;
        let (host_lat, frame_idx) = (au.host_latency_tenths_ms, au.frame_index);
        let rtt_ms = if bench.is_some() {
            sess.rtt().as_millis() as u32
        } else {
            0
        };
        let t0 = std::time::Instant::now();
        let decoded = decoder.push(&au);
        let decode_us = t0.elapsed().as_micros() as u32;
        match decoded {
            Ok(Some(frame)) => {
                frames += 1;
                consec_errs = 0;
                // Loss for the host's BWE = gaps between *decoded* frames. This
                // captures both transit loss AND decode-failure cascades (frames
                // that arrived but couldn't decode), which is the user-visible
                // damage — so the estimator backs off a rate that's overshooting.
                if let Some(last) = last_decoded_idx {
                    if frame_idx > last + 1 {
                        loss_window += (frame_idx - last - 1) as i32;
                    }
                }
                last_decoded_idx = Some(frame_idx);
                if let Some(b) = bench.as_mut() {
                    b.frame(decode_us, host_lat, rtt_ms, frame_idx, frame.width, frame.height);
                }
                if frames <= 3 || frames.is_multiple_of(120) {
                    eprintln!("[starfire] decoded frame {frames}: {}x{}", frame.width, frame.height);
                }
                if let Ok(mut slot) = latest.lock() {
                    *slot = Some(frame);
                }
                emit(AppEvent::Frame);
            }
            Ok(None) => {}
            Err(e) => {
                errs += 1;
                consec_errs += 1;
                if errs <= 3 {
                    eprintln!("[starfire] decode error: {e}");
                }
                // Only ask for an IDR when the decoder is genuinely STUCK — a run
                // of consecutive failures (reference chain broken, no param sets) —
                // and at most once a second. A single failure usually self-heals at
                // the next FEC-recovered frame or the periodic IDR, so re-keying for
                // it just adds a losable keyframe burst.
                if consec_errs >= 5 && last_idr_req.elapsed() > Duration::from_secs(1) {
                    let _ = sess.request_idr();
                    last_idr_req = std::time::Instant::now();
                    consec_errs = 0;
                }
            }
        }
    }
}

/// Audio path, fully decoupled from video: own the Opus decoder + the cpal
/// output device, decode each received audio datagram and feed playback. Runs on
/// its own thread; `CpalPlayer`/the device stream are `!Send`, so they're created
/// here rather than passed in.
fn audio_thread(rx: std::sync::mpsc::Receiver<Vec<u8>>) {
    let player = match CpalPlayer::new() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("[starfire] audio output unavailable: {e}");
            return;
        }
    };
    let mut dec = match OpusAudioDecoder::new() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("[starfire] opus init failed: {e}");
            return;
        }
    };
    let mut played = 0u64;
    while let Ok(pkt) = rx.recv() {
        let Some(payload) = starfire_audio::rtp::opus_payload(&pkt) else {
            continue; // FEC/keepalive — not an Opus data packet
        };
        match dec.decode(payload) {
            Ok(pcm) => {
                player.push(&pcm);
                played += 1;
                if played <= 2 {
                    eprintln!("[starfire] audio playing ({} samples/frame)", pcm.len() / 2);
                }
            }
            Err(e) => {
                if played < 2 {
                    eprintln!("[starfire] audio decode error: {e}");
                }
            }
        }
    }
}

struct App {
    latest: Arc<Mutex<Option<VideoFrame>>>,
    window: Option<Arc<Window>>,
    renderer: Option<ActiveRenderer>,
    /// Encoded input messages -> network thread -> control channel.
    input_tx: Sender<Vec<u8>>,
    /// Pointer captured (FPS mode): raw mouse motion sent as relative deltas.
    grabbed: bool,
    /// Live keyboard modifier mask (GameStream bits).
    modifiers: u8,
    /// Currently in borderless fullscreen (toggled with F11).
    fullscreen: bool,
    /// Latest decoded frame size, the reference viewport for absolute-mouse
    /// coordinates (updated each frame).
    stream_size: Option<(u32, u32)>,
}

impl App {
    /// Capture the pointer for FPS: lock + hide the cursor so OS mouse motion is
    /// delivered as raw relative deltas (no acceleration, no edge clamp). Click
    /// the window to capture; Esc releases.
    fn grab(&mut self) {
        if let Some(w) = &self.window {
            let _ = w
                .set_cursor_grab(CursorGrabMode::Locked)
                .or_else(|_| w.set_cursor_grab(CursorGrabMode::Confined));
            w.set_cursor_visible(false);
            self.grabbed = true;
        }
    }

    fn ungrab(&mut self) {
        if let Some(w) = &self.window {
            let _ = w.set_cursor_grab(CursorGrabMode::None);
            w.set_cursor_visible(true);
        }
        self.grabbed = false;
    }

    fn send(&self, msg: Vec<u8>) {
        let _ = self.input_tx.send(msg);
    }

    fn track_modifier(&mut self, vk: u16, down: bool) {
        let bit: u8 = match vk {
            0x10 => 0x01,        // shift
            0x11 => 0x02,        // ctrl
            0x12 => 0x04,        // alt
            0x5B | 0x5C => 0x08, // meta / super
            _ => return,
        };
        if down {
            self.modifiers |= bit;
        } else {
            self.modifiers &= !bit;
        }
    }

    /// Enter/leave borderless fullscreen (F11).
    fn set_fullscreen(&mut self, on: bool) {
        if let Some(w) = &self.window {
            w.set_fullscreen(on.then(|| Fullscreen::Borderless(None)));
            self.fullscreen = on;
        }
    }

    /// Forward an absolute cursor position (ungrabbed / desktop mode). The
    /// renderer stretches the video to fill the window, so this is a straight
    /// normalize-to-window then scale-to-stream-resolution. Grabbed/FPS mode uses
    /// the raw relative path (`device_event`) instead.
    fn send_abs_cursor(&self, x: f64, y: f64) {
        let (Some(w), Some((sw, sh))) = (&self.window, self.stream_size) else {
            return;
        };
        let size = w.inner_size();
        if size.width == 0 || size.height == 0 {
            return;
        }
        let nx = (x / size.width as f64).clamp(0.0, 1.0);
        let ny = (y / size.height as f64).clamp(0.0, 1.0);
        let sx = (nx * sw as f64).round() as i16;
        let sy = (ny * sh as f64).round() as i16;
        self.send(input::mouse_move_abs(sx, sy, sw as i16, sh as i16));
    }
}

impl ApplicationHandler<AppEvent> for App {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        // Start fullscreen by default (like Moonlight); STARFIRE_FULLSCREEN=0 to
        // start windowed. F11 toggles either way.
        let start_fs = !matches!(
            env("STARFIRE_FULLSCREEN").as_deref(),
            Some("0") | Some("off") | Some("false") | Some("no")
        );
        let mut attrs = Window::default_attributes().with_title("Starfire");
        if start_fs {
            attrs = attrs.with_fullscreen(Some(Fullscreen::Borderless(None)));
        }
        let window = match el.create_window(attrs) {
            Ok(w) => Arc::new(w),
            Err(e) => {
                eprintln!("[starfire] create_window failed: {e}");
                el.exit();
                return;
            }
        };
        self.fullscreen = start_fs;
        let size = window.inner_size();
        match new_for_window(window.clone(), size.width.max(1), size.height.max(1)) {
            Ok(r) => self.renderer = Some(r),
            Err(e) => {
                eprintln!("[starfire] renderer init failed: {e}");
                el.exit();
                return;
            }
        }
        self.window = Some(window);
    }

    fn user_event(&mut self, el: &ActiveEventLoop, event: AppEvent) {
        match event {
            AppEvent::Frame => {
                // Cache the stream resolution (reference viewport for absolute
                // mouse) and ask the window to redraw.
                if let Ok(slot) = self.latest.lock() {
                    if let Some(f) = slot.as_ref() {
                        self.stream_size = Some((f.width, f.height));
                    }
                }
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
            }
            AppEvent::Stopped(msg) => {
                eprintln!("[starfire] session stopped: {msg}");
                el.exit();
            }
        }
    }

    fn window_event(&mut self, el: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => el.exit(),
            WindowEvent::Resized(size) => {
                if let Some(r) = self.renderer.as_mut() {
                    r.resize(size.width.max(1), size.height.max(1));
                }
            }
            WindowEvent::RedrawRequested => {
                if let (Some(r), Ok(slot)) = (self.renderer.as_mut(), self.latest.lock()) {
                    if let Some(frame) = slot.as_ref() {
                        if let Err(e) = r.present(frame) {
                            eprintln!("[starfire] present error: {e}");
                        }
                    }
                }
            }
            WindowEvent::Focused(false) => self.ungrab(),
            WindowEvent::MouseInput { state, button, .. } => {
                if !self.grabbed {
                    // First click captures the pointer; the click isn't forwarded.
                    if state == ElementState::Pressed {
                        self.grab();
                    }
                    return;
                }
                if let Some(b) = map_button(button) {
                    self.send(input::mouse_button(b, state == ElementState::Pressed));
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let (dx, dy) = match delta {
                    MouseScrollDelta::LineDelta(x, y) => (x * 120.0, y * 120.0),
                    MouseScrollDelta::PixelDelta(p) => (p.x as f32, p.y as f32),
                };
                if dy != 0.0 {
                    self.send(input::scroll_vertical(dy as i16));
                }
                if dx != 0.0 {
                    self.send(input::scroll_horizontal(dx as i16));
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                // Desktop/absolute mode: forward the cursor position when the
                // pointer isn't grabbed. Grabbed/FPS mode uses raw relative deltas
                // from `device_event` instead.
                if !self.grabbed {
                    self.send_abs_cursor(position.x, position.y);
                }
            }
            WindowEvent::KeyboardInput { event, .. } => {
                if let PhysicalKey::Code(code) = event.physical_key {
                    if code == KeyCode::Escape && self.grabbed {
                        self.ungrab(); // Esc releases the pointer
                        return;
                    }
                    if code == KeyCode::F11 {
                        // Toggle fullscreen locally; never forward F11 to the host.
                        if event.state == ElementState::Pressed {
                            let on = !self.fullscreen;
                            self.set_fullscreen(on);
                        }
                        return;
                    }
                    if let Some(vk) = vk_from_keycode(code) {
                        let down = event.state == ElementState::Pressed;
                        self.track_modifier(vk, down);
                        self.send(input::key(vk, self.modifiers, down));
                    }
                }
            }
            _ => {}
        }
    }

    fn device_event(&mut self, _el: &ActiveEventLoop, _id: DeviceId, event: DeviceEvent) {
        // Raw relative motion — the FPS aim path. Only when the pointer is grabbed.
        if let DeviceEvent::MouseMotion { delta: (dx, dy) } = event {
            if self.grabbed && (dx != 0.0 || dy != 0.0) {
                self.send(input::mouse_move_rel(dx as i16, dy as i16));
            }
        }
    }
}

/// Map a winit mouse button to the GameStream button id.
fn map_button(b: MouseButton) -> Option<SfButton> {
    Some(match b {
        MouseButton::Left => SfButton::Left,
        MouseButton::Right => SfButton::Right,
        MouseButton::Middle => SfButton::Middle,
        MouseButton::Back => SfButton::Side1,
        MouseButton::Forward => SfButton::Side2,
        _ => return None,
    })
}

/// Map a winit physical key to a Windows virtual-key code (what the host expects).
fn vk_from_keycode(code: KeyCode) -> Option<u16> {
    use KeyCode as K;
    Some(match code {
        K::KeyA => 0x41, K::KeyB => 0x42, K::KeyC => 0x43, K::KeyD => 0x44,
        K::KeyE => 0x45, K::KeyF => 0x46, K::KeyG => 0x47, K::KeyH => 0x48,
        K::KeyI => 0x49, K::KeyJ => 0x4A, K::KeyK => 0x4B, K::KeyL => 0x4C,
        K::KeyM => 0x4D, K::KeyN => 0x4E, K::KeyO => 0x4F, K::KeyP => 0x50,
        K::KeyQ => 0x51, K::KeyR => 0x52, K::KeyS => 0x53, K::KeyT => 0x54,
        K::KeyU => 0x55, K::KeyV => 0x56, K::KeyW => 0x57, K::KeyX => 0x58,
        K::KeyY => 0x59, K::KeyZ => 0x5A,
        K::Digit0 => 0x30, K::Digit1 => 0x31, K::Digit2 => 0x32, K::Digit3 => 0x33,
        K::Digit4 => 0x34, K::Digit5 => 0x35, K::Digit6 => 0x36, K::Digit7 => 0x37,
        K::Digit8 => 0x38, K::Digit9 => 0x39,
        K::F1 => 0x70, K::F2 => 0x71, K::F3 => 0x72, K::F4 => 0x73,
        K::F5 => 0x74, K::F6 => 0x75, K::F7 => 0x76, K::F8 => 0x77,
        K::F9 => 0x78, K::F10 => 0x79, K::F11 => 0x7A, K::F12 => 0x7B,
        K::Escape => 0x1B, K::Space => 0x20, K::Enter => 0x0D, K::Backspace => 0x08,
        K::Tab => 0x09, K::CapsLock => 0x14,
        K::ShiftLeft | K::ShiftRight => 0x10,
        K::ControlLeft | K::ControlRight => 0x11,
        K::AltLeft | K::AltRight => 0x12,
        K::SuperLeft => 0x5B, K::SuperRight => 0x5C,
        K::ArrowLeft => 0x25, K::ArrowUp => 0x26, K::ArrowRight => 0x27, K::ArrowDown => 0x28,
        K::Home => 0x24, K::End => 0x23, K::PageUp => 0x21, K::PageDown => 0x22,
        K::Insert => 0x2D, K::Delete => 0x2E,
        K::Minus => 0xBD, K::Equal => 0xBB, K::BracketLeft => 0xDB, K::BracketRight => 0xDD,
        K::Backslash => 0xDC, K::Semicolon => 0xBA, K::Quote => 0xDE, K::Backquote => 0xC0,
        K::Comma => 0xBC, K::Period => 0xBE, K::Slash => 0xBF,
        _ => return None,
    })
}

// Top-level init failures (event loop / GPU) are fatal and worth a panic.
#[allow(clippy::expect_used)]
fn main() {
    keep_awake(); // never let App Nap / display sleep throttle the stream

    let latest: Arc<Mutex<Option<VideoFrame>>> = Arc::new(Mutex::new(None));
    // Captured input (main/winit thread) -> network thread -> control channel.
    let (input_tx, input_rx) = std::sync::mpsc::channel::<Vec<u8>>();

    // Headless mode: run the full pair → stream → depacketize → decode loop on
    // this thread and log decoded frames — no window (no input source). Validates
    // HW decode over a headless/SSH session where a GPU window can't be created.
    if env("STARFIRE_HEADLESS").is_some() {
        run_session(
            latest,
            |e| {
                if let AppEvent::Stopped(m) = e {
                    eprintln!("[starfire] stopped: {m}");
                }
            },
            input_rx,
        );
        return;
    }

    let event_loop = EventLoop::<AppEvent>::with_user_event()
        .build()
        .expect("build event loop");
    let proxy = event_loop.create_proxy();
    {
        let latest = latest.clone();
        thread::spawn(move || {
            run_session(latest, move |e| {
                let _ = proxy.send_event(e);
            }, input_rx)
        });
    }

    let mut app = App {
        latest,
        window: None,
        renderer: None,
        input_tx,
        grabbed: false,
        modifiers: 0,
        fullscreen: false,
        stream_size: None,
    };
    event_loop.run_app(&mut app).expect("run event loop");
}
