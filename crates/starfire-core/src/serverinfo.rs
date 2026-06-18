// SPDX-License-Identifier: Apache-2.0
//! Server capabilities & negotiation — docs/protocol/03-serverinfo-and-negotiation.md.
//! Derived from protocol observation against Sunshine 2026.516.143833. Clean-room.
//!
//! Parses the `/serverinfo` GameStream XML. The element names below are no longer
//! assumptions — they are pinned to a real capture
//! (tests/fixtures/serverinfo/http-unpaired.bin) and golden-tested.

use std::collections::BTreeMap;

/// Decoded `ServerCodecModeSupport` bitfield.
///
/// NOTE (capture finding): the real value observed from Sunshine on a host with
/// no AV1 encoder is `1573633` (`0x180301`), which does **not** set `0x40000`.
/// So `0x40000` as the AV1 bit is **unverified** — it was the readme's
/// assumption and this host can't confirm it. Re-derive every bit position from
/// a host that advertises AV1 before trusting it. [CAPTURE-LOCKED]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CodecCaps {
    pub av1: bool,
    pub hevc: bool,
    pub h264: bool,
    pub main10: bool,
}

/// Assumed AV1 bit in `ServerCodecModeSupport`. **Unverified** — see [`CodecCaps`].
pub const SERVER_CODEC_AV1: u32 = 0x40000;

impl CodecCaps {
    /// Decode only what we can state with confidence today: whether the assumed
    /// AV1 bit is set. HEVC/H.264/Main10 bit positions remain [CAPTURE-LOCKED]
    /// until a host that advertises them lets us pin them.
    pub fn from_server_codec_mode_support(mask: u32) -> Self {
        Self {
            av1: mask & SERVER_CODEC_AV1 != 0,
            ..Self::default()
        }
    }
}

/// Parsed `/serverinfo`. Typed accessors for the fields we consume, plus the raw
/// `fields` map (tag → text) for anything not yet promoted to a typed field.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ServerInfo {
    /// `status_code` attribute on the `<root>` element (200 on success).
    pub status_code: Option<u16>,
    pub hostname: Option<String>,
    pub app_version: Option<String>,
    pub gfe_version: Option<String>,
    pub unique_id: Option<String>,
    pub https_port: Option<u16>,
    pub external_port: Option<u16>,
    pub max_luma_pixels_hevc: Option<u64>,
    pub mac: Option<String>,
    pub local_ip: Option<String>,
    pub server_codec_mode_support: Option<u32>,
    /// 0 = unpaired, 1 = paired.
    pub pair_status: Option<u8>,
    /// 0 = no running app; otherwise the running app id.
    pub current_game: Option<u32>,
    pub state: Option<String>,
    /// Every leaf element by tag name (last value wins) — the escape hatch for
    /// fields we haven't typed yet.
    pub fields: BTreeMap<String, String>,
}

impl ServerInfo {
    /// Parse the GameStream `/serverinfo` XML document.
    pub fn parse(xml: &[u8]) -> crate::Result<Self> {
        use quick_xml::events::Event;
        use quick_xml::Reader;

        let mut reader = Reader::from_reader(xml);
        let mut buf = Vec::new();
        let mut stack: Vec<String> = Vec::new();
        let mut fields: BTreeMap<String, String> = BTreeMap::new();
        let mut status_code: Option<u16> = None;

        loop {
            match reader.read_event_into(&mut buf).map_err(xml_err)? {
                Event::Start(e) => {
                    let name = local_name(e.name().into_inner());
                    if name == "root" {
                        status_code = attr_u16(&e, b"status_code");
                    }
                    stack.push(name);
                }
                Event::Empty(e) => {
                    let name = local_name(e.name().into_inner());
                    if name == "root" {
                        status_code = attr_u16(&e, b"status_code");
                    }
                    fields.entry(name).or_default();
                }
                Event::Text(e) => {
                    let text = e.unescape().map_err(xml_err)?;
                    let trimmed = text.trim();
                    if !trimmed.is_empty() {
                        if let Some(current) = stack.last() {
                            fields.insert(current.clone(), trimmed.to_string());
                        }
                    }
                }
                Event::End(_) => {
                    stack.pop();
                }
                Event::Eof => break,
                _ => {}
            }
            buf.clear();
        }

        Ok(Self {
            status_code,
            hostname: fields.get("hostname").cloned(),
            app_version: fields.get("appversion").cloned(),
            gfe_version: fields.get("GfeVersion").cloned(),
            unique_id: fields.get("uniqueid").cloned(),
            https_port: parse_field(&fields, "HttpsPort"),
            external_port: parse_field(&fields, "ExternalPort"),
            max_luma_pixels_hevc: parse_field(&fields, "MaxLumaPixelsHEVC"),
            mac: fields.get("mac").cloned(),
            local_ip: fields.get("LocalIP").cloned(),
            server_codec_mode_support: parse_field(&fields, "ServerCodecModeSupport"),
            pair_status: parse_field(&fields, "PairStatus"),
            current_game: parse_field(&fields, "currentgame"),
            state: fields.get("state").cloned(),
            fields,
        })
    }

