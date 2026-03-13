mod audio;
mod capture;
mod clipboard;
mod egfx;
mod input;
mod server;

use anyhow::Result;
use clap::Parser;
use tracing_subscriber::EnvFilter;

#[derive(Parser, Debug)]
#[command(name = "hypr-rdp", about = "Native RDP server for Hyprland")]
struct Args {
    /// Address to bind the RDP server
    #[arg(short, long, default_value = "0.0.0.0:3389")]
    bind: String,

    /// TLS certificate file (PEM)
    #[arg(long)]
    cert: Option<String>,

    /// TLS private key file (PEM)
    #[arg(long)]
    key: Option<String>,

    /// Username for RDP authentication
    #[arg(short, long, default_value = "")]
    username: String,

    /// Password for RDP authentication
    #[arg(short, long, default_value = "")]
    password: String,

    /// RDP session resolution (WxH), e.g. 1920x1080
    #[arg(short, long, default_value = "1920x1080")]
    resolution: String,

    /// Screen capture protocol: "wlr" (wlr-screencopy-v1) or "ext" (ext-image-copy-capture-v1)
    #[arg(long, default_value = "wlr")]
    capture_mode: String,

    /// H.264 encoder bitrate in bps
    #[arg(long, default_value_t = 5_000_000)]
    bitrate: u32,

    /// H.264 quality level (0-51, lower = better)
    #[arg(long, default_value_t = 23)]
    quality: u8,

    /// Maximum capture frame rate
    #[arg(long, default_value_t = 30)]
    fps: u32,

    /// Capture a specific output instead of creating a headless one
    #[arg(long)]
    output: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("hypr_rdp=info")))
        .init();

    let args = Args::parse();

    let resolution = parse_resolution(&args.resolution)?;
    let capture_mode = match args.capture_mode.as_str() {
        "ext" => capture::CaptureMode::Ext,
        "wlr" => capture::CaptureMode::Wlr,
        other => anyhow::bail!("unknown capture mode '{}', expected 'ext' or 'wlr'", other),
    };

    if args.quality > 51 {
        anyhow::bail!("quality must be 0-51");
    }
    if args.fps == 0 {
        anyhow::bail!("fps must be > 0");
    }

    tracing::info!("Starting hypr-rdp on {}", args.bind);

    // Spawn signal handler as independent task — process::exit(0)
    // ensures termination even if server.run() blocks the runtime.
    tokio::spawn(async {
        if shutdown_signal().await.is_ok() {
            tracing::info!("Shutting down hypr-rdp");
            std::process::exit(0);
        }
    });

    server::run(
        &args.bind,
        args.cert.as_deref(),
        args.key.as_deref(),
        &args.username,
        &args.password,
        resolution,
        capture_mode,
        args.bitrate,
        args.quality,
        args.fps,
        args.output,
    )
    .await
}

fn parse_resolution(s: &str) -> anyhow::Result<(u32, u32)> {
    let parts: Vec<&str> = s.split('x').collect();
    if parts.len() != 2 {
        anyhow::bail!("invalid resolution format, expected WxH (e.g. 1920x1080)");
    }
    let w: u32 = parts[0]
        .parse()
        .map_err(|_| anyhow::anyhow!("invalid width"))?;
    let h: u32 = parts[1]
        .parse()
        .map_err(|_| anyhow::anyhow!("invalid height"))?;
    Ok((w, h))
}

async fn shutdown_signal() -> Result<()> {
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    tokio::select! {
        _ = sigterm.recv() => tracing::info!("Received SIGTERM"),
        _ = tokio::signal::ctrl_c() => tracing::info!("Received SIGINT"),
    }
    Ok(())
}
