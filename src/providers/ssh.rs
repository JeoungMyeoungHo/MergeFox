//! SSH key generation and on-disk placement.
//!
//! We default to Ed25519: smaller (68-byte public key vs 400+ for RSA-4096),
//! faster signing, uniformly supported by every provider we target. RSA is
//! kept available in the ssh-key crate features for legacy hosts, but we
//! don't expose it here to keep the happy path opinionated.
//!
//! Private key material is wrapped in `SecretString` (secrecy's zeroing
//! wrapper) so it doesn't linger in free'd heap memory after we hand it
//! to the OS.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rand_core::OsRng;
use secrecy::{ExposeSecret, SecretString};
use ssh_key::{Algorithm, LineEnding, PrivateKey};

/// A freshly minted key pair ready to be saved.
///
/// `private_pem` is wrapped in `SecretString` — secrecy's zeroing wrapper —
/// so the key material is scrubbed from memory the moment this struct is
/// dropped. (We reuse `SecretString` rather than pulling in `zeroize`
/// directly because it's already a project dependency.)
pub struct GeneratedKey {
    pub public_openssh: String,
    pub private_pem: SecretString,
}

/// Generate a new Ed25519 key pair. `comment` is embedded in the public
/// key's trailing comment — conventionally `user@host-YYYY-MM-DD` so the
/// owner can spot it in a provider's SSH keys list.
pub fn generate_ed25519(comment: &str) -> Result<GeneratedKey> {
    let mut key = PrivateKey::random(&mut OsRng, Algorithm::Ed25519)
        .context("ssh-key random ed25519 generation failed")?;
    key.set_comment(comment);

    let pem = key
        .to_openssh(LineEnding::LF)
        .context("encode ed25519 private key as openssh pem")?
        .to_string();

    let public = key
        .public_key()
        .to_openssh()
        .context("encode ed25519 public key as openssh")?;

    Ok(GeneratedKey {
        public_openssh: public,
        private_pem: SecretString::new(pem),
    })
}

/// Write the private key to `private_path` and its `.pub` sibling.
///
/// On Unix we explicitly chmod 0600 — ssh-agent/openssh will refuse to use
/// a key with looser permissions. On Windows the filesystem ACL model is
/// different and openssh-for-windows is less strict, so we just write the
/// file; hardening there is left to the user's own profile.
pub fn save_key_pair(generated: &GeneratedKey, private_path: &Path) -> Result<()> {
    if let Some(parent) = private_path.parent() {
        fs::create_dir_all(parent).context("create ssh key directory")?;
    }

    // Write private key first, with tight permissions baked in at open time
    // on Unix so there's never a moment when a world-readable file exists.
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(private_path)
            .with_context(|| format!("create {}", private_path.display()))?;
        f.write_all(generated.private_pem.expose_secret().as_bytes())?;
    }

    #[cfg(not(unix))]
    {
        let mut f = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(private_path)
            .with_context(|| format!("create {}", private_path.display()))?;
        f.write_all(generated.private_pem.expose_secret().as_bytes())?;
        // NOTE: Windows ACL tightening is intentionally not done here;
        // the user's %USERPROFILE%\.ssh already inherits a per-user ACL,
        // and a wrong SetSecurityInfo call would make things worse.
    }

    let pub_path = public_path_for(private_path);
    fs::write(&pub_path, generated.public_openssh.as_bytes())
        .with_context(|| format!("write {}", pub_path.display()))?;

    Ok(())
}

/// Appends `.pub` to the private key filename — this is the ssh convention
/// (`id_ed25519` / `id_ed25519.pub`) and tools expect the pair to be colocated.
fn public_path_for(private: &Path) -> PathBuf {
    let mut s = private.as_os_str().to_os_string();
    s.push(".pub");
    PathBuf::from(s)
}

/// List existing SSH private keys in `~/.ssh` by finding `*.pub` and
/// stripping the suffix. We don't try to parse every private key — that
/// would be slow and could prompt for passphrases — just return paths.
pub fn list_existing_keys() -> Vec<PathBuf> {
    let Some(home) = dirs::home_dir() else {
        return Vec::new();
    };
    let ssh_dir = home.join(".ssh");
    let Ok(entries) = fs::read_dir(&ssh_dir) else {
        return Vec::new();
    };

    let mut out = Vec::new();
    for entry in entries.flatten() {
        let p = entry.path();
        // Only .pub files — those uniquely identify a key pair. The
        // matching private key is whatever has the same stem.
        if p.extension().and_then(|s| s.to_str()) != Some("pub") {
            continue;
        }
        // Strip the .pub suffix to get the private-key path. `with_extension("")`
        // would wrongly handle filenames like `id_rsa-work.pub`, so do it by
        // raw os-string manipulation.
        let s = p.as_os_str().to_string_lossy();
        if let Some(stem) = s.strip_suffix(".pub") {
            let priv_path = PathBuf::from(stem);
            if priv_path.exists() {
                out.push(priv_path);
            }
        }
    }
    out
}
