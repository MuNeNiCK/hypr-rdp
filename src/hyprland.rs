//! Hyprland IPC socket communication.
//!
//! Direct Unix socket communication instead of spawning hyprctl subprocesses.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};

const COMMAND_SOCKET: &str = ".socket.sock";
const EVENT_SOCKET: &str = ".socket2.sock";

static INSTANCE_SIGNATURE: OnceLock<String> = OnceLock::new();

#[derive(Debug, PartialEq, Eq)]
struct InstanceCandidate {
    signature: String,
    pid: libc::pid_t,
    wayland_display: String,
}

pub(crate) fn initialize() -> Result<String> {
    instance_signature()
}

fn instance_signature() -> Result<String> {
    if let Some(signature) = INSTANCE_SIGNATURE.get() {
        return Ok(signature.clone());
    }

    let explicit = std::env::var("HYPRLAND_INSTANCE_SIGNATURE").ok();
    let wayland_display = std::env::var("WAYLAND_DISPLAY").ok();
    let hypr_dir = runtime_hypr_dir();
    let signature = resolve_instance(explicit, wayland_display, &hypr_dir, process_is_live)?;

    let _ = INSTANCE_SIGNATURE.set(signature);
    Ok(INSTANCE_SIGNATURE
        .get()
        .expect("instance signature was initialized")
        .clone())
}

fn resolve_instance(
    explicit: Option<String>,
    wayland_display: Option<String>,
    hypr_dir: &Path,
    is_live: impl Fn(libc::pid_t) -> bool,
) -> Result<String> {
    let candidates = if explicit.as_deref().is_some_and(|value| !value.is_empty()) {
        Vec::new()
    } else {
        read_instance_candidates(hypr_dir)?
    };
    select_instance(explicit, wayland_display, candidates, is_live)
}

fn runtime_hypr_dir() -> PathBuf {
    match std::env::var_os("XDG_RUNTIME_DIR").filter(|value| !value.is_empty()) {
        Some(runtime_dir) => PathBuf::from(runtime_dir).join("hypr"),
        None => PathBuf::from(format!("/run/user/{}/hypr", effective_uid())),
    }
}

fn effective_uid() -> libc::uid_t {
    // SAFETY: geteuid has no arguments and no failure mode.
    unsafe { libc::geteuid() }
}

fn read_instance_candidates(hypr_dir: &Path) -> Result<Vec<InstanceCandidate>> {
    let entries = std::fs::read_dir(hypr_dir).with_context(|| {
        format!(
            "failed to read Hyprland runtime directory {}",
            hypr_dir.display()
        )
    })?;
    let mut candidates = Vec::new();

    for entry in entries.flatten() {
        if !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            continue;
        }
        let Some(signature) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let Ok(lock) = std::fs::read_to_string(entry.path().join("hyprland.lock")) else {
            continue;
        };
        if let Some(candidate) = parse_instance_lock(signature, &lock) {
            candidates.push(candidate);
        }
    }

    Ok(candidates)
}

fn parse_instance_lock(signature: String, lock: &str) -> Option<InstanceCandidate> {
    let first_separator = signature.find('_')?;
    let last_separator = signature.rfind('_')?;
    if last_separator <= first_separator + 1
        || signature[first_separator + 1..last_separator]
            .parse::<u64>()
            .is_err()
    {
        return None;
    }

    let mut lines = lock.lines();
    let pid = lines.next()?.parse::<libc::pid_t>().ok()?;
    let wayland_display = lines.next()?.to_owned();
    if pid <= 0 || wayland_display.is_empty() || lines.next().is_some() {
        return None;
    }

    Some(InstanceCandidate {
        signature,
        pid,
        wayland_display,
    })
}

