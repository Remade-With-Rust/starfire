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

use starfire_core::launch::LaunchConfig;
use starfire_core::rtsp::AnnounceConfig;
use starfire_core::session::{self, StreamSession};
use starfire_core::video::reassembly::Depacketizer;
use starfire_core::video::Codec;
use starfire_audio::{CpalPlayer, OpusAudioDecoder};
use starfire_decode::select::{create_decoder, Accel};
use starfire_decode::VideoFrame;
use starfire_render::{Renderer, VideoRenderer};
use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::window::{Window, WindowId};

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

/// The full client bring-up + ingest loop, run on its own thread so the main
/// thread can own the winit event loop. Decoded frames land in `latest`; each
/// event (new frame / teardown) is delivered via `emit` — the windowed path
/// forwards it to the event loop, the headless path just logs.
fn run_session(latest: Arc<Mutex<Option<VideoFrame>>>, emit: impl Fn(AppEvent)) {
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

    eprintln!("[starfire] launching {app_name:?} and bringing up the data plane …");
    let mut sess = match StreamSession::start(
        client,
        &host,
        &app,
        &LaunchConfig::default(),
        &AnnounceConfig::default(),
    ) {
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
        // Periodic pipeline health report (helps diagnose where frames stall).
        if last_report.elapsed() > Duration::from_secs(2) {
            eprintln!("[starfire] rx_pkts={pkts} apkts={apkts} aus={aus} decoded={frames} decode_errs={errs}");
            last_report = std::time::Instant::now();
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
        let Some(au) = dep.push(&buf[..n]) else {
            continue;
        };
        aus += 1;
        match decoder.push(&au) {
            Ok(Some(frame)) => {
                frames += 1;
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
                if errs <= 3 {
                    eprintln!("[starfire] decode error: {e}");
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
    renderer: Option<VideoRenderer>,
}

impl ApplicationHandler<AppEvent> for App {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let attrs = Window::default_attributes().with_title("Starfire");
        let window = match el.create_window(attrs) {
            Ok(w) => Arc::new(w),
            Err(e) => {
                eprintln!("[starfire] create_window failed: {e}");
                el.exit();
                return;
            }
        };
        let size = window.inner_size();
        match VideoRenderer::new(window.clone(), size.width.max(1), size.height.max(1)) {
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
            _ => {}
        }
    }
}

// Top-level init failures (event loop / GPU) are fatal and worth a panic.
#[allow(clippy::expect_used)]
fn main() {
    keep_awake(); // never let App Nap / display sleep throttle the stream

    let latest: Arc<Mutex<Option<VideoFrame>>> = Arc::new(Mutex::new(None));

    // Headless mode: run the full pair → stream → depacketize → decode loop on
    // this thread and log decoded frames — no window. Validates HW decode over a
    // headless/SSH session where a GPU window can't be created.
    if env("STARFIRE_HEADLESS").is_some() {
        run_session(latest, |e| {
            if let AppEvent::Stopped(m) = e {
                eprintln!("[starfire] stopped: {m}");
            }
        });
        return;
    }

    let event_loop = EventLoop::<AppEvent>::with_user_event()
        .build()
        .expect("build event loop");
    let proxy = event_loop.create_proxy();
    {
        let latest = latest.clone();
        thread::spawn(move || run_session(latest, move |e| {
            let _ = proxy.send_event(e);
        }));
    }

    let mut app = App {
        latest,
        window: None,
        renderer: None,
    };
    event_loop.run_app(&mut app).expect("run event loop");
}
