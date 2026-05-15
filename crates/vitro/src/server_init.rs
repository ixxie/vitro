use anyhow::{Context, Result};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const PROBE_SCRIPT: &str = r#"set -e
if [ -d /sys/firmware/efi ]; then echo "firmware=uefi"; else echo "firmware=bios"; fi
ROOT_DEV=$(lsblk -dn -o NAME,TYPE 2>/dev/null | awk '$2=="disk" {print $1; exit}')
if [ -z "$ROOT_DEV" ]; then echo "ERROR: no disk candidate found via lsblk" >&2; exit 1; fi
echo "disk=/dev/$ROOT_DEV"
SIZE=$(lsblk -dn -o SIZE "/dev/$ROOT_DEV" 2>/dev/null | tr -d ' ')
echo "disk_size=${SIZE:-unknown}"
BY_ID=""
for prefix in nvme- ata- scsi- wwn-; do
  for f in /dev/disk/by-id/${prefix}*; do
    [ -e "$f" ] || continue
    case "$f" in *-part*) continue;; esac
    if [ "$(readlink -f "$f")" = "/dev/$ROOT_DEV" ]; then
      BY_ID="$f"
      break 2
    fi
  done
done
if [ -z "$BY_ID" ]; then BY_ID="/dev/$ROOT_DEV"; fi
echo "disk_by_id=$BY_ID"
if [ -f /etc/os-release ]; then . /etc/os-release; echo "os=${ID:-unknown}"; else echo "os=unknown"; fi
"#;

struct Probe {
    firmware: String,
    disk: String,
    disk_by_id: String,
    disk_size: String,
    os: String,
}

pub fn run(name: &str, target: &str, ssh_key: Option<&Path>) -> Result<()> {
    validate_name(name)?;

    let cwd = std::env::current_dir().context("getting current directory")?;
    let host_dir = cwd.join("hosts").join(name);
    if host_dir.exists() {
        anyhow::bail!(
            "hosts/{name}/ already exists — remove it or choose a different name"
        );
    }
    if registry_has(name)? {
        anyhow::bail!(
            "server '{name}' is already in ~/.config/vitro/servers.toml"
        );
    }

    let pubkey = read_pubkey(ssh_key)?;

    println!("probing {target}...");
    let probe = probe(target, ssh_key)
        .with_context(|| format!("probing {target}"))?;

    println!("  firmware: {}", probe.firmware);
    println!("  disk:     {} ({})", probe.disk_by_id, probe.disk_size);
    if probe.disk_by_id == probe.disk {
        println!("            (no /dev/disk/by-id symlink; path may be unstable)");
    }
    println!("  os:       {}", probe.os);
    println!("  ssh key:  {}", pubkey_preview(&pubkey));

    if probe.os != "nixos" {
        println!();
        println!("⚠ this host is NOT running NixOS (detected: {}).", probe.os);
        println!("  `vitro server deploy` will use nixos-anywhere to install NixOS.");
        println!("  The existing root disk WILL BE WIPED.");
        println!();
    }

    std::fs::create_dir_all(&host_dir).context("creating hosts/<name>/")?;
    write_disk_nix(&host_dir, &probe)?;
    write_config_nix(&host_dir, &pubkey)?;

    let has_flake = cwd.join("flake.nix").exists();
    if has_flake {
        println!("wrote hosts/{name}/disk.nix");
        println!("wrote hosts/{name}/config.nix");
        println!();
        print_wiring_snippet(name, &pubkey);
    } else {
        write_flake_nix(&cwd, name, &pubkey)?;
        println!("wrote flake.nix");
        println!("wrote hosts/{name}/disk.nix");
        println!("wrote hosts/{name}/config.nix");
    }

    append_registry(name, target)?;
    println!();
    println!("registered '{name}' → {target} in ~/.config/vitro/servers.toml");
    println!();
    if probe.os == "nixos" {
        println!("next: vitro server deploy");
    } else {
        println!("next: vitro server deploy  (will reinstall the target as NixOS)");
    }

    Ok(())
}

