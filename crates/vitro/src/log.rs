use std::path::PathBuf;
use tracing_subscriber::{fmt, EnvFilter, Layer, layer::SubscriberExt, util::SubscriberInitExt};

fn log_dir() -> PathBuf {
    // server: /var/log/vitro/ exists and is writable (created by tmpfiles)
    let server_dir = PathBuf::from("/var/log/vitro");
    if server_dir.exists() && is_writable(&server_dir) {
        return server_dir;
    }

    // client: ~/.local/share/vitro/
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    let client_dir = PathBuf::from(home).join(".local/share/vitro");
    std::fs::create_dir_all(&client_dir).ok();
    client_dir
}

fn is_writable(path: &PathBuf) -> bool {
    let test = path.join(".write-test");
    if std::fs::write(&test, "").is_ok() {
        std::fs::remove_file(&test).ok();
        true
    } else {
        false
    }
}

pub fn init() {
    let stderr_filter = EnvFilter::try_from_env("VITRO_LOG")
        .unwrap_or_else(|_| EnvFilter::new("warn"));
    let stderr_layer = fmt::layer()
        .with_writer(std::io::stderr)
        .with_ansi(true)
        .with_target(false)
        .with_level(true)
        .compact()
        .with_filter(stderr_filter);

    // file logging is best-effort — sandboxed / read-only environments
    // (CI, claude-code's command sandbox, etc.) don't get a file but
    // the binary still runs.
    let dir = log_dir();
    let file_layer = if is_writable(&dir) {
        let file_appender = tracing_appender::rolling::never(&dir, "vitro.log");
        Some(
            fmt::layer()
                .with_writer(file_appender)
                .with_ansi(false)
                .with_target(true)
                .with_thread_ids(false),
        )
    } else {
        None
    };

    tracing_subscriber::registry()
        .with(file_layer)
        .with(stderr_layer)
        .init();
}
