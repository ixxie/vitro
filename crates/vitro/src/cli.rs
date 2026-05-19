use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};
use console::{style, Style};

use tracing::instrument;
use crate::{client, config, deploy, env_state, git, proxy, secrets, server, server_init, transport, vm};
use crate::transport::Transport;

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn parse_duration(s: &str) -> Option<u64> {
    if let Ok(n) = s.parse::<u64>() {
        return Some(n);
    }
    let (num_str, suffix) = if s.ends_with('d') {
        (&s[..s.len()-1], 'd')
    } else if s.ends_with('m') {
        (&s[..s.len()-1], 'm')
    } else if s.ends_with('h') {
        (&s[..s.len()-1], 'h')
    } else if s.ends_with('s') {
        (&s[..s.len()-1], 's')
    } else {
        return None;
    };
    let n: u64 = num_str.parse().ok()?;
    match suffix {
        's' => Some(n),
        'm' => Some(n * 60),
        'h' => Some(n * 3600),
        'd' => Some(n * 86400),
        _ => None,
    }
}

#[derive(Parser)]
#[command(
    name = "vitro",
    version,
    about = "Sandboxed development environments",
    after_help = "Run 'vitro <command> --help' for details on each command."
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Scaffold .vitro/ for a repo
    Init,
    /// Create a new env
    Create(CreateArgs),
    /// Remove an env
    Remove(RemoveArgs),
    /// List envs
    List {
        /// Emit machine-readable JSON
        #[arg(long)]
        json: bool,
    },
    /// Stop an env (data preserved)
    Stop(StopArgs),
    /// Show env status
    Status {
        /// Env name (omit for all)
        name: Option<String>,
        /// Emit machine-readable JSON
        #[arg(long)]
        json: bool,
    },
    /// SSH into an env
    Shell(ShellArgs),
    /// Forward declared ports from a remote env to localhost
    Tunnel(TunnelArgs),
    /// Manage encrypted secrets
    Secrets(SecretsArgs),
    /// Manage servers
    Server(ServerArgs),
    /// Manage agent sessions (send input, tail logs)
    Session(SessionArgs),
    #[command(hide = true)]
    /// Internal utilities
    Util(UtilArgs),
}

#[derive(Args, Debug)]
struct CreateArgs {
    /// Env name to create
    name: String,
    /// Server to use
    #[arg(short, long)]
    server: Option<String>,
}

#[derive(Args, Debug)]
struct RemoveArgs {
    /// Env name
    name: String,
}

#[derive(Args, Debug)]
struct StopArgs {
    /// Env name
    name: String,
}

#[derive(Args)]
struct ShellArgs {
    /// Env name
    name: String,
    /// Run a command instead of interactive shell
    #[arg(short = 'c', long = "command")]
    command: Option<String>,
    /// Emit JSON {exit_code, stdout, stderr, duration_ms} (only with -c)
    #[arg(long)]
    json: bool,
    /// Server-side mode (skip repo checks)
    #[arg(long, hide = true)]
    server: bool,
    /// Named dtach session (default: "default" for interactive shells)
    #[arg(long)]
    session: Option<String>,
    /// List active dtach sessions for this env
    #[arg(short = 'l', long = "list-sessions")]
    list_sessions: bool,
    /// Kill a dtach session (and its process)
    #[arg(short = 'k', long = "kill-session")]
    kill_session: Option<String>,
}

#[derive(Args)]
struct TunnelArgs {
    /// Env name
    name: String,
    /// Ports to forward, supports ranges (e.g. -p 5173 -p 8001-8004)
    #[arg(short, long)]
    port: Vec<String>,
    /// Open in default browser
    #[arg(short, long)]
    open: bool,
}

#[derive(Args)]
struct ServerArgs {
    #[command(subcommand)]
    action: ServerAction,
}