fn select_instance(
    explicit: Option<String>,
    wayland_display: Option<String>,
    candidates: Vec<InstanceCandidate>,
    is_live: impl Fn(libc::pid_t) -> bool,
) -> Result<String> {
    if let Some(signature) = explicit.filter(|value| !value.is_empty()) {
        return Ok(signature);
    }

    let wayland_display = wayland_display
        .filter(|value| !value.is_empty())
        .context("HYPRLAND_INSTANCE_SIGNATURE is not set and WAYLAND_DISPLAY is unavailable")?;
    let mut matches: Vec<_> = candidates
        .into_iter()
        .filter(|candidate| candidate.wayland_display == wayland_display && is_live(candidate.pid))
        .map(|candidate| candidate.signature)
        .collect();
    matches.sort();

    match matches.as_slice() {
        [signature] => Ok(signature.clone()),
        [] => bail!(
            "no live Hyprland instance matches WAYLAND_DISPLAY={wayland_display}; set HYPRLAND_INSTANCE_SIGNATURE explicitly"
        ),
        _ => bail!(
            "multiple live Hyprland instances match WAYLAND_DISPLAY={wayland_display} ({}); set HYPRLAND_INSTANCE_SIGNATURE explicitly",
            matches.join(", ")
        ),
    }
}

fn process_is_live(pid: libc::pid_t) -> bool {
    // SAFETY: signal 0 performs permission/existence checking without sending a signal.
    let result = unsafe { libc::kill(pid, 0) };
    result == 0 || std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

fn instance_socket_path(name: &str) -> Result<String> {
    let runtime_dir = runtime_hypr_dir();
    let signature = instance_signature()?;
    Ok(socket_path_in(&runtime_dir, &signature, name))
}

fn socket_path_in(runtime_dir: &Path, signature: &str, name: &str) -> String {
    runtime_dir
        .join(signature)
        .join(name)
        .to_string_lossy()
        .into_owned()
}

/// Send a raw command to Hyprland IPC socket and return the response.
fn send_command(cmd: &str) -> Result<String> {
    let path = instance_socket_path(COMMAND_SOCKET)?;
    let mut sock = UnixStream::connect(&path)
        .with_context(|| format!("failed to connect to Hyprland socket: {}", path))?;
    sock.set_read_timeout(Some(Duration::from_secs(3)))?;
    sock.write_all(cmd.as_bytes())?;
    sock.shutdown(std::net::Shutdown::Write)?;
    let mut response = String::new();
    sock.read_to_string(&mut response)?;
    Ok(response)
}

/// Send a command that expects "ok" response (dispatch, keyword, output).
fn send_action(cmd: &str) -> Result<()> {
    let response = send_command(cmd)?;
    if response.starts_with("ok") || response.trim().is_empty() {
        Ok(())
    } else {
        bail!("Hyprland IPC error: {}", response.trim())
    }
}

/// Query monitors as JSON value (array).
pub fn monitors() -> Result<serde_json::Value> {
    let response = send_command("j/monitors")?;
    serde_json::from_str(&response).context("failed to parse Hyprland monitors JSON")
}

/// Query input devices as JSON value (object with a "keyboards" array).
pub fn devices() -> Result<serde_json::Value> {
    let response = send_command("j/devices")?;
    serde_json::from_str(&response).context("failed to parse Hyprland devices JSON")
}

/// Query a Hyprland option string value.
pub fn option_string(option: &str) -> Result<Option<String>> {
    let response = send_command(&format!("j/getoption {}", option))?;
    option_string_from_response(&response)
        .with_context(|| format!("failed to parse Hyprland option {}", option))
}

fn option_string_from_response(response: &str) -> Result<Option<String>> {
    let value: serde_json::Value =
        serde_json::from_str(response).context("failed to parse Hyprland option JSON")?;
    Ok(value
        .get("str")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned))
}

/// Create a headless output with a custom name prefix.
/// Hyprland will name it `{name}-1`, `{name}-2`, etc.
pub fn output_create_headless(name: &str) -> Result<()> {
    send_action(&format!("output create headless {}", name))
}

