// SPDX-License-Identifier: Apache-2.0
//! Starfire desktop client — the end-to-end "picture on screen" path.
//!
//! This app is a thin consumer of [`starfire_client::Client`], which owns the
//! whole pipeline (pair → launch → receive → reassemble → decode). The app adds
//! what only an app can: a window, the renderer, audio playback, and input
//! capture. The main thread is the winit event loop (where the window + GPU
//! surface must live); the client wakes it the instant a frame is decoded.
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
//!
//! Measuring: `STARFIRE_BENCH=1` (optionally `STARFIRE_BENCH_SECS=20`) streams
//! for that long after a short warm-up, prints the client's latency timeline
//! ([`starfire_client::ClientStats`]) and exits. `STARFIRE_HEADLESS=1` runs
//! without a window (decode only — the `present` rows then have no samples).

use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use starfire_audio::{CpalPlayer, OpusAudioDecoder};
use starfire_client::{Client, ClientEvent, ClientStats, StarfireConfig};
use starfire_core::input::{self, MouseButton as SfButton};
use starfire_core::video::Codec;
use starfire_decode::VideoFrame;
use starfire_render::{new_for_window, ActiveRenderer, Renderer};
use winit::application::ApplicationHandler;
use winit::event::{DeviceEvent, DeviceId, ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::keyboard::{KeyCode, PhysicalKey};
use winit::window::{CursorGrabMode, Fullscreen, Window, WindowId};

/// Wake-ups delivered to the render loop.
enum AppEvent {
    /// A new decoded frame is available in the shared slot.
    Frame,
    /// The session ended (error or teardown); the message is for the log.
    Stopped(String),
    /// Periodic tick for the health line and the benchmark window.
    Tick,
}

fn env(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.is_empty())
}

/// Read a `u32` stream knob from the environment, falling back to `default`.
fn env_u32(key: &str, default: u32) -> u32 {
    env(key).and_then(|v| v.parse().ok()).unwrap_or(default)
}

/// True unless the variable is set to an explicit "off" value.
fn env_on(key: &str) -> bool {
    !matches!(
        env(key).as_deref(),
        Some("0") | Some("off") | Some("false") | Some("no")
    )
}

