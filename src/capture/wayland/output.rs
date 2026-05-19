use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};

use super::CaptureInfo;

pub(crate) struct HeadlessOutputGuard {
    name: Option<String>,
}

impl HeadlessOutputGuard {
    fn new(name: String) -> Self {
        Self { name: Some(name) }
    }

    /// Take ownership of an existing headless output (for reuse across restarts).
    pub(crate) fn adopt(name: String) -> Self {
        Self { name: Some(name) }
    }
}

impl Drop for HeadlessOutputGuard {
    fn drop(&mut self) {
        if let Some(name) = self.name.take() {
            // Safe to remove here: HyprDisplay::drop() has already joined the
            // capture thread, so no Wayland clients reference this output.
            match crate::hyprland::output_remove(&name) {
                Ok(()) => tracing::info!(name, "Removed headless output"),
                Err(e) => tracing::warn!(name, error = %e, "Failed to remove headless output"),
            }
        }
    }
}

const HEADLESS_PREFIX: &str = "hypr-rdp";

/// List headless outputs created by hypr-rdp (name starts with "hypr-rdp-").
pub(crate) fn list_stale_headless_outputs() -> Result<Vec<String>> {
    let monitors = crate::hyprland::monitors()?;
    let arr = monitors.as_array().context("expected monitors array")?;
    Ok(arr
        .iter()
        .filter_map(|m| {
            let name = m["name"].as_str()?;
            name.starts_with(HEADLESS_PREFIX).then(|| name.to_string())
        })
        .collect())
}

/// Create a headless output in Hyprland at the given resolution.
/// Returns the output name and RAII guard that removes it on drop.
/// The guard is created immediately after the output appears so that
/// any subsequent failure (e.g., keyword_monitor) cleans up automatically.
pub(crate) fn create_headless_output(
    width: u32,
    height: u32,
) -> Result<(String, HeadlessOutputGuard)> {
    // Subscribe to events BEFORE creating the output to catch monitoradded.
    // The ensure_registered() roundtrip guarantees Hyprland has accept()'ed
    // our socket2 connection before we trigger the creation.
    let mut events = crate::hyprland::EventStream::connect()?;
    events.ensure_registered()?;

    crate::hyprland::output_create_headless(HEADLESS_PREFIX)
        .context("failed to create headless output")?;

    // Wait for monitoradded event — data is the output name.
    let name = loop {
        let candidate = events
            .wait_for("monitoradded", Duration::from_secs(5))
            .context("failed to detect new headless output")?;
        if candidate.starts_with(HEADLESS_PREFIX) {
            break candidate;
        }
        tracing::trace!(name = %candidate, "Ignoring unrelated monitoradded event");
    };

    // Guard created immediately — any failure below will clean up the output
    let guard = HeadlessOutputGuard::new(name.clone());

    // Set resolution
    let mode = format!("{}x{}@60", width, height);
    let rule = format!("{},{},-9999x0,1", name, mode);
    crate::hyprland::keyword_monitor(&rule).context("failed to set headless output resolution")?;

    tracing::info!(name = %name, width, height, "Created headless output");
    Ok((name, guard))
}

/// Wait for a Hyprland output to be ready (has non-zero dimensions).
pub(crate) fn wait_for_output(output_name: &str, timeout: Duration) -> Result<()> {
    let start = Instant::now();
    let poll_interval = Duration::from_millis(100);

    loop {
        if let Ok(monitors) = crate::hyprland::monitors() {
            if let Some(arr) = monitors.as_array() {
                let found = arr.iter().any(|m| {
                    m["name"].as_str() == Some(output_name) && m["width"].as_i64().unwrap_or(0) > 0
                });
                if found {
                    return Ok(());
                }
            }
        }

        if start.elapsed() >= timeout {
            bail!(
                "timed out waiting for output '{}' after {}ms",
                output_name,
                timeout.as_millis()
            );
        }

        std::thread::sleep(poll_interval);
    }
}

/// Query a Hyprland output's current dimensions without starting capture.
pub(crate) fn output_info(output_name: &str) -> Result<CaptureInfo> {
    let monitors = crate::hyprland::monitors()?;
    let monitor = monitors
        .as_array()
        .context("expected monitors array")?
        .iter()
        .find(|m| m["name"].as_str() == Some(output_name))
        .with_context(|| format!("output '{}' not found in Hyprland monitors", output_name))?;

    let width = monitor["width"].as_u64().unwrap_or(0) as u32;
    let height = monitor["height"].as_u64().unwrap_or(0) as u32;
    if width == 0 || height == 0 {
        bail!(
            "output '{}' has invalid dimensions: {}x{}",
            output_name,
            width,
            height
        );
    }

    Ok(CaptureInfo {
        width,
        height,
        output_name: output_name.to_string(),
    })
}
