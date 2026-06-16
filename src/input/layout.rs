use std::sync::Mutex;

use anyhow::{bail, Context, Result};
use serde_json::Value;

use crate::display::geometry::{PresentationGeometry, Size};

#[derive(Clone, Debug)]
pub(crate) struct OutputLayoutSnapshot {
    pub(crate) output_name: String,
    pub(crate) output_w: u32,
    pub(crate) output_h: u32,
    pub(crate) layout_extent_w: u32,
    pub(crate) layout_extent_h: u32,
    pub(crate) output_offset_x: u32,
    pub(crate) output_offset_y: u32,
    pub(crate) presentation_geometry: PresentationGeometry,
    pub(crate) geometry_generation: u32,
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
        self.update_snapshot(
            output_name,
            output_w,
            output_h,
            layout_extent_w,
            layout_extent_h,
            output_offset_x,
            output_offset_y,
            (output_w, output_h),
        )
    }

    pub fn update_from_output_with_presentation(
        &self,
        output_name: &str,
        presentation: (u32, u32),
    ) -> Result<()> {
        let (
            output_w,
            output_h,
            layout_extent_w,
            layout_extent_h,
            output_offset_x,
            output_offset_y,
        ) = query_layout(output_name)?;
        self.update_snapshot(
            output_name,
            output_w,
            output_h,
            layout_extent_w,
            layout_extent_h,
            output_offset_x,
            output_offset_y,
            presentation,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn update_snapshot(
        &self,
        output_name: &str,
        output_w: u32,
        output_h: u32,
        layout_extent_w: u32,
        layout_extent_h: u32,
        output_offset_x: u32,
        output_offset_y: u32,
        presentation: (u32, u32),
    ) -> Result<()> {
        let source = Size::new(output_w, output_h).context("output has invalid source size")?;
        let presentation = Size::new(presentation.0, presentation.1)
            .context("output has invalid presentation size")?;
        let presentation_geometry = PresentationGeometry::new(source, presentation);
        let geometry_generation = self
            .inner
            .lock()
            .ok()
            .and_then(|guard| {
                guard.as_ref().map(|old| {
                    if old.presentation_geometry == presentation_geometry {
                        old.geometry_generation
                    } else {
                        old.geometry_generation.saturating_add(1)
                    }
                })
            })
            .unwrap_or(0);
        let snapshot = OutputLayoutSnapshot {
            output_name: output_name.to_string(),
            output_w,
            output_h,
            layout_extent_w,
            layout_extent_h,
            output_offset_x,
            output_offset_y,
            presentation_geometry,
            geometry_generation,
        };
        tracing::info!(
            output = %snapshot.output_name,
            source_w = snapshot.output_w,
            source_h = snapshot.output_h,
            presentation_w = snapshot.presentation_geometry.presentation().width,
            presentation_h = snapshot.presentation_geometry.presentation().height,
            geometry_generation = snapshot.geometry_generation,
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

    pub(crate) fn snapshot(&self) -> Option<OutputLayoutSnapshot> {
        self.inner.lock().ok()?.clone()
    }

    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn update_snapshot_for_test(
        &self,
        output_name: &str,
        output_w: u32,
        output_h: u32,
        layout_extent_w: u32,
        layout_extent_h: u32,
        output_offset_x: u32,
        output_offset_y: u32,
        presentation: (u32, u32),
    ) -> Result<()> {
        self.update_snapshot(
            output_name,
            output_w,
            output_h,
            layout_extent_w,
            layout_extent_h,
            output_offset_x,
            output_offset_y,
            presentation,
        )
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

    #[test]
    fn output_layout_generation_advances_on_presentation_or_source_geometry_change() {
        let layout = SharedOutputLayout::new();

        layout
            .update_snapshot_for_test("DP-1", 3840, 2160, 3840, 2160, 0, 0, (3840, 2160))
            .expect("initial snapshot");
        assert_eq!(layout.snapshot().unwrap().geometry_generation, 0);

        layout
            .update_snapshot_for_test("DP-1", 3840, 2160, 3840, 2160, 0, 0, (3840, 2160))
            .expect("same snapshot");
        assert_eq!(layout.snapshot().unwrap().geometry_generation, 0);

        layout
            .update_snapshot_for_test("DP-1", 3840, 2160, 3840, 2160, 0, 0, (1920, 1080))
            .expect("presentation resize");
        assert_eq!(layout.snapshot().unwrap().geometry_generation, 1);

        layout
            .update_snapshot_for_test("DP-1", 2560, 1440, 2560, 1440, 0, 0, (1920, 1080))
            .expect("source resize");
        assert_eq!(layout.snapshot().unwrap().geometry_generation, 2);
    }
}
