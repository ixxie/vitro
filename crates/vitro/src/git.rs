use anyhow::{Context, Result};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::config::EnvConfig;

pub struct Repo {
    root: PathBuf,
}

impl Repo {
    pub fn open() -> Result<Self> {
        let output = Command::new("git")
            .args(["rev-parse", "--show-toplevel"])
            .output()
            .context("running git")?;
        let root = String::from_utf8(output.stdout)
            .context("git output")?
            .trim()
            .to_string();
        Ok(Self {
            root: PathBuf::from(root),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn current_branch(&self) -> Result<String> {
        let output = Command::new("git")
            .args(["branch", "--show-current"])
            .current_dir(&self.root)
            .output()?;
        Ok(String::from_utf8(output.stdout)?.trim().to_string())
    }

    pub fn rev_parse(&self, refname: &str) -> Result<String> {
        let output = Command::new("git")
            .args(["rev-parse", "--short", refname])
            .current_dir(&self.root)
            .output()?;
        if !output.status.success() {
            anyhow::bail!("rev-parse failed for {refname}");
        }
        Ok(String::from_utf8(output.stdout)?.trim().to_string())
    }

    /// Returns (ahead, behind) — commits on `branch` not on `base`, and vice versa.
    /// Returns (0, 0) if the comparison fails (e.g. base missing).
    pub fn ahead_behind(&self, branch: &str, base: &str) -> Result<(u32, u32)> {
        let output = Command::new("git")
            .args(["rev-list", "--left-right", "--count", &format!("{base}...{branch}")])
            .current_dir(&self.root)
            .output()?;
        if !output.status.success() {
            return Ok((0, 0));
        }
        let s = String::from_utf8(output.stdout)?;
        let parts: Vec<&str> = s.split_whitespace().collect();
        if parts.len() != 2 {
            return Ok((0, 0));
        }
        let behind: u32 = parts[0].parse().unwrap_or(0);
        let ahead: u32 = parts[1].parse().unwrap_or(0);
        Ok((ahead, behind))
    }

    // Remote management

    pub fn add_vitro_remote(&self, url: &str) -> Result<()> {
        let output = Command::new("git")
            .args(["remote", "add", "vitro", url])
            .current_dir(&self.root)
            .output()
            .context("adding vitro remote")?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            if stderr.contains("already exists") {
                // update existing remote URL
                let status = Command::new("git")
                    .args(["remote", "set-url", "vitro", url])
                    .current_dir(&self.root)
                    .status()
                    .context("updating vitro remote URL")?;
                if !status.success() {
                    anyhow::bail!("failed to update vitro remote URL");
                }
                return Ok(());
            }
            anyhow::bail!("failed to add vitro remote: {stderr}");
        }
        Ok(())
    }

    pub fn push_to_vitro(&self) -> Result<()> {
        let status = Command::new("git")
            .args(["push", "--force", "vitro", "HEAD:main"])
            .current_dir(&self.root)
            .status()
            .context("git push vitro")?;
        if !status.success() {
            anyhow::bail!("git push to vitro remote failed");
        }
        Ok(())
    }

    /// Resolve the env repo path for the git-remote-vitro helper.
    /// Finds a running local env; falls back to the first env dir.
    pub fn resolve_env_path(&self) -> Result<PathBuf> {
        for name in crate::vm::list_envs().unwrap_or_default() {
            if crate::vm::is_running(&name).unwrap_or(false) {
                return Ok(crate::vm::env_repo_dir(&name));
            }
        }

        anyhow::bail!("no active env — use 'vitro create' first")
    }
}

// Server-side clone management — clones live inside env dirs

pub fn server_clone_path(name: &str) -> PathBuf {
    crate::vm::env_repo_dir(name)
}

pub fn init_clone_server(name: &str, _config: &EnvConfig) -> Result<PathBuf> {
    let clone = server_clone_path(name);
    if clone.join(".git").exists() {
        return Ok(clone);
    }
    // clean up any stale empty dir
    if clone.exists() {
        std::fs::remove_dir_all(&clone).ok();
    }

    // ensure env dir exists
    let env_dir = crate::vm::env_dir(name);
    std::fs::create_dir_all(&env_dir)?;
    std::fs::create_dir_all(&clone)?;

    let clone_str = clone.to_str().unwrap();
    let cmds: &[&[&str]] = &[
        &["init", "-b", "main", clone_str],
        &["-C", clone_str, "config", "receive.denyCurrentBranch", "updateInstead"],
    ];

    for args in cmds {
        let status = Command::new("git").args(*args).status()?;
        if !status.success() {
            std::fs::remove_dir_all(&clone).ok();
            anyhow::bail!("failed to init server clone for '{name}': git {:?}", args);
        }
    }

    install_chown_hook(&clone)?;

    // set ownership to env user (uid 1000)
    Command::new("chown")
        .args(["-R", "1000:users", &env_dir.to_string_lossy()])
        .status()
        .ok();

    Ok(clone)
}

fn install_chown_hook(clone: &Path) -> Result<()> {
    let hooks_dir = clone.join(".git/hooks");
    std::fs::create_dir_all(&hooks_dir)?;
    let hook_path = hooks_dir.join("post-receive");
    let repo_dir = clone.to_string_lossy();
    let hook = format!(
        "#!/bin/sh\ngit --work-tree=\"{repo_dir}\" --git-dir=\"{repo_dir}/.git\" checkout -f HEAD\nchown -R 1000:users \"{repo_dir}\"\n",
    );
    std::fs::write(&hook_path, hook)?;
    std::fs::set_permissions(&hook_path, std::fs::Permissions::from_mode(0o755))?;
    Ok(())
}

pub fn ensure_gitignore_entry(repo_root: &Path, entry: &str) -> Result<()> {
    let gitignore = repo_root.join(".gitignore");
    let content = std::fs::read_to_string(&gitignore).unwrap_or_default();
    if content.lines().any(|l| l.trim() == entry) {
        return Ok(());
    }
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&gitignore)?;
    if !content.is_empty() && !content.ends_with('\n') {
        writeln!(f)?;
    }
    writeln!(f, "{entry}")?;
    Ok(())
}
