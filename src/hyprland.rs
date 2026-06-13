//! Hyprland IPC socket communication.
//!
//! Direct Unix socket communication instead of spawning hyprctl subprocesses.

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

/// Set a monitor keyword rule (e.g. "HEADLESS-1,1920x1080@60,-9999x0,1").
///
/// Hyprland's new (Lua) config parser rejects `keyword` with
/// "keyword can't work with non-legacy parsers. Use eval."
/// In that case, retry as `eval hl.monitor({...})`.
pub fn keyword_monitor(rule: &str) -> Result<()> {
    match send_action(&format!("keyword monitor {}", rule)) {
        Ok(()) => Ok(()),
        Err(e) if e.to_string().contains("non-legacy parsers") => {
            let lua = monitor_rule_to_lua(rule)?;
            send_action(&format!("eval {}", lua))
        }
        Err(e) => Err(e),
    }
}

fn monitor_rule_to_lua(rule: &str) -> Result<String> {
    let parts: Vec<&str> = rule.splitn(4, ',').collect();
    let [output, mode, position, scale] = parts.as_slice() else {
        bail!("cannot translate monitor rule to Lua (expected 4 fields): {}", rule);
    };
    Ok(format!(
        r#"hl.monitor({{ output = "{}", mode = "{}", position = "{}", scale = "{}" }})"#,
        output, mode, position, scale
    ))
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
    buf: String,
}

impl EventStream {
    pub fn connect() -> Result<Self> {
        let path = socket2_path()?;
        let sock = UnixStream::connect(&path)
            .with_context(|| format!("failed to connect to Hyprland event socket: {}", path))?;
        sock.set_read_timeout(Some(Duration::from_millis(500)))?;
        Ok(Self {
            sock,
            buf: String::new(),
        })
    }

    /// Force a socket1 roundtrip so Hyprland's event loop processes our
    /// socket2 accept() before we trigger any actions.
    pub fn ensure_registered(&self) -> Result<()> {
        let _ = monitors()?;
        Ok(())
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
            while let Some(newline_pos) = self.buf.find('\n') {
                let line = self.buf[..newline_pos].trim().to_string();
                self.buf = self.buf[newline_pos + 1..].to_string();
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
                    self.buf.push_str(&String::from_utf8_lossy(&raw[..n]));
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
    use super::option_string_from_response;

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
}
