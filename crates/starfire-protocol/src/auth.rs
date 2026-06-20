// SPDX-License-Identifier: Apache-2.0
//! Negotiated **security profile** for the Comet-native control path — the shared
//! vocabulary the Comet host and the Starfire client agree on so a deployment can
//! dial the security/speed tradeoff. Three independent layers, each toggled on
//! its own (see ../../../comet/docs/security-profiles.md):
//!
//! * **transport** — mTLS vs plaintext on the control/HTTP plane (a connect +
//!   control-plane cost; the media plane is independent).
//! * **auth** — an optional mID identity gate (mata-master, allowlisted DIDs)
//!   *composed with* a PIN mode (none / automated / manual). The default uses
//!   both: a mID gate plus an automated PIN.
//! * **media** — video/audio confidentiality. `Plaintext` in v1; `AesGcm` is
//!   reserved (the only *per-frame* cost — deferred).
//!
//! Moonlight only speaks GameStream mTLS + manual-PIN, so the relaxed and mID
//! profiles are a Comet ↔ Starfire exclusive; the full secure path is always
//! available too.

/// Control-plane (HTTP / handshake) transport security.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    /// No TLS — fastest connect, for a trusted LAN.
    Plaintext,
    /// GameStream-grade mutual TLS (client + host certs). Required by Moonlight.
    MutualTls,
}

/// PIN handling at pairing time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PinMode {
    /// No PIN exchanged.
    None,
    /// PIN minted + exchanged programmatically, no human in the loop.
    Automated,
    /// Human confirms a PIN out of band (GameStream behaviour).
    Manual,
}

/// How a client is authorised to pair: an optional mID identity gate composed
/// with a PIN mode. Both may be active at once (the default uses both).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AuthPolicy {
    /// Require a valid, **allowlisted** mata-master mID (verified locally with
    /// `mid-verify`). The allowed DID set is host config, not part of the wire
    /// profile. `false` = no identity gate.
    pub mid_gate: bool,
    /// PIN requirement at pairing time.
    pub pin: PinMode,
}

/// Media (video + audio) confidentiality.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Media {
    /// Plaintext RTP (v1) — what the benchmarks use; zero per-frame cost.
    Plaintext,
    /// Per-packet AES-GCM. Reserved — deferred; the only per-frame CPU lever.
    AesGcm,
}

/// A negotiated security profile: three independent layers, dialled per deployment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SecurityProfile {
    pub transport: Transport,
    pub auth: AuthPolicy,
    pub media: Media,
}

impl SecurityProfile {
    /// **The default.** Plaintext transport + (mID gate + automated PIN) +
    /// plaintext media: headless, scriptable, identity-gated, fast.
    pub const PROGRAMMATIC: Self = Self {
        transport: Transport::Plaintext,
        auth: AuthPolicy {
            mid_gate: true,
            pin: PinMode::Automated,
        },
        media: Media::Plaintext,
    };

    /// Maximum speed / least ceremony: open on a fully trusted LAN, no auth.
    pub const FAST: Self = Self {
        transport: Transport::Plaintext,
        auth: AuthPolicy {
            mid_gate: false,
            pin: PinMode::None,
        },
        media: Media::Plaintext,
    };

    /// GameStream-grade secure path (the open-source opt-in): mutual TLS + a
    /// human-confirmed PIN — how Moonlight pairs.
    pub const SECURE: Self = Self {
        transport: Transport::MutualTls,
        auth: AuthPolicy {
            mid_gate: false,
            pin: PinMode::Manual,
        },
        media: Media::Plaintext,
    };

    /// Whether this profile requires a mata-master mID to be presented.
    pub fn requires_mid(&self) -> bool {
        self.auth.mid_gate
    }

    /// Whether a PIN is exchanged at all (either mode).
    pub fn uses_pin(&self) -> bool {
        !matches!(self.auth.pin, PinMode::None)
    }
}

impl Default for SecurityProfile {
    fn default() -> Self {
        Self::PROGRAMMATIC
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_programmatic_mid_plus_automated_pin() {
        let p = SecurityProfile::default();
        assert_eq!(p, SecurityProfile::PROGRAMMATIC);
        assert!(p.requires_mid());
        assert!(p.uses_pin());
        assert_eq!(p.transport, Transport::Plaintext);
        assert_eq!(p.auth.pin, PinMode::Automated);
        assert_eq!(p.media, Media::Plaintext); // media encryption deferred
    }

    #[test]
    fn presets_dial_the_tradeoff() {
        // Fast: no auth, no PIN, no TLS.
        assert!(!SecurityProfile::FAST.requires_mid());
        assert!(!SecurityProfile::FAST.uses_pin());
        // Secure: mTLS + manual PIN, no mID gate (GameStream-compatible).
        assert_eq!(SecurityProfile::SECURE.transport, Transport::MutualTls);
        assert_eq!(SecurityProfile::SECURE.auth.pin, PinMode::Manual);
        assert!(!SecurityProfile::SECURE.requires_mid());
    }
}
