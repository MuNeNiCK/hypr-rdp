use std::path::PathBuf;

use clap::Parser;
use serde::Deserialize;

use crate::capture::CaptureMode;
use crate::egfx::{H264RateControl, DEFAULT_MAX_FRAMES_IN_FLIGHT};

#[derive(Parser, Debug)]
#[command(name = "hypr-rdp", version, about = "Native RDP server for Hyprland")]
struct Args {
    /// Address to bind the RDP server
    #[arg(short, long)]
    bind: Option<String>,

    /// TLS certificate file (PEM)
    #[arg(long)]
    cert: Option<String>,

    /// TLS private key file (PEM)
    #[arg(long)]
    key: Option<String>,

    /// Username for RDP authentication
    #[arg(short, long)]
    username: Option<String>,

    /// Password for RDP authentication
    #[arg(short, long)]
    password: Option<String>,

    /// RDP session resolution (WxH), e.g. 1920x1080
    #[arg(short, long)]
    resolution: Option<String>,

    /// Screen capture protocol: "wlr" (wlr-screencopy-v1) or "ext" (ext-image-copy-capture-v1)
    #[arg(long)]
    capture_mode: Option<String>,

    /// H.264 encoder bitrate in bps
    #[arg(long)]
    bitrate: Option<u32>,

    /// H.264 quality level (0-51, lower = better)
    #[arg(long)]
    quality: Option<u8>,

    /// H.264 rate control mode: "vbr" (default) or "cqp"
    #[arg(long)]
    rate_control: Option<String>,

    /// Maximum capture frame rate
    #[arg(long)]
    fps: Option<u32>,

    /// Maximum unacknowledged EGFX frames in flight
    #[arg(long)]
    max_frames_in_flight: Option<u32>,

    /// Capture a specific output instead of creating a headless one
    #[arg(long)]
    output: Option<String>,

    /// Path to config file [default: ~/.config/hypr-rdp/config.toml]
    #[arg(long)]
    config: Option<String>,
}

#[derive(Deserialize, Default)]
struct ConfigFile {
    bind: Option<String>,
    cert: Option<String>,
    key: Option<String>,
    username: Option<String>,
    password: Option<String>,
    resolution: Option<String>,
    capture_mode: Option<String>,
    bitrate: Option<u32>,
    quality: Option<u8>,
    rate_control: Option<String>,
    fps: Option<u32>,
    max_frames_in_flight: Option<u32>,
    output: Option<String>,
}

impl ConfigFile {
    fn load(path: Option<&str>) -> Self {
        let config_path = match path {
            Some(p) => PathBuf::from(p),
            None => {
                let home = match std::env::var("HOME") {
                    Ok(h) => h,
                    Err(_) => return Self::default(),
                };
                PathBuf::from(home)
                    .join(".config")
                    .join("hypr-rdp")
                    .join("config.toml")
            }
        };

        let content = match std::fs::read_to_string(&config_path) {
            Ok(c) => c,
            Err(_) => return Self::default(),
        };

        match toml::from_str(&content) {
            Ok(config) => {
                tracing::info!("Loaded config from {}", config_path.display());
                config
            }
            Err(e) => {
                tracing::warn!("Failed to parse {}: {}", config_path.display(), e);
                Self::default()
            }
        }
    }
}

pub struct RuntimeConfig {
    pub bind: String,
    pub cert: Option<String>,
    pub key: Option<String>,
    pub username: String,
    pub password: String,
    pub resolution: (u32, u32),
    pub capture_mode: CaptureMode,
    pub bitrate: u32,
    pub quality: u8,
    pub rate_control: H264RateControl,
    pub fps: u32,
    pub max_frames_in_flight: u32,
    pub resolution_fixed: bool,
    pub output: Option<String>,
}

impl RuntimeConfig {
    pub fn load() -> anyhow::Result<Self> {
        let args = Args::parse();
        let config = ConfigFile::load(args.config.as_deref());

        let bind = args
            .bind
            .or(config.bind)
            .unwrap_or_else(|| "127.0.0.1:3389".into());
        let cert = args.cert.or(config.cert);
        let key = args.key.or(config.key);
        let username = args.username.or(config.username).unwrap_or_default();
        let password = args.password.or(config.password).unwrap_or_default();

        if username.is_empty() || password.is_empty() {
            tracing::warn!(
                "No credentials set (-u/-p). Use -u <user> -p <pass> to require authentication."
            );
            if bind.starts_with("0.0.0.0") {
                tracing::warn!("Binding to all interfaces without credentials is a security risk.");
            }
        }

        let resolution_fixed = args.resolution.is_some() || config.resolution.is_some();
        let resolution_str = args
            .resolution
            .or(config.resolution)
            .unwrap_or_else(|| "1920x1080".into());
        let capture_mode_str = args
            .capture_mode
            .or(config.capture_mode)
            .unwrap_or_else(|| "wlr".into());
        let bitrate = args.bitrate.or(config.bitrate).unwrap_or(10_000_000);
        let quality = args.quality.or(config.quality).unwrap_or(23);
        let rate_control_str = args
            .rate_control
            .or(config.rate_control)
            .unwrap_or_else(|| "vbr".into());
        let rate_control = parse_rate_control(&rate_control_str)?;
        let fps = args.fps.or(config.fps).unwrap_or(30);
        let max_frames_in_flight = args
            .max_frames_in_flight
            .or(config.max_frames_in_flight)
            .unwrap_or(DEFAULT_MAX_FRAMES_IN_FLIGHT);
        let output = args.output.or(config.output);

        let resolution = parse_resolution(&resolution_str)?;
        let capture_mode = match capture_mode_str.as_str() {
            "ext" => CaptureMode::Ext,
            "wlr" => CaptureMode::Wlr,
            other => anyhow::bail!("unknown capture mode '{}', expected 'ext' or 'wlr'", other),
        };

        if quality > 51 {
            anyhow::bail!("quality must be 0-51");
        }
        if fps == 0 {
            anyhow::bail!("fps must be > 0");
        }
        if max_frames_in_flight == 0 {
            anyhow::bail!("max-frames-in-flight must be > 0");
        }

        Ok(Self {
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
        })
    }
}

fn parse_rate_control(s: &str) -> anyhow::Result<H264RateControl> {
    match s {
        "vbr" => Ok(H264RateControl::Vbr),
        "cqp" => Ok(H264RateControl::Cqp),
        other => anyhow::bail!("unknown rate control '{}', expected 'vbr' or 'cqp'", other),
    }
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
    if w == 0 || h == 0 {
        anyhow::bail!("resolution dimensions must be non-zero");
    }
    if w > u16::MAX as u32 || h > u16::MAX as u32 {
        anyhow::bail!("resolution dimensions must be <= {}", u16::MAX);
    }
    // H.264 requires even dimensions (4:2:0 chroma subsampling)
    let w = w & !1;
    let h = h & !1;
    if w == 0 || h == 0 {
        anyhow::bail!("resolution too small (minimum 2x2 for H.264)");
    }
    Ok((w, h))
}
