//! The tool lock as the settings pane reads it. The rules are the client's (`lib/toolLock.ts`); this adds what a row says about itself.

use mewrk_client::t;
pub use mewrk_client::tool_lock::*;

/// What a toned row says about itself (`LockTone.tsx`).
pub fn lock_hint(tone: LockTone) -> &'static str {
    match tone {
        LockTone::Cache => t("缓存还热：改动这一项会让它失效。", "The cache is still warm: changing this throws it away."),
    }
}

/// Milliseconds since the epoch, for reading the lock's clock.
pub fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}