fn probe(target: &str, ssh_key: Option<&Path>) -> Result<Probe> {
    let stdout = ssh_exec(target, ssh_key, PROBE_SCRIPT)?;

    let mut p = Probe {
        firmware: String::new(),
        disk: String::new(),
        disk_by_id: String::new(),
        disk_size: String::new(),
        os: String::new(),
    };
    for line in stdout.lines() {
        if let Some((k, v)) = line.split_once('=') {
            match k {
                "firmware" => p.firmware = v.to_string(),
                "disk" => p.disk = v.to_string(),
                "disk_by_id" => p.disk_by_id = v.to_string(),
                "disk_size" => p.disk_size = v.to_string(),
                "os" => p.os = v.to_string(),
                _ => {}
            }
        }
    }
    if p.firmware.is_empty() || p.disk.is_empty() || p.disk_by_id.is_empty() {
        anyhow::bail!(
            "probe returned incomplete output (firmware/disk/disk_by_id missing):\n{stdout}"
        );
    }
    Ok(p)
}

fn ssh_exec(target: &str, ssh_key: Option<&Path>, script: &str) -> Result<String> {
    let mut cmd = Command::new("ssh");
    cmd.args([
        "-o", "StrictHostKeyChecking=accept-new",
        "-o", "ConnectTimeout=15",
        "-o", "BatchMode=yes",
    ]);
    if let Some(key) = ssh_key {
        cmd.arg("-i").arg(key);
    }
    cmd.arg(target).arg("bash").arg("-s");

    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("spawning ssh")?;

    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(script.as_bytes())
        .context("writing probe script to ssh stdin")?;

    let output = child.wait_with_output().context("waiting for ssh")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!(
            "ssh exit {:?}:\n{stderr}",
            output.status.code()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

fn read_pubkey(ssh_key: Option<&Path>) -> Result<String> {
    let path = match ssh_key {
        Some(p) => {
            let pub_path = if p.extension().and_then(|s| s.to_str()) == Some("pub") {
                p.to_path_buf()
            } else {
                let mut s = p.as_os_str().to_os_string();
                s.push(".pub");
                PathBuf::from(s)
            };
            pub_path
        }
        None => {
            let home = std::env::var("HOME").context("HOME not set")?;
            PathBuf::from(home).join(".ssh/id_ed25519.pub")
        }
    };
    let content = std::fs::read_to_string(&path)
        .with_context(|| format!("reading SSH pubkey at {}", path.display()))?;
    let trimmed = content.trim().to_string();
    if trimmed.is_empty() {
        anyhow::bail!("SSH pubkey file {} is empty", path.display());
    }
    Ok(trimmed)
}

fn pubkey_preview(pubkey: &str) -> String {
    let mut parts = pubkey.split_whitespace();
    let kind = parts.next().unwrap_or("");
    let body = parts.next().unwrap_or("");
    let truncated = if body.len() > 16 { &body[..16] } else { body };
    format!("{kind} {truncated}…")
}

fn write_disk_nix(host_dir: &Path, probe: &Probe) -> Result<()> {
    let content = if probe.firmware == "uefi" {
        uefi_disk_nix(&probe.disk_by_id)
    } else {
        bios_disk_nix(&probe.disk_by_id)
    };
    std::fs::write(host_dir.join("disk.nix"), content).context("writing disk.nix")
}

fn uefi_disk_nix(device: &str) -> String {
    format!(
        r#"{{
  boot.loader.systemd-boot.enable = true;
  boot.loader.efi.canTouchEfiVariables = true;

  disko.devices.disk.main = {{
    type = "disk";
    device = "{device}";
    content = {{
      type = "gpt";
      partitions = {{
        boot = {{
          size = "512M";
          type = "EF00";
          content = {{
            type = "filesystem";
            format = "vfat";
            mountpoint = "/boot";
          }};
        }};
        root = {{
          size = "100%";
          content = {{
            type = "filesystem";
            format = "ext4";
            mountpoint = "/";
          }};
        }};
      }};
    }};
  }};
}}
"#
    )
}

