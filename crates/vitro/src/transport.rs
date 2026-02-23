use anyhow::Result;
use console::style;
use tracing::instrument;

use crate::{client, config, git, secrets, server, vm};

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

pub trait Transport {
    fn is_running(&self, env: &str) -> Result<bool>;
    fn ensure_running(&self, env: &str, repo: &git::Repo, cfg: &config::EnvConfig) -> Result<()>;
    fn shell(&self, env: &str, command: Option<&str>) -> Result<()>;
    /// Run a single command with captured output. Used by `--json`.
    fn shell_capture(&self, env: &str, command: &str) -> Result<vm::CapturedShell>;
}

// Local transport — calls vm.rs directly

pub struct LocalTransport;

impl Transport for LocalTransport {
    fn is_running(&self, env: &str) -> Result<bool> {
        vm::is_running(env)
    }

    #[instrument(skip(self, repo, cfg))]
    fn ensure_running(&self, env: &str, repo: &git::Repo, cfg: &config::EnvConfig) -> Result<()> {
        if self.is_running(env)? {
            return Ok(());
        }

        let sp = spinner(&format!("booting {}", env));
        secrets::resolve_local(repo.root(), cfg)?;
        let repo_name = repo.root()
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown");
        vm::start(env, repo_name, cfg)?;
        sp.finish_with_message(format!("{} booted {}", up_icon(), bold(env)));

        Ok(())
    }

    fn shell(&self, env: &str, command: Option<&str>) -> Result<()> {
        vm::shell(env, command)
    }

    fn shell_capture(&self, env: &str, command: &str) -> Result<vm::CapturedShell> {
        vm::shell_capture(env, command)
    }
}

// Remote transport — calls client.rs over SSH

pub struct RemoteTransport {
    pub client: client::Client,
}

impl Transport for RemoteTransport {
    fn is_running(&self, env: &str) -> Result<bool> {
        let envs = self.client.list().unwrap_or_default();
        Ok(envs.iter().any(|e| e.name == env && e.status == "running"))
    }

    #[instrument(skip(self, repo, cfg))]
    fn ensure_running(&self, env: &str, repo: &git::Repo, cfg: &config::EnvConfig) -> Result<()> {
        let repo_name = repo.root()
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown");

        let already_running = self.is_running(env)?;

        // ALWAYS sync secrets — idempotent, cheap. Host never holds an age key.
        let secrets_content = secrets::decrypt_content(repo.root(), cfg)?;
        if let Some(ref content) = secrets_content {
            self.client.push_secrets(content)?;
        }

        // ALWAYS prepare — init_clone_server is a no-op if the bare repo
        // already exists, so this is safe to call on every run.
        if !already_running {
            let sp = spinner("preparing env");
            self.client.prepare(env, repo_name, cfg)?;
            sp.finish_with_message(format!("{} env ready", ok()));
        } else {
            self.client.prepare(env, repo_name, cfg)?;
        }

        // ALWAYS ensure laptop's vitro remote URL is set correctly.
        let remote_url = format!("vitro://{}/{}", self.client.user_host(), env);
        repo.add_vitro_remote(&remote_url).ok();

        if already_running {
            // Push updated secrets into the running env.
            if let Some(ref content) = secrets_content {
                self.client.push_secrets_env(env, content)?;
            }
            return Ok(());
        }

        // Cold start: seed the env's bare repo from laptop HEAD. After this
        // first push, the env is the source of truth — subsequent shells
        // don't re-push (would clobber in-env commits). Use `git push vitro`
        // explicitly when you want to move work from laptop to env.
        let sp = spinner("pushing repo");
        repo.push_to_vitro()?;
        sp.finish_with_message(format!("{} pushed", ok()));

        // build and start with full config (cold-start only)
        let sp = spinner(&format!("booting {}", env));
        self.client.up(env, repo_name, false, cfg)?;
        sp.finish_with_message(format!("{} booted {}", up_icon(), bold(env)));

        // Push secrets into the env now that it's running.
        if let Some(ref content) = secrets_content {
            self.client.push_secrets_env(env, content)?;
        }

        // sync user files
        let client_cfg = server::load_client_config();
        if !client_cfg.sync.is_empty() {
            let sp = spinner("syncing files");
            self.client.sync_files(env, &client_cfg.sync)?;
            sp.finish_with_message(format!("{} synced", ok()));
        }

        Ok(())
    }

    fn shell(&self, env: &str, command: Option<&str>) -> Result<()> {
        self.client.shell(env, command)
    }

    fn shell_capture(&self, env: &str, command: &str) -> Result<vm::CapturedShell> {
        self.client.shell_capture(env, command)
    }
}