#[derive(Subcommand)]
enum ServerAction {
    /// List declared servers (read-only — declare via vitro.client.servers in NixOS, or edit ~/.config/vitro/servers.toml)
    List,
    /// Scaffold a new server: probes target via SSH, writes disk.nix/config.nix, registers in ~/.config/vitro/servers.toml
    Init {
        /// Server name (becomes nixosConfigurations.<name>)
        name: String,
        /// SSH target — user@host
        target: String,
        /// Path to SSH private key (default: ~/.ssh/id_ed25519); the .pub sibling is used as the server's authorized key
        #[arg(long)]
        ssh_key: Option<std::path::PathBuf>,
    },
    /// Deploy server config (defaults to current directory)
    Deploy {
        /// Path to host config flake
        host_dir: Option<std::path::PathBuf>,
        /// Activate at next boot instead of switching live (use when
        /// switch refuses, e.g. on dbus implementation changes)
        #[arg(long)]
        boot: bool,
        /// After a successful `boot`-mode deploy, reboot the target
        #[arg(long, requires = "boot")]
        reboot: bool,
    },
    /// Garbage collect stopped envs older than a threshold
    Gc(GcArgs),
    #[command(hide = true)]
    /// Run the network proxy (used by systemd)
    Proxy(ProxyArgs),
}

#[derive(Args, Debug)]
struct GcArgs {
    /// Delete envs stopped longer than this (e.g. "7d", "24h", "1h")
    #[arg(long, default_value = "7d")]
    older_than: String,
    /// Dry run — show what would be deleted without deleting
    #[arg(long)]
    dry_run: bool,
}

#[derive(Args)]
struct ProxyArgs {
    /// Path to proxy config JSON
    #[arg(short, long, default_value = "/etc/vitro/proxy-config.json")]
    config: String,
}

#[derive(Args)]
struct SecretsArgs {
    #[command(subcommand)]
    action: SecretsAction,
}

#[derive(Subcommand)]
enum SecretsAction {
    /// Encrypt .vitro/secrets.env → .vitro/secrets.age
    Encrypt,
    /// Decrypt, open in $EDITOR, re-encrypt on save
    Edit,
    /// Decrypt .vitro/secrets.age → .vitro/secrets.env
    Decrypt,
}

#[derive(Args)]
struct SessionArgs {
    #[command(subcommand)]
    action: SessionAction,
}

#[derive(Subcommand)]
enum SessionAction {
    /// Send text input to a running session
    Send {
        /// Env name
        env: String,
        /// Text to send (escape sequences like \\n are interpreted)
        text: String,
        /// Session name (default: "default")
        #[arg(long, default_value = "default")]
        session: String,
    },
    /// Tail the session log for an env
    Log {
        /// Env name
        env: String,
        /// Session name (default: "default")
        #[arg(long, default_value = "default")]
        session: String,
        /// Follow (tail -f)
        #[arg(short, long)]
        follow: bool,
    },
}

#[derive(Args)]
struct UtilArgs {
    #[command(subcommand)]
    action: UtilAction,
}

#[derive(Subcommand)]
enum UtilAction {
    /// Print the env repo path for the current VM (used by git-remote-vitro)
    ResolveEnv {
        /// Match envs for this repo name
        #[arg(long)]
        repo: Option<String>,
    },
    /// Manage /etc/hosts entries for tunnels
    Hosts {
        #[command(subcommand)]
        action: HostsAction,
    },
}

#[derive(Subcommand)]
enum HostsAction {
    /// Add a tunnel entry to /etc/hosts
    Add {
        /// IP address
        ip: String,
        /// Hostname
        hostname: String,
    },
    /// Remove a tunnel entry from /etc/hosts
    Remove {
        /// Hostname to remove
        hostname: String,
    },
}

// Styles

fn ok() -> console::StyledObject<&'static str> { style("✓").green() }
fn dn() -> console::StyledObject<&'static str> { style("▼").red() }
fn rm() -> console::StyledObject<&'static str> { style("✕").red() }
fn arrow() -> console::StyledObject<&'static str> { style("→").cyan() }
fn add() -> console::StyledObject<&'static str> { style("+").green() }

fn dim(s: &str) -> console::StyledObject<&str> { style(s).dim() }
fn bold(s: &str) -> console::StyledObject<&str> { style(s).bold() }

fn vm_status(s: &str) -> String {
    match s {
        "running" => style(s).green().to_string(),
        "stopped" => style(s).red().to_string(),
        _ => style(s).yellow().to_string(),
    }
}

// Dispatch

