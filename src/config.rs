use std::path::PathBuf;

use clap::Parser;
use serde::Deserialize;

use crate::capture::CaptureMode;
use crate::egfx::{EgfxCodecPolicy, H264RateControl, DEFAULT_MAX_FRAMES_IN_FLIGHT};

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

    /// EGFX codec policy: "avc420" (default), "avc444" (experimental), or "auto"
    #[arg(long)]
    egfx_codec: Option<String>,

    /// Capture a specific output instead of creating a headless one
    #[arg(long)]
    output: Option<String>,

    /// Path to config file [default: ~/.config/hypr-rdp/config.toml]
    #[arg(long)]
    config: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
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
    egfx_codec: Option<String>,
    output: Option<String>,
}

impl ConfigFile {
    fn load(path: Option<&str>) -> anyhow::Result<Self> {
        let (config_path, explicit) = match path {
            Some(p) => (PathBuf::from(p), true),
            None => {
                let home = match std::env::var("HOME") {
                    Ok(home) => home,
                    Err(_) => return Ok(Self::default()),
                };
                (
                    PathBuf::from(home)
                        .join(".config")
                        .join("hypr-rdp")
                        .join("config.toml"),
                    false,
                )
            }
        };

        let content = match std::fs::read_to_string(&config_path) {
            Ok(c) => c,
            Err(error) if !explicit && error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(error) => {
                anyhow::bail!("failed to read config {}: {}", config_path.display(), error);
            }
        };

        match toml::from_str(&content) {
            Ok(config) => {
                tracing::info!("Loaded config from {}", config_path.display());
                Ok(config)
            }
            Err(error) => {
                anyhow::bail!(
                    "failed to parse config {}: {}",
                    config_path.display(),
                    error
                );
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
    pub egfx_codec: EgfxCodecPolicy,
    pub resolution_fixed: bool,
    pub output: Option<String>,
}

impl RuntimeConfig {
    pub fn load() -> anyhow::Result<Self> {
        let args = Args::parse();
        let config = ConfigFile::load(args.config.as_deref())?;

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
        let egfx_codec = resolve_egfx_codec_policy(args.egfx_codec, config.egfx_codec)?;
        let output = args.output.or(config.output);

        let resolution = parse_resolution(&resolution_str)?;
        let capture_mode = parse_capture_mode(&capture_mode_str)?;

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
            egfx_codec,
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

fn parse_capture_mode(s: &str) -> anyhow::Result<CaptureMode> {
    match s {
        "ext" => Ok(CaptureMode::Ext),
        "wlr" => Ok(CaptureMode::Wlr),
        other => anyhow::bail!("unknown capture mode '{}', expected 'ext' or 'wlr'", other),
    }
}

fn parse_egfx_codec_policy(s: &str) -> anyhow::Result<EgfxCodecPolicy> {
    match s {
        "auto" => Ok(EgfxCodecPolicy::Auto),
        "avc420" => Ok(EgfxCodecPolicy::Avc420),
        "avc444" => Ok(EgfxCodecPolicy::Avc444),
        other => anyhow::bail!(
            "unknown EGFX codec '{}', expected 'auto', 'avc420', or 'avc444'",
            other
        ),
    }
}

fn default_egfx_codec_policy_name() -> String {
    "avc420".into()
}

fn resolve_egfx_codec_policy(
    cli_value: Option<String>,
    config_value: Option<String>,
) -> anyhow::Result<EgfxCodecPolicy> {
    let value = cli_value
        .or(config_value)
        .unwrap_or_else(default_egfx_codec_policy_name);
    parse_egfx_codec_policy(&value)
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

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use std::fs;

    fn temp_config_path(name: &str) -> PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!("hypr-rdp-{name}-{}.toml", std::process::id()));
        path
    }

    #[test]
    fn explicit_missing_config_returns_error() {
        let path = temp_config_path("missing");
        let _ = fs::remove_file(&path);

        let error = ConfigFile::load(Some(path.to_str().unwrap()))
            .expect_err("explicit missing config must not fall back to defaults");

        assert!(format!("{error:#}").contains("failed to read config"));
    }

    #[test]
    fn explicit_invalid_config_returns_error() {
        let path = temp_config_path("invalid");
        fs::write(&path, "bind = [").expect("write invalid config");

        let error = ConfigFile::load(Some(path.to_str().unwrap()))
            .expect_err("explicit invalid config must not fall back to defaults");

        assert!(format!("{error:#}").contains("failed to parse config"));
        fs::remove_file(&path).expect("remove invalid config");
    }

    #[test]
    fn explicit_valid_config_loads_values() {
        let path = temp_config_path("valid");
        fs::write(&path, "bind = '127.0.0.1:3390'\nusername = 'alice'\n")
            .expect("write valid config");

        let config = ConfigFile::load(Some(path.to_str().unwrap())).expect("config loads");

        assert_eq!(config.bind.as_deref(), Some("127.0.0.1:3390"));
        assert_eq!(config.username.as_deref(), Some("alice"));
        fs::remove_file(&path).expect("remove valid config");
    }

    #[test]
    fn parses_egfx_codec_policy_values() {
        assert_eq!(
            parse_egfx_codec_policy("auto").unwrap(),
            EgfxCodecPolicy::Auto
        );
        assert_eq!(
            parse_egfx_codec_policy("avc420").unwrap(),
            EgfxCodecPolicy::Avc420
        );
        assert_eq!(
            parse_egfx_codec_policy("avc444").unwrap(),
            EgfxCodecPolicy::Avc444
        );
        assert!(parse_egfx_codec_policy("h264").is_err());
    }

    #[test]
    fn default_egfx_codec_policy_is_avc420() {
        let policy = resolve_egfx_codec_policy(None, None).unwrap();

        assert_eq!(policy, EgfxCodecPolicy::Avc420);
    }

    #[test]
    fn explicit_egfx_codec_policy_overrides_default_and_config() {
        assert_eq!(
            resolve_egfx_codec_policy(None, Some("avc444".into())).unwrap(),
            EgfxCodecPolicy::Avc444
        );
        assert_eq!(
            resolve_egfx_codec_policy(Some("avc420".into()), Some("avc444".into())).unwrap(),
            EgfxCodecPolicy::Avc420
        );
    }

    #[test]
    fn parses_capture_mode_values() {
        assert_eq!(parse_capture_mode("wlr").unwrap(), CaptureMode::Wlr);
        assert_eq!(parse_capture_mode("ext").unwrap(), CaptureMode::Ext);
        assert!(parse_capture_mode("invalid").is_err());
    }

    #[test]
    fn cli_accepts_capture_mode_values() {
        let wlr = Args::try_parse_from(["hypr-rdp", "--capture-mode", "wlr"]).unwrap();
        assert_eq!(wlr.capture_mode.as_deref(), Some("wlr"));

        let ext = Args::try_parse_from(["hypr-rdp", "--capture-mode", "ext"]).unwrap();
        assert_eq!(ext.capture_mode.as_deref(), Some("ext"));
    }

    proptest! {
        #[test]
        fn generated_resolution_parser_rounds_even_dimensions_or_rejects_too_small(
            width in 0u32..=u16::MAX as u32,
            height in 0u32..=u16::MAX as u32,
        ) {
            let parsed = parse_resolution(&format!("{width}x{height}"));
            let expected_width = width & !1;
            let expected_height = height & !1;

            if expected_width >= 2 && expected_height >= 2 {
                prop_assert_eq!(parsed.unwrap(), (expected_width, expected_height));
            } else {
                prop_assert!(parsed.is_err());
            }
        }

        #[test]
        fn generated_resolution_parser_rejects_dimensions_above_wire_limit(
            wide in (u16::MAX as u32 + 1)..=u32::MAX,
            high in (u16::MAX as u32 + 1)..=u32::MAX,
            valid in 2u32..=u16::MAX as u32,
        ) {
            let wide_resolution = format!("{wide}x{valid}");
            let high_resolution = format!("{valid}x{high}");

            prop_assert!(parse_resolution(&wide_resolution).is_err());
            prop_assert!(parse_resolution(&high_resolution).is_err());
        }

        #[test]
        fn generated_policy_parsers_accept_only_documented_exact_tokens(
            token in "[a-z0-9_-]{0,16}"
        ) {
            match token.as_str() {
                "auto" | "avc420" | "avc444" => {
                    prop_assert!(parse_egfx_codec_policy(&token).is_ok());
                }
                _ => {
                    prop_assert!(parse_egfx_codec_policy(&token).is_err());
                }
            }

            match token.as_str() {
                "wlr" | "ext" => {
                    prop_assert!(parse_capture_mode(&token).is_ok());
                }
                _ => {
                    prop_assert!(parse_capture_mode(&token).is_err());
                }
            }

            match token.as_str() {
                "vbr" | "cqp" => {
                    prop_assert!(parse_rate_control(&token).is_ok());
                }
                _ => {
                    prop_assert!(parse_rate_control(&token).is_err());
                }
            }
        }
    }
}
