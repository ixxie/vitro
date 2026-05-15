use anyhow::{Context, Result};
use tracing::{warn, instrument};

use crate::config::CellConfig;
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
pub struct CellStatus {
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

    pub fn prepare(&self, name: &str, repo: &str, config: &CellConfig) -> Result<()> {
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

    pub fn up(&self, name: &str, repo: &str, create: bool, config: &CellConfig) -> Result<UpResponse> {
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

    pub fn list(&self) -> Result<Vec<CellStatus>> {
        let resp = self.request("GET", "/list", None)?;
        serde_json::from_str(&resp).context(format!("bad response: {resp}"))
    }

    /// SSH hop for shell or command execution
    pub fn shell(&self, name: &str, command: Option<&str>) -> Result<()> {
        let target = &self.user_host;
        let remote_cmd = match command {
            Some(cmd) => crate::exec::vitro_hop(name, cmd),
            None => format!("vitro shell --server {}", name),
        };
        let status = std::process::Command::new("ssh")
            .args([
                "-t", "-A",
                "-o", "StrictHostKeyChecking=no",
                "-o", "UserKnownHostsFile=/dev/null",
                "-o", "ServerAliveInterval=30",
                "-o", "ServerAliveCountMax=3",
                target,
                &remote_cmd,
            ])
            .status()
            .context("ssh shell failed")?;
        if !status.success() {
            anyhow::bail!("shell exited with {}", status);
        }
        Ok(())
    }

    /// SSH hop with captured output. Mirrors vm::shell_capture for remote cells.
    pub fn shell_capture(&self, name: &str, command: &str) -> Result<crate::vm::CapturedShell> {
        let target = &self.user_host;
        let remote_cmd = crate::exec::vitro_hop(name, command);
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
        let remote_cmd = crate::exec::vitro_hop(name, command);
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

    /// Push plaintext secrets into the cell at `/var/lib/vitro/secrets.env`
    /// so the ACP agent can source them. Uses `vitro shell --server` to hop
    /// from host into the cell.
    pub fn push_secrets_cell(&self, name: &str, content: &str) -> Result<()> {
        let encoded = crate::exec::shell_escape(content);
        let cell_cmd = format!("printf '%s' {} > /var/lib/vitro/secrets.env && chmod 600 /var/lib/vitro/secrets.env", encoded);
        let host_cmd = crate::exec::vitro_hop(name, &cell_cmd);
        let (stdout, exit_code) = self.rt.block_on(async {
            self.session.exec(&host_cmd).await
        }).context("push_secrets_cell failed")?;
        if exit_code != 0 {
            anyhow::bail!("push_secrets_cell exited with code {exit_code}: {stdout}");
        }
        Ok(())
    }

    /// Sync local files to the cell on the server
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

            let dest = format!("/var/lib/vitro/cells/{name}/sync/{rel}");

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
                "chown -R 1000:users /var/lib/vitro/cells/{name}/sync/ 2>/dev/null"
            )).await.ok();
        });

        Ok(())
    }
}
