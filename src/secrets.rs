//! Credential store for API tokens / PATs / OAuth secrets.
//!
//! As of the "OS keychain removal" refactor this is the single credential
//! backend: a JSON file at
//! `~/Library/Application Support/mergefox/secrets.json`
//! (platform-equivalent on Linux / Windows), chmod 0600, with an
//! in-memory cache in front.
//!
//! Why not the OS keychain?
//! -----------------------
//! macOS spawned a swarm of "AutoFill (mergefox)" helper processes every
//! time we called `keyring::Entry::get_password` — one XPC session per
//! request, each ~15 MB resident. Fifteen live helpers at steady state
//! was normal, eating ~230 MB of RAM attributed to our bundle ID for no
//! functional benefit (the user still had to click Allow on every prompt
//! before we got our own cache). We accept the tradeoff of plaintext-at-
//! rest for:
//!   * zero consent prompts,
//!   * no OS helper overhead,
//!   * portable single-file backup,
//!   * matching behaviour on Linux / Windows where keyring's UX is
//!     similarly inconsistent.
//!
//! Security model
//! --------------
//! * File is `0600` (owner read/write only).
//! * Same threat surface as `~/.ssh/id_*` — any process running as the
//!   user can read it.
//! * FileVault protects at rest when the disk is locked.
//! * No user passphrase / key derivation on top; add that later if we
//!   decide the UX cost is worth it.
//!
//! Concurrency
//! -----------
//! The store is held as `Arc<SecretStore>` on `MergeFoxApp` so background
//! tasks clone a handle cheaply. Internal state is behind `Mutex`.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use anyhow::{Context, Result};
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};

/// Process-wide instance of the secret store.
///
/// Initialized once by `MergeFoxApp::new` and accessed by the existing
/// module-level shims in `providers::pat` and `ai::config` so all ~11
/// call sites get the shared cache without parameter-threading.
static GLOBAL_STORE: OnceLock<Arc<SecretStore>> = OnceLock::new();

/// A `(service, account)` pair used as the credential lookup key.
/// `String`-owned so it drops straight into a `BTreeMap` key.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Credential {
    pub service: String,
    pub account: String,
}

impl Credential {
    pub fn new(service: impl Into<String>, account: impl Into<String>) -> Self {
        Self {
            service: service.into(),
            account: account.into(),
        }
    }
}

pub struct SecretStore {
    /// In-memory cache. Every `load` with a cache hit returns here
    /// without touching the file, which keeps hot-path operations
    /// (per-frame sidebar ref lookups, per-commit-click PAT fetches)
    /// free of disk I/O.
    cache: Mutex<BTreeMap<Credential, SecretString>>,
    file_path: PathBuf,
}

impl SecretStore {
    pub fn new(file_path: PathBuf) -> Self {
        Self {
            cache: Mutex::new(BTreeMap::new()),
            file_path,
        }
    }

    /// Install this store as the process-wide default. Idempotent.
    pub fn install_global(self: Arc<Self>) {
        let _ = GLOBAL_STORE.set(self);
    }

