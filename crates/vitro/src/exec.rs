/// Shell-safe command construction for multi-hop SSH execution.

/// Escape a string for safe embedding in a POSIX shell command.
/// Result is always wrapped in single quotes.
pub fn shell_escape(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Wrap a command for the client→server SSH hop.
/// Produces: vitro shell --server <env> -c <escaped_cmd>
/// If a session is given, appends --session <name>.
pub fn vitro_hop(env: &str, cmd: &str, session: Option<&str>) -> String {
    let mut s = format!("vitro shell --server {}", env);
    if let Some(sess) = session {
        s.push_str(&format!(" --session {}", shell_escape(sess)));
    }
    s.push_str(&format!(" -c {}", shell_escape(cmd)));
    s
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
    fn vitro_hop_with_session() {
        let s = vitro_hop("my-env", "ls -la", Some("agent"));
        assert!(s.contains("--server my-env"));
        assert!(s.contains("--session 'agent'"));
        assert!(s.contains("-c 'ls -la'"));
    }

    #[test]
    fn vitro_hop_without_session() {
        let s = vitro_hop("my-env", "ls -la", None);
        assert_eq!(s, "vitro shell --server my-env -c 'ls -la'");
    }

    #[test]
    fn vitro_hop_escapes_quotes() {
        let s = vitro_hop("env", "echo 'hi'", None);
        assert!(s.contains("-c 'echo '\\''hi'\\'''"));
    }

    #[test]
    fn vitro_hop_escapes_redirects() {
        let s = vitro_hop("env", "ls > /tmp/out", None);
        assert_eq!(s, "vitro shell --server env -c 'ls > /tmp/out'");
    }
}
