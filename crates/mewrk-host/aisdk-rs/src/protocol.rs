//! The sidecar protocol's constants and host-frame decoding (`protocol.ts`).

use crate::json::Json;

/// Protocol generation. The host and sidecar must match exactly; do not negotiate.
pub const PROTOCOL_VERSION: u64 = 18;

/// Maximum line size (128 MiB); mirrors the host constant `MAX_LINE_BYTES`.
pub const MAX_LINE_BYTES: usize = 128 * 1024 * 1024;

/// Maximum accumulated visible text per request; mirrors the host constant `MAX_STREAM_TEXT`.
pub const MAX_STREAM_TEXT: usize = 16 * 1024 * 1024;

/// Maximum JSON argument size for one tool call; mirrors the host constant `MAX_STREAM_TOOL_ARGUMENTS`.
pub const MAX_TOOL_ARGUMENTS: usize = 4 * 1024 * 1024;

/// Heartbeat interval while no events are available; the host uses it to probe cancellation.
pub const HEARTBEAT_INTERVAL_MS: u64 = 500;

const SYSTEM_SECTION_SEPARATOR: &str = "\n\n";

/// The whole system prompt a provider receives: the stable part followed by the per-step tail.
pub fn full_system_prompt(request: &Json) -> Option<String> {
    let system = request.str_at("system");
    let dynamic = request.str_at("systemDynamic");
    let parts: Vec<&str> = [system, dynamic].into_iter().flatten().filter(|part| !part.is_empty()).collect();
    if parts.is_empty() {
        return system.map(str::to_owned);
    }
    Some(parts.join(SYSTEM_SECTION_SEPARATOR))
}

/// A decoded host frame.
#[derive(Debug)]
pub enum HostFrame {
    Hello,
    Step { id: String, payload: Json },
    Cancel { id: String },
    Release { session: String },
    Shutdown,
}

/// Decodes one host frame, rejecting malformed shapes rather than guessing.
pub fn decode_host_frame(line: &str) -> Result<HostFrame, String> {
    if line.len() > MAX_LINE_BYTES {
        return Err(format!("宿主帧超过 {MAX_LINE_BYTES} 字节上限"));
    }
    let mut value = Json::parse(line).map_err(|error| format!("宿主帧不是有效 JSON: {error}"))?;
    if !value.is_obj() {
        return Err("宿主帧必须是 JSON 对象".to_owned());
    }
    let version = value.get("v").and_then(Json::as_u64);
    if version != Some(PROTOCOL_VERSION) {
        let shown = value.get("v").map_or_else(|| "undefined".to_owned(), Json::to_text);
        return Err(format!("协议世代不匹配：侧车 {PROTOCOL_VERSION}，宿主 {shown}"));
    }
    let kind = value.str_at("type").map(str::to_owned);
    let id = || value.str_at("id").map(str::to_owned);
    match kind.as_deref() {
        Some("hello") => Ok(HostFrame::Hello),
        Some("step") => {
            let id = id().ok_or("step 帧缺少字符串 id")?;
            let payload = value
                .as_obj_mut()
                .and_then(|o| o.remove("payload"))
                .filter(Json::is_obj)
                .ok_or("step 帧缺少 payload")?;
            Ok(HostFrame::Step { id, payload })
        }
        Some("cancel") => Ok(HostFrame::Cancel { id: id().ok_or("cancel 帧缺少字符串 id")? }),
        Some("release") => {
            Ok(HostFrame::Release { session: value.str_at("session").map(str::to_owned).ok_or("release 帧缺少字符串 session")? })
        }
        Some("shutdown") => Ok(HostFrame::Shutdown),
        other => Err(format!("未知的宿主帧类型：{}", other.unwrap_or("undefined"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_decode_and_violations_do_not() {
        assert!(matches!(decode_host_frame(r#"{"v":18,"type":"hello"}"#), Ok(HostFrame::Hello)));
        assert!(matches!(decode_host_frame(r#"{"v":18,"type":"cancel","id":"x"}"#), Ok(HostFrame::Cancel { id }) if id == "x"));
        assert!(decode_host_frame(r#"{"v":17,"type":"hello"}"#).unwrap_err().contains("协议世代不匹配"));
        assert!(decode_host_frame(r#"{"v":18,"type":"nope"}"#).is_err());
        assert!(decode_host_frame(r#"{"v":18,"type":"step","id":"x"}"#).is_err());
        assert!(decode_host_frame("[]").is_err());
        assert!(decode_host_frame("not json").is_err());
    }

    #[test]
    fn the_system_prompt_joins_its_parts() {
        let request = |system: Option<&str>, dynamic: Option<&str>| crate::jobj! { "system"?: system, "systemDynamic"?: dynamic };
        assert_eq!(full_system_prompt(&request(Some("a"), Some("b"))).as_deref(), Some("a\n\nb"));
        assert_eq!(full_system_prompt(&request(Some("a"), None)).as_deref(), Some("a"));
        assert_eq!(full_system_prompt(&request(Some(""), Some(""))).as_deref(), Some(""));
        assert_eq!(full_system_prompt(&request(None, None)), None);
    }
}
