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
    use rusty_xml_reader::{ReaderType, XmlTextReader};

    let mut reader = XmlTextReader::xml_reader_for_memory(xml, None, None, 0)
        .map_err(|e| crate::Error::Protocol(format!("XML: {e}")))?;
    let mut stack: Vec<String> = Vec::new();
    let mut out = Flat::default();

    // `read()` follows libxml2: 1 = a node, 0 = end of document, -1 = error.
    loop {
        match reader.read() {
            1 => {}
            0 => break,
            _ => return Err(crate::Error::Protocol("XML: malformed document".into())),
        }
        match reader.node_type() {
            ReaderType::Element => {
                // The reader returns the local name with any `ns:` prefix stripped.
                let name = reader.local_name().unwrap_or_default().to_string();
                if name == "root" {
                    out.status_code = reader
                        .get_attribute("status_code")
                        .and_then(|v| v.parse().ok());
                    out.status_message = reader.get_attribute("status_message");
                }
                // `<tag/>` raises no EndElement, so it must not be pushed.
                if !reader.is_empty_element() {
                    stack.push(name);
                }
            }
            ReaderType::Text | ReaderType::CData => {
                if let Some(text) = reader.value() {
                    let trimmed = text.trim();
                    if !trimmed.is_empty() {
                        if let Some(cur) = stack.last() {
                            out.fields.insert(cur.clone(), trimmed.to_string());
                        }
                    }
                }
            }
            ReaderType::EndElement => {
                stack.pop();
            }
            _ => {}
        }
    }
    Ok(out)
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
