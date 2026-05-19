use anyhow::{Context, Result};
use tracing::{warn, instrument};

use crate::config::EnvConfig;
use crate::ssh;

const CONTROL_PORT: u32 = 8082;

pub struct Client {
    session: ssh::Session,
    rt: tokio::runtime::Runtime,
    user_host: String,
}

#[derive(serde::Deserialize)]
#[allow(dead_code)]
pub struct UpResponse {
    pub ok: bool,
    pub ip: Option<String>,
    pub error: Option<String>,
}

#[derive(serde::Deserialize)]
pub struct EnvStatus {
    pub name: String,
    pub status: String,
    pub ip: Option<String>,
    pub repo: Option<String>,
}

impl Client {
    pub fn user_host(&self) -> &str {
        &self.user_host
    }

    #[instrument]
    pub fn connect(user_host: &str) -> Result<Self> {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .context("failed to create tokio runtime")?;

        let session = rt.block_on(async {
            ssh::Session::connect(user_host).await
        }).context("SSH connection failed")?;

        Ok(Self {
            session,
            rt,
            user_host: user_host.to_string(),
        })
    }

    fn request(&self, method: &str, path: &str, body: Option<&str>) -> Result<String> {
        self.rt.block_on(async {
            tokio::time::timeout(
                std::time::Duration::from_secs(300),
                self.session.http_request("127.0.0.1", CONTROL_PORT, method, path, body),
            )
            .await
            .context("control API request timed out")?
        })
    }

    pub fn prepare(&self, name: &str, repo: &str, config: &EnvConfig) -> Result<()> {
        let body = serde_json::json!({
            "name": name,
            "repo": repo,
            "config": config,
        });
        let resp = self.request("POST", "/prepare", Some(&body.to_string()))?;
        if !resp.contains("\"ok\":true") {
            anyhow::bail!("server error: {resp}");
        }
        Ok(())
    }

    pub fn up(&self, name: &str, repo: &str, create: bool, config: &EnvConfig) -> Result<UpResponse> {
        let body = serde_json::json!({
            "name": name,
            "repo": repo,
            "create": create,
            "config": config,
        });
        let resp = self.request("POST", "/up", Some(&body.to_string()))?;
        let up: UpResponse = serde_json::from_str(&resp)
            .context(format!("bad response from server: {resp}"))?;
        if !up.ok {
            anyhow::bail!("server error: {}", up.error.unwrap_or_default());
        }
        Ok(up)
    }

    pub fn down(&self, name: &str) -> Result<()> {
        let body = serde_json::json!({ "name": name });
        let resp = self.request("POST", "/down", Some(&body.to_string()))?;
        if !resp.contains("\"ok\":true") {
            anyhow::bail!("server error: {resp}");
        }
        Ok(())
    }

    pub fn delete(&self, name: &str) -> Result<()> {
        let body = serde_json::json!({ "name": name });
        let resp = self.request("POST", "/delete", Some(&body.to_string()))?;
        if !resp.contains("\"ok\":true") {
            anyhow::bail!("server error: {resp}");
        }
        Ok(())
    }

    pub fn list(&self) -> Result<Vec<EnvStatus>> {
        let resp = self.request("GET", "/list", None)?;
        serde_json::from_str(&resp).context(format!("bad response: {resp}"))
    }

    /// SSH into an env.
    ///
    /// Interactive sessions use ProxyJump (laptop → grove → VM in one hop)
    /// to avoid double-encryption lag. Non-interactive commands fall back to
    /// the grove-side `vitro shell -c` path since they don't need a PTY.
    pub fn shell(&self, name: &str, command: Option<&str>, session: Option<&str>) -> Result<()> {
        let grove = &self.user_host;

        if command.is_none() {
            // Interactive: ProxyJump directly to the VM for low-latency PTY.
            let vm_ip = self.list()
                .ok()
                .and_then(|envs| envs.into_iter().find(|e| e.name == name))
                .and_then(|e| e.ip);

            if let Some(ip) = vm_ip {
                let vm_target = format!("agent@{ip}");
                let sess = session.unwrap_or("default");
                let envs = self.list().unwrap_or_default();
                let repo_name = envs.iter()
                    .find(|e| e.name == name)
                    .and_then(|e| e.repo.clone())
                    .unwrap_or_else(|| name.to_string());
                let repo = format!("/{repo_name}");
                let inner = format!(
                    "cd {} && exec $SHELL -l",
                    crate::exec::shell_escape(&repo)
                );
                let dtach_cmd = crate::session::dtach_attach(name, sess, &inner);
                let status = std::process::Command::new("ssh")
                    .args([
                        "-t", "-A",
                        "-o", "StrictHostKeyChecking=no",
                        "-o", "UserKnownHostsFile=/dev/null",
                        "-o", "ServerAliveInterval=30",
                        "-o", "ServerAliveCountMax=3",
                        "-J", grove,
                        &vm_target,
                        &dtach_cmd,
                    ])
                    .status()
                    .context("ssh shell (ProxyJump) failed")?;
                if !status.success() {
                    anyhow::bail!("shell exited with {}", status);
                }
                return Ok(());
            }
        }

        // Non-interactive or IP not available: run via grove
        let remote_cmd = match command {
            Some(cmd) => crate::exec::vitro_hop(name, cmd, session),
            None => {
                let mut s = format!("vitro shell --server {}", name);
                if let Some(sess) = session {
                    s.push_str(&format!(" --session {}", crate::exec::shell_escape(sess)));
                }
                s
            }
        };
        let status = std::process::Command::new("ssh")
            .args([
                "-t", "-A",
                "-o", "StrictHostKeyChecking=no",
                "-o", "UserKnownHostsFile=/dev/null",
                "-o", "ServerAliveInterval=30",
                "-o", "ServerAliveCountMax=3",
                grove,
                &remote_cmd,
            ])
            .status()
            .context("ssh shell failed")?;
        if !status.success() {
            anyhow::bail!("shell exited with {}", status);
        }
        Ok(())
    }