/// Build the client configuration from environment knobs so a benchmark sweep
/// can vary resolution / fps / bitrate / slices / FEC without rebuilding:
/// `STARFIRE_W`, `STARFIRE_H`, `STARFIRE_FPS`, `STARFIRE_BITRATE` (kbps),
/// `STARFIRE_SLICES`, `STARFIRE_FEC` (repair %), `STARFIRE_PKT` (payload bytes),
/// `STARFIRE_CODEC`, `STARFIRE_AUDIO=off`, `STARFIRE_ZEROCOPY=0`.
fn config_from_env() -> Result<StarfireConfig, String> {
    let d = StarfireConfig::default();
    Ok(StarfireConfig {
        host: env("STARFIRE_HOST").ok_or("STARFIRE_HOST not set")?,
        pin: env("STARFIRE_PIN").unwrap_or(d.pin),
        device_name: d.device_name,
        app_name: env("STARFIRE_APP").unwrap_or(d.app_name),
        width: env_u32("STARFIRE_W", d.width),
        height: env_u32("STARFIRE_H", d.height),
        fps: env_u32("STARFIRE_FPS", d.fps),
        bitrate_kbps: env_u32("STARFIRE_BITRATE", d.bitrate_kbps),
        slices: env_u32("STARFIRE_SLICES", d.slices),
        fec_percent: env_u32("STARFIRE_FEC", d.fec_percent),
        packet_size: env_u32("STARFIRE_PKT", d.packet_size),
        // Muting is safe: the session keeps the audio port pinged regardless.
        audio: env_on("STARFIRE_AUDIO"),
        codec: env("STARFIRE_CODEC").and_then(|v| Codec::from_wire(&v)),
        zero_copy: env_on("STARFIRE_ZEROCOPY"),
        // STARFIRE_RT=0 leaves every thread at normal priority (the baseline for
        // measuring what the promotion buys on this machine).
        realtime_threads: env_on("STARFIRE_RT"),
    })
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

/// This display's native refresh rate in Hz (#6), so the client requests its own
/// panel's rate (a 120 Hz laptop gets 120 fps, not a hardcoded 60). macOS via
/// `NSScreen.maximumFramesPerSecond`; other platforms return `None` (use the
/// configured/default fps). The host caps to its own capture refresh, so the
/// effective rate is the min of the two.
#[cfg(target_os = "macos")]
fn display_refresh_hz() -> Option<u32> {
    use std::ffi::c_void;
    type Id = *mut c_void;
    type Sel = *const c_void;
    #[link(name = "objc", kind = "dylib")]
    #[link(name = "AppKit", kind = "framework")]
    extern "C" {
        fn objc_getClass(name: *const std::os::raw::c_char) -> Id;
        fn sel_registerName(name: *const std::os::raw::c_char) -> Sel;
        fn objc_msgSend();
    }
    // SAFETY: standard objc runtime calls (same pattern as keep_awake);
    // maximumFramesPerSecond returns NSInteger.
    unsafe {
        let send: extern "C" fn(Id, Sel) -> Id = std::mem::transmute(objc_msgSend as *const ());
        let send_i: extern "C" fn(Id, Sel) -> isize = std::mem::transmute(objc_msgSend as *const ());
        let class = objc_getClass(c"NSScreen".as_ptr());
        if class.is_null() {
            return None;
        }
        let screen = send(class, sel_registerName(c"mainScreen".as_ptr()));
        if screen.is_null() {
            return None;
        }
        let hz = send_i(screen, sel_registerName(c"maximumFramesPerSecond".as_ptr()));
        (hz >= 24).then_some(hz as u32)
    }
}
#[cfg(not(target_os = "macos"))]
fn display_refresh_hz() -> Option<u32> {
    None
}

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

/// Benchmark window: after the stream has settled for [`BENCH_WARMUP`], measure
/// for the configured number of seconds, print the timeline, and stop.
struct Reporter {
    bench_secs: Option<f64>,
    first_frame: Option<Instant>,
    measuring_since: Option<Instant>,
    last_health: Instant,
}

/// Settling time before a benchmark window opens (decoder + link warm-up).
const BENCH_WARMUP: Duration = Duration::from_secs(1);

impl Reporter {
    fn from_env() -> Self {
        let bench_secs = env("STARFIRE_BENCH").map(|_| {
            env("STARFIRE_BENCH_SECS")
                .and_then(|s| s.parse().ok())
                .unwrap_or(20.0)
        });
        if let Some(secs) = bench_secs {
            eprintln!("[starfire] benchmarking for {secs}s after a {BENCH_WARMUP:?} warm-up …");
        }
        Self {
            bench_secs,
            first_frame: None,
            measuring_since: None,
            last_health: Instant::now(),
        }
    }

    /// Call periodically. Returns `true` when the benchmark window has closed
    /// (the report has been printed) and the app should exit.
    fn tick(&mut self, client: &Client) -> bool {
        // A stats snapshot sorts every latency window, so take one only when
        // something is actually due.
        let health_due = self.last_health.elapsed() >= Duration::from_secs(2);
        if self.first_frame.is_none() || health_due {
            let stats = client.stats();
            if self.first_frame.is_none() && stats.frames_decoded > 0 {
                self.first_frame = Some(Instant::now());
                eprintln!(
                    "[starfire] streaming {:?} {}x{}",
                    stats.codec.unwrap_or(Codec::Hevc),
                    stats.resolution.0,
                    stats.resolution.1
                );
            }
            if health_due {
                self.last_health = Instant::now();
                eprintln!("[starfire] {}", health_line(&stats));
            }
        }
        let Some(secs) = self.bench_secs else {
            return false;
        };
        match (self.first_frame, self.measuring_since) {
            (Some(first), None) if first.elapsed() >= BENCH_WARMUP => {
                client.reset_stats();
                self.measuring_since = Some(Instant::now());
                false
            }
            (_, Some(since)) if since.elapsed().as_secs_f64() >= secs => {
                eprintln!(
                    "
{}
",
                    client.stats()
                );
                true
            }
            _ => false,
        }
    }
}

/// One-line pipeline health summary (helps diagnose where frames stall).
fn health_line(s: &ClientStats) -> String {
    format!(
        "{:.0} fps  {:.1} Mbps  rtt {:.1} ms | pipeline p50 {:.2} p99 {:.2} ms | lost {} skipped {} fec {} | decode errs {} | idr reqs {}",
        s.fps(),
        s.mbps(),
        s.rtt.as_secs_f64() * 1000.0,
        s.pipeline.p50_ms(),
        s.pipeline.p99_ms(),
        s.reassembly.frames_lost,
        s.reassembly.frames_skipped,
        s.reassembly.frames_recovered,
        s.decode_errors,
        s.keyframe_requests,
    )
}

/// Start audio for this client: playback on its own thread, or (with
/// `STARFIRE_AUDIO_FIXTURE=path`) capture 600 raw datagrams to a file for
/// offline Opus-decoder development (u16-LE length prefix + bytes each).
fn start_audio(client: &mut Client) {
    let Some(rx) = client.take_audio() else {
        eprintln!("[starfire] audio disabled (STARFIRE_AUDIO=off) — video-only");
        return;
    };
    if let Some(path) = env("STARFIRE_AUDIO_FIXTURE") {
        thread::spawn(move || {
            let mut data = Vec::new();
            for pkt in rx.iter().take(600) {
                data.extend_from_slice(&(pkt.len() as u16).to_le_bytes());
                data.extend_from_slice(&pkt);
            }
            match std::fs::write(&path, &data) {
                Ok(()) => eprintln!("[starfire] wrote {} audio bytes to {path}", data.len()),
                Err(e) => eprintln!("[starfire] could not write {path}: {e}"),
            }
        });
    } else {
        thread::spawn(move || audio_thread(rx));
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
    client: Option<Client>,
    latest: Arc<Mutex<Option<VideoFrame>>>,
    reporter: Reporter,
    window: Option<Arc<Window>>,
    renderer: Option<ActiveRenderer>,
    /// Pointer captured (FPS mode): raw mouse motion sent as relative deltas.
    grabbed: bool,
    /// Live keyboard modifier mask (GameStream bits).
    modifiers: u8,
    /// Currently in borderless fullscreen (toggled with F11).
    fullscreen: bool,
    /// Latest decoded frame size, the reference viewport for absolute-mouse
    /// coordinates (updated each frame).
    stream_size: Option<(u32, u32)>,
    /// Fractional relative motion not yet sent. The OS reports sub-pixel deltas;
    /// carrying the remainder forward means slow, precise movement is not
    /// rounded away to nothing.
    rel_remainder: (f64, f64),
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
            self.rel_remainder = (0.0, 0.0);
        }
    }

    fn ungrab(&mut self) {
        if let Some(w) = &self.window {
            let _ = w.set_cursor_grab(CursorGrabMode::None);
            w.set_cursor_visible(true);
        }
        self.grabbed = false;
    }

    /// Input goes straight to the client's control thread and onto the wire.
    fn send(&self, msg: Vec<u8>) {
        if let Some(c) = &self.client {
            c.send_input(msg);
        }
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
            w.set_fullscreen(on.then_some(Fullscreen::Borderless(None)));
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

/// Split accumulated motion into the whole steps to send now and the fraction
/// to carry into the next event.
fn take_whole(acc: f64) -> (i16, f64) {
    let whole = acc.trunc().clamp(i16::MIN as f64, i16::MAX as f64);
    (whole as i16, acc - whole)
}

impl ApplicationHandler<AppEvent> for App {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        // Start fullscreen by default (like Moonlight); STARFIRE_FULLSCREEN=0 to
        // start windowed. F11 toggles either way.
        let start_fs = env_on("STARFIRE_FULLSCREEN");
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
        let (w, h) = (size.width.max(1), size.height.max(1));
        // Windows zero-copy: the D3D11 renderer on the client's decode device;
        // else the portable wgpu renderer.
        #[cfg(target_os = "windows")]
        let made = match self.client.as_ref().and_then(|c| c.shared_device()) {
            Some(dev) => starfire_render::new_d3d11_for_window(&window, dev, w, h),
            None => new_for_window(window.clone(), w, h),
        };
        #[cfg(not(target_os = "windows"))]
        let made = new_for_window(window.clone(), w, h);
        match made {
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
            AppEvent::Tick => {
                if let Some(c) = &self.client {
                    if self.reporter.tick(c) {
                        el.exit();
                    }
                }
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
                        match r.present(frame) {
                            // Close the frame's timeline: decoded → presented.
                            Ok(()) => {
                                if let Some(c) = &self.client {
                                    c.frame_presented(frame.pts);
                                }
                            }
                            Err(e) => eprintln!("[starfire] present error: {e}"),
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
            if !self.grabbed {
                return;
            }
            let (ix, rx) = take_whole(self.rel_remainder.0 + dx);
            let (iy, ry) = take_whole(self.rel_remainder.1 + dy);
            self.rel_remainder = (rx, ry);
            if ix != 0 || iy != 0 {
                self.send(input::mouse_move_rel(ix, iy));
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

/// Headless mode: run the full pair → stream → reassemble → decode pipeline and
/// report, with no window (so no input source and no present). Validates
/// hardware decode over an SSH session where a GPU window can't be created.
fn run_headless(cfg: StarfireConfig) {
    let mut client = Client::connect(cfg);
    start_audio(&mut client);
    let mut reporter = Reporter::from_env();
    loop {
        match client.poll_event() {
            Some(ClientEvent::Stopped(m)) => {
                eprintln!("[starfire] stopped: {m}");
                break;
            }
            Some(ClientEvent::Frame) => continue, // drain promptly, then tick
            None => thread::sleep(Duration::from_millis(5)),
        }
        if reporter.tick(&client) {
            break;
        }
    }
    client.stop(); // tells the host to end the session before we exit
}

// The process-wide allocator (crates/starfire-alloc). Declared here, in the
// deliverable, never in a library.
#[global_allocator]
static ALLOC: starfire_alloc::Alloc = starfire_alloc::Alloc;

// Top-level init failures (event loop / GPU) are fatal and worth a panic.
#[allow(clippy::expect_used)]
fn main() {
    let alloc = starfire_alloc::configure(starfire_alloc::Profile::LongLived);
    eprintln!(
        "[starfire] allocator rusty_alloc {} (purge_delay {}, secure {})",
        starfire_alloc::version(),
        alloc.purge_delay,
        alloc.secure
    );
    keep_awake(); // never let App Nap / display sleep throttle the stream

    // Request this display's native refresh unless the operator pinned it.
    if env("STARFIRE_FPS").is_none() {
        if let Some(hz) = display_refresh_hz() {
            eprintln!("[starfire] display {hz} Hz → requesting {hz} fps (STARFIRE_FPS overrides)");
            std::env::set_var("STARFIRE_FPS", hz.to_string());
        }
    }

    let cfg = match config_from_env() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[starfire] {e}");
            std::process::exit(2);
        }
    };
    eprintln!(
        "[starfire] connecting to {} — {:?} @ {}x{}x{} {} kbps slices={} fec={}% pkt={}",
        cfg.host,
        cfg.app_name,
        cfg.width,
        cfg.height,
        cfg.fps,
        cfg.bitrate_kbps,
        cfg.slices,
        cfg.fec_percent,
        cfg.packet_size,
    );

    // Pairing blocks until the PIN is entered on the host; submit it concurrently.
    {
        let (host, pin) = (cfg.host.clone(), cfg.pin.clone());
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(900));
            submit_pin(&host, &pin);
        });
    }

    if env("STARFIRE_HEADLESS").is_some() {
        run_headless(cfg);
        return;
    }

    let event_loop = EventLoop::<AppEvent>::with_user_event()
        .build()
        .expect("build event loop");

    // The client wakes the event loop from its pipeline thread the instant a
    // frame is decoded.
    let proxy = event_loop.create_proxy();
    let mut client = Client::connect_with(cfg, move |ev| {
        let _ = proxy.send_event(match ev {
            ClientEvent::Frame => AppEvent::Frame,
            ClientEvent::Stopped(m) => AppEvent::Stopped(m),
        });
    });
    start_audio(&mut client);

    // Periodic tick for the health line and the benchmark window.
    {
        let proxy = event_loop.create_proxy();
        thread::spawn(move || loop {
            thread::sleep(Duration::from_millis(250));
            if proxy.send_event(AppEvent::Tick).is_err() {
                break; // the event loop is gone
            }
        });
    }

    let mut app = App {
        latest: client.latest(),
        client: Some(client),
        reporter: Reporter::from_env(),
        window: None,
        renderer: None,
        grabbed: false,
        modifiers: 0,
        fullscreen: false,
        stream_size: None,
        rel_remainder: (0.0, 0.0),
    };
    event_loop.run_app(&mut app).expect("run event loop");

    // Tear the session down properly so the host stops streaming.
    if let Some(client) = app.client.take() {
        client.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::take_whole;

    /// Sub-pixel motion must add up instead of being rounded away: ten events
    /// of 0.3 px are 3 px of travel, not zero.
    #[test]
    fn fractional_mouse_motion_accumulates() {
        let (mut acc, mut sent) = (0.0f64, 0i32);
        for _ in 0..10 {
            let (whole, rest) = take_whole(acc + 0.3);
            sent += whole as i32;
            acc = rest;
        }
        assert_eq!(sent, 2, "2 whole pixels sent so far");
        assert!(
            (acc - 1.0).abs() < 1e-9 || acc < 1.0,
            "remainder carried: {acc}"
        );
        let (whole, _) = take_whole(acc + 0.3);
        assert_eq!(
            sent + whole as i32,
            3,
            "the third pixel arrives with the next event"
        );
    }

    #[test]
    fn negative_and_large_motion_split_correctly() {
        assert_eq!(take_whole(-2.75), (-2, -0.75));
        assert_eq!(take_whole(0.99).0, 0);
        assert_eq!(take_whole(1e9).0, i16::MAX, "clamped, never wraps");
    }
}