pub fn run() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Server(args) => match args.action {
            ServerAction::Proxy(proxy_args) => {
                let rt = tokio::runtime::Runtime::new()?;
                rt.block_on(proxy::run(&proxy_args.config))
            }
            ServerAction::List => cmd_server_list(),
            ServerAction::Init { name, target, ssh_key } => {
                server_init::run(&name, &target, ssh_key.as_deref())
            }
            ServerAction::Deploy { host_dir, boot, reboot } => cmd_deploy(host_dir, boot, reboot),
            ServerAction::Gc(args) => cmd_gc(args),
        },
        Commands::Util(args) => match args.action {
            UtilAction::ResolveEnv { repo } => cmd_resolve_env(repo.as_deref()),
            UtilAction::Hosts { action } => cmd_hosts(action),
        },

        Commands::Init => {
            let repo = git::Repo::open()?;
            cmd_init(&repo)
        }
        Commands::Create(args) => {
            let repo = git::Repo::open()?;
            cmd_create(&repo, args)
        }
        Commands::Remove(args) => {
            let repo = git::Repo::open()?;
            cmd_remove(&repo, args)
        }
        Commands::List { json } => cmd_list(git::Repo::open().ok().as_ref(), json),
        Commands::Status { name, json } => {
            let repo = git::Repo::open()?;
            cmd_status(&repo, name.as_deref(), json)
        }
        Commands::Secrets(args) => {
            let repo = git::Repo::open()?;
            cmd_secrets(&repo, args)
        }

        Commands::Shell(args) if args.server => {
            vm::shell(&args.name, args.command.as_deref(), args.session.as_deref())
        }
        Commands::Stop(args) => {
            let repo = git::Repo::open()?;
            cmd_stop(&repo, &args.name)
        }
        Commands::Shell(args) => {
            let repo = git::Repo::open()?;
            cmd_shell(&repo, args)
        }
        Commands::Tunnel(args) => {
            let repo = git::Repo::open()?;
            cmd_tunnel(&repo, args)
        }
        Commands::Session(args) => {
            let repo = git::Repo::open()?;
            cmd_session(&repo, args)
        }
    }
}

// Helpers

/// Find which server hosts an env. Resolution order:
///   1. recorded binding from `vitro create --server X` (laptop-local)
///   2. env exists locally (vm::list_envs)
///   3. env exists on a registered remote server (running OR stopped)
///   4. repo or client config default
///   5. error (no silent localhost fallback)
fn find_env_server(repo: &git::Repo, env: &str) -> Result<server::ActiveServer> {
    if let Some(srv) = env_state::get_server(repo.root(), env) {
        return server::resolve(&srv);
    }

    if vm::list_envs().unwrap_or_default().iter().any(|e| e == env) {
        return Ok(server::ActiveServer::Localhost);
    }

    for (name, target) in server::list()? {
        if let Ok(c) = client::Client::connect(&target) {
            let envs = c.list().unwrap_or_default();
            if envs.iter().any(|e| e.name == env) {
                return Ok(server::ActiveServer::Remote { name });
            }
        }
    }

    // fall back to config defaults
    let cfg = config::load(repo.root())?;
    let client_cfg = server::load_client_config();
    if let Some(srv) = cfg.server.as_ref().or(client_cfg.server.as_ref()) {
        return server::resolve(srv);
    }

    anyhow::bail!(
        "no server bound for env '{env}' — pass --server, set [server] in .vitro/config.toml, \
         or recreate with `vitro create {env} --server <name>`"
    )
}

fn connect_remote(s: &server::ActiveServer) -> Result<client::Client> {
    let target = s.target()?
        .ok_or_else(|| anyhow::anyhow!("server has no target"))?;
    client::Client::connect(&target)
}

fn make_transport(active: &server::ActiveServer) -> Result<Box<dyn Transport>> {
    if active.is_server() {
        let c = connect_remote(active)?;
        Ok(Box::new(transport::RemoteTransport { client: c }))
    } else {
        Ok(Box::new(transport::LocalTransport))
    }
}

// Init

fn cmd_init(repo: &git::Repo) -> Result<()> {
    let vitro_dir = repo.root().join(".vitro");
    std::fs::create_dir_all(&vitro_dir)?;

    let config_path = vitro_dir.join("config.toml");
    if !config_path.exists() {
        std::fs::write(&config_path, "memory = \"2G\"\nvcpu = 2\n")?;
        println!("  {} .vitro/config.toml", add());
    }

    repo.add_vitro_remote("vitro://localhost")?;
    println!("{} initialized vitro", ok());
    Ok(())
}

