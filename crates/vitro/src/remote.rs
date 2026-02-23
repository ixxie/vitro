use anyhow::{Context, Result};
use std::io::BufRead;
use std::os::unix::process::CommandExt;
use std::process::Command;

use crate::git;

pub enum Target {
    Local,
    Remote(String), // user@host
}

pub fn parse_url(url: &str) -> (Target, Option<String>) {
    let stripped = url.strip_prefix("vitro://").unwrap_or(url);
    if stripped == "localhost" || stripped == "local" || stripped.is_empty() {
        return (Target::Local, None);
    }
    // format: user@host or user@host/cellname
    if let Some((host, cell)) = stripped.split_once('/') {
        (Target::Remote(host.to_string()), Some(cell.to_string()))
    } else {
        (Target::Remote(stripped.to_string()), None)
    }
}

/// Git remote helper for the `vitro://` transport.
///
/// Invoked by git as `git-remote-vitro <remote> <url>`.
/// Uses the `connect` capability to delegate to git-upload-pack
/// or git-receive-pack on the resolved cell repo path.
pub fn run() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let url = args.get(2).map(|s| s.as_str()).unwrap_or("vitro://localhost");
    let (target, cell_name) = parse_url(url);

    let cell_path = match &target {
        Target::Local => {
            let repo = git::Repo::open()?;
            let path = repo.resolve_cell_path()?;
            path.to_str()
                .context("cell path is not valid UTF-8")?
                .to_string()
        }
        Target::Remote(user_host) => {
            if let Some(name) = &cell_name {
                // cell name in URL — resolve directly
                format!("/var/lib/vitro/cells/{name}/repo")
            } else {
                // fall back to repo-name-based resolution
                let repo_name = git::Repo::open()
                    .ok()
                    .and_then(|r| r.root().file_name().map(|n| n.to_string_lossy().to_string()))
                    .unwrap_or_default();
                let mut ssh_args = vec![user_host.as_str(), "vitro", "util", "resolve-cell"];
                if !repo_name.is_empty() {
                    ssh_args.push("--repo");
                    ssh_args.push(&repo_name);
                }
                let output = Command::new("ssh")
                    .args(&ssh_args)
                    .output()
                    .context("failed to SSH for cell resolution")?;
                if !output.status.success() {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    anyhow::bail!("remote cell resolution failed: {stderr}");
                }
                String::from_utf8(output.stdout)?.trim().to_string()
            }
        }
    };

    let stdin = std::io::stdin();
    let mut lines = stdin.lock().lines();

    // protocol: read commands from git
    while let Some(Ok(line)) = lines.next() {
        let line = line.trim().to_string();
        if line.is_empty() {
            continue;
        }

        if line == "capabilities" {
            // respond with supported capabilities
            println!("connect");
            println!();
            continue;
        }

        if let Some(service) = line.strip_prefix("connect ") {
            // respond with empty line to indicate ready
            println!();

            // exec the service — locally or via SSH
            let err = match &target {
                Target::Local => Command::new(service).arg(&cell_path).exec(),
                Target::Remote(user_host) => Command::new("ssh")
                    .args([user_host.as_str(), service, &cell_path])
                    .exec(),
            };
            anyhow::bail!("failed to exec {service}: {err}");
        } else {
            anyhow::bail!("unexpected command: {line}");
        }
    }

    Ok(())
}
