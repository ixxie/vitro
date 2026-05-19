use anyhow::{Context, Result};
use std::path::Path;
use std::process::Command;
use tracing::{info, warn, instrument};

use crate::env;
use crate::config::EnvConfig;
const PROXY_CONTROL: &str = "http://127.0.0.1:8082";
const DNS_HOSTS: &str = "/var/lib/vitro/dns-hosts";

fn is_root() -> bool {
    std::env::var("USER").unwrap_or_default() == "root"
}

fn run_privileged(args: &[&str]) -> Result<std::process::ExitStatus> {
    if is_root() {
        Command::new(args[0]).args(&args[1..]).status().map_err(Into::into)
    } else {
        Command::new("sudo").args(args).status().map_err(Into::into)
    }
}

// Re-export env path helpers
pub use crate::env::{env_dir, env_repo_dir, runtime_dir, list_envs};

const SERVER_VM_CONFIG: &str = "/var/lib/vitro/vm-config";

fn has_server_config() -> bool {
    Path::new(SERVER_VM_CONFIG).join("flake.nix").exists()
}

fn has_repo_config(name: &str) -> bool {
    env_repo_dir(name).join(".vitro/flake.nix").exists()
}

const HOST_CONFIG: &str = "/etc/vitro/host-config.json";

fn read_host_config_user() -> Option<String> {
    let json = std::fs::read_to_string(HOST_CONFIG).ok()?;
    let val: serde_json::Value = serde_json::from_str(&json).ok()?;
    val.get("user")?.get("name")?.as_str().map(|s| s.to_string())
}

fn generate_flake(name: &str, ip: &str, repo_name: &str, config: &EnvConfig) -> Result<()> {
    let flake_dir = env::env_flake_dir(name);
    // clean slate — remove stale lock files
    if flake_dir.exists() {
        std::fs::remove_dir_all(&flake_dir).ok();
    }
    std::fs::create_dir_all(&flake_dir)?;

    let env_dir_str = env_dir(name).to_string_lossy().to_string();
    let repo_vitro_dir = env_repo_dir(name).join(".vitro");

    let has_server = has_server_config();
    let has_repo = has_repo_config(name);

    // read host config and inline as nix attrset
    let host_config_nix = if Path::new(HOST_CONFIG).exists() {
        let json = std::fs::read_to_string(HOST_CONFIG)
            .context("reading host-config.json")?;
        let mut val: serde_json::Value = serde_json::from_str(&json)
            .context("parsing host-config.json")?;

        // inject the host's SSH public key so server-side operations (sweep) can reach VMs
        if let Ok(host_pubkey) = std::fs::read_to_string("/var/lib/vitro/ssh/id_ed25519.pub") {
            let key = host_pubkey.trim().to_string();
            if let Some(keys) = val.pointer_mut("/user/authorizedKeys") {
                if let Some(arr) = keys.as_array_mut() {
                    if !arr.iter().any(|k| k.as_str() == Some(&key)) {
                        arr.push(serde_json::Value::String(key));
                    }
                }
            }
        }

        json_to_nix(&val)
    } else {
        "{}".to_string()
    };

    // build inputs block
    let mut extra_inputs = String::new();
    if has_server {
        extra_inputs.push_str(&format!(
            "    serverConfig.url = \"path:{}\";\n",
            SERVER_VM_CONFIG
        ));
    }
    if has_repo {
        extra_inputs.push_str(&format!(
            "    repoConfig.url = \"path:{}\";\n",
            repo_vitro_dir.display()
        ));
    }

    // build modules list
    let mut modules = Vec::new();
    if has_server {
        modules.push("inputs.serverConfig.nixosModule");
    }
    if has_repo {
        modules.push("inputs.repoConfig.nixosModule");
    }
    let modules_nix = if modules.is_empty() {
        String::new()
    } else {
        format!("      modules = [ {} ];", modules.join(" "))
    };

    // build persist list
    let persist_nix = if config.persist.is_empty() {
        String::new()
    } else {
        let entries: Vec<String> = config.persist.iter().map(|p| {
            format!("{{ path = \"{}\"; }}", p.path.replace('"', "\\\""))
        }).collect();
        format!("      persist = [ {} ];\n", entries.join(" "))
    };

    let flake = format!(r#"{{
  inputs = {{
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    vitro.url = "github:ixxie/vitro";
    microvm = {{
      url = "github:microvm-nix/microvm.nix";
      inputs.nixpkgs.follows = "nixpkgs";
    }};
{extra_inputs}  }};

  outputs = {{ nixpkgs, vitro, microvm, ... }} @ inputs:
    vitro.lib.mkEnv {{ inherit vitro nixpkgs microvm; }} {{
      name = "{name}";
      ip = "{ip}";
      envDir = "{env_dir}";
      repo = "{repo}";
      hostConfig = {host_config};
{modules}{persist}    }};
}}
"#,
        extra_inputs = extra_inputs,
        name = name,
        ip = ip,
        env_dir = env_dir_str,
        repo = repo_name,
        host_config = host_config_nix,
        modules = modules_nix,
        persist = persist_nix,
    );

    std::fs::write(flake_dir.join("flake.nix"), flake)?;

    // resolve latest versions of all inputs
    let status = Command::new("nix")
        .args([
            "--extra-experimental-features", "nix-command flakes",
            "flake", "update", "--flake", &flake_dir.to_string_lossy(),
        ])
        .status()
        .context("nix flake update failed")?;
    if !status.success() {
        anyhow::bail!("failed to update flake inputs for env '{name}'");
    }

    Ok(())
}