// Env lifecycle commands

#[instrument(skip(repo), fields(env = %args.name))]
fn cmd_create(repo: &git::Repo, args: CreateArgs) -> Result<()> {
    let name = &args.name;

    if let Some(srv) = args.server.as_deref() {
        env_state::set_server(repo.root(), name, srv)?;
        println!("  {} server {}", add(), bold(srv));
    }

    let active = find_env_server(repo, name)?;
    let cfg = config::load(repo.root())?;
    let t = make_transport(&active)?;
    t.ensure_running(name, repo, &cfg)?;

    println!("{} created {}", ok(), bold(name));
    Ok(())
}

#[instrument(skip(repo), fields(env = %args.name))]
fn cmd_remove(repo: &git::Repo, args: RemoveArgs) -> Result<()> {
    let name = &args.name;

    // stop and delete env if found
    if let Ok(active) = find_env_server(repo, name) {
        if active.is_server() {
            let c = connect_remote(&active)?;
            c.delete(name).ok();
        } else if vm::is_running(name).unwrap_or(false) {
            vm::stop(name)?;
            vm::delete(name).ok();
        } else {
            vm::delete(name).ok();
        }
    }

    env_state::clear(repo.root(), name).ok();

    println!("{} removed {}", ok(), bold(name));
    Ok(())
}

// Server commands

fn cmd_server_list() -> Result<()> {
    let servers = server::list()?;
    let hdr = Style::new().bold();
    println!("  {:<20} {}", hdr.apply_to("SERVER"), hdr.apply_to("TARGET"));
    println!("  {:<20} {}", "localhost", dim("—"));
    for (name, target) in &servers {
        println!("  {:<20} {}", name, dim(target));
    }
    Ok(())
}

fn cmd_deploy(host_dir: Option<std::path::PathBuf>, boot: bool, reboot: bool) -> Result<()> {
    deploy::run(host_dir, boot, reboot)
}

fn cmd_gc(args: GcArgs) -> Result<()> {
    let threshold_secs = parse_duration(&args.older_than)
        .ok_or_else(|| anyhow::anyhow!("invalid duration '{}' — use e.g. 7d, 24h, 1h", args.older_than))?;
    let now = now_secs();

    let envs = vm::list_envs().unwrap_or_default();
    let mut deleted = 0;

    for name in &envs {
        if vm::is_running(name).unwrap_or(false) {
            continue;
        }

        // use env dir mtime as last activity
        let last_active = std::fs::metadata(crate::env::env_dir(name))
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);

        if last_active == 0 {
            continue;
        }

        let age = now.saturating_sub(last_active);
        if age < threshold_secs {
            continue;
        }

        let age_str = if age >= 86400 {
            format!("{}d", age / 86400)
        } else if age >= 3600 {
            format!("{}h", age / 3600)
        } else {
            format!("{}m", age / 60)
        };

        if args.dry_run {
            println!("  {} would delete {} (stopped {})", dim("~"), bold(name), dim(&age_str));
        } else {
            vm::delete(name)?;
            println!("  {} deleted {} (stopped {})", rm(), bold(name), dim(&age_str));
            deleted += 1;
        }
    }

    if args.dry_run {
        println!("{} dry run — no envs deleted", dim("ℹ"));
    } else if deleted == 0 {
        println!("  {}", dim("no stale envs to clean up"));
    } else {
        println!("{} cleaned up {} env{}", ok(), deleted, if deleted == 1 { "" } else { "s" });
    }
    Ok(())
}

// Env commands

fn cmd_resolve_env(repo_filter: Option<&str>) -> Result<()> {
    if let Ok(envs) = vm::list_envs() {
        for name in &envs {
            if !vm::is_running(name).unwrap_or(false) {
                continue;
            }
            if let Some(filter) = repo_filter {
                let rt = vm::runtime_dir(name);
                let env_repo = std::fs::read_to_string(rt.join("repo"))
                    .unwrap_or_default().trim().to_string();
                if env_repo != filter {
                    continue;
                }
            }
            println!("{}", vm::env_repo_dir(name).display());
            return Ok(());
        }
    }

    let repo = git::Repo::open()?;
    let path = repo.resolve_env_path()?;
    println!("{}", path.display());
    Ok(())
}

