use anyhow::{Context, Result};
use std::net::SocketAddr;
use std::path::Path;

// Config types (shared with mitmproxy addon via JSON)

#[allow(dead_code)] // fields read by mitmproxy addon via JSON
#[derive(Debug, Clone, serde::Deserialize)]
pub struct ProxyConfig {
    pub envs: Vec<EnvRules>,
    pub egress: EgressConfig,
    #[serde(rename = "httpPort")]
    pub http_port: u16,
    #[serde(rename = "gitCredentialPort")]
    pub git_credential_port: u16,
    #[serde(rename = "controlPort")]
    pub control_port: u16,
    #[serde(rename = "logFile")]
    pub log_file: String,
    #[serde(rename = "bindAddress")]
    pub bind_address: String,
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct EgressConfig {
    pub reads: EgressRules,
    pub writes: EgressRules,
    pub credentials: Vec<CredentialRule>,
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct EgressRules {
    pub methods: Vec<String>,
    pub allowed: serde_json::Value,
    pub denied: serde_json::Value,
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct EnvRules {
    #[serde(rename = "envIp")]
    pub env_ip: String,
    #[serde(rename = "envId")]
    pub env_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub egress: Option<EnvEgress>,
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct EnvEgress {
    #[serde(default)]
    pub additive: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reads: Option<EnvEgressRules>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub writes: Option<EnvEgressRules>,
    /// Per-env credentials. The Python addon reads these alongside
    /// the static global credentials when injecting Authorization /
    /// custom headers on egress. Without this field, serde drops
    /// `credentials` from the registration payload silently and the
    /// proxy emits requests with the placeholder token unchanged.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub credentials: Vec<CredentialRule>,
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct EnvEgressRules {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub denied: Option<Vec<String>>,
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct CredentialRule {
    pub host: String,
    pub header: String,
    #[serde(rename = "envVar")]
    pub env_var: String,
}

const DYNAMIC_ENVS: &str = "/var/lib/vitro/envs.json";

fn load_envs() -> Vec<EnvRules> {
    std::fs::read_to_string(DYNAMIC_ENVS)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_envs(envs: &[EnvRules]) {
    let json = serde_json::to_string_pretty(envs).unwrap_or_default();
    if let Err(e) = std::fs::write(DYNAMIC_ENVS, json) {
        eprintln!("warning: failed to save envs.json: {e}");
    }
}

// Pure helpers — testable without I/O.

pub fn upsert_env(mut envs: Vec<EnvRules>, rule: EnvRules) -> Vec<EnvRules> {
    envs.retain(|e| e.env_ip != rule.env_ip);
    envs.push(rule);
    envs
}

pub fn remove_env(mut envs: Vec<EnvRules>, ip: &str) -> Vec<EnvRules> {
    envs.retain(|e| e.env_ip != ip);
    envs
}

/// Extract the IP from a `DELETE /envs/<ip>` request line.
pub fn parse_delete_envs_path(first_line: &str) -> Option<&str> {
    let path = first_line.split_whitespace().nth(1)?;
    let ip = path.strip_prefix("/envs/")?;
    if ip.is_empty() { None } else { Some(ip) }
}

/// Parse `host=...` out of a git-credential helper request body.
pub fn parse_git_credential_host(req: &str) -> Option<String> {
    let body_start = req.find("\r\n\r\n")?;
    for line in req[body_start + 4..].lines() {
        if let Some((k, v)) = line.split_once('=') {
            if k.trim() == "host" {
                return Some(v.trim().to_string());
            }
        }
    }
    None
}

/// Map a git host to (basic-auth username, env var holding the token).
pub fn git_credential_lookup(host: &str) -> Option<(&'static str, &'static str)> {
    match host {
        "github.com" => Some(("x-access-token", "GITHUB_TOKEN")),
        "gitlab.com" => Some(("oauth2", "GITLAB_TOKEN")),
        "bitbucket.org" => Some(("x-token-auth", "BITBUCKET_TOKEN")),
        _ => None,
    }
}

/// Strip the IPv4-mapped IPv6 prefix from a peer-address string,
/// matching `vitro_policy.normalize_client_ip` in the addon.
pub fn normalize_peer_ip(addr: SocketAddr) -> String {
    let ip = match addr {
        SocketAddr::V4(v4) => v4.ip().to_string(),
        SocketAddr::V6(v6) => {
            let ip = *v6.ip();
            if let Some(v4) = ip.to_ipv4_mapped() {
                v4.to_string()
            } else {
                ip.to_string()
            }
        }
    };
    ip.strip_prefix("::ffff:").map(str::to_string).unwrap_or(ip)
}

/// True iff `ip` is the `envIp` of a currently-registered env in
/// `/var/lib/vitro/envs.json`. Reads lazily — the file is small and
/// changes on every env up/down.
fn is_registered_env_ip(ip: &str) -> bool {
    let content = match std::fs::read_to_string(DYNAMIC_ENVS) {
        Ok(c) => c,
        Err(_) => return false,
    };
    let envs: Vec<serde_json::Value> = match serde_json::from_str(&content) {
        Ok(c) => c,
        Err(_) => return false,
    };
    envs.iter().any(|e| e.get("envIp").and_then(|v| v.as_str()) == Some(ip))
}

// Git credential handler (simple TCP)

async fn serve_git_credentials(listener: tokio::net::TcpListener) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    loop {
        let (mut stream, peer) = match listener.accept().await {
            Ok(s) => s,
            Err(e) => { eprintln!("git accept error: {e}"); continue; }
        };

        tokio::spawn(async move {
            let peer_ip = normalize_peer_ip(peer);
            if !is_registered_env_ip(&peer_ip) {
                eprintln!("git credential: denied unknown peer {peer_ip}");
                let _ = stream.write_all(b"HTTP/1.1 403 Forbidden\r\n\r\n").await;
                return;
            }

            let mut buf = vec![0u8; 4096];
            let n = match stream.read(&mut buf).await {
                Ok(n) => n,
                Err(_) => return,
            };
            let body = String::from_utf8_lossy(&buf[..n]);

            if !body.contains("POST") || !body.contains("/git-credential") {
                let _ = stream.write_all(b"HTTP/1.1 404 Not Found\r\n\r\n").await;
                return;
            }

            let host = parse_git_credential_host(&body).unwrap_or_default();
            let (username, env_var) = match git_credential_lookup(&host) {
                Some(p) => p,
                None => {
                    let _ = stream.write_all(b"HTTP/1.1 403 Forbidden\r\n\r\n").await;
                    return;
                }
            };

            let token = match std::env::var(env_var) {
                Ok(t) => t,
                Err(_) => {
                    let _ = stream.write_all(b"HTTP/1.1 500 Error\r\n\r\n").await;
                    return;
                }
            };

            let response_body = format!("username={username}\npassword={token}\n");
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\ncontent-length: {}\r\n\r\n{response_body}",
                response_body.len(),
            );
            let _ = stream.write_all(response.as_bytes()).await;
        });
    }
}

// VM lifecycle API types

#[allow(dead_code)] // fields consumed by serde
#[derive(serde::Deserialize)]
struct UpRequest {
    name: String,
    #[serde(default)]
    repo: Option<String>,
    #[serde(default)]
    create: bool,
    #[serde(default)]
    config: crate::config::EnvConfig,
}

#[derive(serde::Serialize)]
struct UpResponse {
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    ip: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(serde::Deserialize)]
struct NameRequest {
    name: String,
}

#[derive(serde::Serialize)]
struct EnvStatus {
    name: String,
    status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    ip: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    repo: Option<String>,
}

async fn handle_prepare(req: &str) -> (&'static str, String) {
    let body = match req.find("\r\n\r\n") {
        Some(i) => &req[i + 4..],
        None => return ("400 Bad Request", "missing body".to_string()),
    };
    let up: UpRequest = match serde_json::from_str(body) {
        Ok(u) => u,
        Err(e) => return ("400 Bad Request", format!("bad request: {e}")),
    };

    let result = tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        crate::git::init_clone_server(&up.name, &up.config)?;

        // Auto soft-reload: if the env is already running, re-push its
        // egress/credentials so config edits land without a full restart.
        if crate::vm::microvm_unit_active(&up.name) {
            let rt = crate::vm::runtime_dir(&up.name);
            if let Ok(ip) = std::fs::read_to_string(rt.join("ip")) {
                let ip = ip.trim();
                if !ip.is_empty() {
                    crate::vm::register_proxy_rules(ip, &up.name, &up.config)?;
                }
            }
        }
        Ok(())
    }).await;

    match result {
        Ok(Ok(())) => ("200 OK", r#"{"ok":true}"#.to_string()),
        Ok(Err(e)) => {
            let resp = format!("{{\"ok\":false,\"error\":\"{}\"}}", e);
            ("500 Internal Server Error", resp)
        }
        Err(e) => ("500 Internal Server Error", format!("{{\"ok\":false,\"error\":\"{e}\"}}")),
    }
}

async fn handle_up(req: &str) -> (&'static str, String) {
    let body = match req.find("\r\n\r\n") {
        Some(i) => &req[i + 4..],
        None => return ("400 Bad Request", "missing body".to_string()),
    };
    let up: UpRequest = match serde_json::from_str(body) {
        Ok(u) => u,
        Err(e) => return ("400 Bad Request", format!("bad request: {e}")),
    };

    let result = tokio::task::spawn_blocking(move || -> anyhow::Result<String> {
        crate::git::init_clone_server(&up.name, &up.config)?;

        // Secrets decryption happens client-side now (the laptop holds
        // the recipient key). The client pushes plaintext into
        // /var/lib/vitro/secrets.env via SSH before calling /up; the
        // host never needs to be a recipient of secrets.age.

        if !crate::vm::is_running(&up.name)? {
            let repo_name = up.repo.as_deref().unwrap_or("unknown");
            crate::vm::start(&up.name, repo_name, &up.config)?;
        }
        let rt = crate::vm::runtime_dir(&up.name);
        let ip = std::fs::read_to_string(rt.join("ip")).unwrap_or_default().trim().to_string();
        Ok(ip)
    }).await;

    match result {
        Ok(Ok(ip)) => {
            let resp = UpResponse { ok: true, ip: Some(ip), error: None };
            ("200 OK", serde_json::to_string(&resp).unwrap())
        }
        Ok(Err(e)) => {
            let resp = UpResponse { ok: false, ip: None, error: Some(e.to_string()) };
            ("500 Internal Server Error", serde_json::to_string(&resp).unwrap())
        }
        Err(e) => ("500 Internal Server Error", format!("{{\"ok\":false,\"error\":\"{e}\"}}")),
    }
}

async fn handle_down(req: &str) -> (&'static str, String) {
    let body = match req.find("\r\n\r\n") {
        Some(i) => &req[i + 4..],
        None => return ("400 Bad Request", "missing body".to_string()),
    };
    let nr: NameRequest = match serde_json::from_str(body) {
        Ok(n) => n,
        Err(e) => return ("400 Bad Request", format!("bad request: {e}")),
    };

    let result = tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        if crate::vm::is_running(&nr.name)? {
            crate::vm::stop(&nr.name)?;
        }
        Ok(())
    }).await;

    match result {
        Ok(Ok(())) => ("200 OK", r#"{"ok":true}"#.to_string()),
        Ok(Err(e)) => ("500 Internal Server Error", format!("{{\"ok\":false,\"error\":\"{e}\"}}")),
        Err(e) => ("500 Internal Server Error", format!("{{\"ok\":false,\"error\":\"{e}\"}}")),
    }
}

async fn handle_delete(req: &str) -> (&'static str, String) {
    let body = match req.find("\r\n\r\n") {
        Some(i) => &req[i + 4..],
        None => return ("400 Bad Request", "missing body".to_string()),
    };
    let nr: NameRequest = match serde_json::from_str(body) {
        Ok(n) => n,
        Err(e) => return ("400 Bad Request", format!("bad request: {e}")),
    };

    let result = tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        crate::vm::delete(&nr.name)?;
        Ok(())
    }).await;

    match result {
        Ok(Ok(())) => ("200 OK", r#"{"ok":true}"#.to_string()),
        Ok(Err(e)) => ("500 Internal Server Error", format!("{{\"ok\":false,\"error\":\"{e}\"}}")),
        Err(e) => ("500 Internal Server Error", format!("{{\"ok\":false,\"error\":\"{e}\"}}")),
    }
}

async fn handle_list() -> (&'static str, String) {
    let result = tokio::task::spawn_blocking(|| -> anyhow::Result<Vec<EnvStatus>> {
        let names = crate::vm::list_envs()?;
        let mut envs = Vec::new();
        for name in names {
            let running = crate::vm::is_running(&name).unwrap_or(false);
            let rt = crate::vm::runtime_dir(&name);
            let ip = if running {
                std::fs::read_to_string(rt.join("ip")).ok().map(|s| s.trim().to_string())
            } else {
                None
            };
            let repo = std::fs::read_to_string(rt.join("repo"))
                .ok().map(|s| s.trim().to_string());
            envs.push(EnvStatus {
                name,
                status: if running { "running" } else { "stopped" }.to_string(),
                ip,
                repo,
            });
        }
        Ok(envs)
    }).await;

    match result {
        Ok(Ok(envs)) => ("200 OK", serde_json::to_string(&envs).unwrap()),
        Ok(Err(e)) => ("500 Internal Server Error", format!("{{\"error\":\"{e}\"}}")),
        Err(e) => ("500 Internal Server Error", format!("{{\"error\":\"{e}\"}}")),
    }
}

// Control API — manages dynamic env registrations + VM lifecycle

async fn serve_control_api(listener: tokio::net::TcpListener) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    loop {
        let (mut stream, _) = match listener.accept().await {
            Ok(s) => s,
            Err(e) => { eprintln!("control accept error: {e}"); continue; }
        };

        tokio::spawn(async move {
            let mut buf = vec![0u8; 8192];
            let n = match stream.read(&mut buf).await {
                Ok(n) => n,
                Err(_) => return,
            };
            let req = String::from_utf8_lossy(&buf[..n]).to_string();

            let (status, body) = if req.starts_with("POST /envs") {
                if let Some(body_start) = req.find("\r\n\r\n") {
                    let json = &req[body_start + 4..];
                    match serde_json::from_str::<EnvRules>(json) {
                        Ok(rules) => {
                            let ip = rules.env_ip.clone();
                            let env_id = rules.env_id.clone();
                            let envs = upsert_env(load_envs(), rules);
                            save_envs(&envs);
                            eprintln!("registered env {ip} (id: {env_id})");
                            ("200 OK", "ok".to_string())
                        }
                        Err(e) => ("400 Bad Request", format!("bad request: {e}")),
                    }
                } else {
                    ("400 Bad Request", "missing body".to_string())
                }
            } else if req.starts_with("DELETE /envs/") {
                let first_line = req.lines().next().unwrap_or("");
                match parse_delete_envs_path(first_line) {
                    Some(ip) => {
                        let envs = remove_env(load_envs(), ip);
                        save_envs(&envs);
                        eprintln!("deregistered env {ip}");
                        ("200 OK", "ok".to_string())
                    }
                    None => ("400 Bad Request", "missing ip".to_string()),
                }
            } else if req.starts_with("GET /envs") {
                let envs = load_envs();
                let json = serde_json::to_string(&envs).unwrap_or_default();
                ("200 OK", json)
            } else if req.starts_with("POST /prepare") {
                handle_prepare(&req).await
            } else if req.starts_with("POST /up") {
                handle_up(&req).await
            } else if req.starts_with("POST /down") {
                handle_down(&req).await
            } else if req.starts_with("POST /delete") {
                handle_delete(&req).await
            } else if req.starts_with("GET /list") {
                handle_list().await
            } else {
                ("404 Not Found", "not found".to_string())
            };

            let response = format!(
                "HTTP/1.1 {status}\r\ncontent-length: {}\r\n\r\n{body}",
                body.len(),
            );
            let _ = stream.write_all(response.as_bytes()).await;
        });
    }
}

// Main entry point

pub async fn run(config_path: &str) -> Result<()> {
    let config: ProxyConfig = {
        let content = std::fs::read_to_string(config_path)
            .context("reading proxy config")?;
        serde_json::from_str(&content)
            .context("parsing proxy config")?
    };

    // `parent()` returns None only for root-like paths; in those cases
    // there's no directory to create. create_dir_all is best-effort, so
    // silently skip when there's no parent.
    if let Some(log_dir) = Path::new(&config.log_file).parent() {
        std::fs::create_dir_all(log_dir).ok();
    }

    save_envs(&config.envs);

    let git_addr: SocketAddr = format!("{}:{}", config.bind_address, config.git_credential_port).parse()?;
    let git_listener = tokio::net::TcpListener::bind(git_addr).await?;
    eprintln!("Git credential server listening on {git_addr}");
    tokio::spawn(serve_git_credentials(git_listener));

    let ctrl_local: SocketAddr = format!("127.0.0.1:{}", config.control_port).parse()?;
    let ctrl_bridge: SocketAddr = format!("{}:{}", config.bind_address, config.control_port).parse()?;
    let ctrl_listener_local = tokio::net::TcpListener::bind(ctrl_local).await?;
    tokio::spawn(serve_control_api(ctrl_listener_local));
    if ctrl_bridge != ctrl_local {
        let ctrl_listener_bridge = tokio::net::TcpListener::bind(ctrl_bridge).await?;
        tokio::spawn(serve_control_api(ctrl_listener_bridge));
        eprintln!("Control API listening on {ctrl_local} + {ctrl_bridge}");
    } else {
        eprintln!("Control API listening on {ctrl_local}");
    }

    eprintln!("Vitro services started");
    eprintln!("  Git credentials: {git_addr}");

    tokio::signal::ctrl_c().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(ip: &str, env_id: &str) -> EnvRules {
        EnvRules {
            env_ip: ip.to_string(),
            env_id: env_id.to_string(),
            egress: None,
        }
    }

    #[test]
    fn upsert_adds_new() {
        let envs = upsert_env(vec![], rule("10.0.0.5", "feat"));
        assert_eq!(envs.len(), 1);
        assert_eq!(envs[0].env_ip, "10.0.0.5");
    }

    #[test]
    fn upsert_replaces_existing_by_ip() {
        let envs = vec![rule("10.0.0.5", "old")];
        let envs = upsert_env(envs, rule("10.0.0.5", "new"));
        assert_eq!(envs.len(), 1);
        assert_eq!(envs[0].env_id, "new");
    }

    #[test]
    fn upsert_keeps_other_envs() {
        let envs = vec![rule("10.0.0.5", "a"), rule("10.0.0.6", "b")];
        let envs = upsert_env(envs, rule("10.0.0.5", "a-new"));
        assert_eq!(envs.len(), 2);
        let by_ip: std::collections::HashMap<_, _> =
            envs.iter().map(|e| (e.env_ip.clone(), e.env_id.clone())).collect();
        assert_eq!(by_ip["10.0.0.5"], "a-new");
        assert_eq!(by_ip["10.0.0.6"], "b");
    }

    #[test]
    fn remove_strips_matching_ip() {
        let envs = vec![rule("10.0.0.5", "a"), rule("10.0.0.6", "b")];
        let envs = remove_env(envs, "10.0.0.5");
        assert_eq!(envs.len(), 1);
        assert_eq!(envs[0].env_ip, "10.0.0.6");
    }

    #[test]
    fn remove_unknown_ip_is_noop() {
        let envs = vec![rule("10.0.0.5", "a")];
        let envs = remove_env(envs, "10.0.0.99");
        assert_eq!(envs.len(), 1);
    }

    #[test]
    fn parse_delete_path_basic() {
        assert_eq!(
            parse_delete_envs_path("DELETE /envs/10.0.0.5 HTTP/1.1"),
            Some("10.0.0.5"),
        );
    }

    #[test]
    fn parse_delete_path_missing_ip_rejected() {
        assert_eq!(parse_delete_envs_path("DELETE /envs/ HTTP/1.1"), None);
    }

    #[test]
    fn parse_delete_path_garbage_rejected() {
        assert_eq!(parse_delete_envs_path(""), None);
        assert_eq!(parse_delete_envs_path("GET / HTTP/1.1"), None);
    }

    #[test]
    fn parse_git_credential_extracts_host() {
        let req = "POST /git-credential HTTP/1.1\r\ncontent-type: text/plain\r\n\r\nprotocol=https\nhost=github.com\n";
        assert_eq!(parse_git_credential_host(req).as_deref(), Some("github.com"));
    }

    #[test]
    fn parse_git_credential_no_body_returns_none() {
        let req = "POST /git-credential HTTP/1.1\r\nNo-Body: yes";
        assert!(parse_git_credential_host(req).is_none());
    }

    #[test]
    fn parse_git_credential_no_host_returns_none() {
        let req = "POST /git-credential HTTP/1.1\r\n\r\nprotocol=https\n";
        assert!(parse_git_credential_host(req).is_none());
    }

    #[test]
    fn git_credential_lookup_known_hosts() {
        assert_eq!(git_credential_lookup("github.com"), Some(("x-access-token", "GITHUB_TOKEN")));
        assert_eq!(git_credential_lookup("gitlab.com"), Some(("oauth2", "GITLAB_TOKEN")));
        assert_eq!(git_credential_lookup("bitbucket.org"), Some(("x-token-auth", "BITBUCKET_TOKEN")));
    }

    #[test]
    fn git_credential_lookup_unknown_returns_none() {
        assert!(git_credential_lookup("evil.com").is_none());
        assert!(git_credential_lookup("").is_none());
        // important: substring of a known host must not match
        assert!(git_credential_lookup("evil.github.com").is_none());
    }

    #[test]
    fn env_rules_json_roundtrip_minimal() {
        let r = rule("10.0.0.5", "feat");
        let json = serde_json::to_string(&r).unwrap();
        // shape the addon expects
        assert!(json.contains("\"envIp\":\"10.0.0.5\""));
        assert!(json.contains("\"envId\":\"feat\""));
        let back: EnvRules = serde_json::from_str(&json).unwrap();
        assert_eq!(back.env_ip, "10.0.0.5");
        assert_eq!(back.env_id, "feat");
    }

    #[test]
    fn env_rules_json_roundtrip_with_egress() {
        let json = r#"{"envIp":"10.0.0.5","envId":"feat","egress":{"additive":true,"writes":{"allowed":["api.x.com"]}}}"#;
        let r: EnvRules = serde_json::from_str(json).unwrap();
        let eg = r.egress.unwrap();
        assert!(eg.additive);
        let writes = eg.writes.unwrap();
        assert_eq!(writes.allowed.unwrap(), vec!["api.x.com"]);
    }
}
