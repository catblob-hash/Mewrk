//! Reading the files a `.mewrk` directory holds — `mcp.json`, `hooks.json`,
//! `lsp.json`, the tool-description profiles and every `SKILL.md` — the same
//! way for all of them.
//!
//! Editors on Windows (Notepad, and PowerShell's `Out-File` before 7) start a
//! UTF-8 file with a byte-order mark. `serde_json` refuses it ("expected value
//! at line 1 column 1") and `str::trim` does not count U+FEFF as whitespace, so
//! each reader that took the bytes as they came dropped a whole file, or read
//! a frontmatter as body, over one invisible character. Every reader goes
//! through here instead, so a file reads the same with or without one.

use serde::de::{Deserialize, Deserializer, MapAccess, Visitor};
use serde_json::Value;

/// The UTF-8 encoding of U+FEFF.
const BOM: &[u8] = b"\xEF\xBB\xBF";

/// `bytes` without a leading UTF-8 byte-order mark.
pub(crate) fn without_bom(bytes: &[u8]) -> &[u8] {
    bytes.strip_prefix(BOM).unwrap_or(bytes)
}

/// `text` without a leading byte-order mark.
pub(crate) fn text_without_bom(text: &str) -> &str {
    text.strip_prefix('\u{feff}').unwrap_or(text)
}

/// Splits a leading byte-order mark off `text`, for an edit that has to put it
/// back: the file is the user's, and a rewrite should differ from it only by
/// what was removed.
pub(crate) fn split_bom(text: &str) -> (&str, &str) {
    match text.strip_prefix('\u{feff}') {
        Some(rest) => (&text[..text.len() - rest.len()], rest),
        None => ("", text),
    }
}

/// Parses a JSON configuration file's bytes.
pub(crate) fn parse_json(bytes: &[u8]) -> Result<Value, serde_json::Error> {
    serde_json::from_slice(without_bom(bytes))
}

/// The members of the top-level object's `key`, in the order the file writes
/// them, or `None` when the file has no such object.
///
/// [`Value`]'s map is sorted by key, so iterating it answers in alphabetical
/// order. Where the order means something — the first `lsp.json` server
/// claiming an extension keeps it — the members are read again from the text.
/// A key written twice keeps its first place and its last value, which is the
/// value [`parse_json`] reports.
pub(crate) fn object_members_in_order(bytes: &[u8], key: &str) -> Option<Vec<(String, Value)>> {
    let root: OrderedObject = serde_json::from_slice(without_bom(bytes)).ok()?;
    let (_, value) = root.0.into_iter().find(|(name, _)| name == key)?;
    match value {
        Ordered::Object(members) => Some(
            members
                .into_iter()
                .map(|(name, value)| (name, value.into_value()))
                .collect(),
        ),
        Ordered::Other(_) => None,
    }
}

/// A JSON object that remembers the order of its members.
struct OrderedObject(Vec<(String, Ordered)>);

/// One value of an [`OrderedObject`]: a nested object keeps its order too, so
/// the one being asked about can be at any depth below the root's key; anything
/// else is kept as it parsed.
enum Ordered {
    Object(Vec<(String, Ordered)>),
    Other(Value),
}

impl Ordered {
    fn into_value(self) -> Value {
        match self {
            Ordered::Object(members) => Value::Object(
                members
                    .into_iter()
                    .map(|(name, value)| (name, value.into_value()))
                    .collect(),
            ),
            Ordered::Other(value) => value,
        }
    }
}

impl<'de> Deserialize<'de> for OrderedObject {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        match Ordered::deserialize(deserializer)? {
            Ordered::Object(members) => Ok(OrderedObject(members)),
            Ordered::Other(_) => Err(serde::de::Error::custom("expected a JSON object")),
        }
    }
}

impl<'de> Deserialize<'de> for Ordered {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct OrderedVisitor;

