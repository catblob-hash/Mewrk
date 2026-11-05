//! JSON that keeps its keys in the order they were written.
//!
//! The history pane shows recorded bytes as evidence, and `JSON.stringify` keeps insertion order where
//! `serde_json::Value` here sorts its keys. Turning on `preserve_order` would change the host's own maps too, so this
//! stays local.

use serde::de::{Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};

#[derive(Clone, Debug, PartialEq)]
pub enum J {
    Null,
    Bool(bool),
    Num(serde_json::Number),
    Str(String),
    Arr(Vec<J>),
    Obj(Vec<(String, J)>),
}

impl<'de> Deserialize<'de> for J {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = J;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("JSON")
            }
            fn visit_unit<E>(self) -> Result<J, E> {
                Ok(J::Null)
            }
            fn visit_bool<E>(self, v: bool) -> Result<J, E> {
                Ok(J::Bool(v))
            }
            fn visit_i64<E>(self, v: i64) -> Result<J, E> {
                Ok(J::Num(v.into()))
            }
            fn visit_u64<E>(self, v: u64) -> Result<J, E> {
                Ok(J::Num(v.into()))
            }
            fn visit_f64<E>(self, v: f64) -> Result<J, E> {
                Ok(serde_json::Number::from_f64(v).map_or(J::Null, J::Num))
            }
            fn visit_str<E>(self, v: &str) -> Result<J, E> {
                Ok(J::Str(v.to_owned()))
            }
            fn visit_string<E>(self, v: String) -> Result<J, E> {
                Ok(J::Str(v))
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut a: A) -> Result<J, A::Error> {
                let mut out = Vec::new();
                while let Some(v) = a.next_element()? {
                    out.push(v);
                }
                Ok(J::Arr(out))
            }
            fn visit_map<A: MapAccess<'de>>(self, mut a: A) -> Result<J, A::Error> {
                let mut out: Vec<(String, J)> = Vec::new();
                while let Some((k, v)) = a.next_entry::<String, J>()? {
                    // A repeated key keeps its first place and its last value, as an object literal does.
                    match out.iter_mut().find(|e| e.0 == k) {
                        Some(e) => e.1 = v,
                        None => out.push((k, v)),
                    }
                }
                Ok(J::Obj(out))
            }
        }
        d.deserialize_any(V)
    }
}

fn write_num(n: &serde_json::Number, out: &mut String) {
    // `1.0` prints as `1` in JavaScript.
    match n.as_f64().filter(|f| n.is_f64() && f.fract() == 0.0 && f.abs() < 1e15) {
        Some(f) => out.push_str(&(f as i64).to_string()),
        None => out.push_str(&n.to_string()),
    }
}

fn write_str(s: &str, out: &mut String) {
    out.push_str(&serde_json::to_string(s).unwrap_or_default());
}

impl J {
    /// `JSON.parse`, or `None` where it would throw.
    pub fn parse(body: &str) -> Option<J> {
        serde_json::from_str(body).ok()
    }

    pub fn get(&self, key: &str) -> Option<&J> {
        match self {
            J::Obj(fields) => fields.iter().find(|e| e.0 == key).map(|e| &e.1),
            _ => None,
        }
    }

    pub fn has(&self, key: &str) -> bool {
        self.get(key).is_some()
    }

    pub fn is_obj(&self) -> bool {
        matches!(self, J::Obj(_))
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            J::Str(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_arr(&self) -> Option<&[J]> {
        match self {
            J::Arr(a) => Some(a),
            _ => None,
        }
    }

    /// A truthy value, as `if (inner[key])` reads it.
    pub fn truthy(&self) -> bool {
        match self {
            J::Null => false,
            J::Bool(b) => *b,
            J::Num(n) => n.as_f64().is_some_and(|f| f != 0.0),
            J::Str(s) => !s.is_empty(),
            J::Arr(_) | J::Obj(_) => true,
        }
    }

    fn write(&self, out: &mut String, indent: Option<usize>) {
        let newline = |out: &mut String, level: usize| {
            out.push('\n');
            out.extend(std::iter::repeat_n("  ", level));
        };
        match self {
            J::Null => out.push_str("null"),
            J::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            J::Num(n) => write_num(n, out),
            J::Str(s) => write_str(s, out),
            J::Arr(items) => {
                if items.is_empty() {
                    out.push_str("[]");
                    return;
                }
                out.push('[');
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    if let Some(level) = indent {
                        newline(out, level + 1);
                    }
                    item.write(out, indent.map(|l| l + 1));
                }
                if let Some(level) = indent {
                    newline(out, level);
                }
                out.push(']');
            }
            J::Obj(fields) => {
                if fields.is_empty() {
                    out.push_str("{}");
                    return;
                }
                out.push('{');
                for (i, (k, v)) in fields.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    if let Some(level) = indent {
                        newline(out, level + 1);
                    }
                    write_str(k, out);
                    out.push(':');
                    if indent.is_some() {
                        out.push(' ');
                    }
                    v.write(out, indent.map(|l| l + 1));
                }
                if let Some(level) = indent {
                    newline(out, level);
                }
                out.push('}');
            }
        }
    }

    /// `JSON.stringify(value)`.
    pub fn stringify(&self) -> String {
        let mut out = String::new();
        self.write(&mut out, None);
        out
    }

    /// `JSON.stringify(value, null, 2)`.
    pub fn pretty(&self) -> String {
        let mut out = String::new();
        self.write(&mut out, Some(0));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_key_order_and_prints_like_json_stringify() {
        let j = J::parse(r#"{"z":1,"a":[true,null,{"b":"x\ny"}],"e":{},"f":[],"n":1.0,"m":1.5}"#).unwrap();
        assert_eq!(j.stringify(), r#"{"z":1,"a":[true,null,{"b":"x\ny"}],"e":{},"f":[],"n":1,"m":1.5}"#);
        assert_eq!(J::parse(r#"{"z":1,"a":[2]}"#).unwrap().pretty(), "{\n  \"z\": 1,\n  \"a\": [\n    2\n  ]\n}");
        assert!(J::parse("{ not json").is_none());
    }
}
