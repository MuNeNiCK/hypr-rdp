use std::net::SocketAddr;
use std::path::PathBuf;

use clap::Parser;
use serde::Deserialize;

use crate::audio::AudioMode;
use crate::capture::CaptureMode;
use crate::egfx::{
    EgfxCodecPolicy, H264BackendPolicy, H264RateControl, DEFAULT_MAX_FRAMES_IN_FLIGHT,
};
use crate::input::KeyboardLayoutPolicy;
use ironrdp_cliprdr::pdu::MAX_FILE_COUNT;

pub(crate) const DEFAULT_FILE_TRANSFER_MAX_ENTRIES: usize = 10_000;

/// Which directions of clipboard file transfer a session may serve.
///
/// Disabling a build feature must never enable a direction the user forbade.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FileTransferMode {
    Off,
    ToClient,
    ToServer,
    Both,
}

impl FileTransferMode {
    pub(crate) fn permits_to_client(self) -> bool {
        matches!(self, Self::ToClient | Self::Both)
    }

    pub(crate) fn permits_to_server(self) -> bool {
        cfg!(feature = "client-to-server") && matches!(self, Self::ToServer | Self::Both)
    }

    fn for_build(self) -> Self {
        if cfg!(feature = "client-to-server") {
            self
        } else {
            match self {
                Self::ToServer => Self::Off,
                Self::Both => Self::ToClient,
                mode => mode,
            }
        }
    }
}

fn default_file_transfer_mode_name() -> String {
    if cfg!(feature = "client-to-server") {
        "both"
    } else {
        "to-client"
    }
    .into()
}

/// The command line beats the config file, as it does for every other option.
fn resolve_file_transfer_mode(
    cli_value: Option<String>,
    config_value: Option<String>,
) -> anyhow::Result<FileTransferMode> {
    parse_file_transfer_mode(
        &cli_value
            .or(config_value)
            .unwrap_or_else(default_file_transfer_mode_name),
    )
}

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

    /// Authentication mode: configured credentials (default), Linux PAM, or
    /// none (explicitly allow unauthenticated connections)
    #[arg(long)]
    auth_mode: Option<String>,

    /// PAM service name (only with --auth-mode pam)
    #[arg(long)]
    pam_service: Option<String>,

    /// Username for RDP authentication
    #[arg(short, long)]
    username: Option<String>,

    /// Optional NTLM domain for configured RDP authentication
    #[arg(short, long)]
    domain: Option<String>,

    /// Password for RDP authentication
    #[arg(short, long, conflicts_with = "password_file")]
    password: Option<String>,

    /// Read the RDP password from a file (one trailing line ending is
    /// stripped; the rest of the content is used as-is)
    #[arg(long, conflicts_with = "password")]
    password_file: Option<String>,

    /// RDP session resolution (WxH), e.g. 1920x1080
    #[arg(short, long)]
    resolution: Option<String>,

    /// Scale of the managed headless output, e.g. 2 for HiDPI clients
    #[arg(long)]
    scale: Option<f64>,

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

    /// EGFX codec policy: "avc420" (default), "avc444" (experimental), "auto", or
    /// "clearcodec" (send lossless, damage-only ClearCodec frames instead of H.264)
    #[arg(long)]
    egfx_codec: Option<String>,

    /// Keyboard layout policy: "client" (default) or "compositor"
    #[arg(long)]
    keyboard_layout_policy: Option<String>,

    /// Audio output policy: "redirect" (default), "mirror", or "off"
    #[arg(long)]
    audio_mode: Option<String>,

    /// H.264 encoder backend: "auto" (default), "software", or "vaapi"
    #[arg(long)]
    h264_backend: Option<String>,

    /// Capture a specific output instead of creating a headless one
    #[arg(long)]
    output: Option<String>,

    /// Shell command to run when an authenticated session starts
    /// (without configured credentials: any established session)
    #[arg(long)]
    on_session_start: Option<String>,

    /// Shell command to run when an authenticated session ends
    /// (also on service stop, and without configured credentials: any
    /// established session)
    #[arg(long)]
    on_session_end: Option<String>,

    /// File transfer policy: "off", "to-client", "to-server", or "both"
    #[arg(long)]
    file_transfer_mode: Option<String>,

    /// Maximum bytes accepted in one clipboard file-content range request
    #[arg(long)]
    file_transfer_max_chunk_bytes: Option<u32>,

    /// Maximum files and directories enumerated from one clipboard selection
    #[arg(long)]
    file_transfer_max_entries: Option<usize>,

    /// Path to config file [default: ~/.config/hypr-rdp/config.toml]
    #[arg(long)]
    config: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
