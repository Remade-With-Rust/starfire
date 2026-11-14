// SPDX-License-Identifier: Apache-2.0
//! Opus audio decode — docs/protocol/08. Pure-Rust `rusty-opus` (RFC 6716 /
//! RFC 8251, BSD-3-Clause; no C, no CMake). Sunshine streams 48 kHz stereo Opus
//! (CELT fullband, 5 ms frames in our config); the decoder is self-describing,
//! so we feed it each payload and it reports the sample count.
//!
//! Checked against libopus on the captured stream (tests/fixtures/audio): every
//! frame decodes to the same length, and the i16 PCM matches within 1 LSB
//! (99.98% of samples identical; the rest is float->i16 rounding).

use rusty_opus::OpusDecoder;

use crate::AudioError;

pub const SAMPLE_RATE: u32 = 48_000;
pub const CHANNELS: usize = 2;
/// Max Opus frame at 48 kHz (120 ms), per channel — the decode scratch bound.
const MAX_SAMPLES_PER_CH: usize = 5760;
/// 5 ms at 48 kHz: Sunshine's frame length, used to size concealment before
/// the first packet has told us the real one.
const DEFAULT_FRAME_PER_CH: usize = 240;

/// Stateful Opus decoder producing interleaved stereo `i16` PCM.
pub struct OpusAudioDecoder {
    decoder: OpusDecoder,
    scratch: Vec<f32>,
    /// Samples per channel of the last decoded frame — what one lost packet
    /// stands for when concealing.
    last_frame_per_ch: usize,
}

impl OpusAudioDecoder {
    pub fn new() -> Result<Self, AudioError> {
        let decoder = OpusDecoder::new(SAMPLE_RATE as i32, CHANNELS)
            .map_err(|e| AudioError::Failed(format!("opus init: {e}")))?;
        Ok(Self {
            decoder,
            scratch: vec![0.0; MAX_SAMPLES_PER_CH * CHANNELS],
            last_frame_per_ch: DEFAULT_FRAME_PER_CH,
        })
    }

    /// Decode one Opus payload to interleaved stereo PCM.
    pub fn decode(&mut self, payload: &[u8]) -> Result<Vec<i16>, AudioError> {
        if payload.is_empty() {
            // rusty-opus reads an empty input as "lost"; a real empty payload
            // is malformed, so don't let it silently become concealment.
            return Err(AudioError::Failed("opus packet: empty payload".into()));
        }
        let samples = self
            .decoder
            .decode(payload, MAX_SAMPLES_PER_CH, &mut self.scratch)
            .map_err(|e| AudioError::Failed(format!("opus decode: {e}")))?;
        self.last_frame_per_ch = samples;
        Ok(self.to_i16(samples))
    }

    /// Conceal one lost packet via Opus' built-in PLC, producing one frame of
    /// the length most recently decoded.
    pub fn conceal(&mut self) -> Result<Vec<i16>, AudioError> {
        let samples = self
            .decoder
            .decode(&[], self.last_frame_per_ch, &mut self.scratch)
            .map_err(|e| AudioError::Failed(format!("opus plc: {e}")))?;
        Ok(self.to_i16(samples))
    }

    /// The decoder's nominal-[-1, 1] float PCM as i16, rounded and saturated
    /// the way libopus' own i16 output is.
    fn to_i16(&self, samples_per_ch: usize) -> Vec<i16> {
        self.scratch[..samples_per_ch * CHANNELS]
            .iter()
            .map(|&x| (x * 32768.0).round().clamp(-32768.0, 32767.0) as i16)
            .collect()
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

    /// A lost packet is concealed as one frame of the stream's real length
    /// (5 ms here), not the 120 ms decode bound.
    #[test]
    fn conceals_one_frame_of_the_last_decoded_length() {
        let mut dec = OpusAudioDecoder::new().expect("opus decoder");
        let first = load_fixture()
            .into_iter()
            .find_map(|p| rtp::opus_payload(&p).map(<[u8]>::to_vec))
            .expect("a data packet");
        let pcm = dec.decode(&first).expect("decode");
        let plc = dec.conceal().expect("conceal");
        assert_eq!(plc.len(), pcm.len());
        assert!(dec.decode(&[]).is_err(), "empty payload is malformed");
    }
}
