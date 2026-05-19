use std::sync::Mutex;

use anyhow::{bail, Context, Result};

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
        let x = m["x"].as_i64().unwrap_or(0);
        let y = m["y"].as_i64().unwrap_or(0);
        let w = m["width"].as_i64().unwrap_or(0);
        let h = m["height"].as_i64().unwrap_or(0);
        min_x = min_x.min(x);
        min_y = min_y.min(y);
        max_x = max_x.max(x + w);
        max_y = max_y.max(y + h);

        if m["name"].as_str() == Some(output_name) {
            target = Some((x, y, w, h));
        }
    }

    let (target_x, target_y, target_w, target_h) =
        target.context(format!("output '{}' not found", output_name))?;
    let layout_w = (max_x - min_x) as u32;
    let layout_h = (max_y - min_y) as u32;
    if layout_w == 0 || layout_h == 0 {
        bail!("invalid layout bounds: {}x{}", layout_w, layout_h);
    }
    let offset_x = (target_x - min_x) as u32;
    let offset_y = (target_y - min_y) as u32;

    Ok((
        target_w as u32,
        target_h as u32,
        layout_w,
        layout_h,
        offset_x,
        offset_y,
    ))
}