#[instrument(skip(repo), fields(env = %env))]
fn cmd_stop(repo: &git::Repo, env: &str) -> Result<()> {
    let active = find_env_server(repo, env)?;

    if active.is_server() {
        let c = connect_remote(&active)?;
        c.down(env)?;
        println!("{} stopped {}", dn(), bold(env));
    } else if vm::is_running(env).unwrap_or(false) {
        vm::stop(env)?;
        println!("{} stopped {}", dn(), bold(env));
    } else {
        println!("  {} not running", bold(env));
    }

    Ok(())
}

#[instrument(skip_all, fields(env = %args.name))]
fn cmd_shell(repo: &git::Repo, args: ShellArgs) -> Result<()> {
    let env = &args.name;

    if args.list_sessions {
        let sessions = crate::session::list(env)?;
        if sessions.is_empty() {
            println!("  {}", dim("no sessions"));
        } else {
            for s in sessions {
                println!("  {} {}", ok(), bold(&s));
            }
        }
        return Ok(());
    }

    if let Some(name) = args.kill_session.as_deref() {
        crate::session::kill(env, name)?;
        println!("{} killed session {}", rm(), bold(name));
        return Ok(());
    }

    let active = find_env_server(repo, env)?;
    let cfg = config::load(repo.root())?;
    let t = make_transport(&active)?;
    t.ensure_running(env, repo, &cfg)?;

    if args.json {
        let cmd = args.command.as_deref()
            .ok_or_else(|| anyhow::anyhow!("--json requires -c <command>"))?;
        let captured = t.shell_capture(env, cmd)?;
        println!("{}", serde_json::to_string(&serde_json::json!({
            "exit_code": captured.exit_code,
            "stdout": captured.stdout,
            "stderr": captured.stderr,
            "duration_ms": captured.duration_ms,
        }))?);
        return Ok(());
    }

    if args.command.is_none() && args.session.is_none() {
        println!("{} entering {}", arrow(), bold(env));
    }
    let session = args.session.as_deref();
    t.shell(env, args.command.as_deref(), session)
}

struct EnvRow {
    name: String,
    status: String,
    server: String,
    repo: Option<String>,
    ip: Option<String>,
}

fn collect_env_rows(_repo: Option<&git::Repo>) -> Result<Vec<EnvRow>> {
    let mut rows: Vec<EnvRow> = Vec::new();

    let local_names = vm::list_envs().unwrap_or_default();
    for name in local_names {
        let running = vm::is_running(&name).unwrap_or(false);
        let rt = vm::runtime_dir(&name);
        let env_repo = std::fs::read_to_string(rt.join("repo"))
            .ok().map(|s| s.trim().to_string());
        let ip = if running {
            std::fs::read_to_string(rt.join("ip"))
                .ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
        } else {
            None
        };
        rows.push(EnvRow {
            name,
            status: if running { "running" } else { "stopped" }.to_string(),
            server: "localhost".to_string(),
            repo: env_repo,
            ip,
        });
    }

    for (srv_name, target) in server::list()? {
        let c = match client::Client::connect(&target) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let envs = c.list().unwrap_or_default();
        for env in envs {
            rows.push(EnvRow {
                name: env.name,
                status: env.status,
                server: srv_name.clone(),
                repo: env.repo,
                ip: env.ip,
            });
        }
    }

    Ok(rows)
}

/// Build the JSON shape: { "envs": [...] }.
fn list_to_json(rows: &[EnvRow]) -> serde_json::Value {
    let envs: Vec<serde_json::Value> = rows.iter().map(|r| {
        serde_json::json!({
            "name": r.name,
            "server": r.server,
            "status": r.status,
            "ip": r.ip,
            "repo": r.repo,
            "started_at": null,
        })
    }).collect();
    serde_json::json!({ "envs": envs })
}

