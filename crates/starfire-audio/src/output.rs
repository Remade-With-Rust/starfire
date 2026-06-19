// SPDX-License-Identifier: Apache-2.0
//! Audio output via `cpal` (CoreAudio / WASAPI; Apache-2.0). Decoded interleaved
//! `i16` PCM is converted to `f32` and pushed into a small ring buffer that the
//! device callback drains, so playback timing is decoupled from decode/network.
//! The buffer is capped to bound audio latency (old samples are dropped if the
//! producer outruns the device).

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

use crate::decode::{CHANNELS, SAMPLE_RATE};
use crate::AudioError;

/// Max queued audio before we drop the oldest, in milliseconds — caps latency.
const MAX_BUFFER_MS: usize = 60;

/// A live audio output stream pulling from a shared ring buffer.
pub struct CpalPlayer {
    ring: Arc<Mutex<VecDeque<f32>>>,
    cap: usize,
    // The stream must stay alive to keep playing; it is `!Send` on some
    // platforms, so its owner (the audio thread) holds it here.
    _stream: cpal::Stream,
}

impl CpalPlayer {
    /// Open the default output device as 48 kHz stereo and start playback.
    pub fn new() -> Result<Self, AudioError> {
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or_else(|| AudioError::Failed("no default audio output device".into()))?;

        let config = cpal::StreamConfig {
            channels: CHANNELS as u16,
            sample_rate: cpal::SampleRate(SAMPLE_RATE),
            buffer_size: cpal::BufferSize::Default,
        };
        let cap = SAMPLE_RATE as usize * CHANNELS * MAX_BUFFER_MS / 1000;

        let ring: Arc<Mutex<VecDeque<f32>>> = Arc::new(Mutex::new(VecDeque::with_capacity(cap)));
        let ring_cb = ring.clone();
        let stream = device
            .build_output_stream(
                &config,
                move |out: &mut [f32], _| {
                    let mut q = match ring_cb.lock() {
                        Ok(q) => q,
                        Err(_) => return,
                    };
                    for s in out.iter_mut() {
                        *s = q.pop_front().unwrap_or(0.0); // silence on underrun
                    }
                },
                |err| eprintln!("[audio] output stream error: {err}"),
                None,
            )
            .map_err(|e| AudioError::Failed(format!("build output stream: {e}")))?;
        stream
            .play()
            .map_err(|e| AudioError::Failed(format!("play: {e}")))?;

        Ok(Self {
            ring,
            cap,
            _stream: stream,
        })
    }

    /// Enqueue interleaved stereo `i16` PCM for playback (converted to `f32`).
    /// Drops the oldest samples if the buffer would exceed the latency cap.
    pub fn push(&self, pcm: &[i16]) {
        if let Ok(mut q) = self.ring.lock() {
            for &s in pcm {
                q.push_back(s as f32 / 32768.0);
            }
            while q.len() > self.cap {
                q.pop_front();
            }
        }
    }
}
