// Per-cell metadata kept on the developer's laptop. Today: just the
// server a cell was created against, so `vitro run` can route there
// without an explicit --server every invocation.
//
// Lives at .vitro/state/<cell>/<key>; gitignored.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

const STATE_DIR: &str = ".vitro/state";

fn cell_dir(repo_root: &Path, cell: &str) -> PathBuf {
    repo_root.join(STATE_DIR).join(cell)
}

fn server_file(repo_root: &Path, cell: &str) -> PathBuf {
    cell_dir(repo_root, cell).join("server")
}

pub fn set_server(repo_root: &Path, cell: &str, server: &str) -> Result<()> {
    let dir = cell_dir(repo_root, cell);
    std::fs::create_dir_all(&dir).context("creating state dir")?;
    crate::git::ensure_gitignore_entry(repo_root, ".vitro/state/")?;
    std::fs::write(server_file(repo_root, cell), format!("{server}\n"))
        .context("writing cell server")?;
    Ok(())
}

pub fn get_server(repo_root: &Path, cell: &str) -> Option<String> {
    std::fs::read_to_string(server_file(repo_root, cell))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

pub fn clear(repo_root: &Path, cell: &str) -> Result<()> {
    let dir = cell_dir(repo_root, cell);
    if dir.exists() {
        std::fs::remove_dir_all(&dir).context("removing cell state dir")?;
    }
    Ok(())
}