    /// SSH hop with captured output. Mirrors vm::shell_capture for remote envs.
    pub fn shell_capture(&self, name: &str, command: &str) -> Result<crate::vm::CapturedShell> {
        let target = &self.user_host;
        let remote_cmd = crate::exec::vitro_hop(name, command, None);
        let start = std::time::Instant::now();
        let output = std::process::Command::new("ssh")
            .args([
                "-A",
                "-o", "StrictHostKeyChecking=no",
                "-o", "UserKnownHostsFile=/dev/null",
                "-o", "ServerAliveInterval=30",
                "-o", "ServerAliveCountMax=3",
                target,
                &remote_cmd,
            ])
            .output()
            .context("ssh shell failed")?;
        let duration_ms = start.elapsed().as_millis() as u64;
        Ok(crate::vm::CapturedShell {
            exit_code: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            duration_ms,
        })
    }

    /// SSH hop for stdin/stdout-bridged commands. Used by `vitro acp`.
    /// Same shape as `shell` but no PTY (so JSON-RPC framing isn't
    /// corrupted by terminal modes) and no `-t`.
    pub fn acp_forward(&self, name: &str, command: &str) -> Result<()> {
        let target = &self.user_host;
        let remote_cmd = crate::exec::vitro_hop(name, command, None);
        let status = std::process::Command::new("ssh")
            .args([
                "-A",
                "-o", "StrictHostKeyChecking=no",
                "-o", "UserKnownHostsFile=/dev/null",
                "-o", "ServerAliveInterval=30",
                "-o", "ServerAliveCountMax=3",
                target,
                &remote_cmd,
            ])
            .status()
            .context("ssh acp forward failed")?;
        if !status.success() {
            anyhow::bail!("acp forward exited with {}", status);
        }
        Ok(())
    }

    /// Push plaintext secrets env to the host's `/var/lib/vitro/secrets.env`.
    /// Travels over the already-trusted SSH channel; no on-host decryption
    /// key needed.
    pub fn push_secrets(&self, content: &str) -> Result<()> {
        let bytes = content.as_bytes().to_vec();
        self.rt.block_on(async {
            self.session.exec_detached(
                "mkdir -p /var/lib/vitro && cat > /var/lib/vitro/secrets.env && chmod 600 /var/lib/vitro/secrets.env",
                Some(&bytes),
            ).await
        }).context("push_secrets failed")?;
        Ok(())
    }

    /// Push plaintext secrets into the env at `/var/lib/vitro/secrets.env`
    /// so the ACP agent can source them. Uses `vitro shell --server` to hop
    /// from host into the env.
    pub fn push_secrets_env(&self, name: &str, content: &str) -> Result<()> {
        // Encode content as \xHH hex escapes so no shell metacharacters leak.
        let hex: String = content.as_bytes().iter()
            .map(|b| format!("\\x{:02x}", b))
            .collect();
        let env_cmd = format!("printf '{}' > /var/lib/vitro/secrets.env && chmod 600 /var/lib/vitro/secrets.env", hex);
        let host_cmd = crate::exec::vitro_hop(name, &env_cmd, None);
        let (stdout, exit_code) = self.rt.block_on(async {
            self.session.exec(&host_cmd).await
        }).context("push_secrets_env failed")?;
        if exit_code != 0 {
            anyhow::bail!("push_secrets_env exited {exit_code}: {stdout}");
        }
        Ok(())
    }

    /// Sync local files to the env on the server
    #[instrument(skip(self, paths))]
    pub fn sync_files(&self, name: &str, paths: &[String]) -> Result<()> {
        let home = std::env::var("HOME").unwrap_or_default();

        for path in paths {
            let expanded = if path.starts_with("~/") {
                format!("{}/{}", home, &path[2..])
            } else {
                path.clone()
            };

            let local = std::path::Path::new(&expanded);
            if !local.exists() {
                continue;
            }

            let rel = if path.starts_with("~/") {
                &path[2..]
            } else if let Ok(stripped) = local.strip_prefix(&home) {
                stripped.to_str().unwrap_or(path)
            } else {
                continue;
            };

            let dest = format!("/var/lib/vitro/envs/{name}/sync/{rel}");

            self.rt.block_on(async {
                if let Some(parent) = std::path::Path::new(&dest).parent() {
                    self.session.exec(&format!("mkdir -p '{}'", parent.display())).await.ok();
                }

                if local.is_file() {
                    let content = std::fs::read(&expanded).unwrap_or_default();
                    let cmd = format!("cat > '{dest}'");
                    if let Err(e) = self.session.exec_detached(&cmd, Some(&content)).await {
                        warn!(error = %e, path = %expanded, "file sync failed");
                    }
                }
            });
        }

        self.rt.block_on(async {
            self.session.exec(&format!(
                "chown -R 1000:users /var/lib/vitro/envs/{name}/sync/ 2>/dev/null"
            )).await.ok();
        });

        Ok(())
    }
}