fn cmd_list(repo: Option<&git::Repo>, json: bool) -> Result<()> {
    let rows = collect_env_rows(repo)?;

    let current_repo = repo.and_then(|r| r.root().file_name())
        .and_then(|n| n.to_str())
        .map(|s| s.to_string());

    if json {
        println!("{}", serde_json::to_string(&list_to_json(&rows))?);
        return Ok(());
    }

    if rows.is_empty() {
        println!("  {}", dim("no envs"));
        return Ok(());
    }

    for row in &rows {
        let env_label = match (row.repo.as_deref(), current_repo.as_deref()) {
            (Some(r), Some(cr)) if r == cr => row.name.clone(),
            (Some(r), _) => format!("{}/{}", r, row.name),
            (None, _) => row.name.clone(),
        };

        println!("  {:<24}  [{}]  {}",
            bold(&env_label),
            vm_status(&row.status), dim(&row.server));
    }
    Ok(())
}

fn parse_ports(specs: &[String]) -> Result<Vec<u16>> {
    let mut ports = Vec::new();
    for spec in specs {
        if let Some((start, end)) = spec.split_once('-') {
            let s: u16 = start.parse().context(format!("invalid port: {start}"))?;
            let e: u16 = end.parse().context(format!("invalid port: {end}"))?;
            if s > e {
                anyhow::bail!("invalid range: {spec}");
            }
            ports.extend(s..=e);
        } else {
            ports.push(spec.parse().context(format!("invalid port: {spec}"))?);
        }
    }
    Ok(ports)
}

fn derive_loopback(vm_ip: &str) -> Option<String> {
    let parts: Vec<&str> = vm_ip.split('.').collect();
    if parts.len() == 4 {
        Some(format!("127.0.{}.{}", parts[2], parts[3]))
    } else {
        None
    }
}

