// Standalone binary for git's remote-helper protocol. The flake's
// postInstall used to symlink `vitro` → `git-remote-vitro`; this is the
// pure-cargo equivalent so `cargo build` produces a working pair.

fn main() -> anyhow::Result<()> {
    vitro::remote::run()
}
