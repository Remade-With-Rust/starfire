// SPDX-License-Identifier: Apache-2.0
//! Opus audio decode — docs/protocol/08. libopus via `audiopus` (libopus is
//! BSD-3-Clause; permissive). Sunshine streams 48 kHz stereo Opus (CELT
//! fullband, 5 ms frames in our config); the decoder is self-describing, so we
//! just feed it each payload and it reports the sample count.

use audiopus::{coder::Decoder, packet::Packet, Channels, MutSignals, SampleRate};

use crate::AudioError;

pub const SAMPLE_RATE: u32 = 48_000;
pub const CHANNELS: usize = 2;
/// Max Opus frame at 48 kHz (120 ms), per channel — the decode scratch bound.
const MAX_SAMPLES_PER_CH: usize = 5760;

/// Stateful Opus decoder producing interleaved stereo `i16` PCM.
pub struct OpusAudioDecoder {
    decoder: Decoder,
    scratch: Vec<i16>,
}

impl OpusAudioDecoder {
    pub fn new() -> Result<Self, AudioError> {
        let decoder = Decoder::new(SampleRate::Hz48000, Channels::Stereo)
            .map_err(|e| AudioError::Failed(format!("opus init: {e}")))?;
        Ok(Self {
            decoder,
            scratch: vec![0i16; MAX_SAMPLES_PER_CH * CHANNELS],
        })
    }

    /// Decode one Opus payload to interleaved stereo PCM.
    pub fn decode(&mut self, payload: &[u8]) -> Result<Vec<i16>, AudioError> {
        let packet = Packet::try_from(payload)
            .map_err(|e| AudioError::Failed(format!("opus packet: {e}")))?;
        let signals = MutSignals::try_from(&mut self.scratch[..])
            .map_err(|e| AudioError::Failed(format!("opus signals: {e}")))?;
        let samples = self
            .decoder
            .decode(Some(packet), signals, false)
            .map_err(|e| AudioError::Failed(format!("opus decode: {e}")))?;
        Ok(self.scratch[..samples * CHANNELS].to_vec())
    }

    /// Conceal one lost packet via Opus' built-in PLC (feed no data).
    pub fn conceal(&mut self) -> Result<Vec<i16>, AudioError> {
        let signals = MutSignals::try_from(&mut self.scratch[..])
            .map_err(|e| AudioError::Failed(format!("opus signals: {e}")))?;
        let samples = self
            .decoder
            .decode(None, signals, false)
            .map_err(|e| AudioError::Failed(format!("opus plc: {e}")))?;
        Ok(self.scratch[..samples * CHANNELS].to_vec())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::rtp;

    fn load_fixture() -> Vec<Vec<u8>> {
        let path = format!(
            "{}/../../tests/fixtures/audio/stream-opus.fix",
            env!("CARGO_MANIFEST_DIR")
        );
        let raw = std::fs::read(&path).expect("read audio fixture");
        let mut pkts = Vec::new();
        let mut i = 0;
        while i + 2 <= raw.len() {
            let n = u16::from_le_bytes([raw[i], raw[i + 1]]) as usize;
            i += 2;
            if i + n > raw.len() {
                break;
            }
            pkts.push(raw[i..i + n].to_vec());
            i += n;
        }
        pkts
    }

    /// Parse the captured audio stream and Opus-decode every data packet,
    /// asserting we get plausible 48 kHz stereo PCM (non-empty, sane sample
    /// counts) with no decode errors.
    #[test]
    fn decodes_fixture_opus_to_pcm() {
        let pkts = load_fixture();
        assert!(pkts.len() > 100, "fixture should have many packets");

        let mut dec = OpusAudioDecoder::new().expect("opus decoder");
        let mut types = std::collections::BTreeMap::new();
        let (mut total_samples, mut frames) = (0usize, 0);
        for p in &pkts {
            let h = rtp::parse(p).expect("parse");
            *types.entry(h.packet_type).or_insert(0u32) += 1;
            if !h.is_data() {
                continue; // FEC (127) or any non-Opus-data packet
            }
            let payload = &p[rtp::RTP_HEADER_LEN..];
            if payload.is_empty() {
                continue;
            }
            let pcm = dec.decode(payload).expect("decode");
            assert!(!pcm.is_empty(), "decoded PCM should be non-empty");
            assert!(pcm.len() % CHANNELS == 0, "stereo interleave");
            total_samples += pcm.len() / CHANNELS;
            frames += 1;
        }
        println!(
            "audio packet types {types:?}; decoded {frames} Opus frames, \
             {total_samples} samples/ch (~{} ms)",
            total_samples * 1000 / SAMPLE_RATE as usize
        );
        assert!(frames > 100, "should decode many audio frames");
    }
}
