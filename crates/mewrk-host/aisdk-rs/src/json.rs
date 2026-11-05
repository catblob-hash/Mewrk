//! An ordered JSON value.
//!
//! Every wire body the sidecar sent was `JSON.stringify` of an object literal, so key order is part of
//! the bytes on the wire, and Anthropic's prompt cache hashes those bytes. `serde_json::Value` sorts its
//! keys unless the `preserve_order` feature is on, and that feature would unify across the whole host and
//! change what it persists. So this crate keeps its own value: insertion order, last write wins at the
//! first position, the same as a JavaScript object apart from integer-like keys.

use std::fmt;

use serde::de::{self, Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};
use serde::ser::{Serialize, SerializeMap, SerializeSeq, Serializer};

#[derive(Clone, Copy, Debug)]
pub enum Num {
    I(i64),
    U(u64),
    F(f64),
}

impl PartialEq for Num {
    fn eq(&self, other: &Self) -> bool {
        self.as_f64() == other.as_f64()
    }
}

impl Num {
    pub fn as_f64(self) -> f64 {
        match self {
            Num::I(v) => v as f64,
            Num::U(v) => v as f64,
            Num::F(v) => v,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Default)]
pub enum Json {
    #[default]
    Null,
    Bool(bool),
    Num(Num),
    Str(String),
    Arr(Vec<Json>),
    Obj(Obj),
}

/// Keys in insertion order. Lookup is linear: request and chunk objects hold a handful of keys.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct Obj(pub Vec<(String, Json)>);

impl Obj {
    pub fn new() -> Self {
        Self(Vec::new())
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn get(&self, key: &str) -> Option<&Json> {
        self.0.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    pub fn get_mut(&mut self, key: &str) -> Option<&mut Json> {
        self.0.iter_mut().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    pub fn contains_key(&self, key: &str) -> bool {
        self.get(key).is_some()
    }

    /// The value under `key`, with a JSON `null` read as absent (`value.key ?? …`).
    pub fn non_null(&self, key: &str) -> Option<&Json> {
        self.get(key).filter(|v| !v.is_null())
    }

    /// Replaces in place or appends, as assigning to a JavaScript object does.
    pub fn insert(&mut self, key: impl Into<String>, value: impl Into<Json>) {
        let key = key.into();
        let value = value.into();
        if let Some(slot) = self.get_mut(&key) {
            *slot = value;
        } else {
            self.0.push((key, value));
        }
    }

    pub fn remove(&mut self, key: &str) -> Option<Json> {
        let at = self.0.iter().position(|(k, _)| k == key)?;
        Some(self.0.remove(at).1)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &Json)> {
        self.0.iter().map(|(k, v)| (k.as_str(), v))
    }

    pub fn iter_mut(&mut self) -> impl Iterator<Item = (&str, &mut Json)> {
        self.0.iter_mut().map(|(k, v)| (k.as_str(), v))
    }

    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.0.iter().map(|(k, _)| k.as_str())
    }

    /// `{ ...self, ...other }`.
    pub fn extend_from(&mut self, other: &Obj) {
        for (k, v) in other.iter() {
            self.insert(k, v.clone());
        }
    }
}

impl Json {
    pub fn obj() -> Json {
        Json::Obj(Obj::new())
    }

    pub fn is_null(&self) -> bool {
        matches!(self, Json::Null)
    }

    pub fn is_obj(&self) -> bool {
        matches!(self, Json::Obj(_))
    }

    pub fn is_arr(&self) -> bool {
        matches!(self, Json::Arr(_))
    }

    pub fn is_str(&self) -> bool {
        matches!(self, Json::Str(_))
    }

    pub fn is_num(&self) -> bool {
        matches!(self, Json::Num(_))
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Json::Str(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Json::Bool(b) => Some(*b),
            _ => None,
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Json::Num(n) => Some(n.as_f64()),
            _ => None,
        }
    }

    /// A non-negative integer value, whichever way it was written.
    pub fn as_u64(&self) -> Option<u64> {
        match self {
            Json::Num(Num::U(v)) => Some(*v),
            Json::Num(Num::I(v)) if *v >= 0 => Some(*v as u64),
            Json::Num(Num::F(v)) if *v >= 0.0 && v.fract() == 0.0 && *v < 1.8e19 => Some(*v as u64),
            _ => None,
        }
    }

    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Json::Num(Num::U(v)) => i64::try_from(*v).ok(),
            Json::Num(Num::I(v)) => Some(*v),
            Json::Num(Num::F(v)) if v.fract() == 0.0 && v.abs() < 9.0e18 => Some(*v as i64),
            _ => None,
        }
    }

