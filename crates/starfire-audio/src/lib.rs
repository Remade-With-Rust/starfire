// SPDX-License-Identifier: Apache-2.0
//! Audio output behind the `AudioOutput` trait — docs/04-platform-backends.md.
//!
//! Accepts decoded PCM and owns the device + channel layout (`cpal`). Opus
//! decode + A/V sync logic is core-adjacent (docs/protocol/08). Lands in Phase 2.

use starfire_core::audio::ChannelLayout;

pub mod decode;
pub mod output;
pub mod rtp;

pub use decode::OpusAudioDecoder;
pub use output::CpalPlayer;

#[derive(Debug, thiserror::Error)]
pub enum AudioError {
    #[error("no audio backend available yet")]
    NoBackend,
    #[error("output failed: {0}")]
    Failed(String),
}

/// The platform seam: push decoded PCM frames to the output device.
pub trait AudioOutput: Send {
    fn configure(&mut self, layout: ChannelLayout, sample_rate: u32) -> Result<(), AudioError>;
    fn play(&mut self, pcm: &[i16]) -> Result<(), AudioError>;
}
