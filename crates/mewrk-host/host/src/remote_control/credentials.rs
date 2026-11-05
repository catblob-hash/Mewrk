//! Remembered VNC passwords, in the system credential store.
//!
//! A password reaches a session only from the person at the preview pane
//! (`docs/remote-control.md` §3.2): never from `.mewrk/launch.json`, never from
//! the model. When they tick "remember", it goes into the operating system's
//! credential store under service [`SERVICE`], keyed by where the server is —
//! the place's stable identity, the port and the account name ([`account`]) —
//! so a password typed for one machine's screen is never offered to another's.
//! On macOS the app routes every `keyring` entry through its own vault
//! (`credential_vault.rs`); the plain API used here goes there unchanged.
//!
//! The store is a convenience, not a dependency: any error reading it means
//! there is no stored password (the person signs in again), any error writing
//! it means the password is not remembered. The caller logs the error; nothing
//! here panics or blocks a connection on it.

use zeroize::Zeroizing;

/// The credential store's service name for remote-control sign-ins.
pub const SERVICE: &str = "com.mewrk.remote-control";

/// Passwords longer than this are not stored or read back: no VNC or Apple
/// Remote Desktop password is anywhere near it, and a corrupt entry should not
/// become a megabyte held in memory.
const MAX_PASSWORD_BYTES: usize = 1024;

/// The account a VNC sign-in is stored under:
/// `vnc|<place key>|<port>|<username or empty>`. The place key is `local` or
/// `ssh:<machine id>`, never a display name, so renaming a machine neither
/// loses its password nor hands it to another machine that takes the name.
pub fn account(place_key: &str, port: u16, username: Option<&str>) -> String {
    format!("vnc|{place_key}|{port}|{}", username.unwrap_or(""))
}

/// Where remembered passwords live. The session talks to this rather than to
/// `keyring` directly so its tests never touch the real store.
pub trait CredentialStore: Send + Sync {
    /// The stored password for `account`, `None` when there is none.
    fn get(&self, account: &str) -> Result<Option<Zeroizing<String>>, String>;
    fn set(&self, account: &str, password: &str) -> Result<(), String>;
    /// Removes the stored password; removing one that is not there succeeds.
    fn delete(&self, account: &str) -> Result<(), String>;
}

/// The operating system's credential store.
#[derive(Clone, Copy, Debug, Default)]
pub struct KeyringStore;

fn entry(account: &str) -> Result<keyring::Entry, String> {
    keyring::Entry::new(SERVICE, account).map_err(|error| store_error("open", error))
}

/// A credential-store failure for the session's log. Like the API-key store's
/// messages, only `NoStorageAccess` keeps its reason (on macOS, a keychain
/// prompt that was refused, and how to get it back); every other variant stays
/// generic — `BadEncoding` carries the stored secret's own bytes.
fn store_error(action: &str, error: keyring::Error) -> String {
    match error {
        keyring::Error::NoStorageAccess(reason) => {
            format!(
                "could not {action} the stored password in the system credential store: {reason}"
            )
        }
        _ => format!("could not {action} the stored password in the system credential store"),
    }
}

impl CredentialStore for KeyringStore {
    fn get(&self, account: &str) -> Result<Option<Zeroizing<String>>, String> {
        match entry(account)?.get_password() {
            Ok(password) => {
                let password = Zeroizing::new(password);
                if password.is_empty() || password.len() > MAX_PASSWORD_BYTES {
                    return Ok(None);
                }
                Ok(Some(password))
            }
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(error) => Err(store_error("read", error)),
        }
    }

    fn set(&self, account: &str, password: &str) -> Result<(), String> {
        if password.is_empty() || password.len() > MAX_PASSWORD_BYTES {
            return Err("the password is empty or too long to store".to_owned());
        }
        entry(account)?
            .set_password(password)
            .map_err(|error| store_error("save", error))
    }

    fn delete(&self, account: &str) -> Result<(), String> {
        match entry(account)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(error) => Err(store_error("delete", error)),
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::{collections::HashMap, sync::Mutex};

    use super::*;

    /// An in-memory store for tests, which must not reach the real one.
    #[derive(Default)]
    pub(crate) struct MemoryStore {
        pub(crate) entries: Mutex<HashMap<String, String>>,
        pub(crate) fail: bool,
    }

    impl CredentialStore for MemoryStore {
        fn get(&self, account: &str) -> Result<Option<Zeroizing<String>>, String> {
            if self.fail {
                return Err(
                    "could not read the stored password in the system credential store".into(),
                );
            }
            Ok(self
                .entries
                .lock()
                .unwrap()
                .get(account)
                .map(|password| Zeroizing::new(password.clone())))
        }

        fn set(&self, account: &str, password: &str) -> Result<(), String> {
            if self.fail {
                return Err(
                    "could not save the stored password in the system credential store".into(),
                );
            }
            self.entries
                .lock()
                .unwrap()
                .insert(account.to_owned(), password.to_owned());
            Ok(())
        }

        fn delete(&self, account: &str) -> Result<(), String> {
            self.entries.lock().unwrap().remove(account);
            Ok(())
        }
    }

    #[test]
    fn accounts_name_the_place_port_and_user() {
        assert_eq!(account("local", 5901, None), "vnc|local|5901|");
        assert_eq!(
            account("ssh:4f1c", 5900, Some("holycat")),
            "vnc|ssh:4f1c|5900|holycat"
        );
        assert_ne!(account("ssh:a", 5900, None), account("ssh:b", 5900, None));
        assert_ne!(account("local", 5900, None), account("local", 5901, None));
    }

    #[test]
    fn a_keyring_error_keeps_only_a_storage_access_reason() {
        let refused = store_error(
            "read",
            keyring::Error::NoStorageAccess("the keychain prompt was refused".into()),
        );
        assert!(
            refused.contains("the keychain prompt was refused"),
            "{refused}"
        );
        let encoded = store_error("read", keyring::Error::BadEncoding(b"hunter2".to_vec()));
        assert!(!encoded.contains("hunter2"), "{encoded}");
    }
}
