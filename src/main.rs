mod audio;
mod capture;
mod clipboard;
mod config;
mod display;
mod egfx;
mod failure_evidence;
mod hyprland;
mod input;
mod server;

use anyhow::Result;
use tracing_subscriber::EnvFilter;

const DEFAULT_LOG_FILTER: &str = "hypr_rdp=info,ironrdp_server=error";

#[tokio::main]
async fn main() -> Result<()> {
    if let Some(status) = server::auth::dispatch_helper() {
        std::process::exit(status);
    }
    let _ = rustls::crypto::ring::default_provider().install_default();

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new(DEFAULT_LOG_FILTER)),
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
    // Route SIGHUP through graceful shutdown so active hooks can drain.
    let mut sighup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())?;
    tokio::select! {
        _ = sigterm.recv() => tracing::info!("Received SIGTERM"),
        _ = sighup.recv() => tracing::info!("Received SIGHUP"),
        _ = tokio::signal::ctrl_c() => tracing::info!("Received SIGINT"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{self, Write};
    use std::sync::{Arc, Mutex};

    struct CapturedLog(Arc<Mutex<Vec<u8>>>);

    impl Write for CapturedLog {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn default_logging_reports_rdp_connection_errors() {
        let captured = Arc::new(Mutex::new(Vec::new()));
        let writer = captured.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_env_filter(EnvFilter::new(DEFAULT_LOG_FILTER))
            .without_time()
            .with_ansi(false)
            .with_writer(move || CapturedLog(writer.clone()))
            .finish();

        tracing::subscriber::with_default(subscriber, || {
            tracing::error!(target: "ironrdp_server::server", "connection-error-regression");
            tracing::info!(target: "hypr_rdp::server", "application-info-regression");
            tracing::warn!(target: "ironrdp_server::server", "suppressed-slow-writer-warning");
            tracing::debug!(target: "ironrdp_server::server", "suppressed-protocol-debug");
            tracing::error!(target: "unrelated_dependency", "suppressed-unrelated-error");
            tracing::debug!(target: "hypr_rdp::server", "suppressed-application-debug");
        });

        let output = String::from_utf8(captured.lock().unwrap().clone()).unwrap();
        assert!(output.contains("connection-error-regression"), "{output}");
        assert!(output.contains("application-info-regression"), "{output}");
        assert!(!output.contains("suppressed-"), "{output}");
    }
}
