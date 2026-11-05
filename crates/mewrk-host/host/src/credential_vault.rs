//! macOS credential store: one keychain item, one prompt.
//!
//! The `keyring` crate's native macOS backend stores every entry as its own
//! login-keychain item, and each item's access list names the code signature of
//! the binary that created it. A development build is ad-hoc signed, as is any
//! release not signed with a stable Developer ID, so after every rebuild or
//! update the running binary is a stranger to all of them. Every read, update
//! and delete of every item then raises its own "Mewrk wants to use your
//! confidential information" dialog, and its "Allow" button covers that single
//! access only. The provider page reads two items per provider (binding and
//! secret), and the Codex panel re-reads them every two seconds while a sign-in
//! is pending, so the dialogs arrive faster than they can be answered. Only
//! "Deny" makes them stop.
//!
//! This backend keeps one random master key in the keychain ("Mewrk Safe
//! Storage") and every `keyring` entry in a single file sealed under it — the
//! arrangement Chromium and Electron's `safeStorage` use on macOS, and the one
//! `codex_oauth` already uses for its token file. The master key is read at most
//! once per process and then kept in memory, so a new binary costs one dialog,
//! and answering it with "Always Allow" covers every credential Mewrk holds.
//!
//! The dialog only ever answers something a person did. A read starts silent,
//! with the keychain's user interaction off, and a binary macOS would ask about
//! simply gets an error that says where to allow it. An action that genuinely
//! needs a credential (saving, deleting or revealing a key, starting a run, the
//! Allow button on the Permissions page) holds a [`PromptScope`] across its read,
//! and that read may ask. Choosing "Allow" rather than "Always Allow" is what
//! makes macOS ask twice for one read (the item's access list, then its partition
//! list) and again on the next launch, so the wording that leads to the dialog
//! names the button. A refused dialog is remembered for the rest of the process.
//!
//! The file is shared by every Mewrk identifier on the machine, as the
//! keychain service was, and every write is a locked read-modify-replace, so
//! the production app and a browser-dev instance never lose each other's
//! entries.

use std::{
    collections::BTreeMap,
    fs::{self, OpenOptions},
    io::Write as _,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Mutex, OnceLock,
    },
};

use base64::{engine::general_purpose::STANDARD, Engine as _};
use chacha20poly1305::{
    aead::{Aead, KeyInit, Payload},
    ChaCha20Poly1305, Key, Nonce,
};
use keyring::credential::{
    Credential, CredentialApi, CredentialBuilderApi, CredentialPersistence,
};
use security_framework::os::macos::keychain::SecKeychain;
use serde::{Deserialize, Serialize};
use uuid::Uuid;
use zeroize::{Zeroize, Zeroizing};

use crate::system_permissions::KeychainAccess;
use crate::ui_text::{self, ui_text};

/// The keychain item holding the master key. The service name is what the
/// macOS dialog and Keychain Access show, so it names the product rather than
/// an internal identifier.
const MASTER_KEY_SERVICE: &str = "Mewrk Safe Storage";
const MASTER_KEY_ACCOUNT: &str = "Mewrk";
const MAGIC: &[u8] = b"MEWRK-CREDENTIAL-VAULT-v1\0";
const NONCE_BYTES: usize = 12;
const TAG_BYTES: usize = 16;
const MAX_VAULT_BYTES: u64 = 4 * 1024 * 1024;

static INSTALLED: AtomicBool = AtomicBool::new(false);

/// Routes every `keyring::Entry` in this process through the vault. Must run
/// before the first entry is created; `keyring` binds an entry to whichever
/// builder was the default when the entry was built.
pub(crate) fn install() {
    keyring::set_default_credential_builder(Box::new(VaultCredentialBuilder));
    INSTALLED.store(true, Ordering::SeqCst);
}

static PROMPT_SCOPES: AtomicUsize = AtomicUsize::new(0);

/// While one of these is alive a read of the master key may raise the keychain
/// dialog. Held by actions the user started and that cannot work without a
/// credential, never by reads that only render status.
#[must_use]
pub(crate) struct PromptScope(());

pub(crate) fn allow_prompt() -> PromptScope {
    PROMPT_SCOPES.fetch_add(1, Ordering::SeqCst);
    PromptScope(())
}

