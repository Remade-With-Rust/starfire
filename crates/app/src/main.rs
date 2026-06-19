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
    let (mut frames, mut pkts, mut aus, mut errs) = (0u64, 0u64, 0u64, 0u64);
    let mut last_report = std::time::Instant::now();
    loop {
        // Periodic pipeline health report (helps diagnose where frames stall).
        if last_report.elapsed() > Duration::from_secs(2) {
            eprintln!("[starfire] rx_pkts={pkts} aus={aus} decoded={frames} decode_errs={errs}");
            last_report = std::time::Instant::now();
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
