use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use ironrdp_server::{Credentials, RdpServer};

use crate::capture::{CaptureMode, HyprDisplay};
use crate::egfx::{EgfxShared, HyprGfxFactory};
use crate::input::{HyprInputHandler, SharedOutputLayout};

pub async fn run(
    bind: &str,
    cert: Option<&str>,
    key: Option<&str>,
    username: &str,
    password: &str,
    resolution: (u32, u32),
    capture_mode: CaptureMode,
) -> Result<()> {
    let addr: SocketAddr = bind.parse().context("invalid bind address")?;

    // Create shared EGFX state before display so capture thread has it from the start
    let egfx_shared = Arc::new(EgfxShared::new());
    let output_layout = Arc::new(SharedOutputLayout::new());

    let display = HyprDisplay::new(
        resolution,
        capture_mode,
        Arc::clone(&egfx_shared),
        Arc::clone(&output_layout),
    )
    .await
    .context("failed to initialize display capture")?;
    let (rdp_width, rdp_height) = display.dimensions();
    let input_handler = HyprInputHandler::new(rdp_width, rdp_height, output_layout)
        .context("failed to initialize input handler")?;

    let gfx_factory = HyprGfxFactory::new(egfx_shared);

    let builder = RdpServer::builder().with_addr(addr);

    let mut server = match (cert, key) {
        (Some(cert_path), Some(key_path)) => {
            use ironrdp_server::TlsIdentityCtx;
            let tls_ctx =
                TlsIdentityCtx::init_from_paths(Path::new(cert_path), Path::new(key_path))
                    .context("failed to load TLS certificates")?;
            let acceptor = tls_ctx
                .make_acceptor()
                .context("failed to create TLS acceptor")?;
            builder
                .with_tls(acceptor)
                .with_input_handler(input_handler)
                .with_display_handler(display)
                .with_gfx_factory(Some(Box::new(gfx_factory)))
                .build()
        }
        _ => {
            tracing::warn!("Running without TLS - use --cert and --key for production");
            builder
                .with_no_security()
                .with_input_handler(input_handler)
                .with_display_handler(display)
                .with_gfx_factory(Some(Box::new(gfx_factory)))
                .build()
        }
    };

    server.set_credentials(Some(Credentials {
        username: username.to_string(),
        password: password.to_string(),
        domain: None,
    }));

    tracing::info!("RDP server listening on {}", addr);
    server.run().await.context("RDP server error")?;

    Ok(())
}