impl Drop for PromptScope {
    fn drop(&mut self) {
        PROMPT_SCOPES.fetch_sub(1, Ordering::SeqCst);
    }
}

enum MasterKeyState {
    Unknown,
    /// `remembered` is false after a one-time "Allow": the key is in memory, and
    /// the next launch will have to ask again.
    Ready {
        key: Zeroizing<[u8; 32]>,
        remembered: bool,
    },
    /// macOS would ask, and nobody has asked for it yet.
    NeedsPermission,
    Refused,
}

static MASTER_KEY: Mutex<MasterKeyState> = Mutex::new(MasterKeyState::Unknown);

fn lock_master_key() -> std::sync::MutexGuard<'static, MasterKeyState> {
    MASTER_KEY
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

enum Step {
    Have(Zeroizing<[u8; 32]>),
    Fail(keyring::Error),
    ReadSilently,
    Ask,
}

fn next_step(state: &MasterKeyState, may_ask: bool) -> Step {
    match state {
        MasterKeyState::Ready { key, .. } => Step::Have(key.clone()),
        _ if may_ask => Step::Ask,
        MasterKeyState::Refused => Step::Fail(refused()),
        // Read again each time: another Mewrk may have been given "Always Allow",
        // or the login keychain may have been unlocked.
        MasterKeyState::Unknown | MasterKeyState::NeedsPermission => Step::ReadSilently,
    }
}

/// The master key, from memory after the first successful read. The mutex is
/// held across the keychain call on purpose: concurrent first reads wait for
/// the one dialog instead of stacking a dialog each.
fn master_key(vault: &Vault) -> keyring::Result<Zeroizing<[u8; 32]>> {
    let mut state = lock_master_key();
    match next_step(&state, PROMPT_SCOPES.load(Ordering::SeqCst) > 0) {
        Step::Have(key) => Ok(key),
        Step::Fail(error) => Err(error),
        Step::ReadSilently => match read_silently(vault) {
            Ok(key) => {
                *state = MasterKeyState::Ready {
                    key: key.clone(),
                    remembered: true,
                };
                Ok(key)
            }
            Err(error) => {
                if !is_waiting_for_permission(&error) {
                    log_keychain_failure(&error);
                }
                *state = MasterKeyState::NeedsPermission;
                Err(needs_permission())
            }
        },
        Step::Ask => match load_or_create_master_key(vault) {
            Ok(key) => {
                // Only "Always Allow" leaves this binary able to read it unasked.
                let remembered = read_silently(vault).is_ok();
                *state = MasterKeyState::Ready {
                    key: key.clone(),
                    remembered,
                };
                Ok(key)
            }
            Err(error) => {
                log_keychain_failure(&error);
                *state = MasterKeyState::Refused;
                Err(refused())
            }
        },
    }
}

/// The master key if macOS gives it without asking anyone. Creating a missing
/// one needs no dialog either, as long as the login keychain is unlocked, but
/// that throws away any vault sealed under the old one, which only something a
/// person did may do. The switch is the whole process's: the caller holds the
/// only lock anything here reaches the keychain under.
fn read_silently(vault: &Vault) -> keyring::Result<Zeroizing<[u8; 32]>> {
    let _quiet = SecKeychain::disable_user_interaction()
        .map_err(|error| keyring::Error::PlatformFailure(Box::new(error)))?;
    match read_master_key(&master_key_entry()?) {
        Err(keyring::Error::NoEntry) if !vault.exists() => load_or_create_master_key(vault),
        result => result,
    }
}

/// What a read with user interaction off gets instead of a dialog:
/// `errSecAuthFailed` from the access list (measured on macOS with a throwaway
/// item), `errSecInteractionNotAllowed` from a locked keychain.
fn is_waiting_for_permission(error: &keyring::Error) -> bool {
    let keyring::Error::PlatformFailure(reason) = error else {
        return false;
    };
    reason
        .downcast_ref::<security_framework::base::Error>()
        .is_some_and(|error| matches!(error.code(), -25293 | -25308))
}

/// Where the keychain stands, without asking anyone unless the caller holds a
/// [`PromptScope`]. Not applicable where the vault was never installed, which is
/// every test process: those must not reach the real keychain.
pub(crate) fn access() -> KeychainAccess {
    if !INSTALLED.load(Ordering::SeqCst) {
        return KeychainAccess::NotApplicable;
    }
    if let Ok(vault) = shared_vault() {
        // The outcome is in the state; the error is only its wording.
        let _ = master_key(vault);
    }
    let state = lock_master_key();
    access_of(&state)
}

