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
    // format: user@host or user@host/envname
    if let Some((host, env)) = stripped.split_once('/') {
        (Target::Remote(host.to_string()), Some(env.to_string()))
    } else {
        (Target::Remote(stripped.to_string()), None)
    }
}

/// Git remote helper for the `vitro://` transport.
///
/// Invoked by git as `git-remote-vitro <remote> <url>`.
/// Uses the `connect` capability to delegate to git-upload-pack
/// or git-receive-pack on the resolved env repo path.
pub fn run() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let url = args.get(2).map(|s| s.as_str()).unwrap_or("vitro://localhost");
    let (target, env_name) = parse_url(url);

    let env_path = match &target {
        Target::Local => {
            let repo = git::Repo::open()?;
            let path = repo.resolve_env_path()?;
            path.to_str()
                .context("env path is not valid UTF-8")?
                .to_string()
        }
        Target::Remote(_user_host) => {
            let name = env_name.as_deref()
                .ok_or_else(|| anyhow::anyhow!(
                    "remote vitro URL must include env name: vitro://<user>@<host>/<env>"
                ))?;
            format!("/var/lib/vitro/envs/{name}/repo")
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
                Target::Local => Command::new(service).arg(&env_path).exec(),
                Target::Remote(user_host) => Command::new("ssh")
                    .args([user_host.as_str(), service, &env_path])
                    .exec(),
            };
            anyhow::bail!("failed to exec {service}: {err}");
        } else {
            anyhow::bail!("unexpected command: {line}");
        }
    }

    Ok(())
}
