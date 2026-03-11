use std::net::SocketAddr;
use std::path::Path;

use anyhow::{Context, Result};
use ironrdp_server::{Credentials, RdpServer};

use crate::capture::HyprDisplay;
use crate::input::HyprInputHandler;

pub async fn run(
    bind: &str,
    cert: Option<&str>,
    key: Option<&str>,
    username: &str,
    password: &str,
    resolution: (u32, u32),
) -> Result<()> {
    let addr: SocketAddr = bind.parse().context("invalid bind address")?;

    let display = HyprDisplay::new(resolution).await.context("failed to initialize display capture")?;
    let (rdp_width, rdp_height) = display.dimensions();
    let output_name = display.output_name().to_string();
    let input_handler = HyprInputHandler::new(rdp_width, rdp_height, &output_name).context("failed to initialize input handler")?;

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

    server.set_credentials(Some(Credentials {
        username: username.to_string(),
        password: password.to_string(),
        domain: None,
    }));

    tracing::info!("RDP server listening on {}", addr);
    server.run().await.context("RDP server error")?;

    Ok(())
}
