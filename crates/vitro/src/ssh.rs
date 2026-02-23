use anyhow::{Context, Result};
use russh::*;
use russh::keys::ssh_key;
use std::sync::Arc;

/// SSH client handler (required by russh)
pub(crate) struct Handler;

impl client::Handler for Handler {
    type Error = anyhow::Error;

    fn check_server_key(
        &mut self,
        _server_public_key: &ssh_key::PublicKey,
    ) -> impl std::future::Future<Output = std::result::Result<bool, Self::Error>> + Send {
        async { Ok(true) }
    }
}

/// A connected SSH session
pub struct Session {
    pub(crate) handle: client::Handle<Handler>,
}

impl Session {
    /// Connect and authenticate using key files.
    pub async fn connect(user_host: &str) -> Result<Self> {
        let (user, host) = parse_user_host(user_host)?;

        let config = client::Config {
            inactivity_timeout: Some(std::time::Duration::from_secs(300)),
            keepalive_interval: Some(std::time::Duration::from_secs(30)),
            keepalive_max: 3,
            ..Default::default()
        };

        let mut handle = tokio::time::timeout(
            std::time::Duration::from_secs(15),
            client::connect(Arc::new(config), (host.as_str(), 22u16), Handler),
        )
        .await
        .context("SSH connection timed out")?
        .context("SSH connection failed")?;

        let home = std::env::var("HOME")
            .unwrap_or_else(|_| "/root".to_string());
        let mut key_paths = vec![
            format!("{home}/.ssh/id_ed25519"),
            format!("{home}/.ssh/id_rsa"),
        ];
        // server-side key (systemd services may not have HOME set)
        key_paths.push("/var/lib/vitro/ssh/id_ed25519".to_string());
        for path in &key_paths {
            if let Ok(key) = keys::load_secret_key(path, None) {
                let key = keys::key::PrivateKeyWithHashAlg::new(Arc::new(key), None);
                if let Ok(client::AuthResult::Success) = handle.authenticate_publickey(&user, key).await {
                    return Ok(Self { handle });
                }
            }
        }

        anyhow::bail!("SSH authentication failed for {user_host}")
    }

    /// Execute a command and return (stdout, exit_code).
    pub async fn exec(&self, command: &str) -> Result<(String, u32)> {
        let mut channel = self.handle.channel_open_session().await?;
        channel.exec(true, command).await?;

        let mut stdout = Vec::new();
        let mut exit_code = 0u32;

        while let Some(msg) = channel.wait().await {
            match msg {
                ChannelMsg::Data { ref data } => {
                    stdout.extend_from_slice(data);
                }
                ChannelMsg::ExitStatus { exit_status } => {
                    exit_code = exit_status;
                }
                _ => {}
            }
        }

        Ok((String::from_utf8_lossy(&stdout).to_string(), exit_code))
    }

    /// Execute a command with stdin, fire-and-forget.
    pub async fn exec_detached(&self, command: &str, stdin_data: Option<&[u8]>) -> Result<()> {
        let channel = self.handle.channel_open_session().await?;
        channel.exec(true, command).await?;
        if let Some(data) = stdin_data {
            channel.data(data).await?;
            channel.eof().await?;
        }
        Ok(())
    }

    /// Make an HTTP request through a direct-tcpip channel.
    pub async fn http_request(
        &self,
        host: &str,
        port: u32,
        method: &str,
        path: &str,
        body: Option<&str>,
    ) -> Result<String> {
        let mut channel = self.handle
            .channel_open_direct_tcpip(host, port, "127.0.0.1", 0)
            .await
            .context("failed to open direct-tcpip channel")?;

        let body_bytes = body.unwrap_or("");
        let req = format!(
            "{method} {path} HTTP/1.0\r\nHost: localhost\r\nContent-Length: {}\r\n\r\n{body_bytes}",
            body_bytes.len(),
        );
        channel.data(req.as_bytes()).await?;
        channel.eof().await?;

        let mut response = Vec::new();
        loop {
            tokio::select! {
                msg = channel.wait() => {
                    match msg {
                        Some(ChannelMsg::Data { ref data }) => {
                            response.extend_from_slice(data);
                        }
                        Some(ChannelMsg::Eof) | None => break,
                        _ => {}
                    }
                }
                _ = tokio::time::sleep(std::time::Duration::from_secs(300)) => {
                    anyhow::bail!("control API request timed out");
                }
            }
        }
        let response = String::from_utf8_lossy(&response).to_string();

        if let Some(i) = response.find("\r\n\r\n") {
            Ok(response[i + 4..].to_string())
        } else {
            Ok(response)
        }
    }
}

fn parse_user_host(user_host: &str) -> Result<(String, String)> {
    if let Some((user, host)) = user_host.split_once('@') {
        Ok((user.to_string(), host.to_string()))
    } else {
        anyhow::bail!("invalid SSH target: expected user@host, got '{user_host}'")
    }
}
