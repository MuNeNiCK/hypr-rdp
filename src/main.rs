mod capture;
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
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive("hypr_rdp=info".parse()?))
        .init();

    let args = Args::parse();

    tracing::info!("Starting hypr-rdp on {}", args.bind);

    server::run(
        &args.bind,
        args.cert.as_deref(),
        args.key.as_deref(),
        &args.username,
        &args.password,
    )
    .await
}