fn access_of(state: &MasterKeyState) -> KeychainAccess {
    match state {
        MasterKeyState::Ready {
            remembered: true, ..
        } => KeychainAccess::Granted,
        MasterKeyState::Ready { .. } => KeychainAccess::ThisLaunchOnly,
        MasterKeyState::Refused => KeychainAccess::Refused,
        MasterKeyState::Unknown | MasterKeyState::NeedsPermission => {
            KeychainAccess::NeedsPermission
        }
    }
}

/// The person pressed Allow: raise the dialog if macOS wants one, which is also
/// how a one-time "Allow" becomes "Always Allow". Blocks until it is answered.
pub(crate) fn request_access() -> KeychainAccess {
    if !INSTALLED.load(Ordering::SeqCst) {
        return KeychainAccess::NotApplicable;
    }
    let _prompt = allow_prompt();
    {
        let mut state = lock_master_key();
        if matches!(
            *state,
            MasterKeyState::Ready {
                remembered: false,
                ..
            }
        ) {
            *state = MasterKeyState::Unknown;
        }
    }
    access()
}

/// A read that stayed silent, for whatever showed that read's failure.
fn needs_permission() -> keyring::Error {
    keyring::Error::NoStorageAccess(
        ui_text::pick(
            "Mewrk 还没有读取钥匙串的权限。请到「全局设置 → 权限」中允许一次，或重新执行刚才的操作",
            "Mewrk isn't allowed to read the keychain yet. Allow it once under Global settings → Permissions, or do it again",
        )
        .to_owned()
        .into(),
    )
}

/// The refusal as the user reads it, naming the keychain dialog's own button
/// in the app language.
fn refused() -> keyring::Error {
    keyring::Error::NoStorageAccess(
        ui_text!(
            "macOS 钥匙串没有允许 Mewrk 读取「{MASTER_KEY_SERVICE}」。\
             请重新执行刚才的操作，在系统弹窗中输入登录密码后选择「始终允许」",
            "The macOS keychain did not let Mewrk read “{MASTER_KEY_SERVICE}”. \
             Do it again, enter your login password in the system dialog and choose “Always Allow”"
        )
        .into(),
    )
}

fn log_keychain_failure(error: &keyring::Error) {
    // Never `BadEncoding`'s payload: it is the stored bytes themselves.
    let reason = match error {
        keyring::Error::PlatformFailure(reason) | keyring::Error::NoStorageAccess(reason) => {
            reason.to_string()
        }
        keyring::Error::BadEncoding(_) => "master key is not valid UTF-8".into(),
        other => format!("{other:?}"),
    };
    eprintln!("[credential-vault] keychain master key unavailable: {reason}");
}

fn master_key_entry() -> keyring::Result<keyring::macos::MacCredential> {
    keyring::macos::MacCredential::new_with_target(None, MASTER_KEY_SERVICE, MASTER_KEY_ACCOUNT)
}

fn read_master_key(
    entry: &keyring::macos::MacCredential,
) -> keyring::Result<Zeroizing<[u8; 32]>> {
    let encoded = Zeroizing::new(entry.get_password()?);
    decode_master_key(&encoded).ok_or_else(|| {
        keyring::Error::PlatformFailure(
            ui_text!(
                "「{MASTER_KEY_SERVICE}」的内容无效",
                "The contents of “{MASTER_KEY_SERVICE}” are invalid"
            )
            .into(),
        )
    })
}

fn load_or_create_master_key(vault: &Vault) -> keyring::Result<Zeroizing<[u8; 32]>> {
    let entry = master_key_entry()?;
    match read_master_key(&entry) {
        Err(keyring::Error::NoEntry) => {}
        result => return result,
    }
    // Creation is serialized with every other process through the vault lock,
    // and the item is looked up again under it: another Mewrk process may have
    // created it between the miss above and taking the lock.
    let _lock = vault.lock()?;
    match read_master_key(&entry) {
        Err(keyring::Error::NoEntry) => {}
        result => return result,
    }
    let key = generate_key();
    let encoded = Zeroizing::new(STANDARD.encode(key.as_ref()));
    entry.set_password(&encoded)?;
    // A vault left behind was sealed under a key that no longer exists (the
    // item was deleted in Keychain Access, or by `reset:data --keys`), so
    // nothing in it can ever be opened again. Starting empty lets the user
    // re-enter their keys instead of failing every read forever.
    vault.discard()?;
    Ok(key)
}

