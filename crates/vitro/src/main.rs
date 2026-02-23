use anyhow::Result;
use vitro::{cli, log};

fn main() -> Result<()> {
    let is_secrets = std::env::args().nth(1).as_deref() == Some("secrets");
    if !is_secrets {
        log::init();
    }
    cli::run()
}
