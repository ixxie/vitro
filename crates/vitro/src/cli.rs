use std::path::Path;
use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};
use console::{style, Style};

use tracing::instrument;
use crate::{cell_state, client, config, deploy, git, server, server_init, proxy, secrets, transport, vm, worktree};
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
    /// Create a new branch and add it to vitro
    Create(CreateArgs),
    /// Add an existing branch to vitro
    Add(AddArgs),
    /// Remove a branch from vitro
    Remove(RemoveArgs),
    /// List vitro-managed branches
    List {
        /// Show cells from all repos
        #[arg(short, long)]
        all: bool,
        /// Emit machine-readable JSON
        #[arg(long)]
        json: bool,
    },
    /// Print worktree path for a branch
    Path {
        /// Branch name
        name: String,
    },
    /// Print shell hook for vitro cd (add to shell config)
    Hook {
        /// Shell: fish, bash, zsh, nu, powershell
        shell: String,
    },
    /// Run a flow or ad-hoc command in a cell
    Run(RunArgs),
    /// Stop a cell (data preserved)
    Stop(StopArgs),
    /// Show cell status
    Status {
        /// Branch name (omit for all)
        name: Option<String>,
        /// Emit machine-readable JSON
        #[arg(long)]
        json: bool,
    },
    /// SSH into a cell
    Shell(ShellArgs),
    /// Spawn an ACP (JSON-RPC over stdio) agent in a cell.
    /// stdin/stdout pass through transparently — meant to be invoked
    /// by an ACP client like Paseo as a provider `command`.
    Acp(AcpArgs),
    /// View run output from a cell
    Logs(LogsArgs),
    /// Forward declared ports from a remote cell to localhost
    Tunnel(TunnelArgs),
    /// Manage encrypted secrets
    Secrets(SecretsArgs),
    /// Manage servers
    Server(ServerArgs),
    #[command(hide = true)]
    /// Internal utilities
    Util(UtilArgs),
}

#[derive(Args, Debug)]
struct CreateArgs {
    /// Branch name to create
    name: String,
    /// Server to use
    #[arg(short, long)]
    server: Option<String>,
    /// Don't switch to the worktree after creation
    #[arg(long)]
    no_switch: bool,
}

#[derive(Args, Debug)]
struct AddArgs {
    /// Existing branch name
    name: String,
    /// Server to use
    #[arg(short, long)]
    server: Option<String>,
}

#[derive(Args, Debug)]
struct RemoveArgs {
    /// Branch name
    name: String,
    /// Also delete the git branch
    #[arg(short, long)]
    delete: bool,
}

#[derive(Args, Debug)]
struct RunArgs {
    /// Cell name (optional if in worktree)
    name: Option<String>,
    /// Flow name (optional if exactly one flow exists in .vitro/flows/)
    flow: Option<String>,
    /// Branch name (alternative to positional cell)
    #[arg(short, long)]
    branch: Option<String>,
    /// Server to run on
    #[arg(short, long)]
    server: Option<String>,
    /// Run an ad-hoc command instead of a flow
    #[arg(short = 'c', long = "command")]
    command: Option<String>,
    /// Detach from run output (default is attached)
    #[arg(short, long)]
    detach: bool,
    /// Params as key=value pairs (e.g. vitro run cell flow -- project=foo)
    #[arg(last = true)]
    params: Vec<String>,
}

#[derive(Args, Debug)]
struct StopArgs {
    /// Branch name (optional if in worktree)
    name: Option<String>,
    /// Also remove from vitro (tear down worktree + cell)
    #[arg(short, long)]
    delete: bool,
}

#[derive(Args)]
struct ShellArgs {
    /// Branch name (optional if in worktree)
    name: Option<String>,
    /// Run a command instead of interactive shell
    #[arg(short = 'c', long = "command")]
    command: Option<String>,
    /// Emit JSON {exit_code, stdout, stderr, duration_ms} (only with -c)
    #[arg(long)]
    json: bool,
    /// Server-side mode (skip repo checks)
    #[arg(long, hide = true)]
    server: bool,
}

#[derive(Args)]
struct AcpArgs {
    /// Branch name (optional if in worktree)
    name: Option<String>,
}

