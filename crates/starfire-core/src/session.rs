// SPDX-License-Identifier: Apache-2.0
//! Session orchestration — the connection state machine that walks the protocol
//! lifecycle (docs/02-architecture.md §lifecycle): discover → pair → serverinfo →
//! launch → rtsp → control up → media ingest, with IDR/reconnect on loss and
//! clean teardown on quit. Drives the per-layer modules; owns no wire format.

/// Where we are in the connection lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Phase {
    #[default]
    Idle,
    Discovered,
    Paired,
    Negotiated,
    Launched,
    RtspReady,
    ControlUp,
    Streaming,
    TearingDown,
}

/// The session driver. Phase 1 wires the real layers behind this; today it only
/// models the phase progression so the state machine has a home.
#[derive(Debug, Default)]
pub struct Session {
    phase: Phase,
}

impl Session {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn phase(&self) -> Phase {
        self.phase
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_session_is_idle() {
        assert_eq!(Session::new().phase(), Phase::Idle);
    }
}
