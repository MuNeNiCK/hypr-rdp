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
        egfx_codec,
        resolution_fixed,
        output,
    } = config;

    let addr = parse_bind_addr(&bind)?;

    let egfx_shared = Arc::new(EgfxShared::with_codec_policy(
        max_frames_in_flight,
        egfx_codec,
    ));
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

    server.set_credentials(credentials_from_config(&username, &password));

    tracing::info!("RDP server configured for {}", addr);

    Ok(ServerContext {
        server,
        display_handle,
    })
}

pub async fn serve(ctx: &mut ServerContext) -> Result<()> {
    ctx.server.run().await
}

fn credentials_from_config(username: &str, password: &str) -> Option<Credentials> {
    if username.is_empty() && password.is_empty() {
        None
    } else {
        Some(Credentials {
            username: username.to_string(),
            password: password.to_string(),
            domain: None,
        })
    }
}

fn parse_bind_addr(bind: &str) -> Result<SocketAddr> {
    bind.parse().context("invalid bind address")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_username_and_password_disable_authentication() {
        assert!(credentials_from_config("", "").is_none());
    }

    #[test]
    fn non_empty_username_or_password_enables_authentication() {
        let with_both = credentials_from_config("user", "pass").expect("credentials");
        assert_eq!(with_both.username, "user");
        assert_eq!(with_both.password, "pass");
        assert_eq!(with_both.domain, None);

        let with_username = credentials_from_config("user", "").expect("credentials");
        assert_eq!(with_username.username, "user");
        assert_eq!(with_username.password, "");

        let with_password = credentials_from_config("", "pass").expect("credentials");
        assert_eq!(with_password.username, "");
        assert_eq!(with_password.password, "pass");
    }

    #[test]
    fn invalid_bind_address_is_rejected_before_server_setup() {
        let error = parse_bind_addr("not an address").expect_err("invalid bind must fail");

        assert!(format!("{error:#}").contains("invalid bind address"));
    }
}
