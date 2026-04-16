//! Personal Access Token storage.
//!
//! Post-keychain-removal these functions are thin shims that route
//! through the process-wide `SecretStore`. If the global store hasn't
//! been installed yet (which should only happen in tests — production
//! installs it in `MergeFoxApp::new`), load returns `None` and save/
//! delete return an error so misuse is visible rather than silently
//! dropping secrets on the floor.
//!
//! Tokens never touch `config.json`; they live in
//! `~/.../mergefox/secrets.json` (chmod 0600). The caller passes a
//! `SecretString` in and gets one back so the zero-on-drop guard
//! stays intact.

use anyhow::{anyhow, Result};
use secrecy::SecretString;

use super::types::{AccountId, ProviderKind};
use crate::secrets::{Credential, SecretStore};

fn credential(account: &AccountId) -> Credential {
    Credential::new(account.keyring_service(), account.keyring_user())
}

fn store() -> Result<&'static std::sync::Arc<SecretStore>> {
    SecretStore::global()
        .ok_or_else(|| anyhow!("secret store not yet initialised (programmer error)"))
}

/// Write-or-overwrite a PAT for this account.
pub fn store_pat(account: &AccountId, token: SecretString) -> Result<()> {
    store()?.save(&credential(account), token)
}

/// Look up a PAT. `Ok(None)` when no entry exists.
pub fn load_pat(account: &AccountId) -> Result<Option<SecretString>> {
    match SecretStore::global() {
        Some(s) => s.load(&credential(account)),
        // Pre-install is only hit during very early startup / tests.
        // Returning `None` lets those paths continue as "no credential
        // configured yet" instead of exploding.
        None => Ok(None),
    }
}

/// Remove a stored PAT. No-op if it wasn't there.
pub fn delete_pat(account: &AccountId) -> Result<()> {
    store()?.delete(&credential(account))
}

/// Where to send the user to mint a new PAT. We link to the scoped-token
/// UIs where they exist — GitHub's fine-grained PATs are preferred over
/// classic ones for least-privilege.
pub fn pat_help_url(kind: &ProviderKind) -> &'static str {
    match kind {
        ProviderKind::GitHub => "https://github.com/settings/tokens?type=beta",
        ProviderKind::GitLab => "https://gitlab.com/-/user_settings/personal_access_tokens",
        ProviderKind::Bitbucket => "https://bitbucket.org/account/settings/app-passwords/",
        ProviderKind::AzureDevOps => {
            "https://learn.microsoft.com/azure/devops/organizations/accounts/use-personal-access-tokens-to-authenticate"
        }
        ProviderKind::Codeberg => "https://codeberg.org/user/settings/applications",
        ProviderKind::Gitea { .. } => {
            "https://docs.gitea.com/usage/authentication#personal-access-tokens"
        }
        ProviderKind::Generic { .. } => "https://git-scm.com/docs/gitcredentials",
    }
}
