// SPDX-License-Identifier: Apache-2.0
//! CPU-side decoded video frame: planar YUV (NV12 or I420) with strides + pts.
//!
//! Clean-room: this is a plain data container with no dependency on any decoder
//! or OS framework. Hardware backends (VideoToolbox / Media Foundation) copy
//! their native surfaces (`CVPixelBuffer` / D3D11 texture) down into this shape
//! so the renderer ([`starfire-render`]) has a single, portable upload path.
//!
//! Layouts mirror the two formats hardware HEVC/H.264/AV1 decoders emit:
//! - **NV12**: full-res Y plane, then a single interleaved CbCr plane at half
//!   resolution in both axes (4:2:0). This is what VideoToolbox and most
//!   Windows/D3D decoders hand back.
//! - **I420**: full-res Y, then half-res Cb, then half-res Cr (three planes).
//!
//! All planes are 8-bit. 10-bit HDR (P010 / I420-10) is a later extension and is
//! intentionally out of scope here so the seam stays small and testable.

/// Pixel layout of a [`VideoFrame`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelFormat {
    /// 4:2:0, 2 planes: Y, then interleaved CbCr (Cb byte first).
    Nv12,
    /// 4:2:0, 3 planes: Y, then Cb, then Cr.
    I420,
}

/// Color matrix + range the chroma was encoded with. Game streaming via Sunshine
/// is BT.709 limited ("video"/studio) range by default; we carry it explicitly
/// so the renderer's YUV→RGB matrix is never guessed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorSpace {
    /// BT.709 coefficients, limited range (Y in 16..=235, C in 16..=240).
    Bt709Limited,
    /// BT.709 coefficients, full range (0..=255).
    Bt709Full,
    /// BT.601 coefficients, limited range (SD content; rarely seen here).
    Bt601Limited,
}

impl ColorSpace {
    /// Typical default for Sunshine game streaming.
    pub const DEFAULT: ColorSpace = ColorSpace::Bt709Limited;
}

/// A single plane: a borrowed-then-owned byte buffer plus its row stride.
///
/// `stride` is the number of bytes per row and may exceed `width *
/// bytes_per_sample` because hardware decoders align rows. The renderer must
/// honor it when uploading.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plane {
    /// Tightly-or-loosely packed plane bytes, `stride * rows` long.
    pub data: Vec<u8>,
    /// Bytes per row (>= the plane's visible row width).
    pub stride: usize,
}

impl Plane {
    pub fn new(data: Vec<u8>, stride: usize) -> Self {
        Self { data, stride }
    }
}

/// A decoded frame in CPU memory, ready for the renderer to upload as textures.
///
/// Invariants (checked by [`VideoFrame::validate`]):
/// - `Nv12` ⇒ exactly 2 planes (Y, CbCr); `I420` ⇒ exactly 3 (Y, Cb, Cr).
/// - Each plane buffer is at least `stride * plane_rows` bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoFrame {
    /// Visible width in luma samples.
    pub width: u32,
    /// Visible height in luma samples.
    pub height: u32,
    pub format: PixelFormat,
    pub color_space: ColorSpace,
    /// Presentation timestamp. Unit is whatever the producer chose (we treat it
    /// opaquely for pacing); typically microseconds or 90 kHz ticks.
    pub pts: i64,
    /// 2 planes for NV12, 3 for I420. See [`PixelFormat`].
    pub planes: Vec<Plane>,
}

impl VideoFrame {
    /// Number of planes the format requires.
    pub fn plane_count(format: PixelFormat) -> usize {
        match format {
            PixelFormat::Nv12 => 2,
            PixelFormat::I420 => 3,
        }
    }

    /// Rows in plane `index` for this frame's format/height. Chroma planes in
    /// 4:2:0 are half-height (rounded up).
    pub fn plane_rows(&self, index: usize) -> u32 {
        match (self.format, index) {
            (_, 0) => self.height,                                 // luma
            (PixelFormat::Nv12, 1) => self.height.div_ceil(2),     // interleaved CbCr
            (PixelFormat::I420, 1 | 2) => self.height.div_ceil(2), // Cb / Cr
            _ => 0,
        }
    }

    /// Width in *samples* of plane `index` (Y: width; chroma: half-width).
    /// For NV12's CbCr plane this is the count of CbCr *pairs* (i.e. half-width);
    /// the plane is `2 * that` bytes wide.
    pub fn plane_sample_width(&self, index: usize) -> u32 {
        match (self.format, index) {
            (_, 0) => self.width,
            (PixelFormat::Nv12, 1) => self.width.div_ceil(2),
            (PixelFormat::I420, 1 | 2) => self.width.div_ceil(2),
            _ => 0,
        }
    }

    /// Minimum bytes per row this plane must contain (before stride padding).
    pub fn plane_min_stride(&self, index: usize) -> usize {
        let samples = self.plane_sample_width(index) as usize;
        match (self.format, index) {
            (PixelFormat::Nv12, 1) => samples * 2, // CbCr interleaved: 2 bytes/sample-pair col
            _ => samples,
        }
    }

    /// Validate plane count and buffer sizes. Returns a human-readable error so
    /// backends can surface a `DecodeError::Failed` instead of panicking.
    pub fn validate(&self) -> Result<(), String> {
        let expected = Self::plane_count(self.format);
        if self.planes.len() != expected {
            return Err(format!(
                "{:?} needs {} planes, got {}",
                self.format,
                expected,
                self.planes.len()
            ));
        }
        for (i, plane) in self.planes.iter().enumerate() {
            let min_stride = self.plane_min_stride(i);
            if plane.stride < min_stride {
                return Err(format!(
                    "plane {i} stride {} < minimum {min_stride}",
                    plane.stride
                ));
            }
            let rows = self.plane_rows(i) as usize;
            let need = plane.stride * rows;
            if plane.data.len() < need {
                return Err(format!(
                    "plane {i} buffer {} < required {need} (stride {} * rows {rows})",
                    plane.data.len(),
                    plane.stride
                ));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid_nv12(w: u32, h: u32) -> VideoFrame {
        let cw = w.div_ceil(2) as usize;
        let ch = h.div_ceil(2) as usize;
        VideoFrame {
            width: w,
            height: h,
            format: PixelFormat::Nv12,
            color_space: ColorSpace::Bt709Limited,
            pts: 0,
            planes: vec![
                Plane::new(vec![16; (w * h) as usize], w as usize),
                Plane::new(vec![128; cw * 2 * ch], cw * 2),
            ],
        }
    }

    #[test]
    fn nv12_validates() {
        assert_eq!(solid_nv12(64, 48).validate(), Ok(()));
    }

    #[test]
    fn rejects_wrong_plane_count() {
        let mut f = solid_nv12(64, 48);
        f.planes.pop();
        assert!(f.validate().is_err());
    }

    #[test]
    fn rejects_short_plane() {
        let mut f = solid_nv12(64, 48);
        f.planes[0].data.truncate(10);
        assert!(f.validate().is_err());
    }

    #[test]
    fn odd_dimensions_round_chroma_up() {
        let f = VideoFrame {
            width: 3,
            height: 3,
            format: PixelFormat::I420,
            color_space: ColorSpace::Bt709Limited,
            pts: 0,
            planes: vec![
                Plane::new(vec![16; 9], 3),
                Plane::new(vec![128; 4], 2),
                Plane::new(vec![128; 4], 2),
            ],
        };
        assert_eq!(f.plane_rows(1), 2);
        assert_eq!(f.plane_sample_width(1), 2);
        assert_eq!(f.validate(), Ok(()));
    }
}
