use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

enum Runner {
    Nix,
    Docker,
    Podman,
}

fn has_cmd(name: &str) -> bool {
    Command::new("which")
        .arg(name)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn find_runner() -> Option<Runner> {
    if has_cmd("nix") { return Some(Runner::Nix); }
    if has_cmd("docker") { return Some(Runner::Docker); }
    if has_cmd("podman") { return Some(Runner::Podman); }
    None
}

fn is_nixos(target: &str) -> Result<bool> {
    let output = Command::new("ssh")
        .args([
            "-o", "StrictHostKeyChecking=accept-new",
            "-o", "ConnectTimeout=10",
            target,
            "test -e /etc/NIXOS && test \"$(stat -f -c %T / 2>/dev/null)\" != tmpfs",
        ])
        .output()
        .context("failed to probe remote host")?;
    Ok(output.status.success())
}

fn vitro_src_from_lock(host_dir: &Path) -> Option<PathBuf> {
    let lock = std::fs::read_to_string(host_dir.join("flake.lock")).ok()?;
    let json: serde_json::Value = serde_json::from_str(&lock).ok()?;
    let path = json["nodes"]["vitro"]["locked"]["path"].as_str()?;
    Some(PathBuf::from(path))
}

fn deploy_flake_src(target: &str, host_dir: &Path) -> Result<()> {
    let src = match vitro_src_from_lock(host_dir) {
        Some(p) => p,
        None => return Ok(()),
    };
    println!("deploying vitro flake-src...");
    let status = Command::new("rsync")
        .args([
            "-a", "--delete",
            "--exclude=target/", "--exclude=.git/",
            &format!("{}/", src.display()),
            &format!("{target}:/var/lib/vitro/flake-src/"),
        ])
        .status()
        .context("failed to rsync vitro flake-src")?;
    if !status.success() {
        anyhow::bail!("rsync vitro flake-src failed");
    }
    Ok(())
}

fn deploy_vm_config(target: &str, host_dir: &Path) -> Result<()> {
    let vm_dir = host_dir.join("vm");
    if !vm_dir.exists() {
        return Ok(());
    }
    println!("deploying vm-config...");
    Command::new("ssh")
        .args([target, "rm", "-rf", "/var/lib/vitro/vm-config"])
        .status().ok();
    let status = Command::new("scp")
        .args(["-r", &vm_dir.to_string_lossy(), &format!("{target}:/var/lib/vitro/vm-config")])
        .status()
        .context("failed to copy vm-config to remote")?;
    if !status.success() {
        anyhow::bail!("failed to copy vm-config to remote");
    }
    Ok(())
}

fn update(target: &str, name: &str, host_dir: &Path, boot: bool, reboot: bool) -> Result<()> {
    let mode = if boot { "boot" } else { "switch" };
    println!("updating '{name}' ({target}, mode: {mode})...");

    // Build the system locally and push the closure to the target.
    // Doing the rebuild remotely would require the flake's path inputs
    // (e.g. `vitro.url = "path:/...";`) to resolve on the target, which
    // they don't. --use-substitutes lets the target fetch substitutable
    // paths directly instead of round-tripping through the laptop.
    let flake_arg = format!("{}#{name}", host_dir.display());
    let status = Command::new("nixos-rebuild")
        .args([
            mode,
            "--flake", &flake_arg,
            "--target-host", target,
            "--use-substitutes",
        ])
        .status()
        .context("nixos-rebuild failed")?;
    if !status.success() {
        anyhow::bail!("nixos-rebuild failed for {target}");
    }

    deploy_vm_config(target, host_dir)?;
    deploy_flake_src(target, host_dir)?;

    if boot && reboot {
        println!("rebooting {target}...");
        Command::new("ssh").args([target, "reboot"]).status().ok();
    }

    println!("updated '{name}'");
    Ok(())
}

fn bootstrap(target: &str, name: &str, host_dir: &Path) -> Result<()> {
    let runner = find_runner().ok_or_else(|| {
        anyhow::anyhow!(
            "bootstrap requires nix, docker, or podman.\n\
             Install one of these and try again."
        )
    })?;

    println!("bootstrapping '{name}' ({target}) with nixos-anywhere...");

    let flake_arg = format!("{}#{name}", host_dir.display());

    match runner {
        Runner::Nix => {
            let status = Command::new("nix")
                .args([
                    "run", "github:nix-community/nixos-anywhere", "--",
                    "--flake", &flake_arg,
                    "--target-host", target,
                ])
                .status()
                .context("nixos-anywhere failed")?;
            if !status.success() {
                anyhow::bail!("nixos-anywhere failed");
            }
        }
        Runner::Docker | Runner::Podman => {
            let cmd = match runner {
                Runner::Docker => "docker",
                Runner::Podman => "podman",
                _ => unreachable!(),
            };
            let ssh_auth = std::env::var("SSH_AUTH_SOCK").unwrap_or_default();
            let mut args = vec![
                "run".to_string(), "--rm".to_string(), "-it".to_string(),
                "-v".to_string(), format!("{}:/flake", host_dir.display()),
                "-v".to_string(), format!("{}/.ssh:/root/.ssh:ro",
                    std::env::var("HOME").unwrap_or_else(|_| "/root".to_string())),
            ];
            if !ssh_auth.is_empty() {
                args.push("-v".to_string());
                args.push(format!("{ssh_auth}:/tmp/ssh-agent.sock"));
                args.push("-e".to_string());
                args.push("SSH_AUTH_SOCK=/tmp/ssh-agent.sock".to_string());
            }
            args.extend([
                "nixos/nix".to_string(),
                "sh".to_string(), "-c".to_string(),
                format!(
                    "nix run github:nix-community/nixos-anywhere -- \
                     --flake /flake#{name} --target-host {target}"
                ),
            ]);

            let status = Command::new(cmd)
                .args(&args)
                .status()
                .context("container-based nixos-anywhere failed")?;
            if !status.success() {
                anyhow::bail!("nixos-anywhere (via {cmd}) failed");
            }
        }
    }

    deploy_vm_config(target, host_dir)?;
    deploy_flake_src(target, host_dir)?;

    println!("bootstrapped '{name}'");
    Ok(())
}

fn find_server_dir() -> Result<PathBuf> {
    // look for flake.nix in current dir
    let cwd = std::env::current_dir().context("cannot determine current directory")?;
    if cwd.join("flake.nix").exists() {
        return Ok(cwd);
    }
    anyhow::bail!("no flake.nix in current directory — run vitro deploy from a server config directory")
}

fn detect_server_name(host_dir: &Path) -> Result<String> {
    // try to extract name from flake.nix (look for mkHost { ... } { name = "..."; })
    // fallback: use directory name
    let flake = std::fs::read_to_string(host_dir.join("flake.nix"))
        .context("reading flake.nix")?;
    // simple heuristic: find name = "..."
    if let Some(cap) = flake.split("name = \"").nth(1) {
        if let Some(name) = cap.split('"').next() {
            return Ok(name.to_string());
        }
    }
    // fallback to dir name
    host_dir.file_name()
        .and_then(|n| n.to_str())
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow::anyhow!("cannot determine server name"))
}

