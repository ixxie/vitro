// Per-env metadata kept on the developer's laptop. Today: just the
// server an env was created against, so vitro commands can route there
// without an explicit --server every invocation.
//
// Lives at .vitro/state/<env>/<key>; gitignored.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

const STATE_DIR: &str = ".vitro/state";

fn env_dir(repo_root: &Path, env: &str) -> PathBuf {
    repo_root.join(STATE_DIR).join(env)
}

fn server_file(repo_root: &Path, env: &str) -> PathBuf {
    env_dir(repo_root, env).join("server")
}

pub fn set_server(repo_root: &Path, env: &str, server: &str) -> Result<()> {
    let dir = env_dir(repo_root, env);
    std::fs::create_dir_all(&dir).context("creating state dir")?;
    crate::git::ensure_gitignore_entry(repo_root, ".vitro/state/")?;
    std::fs::write(server_file(repo_root, env), format!("{server}\n"))
        .context("writing env server")?;
    Ok(())
}

pub fn get_server(repo_root: &Path, env: &str) -> Option<String> {
    std::fs::read_to_string(server_file(repo_root, env))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

pub fn clear(repo_root: &Path, env: &str) -> Result<()> {
    let dir = env_dir(repo_root, env);
    if dir.exists() {
        std::fs::remove_dir_all(&dir).context("removing env state dir")?;
    }
    Ok(())
}
