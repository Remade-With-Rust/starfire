// SPDX-License-Identifier: Apache-2.0
//! Render/present behind the `Renderer` trait — docs/04-platform-backends.md.
//!
//! Low-latency present (immediate/mailbox), exclusive/borderless fullscreen,
//! BT.709 SDR + HDR10/BT.2020 PQ passthrough, aspect-correct scaling, and frame
//! pacing against the host clock. Zero-copy from decode where the OS allows
//! (IOSurface / D3D11 shared texture). Backends land in Phase 1 (Mac) / Phase 2.

use starfire_decode::DecodedFrame;

#[derive(Debug, thiserror::Error)]
pub enum RenderError {
    #[error("no renderer backend available for this platform yet")]
    NoBackend,
    #[error("present failed: {0}")]
    Failed(String),
}

/// Color pipeline the renderer must honor end-to-end.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorMode {
    Bt709Sdr,
    Hdr10Bt2020Pq,
}

/// The platform seam: present decoded frames with correct color + pacing.
pub trait Renderer: Send {
    fn set_color_mode(&mut self, mode: ColorMode);
    fn present(&mut self, frame: &DecodedFrame) -> Result<(), RenderError>;
}
