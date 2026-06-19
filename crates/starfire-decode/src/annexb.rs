// SPDX-License-Identifier: Apache-2.0
//! Annex-B NAL parsing + AVCC/HVCC length-prefix conversion.
//!
//! Platform decoders don't eat Annex-B directly:
//! - VideoToolbox wants a `CMVideoFormatDescription` built from the parameter
//!   sets (VPS/SPS/PPS for HEVC; SPS/PPS for H.264) plus sample data in
//!   **length-prefixed** form (4-byte big-endian NAL length, "AVCC"/"HVCC").
//! - Media Foundation similarly takes parameter sets out-of-band or as a
//!   length-prefixed bitstream depending on the decoder.
//!
//! This module is pure, allocation-light, and fully testable on any platform —
//! it's the clean-room glue between `starfire-core`'s Annex-B [`AccessUnit`] and
//! whatever the OS decoder expects. No codec *decoding* happens here, only NAL
//! framing.

/// One NAL unit's payload (start code stripped) plus its decoded NAL type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Nal<'a> {
    /// NAL unit type: H.264 = `byte0 & 0x1f`; HEVC = `(byte0 >> 1) & 0x3f`.
    pub nal_type: u8,
    /// NAL bytes *including* the 1-byte (H.264) / 2-byte (HEVC) NAL header,
    /// but *excluding* the Annex-B start code.
    pub bytes: &'a [u8],
}

/// Which codec's NAL header layout to use when reading `nal_type`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NalCodec {
    H264,
    Hevc,
}

impl NalCodec {
    fn nal_type(self, first_byte: u8) -> u8 {
        match self {
            NalCodec::H264 => first_byte & 0x1f,
            NalCodec::Hevc => (first_byte >> 1) & 0x3f,
        }
    }
}

/// Iterate Annex-B NAL units in `data`, yielding each NAL's bytes with the
/// start code (`00 00 01` or `00 00 00 01`) stripped. Trailing zero bytes and
/// empty NALs are skipped. Robust to malformed input (never panics).
pub fn iter_nals(data: &[u8], codec: NalCodec) -> Vec<Nal<'_>> {
    let mut nals = Vec::new();
    let starts = start_code_positions(data);
    for w in 0..starts.len() {
        let (sc_pos, sc_len) = starts[w];
        let payload_start = sc_pos + sc_len;
        let payload_end = if w + 1 < starts.len() {
            starts[w + 1].0
        } else {
            data.len()
        };
        if payload_start >= payload_end {
            continue;
        }
        let mut bytes = &data[payload_start..payload_end];
        // Strip trailing zero padding that can precede the next start code.
        while bytes.last() == Some(&0) {
            bytes = &bytes[..bytes.len() - 1];
        }
        if bytes.is_empty() {
            continue;
        }
        nals.push(Nal {
            nal_type: codec.nal_type(bytes[0]),
            bytes,
        });
    }
    nals
}

/// Find all start-code positions: returns `(offset, start_code_len)` pairs where
/// `start_code_len` is 3 (`00 00 01`) or 4 (`00 00 00 01`).
fn start_code_positions(data: &[u8]) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            // Prefer the 4-byte form if a leading zero precedes it.
            if i > 0 && data[i - 1] == 0 && !out.iter().any(|&(p, _)| p == i - 1) {
                out.push((i - 1, 4));
            } else {
                out.push((i, 3));
            }
            i += 3;
        } else {
            i += 1;
        }
    }
    out
}

/// Convert an Annex-B access unit into a length-prefixed (AVCC/HVCC) bitstream
/// of *non-parameter-set* NALs, using a 4-byte big-endian length prefix. The
/// parameter-set NALs (VPS/SPS/PPS) are returned separately for building the
/// format description.
///
/// Returns `(parameter_sets, sample_data)`:
/// - `parameter_sets`: each VPS/SPS/PPS NAL's bytes (start code stripped), in
///   stream order — feed these to the format-description builder.
/// - `sample_data`: the slice/VCL NALs concatenated as `[len:u32_be][nal]...`.
pub fn to_length_prefixed(
    data: &[u8],
    codec: NalCodec,
) -> (Vec<Vec<u8>>, Vec<u8>) {
    let mut param_sets = Vec::new();
    let mut sample = Vec::new();
    for nal in iter_nals(data, codec) {
        if is_parameter_set(nal.nal_type, codec) {
            param_sets.push(nal.bytes.to_vec());
        } else {
            let len = nal.bytes.len() as u32;
            sample.extend_from_slice(&len.to_be_bytes());
            sample.extend_from_slice(nal.bytes);
        }
    }
    (param_sets, sample)
}

/// Is this NAL a parameter set (VPS/SPS/PPS) for the codec?
pub fn is_parameter_set(nal_type: u8, codec: NalCodec) -> bool {
    match codec {
        // H.264: SPS = 7, PPS = 8.
        NalCodec::H264 => matches!(nal_type, 7 | 8),
        // HEVC: VPS = 32, SPS = 33, PPS = 34.
        NalCodec::Hevc => matches!(nal_type, 32..=34),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_four_byte_start_codes() {
        // 00000001 40 01 (HEVC VPS, type 32), 00000001 26 01 (IDR slice type 19)
        let data = [
            0, 0, 0, 1, 0x40, 0x01, 0xaa, //
            0, 0, 0, 1, 0x26, 0x01, 0xbb, 0xcc,
        ];
        let nals = iter_nals(&data, NalCodec::Hevc);
        assert_eq!(nals.len(), 2);
        assert_eq!(nals[0].nal_type, 32); // VPS
        assert_eq!(nals[0].bytes, &[0x40, 0x01, 0xaa]);
        assert_eq!(nals[1].nal_type, 19); // IDR_W_RADL slice
        assert_eq!(nals[1].bytes, &[0x26, 0x01, 0xbb, 0xcc]);
    }

    #[test]
    fn parses_three_byte_start_codes() {
        let data = [0, 0, 1, 0x67, 0x42, 0, 0, 1, 0x65, 0x88];
        let nals = iter_nals(&data, NalCodec::H264);
        assert_eq!(nals.len(), 2);
        assert_eq!(nals[0].nal_type, 7); // SPS
        assert_eq!(nals[1].nal_type, 5); // IDR slice
    }

    #[test]
    fn splits_params_from_sample_data() {
        // HEVC: VPS(32), SPS(33), PPS(34), then an IDR slice (type 19).
        let data = [
            0, 0, 0, 1, 0x40, 0x01, // VPS
            0, 0, 0, 1, 0x42, 0x01, // SPS
            0, 0, 0, 1, 0x44, 0x01, // PPS
            0, 0, 0, 1, 0x26, 0x01, 0xde, 0xad, // IDR slice
        ];
        let (params, sample) = to_length_prefixed(&data, NalCodec::Hevc);
        assert_eq!(params.len(), 3);
        assert_eq!(params[0], vec![0x40, 0x01]);
        // sample = [00 00 00 04][26 01 de ad]
        assert_eq!(&sample[..4], &[0, 0, 0, 4]);
        assert_eq!(&sample[4..], &[0x26, 0x01, 0xde, 0xad]);
    }

    #[test]
    fn handles_empty_and_garbage_without_panic() {
        assert!(iter_nals(&[], NalCodec::Hevc).is_empty());
        assert!(iter_nals(&[0, 0, 0], NalCodec::Hevc).is_empty());
        let _ = to_length_prefixed(&[0xff, 0xff, 0xff], NalCodec::H264);
    }
}
