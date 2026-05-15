use anyhow::{Context, Result};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::config::CellConfig;

fn data_dir(repo_root: &Path) -> PathBuf {
    let base = std::env::var("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").expect("HOME not set");
            PathBuf::from(home).join(".local/share")
        });
    let mut hasher = DefaultHasher::new();
    repo_root.hash(&mut hasher);
    let hash = format!("{:016x}", hasher.finish());
    base.join("vitro").join(hash)
}

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

    pub fn from_root(root: PathBuf) -> Self {
        Self { root }
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

    pub fn branch_exists(&self, name: &str) -> bool {
        Command::new("git")
            .args([
                "show-ref",
                "--verify",
                "--quiet",
                &format!("refs/heads/{name}"),
            ])
            .current_dir(&self.root)
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    pub fn create_branch(&self, name: &str) -> Result<()> {
        let status = Command::new("git")
            .args(["branch", name])
            .current_dir(&self.root)
            .status()
            .context("creating branch")?;
        if !status.success() {
            anyhow::bail!("failed to create branch '{name}'");
        }
        Ok(())
    }

    pub fn delete_branch(&self, name: &str) -> Result<()> {
        let status = Command::new("git")
            .args(["branch", "-D", name])
            .current_dir(&self.root)
            .status()
            .context("deleting branch")?;
        if !status.success() {
            anyhow::bail!("failed to delete branch '{name}'");
        }
        Ok(())
    }

    // Worktree management

    pub fn worktree_add(&self, path: &Path, branch: &str) -> Result<()> {
        let status = Command::new("git")
            .args(["worktree", "add", path.to_str().unwrap(), branch])
            .current_dir(&self.root)
            .status()
            .context("adding worktree")?;
        if !status.success() {
            anyhow::bail!("failed to add worktree for branch '{branch}'");
        }
        Ok(())
    }

    pub fn worktree_remove(&self, path: &Path) -> Result<()> {
        let status = Command::new("git")
            .args(["worktree", "remove", "--force", path.to_str().unwrap()])
            .current_dir(&self.root)
            .status()
            .context("removing worktree")?;
        if !status.success() {
            anyhow::bail!("failed to remove worktree at '{}'", path.display());
        }
        Ok(())
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

    // Clone management (coexists with worktree management)

    fn clone_path(&self, name: &str) -> PathBuf {
        data_dir(&self.root).join(name)
    }

    pub fn init_clone(&self, name: &str, _config: &CellConfig) -> Result<PathBuf> {
        let clone = self.clone_path(name);
        if clone.exists() {
            return Ok(clone);
        }

        std::fs::create_dir_all(&clone)?;

        let root_str = self.root.to_string_lossy();
        let cmds: &[&[&str]] = &[
            &["init", clone.to_str().unwrap()],
            &[
                "-C",
                clone.to_str().unwrap(),
                "remote",
                "add",
                "origin",
                &root_str,
            ],
            &["-C", clone.to_str().unwrap(), "fetch", "origin", name],
            &[
                "-C",
                clone.to_str().unwrap(),
                "checkout",
                "-b",
                name,
                &format!("origin/{name}"),
            ],
            &[
                "-C",
                clone.to_str().unwrap(),
                "config",
                "receive.denyCurrentBranch",
                "updateInstead",
            ],
        ];

        for args in cmds {
            let status = Command::new("git").args(*args).status()?;
            if !status.success() {
                std::fs::remove_dir_all(&clone).ok();
                anyhow::bail!("failed to init clone for '{name}': git {:?}", args);
            }
        }

        Ok(clone)
    }

    pub fn list_clones(&self) -> Result<Vec<String>> {
        let dir = data_dir(&self.root);
        if !dir.exists() {
            return Ok(Vec::new());
        }
        let mut names = Vec::new();
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                let name = entry.file_name();
                if let Some(s) = name.to_str() {
                    if s != "meta" {
                        names.push(s.to_string());
                    }
                }
            }
        }
        names.sort();
        Ok(names)
    }

    /// Resolve the cell repo path for the git-remote-vitro helper.
    /// Checks current branch first, then scans all clones.
    pub fn resolve_cell_path(&self) -> Result<PathBuf> {
        // try current branch first
        let branch = self.current_branch()?;
        if crate::vm::is_running(&branch).unwrap_or(false) {
            return Ok(crate::vm::cell_repo_dir(&branch));
        }

        // scan all clones for a running cell
        for name in self.list_clones()? {
            if crate::vm::is_running(&name).unwrap_or(false) {
                return Ok(crate::vm::cell_repo_dir(&name));
            }
        }

        anyhow::bail!("no active cell — use 'vitro create' or 'vitro run' first")
    }
}

// Server-side clone management — clones live inside cell dirs

pub fn server_clone_path(name: &str) -> PathBuf {
    crate::vm::cell_repo_dir(name)
}

pub fn init_clone_server(name: &str, _config: &CellConfig) -> Result<PathBuf> {
    let clone = server_clone_path(name);
    if clone.join(".git").exists() {
        return Ok(clone);
    }
    // clean up any stale empty dir
    if clone.exists() {
        std::fs::remove_dir_all(&clone).ok();
    }

    // ensure cell dir exists
    let cell = crate::vm::cell_dir(name);
    std::fs::create_dir_all(&cell)?;
    std::fs::create_dir_all(&clone)?;

    let clone_str = clone.to_str().unwrap();
    let cmds: &[&[&str]] = &[
        &["init", clone_str],
        &["-C", clone_str, "checkout", "-b", name],
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

    // set ownership to cell user (uid 1000)
    Command::new("chown")
        .args(["-R", "1000:users", &cell.to_string_lossy()])
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
        "#!/bin/sh\nchown -R 1000:users \"{repo_dir}\"\n",
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