/// Set a monitor rule (e.g. "HEADLESS-1,1920x1080@60,-9999x0,1").
///
/// Hyprland's new (Lua) config parser rejects `keyword` with
/// "keyword can't work with non-legacy parsers. Use eval."
/// In that case, retry as `eval hl.monitor({...})`.
pub fn keyword_monitor(rule: &str) -> Result<()> {
    match send_action(&format!("keyword monitor {}", rule)) {
        Ok(()) => Ok(()),
        Err(e) if is_non_legacy_parser_error(&e) => {
            send_action(&format!("eval {}", monitor_rule_to_lua(rule)?))
        }
        Err(e) => Err(e),
    }
}

fn is_non_legacy_parser_error(error: &anyhow::Error) -> bool {
    error
        .chain()
        .any(|cause| cause.to_string().contains("non-legacy parsers"))
}

fn monitor_rule_to_lua(rule: &str) -> Result<String> {
    let parts: Vec<&str> = rule.splitn(4, ',').map(str::trim).collect();
    let [output, mode, position, scale] = parts.as_slice() else {
        bail!(
            "cannot translate monitor rule to Lua (expected 4 fields): {}",
            rule
        );
    };

    Ok(format!(
        "hl.monitor({{ output = {}, mode = {}, position = {}, scale = {} }})",
        lua_string(output)?,
        lua_string(mode)?,
        lua_string(position)?,
        monitor_scale_to_lua(scale)?
    ))
}

fn monitor_scale_to_lua(scale: &str) -> Result<String> {
    if scale.is_empty() {
        bail!("cannot translate monitor rule to Lua with empty scale");
    }

    if let Ok(value) = scale.parse::<f64>() {
        if value.is_finite() {
            return Ok(scale.to_string());
        }
    }

    lua_string(scale)
}

fn lua_string(value: &str) -> Result<String> {
    let mut escaped = String::with_capacity(value.len() + 2);
    escaped.push('"');

    for ch in value.chars() {
        match ch {
            '\\' => escaped.push_str("\\\\"),
            '"' => escaped.push_str("\\\""),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            ch if ch.is_control() => bail!("cannot encode control character in Lua string"),
            ch => escaped.push(ch),
        }
    }

    escaped.push('"');
    Ok(escaped)
}

/// Remove a named output.
pub fn output_remove(name: &str) -> Result<()> {
    send_action(&format!("output remove {}", name))
}

/// Event stream from Hyprland socket2 (subscription-based).
///
/// Connect before triggering the action to avoid missing events.
/// After connecting, call `ensure_registered()` to guarantee Hyprland
/// has accepted the connection before emitting events.
pub struct EventStream {
    sock: UnixStream,
    buf: Vec<u8>,
}

impl EventStream {
    pub fn connect() -> Result<Self> {
        let path = instance_socket_path(EVENT_SOCKET)?;
        let sock = UnixStream::connect(&path)
            .with_context(|| format!("failed to connect to Hyprland event socket: {}", path))?;
        sock.set_read_timeout(Some(Duration::from_millis(500)))?;
        Ok(Self {
            sock,
            buf: Vec::new(),
        })
    }

    /// Force a socket1 roundtrip so Hyprland's event loop processes our
    /// socket2 accept() before we trigger any actions.
    pub fn ensure_registered(&self) -> Result<()> {
        let _ = monitors()?;
        Ok(())
    }

    /// Return the next `EVENT>>DATA` pair, or `None` when `timeout` elapses
    /// without one. Errors only when the socket itself fails, so callers can
    /// tell an idle stream from a dead one.
    pub fn next_event(&mut self, timeout: Duration) -> Result<Option<(String, String)>> {
        let start = Instant::now();
        let mut raw = [0u8; 4096];

        loop {
            if let Some(newline_pos) = self.buf.iter().position(|&byte| byte == b'\n') {
                let line: Vec<u8> = self.buf.drain(..=newline_pos).collect();
                // Decode only after framing: a multi-byte character can
                // straddle two socket reads.
                let line = String::from_utf8_lossy(&line[..newline_pos]);
                let line = line.trim();
                if let Some((event, data)) = line.split_once(">>") {
                    return Ok(Some((event.to_string(), data.to_string())));
                }
                continue;
            }

            if start.elapsed() >= timeout {
                return Ok(None);
            }

            match self.sock.read(&mut raw) {
                Ok(0) => bail!("Hyprland event socket closed"),
                Ok(n) => {
                    self.buf.extend_from_slice(&raw[..n]);
                }
                Err(e)
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::TimedOut =>
                {
                    continue;
                }
                Err(e) => return Err(e).context("failed to read Hyprland event"),
            }
        }
    }

