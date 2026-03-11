use std::net::SocketAddr;
use std::path::Path;

use anyhow::{Context, Result};
use ironrdp_server::RdpServer;

use crate::capture::HyprDisplay;
use crate::input::HyprInputHandler;

pub async fn run(bind: &str, cert: Option<&str>, key: Option<&str>) -> Result<()> {
    let addr: SocketAddr = bind.parse().context("invalid bind address")?;

    let display = HyprDisplay::new().await.context("failed to initialize display capture")?;
    let input_handler = HyprInputHandler::new().context("failed to initialize input handler")?;

    let builder = RdpServer::builder().with_addr(addr);

    let mut server = match (cert, key) {
        (Some(cert_path), Some(key_path)) => {
            use ironrdp_server::TlsIdentityCtx;
            let tls_ctx = TlsIdentityCtx::init_from_paths(Path::new(cert_path), Path::new(key_path))
                .context("failed to load TLS certificates")?;
            let acceptor = tls_ctx.make_acceptor().context("failed to create TLS acceptor")?;
            builder
                .with_tls(acceptor)
                .with_input_handler(input_handler)
                .with_display_handler(display)
                .build()
        }
        _ => {
            tracing::warn!("Running without TLS - use --cert and --key for production");
            builder
                .with_no_security()
                .with_input_handler(input_handler)
                .with_display_handler(display)
                .build()
        }
    };

    tracing::info!("RDP server listening on {}", addr);
    server.run().await.context("RDP server error")?;

    Ok(())
}
