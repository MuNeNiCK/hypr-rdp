mod capture;
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
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive("hypr_rdp=info".parse()?))
        .init();

    let args = Args::parse();

    let resolution = parse_resolution(&args.resolution)?;

    tracing::info!("Starting hypr-rdp on {}", args.bind);

    // Spawn shutdown signal handler.
    // NOTE: Do NOT call cleanup_all_outputs() here — `hyprctl output remove`
    // sends Wayland events that crash Ghostty's GTK4 backend (SEGV in
    // wl_display_dispatch_queue_pending). The capture thread handles its own
    // output cleanup when the channel closes and the loop exits naturally.
    tokio::spawn(async {
        let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to register SIGTERM handler");
        let ctrl_c = tokio::signal::ctrl_c();
        tokio::select! {
            _ = sigterm.recv() => tracing::info!("Received SIGTERM"),
            _ = ctrl_c => tracing::info!("Received SIGINT"),
        }
        std::process::exit(0);
    });

    server::run(
        &args.bind,
        args.cert.as_deref(),
        args.key.as_deref(),
        &args.username,
        &args.password,
        resolution,
    )
    .await
}

fn parse_resolution(s: &str) -> anyhow::Result<(u32, u32)> {
    let parts: Vec<&str> = s.split('x').collect();
    if parts.len() != 2 {
        anyhow::bail!("invalid resolution format, expected WxH (e.g. 1920x1080)");
    }
    let w: u32 = parts[0].parse().map_err(|_| anyhow::anyhow!("invalid width"))?;
    let h: u32 = parts[1].parse().map_err(|_| anyhow::anyhow!("invalid height"))?;
    Ok((w, h))
}