fn json_to_nix(val: &serde_json::Value) -> String {
    match val {
        serde_json::Value::Null => "null".to_string(),
        serde_json::Value::Bool(b) => if *b { "true" } else { "false" }.to_string(),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::String(s) => format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\"")),
        serde_json::Value::Array(arr) => {
            let items: Vec<String> = arr.iter().map(json_to_nix).collect();
            format!("[ {} ]", items.join(" "))
        }
        serde_json::Value::Object(obj) => {
            let pairs: Vec<String> = obj.iter().map(|(k, v)| {
                format!("{} = {};", k, json_to_nix(v))
            }).collect();
            format!("{{ {} }}", pairs.join(" "))
        }
    }
}

// SSH helpers

pub fn ssh_target(name: &str) -> Result<(String, String)> {
    let rt = runtime_dir(name);
    let ip = std::fs::read_to_string(rt.join("ip"))
        .context("VM not running (no ip file)")?
        .trim()
        .to_string();
    let user = std::fs::read_to_string(rt.join("user"))
        .unwrap_or_default().trim().to_string();
    let target = if user.is_empty() {
        ip.clone()
    } else {
        format!("{user}@{ip}")
    };
    Ok((ip, target))
}

// VM lifecycle

#[instrument]
pub fn is_running(name: &str) -> Result<bool> {
    let rt = runtime_dir(name);
    if !rt.join("ip").exists() {
        return Ok(false);
    }
    let output = Command::new("systemctl")
        .args(["is-active", &format!("microvm@{name}")])
        .output()?;
    Ok(output.status.success())
}

#[instrument(skip(config))]
pub fn start(name: &str, repo_name: &str, config: &EnvConfig) -> Result<()> {
    let rt = runtime_dir(name);
    std::fs::create_dir_all(&rt)?;

    let ip = env::allocate_ip(name)?;
    info!(ip = %ip, "allocated IP");

    // create host-side directories for persist mounts
    for p in &config.persist {
        let stripped = p.path.trim_start_matches('/');
        let host_path = env_dir(name).join("persist").join(stripped);
        std::fs::create_dir_all(&host_path)
            .with_context(|| format!("creating persist dir for {}", p.path))?;
    }

    // generate wrapper flake
    generate_flake(name, &ip, repo_name, config)?;

    // build the VM runner
    let flake_path = env::env_flake_dir(name);
    let flake_ref = format!("path:{}", flake_path.display());
    let runner_attr = format!("{flake_ref}#nixosConfigurations.{name}.config.microvm.declaredRunner");

    let output = Command::new("nix")
        .args([
            "--extra-experimental-features", "nix-command flakes",
            "build", &runner_attr,
            "--no-link", "--print-out-paths",
        ])
        .output()
        .context("nix build failed")?;
    if !output.status.success() {
        env::release_ip(name);
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("failed to build env VM '{name}': {stderr}");
    }
    let runner_path = String::from_utf8(output.stdout)?.trim().to_string();

    // link runner into /var/lib/microvms/{name}/current for microvm@ template
    let microvms_dir = format!("/var/lib/microvms/{name}");
    run_privileged(&["mkdir", "-p", &microvms_dir])?;
    run_privileged(&["chown", "microvm:kvm", &microvms_dir])?;
    let current_link = format!("{microvms_dir}/current");
    run_privileged(&["rm", "-f", &current_link]).ok();
    run_privileged(&["ln", "-sf", &runner_path, &current_link])?;
    run_privileged(&["chown", "-h", "microvm:kvm", &current_link])?;

    // start the VM
    let status = run_privileged(&["systemctl", "start", &format!("microvm@{name}")])?;
    if !status.success() {
        env::release_ip(name);
        anyhow::bail!("failed to start VM for env '{name}'");
    }

    // register proxy rules from vitro.toml
    register_proxy_rules(&ip, name, config)?;

    // register DNS: env.repo.env -> VM IP
    register_dns(name, repo_name, &ip);

    // save state
    std::fs::write(rt.join("ip"), &ip)?;
    std::fs::write(rt.join("repo"), repo_name)?;
    let guest_user = read_host_config_user().unwrap_or_else(|| {
        std::env::var("USER").unwrap_or_default()
    });
    if !guest_user.is_empty() {
        std::fs::write(rt.join("user"), &guest_user)?;
    }

    Ok(())
}

