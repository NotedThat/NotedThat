//! `notedthat-server` binary entry point.

use notedthat_server::config::Config;

/// Application entry point. All logic is in `notedthat_server::run::run`.
/// T20 will add the full run function; for now just validate config.
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _config = Config::from_env().map_err(|e| {
        eprintln!("startup error: {e}");
        e
    })?;
    Ok(())
}
