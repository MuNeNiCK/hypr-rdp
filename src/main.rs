mod audio;
mod capture;
mod clipboard;
mod config;
mod display;
mod egfx;
mod hyprland;
mod input;
mod server;

use anyhow::Result;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    let _ = rustls::crypto::ring::default_provider().install_default();

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("hypr_rdp=info")),
        )
        .init();

    let config = config::RuntimeConfig::load()?;
    tracing::info!("Starting hypr-rdp on {}", config.bind);

    let mut ctx = server::setup(config).await?;

    let result = tokio::select! {
        result = server::serve(&mut ctx) => result,
        _ = shutdown_signal() => {
            tracing::info!("Shutting down hypr-rdp");
            Ok(())
        }
    };

    ctx.display_handle.shutdown().await;

    result
}

async fn shutdown_signal() -> Result<()> {
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    tokio::select! {
        _ = sigterm.recv() => tracing::info!("Received SIGTERM"),
        _ = tokio::signal::ctrl_c() => tracing::info!("Received SIGINT"),
    }
    Ok(())
}
