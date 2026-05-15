use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;
use tracing::warn;

use crate::config::CellConfig;

fn secrets_age_path(repo_root: &Path) -> PathBuf {
    repo_root.join(".vitro/secrets.age")
}

fn secrets_env_path(repo_root: &Path) -> PathBuf {
    repo_root.join(".vitro/secrets.env")
}

fn age_identity_paths() -> Vec<PathBuf> {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/root".to_string());
    let mut paths = vec![
        // canonical sops age key location (dotfiles convention)
        PathBuf::from(&home).join(".config/sops/age/keys.txt"),
        // standard age config
        PathBuf::from(&home).join(".config/age/keys.txt"),
        // legacy — age identities stored alongside SSH keys
        PathBuf::from(&home).join(".ssh/id_ed25519"),
        PathBuf::from(&home).join(".ssh/id_rsa"),
    ];
    // server-side key
    paths.push(PathBuf::from("/var/lib/vitro/ssh/id_ed25519"));

    // env-var overrides take precedence
    for var in &["SOPS_AGE_KEY_FILE", "AGE_IDENTITY_FILE"] {
        if let Ok(val) = std::env::var(var) {
            paths.insert(0, PathBuf::from(val));
        }
    }

    paths.retain(|p| p.exists());
    paths
}

fn decrypt(age_file: &Path) -> Result<String> {
    let keys = age_identity_paths();
    if keys.is_empty() {
        anyhow::bail!("no SSH keys found for decryption");
    }

    for key in &keys {
        let output = Command::new("age")
            .args(["-d", "-i"])
            .arg(key)
            .arg(age_file)
            .output()
            .context("failed to run age — is it installed?")?;
        if output.status.success() {
            return Ok(String::from_utf8(output.stdout)?);
        }
    }

    anyhow::bail!("age decryption failed — no key matched any recipient")
}

/// Decrypt repo secrets to an in-memory string. Returns None if neither
/// `secrets.command` is set nor `.vitro/secrets.age` exists. This runs
/// client-side; the resulting plaintext is meant to be pushed to the
/// host over an already-trusted channel (SSH) — the host never needs to
/// be a recipient of `secrets.age`.
pub fn decrypt_content(repo_root: &Path, config: &CellConfig) -> Result<Option<String>> {
    if let Some(ref cmd) = config.secrets.command {
        let output = Command::new("sh")
            .args(["-c", cmd])
            .current_dir(repo_root)
            .output()
            .context("secrets command failed")?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("secrets command failed: {stderr}");
        }
        return Ok(Some(String::from_utf8(output.stdout)?));
    }

    let age_file = secrets_age_path(repo_root);
    if age_file.exists() {
        return Ok(Some(decrypt(&age_file)?));
    }
    Ok(None)
}

/// Decrypt and write `/var/lib/vitro/secrets.env` locally (used by
/// LocalTransport — there the laptop and the host are the same machine).
pub fn resolve_local(repo_root: &Path, config: &CellConfig) -> Result<Option<PathBuf>> {
    match decrypt_content(repo_root, config)? {
        Some(content) => {
            let out = PathBuf::from("/var/lib/vitro/secrets.env");
            std::fs::write(&out, &content).context("writing secrets.env")?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&out, std::fs::Permissions::from_mode(0o600))?;
            }
            Ok(Some(out))
        }
        None => Ok(None),
    }
}

/// Encrypt .vitro/secrets.env → .vitro/secrets.age for all keys.
pub fn encrypt(repo_root: &Path, keys: &[String]) -> Result<()> {
    if keys.is_empty() {
        anyhow::bail!("no keys configured — add secrets.keys in .vitro/config.toml");
    }

    let env_file = secrets_env_path(repo_root);
    if !env_file.exists() {
        anyhow::bail!(".vitro/secrets.env not found — create it first");
    }

    let age_file = secrets_age_path(repo_root);
    let mut cmd = Command::new("age");
    for key in keys {
        cmd.args(["-r", key]);
    }
    cmd.arg("-o").arg(&age_file).arg(&env_file);

    let output = cmd.output().context("failed to run age — is it installed?")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("age encryption failed: {stderr}");
    }
    println!("encrypted → {}", age_file.display());
    Ok(())
}

/// Decrypt .vitro/secrets.age into .vitro/secrets.env (no editor).
pub fn decrypt_to_env(repo_root: &Path) -> Result<()> {
    let age_file = secrets_age_path(repo_root);
    if !age_file.exists() {
        anyhow::bail!(".vitro/secrets.age not found");
    }
    let env_file = secrets_env_path(repo_root);
    let content = decrypt(&age_file)?;
    std::fs::create_dir_all(repo_root.join(".vitro"))?;
    std::fs::write(&env_file, &content).context("writing secrets.env")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&env_file, std::fs::Permissions::from_mode(0o600))?;
    }
    println!("decrypted → {}", env_file.display());
    Ok(())
}

/// Decrypt .vitro/secrets.age, open $EDITOR, re-encrypt on save.
pub fn edit(repo_root: &Path, keys: &[String]) -> Result<()> {
    if keys.is_empty() {
        anyhow::bail!("no keys configured — add secrets.keys in .vitro/config.toml");
    }

    let age_file = secrets_age_path(repo_root);
    let env_file = secrets_env_path(repo_root);

    let content = if age_file.exists() {
        decrypt(&age_file)?
    } else {
        String::new()
    };

    std::fs::create_dir_all(repo_root.join(".vitro"))?;
    std::fs::write(&env_file, &content)?;

    let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vi".to_string());
    let status = Command::new(&editor)
        .arg(&env_file)
        .status()
        .context("failed to open editor")?;
    if !status.success() {
        anyhow::bail!("editor exited with {}", status);
    }

    encrypt(repo_root, keys)?;

    // The encrypted .age file is the source of truth; the plaintext only
    // exists as the editor scratch buffer. Failing to remove it leaves
    // secrets sitting in the worktree — surface it loudly so the user can
    // clean it up manually rather than swallowing the error.
    if let Err(e) = std::fs::remove_file(&env_file) {
        warn!("failed to remove plaintext secrets file {}: {e}", env_file.display());
    }
    Ok(())
}