    pub fn as_obj(&self) -> Option<&Obj> {
        match self {
            Json::Obj(o) => Some(o),
            _ => None,
        }
    }

    pub fn as_obj_mut(&mut self) -> Option<&mut Obj> {
        match self {
            Json::Obj(o) => Some(o),
            _ => None,
        }
    }

    pub fn as_arr(&self) -> Option<&Vec<Json>> {
        match self {
            Json::Arr(a) => Some(a),
            _ => None,
        }
    }

    pub fn as_arr_mut(&mut self) -> Option<&mut Vec<Json>> {
        match self {
            Json::Arr(a) => Some(a),
            _ => None,
        }
    }

    pub fn get(&self, key: &str) -> Option<&Json> {
        self.as_obj()?.get(key)
    }

    pub fn get_mut(&mut self, key: &str) -> Option<&mut Json> {
        self.as_obj_mut()?.get_mut(key)
    }

    /// `value?.a?.b` for a chain of keys.
    pub fn path(&self, keys: &[&str]) -> Option<&Json> {
        let mut at = self;
        for key in keys {
            at = at.get(key)?;
        }
        Some(at)
    }

    /// The string under `key`, when it is one.
    pub fn str_at(&self, key: &str) -> Option<&str> {
        self.get(key)?.as_str()
    }

    /// The value under `key`, with a JSON `null` read as absent (`value.key ?? …`).
    pub fn non_null(&self, key: &str) -> Option<&Json> {
        self.get(key).filter(|v| !v.is_null())
    }

    /// `JSON.parse`. A lone surrogate escape, which `JSON.parse` keeps and serde_json refuses, reads as
    /// U+FFFD: relays cut emoji between frames.
    pub fn parse(text: &str) -> Result<Json, String> {
        match serde_json::from_str::<Json>(text) {
            Ok(value) => Ok(value),
            Err(error) => {
                // serde_json words this several ways depending on what follows the escape.
                if let Some(fixed) = replace_lone_surrogates(text) {
                    return serde_json::from_str::<Json>(&fixed).map_err(|e| e.to_string());
                }
                if error.to_string().starts_with("number out of range") {
                    if let Some(value) = parse_with_infinities(text) {
                        return Ok(value);
                    }
                }
                Err(error.to_string())
            }
        }
    }

    /// `JSON.stringify`.
    pub fn to_text(&self) -> String {
        // A value of this type has nothing a serializer can refuse.
        serde_json::to_string(self).unwrap_or_default()
    }
}

impl fmt::Display for Json {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_text())
    }
}