fn decode_master_key(encoded: &str) -> Option<Zeroizing<[u8; 32]>> {
    let decoded = Zeroizing::new(STANDARD.decode(encoded.trim()).ok()?);
    let key: [u8; 32] = decoded.as_slice().try_into().ok()?;
    Some(Zeroizing::new(key))
}

fn generate_key() -> Zeroizing<[u8; 32]> {
    let mut key = Zeroizing::new([0u8; 32]);
    key[..16].copy_from_slice(Uuid::new_v4().as_bytes());
    key[16..].copy_from_slice(Uuid::new_v4().as_bytes());
    key
}

/// The sealed file and its lock. Paths only; every operation re-reads the file,
/// so a write by another process is visible to the next read here.
struct Vault {
    path: PathBuf,
    lock_path: PathBuf,
}

fn shared_vault() -> keyring::Result<&'static Vault> {
    static VAULT: OnceLock<Option<Vault>> = OnceLock::new();
    VAULT
        .get_or_init(|| {
            let directory = dirs::home_dir()?.join(".mewrk").join("credential-vault");
            Some(Vault::at(&directory))
        })
        .as_ref()
        .ok_or_else(|| {
            store_failure(
                "无法确定用户主目录，凭据库不可用",
                "Could not find the home folder, so the credential store is unavailable",
            )
        })
}

/// A failure of the vault file, worded in the app language.
fn store_failure(zh: &str, en: &str) -> keyring::Error {
    keyring::Error::PlatformFailure(ui_text::pick(zh, en).to_owned().into())
}

/// Plaintext of the vault. Secrets are base64 so the JSON stays text; every
/// one is wiped when the contents are dropped.
#[derive(Serialize, Deserialize)]
struct VaultContents {
    version: u32,
    /// service -> user -> base64(secret)
    entries: BTreeMap<String, BTreeMap<String, String>>,
}

impl Drop for VaultContents {
    fn drop(&mut self) {
        for users in self.entries.values_mut() {
            for secret in users.values_mut() {
                secret.zeroize();
            }
        }
    }
}

impl Vault {
    fn at(directory: &Path) -> Self {
        Self {
            path: directory.join("vault.v1.bin"),
            lock_path: directory.join(".lock"),
        }
    }

    fn exists(&self) -> bool {
        fs::symlink_metadata(&self.path).is_ok()
    }

    fn directory(&self) -> keyring::Result<&Path> {
        self.path
            .parent()
            .ok_or_else(|| store_failure("凭据库路径无效", "The credential store path is invalid"))
    }