    /// True when the host reports it is paired with this client identity.
    pub fn is_paired(&self) -> bool {
        self.pair_status == Some(1)
    }

    /// Decode the advertised codec capabilities.
    pub fn codec_caps(&self) -> CodecCaps {
        CodecCaps::from_server_codec_mode_support(self.server_codec_mode_support.unwrap_or(0))
    }
}

fn parse_field<T: std::str::FromStr>(fields: &BTreeMap<String, String>, key: &str) -> Option<T> {
    fields.get(key).and_then(|s| s.parse().ok())
}

/// Strip any `ns:` prefix from an element name.
fn local_name(name: &[u8]) -> String {
    let s = String::from_utf8_lossy(name);
    s.rsplit(':').next().unwrap_or(&s).to_string()
}

fn attr_u16(e: &quick_xml::events::BytesStart, key: &[u8]) -> Option<u16> {
    e.attributes()
        .flatten()
        .find(|a| a.key.into_inner() == key)
        .and_then(|a| std::str::from_utf8(a.value.as_ref()).ok()?.parse().ok())
}

fn xml_err<E: std::fmt::Display>(e: E) -> crate::Error {
    crate::Error::Protocol(format!("serverinfo XML: {e}"))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn av1_bit_decodes() {
        assert!(CodecCaps::from_server_codec_mode_support(SERVER_CODEC_AV1).av1);
        assert!(!CodecCaps::from_server_codec_mode_support(0).av1);
    }

    #[test]
    fn parse_extracts_known_tags_and_root_attr() {
        let xml = br#"<?xml version="1.0"?><root status_code="200"><hostname>x</hostname><PairStatus>1</PairStatus><HttpsPort>47984</HttpsPort></root>"#;
        let si = ServerInfo::parse(xml).unwrap();
        assert_eq!(si.status_code, Some(200));
        assert_eq!(si.hostname.as_deref(), Some("x"));
        assert_eq!(si.pair_status, Some(1));
        assert!(si.is_paired());
        assert_eq!(si.https_port, Some(47984));
    }

    /// Golden test: the real captured `/serverinfo`, loaded through the test
    /// harness (validates bytes + .meta.toml), parsed field-by-field.
    #[test]
    fn parses_real_unpaired_serverinfo_fixture() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../tests/fixtures/serverinfo/http-unpaired.bin"
        );
        let fx = starfire_testkit::Fixture::load(path).expect("load serverinfo fixture");
        assert_eq!(fx.meta.layer, "serverinfo/http-unpaired");

        let si = ServerInfo::parse(&fx.bytes).expect("parse serverinfo");
        assert_eq!(si.status_code, Some(200));
        assert_eq!(si.hostname.as_deref(), Some("starfire-test-host"));
        assert_eq!(si.app_version.as_deref(), Some("7.1.431.-1"));
        assert_eq!(si.gfe_version.as_deref(), Some("3.23.0.74"));
        assert_eq!(si.https_port, Some(47984));
        assert_eq!(si.external_port, Some(47989));
        assert_eq!(si.max_luma_pixels_hevc, Some(1_869_449_984));
        assert_eq!(si.mac.as_deref(), Some("00:00:00:00:00:00"));
        assert_eq!(si.local_ip.as_deref(), Some("127.0.0.1"));
        assert_eq!(si.pair_status, Some(0));
        assert!(!si.is_paired());
        assert_eq!(si.current_game, Some(0));
        assert_eq!(si.state.as_deref(), Some("SUNSHINE_SERVER_FREE"));

        // The headline finding: real value is 0x180301, and the assumed AV1 bit
        // (0x40000) is NOT set on this host. This assertion is the methodology
        // guarding the codebase against the readme's unverified constant.
        assert_eq!(si.server_codec_mode_support, Some(1_573_633));
        assert_eq!(si.server_codec_mode_support, Some(0x180301));
        assert!(
            !si.codec_caps().av1,
            "host advertises no AV1; bit 0x40000 unset"
        );
    }
}