#[derive(Args)]
struct LogsArgs {
    /// Branch name (optional if in worktree)
    name: Option<String>,
    /// Follow log output
    #[arg(short, long)]
    follow: bool,
    /// Emit JSONL with {ts, stream, line} per line
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct TunnelArgs {
    /// Branch name (optional if in worktree)
    name: Option<String>,
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
    /// Garbage collect stopped cells older than a threshold
    Gc(GcArgs),
    #[command(hide = true)]
    /// Run the network proxy (used by systemd)
    Proxy(ProxyArgs),
}

#[derive(Args, Debug)]
struct GcArgs {
    /// Delete cells stopped longer than this (e.g. "7d", "24h", "1h")
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
struct UtilArgs {
    #[command(subcommand)]
    action: UtilAction,
}

#[derive(Subcommand)]
enum UtilAction {
    /// Print the cell repo path for the current VM (used by git-remote-vitro)
    ResolveCell {
        /// Match cells for this repo name
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
            UtilAction::ResolveCell { repo } => cmd_resolve_cell(repo.as_deref()),
            UtilAction::Hosts { action } => cmd_hosts(action),
        },

        // commands that don't need cell context
        Commands::Init => {
            let repo = git::Repo::open()?;
            cmd_init(&repo)
        }
        Commands::Create(args) => {
            let repo = git::Repo::open()?;
            cmd_create(&repo, args)
        }
        Commands::Add(args) => {
            let repo = git::Repo::open()?;
            cmd_add(&repo, args)
        }
        Commands::Remove(args) => {
            let repo = git::Repo::open()?;
            cmd_remove(&repo, args)
        }
        Commands::List { all, json } => cmd_list(git::Repo::open().ok().as_ref(), all, json),
        Commands::Path { name } => {
            let repo = git::Repo::open()?;
            cmd_path(&repo, &name)
        }
        Commands::Hook { shell } => cmd_hook(&shell),
        Commands::Status { name, json } => {
            let repo = git::Repo::open()?;
            cmd_status(&repo, name.as_deref(), json)
        }
        Commands::Secrets(args) => {
            let repo = git::Repo::open()?;
            cmd_secrets(&repo, args)
        }