#[instrument]
pub fn stop(name: &str) -> Result<()> {
    let rt = runtime_dir(name);
    if !rt.join("ip").exists() {
        return Ok(());
    }

    let ip = std::fs::read_to_string(rt.join("ip")).unwrap_or_default().trim().to_string();
    let repo = std::fs::read_to_string(rt.join("repo")).unwrap_or_default().trim().to_string();

    // deregister proxy rules and DNS
    if !ip.is_empty() {
        if let Err(e) = deregister_proxy_rules(&ip) {
            warn!(error = %e, ip = %ip, "failed to deregister proxy rules");
        }
    }
    if !repo.is_empty() {
        deregister_dns(name, &repo);
    }

    // stop the VM
    if let Err(e) = run_privileged(&["systemctl", "stop", &format!("microvm@{name}")]) {
        warn!(error = %e, "failed to stop VM");
    }

    // cleanup runtime (but NOT env dir or IP — those persist)
    std::fs::remove_file(rt.join("ip")).ok(); // may not exist

    Ok(())
}

#[instrument]
pub fn delete(name: &str) -> Result<()> {
    // stop first if running
    if is_running(name)? {
        stop(name)?;
    }

    // remove env directory
    let dir = env_dir(name);
    if dir.exists() {
        let status = Command::new("sudo")
            .args(["rm", "-rf", &dir.to_string_lossy()])
            .status()
            .context("failed to remove env dir")?;
        if !status.success() {
            anyhow::bail!("failed to remove env dir at {}", dir.display());
        }
    }

    // remove microvm-managed dir too — without this, var.img survives
    // a vitro remove and a future env start reuses it. Manifests as
    // "vm.varSize change didn't take effect".
    let microvm_dir = std::path::PathBuf::from(format!("/var/lib/microvms/{name}"));
    if microvm_dir.exists() {
        let status = Command::new("sudo")
            .args(["rm", "-rf", &microvm_dir.to_string_lossy()])
            .status()
            .context("failed to remove microvm dir")?;
        if !status.success() {
            anyhow::bail!("failed to remove microvm dir at {}", microvm_dir.display());
        }
    }

    // release IP
    env::release_ip(name);

    // cleanup runtime dir
    let rt = runtime_dir(name);
    std::fs::remove_dir_all(&rt).ok(); // best-effort cleanup

    Ok(())
}

