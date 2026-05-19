use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use ironrdp_server::{Credentials, RdpServer, TlsIdentityCtx};

use crate::audio::HyprSoundFactory;
use crate::capture::{HyprDisplay, HyprDisplayHandle};
use crate::clipboard::HyprCliprdrFactory;
use crate::config::RuntimeConfig;
use crate::egfx::{EgfxShared, HyprGfxFactory};
use crate::input::{HyprInputHandler, SharedOutputLayout};

mod tls;

pub struct ServerContext {
    server: RdpServer,
    pub display_handle: HyprDisplayHandle,
}

pub async fn setup(config: RuntimeConfig) -> Result<ServerContext> {
    let RuntimeConfig {
        bind,
        cert,
        key,
        username,
        password,
        resolution,
        capture_mode,
        bitrate,
        quality,
        rate_control,
        fps,
        max_frames_in_flight,
        resolution_fixed,
        output,
    } = config;

    let addr: SocketAddr = bind.parse().context("invalid bind address")?;

    let egfx_shared = Arc::new(EgfxShared::new(max_frames_in_flight));
    let output_layout = Arc::new(SharedOutputLayout::new());

    let (display, display_handle, (rdp_width, rdp_height)) = HyprDisplay::new(
        resolution,
        capture_mode,
        Arc::clone(&egfx_shared),
        Arc::clone(&output_layout),
        bitrate,
        quality,
        rate_control,
        fps,
        resolution_fixed,
        output,
    )
    .await
    .context("failed to initialize display capture")?;
    egfx_shared.set_surface_size(rdp_width, rdp_height);
    let input_handler = HyprInputHandler::new(rdp_width, rdp_height, output_layout)
        .context("failed to initialize input handler")?;

    let gfx_factory = HyprGfxFactory::new(Arc::clone(&egfx_shared));
    let cliprdr_factory = HyprCliprdrFactory::new();
    let sound_factory = HyprSoundFactory::new();

    let builder = RdpServer::builder().with_addr(addr);

    let (cert_path, key_path) = tls::resolve_tls_paths(cert.as_deref(), key.as_deref())?;

    let tls_ctx = TlsIdentityCtx::init_from_paths(Path::new(&cert_path), Path::new(&key_path))
        .context("failed to load TLS certificates")?;
    let acceptor = tls_ctx
        .make_acceptor()
        .context("failed to create TLS acceptor")?;

    let mut server = builder
        .with_tls(acceptor)
        .with_input_handler(input_handler)
        .with_display_handler(display)
        .with_gfx_factory(Some(Box::new(gfx_factory)))
        .with_cliprdr_factory(Some(Box::new(cliprdr_factory)))
        .with_sound_factory(Some(Box::new(sound_factory)))
        .build();

    if username.is_empty() && password.is_empty() {
        server.set_credentials(None);
    } else {
        server.set_credentials(Some(Credentials {
            username: username.to_string(),
            password: password.to_string(),
            domain: None,
        }));
    }

    tracing::info!("RDP server configured for {}", addr);

    Ok(ServerContext {
        server,
        display_handle,
    })
}

pub async fn serve(ctx: &mut ServerContext) -> Result<()> {
    ctx.server.run().await
}
