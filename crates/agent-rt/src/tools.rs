//! Tool registry and built-in tools.

use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;

use bytes::Bytes;
use serde_json::{json, Value};

pub type ToolFuture = Pin<Box<dyn Future<Output = Result<String, String>> + Send>>;

pub trait Tool: Send + Sync + 'static {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    /// JSON Schema of the arguments.
    fn parameters(&self) -> Value;
    /// `args` is whatever the model produced, which is not necessarily valid JSON.
    fn call(&self, args: &str) -> ToolFuture;
}

#[derive(Default, Clone)]
pub struct ToolRegistry {
    tools: HashMap<String, Arc<dyn Tool>>,
    json: Option<Bytes>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with(mut self, tool: impl Tool) -> Self {
        self.tools.insert(tool.name().to_owned(), Arc::new(tool));
        self.json = self.render();
        self
    }

    fn render(&self) -> Option<Bytes> {
        if self.tools.is_empty() {
            return None;
        }
        // Sorted, so that the rendered prompt and therefore the server's prefix cache stay stable.
        let mut names: Vec<&String> = self.tools.keys().collect();
        names.sort();
        let list: Vec<Value> = names
            .iter()
            .map(|name| {
                let t = &self.tools[*name];
                json!({"type": "function", "function": {"name": t.name(), "description": t.description(), "parameters": t.parameters()}})
            })
            .collect();
        Some(Bytes::from(serde_json::to_vec(&list).expect("tool schema")))
    }

    pub fn tools_json(&self) -> Option<Bytes> {
        self.json.clone()
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.get(name).cloned()
    }
}

/// Arithmetic: the tool the stress scenario can check, since the right answer is computable.
pub struct Calc;

impl Tool for Calc {
    fn name(&self) -> &str {
        "calc"
    }
    fn description(&self) -> &str {
        "Evaluate an arithmetic expression with + - * / % ^ and parentheses. Returns the numeric result."
    }
    fn parameters(&self) -> Value {
        json!({"type": "object", "properties": {"expr": {"type": "string", "description": "Expression, such as (2+3)*4"}}, "required": ["expr"]})
    }
    fn call(&self, args: &str) -> ToolFuture {
        let result = serde_json::from_str::<Value>(args)
            .map_err(|e| format!("arguments are not JSON: {e}"))
            .and_then(|v| v.get("expr").and_then(Value::as_str).map(str::to_owned).ok_or_else(|| "missing expr".to_owned()))
            .and_then(|expr| eval(&expr))
            .map(format_number);
        Box::pin(async move { result })
    }
}

pub fn format_number(v: f64) -> String {
    if v.fract() == 0.0 && v.abs() < 1e15 {
        format!("{}", v as i64)
    } else {
        format!("{v}")
    }
}

/// Recursive descent over `expr := term (+|- term)*`, with `^` binding tightest.
pub fn eval(src: &str) -> Result<f64, String> {
    struct P<'a> {
        s: &'a [u8],
        i: usize,
        depth: u32,
    }
    impl P<'_> {
        fn ws(&mut self) {
            while self.s.get(self.i).is_some_and(u8::is_ascii_whitespace) {
                self.i += 1;
            }
        }
        fn peek(&mut self) -> Option<u8> {
            self.ws();
            self.s.get(self.i).copied()
        }
        fn expr(&mut self) -> Result<f64, String> {
            let mut v = self.term()?;
            while let Some(op @ (b'+' | b'-')) = self.peek() {
                self.i += 1;
                let r = self.term()?;
                v = if op == b'+' { v + r } else { v - r };
            }
            Ok(v)
        }
        fn term(&mut self) -> Result<f64, String> {
            let mut v = self.unary()?;
            while let Some(op @ (b'*' | b'/' | b'%')) = self.peek() {
                self.i += 1;
                let r = self.unary()?;
                if op != b'*' && r == 0.0 {
                    return Err("division by zero".into());
                }
                v = match op {
                    b'*' => v * r,
                    b'/' => v / r,
                    _ => v % r,
                };
            }
            Ok(v)
        }
        // Every recursive path of this grammar goes through here, and the input comes from the model. Without a
        // limit, a few thousand minus signs overflow the stack, and that aborts the process rather than a task.
        fn unary(&mut self) -> Result<f64, String> {
            self.depth += 1;
            if self.depth > 64 {
                return Err("expression nested too deeply".into());
            }
            let value = match self.peek() {
                Some(b'-') => {
                    self.i += 1;
                    self.unary().map(|v| -v)
                }
                Some(b'+') => {
                    self.i += 1;
                    self.unary()
                }
                _ => self.power(),
            };
            self.depth -= 1;
            value
        }
        fn power(&mut self) -> Result<f64, String> {
            let base = self.atom()?;
            if self.peek() == Some(b'^') {
                self.i += 1;
                return Ok(base.powf(self.unary()?));
            }
            Ok(base)
        }
        fn atom(&mut self) -> Result<f64, String> {
            match self.peek() {
                Some(b'(') => {
                    self.i += 1;
                    let v = self.expr()?;
                    if self.peek() != Some(b')') {
                        return Err("missing )".into());
                    }
                    self.i += 1;
                    Ok(v)
                }
                Some(c) if c.is_ascii_digit() || c == b'.' => {
                    let start = self.i;
                    while self.s.get(self.i).is_some_and(|c| c.is_ascii_digit() || *c == b'.' || *c == b',' || *c == b'_') {
                        self.i += 1;
                    }
                    let text: String = self.s[start..self.i].iter().filter(|c| !matches!(c, b',' | b'_')).map(|c| *c as char).collect();
                    text.parse().map_err(|_| format!("bad number {text}"))
                }
                Some(c) => Err(format!("unexpected '{}'", c as char)),
                None => Err("unexpected end of expression".into()),
            }
        }
    }
    let mut p = P { s: src.as_bytes(), i: 0, depth: 0 };
    let v = p.expr()?;
    if p.peek().is_some() {
        return Err(format!("unexpected trailing input at byte {}", p.i));
    }
    Ok(v)
}