        // commands that need cell context (resolve from worktree or explicit arg)
        Commands::Shell(args) if args.server => {
            vm::shell(&args.name.unwrap_or_default(), args.command.as_deref())
        }
        Commands::Run(args) => {
            let explicit = args.name.as_deref().or(args.branch.as_deref());
            let (repo, cell) = worktree::resolve_cell(explicit)?;
            cmd_run(&repo, &cell, args)
        }
        Commands::Stop(args) => {
            let (repo, cell) = worktree::resolve_cell(args.name.as_deref())?;
            cmd_stop(&repo, &cell, args)
        }
        Commands::Logs(args) => {
            let (repo, cell) = worktree::resolve_cell(args.name.as_deref())?;
            cmd_logs(&repo, &cell, args)
        }
        Commands::Shell(args) => {
            let (repo, cell) = worktree::resolve_cell(args.name.as_deref())?;
            cmd_shell(&repo, &cell, args)
        }
        Commands::Acp(args) => {
            let (repo, cell) = worktree::resolve_cell(args.name.as_deref())?;
            cmd_acp(&repo, &cell)
        }
        Commands::Tunnel(args) => {
            let (repo, cell) = worktree::resolve_cell(args.name.as_deref())?;
            cmd_tunnel(&repo, &cell, args)
        }
    }
}

// Helpers

/// Find which server hosts a cell. Resolution order:
///   1. recorded binding from `vitro create --server X` (laptop-local)
///   2. cell exists locally (vm::list_cells)
///   3. cell exists on a registered remote server (running OR stopped)
///   4. repo or client config default
///   5. localhost
fn find_cell_server(repo: &git::Repo, cell: &str) -> Result<server::ActiveServer> {
    if let Some(srv) = cell_state::get_server(repo.root(), cell) {
        return server::resolve(&srv);
    }

    if vm::list_cells().unwrap_or_default().iter().any(|c| c == cell) {
        return Ok(server::ActiveServer::Localhost);
    }

    for (name, target) in server::list()? {
        if let Ok(c) = client::Client::connect(&target) {
            let cells = c.list().unwrap_or_default();
            if cells.iter().any(|c| c.name == cell) {
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

    // No binding, no config default, cell unknown locally and remotely.
    // Falling back to localhost silently leads to opaque downstream
    // errors (e.g. "writing /var/lib/vitro/secrets.env: No such file"
    // when there's no local vitro server). Be loud instead.
    anyhow::bail!(
        "no server bound for cell '{cell}' — pass --server, set [server] in .vitro/config.toml, \
         or recreate with `vitro create {cell} --server <name>`"
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

    git::ensure_gitignore_entry(repo.root(), ".vitro/trees/")?;

    repo.add_vitro_remote("vitro://localhost")?;
    println!("{} initialized vitro", ok());
    Ok(())
}

// Branch lifecycle commands

/// Signal the shell hook to cd into a worktree.
/// Uses VITRO_CD_FILE (tempfile set by the shell hook) so vitro's stdout
/// is never piped — interactive commands like `secrets edit` get a real tty.
fn emit_cd(path: &Path) {
    if let Ok(f) = std::env::var("VITRO_CD_FILE") {
        std::fs::write(f, path.display().to_string()).ok();
    }
}

fn cmd_create(repo: &git::Repo, args: CreateArgs) -> Result<()> {
    let name = &args.name;
    if repo.branch_exists(name) {
        anyhow::bail!("branch '{name}' already exists — use 'vitro add {name}' instead");
    }

    repo.create_branch(name)?;
    println!("  {} branch {}", add(), bold(name));

    let path = worktree::add(repo, name)?;
    println!("  {} worktree at {}", add(), dim(&path.display().to_string()));

    if let Some(srv) = args.server.as_deref() {
        cell_state::set_server(repo.root(), name, srv)?;
        println!("  {} server {}", add(), bold(srv));
    }

    println!("{} created {}", ok(), bold(name));
    if !args.no_switch {
        emit_cd(&path);
    }
    Ok(())
}

fn cmd_add(repo: &git::Repo, args: AddArgs) -> Result<()> {
    let name = &args.name;
    if !repo.branch_exists(name) {
        anyhow::bail!("branch '{name}' does not exist — use 'vitro create {name}' to create it");
    }

    let path = worktree::add(repo, name)?;
    println!("  {} worktree at {}", add(), dim(&path.display().to_string()));

    if let Some(srv) = args.server.as_deref() {
        cell_state::set_server(repo.root(), name, srv)?;
        println!("  {} server {}", add(), bold(srv));
    }

    println!("{} added {}", ok(), bold(name));
    Ok(())
}

fn cmd_remove(repo: &git::Repo, args: RemoveArgs) -> Result<()> {
    let name = &args.name;

    // stop cell if running
    if let Ok(active) = find_cell_server(repo, name) {
        if active.is_server() {
            let c = connect_remote(&active)?;
            c.delete(name).ok();
        } else if vm::is_running(name).unwrap_or(false) {
            vm::stop(name)?;
        }
    }

    worktree::remove(repo, name)?;
    println!("  {} worktree removed", rm());

    cell_state::clear(repo.root(), name).ok();

    if args.delete {
        repo.delete_branch(name).ok();
        println!("  {} branch deleted", rm());
    }

    println!("{} removed {}", ok(), bold(name));
    Ok(())
}

fn cmd_path(repo: &git::Repo, name: &str) -> Result<()> {
    let path = worktree::tree_path(repo.root(), name);
    if !path.exists() {
        anyhow::bail!("no worktree for '{name}' — use 'vitro add {name}' first");
    }
    println!("{}", path.display());
    Ok(())
}

fn cmd_hook(shell: &str) -> Result<()> {
    let hook = match shell {
        "fish" => r#"function vitro
    if test "$argv[1]" = "switch"
        cd (command vitro path $argv[2])
    else if test "$argv[1]" = "exit"
        set -l cwd (pwd)
        if string match -q '*/.vitro/trees/*' $cwd
            cd (string replace -r '/\.vitro/trees/.*' '' $cwd)
        end
    else
        set -l cdfile (mktemp)
        set -lx VITRO_CD_FILE $cdfile
        command vitro $argv
        set -l s $status
        if test -s $cdfile
            cd (cat $cdfile)
        end
        rm -f $cdfile
        return $s
    end
end"#,
        "bash" | "zsh" => r#"vitro() {
    if [ "$1" = "switch" ]; then
        cd "$(command vitro path "$2")"
    elif [ "$1" = "exit" ]; then
        case "$PWD" in
            */.vitro/trees/*) cd "${PWD%%/.vitro/trees/*}" ;;
        esac
    else
        local cdfile
        cdfile=$(mktemp)
        VITRO_CD_FILE="$cdfile" command vitro "$@"
        local s=$?
        if [ -s "$cdfile" ]; then
            cd "$(cat "$cdfile")"
        fi
        rm -f "$cdfile"
        return $s
    fi
}"#,
        "nu" | "nushell" => r#"def --wrapped vitro [...args: string] {
    if ($args | first) == "switch" {
        cd (^vitro path ($args | get 1))
    } else if ($args | first) == "exit" {
        let cwd = (pwd)
        if ($cwd | str contains "/.vitro/trees/") {
            cd ($cwd | str replace -r '/\.vitro/trees/.*' '')
        }
    } else {
        let cdfile = (mktemp)
        with-env { VITRO_CD_FILE: $cdfile } { ^vitro ...$args }
        let cdpath = (open $cdfile --raw | str trim)
        rm -f $cdfile
        if not ($cdpath | is-empty) {
            cd $cdpath
        }
    }
}"#,
        "powershell" | "pwsh" => r#"function vitro {
    if ($args[0] -eq "switch") {
        Set-Location (& vitro.exe path $args[1])
    } elseif ($args[0] -eq "exit") {
        $cwd = Get-Location
        if ($cwd -match '[\\/]\.vitro[\\/]trees[\\/]') {
            Set-Location ($cwd -replace '[\\/]\.vitro[\\/]trees[\\/].*', '')
        }
    } else {
        $cdfile = [System.IO.Path]::GetTempFileName()
        $env:VITRO_CD_FILE = $cdfile
        & vitro.exe @args
        $env:VITRO_CD_FILE = $null
        if (Test-Path $cdfile) {
            $cdpath = Get-Content $cdfile -ErrorAction SilentlyContinue
            Remove-Item $cdfile -Force -ErrorAction SilentlyContinue
            if ($cdpath) {
                Set-Location $cdpath
            }
        }
    }
}"#,
        _ => anyhow::bail!("unsupported shell '{shell}' — use fish, bash, zsh, nu, or powershell"),
    };
    println!("{hook}");
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

    let cells = vm::list_cells().unwrap_or_default();
    let mut deleted = 0;

    for name in &cells {
        if vm::is_running(name).unwrap_or(false) {
            continue;
        }

        // use cell dir mtime as last activity
        let last_active = std::fs::metadata(crate::cell::cell_dir(name))
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
        println!("{} dry run — no cells deleted", dim("ℹ"));
    } else if deleted == 0 {
        println!("  {}", dim("no stale cells to clean up"));
    } else {
        println!("{} cleaned up {} cell{}", ok(), deleted, if deleted == 1 { "" } else { "s" });
    }
    Ok(())
}

// Cell commands

fn cmd_resolve_cell(repo_filter: Option<&str>) -> Result<()> {
    if let Ok(cells) = vm::list_cells() {
        for name in &cells {
            if !vm::is_running(name).unwrap_or(false) {
                continue;
            }
            if let Some(filter) = repo_filter {
                let rt = vm::runtime_dir(name);
                let cell_repo = std::fs::read_to_string(rt.join("repo"))
                    .unwrap_or_default().trim().to_string();
                if cell_repo != filter {
                    continue;
                }
            }
            println!("{}", vm::cell_repo_dir(name).display());
            return Ok(());
        }
    }

    let repo = git::Repo::open()?;
    let path = repo.resolve_cell_path()?;
    println!("{}", path.display());
    Ok(())
}

#[instrument(skip(repo), fields(cell = %cell))]
fn cmd_stop(repo: &git::Repo, cell: &str, args: StopArgs) -> Result<()> {
    let active = find_cell_server(repo, cell)?;

    if active.is_server() {
        let c = connect_remote(&active)?;
        c.down(cell)?;
        println!("{} stopped {}", dn(), bold(cell));
    } else if vm::is_running(cell).unwrap_or(false) {
        vm::stop(cell)?;
        println!("{} stopped {}", dn(), bold(cell));
    } else {
        println!("  {} not running", bold(cell));
    }

    if args.delete {
        cmd_remove(repo, RemoveArgs {
            name: cell.to_string(),
            delete: true,
        })?;
    }
    Ok(())
}

#[instrument(skip_all, fields(cell = %cell))]
fn cmd_shell(repo: &git::Repo, cell: &str, args: ShellArgs) -> Result<()> {
    let active = find_cell_server(repo, cell)?;
    let cfg = config::load(repo.root())?;
    let t = make_transport(&active)?;
    t.ensure_running(cell, repo, &cfg)?;

    if args.json {
        let cmd = args.command.as_deref()
            .ok_or_else(|| anyhow::anyhow!("--json requires -c <command>"))?;
        let captured = t.shell_capture(cell, cmd)?;
        println!("{}", serde_json::to_string(&serde_json::json!({
            "exit_code": captured.exit_code,
            "stdout": captured.stdout,
            "stderr": captured.stderr,
            "duration_ms": captured.duration_ms,
        }))?);
        return Ok(());
    }

    if args.command.is_none() {
        println!("{} entering {}", arrow(), bold(cell));
    }
    t.shell(cell, args.command.as_deref())
}

/// Spawn an ACP (JSON-RPC over stdio) agent inside a cell. Stdin and
/// stdout pass through to the spawning ACP client; whatever framing
/// the client and the in-cell agent speak (initialize, prompts, tool
/// calls) is opaque to vitro.
///
/// Intended invocation: an ACP client (e.g. Paseo) configures this as
/// a provider `command`. Each spawn is one ACP session. No run-pid
/// lock — concurrent sessions are expected.
#[instrument(skip_all, fields(cell = %cell))]
fn cmd_acp(repo: &git::Repo, cell: &str) -> Result<()> {
    // Check config first so a misconfigured invocation doesn't pay
    // the cost of substrate sync only to bail.
    let cfg = config::load(repo.root())?;
    let acp_cmd = cfg.acp.as_ref().ok_or_else(|| anyhow::anyhow!(
        "no [acp] block in .vitro/config.toml — add e.g.\n\
         [acp]\n\
         command = [\"claude-code\", \"--acp\"]"
    ))?;
    if acp_cmd.command.is_empty() {
        anyhow::bail!("[acp].command is empty");
    }

    let active = find_cell_server(repo, cell)?;
    let t = make_transport(&active)?;
    t.ensure_running(cell, repo, &cfg)?;

    // Build a shell command that exec's the configured agent with each
    // arg properly quoted, so JSON-RPC frames are the only thing
    // crossing stdin/stdout.
    let escaped: Vec<String> = acp_cmd.command.iter()
        .map(|a| crate::exec::shell_escape(a))
        .collect();
    let line = format!("exec {}", escaped.join(" "));

    t.acp_forward(cell, &line)
}

struct CellRow {
    name: String,
    status: String,
    server: String,
    repo: Option<String>,
    ip: Option<String>,
    worktree: Option<String>,
}

fn collect_cell_rows(repo: Option<&git::Repo>) -> Result<Vec<CellRow>> {
    let mut rows: Vec<CellRow> = Vec::new();

    let local_names = if let Some(repo) = repo {
        repo.list_clones().unwrap_or_default()
    } else {
        vm::list_cells().unwrap_or_default()
    };
    for name in local_names {
        let running = vm::is_running(&name).unwrap_or(false);
        let rt = vm::runtime_dir(&name);
        let cell_repo = std::fs::read_to_string(rt.join("repo"))
            .ok().map(|s| s.trim().to_string());
        let ip = if running {
            std::fs::read_to_string(rt.join("ip"))
                .ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
        } else {
            None
        };
        let worktree = repo.map(|r| worktree::tree_path(r.root(), &name).display().to_string());
        rows.push(CellRow {
            name,
            status: if running { "running" } else { "stopped" }.to_string(),
            server: "localhost".to_string(),
            repo: cell_repo,
            ip,
            worktree,
        });
    }

    for (srv_name, target) in server::list()? {
        let c = match client::Client::connect(&target) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let cells = c.list().unwrap_or_default();
        for cell in cells {
            rows.push(CellRow {
                name: cell.name,
                status: cell.status,
                server: srv_name.clone(),
                repo: cell.repo,
                ip: cell.ip,
                worktree: None,
            });
        }
    }

    Ok(rows)
}

/// Build the JSON shape from README "Machine-readable output":
/// { "cells": [...] }. Fields the substrate doesn't yet track
/// (started_at, last_run) are emitted as null so consumers see a
/// consistent schema.
fn list_to_json(rows: &[CellRow]) -> serde_json::Value {
    let cells: Vec<serde_json::Value> = rows.iter().map(|r| {
        serde_json::json!({
            "name": r.name,
            "branch": r.name,
            "server": r.server,
            "status": r.status,
            "ip": r.ip,
            "repo": r.repo,
            "worktree": r.worktree,
            "started_at": null,
            "last_run": null,
        })
    }).collect();
    serde_json::json!({ "cells": cells })
}

fn cmd_list(repo: Option<&git::Repo>, show_all: bool, json: bool) -> Result<()> {
    let rows = collect_cell_rows(repo)?;

    let current_repo = repo.and_then(|r| r.root().file_name())
        .and_then(|n| n.to_str())
        .map(|s| s.to_string());
    let filter_repo = if show_all { None } else { current_repo.clone() };

    let filtered: Vec<&CellRow> = rows.iter().filter(|r| {
        match (&filter_repo, &r.repo) {
            (Some(cr), Some(rr)) if cr != rr => false,
            _ => true,
        }
    }).collect();

    if json {
        let owned: Vec<CellRow> = filtered.iter().map(|r| CellRow {
            name: r.name.clone(),
            status: r.status.clone(),
            server: r.server.clone(),
            repo: r.repo.clone(),
            ip: r.ip.clone(),
            worktree: r.worktree.clone(),
        }).collect();
        println!("{}", serde_json::to_string(&list_to_json(&owned))?);
        return Ok(());
    }

    if filtered.is_empty() {
        println!("  {}", dim("no cells"));
        return Ok(());
    }

    let current_branch = repo.and_then(|r| r.current_branch().ok());

    for row in filtered {
        let marker = if current_branch.as_deref() == Some(row.name.as_str()) {
            style("▶").cyan().to_string()
        } else {
            " ".to_string()
        };

        let cell_label = match (row.repo.as_deref(), current_repo.as_deref(), show_all) {
            (Some(r), Some(cr), false) if r == cr => row.name.clone(),
            (Some(r), _, _) => format!("{}/{}", r, row.name),
            (None, _, _) => row.name.clone(),
        };

        println!("{} {:<24}  [{}]  {}",
            marker, bold(&cell_label),
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

fn cmd_tunnel(repo: &git::Repo, cell: &str, args: TunnelArgs) -> Result<()> {
    let cfg = config::load(repo.root())?;
    let cli_ports = if args.port.is_empty() { vec![] } else { parse_ports(&args.port)? };
    let ports = if cli_ports.is_empty() { &cfg.ports } else { &cli_ports };
    if ports.is_empty() {
        anyhow::bail!("no ports specified — use -p 5173 or add ports = [5173] to .vitro/config.toml");
    }

    let active = find_cell_server(repo, cell)?;
    let target = active.target()?
        .ok_or_else(|| anyhow::anyhow!("tunnel is for remote hosts — ports are already accessible locally"))?;

    let c = connect_remote(&active)?;
    let cells = c.list()?;
    let found = cells.iter().find(|c| c.name == cell)
        .ok_or_else(|| anyhow::anyhow!("cell '{cell}' not found on server"))?;
    let vm_ip = found.ip.as_deref()
        .ok_or_else(|| anyhow::anyhow!("cell '{cell}' has no IP (not running?)"))?;

    let loopback = derive_loopback(vm_ip)
        .ok_or_else(|| anyhow::anyhow!("cannot derive loopback from IP '{vm_ip}'"))?;

    let repo_name = repo.root()
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown");
    let dns = vm::dns_hostname(cell, repo_name);

    let has_dns = setup_tunnel_dns(&loopback, &dns);
    let bind_addr = if has_dns { &loopback } else { "127.0.0.1" };

    let mut cmd = std::process::Command::new("ssh");
    cmd.args(["-N", "-o", "LogLevel=ERROR"]);
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
    println!("{} tunneling {} port {} → {url}", style("⇄").cyan(), bold(cell), ports_str.join(", "));
    if !has_dns {
        println!("  {}", dim(".cell DNS not available — using localhost"));
    }
    println!("  {}", dim("press ctrl+c to close"));

    if args.open {
        std::process::Command::new("xdg-open")
            .arg(&url)
            .spawn()
            .ok();
    }

    loop {
        let status = cmd.status().context("ssh tunnel failed")?;

        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt;
            if status.success() || status.signal().is_some() {
                break;
            }
        }
        #[cfg(not(unix))]
        if status.success() {
            break;
        }

        std::thread::sleep(std::time::Duration::from_secs(3));
        cmd = std::process::Command::new("ssh");
        cmd.args(["-N", "-o", "LogLevel=ERROR"]);
        for port in ports {
            cmd.arg("-L").arg(format!("{loopback}:{port}:{dns}:{port}"));
        }
        cmd.arg(&target);
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

fn cmd_logs(repo: &git::Repo, cell: &str, args: LogsArgs) -> Result<()> {
    let active = find_cell_server(repo, cell)?;
    let t = make_transport(&active)?;
    if !args.json {
        return t.run_logs(cell, args.follow);
    }

    if args.follow {
        anyhow::bail!("--json --follow not yet supported (logs aren't streamed line-by-line)");
    }

    // Capture the run log, emit JSONL one entry per line.
    let cfg = config::load(repo.root())?;
    t.ensure_running(cell, repo, &cfg)?;
    let captured = t.shell_capture(cell, "tail -100 /tmp/vitro/run.log 2>/dev/null || true")?;
    let now = now_secs();
    for line in captured.stdout.lines() {
        let event = serde_json::json!({
            "ts": now,
            "stream": "stdout",
            "line": line,
        });
        println!("{}", serde_json::to_string(&event)?);
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

// Run commands

const FLOW_EXTS: &[&str] = &["ts", "sh", "py", "js", "mjs", "rb", "lua"];

/// Resolve a flow name to its script path inside the worktree.
/// If `flow` is given, find `.vitro/flows/<flow>.<ext>`.
/// If `flow` is None, list flow files; require exactly one.
fn resolve_flow_script(repo: &git::Repo, flow: Option<&str>) -> Result<(String, std::path::PathBuf)> {
    let flows_dir = repo.root().join(".vitro/flows");

    if let Some(name) = flow {
        for ext in FLOW_EXTS {
            let p = flows_dir.join(format!("{name}.{ext}"));
            if p.exists() {
                return Ok((name.to_string(), p));
            }
        }
        anyhow::bail!("flow '{name}' not found in .vitro/flows/");
    }

    let entries = std::fs::read_dir(&flows_dir)
        .with_context(|| format!("no .vitro/flows/ directory at {}", flows_dir.display()))?;

    let mut found: Vec<(String, std::path::PathBuf)> = Vec::new();
    for e in entries.flatten() {
        let path = e.path();
        if !path.is_file() {
            continue;
        }
        let ext = path.extension().and_then(|s| s.to_str()).unwrap_or("");
        if !FLOW_EXTS.contains(&ext) {
            continue;
        }
        let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("").to_string();
        if stem.is_empty() {
            continue;
        }
        found.push((stem, path));
    }

    match found.len() {
        0 => anyhow::bail!("no flows found in .vitro/flows/"),
        1 => Ok(found.remove(0)),
        _ => {
            let names: Vec<String> = found.iter().map(|(n, _)| n.clone()).collect();
            anyhow::bail!(
                "multiple flows found ({}) — specify one: vitro run <cell> <flow>",
                names.join(", "),
            );
        }
    }
}

/// Path to the flow script as seen from inside the cell (workspace mount).
fn cell_script_path(repo: &git::Repo, script_path: &Path) -> Result<String> {
    let rel = script_path.strip_prefix(repo.root())
        .with_context(|| format!("script {} not inside repo {}", script_path.display(), repo.root().display()))?;
    let repo_name = repo.root()
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("repo");
    Ok(format!("/{}/{}", repo_name, rel.display()))
}

#[instrument(skip(repo))]
fn cmd_run(repo: &git::Repo, cell: &str, args: RunArgs) -> Result<()> {
    let cfg = config::load(repo.root())?;

    // params → VITRO_PARAM_<KEY> env vars
    let mut env_pairs: Vec<(String, String)> = Vec::new();
    for kv in &args.params {
        let (k, v) = kv.split_once('=')
            .ok_or_else(|| anyhow::anyhow!("invalid param '{}' — expected key=value", kv))?;
        env_pairs.push((format!("VITRO_PARAM_{}", k.to_uppercase()), v.to_string()));
    }

    let client_cfg = server::load_client_config();
    let srv_name = args.server.as_ref()
        .or(cfg.server.as_ref())
        .or(client_cfg.server.as_ref());

    let active = if let Some(srv) = srv_name {
        server::resolve(srv)?
    } else {
        find_cell_server(repo, cell)?
    };

    let server_label = match &active {
        server::ActiveServer::Localhost => "localhost".to_string(),
        server::ActiveServer::Remote { name } => name.clone(),
    };
    println!("{} on {}", arrow(), bold(&server_label));

    let t = make_transport(&active)?;
    t.ensure_running(cell, repo, &cfg)?;

    // Reject if a run is already in progress on this cell (default
    // is "reject", not queue — per the project's working decisions).
    let busy = t.shell_capture(cell, crate::exec::run_busy_check())?;
    if busy.stdout.trim() == "busy" {
        anyhow::bail!("a run is in progress on '{cell}' — wait for it or `vitro stop {cell}`");
    }

    // standard env for flows
    let repo_name = repo.root()
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("repo")
        .to_string();
    let server_label = match &active {
        server::ActiveServer::Localhost => "localhost".to_string(),
        server::ActiveServer::Remote { name } => name.clone(),
    };

    env_pairs.push(("VITRO_CELL".to_string(), cell.to_string()));
    env_pairs.push(("VITRO_BRANCH".to_string(), cell.to_string()));
    env_pairs.push(("VITRO_REPO".to_string(), repo_name.clone()));
    env_pairs.push(("VITRO_SERVER".to_string(), server_label));

    let env_refs: Vec<(&str, &str)> = env_pairs.iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();

    let attached = !args.detach;

    if let Some(cmd) = args.command.as_deref() {
        // ad-hoc command — write to a temp script-ish path inline using sh -c via env
        // simplest: invoke shell with the env, running the command directly
        let mut prefix = String::new();
        for (k, v) in &env_refs {
            prefix.push_str(&format!("{}={} ", k, crate::exec::shell_escape(v)));
        }
        let inner = format!("{prefix}sh -c {}", crate::exec::shell_escape(cmd));
        let final_cmd = if attached {
            crate::exec::attached_with_lock(&inner)
        } else {
            crate::exec::detached(&inner, "/tmp/vitro/run.log")
        };
        println!("{} running on {}", arrow(), bold(cell));
        t.shell(cell, Some(&final_cmd))?;
        if attached {
            return Ok(());
        }
        println!("  {} vitro logs -f", dim("follow:"));
        return Ok(());
    }

    let (flow_name, script_path) = resolve_flow_script(repo, args.flow.as_deref())?;
    let cell_path = cell_script_path(repo, &script_path)?;

    println!("{} flow {} on {}", arrow(), bold(&flow_name), bold(cell));
    t.run_start(cell, &cell_path, &env_refs, attached)?;

    if args.detach {
        println!("  {} vitro logs -f", dim("follow:"));
    } else {
        // attached run is exec'd directly; output already streamed
    }
    Ok(())
}

fn git_state(repo: &git::Repo, branch: &str) -> serde_json::Value {
    let head = repo.rev_parse(branch).ok();
    let (ahead, behind) = repo.ahead_behind(branch, "main").unwrap_or((0, 0));
    serde_json::json!({
        "head": head,
        "ahead": ahead,
        "behind": behind,
    })
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
        "git": git_state(repo, name),
    })
}

fn cmd_status(repo: &git::Repo, cell: Option<&str>, json: bool) -> Result<()> {
    let Some(name) = cell else {
        // No name: fall back to list (matches existing UX). JSON falls
        // through to the list shape too, which is also documented.
        return cmd_list(Some(repo), false, json);
    };

    let active = find_cell_server(repo, name)?;
    let (status, ip) = if active.is_server() {
        let c = connect_remote(&active)?;
        let cells = c.list()?;
        let found = cells.iter().find(|c| c.name == name)
            .ok_or_else(|| anyhow::anyhow!("cell '{name}' not found"))?;
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
        println!("cell: {}", bold(name));
        println!("vm: {}", vm_status(&status));
        if let Some(ip) = &ip {
            println!("ip: {ip}");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(name: &str, status: &str, server: &str) -> CellRow {
        CellRow {
            name: name.to_string(),
            status: status.to_string(),
            server: server.to_string(),
            repo: Some("vitro".to_string()),
            ip: Some("192.168.83.42".to_string()),
            worktree: Some(format!("/repo/.vitro/trees/{name}")),
        }
    }

    #[test]
    fn list_json_shape_matches_readme_spec() {
        let rows = vec![row("feat-x", "running", "prod")];
        let v = list_to_json(&rows);
        // top-level shape per README "Machine-readable output"
        let cells = v.get("cells").unwrap().as_array().unwrap();
        assert_eq!(cells.len(), 1);
        let c = &cells[0];
        for k in &["name", "branch", "server", "status", "ip", "worktree", "started_at", "last_run"] {
            assert!(c.get(k).is_some(), "list JSON missing key: {k}");
        }
        assert_eq!(c["name"], "feat-x");
        assert_eq!(c["branch"], "feat-x");
        assert_eq!(c["server"], "prod");
        assert_eq!(c["status"], "running");
    }

    #[test]
    fn list_json_empty_emits_empty_array() {
        let v = list_to_json(&[]);
        assert_eq!(v["cells"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn list_json_unknown_fields_are_null() {
        let mut r = row("feat-x", "running", "prod");
        r.ip = None;
        let v = list_to_json(&[r]);
        let c = &v["cells"][0];
        // ip is part of the schema; null when unknown rather than missing.
        assert!(c["ip"].is_null());
        assert!(c["started_at"].is_null());
        assert!(c["last_run"].is_null());
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