    /// Access the process-wide store. `None` during early startup / tests.
    pub fn global() -> Option<&'static Arc<SecretStore>> {
        GLOBAL_STORE.get()
    }

    /// Load a secret. `Ok(None)` when no entry exists.
    pub fn load(&self, cred: &Credential) -> Result<Option<SecretString>> {
        if let Some(cached) = self.cache.lock().unwrap().get(cred) {
            return Ok(Some(SecretString::new(cached.expose_secret().clone())));
        }
        let loaded = read_file(&self.file_path, cred)?;
        if let Some(ref value) = loaded {
            self.cache.lock().unwrap().insert(
                cred.clone(),
                SecretString::new(value.expose_secret().clone()),
            );
        }
        Ok(loaded)
    }

    /// Store a secret. Writes to file first, then updates cache so a
    /// failed disk write never lies to in-process readers.
    pub fn save(&self, cred: &Credential, value: SecretString) -> Result<()> {
        write_file(&self.file_path, cred, &value)?;
        self.cache.lock().unwrap().insert(
            cred.clone(),
            SecretString::new(value.expose_secret().clone()),
        );
        Ok(())
    }

    /// Remove a secret. No-op (not an error) if it wasn't there.
    pub fn delete(&self, cred: &Credential) -> Result<()> {
        delete_file(&self.file_path, cred)?;
        self.cache.lock().unwrap().remove(cred);
        Ok(())
    }

    // ---- convenience methods for specific credential types ----

    /// AI API key for an endpoint. Returns an empty secret (not an error)
    /// when no key is stored — mirrors the previous `ai::load_api_key`
    /// semantics so call sites don't have to unwrap `Option`.
    pub fn load_api_key(&self, endpoint_name: &str) -> Result<SecretString> {
        let cred = Credential::new(
            crate::ai::config::EndpointKeyringKey::SERVICE,
            endpoint_name,
        );
        Ok(self
            .load(&cred)?
            .unwrap_or_else(|| SecretString::new(String::new())))
    }

    pub fn save_api_key(&self, endpoint_name: &str, key: &SecretString) -> Result<()> {
        let cred = Credential::new(
            crate::ai::config::EndpointKeyringKey::SERVICE,
            endpoint_name,
        );
        self.save(&cred, SecretString::new(key.expose_secret().clone()))
    }

    pub fn delete_api_key(&self, endpoint_name: &str) -> Result<()> {
        let cred = Credential::new(
            crate::ai::config::EndpointKeyringKey::SERVICE,
            endpoint_name,
        );
        self.delete(&cred)
    }

    /// Provider Personal Access Token.
    pub fn load_pat(&self, account: &crate::providers::AccountId) -> Result<Option<SecretString>> {
        let cred = Credential::new(account.keyring_service(), account.keyring_user());
        self.load(&cred)
    }

    pub fn store_pat(
        &self,
        account: &crate::providers::AccountId,
        token: SecretString,
    ) -> Result<()> {
        let cred = Credential::new(account.keyring_service(), account.keyring_user());
        self.save(&cred, token)
    }

    pub fn delete_pat(&self, account: &crate::providers::AccountId) -> Result<()> {
        let cred = Credential::new(account.keyring_service(), account.keyring_user());
        self.delete(&cred)
    }

    // ---------- MCP session token ----------
    //
    // The MCP stdio server authenticates callers via a shared secret
    // passed in `MERGEFOX_MCP_TOKEN`. The UI owns the canonical value
    // (generates it on first read, displays + regenerates in
    // Settings → Integrations → MCP) and stashes it in the secret
    // store so it persists across launches.

    /// Load the current MCP session token, generating + persisting a
    /// new one if none is set. Always returns a value — this is the
    /// single call site Settings uses when rendering the MCP panel,
    /// so "nothing yet" would be a distracting empty-state.
    pub fn load_or_generate_mcp_token(&self) -> Result<SecretString> {
        let cred = mcp_credential();
        if let Some(existing) = self.load(&cred)? {
            return Ok(existing);
        }
        let token = generate_mcp_token();
        self.save(&cred, token.clone())?;
        Ok(token)
    }

    /// Rotate the token — persists a fresh value and returns it. Use
    /// this when the user clicks "Regenerate" in Settings; clients
    /// that were configured with the old token will start failing
    /// `initialize` with a clear "session token does not match"
    /// message (see `mcp::server`).
    pub fn regenerate_mcp_token(&self) -> Result<SecretString> {
        let cred = mcp_credential();
        let token = generate_mcp_token();
        self.save(&cred, token.clone())?;
        Ok(token)
    }

    /// Remove the stored token — MCP server will then run without
    /// authentication (with a loud `tracing::warn!` on startup).
    pub fn delete_mcp_token(&self) -> Result<()> {
        self.delete(&mcp_credential())
    }
}

