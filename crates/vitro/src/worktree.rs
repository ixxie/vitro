use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

use crate::git;

const TREES_DIR: &str = ".vitro/trees";

pub fn trees_dir(repo_root: &Path) -> PathBuf {
    repo_root.join(TREES_DIR)
}

pub fn tree_path(repo_root: &Path, branch: &str) -> PathBuf {
    trees_dir(repo_root).join(branch)
}

/// Detect if the current working directory is inside a vitro worktree.
/// Returns (repo_root, branch_name) if so.
pub fn detect_context() -> Result<Option<(PathBuf, String)>> {
    let cwd = std::env::current_dir().context("getting cwd")?;

    let cwd_str = cwd.to_string_lossy();
    if let Some(pos) = cwd_str.find("/.vitro/trees/") {
        let repo_root = PathBuf::from(&cwd_str[..pos]);
        let after = &cwd_str[pos + "/.vitro/trees/".len()..];
        let branch = after.split('/').next().unwrap_or("").to_string();
        if !branch.is_empty() {
            return Ok(Some((repo_root, branch)));
        }
    }

    Ok(None)
}

/// Resolve cell name: explicit arg > worktree context > error.
pub fn resolve_cell(explicit: Option<&str>) -> Result<(git::Repo, String)> {
    if let Some(name) = explicit {
        // try worktree context first for repo root
        if let Some((root, _)) = detect_context()? {
            let repo = git::Repo::from_root(root);
            return Ok((repo, name.to_string()));
        }
        let repo = git::Repo::open()?;
        return Ok((repo, name.to_string()));
    }

    if let Some((root, branch)) = detect_context()? {
        let repo = git::Repo::from_root(root);
        return Ok((repo, branch));
    }

    anyhow::bail!("not in a vitro worktree — specify a branch name or cd into .vitro/trees/<branch>")
}

pub fn add(repo: &git::Repo, branch: &str) -> Result<PathBuf> {
    let path = tree_path(repo.root(), branch);
    if path.exists() {
        anyhow::bail!("worktree already exists at {}", path.display());
    }

    git::ensure_gitignore_entry(repo.root(), ".vitro/trees/")?;

    let trees = trees_dir(repo.root());
    std::fs::create_dir_all(&trees)?;

    repo.worktree_add(&path, branch)?;

    if path.join(".envrc").exists() {
        std::process::Command::new("direnv")
            .args(["allow", path.to_str().unwrap()])
            .status()
            .ok();
    }

    Ok(path)
}

pub fn remove(repo: &git::Repo, branch: &str) -> Result<()> {
    let path = tree_path(repo.root(), branch);
    if !path.exists() {
        anyhow::bail!("no worktree for branch '{branch}'");
    }
    repo.worktree_remove(&path)?;
    Ok(())
}