fn detect_target(host_dir: &Path) -> Result<String> {
    // check target file first
    let target_file = host_dir.join("target");
    if target_file.exists() {
        return std::fs::read_to_string(&target_file)
            .map(|s| s.trim().to_string())
            .context("reading target file");
    }
    // check registry
    let name = detect_server_name(host_dir)?;
    let registry = crate::server::load_registry()?;
    if let Some(entry) = registry.get(&name) {
        return Ok(entry.target.clone());
    }
    anyhow::bail!("no target file and server '{name}' not in registry. Add with: vitro server add {name} <target>")
}

pub fn run(explicit_dir: Option<std::path::PathBuf>, boot: bool, reboot: bool) -> Result<()> {
    let host_dir = match explicit_dir {
        Some(p) => {
            if !p.join("flake.nix").exists() {
                anyhow::bail!("no flake.nix at {}", p.display());
            }
            p.canonicalize().context("canonicalizing host dir")?
        }
        None => find_server_dir()?,
    };
    let name = detect_server_name(&host_dir)?;
    let target = detect_target(&host_dir)?;

    // update flake inputs
    println!("updating flake inputs...");
    let status = Command::new("nix")
        .args(["flake", "update", "--flake", &host_dir.to_string_lossy()])
        .status()
        .context("failed to update flake inputs")?;
    if !status.success() {
        anyhow::bail!("failed to update flake inputs");
    }

    if is_nixos(&target)? {
        update(&target, &name, &host_dir, boot, reboot)
    } else {
        bootstrap(&target, &name, &host_dir)
    }
}