fn bios_disk_nix(device: &str) -> String {
    format!(
        r#"{{
  boot.loader.grub.enable = true;

  disko.devices.disk.main = {{
    type = "disk";
    device = "{device}";
    content = {{
      type = "gpt";
      partitions = {{
        bios = {{
          size = "1M";
          type = "EF02";
        }};
        root = {{
          size = "100%";
          content = {{
            type = "filesystem";
            format = "ext4";
            mountpoint = "/";
          }};
        }};
      }};
    }};
  }};
}}
"#
    )
}

fn write_config_nix(host_dir: &Path, pubkey: &str) -> Result<()> {
    let content = format!(
        r#"{{pkgs, lib, ...}}: {{
  vitro.server = {{
    enable = true;
    user.authorizedKeys = [
      "{pubkey}"
    ];
    gc.enable = true;
  }};
}}
"#
    );
    std::fs::write(host_dir.join("config.nix"), content).context("writing config.nix")
}

fn write_flake_nix(cwd: &Path, name: &str, pubkey: &str) -> Result<()> {
    let content = format!(
        r#"{{
  description = "{name} — vitro server host";

  inputs = {{
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    disko = {{
      url = "github:nix-community/disko";
      inputs.nixpkgs.follows = "nixpkgs";
    }};
    vitro.url = "github:ixxie/vitro";
  }};

  outputs = {{
    self,
    nixpkgs,
    disko,
    vitro,
  }}:
    (vitro.lib.mkHost {{inherit vitro nixpkgs disko;}}) {{
      name = "{name}";
      disk = ./hosts/{name}/disk.nix;
      sshPubkey = "{pubkey}";
      config = ./hosts/{name}/config.nix;
    }};
}}
"#
    );
    std::fs::write(cwd.join("flake.nix"), content).context("writing flake.nix")
}

fn print_wiring_snippet(name: &str, pubkey: &str) {
    println!("flake.nix already exists. Add these inputs (if not already present):");
    println!();
    println!("    disko.url = \"github:nix-community/disko\";");
    println!("    disko.inputs.nixpkgs.follows = \"nixpkgs\";");
    println!("    vitro.url = \"github:ixxie/vitro\";");
    println!();
    println!("And in outputs, alongside your existing nixosConfigurations:");
    println!();
    println!("    nixosConfigurations.{name} =");
    println!("      (vitro.lib.mkHost {{inherit vitro nixpkgs disko;}}) {{");
    println!("        name = \"{name}\";");
    println!("        disk = ./hosts/{name}/disk.nix;");
    println!("        sshPubkey = \"{pubkey}\";");
    println!("        config = ./hosts/{name}/config.nix;");
    println!("      }};");
}

fn validate_name(name: &str) -> Result<()> {
    if name.is_empty() {
        anyhow::bail!("server name cannot be empty");
    }
    if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        anyhow::bail!(
            "invalid server name '{name}': use ascii letters, digits, '-', '_'"
        );
    }
    Ok(())
}

fn registry_has(name: &str) -> Result<bool> {
    let registry = crate::server::load_registry()?;
    Ok(registry.contains_key(name))
}

fn append_registry(name: &str, target: &str) -> Result<()> {
    let home = std::env::var("HOME").context("HOME not set")?;
    let path = PathBuf::from(home).join(".config/vitro/servers.toml");
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).context("creating ~/.config/vitro")?;
    }
    let existing = if path.exists() {
        std::fs::read_to_string(&path).context("reading servers.toml")?
    } else {
        String::new()
    };
    let separator = if existing.is_empty() || existing.ends_with('\n') {
        ""
    } else {
        "\n"
    };
    let entry = format!("{separator}[{name}]\ntarget = \"{target}\"\n");
    let mut combined = existing;
    combined.push_str(&entry);
    std::fs::write(&path, combined).context("writing servers.toml")
}
