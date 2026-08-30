//! Hyprland IPC socket communication.
//!
//! Direct Unix socket communication instead of spawning hyprctl subprocesses.

use std::sync::OnceLock;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};

fn socket_path() -> Result<String> {
    let sig = std::env::var("HYPRLAND_INSTANCE_SIGNATURE")
        .context("HYPRLAND_INSTANCE_SIGNATURE not set")?;
    let runtime_dir = std::env::var("XDG_RUNTIME_DIR").context("XDG_RUNTIME_DIR not set")?;
    Ok(format!("{}/hypr/{}/.socket.sock", runtime_dir, sig))
}

fn socket2_path() -> Result<String> {
    let sig = std::env::var("HYPRLAND_INSTANCE_SIGNATURE")
        .context("HYPRLAND_INSTANCE_SIGNATURE not set")?;
    let runtime_dir = std::env::var("XDG_RUNTIME_DIR").context("XDG_RUNTIME_DIR not set")?;
    Ok(format!("{}/hypr/{}/.socket2.sock", runtime_dir, sig))
}

/// Send a raw command to Hyprland IPC socket and return the response.
fn send_command(cmd: &str) -> Result<String> {
    let path = socket_path()?;
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

/// Scale applied to the managed headless output, set once at startup from config.
static HEADLESS_SCALE: OnceLock<f64> = OnceLock::new();

/// Record the scale to apply to the managed headless output.
///
/// Called once during startup. Later calls are ignored, so the value stays
/// stable for the lifetime of the process.
pub fn set_headless_scale(scale: f64) {
    let _ = HEADLESS_SCALE.set(scale);
}

fn headless_scale() -> f64 {
    HEADLESS_SCALE.get().copied().unwrap_or(1.0)
}

/// Build the monitor rule for the managed headless output.
///
/// The output is parked off-screen at -9999x0 so it never displaces real
/// monitors in the layout. The scale comes from config; Hyprland only accepts
/// scales that divide the mode into whole logical pixels, so an unusable value
/// is rejected by the compositor rather than silently rounded here.
pub fn headless_monitor_rule(name: &str, mode: &str) -> String {
    headless_monitor_rule_with(name, mode, headless_scale())
}

fn headless_monitor_rule_with(name: &str, mode: &str, scale: f64) -> String {
    format!("{},{},-9999x0,{}", name, mode, scale)
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
        let path = socket2_path()?;
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
        headless_monitor_rule_with, is_non_legacy_parser_error, monitor_rule_to_lua,
        option_string_from_response,
    };

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
    fn headless_monitor_rule_defaults_to_scale_one() {
        assert_eq!(
            headless_monitor_rule_with("hypr-rdp-1", "1920x1080@60", 1.0),
            "hypr-rdp-1,1920x1080@60,-9999x0,1"
        );
    }

    #[test]
    fn headless_monitor_rule_carries_configured_scale() {
        assert_eq!(
            headless_monitor_rule_with("hypr-rdp-1", "3024x1896@60", 2.0),
            "hypr-rdp-1,3024x1896@60,-9999x0,2"
        );
    }

    #[test]
    fn headless_monitor_rule_keeps_fractional_scale() {
        assert_eq!(
            headless_monitor_rule_with("hypr-rdp-1", "2560x1440@60", 1.5),
            "hypr-rdp-1,2560x1440@60,-9999x0,1.5"
        );
    }

    #[test]
    fn generated_headless_rule_with_scale_translates_to_lua() {
        let rule = headless_monitor_rule_with("hypr-rdp-1", "3024x1896@60", 2.0);
        let lua = monitor_rule_to_lua(&rule).expect("monitor rule translates");

        assert_eq!(
            lua,
            r#"hl.monitor({ output = "hypr-rdp-1", mode = "3024x1896@60", position = "-9999x0", scale = 2 })"#
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
