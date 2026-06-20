// SPDX-License-Identifier: Apache-2.0
//! Shared GameStream XML helper. Every control response is a flat
//! `<root status_code=… [status_message=…]>` envelope with leaf elements;
//! `/serverinfo`, `/pair`, and `/launch` all use it (docs/protocol/03, 02, 04).

use std::collections::BTreeMap;

/// A parsed flat envelope: the `<root>` attributes + a tag→text map of leaves.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Flat {
    pub status_code: Option<u16>,
    pub status_message: Option<String>,
    /// Leaf elements by tag name (last value wins).
    pub fields: BTreeMap<String, String>,
}

impl Flat {
    pub fn get(&self, tag: &str) -> Option<&str> {
        self.fields.get(tag).map(String::as_str)
    }
}

/// Parse a flat `<root>` envelope. Repeated/nested non-root elements collapse to
/// last-wins; for genuinely repeated records (e.g. `/applist`'s `<App>`) use a
/// dedicated parser instead.
pub fn parse_flat(xml: &[u8]) -> crate::Result<Flat> {
    use quick_xml::events::Event;
    use quick_xml::Reader;

    let mut reader = Reader::from_reader(xml);
    let mut buf = Vec::new();
    let mut stack: Vec<String> = Vec::new();
    let mut out = Flat::default();

    loop {
        match reader
            .read_event_into(&mut buf)
            .map_err(|e| crate::Error::Protocol(format!("XML: {e}")))?
        {
            Event::Start(e) => {
                let name = local_name(e.name().into_inner());
                if name == "root" {
                    out.status_code = attr(&e, b"status_code").and_then(|v| v.parse().ok());
                    out.status_message = attr(&e, b"status_message");
                }
                stack.push(name);
            }
            Event::Text(e) => {
                let text = e
                    .unescape()
                    .map_err(|e| crate::Error::Protocol(format!("XML text: {e}")))?;
                let trimmed = text.trim();
                if !trimmed.is_empty() {
                    if let Some(cur) = stack.last() {
                        out.fields.insert(cur.clone(), trimmed.to_string());
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
    Ok(out)
}

/// Strip any `ns:` prefix from an element name (used across the crate boundary
/// by `starfire-core`'s `/applist` record parser, so it is `pub`).
pub fn local_name(name: &[u8]) -> String {
    let s = String::from_utf8_lossy(name);
    s.rsplit(':').next().unwrap_or(&s).to_string()
}

fn attr(e: &quick_xml::events::BytesStart, key: &[u8]) -> Option<String> {
    e.attributes()
        .flatten()
        .find(|a| a.key.into_inner() == key)
        .and_then(|a| {
            std::str::from_utf8(a.value.as_ref())
                .ok()
                .map(str::to_string)
        })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn parses_root_attrs_and_leaves() {
        let xml =
            br#"<root status_code="404" status_message="nope"><gamesession>0</gamesession></root>"#;
        let f = parse_flat(xml).unwrap();
        assert_eq!(f.status_code, Some(404));
        assert_eq!(f.status_message.as_deref(), Some("nope"));
        assert_eq!(f.get("gamesession"), Some("0"));
    }
}