        impl<'de> Visitor<'de> for OrderedVisitor {
            type Value = Ordered;

            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                formatter.write_str("a JSON value")
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Ordered, A::Error> {
                let mut members: Vec<(String, Ordered)> = Vec::new();
                while let Some((name, value)) = map.next_entry::<String, Ordered>()? {
                    match members.iter_mut().find(|(existing, _)| *existing == name) {
                        Some(slot) => slot.1 = value,
                        None => members.push((name, value)),
                    }
                }
                Ok(Ordered::Object(members))
            }

            fn visit_seq<A: serde::de::SeqAccess<'de>>(self, seq: A) -> Result<Ordered, A::Error> {
                Value::deserialize(serde::de::value::SeqAccessDeserializer::new(seq))
                    .map(Ordered::Other)
            }

            fn visit_bool<E>(self, value: bool) -> Result<Ordered, E> {
                Ok(Ordered::Other(Value::Bool(value)))
            }

            fn visit_i64<E>(self, value: i64) -> Result<Ordered, E> {
                Ok(Ordered::Other(Value::from(value)))
            }

            fn visit_u64<E>(self, value: u64) -> Result<Ordered, E> {
                Ok(Ordered::Other(Value::from(value)))
            }

            fn visit_f64<E>(self, value: f64) -> Result<Ordered, E> {
                Ok(Ordered::Other(
                    serde_json::Number::from_f64(value).map_or(Value::Null, Value::Number),
                ))
            }

            fn visit_str<E>(self, value: &str) -> Result<Ordered, E> {
                Ok(Ordered::Other(Value::String(value.to_owned())))
            }

            fn visit_string<E>(self, value: String) -> Result<Ordered, E> {
                Ok(Ordered::Other(Value::String(value)))
            }

            fn visit_unit<E>(self) -> Result<Ordered, E> {
                Ok(Ordered::Other(Value::Null))
            }

            fn visit_none<E>(self) -> Result<Ordered, E> {
                Ok(Ordered::Other(Value::Null))
            }
        }

        deserializer.deserialize_any(OrderedVisitor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_byte_order_mark_changes_nothing_a_reader_sees() {
        let plain = br#"{"lspServers":{"b":{"x":1},"a":{"x":2}}}"#.to_vec();
        let mut marked = BOM.to_vec();
        marked.extend_from_slice(&plain);
        assert!(serde_json::from_slice::<Value>(&marked).is_err());
        assert_eq!(parse_json(&marked).unwrap(), parse_json(&plain).unwrap());
        assert_eq!(
            object_members_in_order(&marked, "lspServers"),
            object_members_in_order(&plain, "lspServers")
        );
        assert_eq!(text_without_bom("\u{feff}---"), "---");
        assert_eq!(text_without_bom("---"), "---");
        assert_eq!(split_bom("\u{feff}{}"), ("\u{feff}", "{}"));
        assert_eq!(split_bom("{}"), ("", "{}"));
    }

    #[test]
    fn members_come_back_in_the_order_the_file_writes_them() {
        let text = br#"{
          "note": [1, 2.5, "x", null, true],
          "lspServers": {
            "zls": {"command": "zls", "extensionToLanguage": {".zig": "zig"}},
            "alpha": {"command": "a"},
            "middle": {"command": "m"},
            "alpha": {"command": "a2"}
          }
        }"#;
        let members = object_members_in_order(text, "lspServers").unwrap();
        let names: Vec<&str> = members.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(names, ["zls", "alpha", "middle"]);
        // A repeated key keeps its first place and the value `parse_json` reads.
        assert_eq!(members[1].1["command"], "a2");
        assert_eq!(
            parse_json(text).unwrap()["lspServers"]["alpha"]["command"],
            "a2"
        );
        assert_eq!(members[0].1["extensionToLanguage"][".zig"], "zig");
        assert!(object_members_in_order(text, "note").is_none());
        assert!(object_members_in_order(text, "absent").is_none());
        assert!(object_members_in_order(b"[]", "lspServers").is_none());
    }
}
