use std::sync::Mutex;

use anyhow::{bail, Context, Result};
use serde_json::Value;

#[derive(Clone, Debug)]
pub(super) struct OutputLayoutSnapshot {
    pub(super) output_name: String,
    pub(super) output_w: u32,
    pub(super) output_h: u32,
    pub(super) layout_extent_w: u32,
    pub(super) layout_extent_h: u32,
    pub(super) output_offset_x: u32,
    pub(super) output_offset_y: u32,
}

#[derive(Debug, Default)]
pub struct SharedOutputLayout {
    inner: Mutex<Option<OutputLayoutSnapshot>>,
}

impl SharedOutputLayout {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn update_from_output(&self, output_name: &str) -> Result<()> {
        let (
            output_w,
            output_h,
            layout_extent_w,
            layout_extent_h,
            output_offset_x,
            output_offset_y,
        ) = query_layout(output_name)?;
        let snapshot = OutputLayoutSnapshot {
            output_name: output_name.to_string(),
            output_w,
            output_h,
            layout_extent_w,
            layout_extent_h,
            output_offset_x,
            output_offset_y,
        };
        tracing::info!(
            output = %snapshot.output_name,
            layout_extent_w = snapshot.layout_extent_w,
            layout_extent_h = snapshot.layout_extent_h,
            output_offset_x = snapshot.output_offset_x,
            output_offset_y = snapshot.output_offset_y,
            "Updated input layout mapping"
        );
        if let Ok(mut guard) = self.inner.lock() {
            *guard = Some(snapshot);
        }
        Ok(())
    }

    pub(super) fn snapshot(&self) -> Option<OutputLayoutSnapshot> {
        self.inner.lock().ok()?.clone()
    }
}

/// Query Hyprland monitor layout to compute coordinate mapping.
/// Returns (layout_total_w, layout_total_h, output_offset_x, output_offset_y)
/// Returns (output_w, output_h, layout_total_w, layout_total_h, output_offset_x, output_offset_y)
fn query_layout(output_name: &str) -> Result<(u32, u32, u32, u32, u32, u32)> {
    let monitors_val = crate::hyprland::monitors()?;
    layout_from_monitors(&monitors_val, output_name)
}

fn required_i64(monitor: &Value, field: &str) -> Result<i64> {
    monitor
        .get(field)
        .and_then(Value::as_i64)
        .with_context(|| format!("monitor has missing or invalid '{field}'"))
}

/// Compute output and global layout bounds from Hyprland monitor JSON.
fn layout_from_monitors(
    monitors_val: &Value,
    output_name: &str,
) -> Result<(u32, u32, u32, u32, u32, u32)> {
    let monitors = monitors_val.as_array().context("expected monitors array")?;
    if monitors.is_empty() {
        bail!("no monitors found");
    }

    // Find layout bounds
    let mut min_x = i64::MAX;
    let mut min_y = i64::MAX;
    let mut max_x = i64::MIN;
    let mut max_y = i64::MIN;
    let mut target = None;

    for m in monitors {
        let name = m
            .get("name")
            .and_then(Value::as_str)
            .context("monitor has missing or invalid 'name'")?;
        let x = required_i64(m, "x")?;
        let y = required_i64(m, "y")?;
        let w = required_i64(m, "width")?;
        let h = required_i64(m, "height")?;
        if w <= 0 || h <= 0 {
            bail!("monitor '{}' has invalid dimensions: {}x{}", name, w, h);
        }
        min_x = min_x.min(x);
        min_y = min_y.min(y);
        max_x = max_x.max(x + w);
        max_y = max_y.max(y + h);

        if name == output_name {
            target = Some((x, y, w, h));
        }
    }

    let (target_x, target_y, target_w, target_h) =
        target.context(format!("output '{}' not found", output_name))?;
    let layout_w = u32::try_from(max_x - min_x).context("layout width is out of range")?;
    let layout_h = u32::try_from(max_y - min_y).context("layout height is out of range")?;
    if layout_w == 0 || layout_h == 0 {
        bail!("invalid layout bounds: {}x{}", layout_w, layout_h);
    }
    let offset_x = u32::try_from(target_x - min_x).context("output x offset is out of range")?;
    let offset_y = u32::try_from(target_y - min_y).context("output y offset is out of range")?;

    Ok((
        u32::try_from(target_w).context("output width is out of range")?,
        u32::try_from(target_h).context("output height is out of range")?,
        layout_w,
        layout_h,
        offset_x,
        offset_y,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn layout_parser_preserves_negative_monitor_offsets() {
        let monitors = json!([
            { "name": "DP-1", "x": -1920, "y": 0, "width": 1920, "height": 1080 },
            { "name": "hypr-rdp-1", "x": 0, "y": 180, "width": 1280, "height": 720 }
        ]);

        assert_eq!(
            layout_from_monitors(&monitors, "hypr-rdp-1").expect("layout parses"),
            (1280, 720, 3200, 1080, 1920, 180)
        );
    }

    #[test]
    fn layout_parser_rejects_missing_or_invalid_dimensions() {
        let missing_width = json!([
            { "name": "hypr-rdp-1", "x": 0, "y": 0, "height": 720 }
        ]);
        let string_width = json!([
            { "name": "hypr-rdp-1", "x": 0, "y": 0, "width": "1280", "height": 720 }
        ]);

        assert!(layout_from_monitors(&missing_width, "hypr-rdp-1").is_err());
        assert!(layout_from_monitors(&string_width, "hypr-rdp-1").is_err());
    }

    #[test]
    fn layout_parser_rejects_zero_or_negative_dimensions() {
        let zero_width = json!([
            { "name": "hypr-rdp-1", "x": 0, "y": 0, "width": 0, "height": 720 }
        ]);
        let negative_height = json!([
            { "name": "hypr-rdp-1", "x": 0, "y": 0, "width": 1280, "height": -1 }
        ]);

        assert!(layout_from_monitors(&zero_width, "hypr-rdp-1").is_err());
        assert!(layout_from_monitors(&negative_height, "hypr-rdp-1").is_err());
    }
}
