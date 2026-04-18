//! Persistent app config: recent repos, window settings, per-repo prefs.
//!
//! Stored as JSON at `~/Library/Application Support/mergefox/config.json`
//! (macOS) / `~/.config/mergefox/config.json` (linux) /
//! `%APPDATA%\mergefox\config.json` (windows).
//!
//! Schema evolution: the `schema` field is written on every save so that
//! future versions can either migrate or refuse to load older/newer files
//! without guessing. `#[serde(default)]` on new fields keeps forward-compat
//! with users who skip versions.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Current on-disk schema version. Bump when making a breaking change to
/// the JSON shape; add a matching arm in `Config::load` to migrate.
pub const SCHEMA_VERSION: u32 = 2;

const MAX_RECENTS: usize = 20;

fn default_schema() -> u32 {
    SCHEMA_VERSION
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Config {
    /// Written by every save; read on load. Missing in files from 0.1.0-dev
    /// → defaults to current, treated as v1.
    #[serde(default = "default_schema")]
    pub schema: u32,
    #[serde(default)]
    pub recents: Vec<RecentRepo>,
    #[serde(default)]
    pub ui_language: UiLanguage,
    #[serde(default)]
    pub theme: ThemeSettings,
    #[serde(default)]
    pub settings_window: SettingsWindowState,
    #[serde(default)]
    pub repo_settings: BTreeMap<String, RepoSettings>,
    #[serde(default)]
    pub provider_accounts: Vec<crate::providers::ProviderAccount>,
    /// Configured AI endpoint. The API key is **not** persisted here —
    /// it lives in the OS keyring under `crate::ai::config::EndpointKeyringKey`.
    /// `None` = AI features disabled (buttons shown but grayed with a
    /// "Configure in Settings → AI" hint).
    #[serde(default)]
    pub ai_endpoint: Option<crate::ai::Endpoint>,
    /// Defaults that apply to clone flows (size prompt, shallow depth).
    #[serde(default)]
    pub clone_defaults: CloneDefaults,
    // NOTE: the `secrets_backend` field was removed along with the OS
    // keychain dependency. Old config files that still have it will be
    // silently ignored by serde. Going forward the secret store is
    // always the internal file (`~/.../mergefox/secrets.json`).
}

impl Default for Config {
    fn default() -> Self {
        Self {
            schema: SCHEMA_VERSION,
            recents: Vec::new(),
            ui_language: UiLanguage::default(),
            theme: ThemeSettings::default(),
            settings_window: SettingsWindowState::default(),
            repo_settings: BTreeMap::new(),
            provider_accounts: Vec::new(),
            ai_endpoint: None,
            clone_defaults: CloneDefaults::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecentRepo {
    pub path: PathBuf,
    /// Unix seconds — we use `u64` rather than `SystemTime` so JSON is readable.
    pub last_opened: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UiLanguage {
    #[default]
    System,
    English,
    Korean,
    Japanese,
    Chinese,
    French,
    Spanish,
}

impl UiLanguage {
    pub fn label(self) -> &'static str {
        match self {
            Self::System => "System",
            Self::English => "English",
            Self::Korean => "한국어",
            Self::Japanese => "日本語",
            Self::Chinese => "中文",
            Self::French => "Français",
            Self::Spanish => "Español",
        }
    }

    /// Resolve `System` to a concrete language. Uses the OS locale API via
    /// `sys-locale` so it works in macOS `.app` bundles (where `$LANG` is
    /// empty) as well as terminal launches on Linux/Windows.
    pub fn resolved(self) -> Self {
        match self {
            Self::System => {
                let locale = sys_locale::get_locale()
                    .unwrap_or_default()
                    .to_ascii_lowercase();
                if locale.starts_with("ko") {
                    Self::Korean
                } else if locale.starts_with("ja") {
                    Self::Japanese
                } else if locale.starts_with("zh") {
                    Self::Chinese
                } else if locale.starts_with("fr") {
                    Self::French
                } else if locale.starts_with("es") {
                    Self::Spanish
                } else {
                    Self::English
                }
            }
            other => other,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThemePreset {
    #[default]
    MergeFox,
    Light,
    Dark,
    Custom,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThemeColor {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl ThemeColor {
    pub const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }

    pub fn to_color32(self) -> egui::Color32 {
        egui::Color32::from_rgb(self.r, self.g, self.b)
    }

    pub fn from_color32(color: egui::Color32) -> Self {
        Self {
            r: color.r(),
            g: color.g(),
            b: color.b(),
        }
    }

    pub fn hex(self) -> String {
        format!("#{:02X}{:02X}{:02X}", self.r, self.g, self.b)
    }

    pub fn luminance(self) -> f32 {
        let to_linear = |v: u8| {
            let s = v as f32 / 255.0;
            if s <= 0.04045 {
                s / 12.92
            } else {
                ((s + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * to_linear(self.r) + 0.7152 * to_linear(self.g) + 0.0722 * to_linear(self.b)
    }
}

fn default_translucent_panels() -> bool {
    true
}

const fn default_contrast() -> u8 {
    58
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThemePalette {
    pub accent: ThemeColor,
    pub background: ThemeColor,
    pub foreground: ThemeColor,
    #[serde(default = "default_translucent_panels")]
    pub translucent_panels: bool,
    #[serde(default = "default_contrast")]
    pub contrast: u8,
}

impl Default for ThemePalette {
    fn default() -> Self {
        Self::mergefox()
    }
}

impl ThemePalette {
    pub fn mergefox() -> Self {
        Self {
            accent: ThemeColor::rgb(255, 139, 61),
            background: ThemeColor::rgb(22, 23, 28),
            foreground: ThemeColor::rgb(241, 236, 229),
            translucent_panels: true,
            contrast: 62,
        }
    }

    pub fn light() -> Self {
        Self {
            accent: ThemeColor::rgb(51, 156, 255),
            background: ThemeColor::rgb(250, 248, 244),
            foreground: ThemeColor::rgb(24, 26, 31),
            translucent_panels: false,
            contrast: 38,
        }
    }

    pub fn dark() -> Self {
        Self {
            accent: ThemeColor::rgb(103, 132, 255),
            background: ThemeColor::rgb(24, 24, 24),
            foreground: ThemeColor::rgb(255, 255, 255),
            translucent_panels: true,
            contrast: 60,
        }
    }

    pub fn is_dark(&self) -> bool {
        self.background.luminance() < 0.35
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThemeSettings {
    #[serde(default)]
    pub preset: ThemePreset,
    #[serde(default)]
    pub custom_palette: ThemePalette,
}

impl Default for ThemeSettings {
    fn default() -> Self {
        Self {
            preset: ThemePreset::MergeFox,
            custom_palette: ThemePalette::mergefox(),
        }
    }
}

impl ThemeSettings {
    pub fn active_palette(&self) -> ThemePalette {
        match self.preset {
            ThemePreset::MergeFox => ThemePalette::mergefox(),
            ThemePreset::Light => ThemePalette::light(),
            ThemePreset::Dark => ThemePalette::dark(),
            ThemePreset::Custom => self.custom_palette.clone(),
        }
    }

    pub fn set_custom_from(&mut self, preset: ThemePreset) {
        self.custom_palette = match preset {
            ThemePreset::MergeFox => ThemePalette::mergefox(),
            ThemePreset::Light => ThemePalette::light(),
            ThemePreset::Dark => ThemePalette::dark(),
            ThemePreset::Custom => self.custom_palette.clone(),
        };
        self.preset = ThemePreset::Custom;
    }
}

fn default_settings_window_width() -> f32 {
    860.0
}

fn default_settings_window_height() -> f32 {
    460.0
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SettingsWindowState {
    #[serde(default = "default_settings_window_width")]
    pub width: f32,
    #[serde(default = "default_settings_window_height")]
    pub height: f32,
}

impl Default for SettingsWindowState {
    fn default() -> Self {
        Self {
            width: default_settings_window_width(),
            height: default_settings_window_height(),
        }
    }
}

/// What to do when the user clones a repository whose size we can learn
/// from the provider (GitHub / GitLab) ahead of time.
///
/// The three modes exist because users have legitimately different
/// preferences:
///   * `Prompt` (default) — ask for big repos, take the quick path for
///     everything else.
///   * `AlwaysFull` — power users who want the whole history every time.
///   * `AlwaysShallow` — bandwidth-constrained users who rarely need more
///     than the recent tip.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CloneSizePolicy {
    #[default]
    Prompt,
    AlwaysFull,
    AlwaysShallow,
}

impl CloneSizePolicy {
    pub fn label(self) -> &'static str {
        match self {
            Self::Prompt => "Ask when large",
            Self::AlwaysFull => "Always full",
            Self::AlwaysShallow => "Always shallow",
        }
    }
}

/// Tunable clone behaviour. Single struct so we can evolve the surface
/// without breaking the config schema each time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CloneDefaults {
    #[serde(default)]
    pub size_policy: CloneSizePolicy,
    /// Prompt threshold in megabytes when `size_policy == Prompt`. Ignored
    /// otherwise. Default 500 MB — chosen empirically: the Linux kernel is
    /// ~4 GB (always triggers), a typical product monorepo is ~50–200 MB
    /// (never triggers), so the range between 300 and 1000 MB catches the
    /// right "this is going to sting" population.
    #[serde(default = "default_prompt_threshold_mb")]
    pub prompt_threshold_mb: u32,
    /// Commit depth used for shallow clones (both via the prompt and when
    /// `AlwaysShallow` is set). 100 fits most review workflows while still
    /// shrinking Linux-kernel-scale clones 10× over full history.
    #[serde(default = "default_shallow_depth")]
    pub shallow_depth: u32,
}

impl Default for CloneDefaults {
    fn default() -> Self {
        Self {
            size_policy: CloneSizePolicy::Prompt,
            prompt_threshold_mb: default_prompt_threshold_mb(),
            shallow_depth: default_shallow_depth(),
        }
    }
}

fn default_prompt_threshold_mb() -> u32 {
    500
}

fn default_shallow_depth() -> u32 {
    100
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PullStrategyPref {
    #[default]
    Merge,
    Rebase,
    FastForwardOnly,
}

impl PullStrategyPref {
    pub fn label(self) -> &'static str {
        match self {
            Self::Merge => "Merge",
            Self::Rebase => "Rebase",
            Self::FastForwardOnly => "Fast-forward only",
        }
    }

    pub fn to_git(self) -> crate::git::PullStrategy {
        match self {
            Self::Merge => crate::git::PullStrategy::Merge,
            Self::Rebase => crate::git::PullStrategy::Rebase,
            Self::FastForwardOnly => crate::git::PullStrategy::FastForwardOnly,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoSettings {
    #[serde(default)]
    pub default_remote: Option<String>,
    #[serde(default)]
    pub pull_strategy: PullStrategyPref,
    /// Which connected provider account to use for push / pull / fetch
    /// on this repo. `None` = auto-detect from remote URL host (first
    /// matching account). `Some(slug)` = use this specific account,
    /// where `slug` is `AccountId::slug()` (e.g. `github::alice`).
    ///
    /// This lets users with multiple GitHub accounts (personal + work)
    /// pick which one pushes to which repo.
    #[serde(default)]
    pub provider_account: Option<String>,
}

impl Config {
    pub fn load() -> Self {
        let Some(path) = config_path() else {
            return Self::default();
        };
        let cfg: Config = match load_from(&path) {
            Ok(c) => c,
            Err(_) => return Self::default(),
        };
        cfg
    }

    pub fn save(&self) -> Result<()> {
        let path = config_path().context("no config dir available")?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).ok();
        }
        // Persist with the current schema — deserializing an older file and
        // saving it upgrades the `schema` tag.
        let to_write = SerConfig {
            schema: SCHEMA_VERSION,
            recents: &self.recents,
            ui_language: self.ui_language,
            theme: &self.theme,
            settings_window: &self.settings_window,
            repo_settings: &self.repo_settings,
            provider_accounts: &self.provider_accounts,
            ai_endpoint: self.ai_endpoint.as_ref(),
            clone_defaults: &self.clone_defaults,
        };
        write_config(&path, &to_write)
    }

    pub fn touch_recent(&mut self, path: PathBuf) {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        // Canonicalize for dedup so `~/proj`, `~/proj/`, and
        // `~/./proj` don't produce three recents entries.
        let key = canonical(&path);
        self.recents.retain(|r| canonical(&r.path) != key);
        self.recents.insert(
            0,
            RecentRepo {
                path,
                last_opened: now,
            },
        );
        self.recents.truncate(MAX_RECENTS);
    }

    /// Drop entries whose path no longer exists on disk.
    pub fn prune_recents(&mut self) {
        self.recents.retain(|r| r.path.exists());
    }

    pub fn repo_settings_for(&self, path: &Path) -> RepoSettings {
        self.repo_settings
            .get(&repo_key(path))
            .cloned()
            .unwrap_or_default()
    }

    pub fn set_repo_settings(&mut self, path: &Path, settings: RepoSettings) {
        self.repo_settings.insert(repo_key(path), settings);
    }

    pub fn upsert_provider_account(&mut self, account: crate::providers::ProviderAccount) {
        let mut account = account;
        if let Some(existing) = self
            .provider_accounts
            .iter_mut()
            .find(|existing| existing.id == account.id)
        {
            if account.ssh_key_path.is_none() {
                account.ssh_key_path = existing.ssh_key_path.clone();
            }
            *existing = account;
        } else {
            self.provider_accounts.push(account);
        }
        self.provider_accounts.sort_by(|a, b| {
            a.id.kind
                .slug()
                .cmp(&b.id.kind.slug())
                .then_with(|| a.id.username.cmp(&b.id.username))
        });
    }

    pub fn remove_provider_account(&mut self, id: &crate::providers::AccountId) {
        self.provider_accounts.retain(|account| &account.id != id);
    }
}

/// Serialization helper — the public `Config` uses `#[serde(default)]` on
/// `schema` for forward-compat reads, but when writing we want the field
/// to be present unconditionally.
#[derive(Serialize)]
struct SerConfig<'a> {
    schema: u32,
    recents: &'a [RecentRepo],
    ui_language: UiLanguage,
    theme: &'a ThemeSettings,
    settings_window: &'a SettingsWindowState,
    repo_settings: &'a BTreeMap<String, RepoSettings>,
    provider_accounts: &'a [crate::providers::ProviderAccount],
    #[serde(skip_serializing_if = "Option::is_none")]
    ai_endpoint: Option<&'a crate::ai::Endpoint>,
    clone_defaults: &'a CloneDefaults,
}

pub fn config_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("mergefox").join("config.json"))
}

/// Diagnostic entry point: try to load the user's config and report
/// what (if anything) went wrong. `Config::load` swallows errors by
/// design, so this is the only way to see a parse failure short of
/// adding tracing throughout.
#[doc(hidden)]
pub fn diagnose_load() -> String {
    let Some(path) = config_path() else {
        return "no config dir available".into();
    };
    match std::fs::read(&path) {
        Ok(bytes) => match load_from_bytes(&path, &bytes) {
            Ok(cfg) => format!(
                "ok: {} recents, {} repo_settings, schema {}",
                cfg.recents.len(),
                cfg.repo_settings.len(),
                cfg.schema
            ),
            Err(e) => format!("parse error on {}: {e:#}", path.display()),
        },
        Err(e) => format!("read error on {}: {e}", path.display()),
    }
}

fn load_from(path: &Path) -> Result<Config> {
    let bytes = fs::read(path).with_context(|| format!("read {}", path.display()))?;
    load_from_bytes(path, &bytes)
}

/// Canonical path string used as a map key — resolves symlinks and removes
/// `.` / trailing slashes so `~/proj` and `~/proj/` share config.
/// Falls back to the original path when canonicalization fails (e.g. the
/// directory was moved after config was written).
fn canonical(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

fn repo_key(path: &Path) -> String {
    canonical(path).to_string_lossy().into_owned()
}

fn load_from_bytes(path: &Path, bytes: &[u8]) -> Result<Config> {
    let mut cfg: Config = serde_json::from_slice(bytes)?;
    if cfg.schema > SCHEMA_VERSION {
        return Ok(cfg);
    }
    if cfg.schema == SCHEMA_VERSION {
        return Ok(cfg);
    }

    let from_schema = cfg.schema;
    let backup_path = backup_original(path, from_schema, bytes)?;
    cfg = migrate_config(cfg);
    if let Err(err) = save_config_to_path(path, &cfg) {
        rollback_backup(path, bytes)?;
        anyhow::bail!(
            "migrate config {} -> {} failed (backup at {}): {err:#}",
            from_schema,
            SCHEMA_VERSION,
            backup_path.display()
        );
    }
    Ok(cfg)
}

fn migrate_config(mut cfg: Config) -> Config {
    cfg.schema = SCHEMA_VERSION;
    if !(cfg.settings_window.width.is_finite() && cfg.settings_window.width >= 320.0) {
        cfg.settings_window.width = default_settings_window_width();
    }
    if !(cfg.settings_window.height.is_finite() && cfg.settings_window.height >= 240.0) {
        cfg.settings_window.height = default_settings_window_height();
    }
    cfg
}

fn backup_original(path: &Path, from_schema: u32, bytes: &[u8]) -> Result<PathBuf> {
    let parent = path
        .parent()
        .context("config path has no parent directory for backup")?;
    fs::create_dir_all(parent).with_context(|| format!("mkdir {}", parent.display()))?;
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let backup = parent.join(format!("config.backup-v{from_schema}-{stamp}.json"));
    fs::write(&backup, bytes).with_context(|| format!("write {}", backup.display()))?;
    Ok(backup)
}

fn rollback_backup(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("mkdir {}", parent.display()))?;
    }
    fs::write(path, bytes).with_context(|| format!("restore {}", path.display()))?;
    Ok(())
}

fn save_config_to_path(path: &Path, cfg: &Config) -> Result<()> {
    let to_write = SerConfig {
        schema: SCHEMA_VERSION,
        recents: &cfg.recents,
        ui_language: cfg.ui_language,
        theme: &cfg.theme,
        settings_window: &cfg.settings_window,
        repo_settings: &cfg.repo_settings,
        provider_accounts: &cfg.provider_accounts,
        ai_endpoint: cfg.ai_endpoint.as_ref(),
        clone_defaults: &cfg.clone_defaults,
    };
    write_config(path, &to_write)
}

fn write_config(path: &Path, value: &impl Serialize) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("mkdir {}", parent.display()))?;
    }
    let json = serde_json::to_vec_pretty(value)?;
    let tmp = path.with_extension(format!("json.tmp-{}", std::process::id()));
    fs::write(&tmp, json).with_context(|| format!("write {}", tmp.display()))?;
    if let Err(err) = fs::rename(&tmp, path) {
        fs::write(path, fs::read(&tmp)?).with_context(|| format!("write {}", path.display()))?;
        let _ = fs::remove_file(&tmp);
        tracing::debug!(error = %err, path = %path.display(), "config rename fallback used");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{load_from_bytes, SCHEMA_VERSION};
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_config_dir(name: &str) -> PathBuf {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("mergefox-config-test-{name}-{stamp}"))
    }

    #[test]
    fn older_schema_is_migrated_and_backed_up() {
        let dir = temp_config_dir("migrate");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        let bytes = br#"{
            "schema": 1,
            "ui_language": "english",
            "theme": { "preset": "merge_fox", "custom_palette": {
                "accent": {"r":255,"g":139,"b":61},
                "background": {"r":22,"g":23,"b":28},
                "foreground": {"r":241,"g":236,"b":229},
                "translucent_panels": true,
                "contrast": 62
            }},
            "repo_settings": {},
            "provider_accounts": [],
            "clone_defaults": {
                "size_policy": "prompt",
                "prompt_threshold_mb": 500,
                "shallow_depth": 100
            }
        }"#;

        let cfg = load_from_bytes(&path, bytes).unwrap();
        assert_eq!(cfg.schema, SCHEMA_VERSION);
        let backups = fs::read_dir(&dir)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with("config.backup-v1-"))
            .count();
        assert_eq!(backups, 1);
        let _ = fs::remove_dir_all(&dir);
    }
}
