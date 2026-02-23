/// Shell-safe command construction for multi-hop SSH execution.

/// Escape a string for safe embedding in a POSIX shell command.
/// Result is always wrapped in single quotes.
pub fn shell_escape(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Build a detached command that survives SSH disconnect, writing a
/// pidfile while alive so concurrent runs can be detected and rejected.
pub fn detached(cmd: &str, log_path: &str) -> String {
    let log = shell_escape(log_path);
    let inner = format!(
        "echo $$ > /tmp/vitro/run.pid; trap 'rm -f /tmp/vitro/run.pid' EXIT; {}",
        cmd,
    );
    format!(
        "mkdir -p /tmp/vitro && setsid sh -c {} > {log} 2>&1 < /dev/null &",
        shell_escape(&inner),
    )
}

/// Build an attached command that records a pidfile while alive so
/// concurrent runs are detected and rejected.
pub fn attached_with_lock(cmd: &str) -> String {
    format!(
        "mkdir -p /tmp/vitro && (echo $$ > /tmp/vitro/run.pid; trap 'rm -f /tmp/vitro/run.pid' EXIT; {})",
        cmd,
    )
}

/// Shell snippet that prints "busy" if a run is in progress, else "idle".
pub fn run_busy_check() -> &'static str {
    "if [ -f /tmp/vitro/run.pid ] && kill -0 $(cat /tmp/vitro/run.pid) 2>/dev/null; then echo busy; else echo idle; fi"
}

/// Wrap a command for the client→server SSH hop.
/// Produces: vitro shell --server <cell> -c <escaped_cmd>
pub fn vitro_hop(cell: &str, cmd: &str) -> String {
    format!("vitro shell --server {} -c {}", cell, shell_escape(cmd))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_simple() {
        assert_eq!(shell_escape("hello"), "'hello'");
    }

    #[test]
    fn escape_single_quotes() {
        assert_eq!(shell_escape("it's"), "'it'\\''s'");
    }

    #[test]
    fn escape_empty() {
        assert_eq!(shell_escape(""), "''");
    }

    #[test]
    fn escape_special_chars() {
        assert_eq!(shell_escape("a > b & c"), "'a > b & c'");
    }

    #[test]
    fn detached_structure() {
        let s = detached("echo hi", "/tmp/run.log");
        assert!(s.contains("setsid sh -c"));
        assert!(s.contains("> '/tmp/run.log'"));
        assert!(s.contains("< /dev/null &"));
        assert!(s.starts_with("mkdir -p /tmp/vitro"));
    }

    #[test]
    fn detached_writes_pidfile() {
        let s = detached("echo hi", "/tmp/run.log");
        assert!(s.contains("/tmp/vitro/run.pid"));
        assert!(s.contains("trap"));
        assert!(s.contains("echo hi"));
    }

    #[test]
    fn detached_escapes_cmd() {
        let s = detached("echo 'hello'", "/tmp/out.log");
        // the command is wrapped in another layer of escaping for the pidfile prelude
        assert!(s.contains("echo '\\''hello'\\''"));
    }

    #[test]
    fn attached_with_lock_writes_pidfile() {
        let s = attached_with_lock("echo hi");
        assert!(s.contains("/tmp/vitro/run.pid"));
        assert!(s.contains("trap"));
        assert!(s.contains("echo hi"));
    }

    #[test]
    fn run_busy_check_emits_busy_or_idle() {
        let s = run_busy_check();
        assert!(s.contains("busy") && s.contains("idle"));
        assert!(s.contains("/tmp/vitro/run.pid"));
        assert!(s.contains("kill -0"));
    }

    #[test]
    fn vitro_hop_basic() {
        let s = vitro_hop("my-cell", "ls -la");
        assert_eq!(s, "vitro shell --server my-cell -c 'ls -la'");
    }

    #[test]
    fn vitro_hop_escapes_quotes() {
        let s = vitro_hop("cell", "echo 'hi'");
        assert!(s.contains("-c 'echo '\\''hi'\\'''"));
    }

    #[test]
    fn vitro_hop_escapes_redirects() {
        let s = vitro_hop("cell", "ls > /tmp/out");
        assert_eq!(s, "vitro shell --server cell -c 'ls > /tmp/out'");
    }
}
