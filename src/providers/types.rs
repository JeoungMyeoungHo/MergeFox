//! Core types shared across all provider implementations.
//!
//! Keeping these in a leaf module (no HTTP, no keyring) means unit tests
//! and serde round-trips don't drag in the async runtime or OS secret store.
//! Anything secret lives in `SecretString`; anything that ends up in
//! `config.json` MUST NOT contain credentials — only identity/metadata.

use std::fmt;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Which remote provider we're talking to.
///
/// Gitea/Generic carry an instance URL because self-hosted deployments have
/// no single well-known API base. GitHub/GitLab.com/Bitbucket/etc. are
/// public SaaS so their host is implicit.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ProviderKind {
    GitHub,
    GitLab,
    Bitbucket,
    AzureDevOps,
    Codeberg,
    /// Self-hosted Gitea. `instance` is the scheme+host root,
    /// e.g. `https://git.example.com`.
    Gitea {
        instance: String,
    },
    /// Any other HTTP git host. We can still SSH-clone and use a PAT,
    /// but REST discovery is best-effort.
    Generic {
        host: String,
    },
}

impl ProviderKind {
    /// Short slug for keyring service names, telemetry, etc. — stable across
    /// versions; do NOT change without a config migration.
    pub fn slug(&self) -> String {
        match self {
            ProviderKind::GitHub => "github".into(),
            ProviderKind::GitLab => "gitlab".into(),
            ProviderKind::Bitbucket => "bitbucket".into(),
            ProviderKind::AzureDevOps => "azure-devops".into(),
            ProviderKind::Codeberg => "codeberg".into(),
            ProviderKind::Gitea { instance } => format!("gitea::{instance}"),
            ProviderKind::Generic { host } => format!("generic::{host}"),
        }
    }
}

impl fmt::Display for ProviderKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.slug())
    }
}

/// Metadata about a remote repo — what we show in the UI before clone.
///
/// `clone_https` / `clone_ssh` are *synthesized* (not parsed from an API
/// response) because several providers (Azure DevOps) give back URLs
/// that differ subtly from the actual clone URLs we want.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoMeta {
    pub owner: String,
    pub repo: String,
    pub default_branch: String,
    pub description: Option<String>,
    pub private: bool,
    pub clone_https: String,
    pub clone_ssh: String,
}

/// A repository visible to the authenticated account.
///
/// Used by launcher/home UI to browse connected-host repositories and clone
/// them without first pasting a URL.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteRepoSummary {
    pub owner: String,
    pub repo: String,
    pub description: Option<String>,
    pub default_branch: Option<String>,
    pub private: bool,
    pub clone_https: String,
    pub clone_ssh: String,
    pub web_url: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteRepoOwnerKind {
    User,
    Organization,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteRepoOwner {
    pub login: String,
    pub display_name: String,
    pub kind: RemoteRepoOwnerKind,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateRepositoryDraft {
    pub owner: String,
    pub owner_kind: RemoteRepoOwnerKind,
    pub name: String,
    pub description: Option<String>,
    pub private: bool,
    pub auto_init: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreatedRepositoryRef {
    pub owner: String,
    pub repo: String,
    pub description: Option<String>,
    pub default_branch: Option<String>,
    pub private: bool,
    pub clone_https: String,
    pub clone_ssh: String,
    pub web_url: String,
}

/// How the user authenticates with this account.
///
/// A single provider account can exist multiple times with different methods —
/// e.g. OAuth for REST calls plus SSH for git transport.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthMethod {
    OAuth,
    Pat,
    Ssh,
}

/// Stable identity used as the keyring key for a credential.
///
/// Combined service+user means rotating a PAT for the same user on the same
/// provider cleanly overwrites — no stale duplicates in the keychain.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AccountId {
    pub kind: ProviderKind,
    pub username: String,
}

impl AccountId {
    /// Stable human-readable slug for config persistence and UI display.
    /// Format: `<provider>::<username>`, e.g. `github::alice`.
    pub fn slug(&self) -> String {
        format!("{}::{}", self.kind.slug(), self.username)
    }

    /// Service name for the OS keyring entry — prefixed with our app name so
    /// it's easy to spot in Keychain.app / Credential Manager / Secret Service.
    pub fn keyring_service(&self) -> String {
        format!("mergefox::{}", self.kind.slug())
    }

    /// Username component for the keyring entry. We append the kind again
    /// so entries like `alice@github` vs `alice@gitlab` never collide even
    /// if some platform de-dupes by user alone.
    pub fn keyring_user(&self) -> String {
        self.username.clone()
    }
}

/// Account metadata persisted to on-disk config.
///
/// Intentionally excludes any token material — secrets live only in the
/// OS keyring and are looked up via `AccountId`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderAccount {
    pub id: AccountId,
    pub display_name: String,
    pub avatar_url: Option<String>,
    pub method: AuthMethod,
    pub created_unix: i64,
    /// Optional local private-key path MergeFox should use when git
    /// network operations for this account go over SSH.
    #[serde(default)]
    pub ssh_key_path: Option<PathBuf>,
}

/// Minimal current-user profile fetched after a successful OAuth/PAT auth.
///
/// This lets the settings UI show a stable account card without storing any
/// token material in `config.json`.
#[derive(Debug, Clone)]
pub struct ProviderProfile {
    pub username: String,
    pub display_name: String,
    pub avatar_url: Option<String>,
}

/// OAuth 2.0 device-authorization-grant endpoints (RFC 8628).
///
/// `client_id` is a public client ID — the device flow is designed for
/// confidential clients that can't hold a secret, so no client_secret here.
#[derive(Debug, Clone)]
pub struct OAuthDeviceConfig {
    pub device_auth_url: String,
    pub token_url: String,
    pub client_id: String,
    pub scope: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrState {
    Open,
    Closed,
    All,
}

impl PrState {
    pub fn as_api_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Closed => "closed",
            Self::All => "all",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IssueState {
    Open,
    Closed,
    All,
}

impl IssueState {
    pub fn as_api_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Closed => "closed",
            Self::All => "all",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PullRequestDraft {
    pub owner: String,
    pub repo: String,
    pub title: String,
    pub body: String,
    pub head: String,
    pub base: String,
    pub draft: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PullRequestRef {
    pub number: u64,
    pub title: String,
    pub url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PullRequestSummary {
    pub number: u64,
    pub title: String,
    pub body: Option<String>,
    pub author: String,
    pub url: String,
    pub is_draft: bool,
    pub state: PrState,
    pub head_ref: String,
    pub base_ref: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IssueDraft {
    pub owner: String,
    pub repo: String,
    pub title: String,
    pub body: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IssueRef {
    pub number: u64,
    pub title: String,
    pub url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IssueSummary {
    pub number: u64,
    pub title: String,
    pub body: Option<String>,
    pub author: String,
    pub url: String,
    pub state: IssueState,
    pub labels: Vec<String>,
    pub assignees: Vec<String>,
}