    fn ensure_directory(&self) -> keyring::Result<()> {
        let directory = self.directory()?;
        fs::create_dir_all(directory).map_err(|_| {
            store_failure(
                "无法创建凭据库目录",
                "Could not create the credential store folder",
            )
        })?;
        let metadata = fs::symlink_metadata(directory).map_err(|_| {
            store_failure(
                "无法检查凭据库目录",
                "Could not check the credential store folder",
            )
        })?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Err(store_failure(
                "凭据库目录不安全",
                "The credential store folder is not safe",
            ));
        }
        set_mode(directory, 0o700).map_err(|_| {
            store_failure(
                "无法保护凭据库目录",
                "Could not protect the credential store folder",
            )
        })
    }

    /// Exclusive cross-process lock for a read-modify-replace. Always the
    /// innermost lock: nothing else is acquired while it is held.
    fn lock(&self) -> keyring::Result<fs::File> {
        self.ensure_directory()?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&self.lock_path)
            .map_err(|_| {
                store_failure(
                    "无法打开凭据库锁文件",
                    "Could not open the credential store lock file",
                )
            })?;
        let metadata = fs::symlink_metadata(&self.lock_path).map_err(|_| {
            store_failure(
                "无法检查凭据库锁文件",
                "Could not check the credential store lock file",
            )
        })?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(store_failure(
                "凭据库锁文件不安全",
                "The credential store lock file is not safe",
            ));
        }
        fs2::FileExt::lock_exclusive(&file)
            .map_err(|_| store_failure("无法锁定凭据库", "Could not lock the credential store"))?;
        Ok(file)
    }

    fn read(&self, key: &[u8; 32]) -> keyring::Result<VaultContents> {
        let metadata = match fs::symlink_metadata(&self.path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(VaultContents::new())
            }
            Err(_) => {
                return Err(store_failure(
                    "无法检查凭据库文件",
                    "Could not check the credential store file",
                ))
            }
        };
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            return Err(store_failure(
                "凭据库文件不安全",
                "The credential store file is not safe",
            ));
        }
        if metadata.len() > MAX_VAULT_BYTES {
            return Err(store_failure(
                "凭据库文件超过大小上限",
                "The credential store file is over the size limit",
            ));
        }
        let sealed = fs::read(&self.path).map_err(|_| {
            store_failure(
                "无法读取凭据库文件",
                "Could not read the credential store file",
            )
        })?;
        open(&sealed, key)
    }

    fn write(&self, key: &[u8; 32], contents: &VaultContents) -> keyring::Result<()> {
        let sealed = seal(contents, key)?;
        if sealed.len() as u64 > MAX_VAULT_BYTES {
            return Err(store_failure(
                "凭据库超过大小上限",
                "The credential store is over the size limit",
            ));
        }
        self.ensure_directory()?;
        let directory = self.directory()?;
        let temporary = directory.join(format!(".vault.{}.tmp", Uuid::new_v4()));
        let result = (|| {
            let mut file = open_private_new(&temporary).map_err(|_| {
                store_failure(
                    "无法创建凭据库临时文件",
                    "Could not create the credential store's temporary file",
                )
            })?;
            file.write_all(&sealed).map_err(|_| {
                store_failure(
                    "无法写入凭据库临时文件",
                    "Could not write the credential store's temporary file",
                )
            })?;
            file.sync_all().map_err(|_| {
                store_failure(
                    "无法刷新凭据库临时文件",
                    "Could not flush the credential store's temporary file",
                )
            })?;
            drop(file);
            fs::rename(&temporary, &self.path).map_err(|_| {
                store_failure(
                    "无法替换凭据库文件",
                    "Could not replace the credential store file",
                )
            })
        })();
        let _ = fs::remove_file(&temporary);
        result
    }

    /// Removes a vault that can no longer be opened. Caller holds the lock.
    fn discard(&self) -> keyring::Result<()> {
        match fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(_) => Err(store_failure(
                "无法移除失效的凭据库文件",
                "Could not remove the credential store file that can no longer be opened",
            )),
        }
    }

    fn get(&self, key: &[u8; 32], service: &str, user: &str) -> keyring::Result<Vec<u8>> {
        let contents = self.read(key)?;
        let encoded = contents
            .entries
            .get(service)
            .and_then(|users| users.get(user))
            .ok_or(keyring::Error::NoEntry)?;
        STANDARD.decode(encoded).map_err(|_| {
            store_failure(
                "凭据库中的条目格式无效",
                "An entry in the credential store is malformed",
            )
        })
    }

    fn set(&self, key: &[u8; 32], service: &str, user: &str, secret: &[u8]) -> keyring::Result<()> {
        let _lock = self.lock()?;
        let mut contents = self.read(key)?;
        contents
            .entries
            .entry(service.to_owned())
            .or_default()
            .insert(user.to_owned(), STANDARD.encode(secret));
        self.write(key, &contents)
    }

    fn delete(&self, key: &[u8; 32], service: &str, user: &str) -> keyring::Result<()> {
        let _lock = self.lock()?;
        let mut contents = self.read(key)?;
        let users = contents
            .entries
            .get_mut(service)
            .ok_or(keyring::Error::NoEntry)?;
        let mut removed = users.remove(user).ok_or(keyring::Error::NoEntry)?;
        removed.zeroize();
        if users.is_empty() {
            contents.entries.remove(service);
        }
        self.write(key, &contents)
    }
}

impl VaultContents {
    fn new() -> Self {
        Self {
            version: 1,
            entries: BTreeMap::new(),
        }
    }
}