fn setup_tunnel_dns(loopback: &str, dns: &str) -> bool {
    if !cfg!(unix) {
        return false;
    }

    let lo_ok = std::process::Command::new("sudo")
        .args(["ip", "addr", "add", &format!("{loopback}/8"), "dev", "lo"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);

    if !lo_ok {
        return false;
    }

    let exe = std::env::current_exe().unwrap_or_else(|_| "vitro".into());
    std::process::Command::new("sudo")
        .args([exe.as_os_str(), std::ffi::OsStr::new("util"), std::ffi::OsStr::new("hosts"), std::ffi::OsStr::new("add"), std::ffi::OsStr::new(loopback), std::ffi::OsStr::new(dns)])
        .status().ok();

    true
}

fn cleanup_tunnel_dns(loopback: &str, dns: &str) {
    if !cfg!(unix) {
        return;
    }

    let exe = std::env::current_exe().unwrap_or_else(|_| "vitro".into());
    std::process::Command::new("sudo")
        .args([exe.as_os_str(), std::ffi::OsStr::new("util"), std::ffi::OsStr::new("hosts"), std::ffi::OsStr::new("remove"), std::ffi::OsStr::new(dns)])
        .status().ok();

    std::process::Command::new("sudo")
        .args(["ip", "addr", "del", &format!("{loopback}/8"), "dev", "lo"])
        .output().ok();
}

fn cmd_tunnel(repo: &git::Repo, args: TunnelArgs) -> Result<()> {
    let env = &args.name;
    let cfg = config::load(repo.root())?;
    let cli_ports = if args.port.is_empty() { vec![] } else { parse_ports(&args.port)? };
    let ports = if cli_ports.is_empty() { &cfg.ports } else { &cli_ports };
    if ports.is_empty() {
        anyhow::bail!("no ports specified — use -p 5173 or add ports = [5173] to .vitro/config.toml");
    }

    let active = find_env_server(repo, env)?;
    let target = active.target()?
        .ok_or_else(|| anyhow::anyhow!("tunnel is for remote hosts — ports are already accessible locally"))?;

    let c = connect_remote(&active)?;
    let envs = c.list()?;
    let found = envs.iter().find(|e| e.name == *env)
        .ok_or_else(|| anyhow::anyhow!("env '{env}' not found on server"))?;
    let vm_ip = found.ip.as_deref()
        .ok_or_else(|| anyhow::anyhow!("env '{env}' has no IP (not running?)"))?;

    let loopback = derive_loopback(vm_ip)
        .ok_or_else(|| anyhow::anyhow!("cannot derive loopback from IP '{vm_ip}'"))?;

    let repo_name = repo.root()
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown");
    let dns = vm::dns_hostname(env, repo_name);

    let has_dns = setup_tunnel_dns(&loopback, &dns);
    let bind_addr = if has_dns { &loopback } else { "127.0.0.1" };

    let mut cmd = std::process::Command::new("autossh");
    cmd.args([
        "-M", "0",
        "-N",
        "-o", "LogLevel=ERROR",
        "-o", "ServerAliveInterval=30",
        "-o", "ServerAliveCountMax=3",
    ]);
    for port in ports {
        cmd.arg("-L").arg(format!("{bind_addr}:{port}:{dns}:{port}"));
    }
    cmd.arg(&target);

    let url = if has_dns {
        format!("http://{dns}:{}", ports[0])
    } else {
        format!("http://127.0.0.1:{}", ports[0])
    };
    let ports_str: Vec<String> = ports.iter().map(|p| p.to_string()).collect();
    println!("{} tunneling {} port {} → {url}", style("⇄").cyan(), bold(env), ports_str.join(", "));
    if !has_dns {
        println!("  {}", dim(".env DNS not available — using localhost"));
    }
    println!("  {}", dim("press ctrl+c to close"));

    if args.open {
        std::process::Command::new("xdg-open")
            .arg(&url)
            .spawn()
            .ok();
    }

    let status = cmd.status().context("autossh tunnel failed")?;
    if !status.success() {
        anyhow::bail!("autossh exited with {}", status);
    }

    if has_dns {
        cleanup_tunnel_dns(&loopback, &dns);
    }
    Ok(())
}

const HOSTS_MARKER: &str = "# vitro-tunnel";

fn cmd_hosts(action: HostsAction) -> Result<()> {
    match action {
        HostsAction::Add { ip, hostname } => {
            let hosts = std::fs::read_to_string("/etc/hosts").unwrap_or_default();
            if !hosts.contains(&hostname) {
                use std::io::Write;
                let mut f = std::fs::OpenOptions::new().append(true).open("/etc/hosts")
                    .context("cannot write /etc/hosts (are you root?)")?;
                writeln!(f, "{ip} {hostname} {HOSTS_MARKER}")?;
            }
        }
        HostsAction::Remove { hostname } => {
            let hosts = std::fs::read_to_string("/etc/hosts")
                .context("cannot read /etc/hosts")?;
            let updated: String = hosts
                .lines()
                .filter(|line| !(line.contains(&hostname) && line.contains(HOSTS_MARKER)))
                .collect::<Vec<_>>()
                .join("\n");
            std::fs::write("/etc/hosts", format!("{updated}\n"))
                .context("cannot write /etc/hosts (are you root?)")?;
        }
    }
    Ok(())
}

fn cmd_secrets(repo: &git::Repo, args: SecretsArgs) -> Result<()> {
    let cfg = config::load(repo.root())?;
    let keys = &cfg.secrets.keys;
    match args.action {
        SecretsAction::Encrypt => secrets::encrypt(repo.root(), keys),
        SecretsAction::Edit => secrets::edit(repo.root(), keys),
        SecretsAction::Decrypt => secrets::decrypt_to_env(repo.root()),
    }
}

fn git_state(repo: &git::Repo) -> serde_json::Value {
    let head = repo.rev_parse("HEAD").ok();
    serde_json::json!({ "head": head })
}

fn status_to_json(repo: &git::Repo, name: &str, status: &str, ip: Option<&str>) -> serde_json::Value {
    serde_json::json!({
        "name": name,
        "status": status,
        "ip": ip,
        "process": serde_json::json!({
            "running": status == "running",
            "pid": null,
            "started_at": null,
        }),
        "git": git_state(repo),
    })
}

fn cmd_status(repo: &git::Repo, env: Option<&str>, json: bool) -> Result<()> {
    let Some(name) = env else {
        return cmd_list(Some(repo), json);
    };

    let active = find_env_server(repo, name)?;
    let (status, ip) = if active.is_server() {
        let c = connect_remote(&active)?;
        let envs = c.list()?;
        let found = envs.iter().find(|e| e.name == name)
            .ok_or_else(|| anyhow::anyhow!("env '{name}' not found"))?;
        (found.status.clone(), found.ip.clone())
    } else {
        let running = vm::is_running(name)?;
        let s = if running { "running".to_string() } else { "stopped".to_string() };
        let ip = if running {
            std::fs::read_to_string(vm::runtime_dir(name).join("ip"))
                .ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
        } else { None };
        (s, ip)
    };

    if json {
        println!("{}", serde_json::to_string(&status_to_json(repo, name, &status, ip.as_deref()))?);
    } else {
        println!("env: {}", bold(name));
        println!("vm: {}", vm_status(&status));
        if let Some(ip) = &ip {
            println!("ip: {ip}");
        }
    }
    Ok(())
}

fn cmd_session(repo: &git::Repo, args: SessionArgs) -> Result<()> {
    match args.action {
        SessionAction::Send { env, text, session } => {
            let active = find_env_server(repo, &env)?;
            let (_, ssh_target) = if active.is_server() {
                let _c = connect_remote(&active)?;
                // For remote envs, the VM SSH info must be obtained via the server
                anyhow::bail!("session send on remote envs not yet supported — shell in and use dtach directly");
            } else {
                vm::ssh_target(&env)?
            };
            let text_interp = text.replace("\\n", "\n").replace("\\r", "\r");
            crate::session::send(&env, &session, &text_interp, &ssh_target)?;
            println!("{} sent to {}/{}", ok(), bold(&env), bold(&session));
            Ok(())
        }
        SessionAction::Log { env, session, follow } => {
            let active = find_env_server(repo, &env)?;
            if active.is_server() {
                anyhow::bail!("session log on remote envs not yet supported");
            }
            let log = crate::session::log_path_host(
                &vm::env_dir(&env),
                &session,
            );
            if !log.exists() {
                println!("  {}", dim(&format!("no log at {}", log.display())));
                return Ok(());
            }
            let mut cmd = std::process::Command::new("tail");
            if follow {
                cmd.arg("-f");
            }
            cmd.arg(&log);
            cmd.status().context("tail failed")?;
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(name: &str, status: &str, server: &str) -> EnvRow {
        EnvRow {
            name: name.to_string(),
            status: status.to_string(),
            server: server.to_string(),
            repo: Some("vitro".to_string()),
            ip: Some("192.168.83.42".to_string()),
        }
    }

    #[test]
    fn list_json_shape() {
        let rows = vec![row("feat-x", "running", "prod")];
        let v = list_to_json(&rows);
        let envs = v.get("envs").unwrap().as_array().unwrap();
        assert_eq!(envs.len(), 1);
        let e = &envs[0];
        for k in &["name", "server", "status", "ip", "repo", "started_at"] {
            assert!(e.get(k).is_some(), "list JSON missing key: {k}");
        }
        assert_eq!(e["name"], "feat-x");
        assert_eq!(e["server"], "prod");
        assert_eq!(e["status"], "running");
    }

    #[test]
    fn list_json_empty_emits_empty_array() {
        let v = list_to_json(&[]);
        assert_eq!(v["envs"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn list_json_unknown_fields_are_null() {
        let mut r = row("feat-x", "running", "prod");
        r.ip = None;
        let v = list_to_json(&[r]);
        let e = &v["envs"][0];
        assert!(e["ip"].is_null());
        assert!(e["started_at"].is_null());
    }

    use super::parse_duration;

    #[test]
    fn parse_duration_bare_number_is_seconds() {
        assert_eq!(parse_duration("42"), Some(42));
    }

    #[test]
    fn parse_duration_seconds_suffix() {
        assert_eq!(parse_duration("30s"), Some(30));
    }

    #[test]
    fn parse_duration_minutes() {
        assert_eq!(parse_duration("5m"), Some(300));
    }

    #[test]
    fn parse_duration_hours() {
        assert_eq!(parse_duration("2h"), Some(7200));
    }

    #[test]
    fn parse_duration_days() {
        assert_eq!(parse_duration("7d"), Some(604_800));
    }

    #[test]
    fn parse_duration_zero() {
        assert_eq!(parse_duration("0d"), Some(0));
    }

    #[test]
    fn parse_duration_unknown_suffix_rejected() {
        assert_eq!(parse_duration("5y"), None);
        assert_eq!(parse_duration("5w"), None);
    }

    #[test]
    fn parse_duration_garbage_rejected() {
        assert_eq!(parse_duration(""), None);
        assert_eq!(parse_duration("abc"), None);
        assert_eq!(parse_duration("5.5h"), None);
        assert_eq!(parse_duration("h"), None);
    }
}
