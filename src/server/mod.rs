use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use ironrdp_server::{Credentials, RdpServer, TlsIdentityCtx};

use crate::audio::HyprSoundFactory;
use crate::capture::{CaptureMode, HyprDisplay};
use crate::clipboard::HyprCliprdrFactory;
use crate::egfx::{EgfxShared, HyprGfxFactory};
use crate::input::{HyprInputHandler, SharedOutputLayout};

#[allow(clippy::too_many_arguments)]
pub async fn run(
    bind: &str,
    cert: Option<&str>,
    key: Option<&str>,
    username: &str,
    password: &str,
    resolution: (u32, u32),
    capture_mode: CaptureMode,
    bitrate: u32,
    quality: u8,
    fps: u32,
    output: Option<String>,
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
        bitrate,
        quality,
        fps,
        output,
    )
    .await
    .context("failed to initialize display capture")?;
    let (rdp_width, rdp_height) = display.dimensions();
    let input_handler = HyprInputHandler::new(rdp_width, rdp_height, output_layout)
        .context("failed to initialize input handler")?;

    let gfx_factory = HyprGfxFactory::new(egfx_shared);
    let cliprdr_factory = HyprCliprdrFactory::new();
    let sound_factory = HyprSoundFactory::new();

    let builder = RdpServer::builder().with_addr(addr);

    // Resolve TLS: explicit cert/key > auto-generated (always TLS)
    let (cert_path, key_path) = match (cert, key) {
        (Some(c), Some(k)) => (c.to_string(), k.to_string()),
        (Some(_), None) => anyhow::bail!("--cert provided without --key"),
        (None, Some(_)) => anyhow::bail!("--key provided without --cert"),
        (None, None) => {
            let (c, k) = auto_generate_tls().context("auto TLS certificate generation failed")?;
            tracing::info!("Using auto-generated TLS certificate");
            (c.to_string_lossy().into_owned(), k.to_string_lossy().into_owned())
        }
    };

    let tls_ctx =
        TlsIdentityCtx::init_from_paths(Path::new(&cert_path), Path::new(&key_path))
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

    server.set_credentials(Some(Credentials {
        username: username.to_string(),
        password: password.to_string(),
        domain: None,
    }));

    tracing::info!("RDP server listening on {}", addr);
    server.run().await.context("RDP server error")?;

    Ok(())
}

/// Auto-generate a self-signed TLS certificate and persist it.
/// Returns paths to (cert.pem, key.pem) in ~/.config/hypr-rdp/.
fn auto_generate_tls() -> Result<(PathBuf, PathBuf)> {
    let home = std::env::var("HOME").context("HOME not set")?;
    let config_dir = PathBuf::from(home).join(".config").join("hypr-rdp");
    let cert_path = config_dir.join("cert.pem");
    let key_path = config_dir.join("key.pem");

    // Reuse existing cert if both files exist
    if cert_path.exists() && key_path.exists() {
        tracing::info!("Reusing existing TLS certificate from {}", config_dir.display());
        return Ok((cert_path, key_path));
    }

    std::fs::create_dir_all(&config_dir)
        .context("failed to create config directory")?;

    let subject_alt_names = vec!["localhost".to_string(), "127.0.0.1".to_string()];
    let rcgen::CertifiedKey { cert, key_pair } =
        rcgen::generate_simple_self_signed(subject_alt_names)
            .context("failed to generate self-signed certificate")?;

    // Write to temp files then rename atomically to prevent mismatched cert/key
    // from concurrent starts
    let tmp_cert = config_dir.join(".cert.pem.tmp");
    let tmp_key = config_dir.join(".key.pem.tmp");

    std::fs::write(&tmp_cert, cert.pem())
        .context("failed to write cert.pem")?;
    std::fs::write(&tmp_key, key_pair.serialize_pem())
        .context("failed to write key.pem")?;

    std::fs::rename(&tmp_key, &key_path)
        .context("failed to finalize key.pem")?;
    std::fs::rename(&tmp_cert, &cert_path)
        .context("failed to finalize cert.pem")?;

    tracing::info!("Generated self-signed TLS certificate in {}", config_dir.display());
    Ok((cert_path, key_path))
}
