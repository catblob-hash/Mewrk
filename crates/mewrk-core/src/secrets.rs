//! API keys. They live in the macOS Keychain, never in `document.json`. An environment variable overrides
//! the Keychain, which is also how tests and headless runs supply them.

const SERVICE: &str = "dev.mewrk.native";

fn env_name(provider_id: &str) -> String {
    let id: String = provider_id.chars().map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_uppercase() } else { '_' }).collect();
    format!("MEWRK_KEY_{id}")
}

#[cfg(target_os = "macos")]
pub fn get(provider_id: &str) -> Option<String> {
    if let Ok(v) = std::env::var(env_name(provider_id)) {
        return Some(v);
    }
    let bytes = security_framework::passwords::get_generic_password(SERVICE, provider_id).ok()?;
    String::from_utf8(bytes).ok()
}

#[cfg(target_os = "macos")]
pub fn set(provider_id: &str, key: &str) -> Result<(), String> {
    if key.is_empty() {
        return match security_framework::passwords::delete_generic_password(SERVICE, provider_id) {
            Ok(()) => Ok(()),
            // Nothing stored is what was asked for.
            Err(e) if e.code() == -25300 => Ok(()),
            Err(e) => Err(e.to_string()),
        };
    }
    security_framework::passwords::set_generic_password(SERVICE, provider_id, key.as_bytes()).map_err(|e| e.to_string())
}

#[cfg(not(target_os = "macos"))]
pub fn get(provider_id: &str) -> Option<String> {
    std::env::var(env_name(provider_id)).ok()
}

#[cfg(not(target_os = "macos"))]
pub fn set(_: &str, _: &str) -> Result<(), String> {
    Err("no credential store on this platform".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Touches the real login keychain, so it only runs when asked for: `cargo test -p mewrk-core -- --ignored keychain`.
    #[test]
    #[ignore]
    fn the_keychain_round_trips() {
        let id = format!("test-{}", crate::model::new_id());
        assert_eq!(get(&id), None);
        set(&id, "secret").unwrap();
        assert_eq!(get(&id).as_deref(), Some("secret"));
        set(&id, "").unwrap();
        assert_eq!(get(&id), None);
        set(&id, "").unwrap();
    }

    #[test]
    fn the_environment_overrides_by_provider() {
        assert_eq!(env_name("a-b.c"), "MEWRK_KEY_A_B_C");
    }
}