    /// Wait for an event matching `event_name` (e.g. "monitoradded").
    /// Returns the event data (text after ">>").
    pub fn wait_for(&mut self, event_name: &str, timeout: Duration) -> Result<String> {
        let start = Instant::now();
        let mut raw = [0u8; 4096];

        loop {
            if start.elapsed() >= timeout {
                bail!(
                    "timed out waiting for '{}' event after {}ms",
                    event_name,
                    timeout.as_millis()
                );
            }

            // Check buffered lines first
            while let Some(newline_pos) = self.buf.iter().position(|&byte| byte == b'\n') {
                let line: Vec<u8> = self.buf.drain(..=newline_pos).collect();
                let line = String::from_utf8_lossy(&line[..newline_pos]);
                let line = line.trim();
                if let Some((event, data)) = line.split_once(">>") {
                    if event == event_name {
                        return Ok(data.to_string());
                    }
                }
            }

            // Read more data from socket
            match self.sock.read(&mut raw) {
                Ok(0) => bail!("Hyprland event socket closed"),
                Ok(n) => {
                    self.buf.extend_from_slice(&raw[..n]);
                }
                Err(e)
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::TimedOut =>
                {
                    continue;
                }
                Err(e) => return Err(e).context("failed to read Hyprland event"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use anyhow::anyhow;

    use super::{
        is_non_legacy_parser_error, monitor_rule_to_lua, option_string_from_response,
        parse_instance_lock, resolve_instance, select_instance, socket_path_in, InstanceCandidate,
        COMMAND_SOCKET, EVENT_SOCKET,
    };

    fn candidate(signature: &str, pid: libc::pid_t, display: &str) -> InstanceCandidate {
        InstanceCandidate {
            signature: signature.to_owned(),
            pid,
            wayland_display: display.to_owned(),
        }
    }

    #[test]
    fn instance_lock_parser_accepts_the_hyprctl_shape() {
        assert_eq!(
            parse_instance_lock("abc_123_456".into(), "42\nwayland-1\n"),
            Some(candidate("abc_123_456", 42, "wayland-1"))
        );
        assert!(parse_instance_lock("invalid".into(), "42\nwayland-1\n").is_none());
        assert!(parse_instance_lock("abc_123_456".into(), "bad\nwayland-1\n").is_none());
        assert!(parse_instance_lock("abc_123_456".into(), "42\nwayland-1\nextra\n").is_none());
    }

    #[test]
    fn explicit_instance_signature_takes_precedence() {
        let selected = select_instance(
            Some("explicit".into()),
            Some("wayland-1".into()),
            vec![candidate("other_1_1", 1, "wayland-1")],
            |_| panic!("explicit selection must not inspect candidates"),
        )
        .expect("explicit instance is selected");

        assert_eq!(selected, "explicit");
    }

    #[test]
    fn discovery_selects_the_live_instance_for_the_wayland_display() {
        let selected = select_instance(
            None,
            Some("wayland-1".into()),
            vec![
                candidate("wrong_1_1", 10, "wayland-2"),
                candidate("dead_2_2", 20, "wayland-1"),
                candidate("selected_3_3", 30, "wayland-1"),
            ],
            |pid| pid == 30,
        )
        .expect("one matching live instance is selected");

        assert_eq!(selected, "selected_3_3");
    }

    #[test]
    fn runtime_lock_discovery_works_without_an_explicit_signature() {
        let root = std::env::temp_dir().join(format!(
            "hypr-rdp-instance-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system time")
                .as_nanos()
        ));
        let instance = root.join("commit_123_456");
        std::fs::create_dir_all(&instance).expect("create fake instance");
        std::fs::write(instance.join("hyprland.lock"), "42\nwayland-1\n").expect("write fake lock");

        let selected = resolve_instance(None, Some("wayland-1".into()), &root, |pid| pid == 42)
            .expect("runtime lock selects an instance");

        assert_eq!(selected, "commit_123_456");
        std::fs::remove_dir_all(root).expect("remove fake runtime");
    }

    #[test]
    fn discovery_rejects_zero_or_multiple_matching_instances() {
        let missing = select_instance(None, Some("wayland-1".into()), Vec::new(), |_| true)
            .expect_err("no match is rejected");
        assert!(missing.to_string().contains("no live Hyprland instance"));

        let ambiguous = select_instance(
            None,
            Some("wayland-1".into()),
            vec![
                candidate("one_1_1", 10, "wayland-1"),
                candidate("two_2_2", 20, "wayland-1"),
            ],
            |_| true,
        )
        .expect_err("multiple matches are rejected");
        assert!(ambiguous.to_string().contains("one_1_1, two_2_2"));
    }

    #[test]
    fn command_and_event_paths_share_the_selected_instance() {
        let runtime = std::path::Path::new("/run/user/1000/hypr");

        assert_eq!(
            socket_path_in(runtime, "sig", COMMAND_SOCKET),
            "/run/user/1000/hypr/sig/.socket.sock"
        );
        assert_eq!(
            socket_path_in(runtime, "sig", EVENT_SOCKET),
            "/run/user/1000/hypr/sig/.socket2.sock"
        );
    }

    #[test]
    fn option_string_parser_returns_non_empty_string_values() {
        let value = option_string_from_response(
            r#"{"option":"input:kb_layout","str":" de , us ","set":true}"#,
        )
        .expect("option parses");

        assert_eq!(value.as_deref(), Some("de , us"));
    }

    #[test]
    fn option_string_parser_treats_empty_strings_as_unset() {
        let value =
            option_string_from_response(r#"{"option":"input:kb_variant","str":"","set":true}"#)
                .expect("option parses");

        assert_eq!(value, None);
    }

    #[test]
    fn monitor_rule_to_lua_translates_generated_headless_rule() {
        let lua = monitor_rule_to_lua("hypr-rdp-1,1920x1080@60,-9999x0,1")
            .expect("monitor rule translates");

        assert_eq!(
            lua,
            r#"hl.monitor({ output = "hypr-rdp-1", mode = "1920x1080@60", position = "-9999x0", scale = 1 })"#
        );
    }

    #[test]
    fn monitor_rule_to_lua_quotes_non_numeric_scale() {
        let lua = monitor_rule_to_lua("DP-1,preferred,auto,auto").expect("monitor rule translates");

        assert_eq!(
            lua,
            r#"hl.monitor({ output = "DP-1", mode = "preferred", position = "auto", scale = "auto" })"#
        );
    }

    #[test]
    fn monitor_rule_to_lua_escapes_lua_strings() {
        let lua =
            monitor_rule_to_lua(r#"DP-"1,modeline 1\2,0x0,1"#).expect("monitor rule translates");

        assert_eq!(
            lua,
            r#"hl.monitor({ output = "DP-\"1", mode = "modeline 1\\2", position = "0x0", scale = 1 })"#
        );
    }

    #[test]
    fn monitor_rule_to_lua_rejects_malformed_rules() {
        let err = monitor_rule_to_lua("hypr-rdp-1,1920x1080@60,-9999x0")
            .expect_err("malformed rule is rejected");

        assert!(err.to_string().contains("expected 4 fields"));
    }

    #[test]
    fn non_legacy_parser_error_detection_matches_hyprland_message() {
        let err =
            anyhow!("Hyprland IPC error: keyword can't work with non-legacy parsers. Use eval.");

        assert!(is_non_legacy_parser_error(&err));
    }

    #[test]
    fn non_legacy_parser_error_detection_ignores_other_errors() {
        let err = anyhow!("Hyprland IPC error: monitor rule failed");

        assert!(!is_non_legacy_parser_error(&err));
    }
}
