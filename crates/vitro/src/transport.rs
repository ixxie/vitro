use anyhow::{Context, Result};
use console::style;
use tracing::instrument;

use crate::{client, config, exec, git, secrets, server, vm};

fn ok() -> console::StyledObject<&'static str> { style("✓").green() }
fn up_icon() -> console::StyledObject<&'static str> { style("▲").green() }
fn bold(s: &str) -> console::StyledObject<&str> { style(s).bold() }

fn spinner(msg: &str) -> indicatif::ProgressBar {
    let pb = indicatif::ProgressBar::new_spinner();
    pb.set_style(
        indicatif::ProgressStyle::default_spinner()
            .tick_chars("⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏")
            .template("  {spinner} {msg}")
            .unwrap()
    );
    pb.set_message(msg.to_string());
    pb.enable_steady_tick(std::time::Duration::from_millis(80));
    pb
}

const RUN_LOG: &str = "/tmp/vitro/run.log";

/// Build the inline shell command run inside the cell to start a script with env.
fn build_run_cmd(script_path: &str, env: &[(&str, &str)], attached: bool) -> String {
    let mut prefix = String::new();
    for (k, v) in env {
        prefix.push_str(&format!("{}={} ", k, exec::shell_escape(v)));
    }
    let chmod = format!("chmod +x {} && ", exec::shell_escape(script_path));
    let inner = format!("{chmod}{prefix}{}", exec::shell_escape(script_path));
    if attached {
        exec::attached_with_lock(&inner)
    } else {
        exec::detached(&inner, RUN_LOG)
    }
}

pub trait Transport {
    fn is_running(&self, cell: &str) -> Result<bool>;
    fn ensure_running(&self, cell: &str, repo: &git::Repo, cfg: &config::CellConfig) -> Result<()>;
    fn shell(&self, cell: &str, command: Option<&str>) -> Result<()>;
    /// Run a single command with captured output. Used by `--json`.
    fn shell_capture(&self, cell: &str, command: &str) -> Result<vm::CapturedShell>;
    /// Pipe stdin/stdout/stderr through to a command running inside
    /// the cell. No PTY, no spinners, no log file. Used for ACP
    /// (JSON-RPC over stdio) bridging.
    fn acp_forward(&self, cell: &str, command: &str) -> Result<()>;
    fn run_start(&self, cell: &str, script_path: &str, env: &[(&str, &str)], attached: bool) -> Result<()>;
    fn run_logs(&self, cell: &str, follow: bool) -> Result<()>;
}

// Local transport — calls vm.rs directly

pub struct LocalTransport;

impl Transport for LocalTransport {
    fn is_running(&self, cell: &str) -> Result<bool> {
        vm::is_running(cell)
    }

    #[instrument(skip(self, repo, cfg))]
    fn ensure_running(&self, cell: &str, repo: &git::Repo, cfg: &config::CellConfig) -> Result<()> {
        if self.is_running(cell)? {
            return Ok(());
        }

        if !repo.branch_exists(cell) {
            anyhow::bail!("branch '{cell}' does not exist — use 'vitro create {cell}' first");
        }

        let sp = spinner(&format!("booting {}", cell));
        secrets::resolve_local(repo.root(), cfg)?;
        repo.init_clone(cell, cfg)?;
        let repo_name = repo.root()
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown");
        vm::start(cell, repo_name, cfg)?;
        sp.finish_with_message(format!("{} booted {}", up_icon(), bold(cell)));

        Ok(())
    }

    fn shell(&self, cell: &str, command: Option<&str>) -> Result<()> {
        vm::shell(cell, command)
    }

    fn acp_forward(&self, cell: &str, command: &str) -> Result<()> {
        vm::acp_forward(cell, command)
    }

    fn shell_capture(&self, cell: &str, command: &str) -> Result<vm::CapturedShell> {
        vm::shell_capture(cell, command)
    }

    fn run_start(&self, cell: &str, script_path: &str, env: &[(&str, &str)], attached: bool) -> Result<()> {
        let cmd = build_run_cmd(script_path, env, attached);
        vm::shell(cell, Some(&cmd))
    }