struct ConfigFile {
    auth_mode: Option<String>,
    pam_service: Option<String>,
    bind: Option<String>,
    cert: Option<String>,
    key: Option<String>,
    username: Option<String>,
    domain: Option<String>,
    password: Option<String>,
    password_file: Option<String>,
    resolution: Option<String>,
    scale: Option<f64>,
    capture_mode: Option<String>,
    bitrate: Option<u32>,
    quality: Option<u8>,
    rate_control: Option<String>,
    fps: Option<u32>,
    max_frames_in_flight: Option<u32>,
    egfx_codec: Option<String>,
    keyboard_layout_policy: Option<String>,
    audio_mode: Option<String>,
    h264_backend: Option<String>,
    output: Option<String>,
    on_session_start: Option<String>,
    on_session_end: Option<String>,
    file_transfer_mode: Option<String>,
    file_transfer_max_chunk_bytes: Option<u32>,
    file_transfer_max_entries: Option<usize>,
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
    pub bind: SocketAddr,
    pub cert: Option<String>,
    pub key: Option<String>,
    pub authentication: AuthConfig,
    pub resolution: (u32, u32),
    pub headless_scale: f64,
    pub capture_mode: CaptureMode,
    pub bitrate: u32,
    pub quality: u8,
    pub rate_control: H264RateControl,
    pub fps: u32,
    pub max_frames_in_flight: u32,
    pub egfx_codec: EgfxCodecPolicy,
    pub keyboard_layout_policy: KeyboardLayoutPolicy,
    pub audio_mode: AudioMode,
    pub h264_backend: H264BackendPolicy,
    pub resolution_fixed: bool,
    pub output: Option<String>,
    pub on_session_start: Option<String>,
    pub on_session_end: Option<String>,
    pub file_transfer_mode: FileTransferMode,
    pub file_transfer_max_chunk_bytes: u32,
    pub file_transfer_max_entries: usize,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum AuthConfig {
    /// Fixed username/password checked through NLA; both halves are nonempty.
    Configured(ConfigCredentials),
    /// Explicitly requested unauthenticated access (`auth_mode = "none"`).
    None,
    Pam {
        service: String,
    },
}

fn resolve_authentication(args: &mut Args, config: &mut ConfigFile) -> anyhow::Result<AuthConfig> {
    let mode = args
        .auth_mode
        .take()
        .or(config.auth_mode.take())
        .unwrap_or_else(|| "configured".into());
    let service = args.pam_service.take().or(config.pam_service.take());
    let configured_inputs_absent = || {
        args.username.is_none()
            && config.username.is_none()
            && args.domain.is_none()
            && config.domain.is_none()
            && args.password.is_none()
            && config.password.is_none()
            && args.password_file.is_none()
            && config.password_file.is_none()
    };
    match mode.as_str() {
        "configured" => {
            anyhow::ensure!(service.is_none(), "pam-service requires auth-mode pam");
            let username = args
                .username
                .take()
                .or(config.username.take())
                .unwrap_or_default();
            let domain = args.domain.take().or(config.domain.take());
            let password = resolve_password(
                args.password.take(),
                args.password_file.take(),
                config.password.take(),
                config.password_file.take(),
            )?;
            Ok(AuthConfig::Configured(ConfigCredentials::from_parts(
                username, domain, password,
            )?))
        }
        "none" => {
            anyhow::ensure!(
                configured_inputs_absent() && service.is_none(),
                "auth-mode none cannot be combined with username, domain, password, password-file or pam-service"
            );
            Ok(AuthConfig::None)
        }
        "pam" => {
            anyhow::ensure!(
                configured_inputs_absent(),
                "auth-mode pam cannot be combined with username, domain, password or password-file"
            );
            let service = service.unwrap_or_else(|| "hypr-rdp".into());
            anyhow::ensure!(
                crate::server::auth::valid_pam_service(&service),
                "invalid PAM service name"
            );
            Ok(AuthConfig::Pam { service })
        }
        _ => anyhow::bail!("unknown auth-mode; expected configured, pam or none"),
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ConfigCredentials {
    pub username: String,
    pub domain: Option<String>,
    pub password: String,
}

impl ConfigCredentials {
    /// Requires both halves; missing credentials never fall back to no-auth.
    fn from_parts(
        username: String,
        domain: Option<String>,
        password: String,
    ) -> anyhow::Result<Self> {
        match (username.is_empty(), password.is_empty()) {
            (false, false) => Ok(Self {
                username,
                domain: domain.filter(|domain| !domain.is_empty()),
                password,
            }),
            (true, true) => anyhow::bail!(
                "no credentials configured; set username and password (or password_file), \
                 use auth_mode = \"pam\", or set auth_mode = \"none\" to explicitly allow \
                 unauthenticated connections"
            ),
            (true, false) => anyhow::bail!("a password is configured but the username is empty"),
            (false, true) => anyhow::bail!("a username is configured but the password is empty"),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum StartupWarning {
    AuthenticationOff,
    ReachableBeyondLoopback,
}

fn startup_warnings(authentication: &AuthConfig, bind: SocketAddr) -> Vec<StartupWarning> {
    let mut warnings = Vec::new();
    if *authentication == AuthConfig::None {
        warnings.push(StartupWarning::AuthenticationOff);
        if !bind.ip().to_canonical().is_loopback() {
            warnings.push(StartupWarning::ReachableBeyondLoopback);
        }
    }
    warnings
}

impl RuntimeConfig {
    pub fn load() -> anyhow::Result<Self> {
        let mut args = Args::parse();
        let mut config = ConfigFile::load(args.config.as_deref())?;
        let authentication = resolve_authentication(&mut args, &mut config)?;

        let bind = args
            .bind
            .or(config.bind)
            .unwrap_or_else(|| "127.0.0.1:3389".into());
        let bind = parse_bind_addr(&bind)?;
        let cert = args.cert.or(config.cert);
        let key = args.key.or(config.key);
        let requested_file_transfer_mode =
            resolve_file_transfer_mode(args.file_transfer_mode, config.file_transfer_mode)?;
        let file_transfer_mode = requested_file_transfer_mode.for_build();
        if file_transfer_mode != requested_file_transfer_mode {
            tracing::warn!(?requested_file_transfer_mode, ?file_transfer_mode,
                "Client-to-server file transfer is not compiled in; disabling the unavailable direction");
        }

        for warning in startup_warnings(&authentication, bind) {
            match warning {
                StartupWarning::AuthenticationOff => tracing::warn!(
                    "auth_mode = \"none\": connections are accepted without authentication."
                ),
                StartupWarning::ReachableBeyondLoopback => tracing::warn!(
                    bind = %bind,
                    "RDP is reachable beyond loopback without authentication."
                ),
            }
        }

        let resolution_fixed = args.resolution.is_some() || config.resolution.is_some();
        let resolution_str = args
            .resolution
            .or(config.resolution)
            .unwrap_or_else(|| "1920x1080".into());
        let headless_scale = resolve_headless_scale(args.scale, config.scale)?;
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
        let keyboard_layout_policy = resolve_keyboard_layout_policy(
            args.keyboard_layout_policy,
            config.keyboard_layout_policy,
        )?;
        let audio_mode = resolve_audio_mode(args.audio_mode, config.audio_mode)?;
        let h264_backend = resolve_h264_backend_policy(args.h264_backend, config.h264_backend)?;
        let output = args.output.or(config.output);
        let on_session_start = args.on_session_start.or(config.on_session_start);
        let on_session_end = args.on_session_end.or(config.on_session_end);
        let file_transfer_max_chunk_bytes = args
            .file_transfer_max_chunk_bytes
            .or(config.file_transfer_max_chunk_bytes)
            .unwrap_or(8 * 1024 * 1024);
        let file_transfer_max_entries = args
            .file_transfer_max_entries
            .or(config.file_transfer_max_entries)
            .unwrap_or(DEFAULT_FILE_TRANSFER_MAX_ENTRIES);

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
        if file_transfer_max_chunk_bytes == 0 {
            anyhow::bail!("file-transfer-max-chunk-bytes must be > 0");
        }
        validate_file_transfer_max_entries(file_transfer_max_entries)?;

        Ok(Self {
            bind,
            cert,
            key,
            authentication,
            resolution,
            headless_scale,
            capture_mode,
            bitrate,
            quality,
            rate_control,
            fps,
            max_frames_in_flight,
            egfx_codec,
            keyboard_layout_policy,
            audio_mode,
            h264_backend,
            resolution_fixed,
            output,
            on_session_start,
            on_session_end,
            file_transfer_mode,
            file_transfer_max_chunk_bytes,
            file_transfer_max_entries,
        })
    }
}

fn validate_file_transfer_max_entries(value: usize) -> anyhow::Result<()> {
    if value == 0 {
        anyhow::bail!("file-transfer-max-entries must be > 0");
    }
    if value > MAX_FILE_COUNT {
        anyhow::bail!(
            "file-transfer-max-entries must be at most {MAX_FILE_COUNT}, the clipboard protocol limit"
        );
    }
    Ok(())
}

fn parse_file_transfer_mode(value: &str) -> anyhow::Result<FileTransferMode> {
    match value {
        "off" => Ok(FileTransferMode::Off),
        "to-client" => Ok(FileTransferMode::ToClient),
        "to-server" => Ok(FileTransferMode::ToServer),
        "both" => Ok(FileTransferMode::Both),
        other => {
            anyhow::bail!("unknown file transfer mode '{other}', expected 'off', 'to-client', 'to-server', or 'both'")
        }
    }
}

fn resolve_password(
    args_password: Option<String>,
    args_password_file: Option<String>,
    config_password: Option<String>,
    config_password_file: Option<String>,
) -> anyhow::Result<String> {
    if config_password.is_some() && config_password_file.is_some() {
        anyhow::bail!("`password` and `password_file` cannot both be set in the config file");
    }

    if let Some(path) = args_password_file {
        return read_password_file(&path);
    }
    if let Some(password) = args_password {
        return Ok(password);
    }

    match (config_password, config_password_file) {
        (Some(password), None) => Ok(password),
        (None, Some(path)) => read_password_file(&path),
        (None, None) => Ok(String::new()),
        (Some(_), Some(_)) => unreachable!("checked above"),
    }
}

/// Reads a password from `path`, stripping exactly one trailing line ending
/// (`\n` or `\r\n`). Fails if the file cannot be read, or is empty once that
/// trailing line ending is removed.
fn read_password_file(path: &str) -> anyhow::Result<String> {
    let mut content = std::fs::read(path)
        .map_err(|error| anyhow::anyhow!("failed to read password file {path:?}: {error}"))?;

    if content.last() == Some(&b'\n') {
        content.pop();
        if content.last() == Some(&b'\r') {
            content.pop();
        }
    }

    if content.is_empty() {
        anyhow::bail!("password file {path:?} is empty");
    }

    String::from_utf8(content)
        .map_err(|_| anyhow::anyhow!("password file {path:?} is not valid UTF-8"))
}

fn parse_bind_addr(bind: &str) -> anyhow::Result<SocketAddr> {
    bind.parse()
        .map_err(|error| anyhow::anyhow!("invalid bind address: {error}"))
}

fn parse_h264_backend_policy(s: &str) -> anyhow::Result<H264BackendPolicy> {
    match s {
        "auto" => Ok(H264BackendPolicy::Auto),
        "software" => Ok(H264BackendPolicy::Software),
        "vaapi" => Ok(H264BackendPolicy::Vaapi),
        other => anyhow::bail!(
            "unknown H.264 backend '{}', expected 'auto', 'software', or 'vaapi'",
            other
        ),
    }
}

fn resolve_h264_backend_policy(
    cli_value: Option<String>,
    config_value: Option<String>,
) -> anyhow::Result<H264BackendPolicy> {
    match cli_value.or(config_value) {
        Some(value) => parse_h264_backend_policy(&value),
        None => Ok(H264BackendPolicy::Auto),
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
        "clearcodec" => Ok(EgfxCodecPolicy::ClearCodec),
        other => anyhow::bail!(
            "unknown EGFX codec '{}', expected 'auto', 'avc420', 'avc444', or 'clearcodec'",
            other
        ),
    }
}

fn parse_keyboard_layout_policy(s: &str) -> anyhow::Result<KeyboardLayoutPolicy> {
    match s {
        "client" => Ok(KeyboardLayoutPolicy::Client),
        "compositor" => Ok(KeyboardLayoutPolicy::Compositor),
        other => anyhow::bail!(
            "unknown keyboard layout policy '{}', expected 'client' or 'compositor'",
            other
        ),
    }
}

fn parse_audio_mode(s: &str) -> anyhow::Result<AudioMode> {
    match s {
        "mirror" => Ok(AudioMode::Mirror),
        "redirect" => Ok(AudioMode::Redirect),
        "off" => Ok(AudioMode::Off),
        other => anyhow::bail!(
            "unknown audio mode '{}', expected 'mirror', 'redirect', or 'off'",
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

fn default_keyboard_layout_policy_name() -> String {
    "client".into()
}

fn default_audio_mode_name() -> String {
    "redirect".into()
}

fn resolve_keyboard_layout_policy(
    cli_value: Option<String>,
    config_value: Option<String>,
) -> anyhow::Result<KeyboardLayoutPolicy> {
    let value = cli_value
        .or(config_value)
        .unwrap_or_else(default_keyboard_layout_policy_name);
    parse_keyboard_layout_policy(&value)
}

fn resolve_audio_mode(
    cli_value: Option<String>,
    config_value: Option<String>,
) -> anyhow::Result<AudioMode> {
    let value = cli_value
        .or(config_value)
        .unwrap_or_else(default_audio_mode_name);
    parse_audio_mode(&value)
}

fn resolve_headless_scale(cli: Option<f64>, config: Option<f64>) -> anyhow::Result<f64> {
    let scale = cli.or(config).unwrap_or(1.0);

    if !scale.is_finite() || scale <= 0.0 {
        anyhow::bail!("invalid scale {scale}, expected a positive number (e.g. 1, 1.5, 2)");
    }

    Ok(scale)
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
    use StartupWarning::*;

    #[test]
    fn headless_scale_defaults_and_cli_overrides_config() {
        assert_eq!(resolve_headless_scale(None, None).unwrap(), 1.0);
        assert_eq!(resolve_headless_scale(None, Some(1.5)).unwrap(), 1.5);
        assert_eq!(resolve_headless_scale(Some(2.0), Some(1.5)).unwrap(), 2.0);
    }

    #[test]
    fn headless_scale_rejects_non_positive_and_non_finite_values() {
        for value in [0.0, -2.0, f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert!(resolve_headless_scale(Some(value), None).is_err());
        }
    }

    #[test]
    fn cli_accepts_headless_scale() {
        let args = Args::try_parse_from(["hypr-rdp", "--scale", "1.5"]).unwrap();

        assert_eq!(args.scale, Some(1.5));
    }

    fn warnings(authentication: &AuthConfig, bind: &str) -> Vec<StartupWarning> {
        startup_warnings(authentication, parse_bind_addr(bind).unwrap())
    }

    #[test]
    fn credential_classification_requires_both_halves() {
        for (username, password) in [("user", "secret"), (" ", " "), ("DOMAIN\\User", "secret")] {
            let credentials =
                ConfigCredentials::from_parts(username.to_owned(), None, password.to_owned())
                    .unwrap();
            assert_eq!(credentials.username, username);
            assert_eq!(credentials.password, password);
            assert_eq!(credentials.domain, None);
        }

        for (username, password, reason) in [
            ("", "", "auth_mode = \"none\""),
            ("user", "", "password is empty"),
            ("", "secret", "username is empty"),
        ] {
            let error = ConfigCredentials::from_parts(
                username.to_owned(),
                Some("D".into()),
                password.to_owned(),
            )
            .expect_err("incomplete credentials must not start");
            assert!(
                format!("{error:#}").contains(reason),
                "{username:?}/{password:?}: {error:#}"
            );
        }
    }

    #[test]
    fn configured_domain_round_trips_through_from_parts() {
        for (domain, expected) in [
            (Some("WORKGROUP"), Some("WORKGROUP")),
            (Some(" MiXeD \t "), Some(" MiXeD \t ")),
            (Some(""), None),
            (None, None),
        ] {
            let credentials = ConfigCredentials::from_parts(
                "user".into(),
                domain.map(str::to_owned),
                "pass".into(),
            )
            .unwrap();
            assert_eq!(credentials.username, "user");
            assert_eq!(credentials.password, "pass");
            assert_eq!(credentials.domain.as_deref(), expected);
        }
    }

    #[test]
    fn explicit_no_auth_warns_and_flags_reach_beyond_canonical_loopback() {
        for bind in [
            "0.0.0.0:3389",
            "[::]:3389",
            "[::ffff:0.0.0.0]:3389",
            "192.168.1.5:3389",
        ] {
            assert_eq!(
                warnings(&AuthConfig::None, bind),
                vec![AuthenticationOff, ReachableBeyondLoopback],
                "{bind}"
            );
        }

        for bind in ["127.0.0.1:3389", "[::1]:3389", "[::ffff:127.0.0.1]:3389"] {
            assert_eq!(
                warnings(&AuthConfig::None, bind),
                vec![AuthenticationOff],
                "{bind}"
            );
        }
    }

    #[test]
    fn authenticated_modes_have_no_startup_warnings() {
        let configured = AuthConfig::Configured(ConfigCredentials {
            username: "user".into(),
            domain: None,
            password: "secret".into(),
        });
        let pam = AuthConfig::Pam {
            service: "hypr-rdp".into(),
        };
        for bind in ["0.0.0.0:3389", "127.0.0.1:3389"] {
            assert!(warnings(&configured, bind).is_empty());
            assert!(warnings(&pam, bind).is_empty());
        }
    }

    #[test]
    fn invalid_bind_address_is_rejected_by_config() {
        let error = parse_bind_addr("not an address").expect_err("invalid bind must fail");
        assert!(format!("{error:#}").contains("invalid bind address"));
    }

    #[test]
    fn file_transfer_mode_accepts_all_documented_values() {
        assert_eq!(
            parse_file_transfer_mode("off").unwrap(),
            FileTransferMode::Off
        );
        assert_eq!(
            parse_file_transfer_mode("to-client").unwrap(),
            FileTransferMode::ToClient
        );
        assert!(FileTransferMode::ToClient.permits_to_client());
        assert!(!FileTransferMode::Off.permits_to_client());
        assert!(parse_file_transfer_mode("invalid").is_err());
    }

    #[test]
    fn file_transfer_is_on_for_the_client_by_default() {
        assert_eq!(
            resolve_file_transfer_mode(None, None).unwrap(),
            if cfg!(feature = "client-to-server") {
                FileTransferMode::Both
            } else {
                FileTransferMode::ToClient
            }
        );
    }

    #[test]
    fn unavailable_inbound_never_enables_forbidden_outbound_transfer() {
        for (name, mode, outbound) in [
            ("off", FileTransferMode::Off, false),
            ("to-client", FileTransferMode::ToClient, true),
            ("to-server", FileTransferMode::ToServer, false),
            ("both", FileTransferMode::Both, true),
        ] {
            assert_eq!(parse_file_transfer_mode(name).unwrap(), mode);
            let effective = mode.for_build();
            assert_eq!(effective.permits_to_client(), outbound);
            assert_eq!(
                effective.permits_to_server(),
                cfg!(feature = "client-to-server")
                    && matches!(mode, FileTransferMode::ToServer | FileTransferMode::Both)
            );
        }
    }

    #[test]
    fn a_mode_on_the_command_line_overrides_the_config_file() {
        assert_eq!(
            resolve_file_transfer_mode(Some("off".into()), Some("to-client".into())).unwrap(),
            FileTransferMode::Off
        );
    }

    #[test]
    fn file_transfer_entry_limit_stays_within_the_protocol_limit() {
        assert!(validate_file_transfer_max_entries(1).is_ok());
        assert!(validate_file_transfer_max_entries(MAX_FILE_COUNT).is_ok());
        assert!(validate_file_transfer_max_entries(0).is_err());
        assert!(validate_file_transfer_max_entries(MAX_FILE_COUNT + 1).is_err());
    }

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
        fs::write(
            &path,
            "bind = '127.0.0.1:3390'\nusername = 'alice'\nscale = 2\nh264_backend = 'software'\n",
        )
        .expect("write valid config");

        let config = ConfigFile::load(Some(path.to_str().unwrap())).expect("config loads");

        assert_eq!(config.bind.as_deref(), Some("127.0.0.1:3390"));
        assert_eq!(config.username.as_deref(), Some("alice"));
        assert_eq!(config.scale, Some(2.0));
        assert_eq!(config.h264_backend.as_deref(), Some("software"));
        fs::remove_file(&path).expect("remove valid config");
    }

    #[test]
    fn clearcodec_policy_resolves_from_toml_and_cli() {
        let config: ConfigFile = toml::from_str("egfx_codec = 'clearcodec'").unwrap();
        assert_eq!(
            resolve_egfx_codec_policy(None, config.egfx_codec).unwrap(),
            EgfxCodecPolicy::ClearCodec
        );
        let args = Args::try_parse_from(["hypr-rdp", "--egfx-codec", "clearcodec"]).unwrap();
        assert_eq!(
            resolve_egfx_codec_policy(args.egfx_codec, Some("avc444".into())).unwrap(),
            EgfxCodecPolicy::ClearCodec
        );
        let args = Args::try_parse_from(["hypr-rdp", "--egfx-codec", "avc420"]).unwrap();
        assert_eq!(
            resolve_egfx_codec_policy(args.egfx_codec, Some("clearcodec".into())).unwrap(),
            EgfxCodecPolicy::Avc420
        );
        let args = Args::try_parse_from(["hypr-rdp", "--egfx-codec", "clearcodec-typo"]).unwrap();
        assert!(resolve_egfx_codec_policy(args.egfx_codec, Some("clearcodec".into())).is_err());
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
        assert_eq!(
            parse_egfx_codec_policy("clearcodec").unwrap(),
            EgfxCodecPolicy::ClearCodec
        );
        assert!(parse_egfx_codec_policy("h264").is_err());
    }

    #[test]
    fn parses_keyboard_layout_policy_values() {
        assert_eq!(
            parse_keyboard_layout_policy("client").unwrap(),
            KeyboardLayoutPolicy::Client
        );
        assert_eq!(
            parse_keyboard_layout_policy("compositor").unwrap(),
            KeyboardLayoutPolicy::Compositor
        );
        assert!(parse_keyboard_layout_policy("hyprland").is_err());
    }

    #[test]
    fn parses_audio_mode_values() {
        assert_eq!(parse_audio_mode("mirror").unwrap(), AudioMode::Mirror);
        assert_eq!(parse_audio_mode("redirect").unwrap(), AudioMode::Redirect);
        assert_eq!(parse_audio_mode("off").unwrap(), AudioMode::Off);
        assert!(parse_audio_mode("local").is_err());
    }

    #[test]
    fn parses_h264_backend_policy_values() {
        assert_eq!(
            parse_h264_backend_policy("auto").unwrap(),
            H264BackendPolicy::Auto
        );
        assert_eq!(
            parse_h264_backend_policy("software").unwrap(),
            H264BackendPolicy::Software
        );
        assert_eq!(
            parse_h264_backend_policy("vaapi").unwrap(),
            H264BackendPolicy::Vaapi
        );
        assert!(parse_h264_backend_policy("hardware").is_err());
    }

    #[test]
    fn default_egfx_codec_policy_is_avc420() {
        let policy = resolve_egfx_codec_policy(None, None).unwrap();

        assert_eq!(policy, EgfxCodecPolicy::Avc420);
    }

    #[test]
    fn default_keyboard_layout_policy_uses_client_layout() {
        let policy = resolve_keyboard_layout_policy(None, None).unwrap();

        assert_eq!(policy, KeyboardLayoutPolicy::Client);
    }

    #[test]
    fn default_audio_mode_is_redirect() {
        let mode = resolve_audio_mode(None, None).unwrap();

        assert_eq!(mode, AudioMode::Redirect);
    }

    #[test]
    fn default_h264_backend_policy_is_auto() {
        assert_eq!(
            resolve_h264_backend_policy(None, None).unwrap(),
            H264BackendPolicy::Auto
        );
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
    fn explicit_keyboard_layout_policy_overrides_default_and_config() {
        assert_eq!(
            resolve_keyboard_layout_policy(None, Some("compositor".into())).unwrap(),
            KeyboardLayoutPolicy::Compositor
        );
        assert_eq!(
            resolve_keyboard_layout_policy(Some("client".into()), Some("compositor".into()))
                .unwrap(),
            KeyboardLayoutPolicy::Client
        );
    }

    #[test]
    fn explicit_audio_mode_overrides_default_and_config() {
        assert_eq!(
            resolve_audio_mode(None, Some("redirect".into())).unwrap(),
            AudioMode::Redirect
        );
        assert_eq!(
            resolve_audio_mode(Some("off".into()), Some("redirect".into())).unwrap(),
            AudioMode::Off
        );
    }

    #[test]
    fn explicit_h264_backend_policy_overrides_config() {
        assert_eq!(
            resolve_h264_backend_policy(None, Some("software".into())).unwrap(),
            H264BackendPolicy::Software
        );
        assert_eq!(
            resolve_h264_backend_policy(Some("vaapi".into()), Some("software".into())).unwrap(),
            H264BackendPolicy::Vaapi
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

    #[test]
    fn cli_accepts_keyboard_layout_policy_values() {
        let compositor =
            Args::try_parse_from(["hypr-rdp", "--keyboard-layout-policy", "compositor"]).unwrap();

        assert_eq!(
            compositor.keyboard_layout_policy.as_deref(),
            Some("compositor")
        );
    }

    #[test]
    fn cli_accepts_audio_mode_values() {
        let redirect = Args::try_parse_from(["hypr-rdp", "--audio-mode", "redirect"]).unwrap();

        assert_eq!(redirect.audio_mode.as_deref(), Some("redirect"));
    }

    #[test]
    fn cli_accepts_h264_backend_option() {
        let args = Args::try_parse_from(["hypr-rdp", "--h264-backend", "software"]).unwrap();

        assert_eq!(args.h264_backend.as_deref(), Some("software"));
    }

    #[test]
    fn cli_rejects_password_and_password_file_together() {
        let error = Args::try_parse_from(["hypr-rdp", "--password", "x", "--password-file", "f"])
            .expect_err("--password and --password-file must conflict");

        assert_eq!(error.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    #[test]
    fn config_rejects_password_and_password_file_together() {
        let error = resolve_password(
            None,
            None,
            Some("secret".into()),
            Some("/does/not/matter".into()),
        )
        .expect_err("password and password_file must conflict in the config file");

        assert!(format!("{error:#}").contains("cannot both be set"));
    }

    #[test]
    fn cli_password_file_overrides_config_password() {
        let path = write_password_file("cli-overrides-config-password", "cli-secret\n");

        let password = resolve_password(
            None,
            Some(path.to_str().unwrap().to_owned()),
            Some("config-secret".into()),
            None,
        )
        .expect("password loads from file");

        assert_eq!(password, "cli-secret");
        fs::remove_file(&path).expect("remove password file");
    }

    #[test]
    fn cli_password_overrides_config_password_file() {
        let path = write_password_file("cli-overrides-config-password-file", "config-secret\n");

        let password = resolve_password(
            Some("cli-secret".into()),
            None,
            None,
            Some(path.to_str().unwrap().to_owned()),
        )
        .expect("password comes from the CLI literal");

        assert_eq!(password, "cli-secret");
        fs::remove_file(&path).expect("remove password file");
    }

    #[test]
    fn config_password_file_loads_successfully() {
        let path = write_password_file("config-password-file-loads", "config-secret\n");

        let password = resolve_password(None, None, None, Some(path.to_str().unwrap().to_owned()))
            .expect("password loads from config-specified file");

        assert_eq!(password, "config-secret");
        fs::remove_file(&path).expect("remove password file");
    }

    #[test]
    fn no_password_source_yields_empty_password() {
        let password = resolve_password(None, None, None, None).expect("no sources is fine");
        assert_eq!(password, "");
    }

    #[test]
    fn password_file_missing_fails_startup() {
        let path = temp_config_path("password-file-missing");
        let _ = fs::remove_file(&path);

        let error = read_password_file(path.to_str().unwrap())
            .expect_err("a missing password file must fail startup");

        assert!(format!("{error:#}").contains("failed to read password file"));
    }

    #[test]
    fn password_file_empty_fails_startup() {
        let path = write_password_file("password-file-empty", "");

        let error = read_password_file(path.to_str().unwrap())
            .expect_err("an empty password file must fail startup");

        assert!(format!("{error:#}").contains("is empty"));
        fs::remove_file(&path).expect("remove password file");
    }

    #[test]
    fn password_file_empty_after_stripping_line_ending_fails_startup() {
        let path = write_password_file("password-file-only-newline", "\n");

        let error = read_password_file(path.to_str().unwrap())
            .expect_err("a file containing only a line ending must fail startup");

        assert!(format!("{error:#}").contains("is empty"));
        fs::remove_file(&path).expect("remove password file");
    }

    #[test]
    fn password_file_invalid_utf8_fails_startup() {
        let path = temp_config_path("password-file-invalid-utf8");
        fs::write(&path, [0xff]).expect("write password file");

        let error = read_password_file(path.to_str().unwrap())
            .expect_err("an invalid UTF-8 password file must fail startup");

        assert!(format!("{error:#}").contains("is not valid UTF-8"));
        fs::remove_file(&path).expect("remove password file");
    }

    #[test]
    fn password_file_strips_exactly_one_trailing_line_ending() {
        for (content, expected) in [
            ("secret\n", "secret"),
            ("secret\r\n", "secret"),
            ("secret", "secret"),
            ("secret\n\n", "secret\n"),
            (" secret with spaces \n", " secret with spaces "),
        ] {
            let path = write_password_file("password-file-strip", content);

            let password = read_password_file(path.to_str().unwrap()).expect("password file loads");

            assert_eq!(password, expected, "content: {content:?}");
            fs::remove_file(&path).expect("remove password file");
        }
    }

    #[cfg(unix)]
    #[test]
    fn password_file_unreadable_fails_startup() {
        use std::os::unix::fs::PermissionsExt;

        let path = write_password_file("password-file-unreadable", "secret\n");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o000))
            .expect("remove read permission");

        // Skip if running as a user that bypasses permission bits (e.g. root).
        if fs::read(&path).is_ok() {
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).ok();
            fs::remove_file(&path).ok();
            return;
        }

        let error = read_password_file(path.to_str().unwrap())
            .expect_err("an unreadable password file must fail startup");

        assert!(format!("{error:#}").contains("failed to read password file"));

        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).ok();
        fs::remove_file(&path).expect("remove password file");
    }

    fn write_password_file(name: &str, content: &str) -> PathBuf {
        let path = temp_config_path(name);
        fs::write(&path, content).expect("write password file");
        path
    }

    #[test]
    fn cli_accepts_session_hook_commands() {
        let args = Args::try_parse_from([
            "hypr-rdp",
            "--on-session-start",
            "start command",
            "--on-session-end",
            "end command",
        ])
        .unwrap();

        assert_eq!(args.on_session_start.as_deref(), Some("start command"));
        assert_eq!(args.on_session_end.as_deref(), Some("end command"));
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
            token in prop_oneof![Just("clearcodec".to_owned()), "[a-z0-9_-]{0,16}"]
        ) {
            match token.as_str() {
                "auto" | "avc420" | "avc444" | "clearcodec" => {
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

            match token.as_str() {
                "client" | "compositor" => {
                    prop_assert!(parse_keyboard_layout_policy(&token).is_ok());
                }
                _ => {
                    prop_assert!(parse_keyboard_layout_policy(&token).is_err());
                }
            }

            match token.as_str() {
                "mirror" | "redirect" | "off" => {
                    prop_assert!(parse_audio_mode(&token).is_ok());
                }
                _ => {
                    prop_assert!(parse_audio_mode(&token).is_err());
                }
            }

            match token.as_str() {
                "auto" | "software" | "vaapi" => {
                    prop_assert!(parse_h264_backend_policy(&token).is_ok());
                }
                _ => {
                    prop_assert!(parse_h264_backend_policy(&token).is_err());
                }
            }
        }
    }
}

#[cfg(test)]
mod pam_config_tests {
    use super::*;
    use proptest::prelude::*;

    fn resolve(cli: &[&str], file: &str) -> anyhow::Result<AuthConfig> {
        let mut args = Args::try_parse_from(cli)?;
        let mut config = toml::from_str(file)?;
        resolve_authentication(&mut args, &mut config)
    }

    #[test]
    fn cli_domain_alone_is_a_startup_error_not_no_auth() {
        for option in ["--domain", "-d"] {
            let error = resolve(&["hypr-rdp", option, "WORKGROUP"], "")
                .expect_err("a domain alone must not start the server");
            assert!(format!("{error:#}").contains("no credentials configured"));
        }
    }

    #[test]
    fn missing_or_half_credentials_never_start_without_authentication() {
        for (cli, file) in [
            (&["hypr-rdp"][..], ""),
            (&["hypr-rdp", "--auth-mode", "configured"][..], ""),
            (&["hypr-rdp"][..], "auth_mode = 'configured'"),
            (&["hypr-rdp"][..], "domain = 'WORKGROUP'"),
            (&["hypr-rdp", "-u", "", "-p", ""][..], ""),
            (&["hypr-rdp", "-u", "alice"][..], ""),
            (&["hypr-rdp"][..], "username = 'alice'"),
            (&["hypr-rdp", "-p", "secret"][..], ""),
            (&["hypr-rdp"][..], "password = 'secret'"),
            (
                &["hypr-rdp", "-p", ""][..],
                "username = 'alice'\npassword = 'config'",
            ),
            (
                &["hypr-rdp", "-u", ""][..],
                "username = 'alice'\npassword = 'config'",
            ),
        ] {
            assert!(resolve(cli, file).is_err(), "{cli:?}, {file:?}");
        }
    }

    #[test]
    fn explicit_none_is_the_only_unauthenticated_mode() {
        assert_eq!(
            resolve(&["hypr-rdp", "--auth-mode", "none"], "").unwrap(),
            AuthConfig::None
        );
        assert_eq!(
            resolve(&["hypr-rdp"], "auth_mode = 'none'").unwrap(),
            AuthConfig::None
        );
        // An explicit CLI mode overrides the file mode in both directions.
        assert!(matches!(
            resolve(
                &[
                    "hypr-rdp",
                    "--auth-mode",
                    "configured",
                    "-u",
                    "a",
                    "-p",
                    "b"
                ],
                "auth_mode = 'none'"
            )
            .unwrap(),
            AuthConfig::Configured(_)
        ));
        assert_eq!(
            resolve(&["hypr-rdp", "--auth-mode", "none"], "auth_mode = 'pam'").unwrap(),
            AuthConfig::None
        );
    }

    #[test]
    fn none_rejects_every_credential_or_pam_input_from_either_source() {
        for field in [
            "username",
            "domain",
            "password",
            "password_file",
            "pam_service",
        ] {
            for value in ["", "x"] {
                let file = format!("auth_mode = 'none'\n{field} = '{value}'");
                let error = resolve(&["hypr-rdp"], &file)
                    .expect_err("none must not silently ignore configured inputs");
                assert!(format!("{error:#}").contains("auth-mode none"), "{file}");
            }
        }
        for option in ["-u", "-d", "-p", "--password-file", "--pam-service"] {
            for value in ["", "x"] {
                let cli = ["hypr-rdp", "--auth-mode", "none", option, value];
                let error = resolve(&cli, "").expect_err("none must reject CLI inputs");
                assert!(format!("{error:#}").contains("auth-mode none"), "{cli:?}");
            }
        }
    }

    #[test]
    fn configured_domain_resolves_cli_over_toml_without_changing_credentials() {
        for (cli, file, expected_domain) in [
            (
                &["hypr-rdp"][..],
                "username = ' Alice '\npassword = ' secret '",
                None,
            ),
            (
                &["hypr-rdp"][..],
                "username = ' Alice '\npassword = ' secret '\ndomain = ' File-MiXeD '",
                Some(" File-MiXeD "),
            ),
            (
                &["hypr-rdp", "--domain", " cLi-MiXeD "][..],
                "username = ' Alice '\npassword = ' secret '\ndomain = ' File-MiXeD '",
                Some(" cLi-MiXeD "),
            ),
            (
                &["hypr-rdp", "-d", "Alias"][..],
                "username = ' Alice '\npassword = ' secret '\ndomain = ' File-MiXeD '",
                Some("Alias"),
            ),
        ] {
            assert_eq!(
                resolve(cli, file).unwrap(),
                AuthConfig::Configured(ConfigCredentials {
                    username: " Alice ".into(),
                    domain: expected_domain.map(str::to_owned),
                    password: " secret ".into(),
                }),
                "{cli:?}, {file:?}"
            );
        }
    }

    #[test]
    fn empty_domain_clears_toml_without_trimming_nonempty_values() {
        for (cli, domain_config, expected_domain) in [
            (
                &["hypr-rdp", "--domain", ""][..],
                "domain = 'FileDomain'",
                None,
            ),
            (
                &["hypr-rdp", "--domain="][..],
                "domain = 'FileDomain'",
                None,
            ),
            (&["hypr-rdp", "-d", ""][..], "domain = 'FileDomain'", None),
            (&["hypr-rdp"][..], "domain = ''", None),
            (
                &["hypr-rdp", "--domain", " \t "][..],
                "domain = 'FileDomain'",
                Some(" \t "),
            ),
            (&["hypr-rdp"][..], "domain = '   '", Some("   ")),
        ] {
            let file = format!("username = 'alice'\npassword = 'secret'\n{domain_config}");
            assert_eq!(
                resolve(cli, &file).unwrap(),
                AuthConfig::Configured(ConfigCredentials {
                    username: "alice".into(),
                    domain: expected_domain.map(str::to_owned),
                    password: "secret".into(),
                }),
                "{cli:?}, {file:?}"
            );
        }
    }

    #[test]
    fn configured_domain_preserves_credential_classification() {
        assert!(resolve(&["hypr-rdp"], "domain = 'WORKGROUP'").is_err());
        for (username, password, enabled) in [
            ("", "", false),
            ("user", "", false),
            ("", "secret", false),
            (" User ", " secret ", true),
            ("DOMAIN\\User", "secret", true),
            ("User@example.org", "secret", true),
            (" ", " ", true),
        ] {
            for (cli_domain, file, expected_domain) in [
                (&["hypr-rdp"][..], "", None),
                (
                    &["hypr-rdp", "--domain", "WORKGROUP"][..],
                    "",
                    Some("WORKGROUP"),
                ),
                (&["hypr-rdp"][..], "domain = 'MiXeD'", Some("MiXeD")),
                (
                    &["hypr-rdp", "--domain", ""][..],
                    "domain = 'FileDomain'",
                    None,
                ),
            ] {
                let mut cli = cli_domain.to_vec();
                cli.extend(["-u", username, "-p", password]);
                let actual = resolve(&cli, file);
                if enabled {
                    let expected = AuthConfig::Configured(ConfigCredentials {
                        username: username.into(),
                        domain: expected_domain.map(str::to_owned),
                        password: password.into(),
                    });
                    assert_eq!(actual.unwrap(), expected, "{cli:?}, {file:?}");
                } else {
                    assert!(actual.is_err(), "{cli:?}, {file:?}");
                }
            }
        }
    }

    #[test]
    fn pam_is_explicit_and_configured_credentials_remain_the_default() {
        assert!(resolve(&["hypr-rdp"], "").is_err());
        assert_eq!(
            resolve(&["hypr-rdp", "--auth-mode", "pam"], "").unwrap(),
            AuthConfig::Pam {
                service: "hypr-rdp".into()
            }
        );
        assert!(matches!(
            resolve(&["hypr-rdp", "-u", "alice", "-p", "secret"], "").unwrap(),
            AuthConfig::Configured(_)
        ));
        assert_eq!(
            resolve(
                &["hypr-rdp", "--pam-service", "custom"],
                "auth_mode = 'pam'"
            )
            .unwrap(),
            AuthConfig::Pam {
                service: "custom".into()
            }
        );
    }

    #[test]
    fn pam_conflicts_and_invalid_modes_never_downgrade_authentication() {
        for field in ["username", "password", "password_file"] {
            assert!(resolve(
                &["hypr-rdp", "--auth-mode", "pam"],
                &format!("{field} = ''")
            )
            .is_err());
        }
        for cli in [
            vec!["hypr-rdp", "--auth-mode", "pam", "-u", "alice"],
            vec!["hypr-rdp", "--auth-mode", "pam", "-p", "secret"],
            vec![
                "hypr-rdp",
                "--auth-mode",
                "pam",
                "--password-file",
                "/nonexistent",
            ],
            vec!["hypr-rdp", "--auth-mode", "typo"],
            vec!["hypr-rdp", "--pam-service", "hypr-rdp"],
            vec![
                "hypr-rdp",
                "--auth-mode",
                "pam",
                "--pam-service",
                "../login",
            ],
        ] {
            assert!(resolve(&cli, "").is_err(), "{cli:?}");
        }
    }

    #[test]
    fn pam_rejects_domain_presence_in_either_source_even_when_empty() {
        for domain in ["", "WORKGROUP"] {
            for (cli, file) in [
                (
                    vec!["hypr-rdp", "--auth-mode", "pam", "--domain", domain],
                    String::new(),
                ),
                (
                    vec!["hypr-rdp", "--auth-mode", "pam"],
                    format!("domain = '{domain}'"),
                ),
                (
                    vec!["hypr-rdp", "--domain", domain],
                    "auth_mode = 'pam'".into(),
                ),
                (
                    vec!["hypr-rdp"],
                    format!("auth_mode = 'pam'\ndomain = '{domain}'"),
                ),
                (
                    vec!["hypr-rdp", "--auth-mode", "pam", "--domain", ""],
                    format!("domain = '{domain}'"),
                ),
            ] {
                let error = resolve(&cli, &file)
                    .expect_err("PAM must reject a configured domain before normalization");
                assert!(format!("{error:#}").contains("domain"), "{cli:?}, {file:?}");
            }
        }
    }

    #[test]
    fn configured_domain_preserves_password_source_precedence() {
        use std::io::Write as _;

        let path = std::env::temp_dir().join(format!(
            "hypr-rdp-domain-password-precedence-{}.txt",
            std::process::id()
        ));
        let mut password_file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .unwrap();
        password_file.write_all(b" file Secret \r\n").unwrap();
        drop(password_file);
        let password_path = path.to_str().unwrap();
        let config_password_file = format!(
            "password_file = {}",
            toml::Value::String(password_path.into())
        );
        for (cli, file, expected_domain, expected_password) in [
            (
                vec!["hypr-rdp", "--domain", "Cli", "-p", " cli "],
                "username = 'alice'\ndomain = 'File'\npassword_file = '/nonexistent/domain-test-secret'".into(),
                "Cli",
                " cli ",
            ),
            (
                vec!["hypr-rdp", "--password-file", password_path],
                "username = 'alice'\ndomain = 'File'\npassword = 'config'".into(),
                "File",
                " file Secret ",
            ),
            (
                vec!["hypr-rdp", "--domain", "Cli"],
                format!("username = 'alice'\ndomain = 'File'\n{config_password_file}"),
                "Cli",
                " file Secret ",
            ),
        ] {
            assert_eq!(
                resolve(&cli, &file).unwrap(),
                AuthConfig::Configured(ConfigCredentials {
                    username: "alice".into(),
                    domain: Some(expected_domain.into()),
                    password: expected_password.into(),
                }),
                "{cli:?}, {file:?}"
            );
        }
        assert!(resolve(
            &["hypr-rdp", "--domain", "Cli", "-p", "cli"],
            &format!("username = 'alice'\npassword = 'config'\n{config_password_file}")
        )
        .is_err());
        // An empty CLI password still overrides the file one, which now refuses
        // startup instead of matching an empty secret.
        let error = resolve(
            &["hypr-rdp", "-p", ""],
            "username = 'alice'\ndomain = 'File'\npassword = 'config'",
        )
        .expect_err("an empty password override must not start");
        assert!(format!("{error:#}").contains("password is empty"));
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn configured_mode_override_preserves_existing_password_precedence() {
        let actual = resolve(
            &["hypr-rdp", "--auth-mode", "configured", "-p", "cli"],
            "auth_mode = 'pam'\nusername = 'alice'\npassword = 'config'",
        )
        .unwrap();
        assert_eq!(
            actual,
            AuthConfig::Configured(ConfigCredentials {
                username: "alice".into(),
                domain: None,
                password: "cli".into()
            })
        );
    }

    proptest! {
        #[test]
        fn generated_auth_mode_tokens_never_downgrade_authentication(
            token in prop_oneof![
                Just("none".to_owned()),
                Just("pam".to_owned()),
                Just("configured".to_owned()),
                "[A-Za-z0-9_-]{0,16}",
            ],
            with_credentials in any::<bool>(),
        ) {
            let mut cli = vec!["hypr-rdp", "--auth-mode", token.as_str()];
            if with_credentials {
                cli.extend(["-u", "alice", "-p", "secret"]);
            }
            let result = resolve(&cli, "");
            match (token.as_str(), with_credentials) {
                ("none", false) => prop_assert_eq!(result.unwrap(), AuthConfig::None),
                ("pam", false) => {
                    let is_pam = matches!(result.unwrap(), AuthConfig::Pam { .. });
                    prop_assert!(is_pam)
                },
                ("configured", true) => {
                    let is_configured = matches!(result.unwrap(), AuthConfig::Configured(_));
                    prop_assert!(is_configured)
                },
                _ => prop_assert!(result.is_err(), "{:?}", cli),
            }
        }

        #[test]
        fn generated_configured_domain_precedence_preserves_optional_sources(
            nonempty_domain in "[A-Za-z0-9éΩ _@.-]{1,32}",
            file_domain in prop::option::of("[A-Za-z0-9éΩ _@.-]{0,32}"),
        ) {
            let mut file = "username = 'alice'\npassword = 'secret'".to_owned();
            if let Some(domain) = file_domain {
                file.push_str(&format!("\ndomain = {}", toml::Value::String(domain)));
            }
            let cli_option = format!("--domain={nonempty_domain}");
            prop_assert_eq!(
                resolve(&["hypr-rdp", &cli_option], &file).unwrap(),
                AuthConfig::Configured(ConfigCredentials {
                    username: "alice".into(),
                    domain: Some(nonempty_domain.clone()),
                    password: "secret".into(),
                })
            );
            prop_assert_eq!(
                resolve(&["hypr-rdp", "--domain="], &file).unwrap(),
                AuthConfig::Configured(ConfigCredentials {
                    username: "alice".into(),
                    domain: None,
                    password: "secret".into(),
                })
            );
            let file = format!(
                "username = 'alice'\npassword = 'secret'\ndomain = {}",
                toml::Value::String(nonempty_domain.clone())
            );
            prop_assert_eq!(
                resolve(&["hypr-rdp"], &file).unwrap(),
                AuthConfig::Configured(ConfigCredentials {
                    username: "alice".into(),
                    domain: Some(nonempty_domain),
                    password: "secret".into(),
                })
            );
        }
    }
}