/// `JSON.parse` reads a number too large for a double as an infinity, where serde_json refuses the whole text.
/// Marks each such token with a string no real value holds, and turns those back after parsing.
fn parse_with_infinities(text: &str) -> Option<Json> {
    const POSITIVE: &str = "\u{0}mewrk-json-infinity";
    const NEGATIVE: &str = "\u{0}mewrk-json-negative-infinity";
    fn restore(value: &mut Json) {
        match value {
            Json::Str(text) if text == POSITIVE => *value = Json::Num(Num::F(f64::INFINITY)),
            Json::Str(text) if text == NEGATIVE => *value = Json::Num(Num::F(f64::NEG_INFINITY)),
            Json::Arr(items) => items.iter_mut().for_each(restore),
            Json::Obj(object) => object.iter_mut().for_each(|(_, entry)| restore(entry)),
            _ => {}
        }
    }
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let (mut at, mut copied, mut in_string) = (0, 0, false);
    while at < bytes.len() {
        let byte = bytes[at];
        if in_string {
            match byte {
                b'\\' => at += 1,
                b'"' => in_string = false,
                _ => {}
            }
            at += 1;
        } else if byte == b'"' {
            in_string = true;
            at += 1;
        } else if byte == b'-' || byte.is_ascii_digit() {
            let end = bytes[at..].iter().position(|b| !matches!(b, b'-' | b'+' | b'.' | b'e' | b'E' | b'0'..=b'9')).map_or(bytes.len(), |n| at + n);
            if let Some(number) = text[at..end].parse::<f64>().ok().filter(|n| n.is_infinite()) {
                out.push_str(&text[copied..at]);
                out.push_str(if number > 0.0 { r#""\u0000mewrk-json-infinity""# } else { r#""\u0000mewrk-json-negative-infinity""# });
                copied = end;
            }
            at = end;
        } else {
            at += 1;
        }
    }
    out.push_str(&text[copied..]);
    let mut value = serde_json::from_str::<Json>(&out).ok()?;
    restore(&mut value);
    Some(value)
}

/// Rewrites every `\uD8xx`–`\uDFxx` escape that does not form a pair to `�`. `None` when nothing changed.
fn replace_lone_surrogates(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut changed = false;
    let mut at = 0;
    let mut copied = 0;
    while at < bytes.len() {
        if bytes[at] != b'\\' || at + 1 >= bytes.len() {
            at += 1;
            continue;
        }
        if bytes[at + 1] != b'u' {
            at += 2;
            continue;
        }
        let Some(unit) = hex4(bytes, at + 2) else {
            at += 2;
            continue;
        };
        if (0xD800..0xDC00).contains(&unit) {
            let paired = bytes.get(at + 6) == Some(&b'\\')
                && bytes.get(at + 7) == Some(&b'u')
                && hex4(bytes, at + 8).is_some_and(|low| (0xDC00..0xE000).contains(&low));
            if paired {
                at += 12;
                continue;
            }
        } else if !(0xDC00..0xE000).contains(&unit) {
            at += 6;
            continue;
        }
        out.push_str(&text[copied..at]);
        out.push_str("\\ufffd");
        at += 6;
        copied = at;
        changed = true;
    }
    if !changed {
        return None;
    }
    out.push_str(&text[copied..]);
    Some(out)
}

fn hex4(bytes: &[u8], at: usize) -> Option<u32> {
    let slice = bytes.get(at..at + 4)?;
    u32::from_str_radix(std::str::from_utf8(slice).ok()?, 16).ok()
}

impl From<&str> for Json {
    fn from(v: &str) -> Self {
        Json::Str(v.to_owned())
    }
}
impl From<String> for Json {
    fn from(v: String) -> Self {
        Json::Str(v)
    }
}
impl From<&String> for Json {
    fn from(v: &String) -> Self {
        Json::Str(v.clone())
    }
}
impl From<bool> for Json {
    fn from(v: bool) -> Self {
        Json::Bool(v)
    }
}
impl From<i32> for Json {
    fn from(v: i32) -> Self {
        Json::Num(Num::I(v as i64))
    }
}
impl From<i64> for Json {
    fn from(v: i64) -> Self {
        Json::Num(Num::I(v))
    }
}
impl From<u32> for Json {
    fn from(v: u32) -> Self {
        Json::Num(Num::U(v as u64))
    }
}
impl From<u64> for Json {
    fn from(v: u64) -> Self {
        Json::Num(Num::U(v))
    }
}
impl From<usize> for Json {
    fn from(v: usize) -> Self {
        Json::Num(Num::U(v as u64))
    }
}
impl From<f64> for Json {
    fn from(v: f64) -> Self {
        Json::Num(Num::F(v))
    }
}
impl From<Vec<Json>> for Json {
    fn from(v: Vec<Json>) -> Self {
        Json::Arr(v)
    }
}
impl From<Obj> for Json {
    fn from(v: Obj) -> Self {
        Json::Obj(v)
    }
}
impl From<&Json> for Json {
    fn from(v: &Json) -> Self {
        v.clone()
    }
}
impl From<Num> for Json {
    fn from(v: Num) -> Self {
        Json::Num(v)
    }
}

/// `{ "a": 1, "b"?: maybe }`. A `?` key is left out when its `Option` is `None`, the way `JSON.stringify`
/// leaves out an `undefined` property, and any `null` it does hold is kept.
#[macro_export]
macro_rules! jobj {
    (@p $o:ident ;) => {};
    (@p $o:ident ; $k:literal ? : $v:expr $(, $($rest:tt)*)?) => {
        if let Some(value) = $v {
            $o.insert($k, $crate::json::Json::from(value));
        }
        $crate::jobj!(@p $o ; $($($rest)*)?);
    };
    (@p $o:ident ; $k:literal : $v:expr $(, $($rest:tt)*)?) => {
        $o.insert($k, $crate::json::Json::from($v));
        $crate::jobj!(@p $o ; $($($rest)*)?);
    };
    ($($t:tt)*) => {{
        #[allow(unused_mut)]
        let mut object = $crate::json::Obj::new();
        $crate::jobj!(@p object ; $($t)*);
        $crate::json::Json::Obj(object)
    }};
}

/// `[a, b, c]` of anything `Json` takes.
#[macro_export]
macro_rules! jarr {
    ($($v:expr),* $(,)?) => {
        $crate::json::Json::Arr(vec![$($crate::json::Json::from($v)),*])
    };
}

impl Serialize for Json {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Json::Null => s.serialize_unit(),
            Json::Bool(v) => s.serialize_bool(*v),
            Json::Num(Num::I(v)) => s.serialize_i64(*v),
            Json::Num(Num::U(v)) => s.serialize_u64(*v),
            Json::Num(Num::F(v)) => {
                // `JSON.stringify(1.0)` is `1`, and a non-finite number is `null`.
                if !v.is_finite() {
                    s.serialize_unit()
                } else if v.fract() == 0.0 && v.abs() < 9.0e15 {
                    s.serialize_i64(*v as i64)
                } else {
                    s.serialize_f64(*v)
                }
            }
            Json::Str(v) => s.serialize_str(v),
            Json::Arr(v) => {
                let mut seq = s.serialize_seq(Some(v.len()))?;
                for item in v {
                    seq.serialize_element(item)?;
                }
                seq.end()
            }
            Json::Obj(v) => v.serialize(s),
        }
    }
}