fn seal(contents: &VaultContents, key: &[u8; 32]) -> keyring::Result<Zeroizing<Vec<u8>>> {
    let plaintext =
        Zeroizing::new(serde_json::to_vec(contents).map_err(|_| {
            store_failure("无法编码凭据库", "Could not encode the credential store")
        })?);
    let nonce_source = Uuid::new_v4();
    let nonce = &nonce_source.as_bytes()[..NONCE_BYTES];
    let ciphertext = ChaCha20Poly1305::new(Key::from_slice(key))
        .encrypt(
            Nonce::from_slice(nonce),
            Payload {
                msg: &plaintext,
                aad: MAGIC,
            },
        )
        .map_err(|_| store_failure("无法加密凭据库", "Could not encrypt the credential store"))?;
    let mut sealed = Zeroizing::new(Vec::with_capacity(
        MAGIC.len() + NONCE_BYTES + ciphertext.len(),
    ));
    sealed.extend_from_slice(MAGIC);
    sealed.extend_from_slice(nonce);
    sealed.extend_from_slice(&ciphertext);
    Ok(sealed)
}

fn open(sealed: &[u8], key: &[u8; 32]) -> keyring::Result<VaultContents> {
    if sealed.len() < MAGIC.len() + NONCE_BYTES + TAG_BYTES || !sealed.starts_with(MAGIC) {
        return Err(store_failure(
            "凭据库文件格式无效",
            "The credential store file is malformed",
        ));
    }
    let nonce_end = MAGIC.len() + NONCE_BYTES;
    let plaintext = Zeroizing::new(
        ChaCha20Poly1305::new(Key::from_slice(key))
            .decrypt(
                Nonce::from_slice(&sealed[MAGIC.len()..nonce_end]),
                Payload {
                    msg: &sealed[nonce_end..],
                    aad: MAGIC,
                },
            )
            .map_err(|_| {
                store_failure(
                    "凭据库认证失败",
                    "The credential store failed authentication",
                )
            })?,
    );
    let contents: VaultContents = serde_json::from_slice(&plaintext).map_err(|_| {
        store_failure(
            "凭据库内容无效",
            "The credential store's contents are invalid",
        )
    })?;
    if contents.version != 1 {
        return Err(store_failure(
            "凭据库版本不受支持",
            "The credential store's version is not supported",
        ));
    }
    Ok(contents)
}

fn open_private_new(path: &Path) -> std::io::Result<fs::File> {
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
}

#[cfg(not(unix))]
fn set_mode(_path: &Path, _mode: u32) -> std::io::Result<()> {
    Ok(())
}

struct VaultCredentialBuilder;

impl CredentialBuilderApi for VaultCredentialBuilder {
    fn build(
        &self,
        _target: Option<&str>,
        service: &str,
        user: &str,
    ) -> keyring::Result<Box<Credential>> {
        // Same rule as the native backend: an empty attribute is a wildcard in
        // Keychain Services, and entries must stay addressable one at a time.
        if service.is_empty() || user.is_empty() {
            return Err(keyring::Error::Invalid(
                if service.is_empty() { "service" } else { "user" }.into(),
                "cannot be empty".into(),
            ));
        }
        Ok(Box::new(VaultCredential {
            service: service.to_owned(),
            user: user.to_owned(),
        }))
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn persistence(&self) -> CredentialPersistence {
        CredentialPersistence::UntilDelete
    }
}

#[derive(Debug)]
struct VaultCredential {
    service: String,
    user: String,
}

impl CredentialApi for VaultCredential {
    fn set_secret(&self, secret: &[u8]) -> keyring::Result<()> {
        // Writes only ever come from something the user did.
        let _prompt = allow_prompt();
        let vault = shared_vault()?;
        let key = master_key(vault)?;
        vault.set(&key, &self.service, &self.user, secret)
    }

    fn get_secret(&self) -> keyring::Result<Vec<u8>> {
        let vault = shared_vault()?;
        let key = master_key(vault)?;
        vault.get(&key, &self.service, &self.user)
    }