    fn run_logs(&self, cell: &str, follow: bool) -> Result<()> {
        let cmd = if follow {
            format!("tail -f {RUN_LOG} 2>/dev/null || echo 'no run log yet'")
        } else {
            format!("tail -100 {RUN_LOG} 2>/dev/null || echo 'no run log yet'")
        };
        vm::shell(cell, Some(&cmd))
    }
}

// Remote transport — calls client.rs over SSH

pub struct RemoteTransport {
    pub client: client::Client,
}

impl Transport for RemoteTransport {
    fn is_running(&self, cell: &str) -> Result<bool> {
        let cells = self.client.list().unwrap_or_default();
        Ok(cells.iter().any(|c| c.name == cell && c.status == "running"))
    }

    #[instrument(skip(self, repo, cfg))]
    fn ensure_running(&self, cell: &str, repo: &git::Repo, cfg: &config::CellConfig) -> Result<()> {
        if !repo.branch_exists(cell) {
            anyhow::bail!("branch '{cell}' does not exist — use 'vitro create {cell}' first");
        }

        let repo_name = repo.root()
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown");

        let already_running = self.is_running(cell)?;

        // ALWAYS sync secrets — idempotent, cheap. Host never holds an age key.
        if let Some(content) = secrets::decrypt_content(repo.root(), cfg)? {
            self.client.push_secrets(&content)?;
        }

        // ALWAYS prepare — init_clone_server is a no-op if the bare repo
        // already exists, so this is safe to call on every run.
        if !already_running {
            let sp = spinner("preparing cell");
            self.client.prepare(cell, repo_name, cfg)?;
            sp.finish_with_message(format!("{} cell ready", ok()));
        } else {
            self.client.prepare(cell, repo_name, cfg)?;
        }

        // ALWAYS ensure laptop's vitro remote URL is set correctly.
        let remote_url = format!("vitro://{}/{}", self.client.user_host(), cell);
        repo.add_vitro_remote(&remote_url).ok();

        // ALWAYS push the cell branch — picks up edits to flow files,
        // build.ts, .vitro/config.toml, etc., even when the cell is
        // already running. denyCurrentBranch=updateInstead on the server
        // refreshes the working tree the cell mounts via virtiofs.
        let sp = spinner("pushing code");
        let push = std::process::Command::new("git")
            .args(["push", "vitro", cell])
            .current_dir(repo.root())
            .output()
            .context("git push failed")?;
        if !push.status.success() {
            sp.finish_with_message(format!("{} push failed", style("!").red()));
            anyhow::bail!(
                "git push vitro {cell} failed — cell would run with stale code.\n\
                 stderr: {}",
                String::from_utf8_lossy(&push.stderr).trim(),
            );
        }
        sp.finish_with_message(format!("{} pushed", ok()));

        if already_running {
            return Ok(());
        }

        // build and start with full config (cold-start only)
        let sp = spinner(&format!("booting {}", cell));
        self.client.up(cell, repo_name, false, cfg)?;
        sp.finish_with_message(format!("{} booted {}", up_icon(), bold(cell)));

        // sync user files
        let client_cfg = server::load_client_config();
        if !client_cfg.sync.is_empty() {
            let sp = spinner("syncing files");
            self.client.sync_files(cell, &client_cfg.sync)?;
            sp.finish_with_message(format!("{} synced", ok()));
        }

        Ok(())
    }

    fn shell(&self, cell: &str, command: Option<&str>) -> Result<()> {
        self.client.shell(cell, command)
    }

    fn shell_capture(&self, cell: &str, command: &str) -> Result<vm::CapturedShell> {
        self.client.shell_capture(cell, command)
    }

    fn acp_forward(&self, cell: &str, command: &str) -> Result<()> {
        self.client.acp_forward(cell, command)
    }

    fn run_start(&self, cell: &str, script_path: &str, env: &[(&str, &str)], attached: bool) -> Result<()> {
        let cmd = build_run_cmd(script_path, env, attached);
        self.client.shell(cell, Some(&cmd))
    }

    fn run_logs(&self, cell: &str, follow: bool) -> Result<()> {
        let cmd = if follow {
            format!("tail -f {RUN_LOG} 2>/dev/null || echo 'no run log yet'")
        } else {
            format!("tail -100 {RUN_LOG} 2>/dev/null || echo 'no run log yet'")
        };
        self.client.shell(cell, Some(&cmd))
    }
}
