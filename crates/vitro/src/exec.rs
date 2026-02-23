/// Shell-safe command construction for multi-hop SSH execution.

/// Escape a string for safe embedding in a POSIX shell command.
/// Result is always wrapped in single quotes.
pub fn shell_escape(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Wrap a command for the client→server SSH hop.
/// Produces: vitro shell --server <env> -c <escaped_cmd>
pub fn vitro_hop(env: &str, cmd: &str) -> String {
    format!("vitro shell --server {} -c {}", env, shell_escape(cmd))
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
    fn vitro_hop_basic() {
        let s = vitro_hop("my-env", "ls -la");
        assert_eq!(s, "vitro shell --server my-env -c 'ls -la'");
    }

    #[test]
    fn vitro_hop_escapes_quotes() {
        let s = vitro_hop("env", "echo 'hi'");
        assert!(s.contains("-c 'echo '\\''hi'\\'''"));
    }

    #[test]
    fn vitro_hop_escapes_redirects() {
        let s = vitro_hop("env", "ls > /tmp/out");
        assert_eq!(s, "vitro shell --server env -c 'ls > /tmp/out'");
    }
}