    fn delete_credential(&self) -> keyring::Result<()> {
        let _prompt = allow_prompt();
        let vault = shared_vault()?;
        let key = master_key(vault)?;
        vault.delete(&key, &self.service, &self.user)
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: [u8; 32] = [7; 32];

    fn vault() -> (tempfile::TempDir, Vault) {
        let root = tempfile::tempdir().unwrap();
        let vault = Vault::at(&root.path().join("credential-vault"));
        (root, vault)
    }

    /// The refusal names the keychain dialog's button the way macOS shows it
    /// in the app language.
    #[test]
    fn a_refused_keychain_names_always_allow_in_the_app_language() {
        let reason = |error: keyring::Error| match error {
            keyring::Error::NoStorageAccess(reason) => reason.to_string(),
            other => panic!("unexpected {other:?}"),
        };
        assert!(reason(refused()).contains("「始终允许」"));
        let english = crate::ui_text::with_language(crate::model::ResolvedLanguage::EnUs, || {
            reason(refused())
        });
        assert!(english.contains("“Always Allow”"), "{english}");
        assert!(english.contains("Mewrk Safe Storage"), "{english}");
    }

    #[test]
    fn entries_round_trip_and_stay_separate() {
        let (_root, vault) = vault();
        assert!(matches!(
            vault.get(&KEY, "com.mewrk.api", "a"),
            Err(keyring::Error::NoEntry)
        ));
        vault.set(&KEY, "com.mewrk.api", "a", b"secret-a").unwrap();
        vault.set(&KEY, "com.mewrk.api", "b", b"secret-b").unwrap();
        vault.set(&KEY, "other", "a", b"secret-c").unwrap();
        assert_eq!(vault.get(&KEY, "com.mewrk.api", "a").unwrap(), b"secret-a");
        assert_eq!(vault.get(&KEY, "com.mewrk.api", "b").unwrap(), b"secret-b");
        assert_eq!(vault.get(&KEY, "other", "a").unwrap(), b"secret-c");

        vault.set(&KEY, "com.mewrk.api", "a", b"replaced").unwrap();
        assert_eq!(vault.get(&KEY, "com.mewrk.api", "a").unwrap(), b"replaced");

        vault.delete(&KEY, "com.mewrk.api", "a").unwrap();
        assert!(matches!(
            vault.get(&KEY, "com.mewrk.api", "a"),
            Err(keyring::Error::NoEntry)
        ));
        assert!(matches!(
            vault.delete(&KEY, "com.mewrk.api", "a"),
            Err(keyring::Error::NoEntry)
        ));
        assert_eq!(vault.get(&KEY, "com.mewrk.api", "b").unwrap(), b"secret-b");
    }

    #[test]
    fn the_file_is_sealed_and_private() {
        let (_root, vault) = vault();
        vault.set(&KEY, "svc", "user", b"plaintext-marker").unwrap();
        let sealed = fs::read(&vault.path).unwrap();
        assert!(sealed.starts_with(MAGIC));
        assert!(!sealed
            .windows(b"plaintext-marker".len())
            .any(|window| window == b"plaintext-marker"));
        assert!(!sealed.windows(b"svc".len()).any(|window| window == b"svc"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let file_mode = fs::metadata(&vault.path).unwrap().permissions().mode() & 0o777;
            let directory_mode =
                fs::metadata(vault.directory().unwrap()).unwrap().permissions().mode() & 0o777;
            assert_eq!(file_mode, 0o600);
            assert_eq!(directory_mode, 0o700);
        }
    }

    #[test]
    fn a_different_key_or_a_tampered_file_cannot_be_read() {
        let (_root, vault) = vault();
        vault.set(&KEY, "svc", "user", b"secret").unwrap();
        assert!(vault.get(&[8; 32], "svc", "user").is_err());

        let mut sealed = fs::read(&vault.path).unwrap();
        let last = sealed.len() - 1;
        sealed[last] ^= 1;
        fs::write(&vault.path, &sealed).unwrap();
        assert!(matches!(
            vault.get(&KEY, "svc", "user"),
            Err(keyring::Error::PlatformFailure(_))
        ));
    }

    #[test]
    fn a_vault_exists_once_something_is_stored() {
        let (_root, vault) = vault();
        assert!(!vault.exists());
        vault.set(&KEY, "svc", "user", b"secret").unwrap();
        assert!(vault.exists());
    }

    #[test]
    fn discarding_starts_an_empty_vault() {
        let (_root, vault) = vault();
        vault.set(&KEY, "svc", "user", b"secret").unwrap();
        vault.discard().unwrap();
        vault.discard().unwrap();
        assert!(matches!(
            vault.get(&[9; 32], "svc", "user"),
            Err(keyring::Error::NoEntry)
        ));
    }

    #[test]
    fn concurrent_writers_keep_every_entry() {
        let (_root, vault) = vault();
        let vault = std::sync::Arc::new(vault);
        let writers: Vec<_> = (0..8)
            .map(|index| {
                let vault = std::sync::Arc::clone(&vault);
                std::thread::spawn(move || {
                    vault
                        .set(&KEY, "svc", &format!("user-{index}"), b"secret")
                        .unwrap();
                })
            })
            .collect();
        for writer in writers {
            writer.join().unwrap();
        }
        for index in 0..8 {
            assert_eq!(
                vault.get(&KEY, "svc", &format!("user-{index}")).unwrap(),
                b"secret"
            );
        }
    }

    #[test]
    fn a_master_key_round_trips_through_its_text_form() {
        let key = generate_key();
        let encoded = STANDARD.encode(key.as_ref());
        assert_eq!(decode_master_key(&encoded).unwrap().as_ref(), key.as_ref());
        assert!(decode_master_key("not base64").is_none());
        assert!(decode_master_key(&STANDARD.encode([0u8; 16])).is_none());
    }

    fn ready(remembered: bool) -> MasterKeyState {
        MasterKeyState::Ready {
            key: Zeroizing::new(KEY),
            remembered,
        }
    }

    /// The dialog answers an action, and nothing else may raise it: not a first
    /// read, not a read after macOS said it would ask, not one after a refusal.
    #[test]
    fn only_an_action_may_ask() {
        let silent = [MasterKeyState::Unknown, MasterKeyState::NeedsPermission];
        for state in &silent {
            assert!(matches!(next_step(state, false), Step::ReadSilently));
            assert!(matches!(next_step(state, true), Step::Ask));
        }
        assert!(matches!(
            next_step(&MasterKeyState::Refused, false),
            Step::Fail(keyring::Error::NoStorageAccess(_))
        ));
        assert!(matches!(
            next_step(&MasterKeyState::Refused, true),
            Step::Ask
        ));
        for may_ask in [false, true] {
            assert!(matches!(next_step(&ready(true), may_ask), Step::Have(_)));
        }
    }

    #[test]
    fn a_prompt_scope_lasts_as_long_as_it_is_held() {
        let before = PROMPT_SCOPES.load(Ordering::SeqCst);
        let first = allow_prompt();
        let second = allow_prompt();
        assert_eq!(PROMPT_SCOPES.load(Ordering::SeqCst), before + 2);
        drop(first);
        assert_eq!(PROMPT_SCOPES.load(Ordering::SeqCst), before + 1);
        drop(second);
        assert_eq!(PROMPT_SCOPES.load(Ordering::SeqCst), before);
    }

    #[test]
    fn each_state_reads_as_what_the_page_shows() {
        assert_eq!(access_of(&ready(true)), KeychainAccess::Granted);
        assert_eq!(access_of(&ready(false)), KeychainAccess::ThisLaunchOnly);
        assert_eq!(
            access_of(&MasterKeyState::NeedsPermission),
            KeychainAccess::NeedsPermission
        );
        assert_eq!(access_of(&MasterKeyState::Refused), KeychainAccess::Refused);
    }

    /// The two codes a silent read gets instead of a dialog, and nothing else:
    /// another failure is worth a line in the log.
    #[test]
    fn only_the_silent_refusals_count_as_waiting_for_permission() {
        let failure = |code| {
            keyring::Error::PlatformFailure(Box::new(security_framework::base::Error::from_code(
                code,
            )))
        };
        assert!(is_waiting_for_permission(&failure(-25293)));
        assert!(is_waiting_for_permission(&failure(-25308)));
        assert!(!is_waiting_for_permission(&failure(-25300)));
        assert!(!is_waiting_for_permission(&keyring::Error::NoEntry));
    }

    #[test]
    fn the_silent_failure_says_where_to_allow_it() {
        let reason = |error: keyring::Error| match error {
            keyring::Error::NoStorageAccess(reason) => reason.to_string(),
            other => panic!("unexpected {other:?}"),
        };
        assert!(reason(needs_permission()).contains("权限"));
        let english = crate::ui_text::with_language(crate::model::ResolvedLanguage::EnUs, || {
            reason(needs_permission())
        });
        assert!(english.contains("Permissions"), "{english}");
    }
}
