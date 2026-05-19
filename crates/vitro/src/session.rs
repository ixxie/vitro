use anyhow::{Context, Result};
use std::path::PathBuf;
use std::process::Command;

const SESSIONS_DIR: &str = "/var/lib/vitro/sessions";

pub fn socket_dir(env: &str) -> PathBuf {
    PathBuf::from(SESSIONS_DIR).join(env)
}

pub fn socket_path(env: &str, name: &str) -> PathBuf {
    socket_dir(env).join(format!("{}.sock", name))
}

/// Build a dtach command that attaches to an existing session or creates one
/// if it doesn't exist. Suitable for interactive shells.
pub fn dtach_attach(env: &str, name: &str, inner: &str) -> String {
    let sock = shell_escape(&socket_path(env, name).display().to_string());
    let inner_escaped = shell_escape(inner);
    format!(
        "mkdir -p {} && dtach -A {} -r winch -z sh -c {}",
        shell_escape(&socket_dir(env).display().to_string()),
        sock,
        inner_escaped,
    )
}

/// List active dtach session names for an env by scanning the sockets dir.
pub fn list(env: &str) -> Result<Vec<String>> {
    let dir = socket_dir(env);
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut names = Vec::new();
    for entry in std::fs::read_dir(&dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.ends_with(".sock") {
            names.push(name[..name.len() - 5].to_string());
        }
    }
    names.sort();
    Ok(names)
}

/// Kill a dtach session by removing its socket. The dtach process itself
/// will exit when the last client detaches or when the controlled process
/// terminates.
pub fn kill(env: &str, name: &str) -> Result<()> {
    let path = socket_path(env, name);
    if path.exists() {
        std::fs::remove_file(&path).context("removing dtach socket")?;
    }
    Ok(())
}

/// Send text input to a running dtach session inside an env via SSH.
/// Attaches to the session's dtach socket with piped stdin.
pub fn send(env: &str, session_name: &str, text: &str, ssh_target: &str) -> Result<()> {
    let sock = socket_path(env, session_name);
    let sock_str = sock.to_string_lossy();
    // Feed text through stdin to dtach -a, which forwards it to the slave process.
    // dtach detaches automatically when stdin closes.
    let cmd = format!(
        "printf {} | dtach -a {} -r none",
        shell_escape(text),
        shell_escape(&sock_str),
    );
    let status = Command::new("ssh")
        .args([
            "-o", "StrictHostKeyChecking=no",
            "-o", "UserKnownHostsFile=/dev/null",
            "-o", "LogLevel=ERROR",
            ssh_target,
            &cmd,
        ])
        .status()
        .context("ssh failed for session send")?;
    if !status.success() {
        anyhow::bail!("session send failed (dtach may not be running)");
    }
    Ok(())
}

/// Return the path of the session log file for an env inside the VM.
/// Convention: /var/log/vitro/sessions/<name>.log (on the host, via persist).
pub fn log_path_host(env_dir: &std::path::Path, session_name: &str) -> PathBuf {
    env_dir.join("persist/var/log/vitro/sessions")
        .join(format!("{session_name}.log"))
}

fn shell_escape(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn socket_path_format() {
        let p = socket_path("my-env", "default");
        assert_eq!(
            p,
            PathBuf::from("/var/lib/vitro/sessions/my-env/default.sock")
        );
    }

    #[test]
    fn dtach_attach_includes_flags() {
        let s = dtach_attach("c", "s", "echo hi");
        assert!(s.contains("dtach -A"));
        assert!(s.contains("-r winch"));
        assert!(s.contains("-z"));
        assert!(s.contains("s.sock"));
    }

    #[test]
    fn dtach_escapes_inner_quotes() {
        let s = dtach_attach("c", "s", "echo 'hi'");
        assert!(s.contains("echo '\\''hi'\\'''"));
    }
}
