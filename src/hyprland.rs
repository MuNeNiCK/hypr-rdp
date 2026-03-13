//! Hyprland IPC socket communication.
//!
//! Direct Unix socket communication instead of spawning hyprctl subprocesses.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

use anyhow::{bail, Context, Result};

fn socket_path() -> Result<String> {
    let sig =
        std::env::var("HYPRLAND_INSTANCE_SIGNATURE").context("HYPRLAND_INSTANCE_SIGNATURE not set")?;
    let runtime_dir = std::env::var("XDG_RUNTIME_DIR").context("XDG_RUNTIME_DIR not set")?;
    Ok(format!("{}/hypr/{}/.socket.sock", runtime_dir, sig))
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

/// Create a headless output.
pub fn output_create_headless() -> Result<()> {
    send_action("output create headless")
}

/// Set a monitor keyword rule (e.g. "HEADLESS-1,1920x1080@60,-9999x0,1").
pub fn keyword_monitor(rule: &str) -> Result<()> {
    send_action(&format!("keyword monitor {}", rule))
}

/// Remove a named output.
pub fn output_remove(name: &str) -> Result<()> {
    send_action(&format!("output remove {}", name))
}

