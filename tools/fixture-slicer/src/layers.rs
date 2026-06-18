// SPDX-License-Identifier: Apache-2.0
//! Port → protocol-layer classification (docs/protocol/00-overview.md).
//!
//! Ports are the conventional Sunshine defaults and are themselves
//! [CAPTURE-LOCKED] — a host can be configured with a different base, and the
//! UDP media ports come from RTSP SETUP. This maps the *default* layout; when a
//! capture uses non-standard ports, pass them via the CLI overrides.

use crate::l2l4::Proto;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Layer {
    Mdns,
    HttpControl,
    HttpsControl,
    Rtsp,
    Video,
    Control,
    Audio,
}

impl Layer {
    /// Short directory name used under `tests/fixtures/`.
    pub fn name(self) -> &'static str {
        match self {
            Layer::Mdns => "mdns",
            Layer::HttpControl => "http-control",
            Layer::HttpsControl => "https-control",
            Layer::Rtsp => "rtsp",
            Layer::Video => "video",
            Layer::Control => "control",
            Layer::Audio => "audio",
        }
    }

    pub fn is_tcp(self) -> bool {
        matches!(self, Layer::HttpControl | Layer::HttpsControl | Layer::Rtsp)
    }
}

/// A classified packet: which layer, and which port is the host's (server) side
/// — used to orient TCP streams into client→server vs server→client.
pub struct Class {
    pub layer: Layer,
    pub server_port: u16,
}

/// Default Sunshine port map. (proto, server_port) → Layer.
const PORT_MAP: &[(Proto, u16, Layer)] = &[
    (Proto::Udp, 5353, Layer::Mdns),
    (Proto::Tcp, 47984, Layer::HttpsControl),
    (Proto::Tcp, 47989, Layer::HttpControl),
    (Proto::Tcp, 48010, Layer::Rtsp),
    (Proto::Udp, 47998, Layer::Video),
    (Proto::Udp, 47999, Layer::Control),
    (Proto::Udp, 48000, Layer::Audio),
];

/// Classify a packet by matching either endpoint against a known server port.
pub fn classify(proto: Proto, src_port: u16, dst_port: u16) -> Option<Class> {
    for &(p, server_port, layer) in PORT_MAP {
        if p == proto && (src_port == server_port || dst_port == server_port) {
            return Some(Class { layer, server_port });
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_video_and_rtsp() {
        assert_eq!(
            classify(Proto::Udp, 50000, 47998).unwrap().layer,
            Layer::Video
        );
        assert_eq!(
            classify(Proto::Tcp, 48010, 50000).unwrap().layer,
            Layer::Rtsp
        );
    }

    #[test]
    fn unknown_ports_are_none() {
        assert!(classify(Proto::Udp, 1234, 5678).is_none());
        // Right port, wrong proto.
        assert!(classify(Proto::Tcp, 5353, 5353).is_none());
    }
}
