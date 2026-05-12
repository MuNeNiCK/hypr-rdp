use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use ironrdp_server::{Credentials, RdpServer, TlsIdentityCtx};

use crate::audio::HyprSoundFactory;
use crate::capture::{CaptureMode, HyprDisplay, HyprDisplayHandle};
use crate::clipboard::HyprCliprdrFactory;
use crate::egfx::{EgfxShared, H264RateControl, HyprGfxFactory};
use crate::input::{HyprInputHandler, SharedOutputLayout};

pub struct ServerContext {
    server: RdpServer,
    pub display_handle: HyprDisplayHandle,
}

#[allow(clippy::too_many_arguments)]
pub async fn setup(
    bind: &str,
    cert: Option<&str>,
    key: Option<&str>,
    username: &str,
    password: &str,
    resolution: (u32, u32),
    capture_mode: CaptureMode,
    bitrate: u32,
    quality: u8,
    rate_control: H264RateControl,
    fps: u32,
    max_frames_in_flight: u32,
    resolution_fixed: bool,
    output: Option<String>,
) -> Result<ServerContext> {
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

    let (cert_path, key_path) = match (cert, key) {
        (Some(c), Some(k)) => (c.to_string(), k.to_string()),
        (Some(_), None) => anyhow::bail!("--cert provided without --key"),
        (None, Some(_)) => anyhow::bail!("--key provided without --cert"),
        (None, None) => {
            let (c, k) = auto_generate_tls().context("auto TLS certificate generation failed")?;
            tracing::info!("Using auto-generated TLS certificate");
            (
                c.to_string_lossy().into_owned(),
                k.to_string_lossy().into_owned(),
            )
        }
    };

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

/// Auto-generate a self-signed TLS certificate and persist it.
fn auto_generate_tls() -> Result<(PathBuf, PathBuf)> {
    use std::os::unix::fs::OpenOptionsExt;

    let home = std::env::var("HOME").context("HOME not set")?;
    let config_dir = PathBuf::from(home).join(".config").join("hypr-rdp");
    let cert_path = config_dir.join("cert.pem");
    let key_path = config_dir.join("key.pem");

    if cert_path.exists() && key_path.exists() {
        tracing::info!(
            "Reusing existing TLS certificate from {}",
            config_dir.display()
        );
        return Ok((cert_path, key_path));
    }

    std::fs::create_dir_all(&config_dir).context("failed to create config directory")?;

    let lock_path = config_dir.join(".tls.lock");
    let lock_file = std::fs::File::create(&lock_path).context("failed to create TLS lock file")?;
    let lock_fd = std::os::fd::AsRawFd::as_raw_fd(&lock_file);
    let ret = unsafe { libc::flock(lock_fd, libc::LOCK_EX) };
    if ret != 0 {
        anyhow::bail!("failed to acquire TLS lock");
    }

    if cert_path.exists() && key_path.exists() {
        tracing::info!(
            "Reusing existing TLS certificate from {}",
            config_dir.display()
        );
        return Ok((cert_path, key_path));
    }

    let subject_alt_names = vec!["localhost".to_string(), "127.0.0.1".to_string()];
    let rcgen::CertifiedKey { cert, key_pair } =
        rcgen::generate_simple_self_signed(subject_alt_names)
            .context("failed to generate self-signed certificate")?;

    let tmp_key = config_dir.join(".key.pem.tmp");
    {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp_key)
            .context("failed to create key.pem")?;
        std::io::Write::write_all(&mut f, key_pair.serialize_pem().as_bytes())
            .context("failed to write key.pem")?;
    }

    let tmp_cert = config_dir.join(".cert.pem.tmp");
    std::fs::write(&tmp_cert, cert.pem()).context("failed to write cert.pem")?;

    std::fs::rename(&tmp_key, &key_path).context("failed to finalize key.pem")?;
    std::fs::rename(&tmp_cert, &cert_path).context("failed to finalize cert.pem")?;

    tracing::info!(
        "Generated self-signed TLS certificate in {}",
        config_dir.display()
    );
    Ok((cert_path, key_path))
}