pub struct Clock;

impl Tool for Clock {
    fn name(&self) -> &str {
        "clock"
    }
    fn description(&self) -> &str {
        "Return the current UTC time as seconds since the Unix epoch."
    }
    fn parameters(&self) -> Value {
        json!({"type": "object", "properties": {}})
    }
    fn call(&self, _: &str) -> ToolFuture {
        Box::pin(async {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs().to_string())
                .map_err(|e| e.to_string())
        })
    }
}

/// Reads text files, but only under one directory chosen by the user.
pub struct ReadFile {
    root: PathBuf,
}

impl ReadFile {
    pub fn new(root: impl Into<PathBuf>) -> std::io::Result<Self> {
        Ok(Self { root: std::fs::canonicalize(root.into())? })
    }

    fn resolve(&self, path: &str) -> Result<PathBuf, String> {
        let joined = self.root.join(Path::new(path));
        // Canonicalising resolves `..` and symlinks, so what is checked is where the path really leads.
        let real = std::fs::canonicalize(&joined).map_err(|e| format!("{path}: {e}"))?;
        if !real.starts_with(&self.root) {
            return Err(format!("{path}: outside the allowed directory"));
        }
        Ok(real)
    }
}

impl Tool for ReadFile {
    fn name(&self) -> &str {
        "read_file"
    }
    fn description(&self) -> &str {
        "Read a UTF-8 text file from the workspace. Paths are relative to the workspace root. At most 64 KiB is returned."
    }
    fn parameters(&self) -> Value {
        json!({"type": "object", "properties": {"path": {"type": "string"}}, "required": ["path"]})
    }
    fn call(&self, args: &str) -> ToolFuture {
        let path = serde_json::from_str::<Value>(args)
            .map_err(|e| format!("arguments are not JSON: {e}"))
            .and_then(|v| v.get("path").and_then(Value::as_str).map(str::to_owned).ok_or_else(|| "missing path".to_owned()))
            .and_then(|p| self.resolve(&p));
        Box::pin(async move {
            let path = path?;
            let bytes = tokio::fs::read(&path).await.map_err(|e| format!("{}: {e}", path.display()))?;
            let cut = bytes.len().min(64 << 10);
            Ok(String::from_utf8_lossy(&bytes[..cut]).into_owned())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arithmetic() {
        assert_eq!(eval("12345*6789").unwrap(), 83810205.0);
        assert_eq!(eval("(2+3)*4 - 1").unwrap(), 19.0);
        assert_eq!(eval("2^10").unwrap(), 1024.0);
        assert_eq!(eval("-2^2").unwrap(), -4.0);
        assert_eq!(eval("10 % 3").unwrap(), 1.0);
        assert_eq!(eval("1,000 + 1").unwrap(), 1001.0);
        assert!(eval("1/0").is_err());
        assert!(eval("1+").is_err());
        assert!(eval("(1").is_err());
        assert!(eval("1 2").is_err());
    }

    #[test]
    fn hostile_nesting_is_an_error_and_not_a_crash() {
        for depth in [100, 16_000, 1_000_000] {
            assert!(eval(&"(".repeat(depth)).is_err());
            assert!(eval(&format!("{}1", "-".repeat(depth))).is_err());
            assert!(eval(&format!("{}1", "1^".repeat(depth))).is_err());
            assert!(eval(&format!("{}1", "+-".repeat(depth))).is_err());
        }
        assert_eq!(eval("--1").unwrap(), 1.0);
        assert_eq!(eval("2^2^2").unwrap(), 16.0);
        assert_eq!(eval(&format!("{}1{}", "(".repeat(20), ")".repeat(20))).unwrap(), 1.0);
    }

    #[tokio::test]
    async fn calc_reports_bad_arguments_to_the_model_instead_of_failing() {
        assert_eq!(Calc.call(r#"{"expr":"6*7"}"#).await.unwrap(), "42");
        assert!(Calc.call("{not json").await.is_err());
        assert!(Calc.call(r#"{"other":1}"#).await.is_err());
    }

    #[tokio::test]
    async fn read_file_cannot_escape_its_root() {
        let dir = std::env::temp_dir().join(format!("agent-rt-tools-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("sub/a.txt"), "hello").unwrap();
        let secret = std::env::temp_dir().join(format!("agent-rt-secret-{}", std::process::id()));
        std::fs::write(&secret, "secret").unwrap();

        let tool = ReadFile::new(dir.join("sub")).unwrap();
        assert_eq!(tool.call(r#"{"path":"a.txt"}"#).await.unwrap(), "hello");
        assert!(tool.call(r#"{"path":"../../agent-rt-secret"}"#).await.is_err());
        let absolute = json!({"path": secret}).to_string();
        assert!(tool.call(&absolute).await.unwrap_err().contains("outside"));

        std::fs::remove_dir_all(&dir).unwrap();
        std::fs::remove_file(&secret).unwrap();
    }

    #[test]
    fn tool_schema_is_stable_and_valid() {
        let registry = ToolRegistry::new().with(Calc).with(Clock);
        let a = registry.tools_json().unwrap();
        let again = ToolRegistry::new().with(Clock).with(Calc).tools_json().unwrap();
        assert_eq!(a, again);
        let v: Value = serde_json::from_slice(&a).unwrap();
        assert_eq!(v[0]["function"]["name"], "calc");
    }
}