/// Canonical `Credential` key for the MCP session token. Kept out of
/// the public surface so call sites always go through the typed
/// helpers above.
fn mcp_credential() -> Credential {
    Credential::new("mergefox-mcp", "session-token")
}

/// Generate a 32-byte URL-safe token. Avoids bringing in a new
/// dependency by using the `rand_core` + `getrandom` stack that's
/// already in the graph for ssh-key generation.
fn generate_mcp_token() -> SecretString {
    use rand_core::{OsRng, RngCore};
    let mut bytes = [0u8; 32];
    OsRng.fill_bytes(&mut bytes);
    // URL-safe base64 without pulling in the `base64` crate — the
    // token is 32 bytes of entropy so a hex rendering is 64 chars,
    // perfectly fine for an env var value.
    let hex = bytes.iter().fold(String::with_capacity(64), |mut s, b| {
        use std::fmt::Write;
        let _ = write!(s, "{b:02x}");
        s
    });
    SecretString::new(hex)
}

// ---------------- file storage ----------------

/// On-disk shape. A header warning is baked into the JSON so anyone who
/// opens the file sees the risk before reading tokens.
#[derive(Debug, Default, Serialize, Deserialize)]
struct FileStoreDoc {
    #[serde(default, rename = "__warning__")]
    _warning: String,
    #[serde(default)]
    entries: Vec<FileEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FileEntry {
    service: String,
    account: String,
    value: String,
}

const FILE_WARNING: &str = "mergefox secrets — PLAINTEXT tokens. Anyone with read access \
    to this file can impersonate your Git / AI accounts. Protect it like \
    you would ~/.ssh/id_ed25519.";

fn load_doc(path: &Path) -> Result<FileStoreDoc> {
    match fs::read(path) {
        Ok(bytes) => {
            serde_json::from_slice(&bytes).with_context(|| format!("parse {}", path.display()))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(FileStoreDoc::default()),
        Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
    }
}

fn save_doc(path: &Path, doc: &FileStoreDoc) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).ok();
    }
    let json = serde_json::to_string_pretty(doc)?;
    // Atomic write: write to `.tmp`, chmod 0600, then rename so a
    // half-written secrets file never exists and readers always see a
    // fully-formed document.
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, json.as_bytes()).with_context(|| format!("write {}", tmp.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600));
    }
    fs::rename(&tmp, path)
        .with_context(|| format!("rename {} -> {}", tmp.display(), path.display()))?;
    Ok(())
}

fn read_file(path: &Path, cred: &Credential) -> Result<Option<SecretString>> {
    let doc = load_doc(path)?;
    Ok(doc
        .entries
        .into_iter()
        .find(|e| e.service == cred.service && e.account == cred.account)
        .map(|e| SecretString::new(e.value)))
}

fn write_file(path: &Path, cred: &Credential, value: &SecretString) -> Result<()> {
    let mut doc = load_doc(path)?;
    doc._warning = FILE_WARNING.to_string();
    let s = value.expose_secret().to_string();
    if s.is_empty() {
        doc.entries
            .retain(|e| !(e.service == cred.service && e.account == cred.account));
    } else if let Some(existing) = doc
        .entries
        .iter_mut()
        .find(|e| e.service == cred.service && e.account == cred.account)
    {
        existing.value = s;
    } else {
        doc.entries.push(FileEntry {
            service: cred.service.clone(),
            account: cred.account.clone(),
            value: s,
        });
    }
    save_doc(path, &doc)
}

fn delete_file(path: &Path, cred: &Credential) -> Result<()> {
    let mut doc = load_doc(path)?;
    let before = doc.entries.len();
    doc.entries
        .retain(|e| !(e.service == cred.service && e.account == cred.account));
    if doc.entries.len() != before {
        save_doc(path, &doc)?;
    }
    Ok(())
}

/// Default path for the secrets file, inside the app's config dir.
pub fn default_file_path() -> PathBuf {
    dirs::config_dir()
        .map(|d| d.join("mergefox").join("secrets.json"))
        .unwrap_or_else(|| PathBuf::from("secrets.json"))
}