/// Block until the VM at `target` has SSH up and the workspace mounted.
fn wait_for_vm(target: &str, workspace: &str) -> Result<()> {
    let probe_cmd = format!("test -d {workspace}");
    for _ in 0..60 {
        let probe = Command::new("ssh")
            .args([
                "-o", "StrictHostKeyChecking=no",
                "-o", "UserKnownHostsFile=/dev/null",
                "-o", "LogLevel=ERROR",
                "-o", "ConnectTimeout=1",
                target,
                &probe_cmd,
            ])
            .output();
        if let Ok(out) = probe {
            if out.status.success() {
                return Ok(());
            }
        }
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
    anyhow::bail!("VM did not become reachable within 60s")
}

/// Captured output from running a non-interactive command in an env.
pub struct CapturedShell {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
    pub duration_ms: u64,
}

/// Run a single command in an env and capture its output. Used by
/// `vitro shell -c "..." --json`.
#[instrument]
pub fn shell_capture(name: &str, command: &str) -> Result<CapturedShell> {
    let (_, target) = ssh_target(name)?;
    let repo_name = std::fs::read_to_string(runtime_dir(name).join("repo"))
        .unwrap_or_else(|_| "env".to_string()).trim().to_string();
    let workspace = format!("/{repo_name}");

    wait_for_vm(&target, &workspace)?;

    let script = format!("cd {} && {}", crate::exec::shell_escape(&workspace), command);
    let cmd = format!("sh -c {}", crate::exec::shell_escape(&script));

    let start = std::time::Instant::now();
    let output = Command::new("ssh")
        .args([
            "-A",
            "-o", "StrictHostKeyChecking=no",
            "-o", "UserKnownHostsFile=/dev/null",
            "-o", "LogLevel=ERROR",
            "-o", "ServerAliveInterval=30",
            "-o", "ServerAliveCountMax=3",
            &target,
            &cmd,
        ])
        .output()
        .context("ssh failed")?;
    let duration_ms = start.elapsed().as_millis() as u64;

    Ok(CapturedShell {
        exit_code: output.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        duration_ms,
    })
}

/// Pipe stdin/stdout/stderr through to a command running inside the
/// env. No PTY, no spinner, no log file. Stdin from the parent is
/// inherited (Rust's default) so JSON-RPC frames travel transparently.
/// Exit code propagates.
#[instrument]
pub fn acp_forward(name: &str, command: &str) -> Result<()> {
    let (_, target) = ssh_target(name)?;
    let repo_name = std::fs::read_to_string(runtime_dir(name).join("repo"))
        .unwrap_or_else(|_| "env".to_string()).trim().to_string();
    let workspace = format!("/{repo_name}");

    wait_for_vm(&target, &workspace)?;

    let script = format!("cd {} && {}", crate::exec::shell_escape(&workspace), command);
    let cmd = format!("sh -c {}", crate::exec::shell_escape(&script));

    let status = Command::new("ssh")
        .args([
            "-A",
            "-o", "StrictHostKeyChecking=no",
            "-o", "UserKnownHostsFile=/dev/null",
            "-o", "LogLevel=ERROR",
            "-o", "ServerAliveInterval=30",
            "-o", "ServerAliveCountMax=3",
            &target,
            &cmd,
        ])
        .status()
        .context("ssh failed")?;
    if !status.success() {
        anyhow::bail!("acp forward exited with {}", status);
    }
    Ok(())
}

#[instrument]
pub fn shell(
    name: &str,
    command: Option<&str>,
    session: Option<&str>,
) -> Result<()> {
    let (_, target) = ssh_target(name)?;

    let repo_name = std::fs::read_to_string(runtime_dir(name).join("repo"))
        .unwrap_or_else(|_| "env".to_string()).trim().to_string();
    let workspace = format!("/{repo_name}");

    let sp = indicatif::ProgressBar::new_spinner();
    sp.set_style(
        indicatif::ProgressStyle::default_spinner()
            .tick_chars("⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏")
            .template("  {spinner} {msg}")
            .unwrap()
    );
    sp.set_message("waiting for VM");
    sp.enable_steady_tick(std::time::Duration::from_millis(80));
    let wait_result = wait_for_vm(&target, &workspace);
    sp.finish_and_clear();
    wait_result?;

    // apply synced files from host-side sync dir to VM home
    let sync_dir = format!("/var/lib/vitro/envs/{name}/sync");
    if Path::new(&sync_dir).exists() {
        let scp_result = Command::new("scp")
            .args([
                "-r",
                "-o", "StrictHostKeyChecking=no",
                "-o", "UserKnownHostsFile=/dev/null",
                "-o", "LogLevel=ERROR",
                &format!("{sync_dir}/."),
                &format!("{target}:~/"),
            ])
            .output();
        if let Ok(out) = &scp_result {
            if !out.status.success() {
                warn!(stderr = %String::from_utf8_lossy(&out.stderr), "sync apply failed");
            }
        }
    }

    let (use_pty, cmd) = match (command, session) {
        (None, None) => {
            // interactive shell without explicit session -> default session
            let inner = format!("cd {} && exec $SHELL -l", crate::exec::shell_escape(&workspace));
            (true, crate::session::dtach_attach(name, "default", &inner))
        }
        (Some(c), None) => {
            // one-shot command, no session -> direct SSH
            let script = format!("cd {} && {}", crate::exec::shell_escape(&workspace), c);
            (false, format!("sh -c {}", crate::exec::shell_escape(&script)))
        }
        (_, Some(s)) => {
            // named session (interactive or command) -> dtach attach/create
            let inner = match command {
                Some(c) => format!("cd {} && {}", crate::exec::shell_escape(&workspace), c),
                None => format!("cd {} && exec $SHELL -l", crate::exec::shell_escape(&workspace)),
            };
            (true, crate::session::dtach_attach(name, s, &inner))
        }
    };

    let mut ssh = Command::new("ssh");
    if use_pty {
        ssh.arg("-t");
    }
    ssh.args([
        "-A",
        "-o", "StrictHostKeyChecking=no",
        "-o", "UserKnownHostsFile=/dev/null",
        "-o", "LogLevel=ERROR",
        "-o", "ServerAliveInterval=30",
        "-o", "ServerAliveCountMax=3",
        &target,
        &cmd,
    ]);

    let status = ssh
        .status()
        .context("ssh failed")?;

    if !status.success() {
        anyhow::bail!("ssh exited with {}", status);
    }
    Ok(())
}

// Proxy control API

fn register_proxy_rules(ip: &str, env_name: &str, config: &EnvConfig) -> Result<()> {
    let mut body = serde_json::json!({
        "envIp": ip,
        "envId": env_name,
    });

    // add env-level egress rules
    let egress = &config.egress;
    if egress.writes.is_some() || egress.reads.is_some() || !egress.credentials.is_empty() {
        let mut egress_json = serde_json::json!({ "additive": true });
        if let Some(ref writes) = egress.writes {
            egress_json["writes"] = serde_json::json!({
                "allowed": writes.allowed,
                "denied": writes.denied,
            });
        }
        if let Some(ref reads) = egress.reads {
            egress_json["reads"] = serde_json::json!({
                "allowed": reads.allowed,
                "denied": reads.denied,
            });
        }
        if !egress.credentials.is_empty() {
            egress_json["credentials"] = serde_json::json!(
                egress.credentials.iter().map(|c| serde_json::json!({
                    "host": c.host,
                    "header": c.header,
                    "envVar": c.env_var,
                })).collect::<Vec<_>>()
            );
        }
        body["egress"] = egress_json;
    }

    let output = Command::new("curl")
        .args([
            "-sf",
            "-X", "POST",
            "-H", "Content-Type: application/json",
            "-d", &body.to_string(),
            &format!("{PROXY_CONTROL}/envs"),
        ])
        .output()
        .context("failed to register proxy rules")?;

    if !output.status.success() {
        warn!("failed to register proxy rules (proxy may not be running)");
    }
    Ok(())
}

fn deregister_proxy_rules(ip: &str) -> Result<()> {
    let output = Command::new("curl")
        .args([
            "-sf",
            "-X", "DELETE",
            &format!("{PROXY_CONTROL}/envs/{ip}"),
        ])
        .output()
        .context("failed to deregister proxy rules")?;

    if !output.status.success() {
        warn!("failed to deregister proxy rules");
    }
    Ok(())
}

// DNS registration

fn sanitize_dns(s: &str) -> String {
    let sanitized: String = s
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let mut result = String::new();
    let mut prev_hyphen = true;
    for c in sanitized.chars() {
        if c == '-' {
            if !prev_hyphen {
                result.push(c);
            }
            prev_hyphen = true;
        } else {
            result.push(c);
            prev_hyphen = false;
        }
    }
    result.trim_end_matches('-').to_string()
}

pub fn dns_hostname(env_name: &str, repo: &str) -> String {
    format!("{}.{}.env", sanitize_dns(env_name), sanitize_dns(repo))
}

fn register_dns(env_name: &str, repo: &str, ip: &str) {
    let hostname = dns_hostname(env_name, repo);
    let entry = format!("{ip} {hostname}");
    let hosts = std::fs::read_to_string(DNS_HOSTS).unwrap_or_default();
    if !hosts.contains(&entry) {
        let updated = if hosts.is_empty() {
            format!("{entry}\n")
        } else {
            format!("{hosts}{entry}\n")
        };
        if let Err(e) = std::fs::write(DNS_HOSTS, updated) {
            warn!(error = %e, "failed to write DNS hosts");
        }
    }
    if let Err(e) = Command::new("sudo")
        .args(["systemctl", "reload", "dnsmasq"])
        .status() {
        warn!(error = %e, "failed to reload dnsmasq");
    }
    info!(hostname = %hostname, "registered DNS");
}

fn deregister_dns(env_name: &str, repo: &str) {
    let hostname = dns_hostname(env_name, repo);
    if let Ok(hosts) = std::fs::read_to_string(DNS_HOSTS) {
        let updated: String = hosts
            .lines()
            .filter(|line| !line.contains(&hostname))
            .collect::<Vec<_>>()
            .join("\n");
        let updated = if updated.is_empty() {
            String::new()
        } else {
            format!("{updated}\n")
        };
        if let Err(e) = std::fs::write(DNS_HOSTS, updated) {
            warn!(error = %e, "failed to write DNS hosts on deregister");
        }
        if let Err(e) = Command::new("sudo")
            .args(["systemctl", "reload", "dnsmasq"])
            .status() {
            warn!(error = %e, "failed to reload dnsmasq on deregister");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    // DNS sanitization

    #[test]
    fn sanitize_simple() {
        assert_eq!(sanitize_dns("feat"), "feat");
    }

    #[test]
    fn sanitize_slashes() {
        assert_eq!(sanitize_dns("feature/auth-flow"), "feature-auth-flow");
    }

    #[test]
    fn sanitize_uppercase() {
        assert_eq!(sanitize_dns("MyEnv"), "myenv");
    }

    #[test]
    fn sanitize_consecutive_special() {
        assert_eq!(sanitize_dns("a//b--c"), "a-b-c");
    }

    #[test]
    fn sanitize_leading_trailing() {
        assert_eq!(sanitize_dns("-leading-"), "leading");
        assert_eq!(sanitize_dns("//trailing//"), "trailing");
    }

    #[test]
    fn dns_hostname_basic() {
        assert_eq!(dns_hostname("feat", "myapp"), "feat.myapp.env");
    }

    #[test]
    fn dns_hostname_complex() {
        assert_eq!(dns_hostname("feature/auth", "my-app"), "feature-auth.my-app.env");
    }

    // Env path construction

    #[test]
    fn env_dir_path() {
        assert_eq!(env_dir("myapp-feat"), PathBuf::from("/var/lib/vitro/envs/myapp-feat"));
    }

    #[test]
    fn env_repo_dir_path() {
        assert_eq!(env_repo_dir("myapp-feat"), PathBuf::from("/var/lib/vitro/envs/myapp-feat/repo"));
    }

    #[test]
    fn env_flake_dir_path() {
        assert_eq!(env::env_flake_dir("myapp-feat"), PathBuf::from("/var/lib/vitro/envs/myapp-feat/flake"));
    }

    // IP pool (using temp files)

    #[test]
    fn ip_pool_roundtrip() {
        let mut pool = std::collections::HashMap::new();
        pool.insert("env-a".to_string(), "192.168.83.11".to_string());
        pool.insert("env-b".to_string(), "192.168.83.12".to_string());

        let json = serde_json::to_string_pretty(&pool).unwrap();
        let loaded: std::collections::HashMap<String, String> = serde_json::from_str(&json).unwrap();
        assert_eq!(pool, loaded);
    }

    #[test]
    fn ip_pool_first_free() {
        let mut used = std::collections::HashSet::new();
        used.insert("192.168.83.11");
        used.insert("192.168.83.12");

        let mut found = None;
        for i in 11..=254u16 {
            let ip = format!("192.168.83.{i}");
            if !used.contains(ip.as_str()) {
                found = Some(ip);
                break;
            }
        }
        assert_eq!(found, Some("192.168.83.13".to_string()));
    }

    #[test]
    fn ip_pool_range() {
        // verify range is .11 to .254 (244 addresses)
        let count = (11..=254u16).count();
        assert_eq!(count, 244);
    }
}