impl Serialize for Obj {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut map = s.serialize_map(Some(self.0.len()))?;
        for (k, v) in &self.0 {
            map.serialize_entry(k, v)?;
        }
        map.end()
    }
}

impl<'de> Deserialize<'de> for Json {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Json, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Json;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("any JSON value")
            }
            fn visit_unit<E: de::Error>(self) -> Result<Json, E> {
                Ok(Json::Null)
            }
            fn visit_none<E: de::Error>(self) -> Result<Json, E> {
                Ok(Json::Null)
            }
            fn visit_some<D: Deserializer<'de>>(self, d: D) -> Result<Json, D::Error> {
                Deserialize::deserialize(d)
            }
            fn visit_bool<E: de::Error>(self, v: bool) -> Result<Json, E> {
                Ok(Json::Bool(v))
            }
            fn visit_i64<E: de::Error>(self, v: i64) -> Result<Json, E> {
                Ok(Json::Num(Num::I(v)))
            }
            fn visit_u64<E: de::Error>(self, v: u64) -> Result<Json, E> {
                Ok(Json::Num(Num::U(v)))
            }
            fn visit_f64<E: de::Error>(self, v: f64) -> Result<Json, E> {
                Ok(Json::Num(Num::F(v)))
            }
            fn visit_str<E: de::Error>(self, v: &str) -> Result<Json, E> {
                Ok(Json::Str(v.to_owned()))
            }
            fn visit_string<E: de::Error>(self, v: String) -> Result<Json, E> {
                Ok(Json::Str(v))
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut a: A) -> Result<Json, A::Error> {
                let mut out = Vec::with_capacity(a.size_hint().unwrap_or(0));
                while let Some(item) = a.next_element()? {
                    out.push(item);
                }
                Ok(Json::Arr(out))
            }
            fn visit_map<A: MapAccess<'de>>(self, mut a: A) -> Result<Json, A::Error> {
                let mut out: Vec<(String, Json)> = Vec::with_capacity(a.size_hint().unwrap_or(0));
                // Duplicate keys are checked linearly up to a point, then through an index: a body
                // with thousands of keys must not cost a quadratic pass.
                let mut index: Option<std::collections::HashMap<String, usize>> = None;
                while let Some((key, value)) = a.next_entry::<String, Json>()? {
                    if out.len() == 64 && index.is_none() {
                        index = Some(out.iter().enumerate().map(|(i, (k, _))| (k.clone(), i)).collect());
                    }
                    if let Some(map) = &mut index {
                        if let Some(&at) = map.get(&key) {
                            out[at].1 = value;
                        } else {
                            map.insert(key.clone(), out.len());
                            out.push((key, value));
                        }
                    } else if let Some(slot) = out.iter_mut().find(|(k, _)| *k == key) {
                        slot.1 = value;
                    } else {
                        out.push((key, value));
                    }
                }
                Ok(Json::Obj(Obj(out)))
            }
        }
        d.deserialize_any(V)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_past_a_double_read_as_infinities() {
        let parsed = Json::parse(r#"{"a":1e400,"s":"1e400 \" 1e400","n":[-1e400,2]}"#).unwrap();
        assert_eq!(parsed.get("a").and_then(Json::as_f64), Some(f64::INFINITY));
        assert_eq!(parsed.str_at("s"), Some("1e400 \" 1e400"));
        assert_eq!(parsed.to_text(), r#"{"a":null,"s":"1e400 \" 1e400","n":[null,2]}"#);
        assert!(Json::parse("[1e400,").is_err());
    }

    #[test]
    fn keeps_key_order_and_javascript_numbers() {
        let parsed = Json::parse(r#"{"z":1,"a":[1.0,2.5,-3],"m":{"b":null,"a":true},"z":2}"#).unwrap();
        assert_eq!(parsed.to_text(), r#"{"z":2,"a":[1,2.5,-3],"m":{"b":null,"a":true}}"#);
    }

    #[test]
    fn optional_keys_are_left_out() {
        let none: Option<String> = None;
        let value = jobj! { "type": "text", "gone"?: none, "here"?: Some(1u64), "n": Json::Null };
        assert_eq!(value.to_text(), r#"{"type":"text","here":1,"n":null}"#);
    }

    #[test]
    fn lone_surrogates_do_not_fail_a_frame() {
        let parsed = Json::parse(r#"{"a":"x\ud83d","b":"😀","c":"\ude00y"}"#).unwrap();
        assert_eq!(parsed.str_at("a"), Some("x\u{fffd}"));
        assert_eq!(parsed.str_at("b"), Some("😀"));
        assert_eq!(parsed.str_at("c"), Some("\u{fffd}y"));
    }

    #[test]
    fn strings_escape_as_json_stringify_does() {
        let value = Json::from("q\"\\\n\t\u{1}\u{7f}é");
        assert_eq!(value.to_text(), "\"q\\\"\\\\\\n\\t\\u0001\u{7f}é\"");
    }
}
