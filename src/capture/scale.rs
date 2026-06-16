use std::borrow::Cow;

use anyhow::{bail, Context, Result};
use ironrdp_server::PixelFormat;

use crate::display::geometry::{PresentationGeometry, Size};
use crate::input::OutputLayoutSnapshot;

pub(super) struct PreparedPresentationFrame<'a> {
    pub(super) data: Cow<'a, [u8]>,
    pub(super) width: u32,
    pub(super) height: u32,
    pub(super) stride: u32,
    pub(super) damage_regions: Vec<(i32, i32, i32, i32)>,
    pub(super) geometry_generation: u32,
}

pub(super) struct PresentationGenerationAction {
    pub(super) refresh_processor: bool,
    pub(super) next_generation: u32,
    pub(super) damage_regions: Vec<(i32, i32, i32, i32)>,
}

pub(super) fn prepare_presentation_frame<'a>(
    source_data: &'a [u8],
    source_width: u32,
    source_height: u32,
    source_stride: u32,
    pixel_format: PixelFormat,
    source_damage_regions: &[(i32, i32, i32, i32)],
    snapshot: &OutputLayoutSnapshot,
) -> Result<PreparedPresentationFrame<'a>> {
    let source = Size::new(source_width, source_height).context("invalid source size")?;
    let presentation = snapshot.presentation_geometry.presentation();
    let geometry = if snapshot.presentation_geometry.source() == source {
        snapshot.presentation_geometry
    } else {
        PresentationGeometry::new(source, presentation)
    };
    let damage_regions = geometry.map_source_damage(source_damage_regions);

    if geometry.is_identity() {
        let min_len = source_stride
            .checked_mul(source_height)
            .and_then(|len| usize::try_from(len).ok())
            .context("source frame size overflow")?;
        if source_data.len() < min_len {
            bail!(
                "source frame too small: got {} bytes, need {}",
                source_data.len(),
                min_len
            );
        }
        return Ok(PreparedPresentationFrame {
            data: Cow::Borrowed(source_data),
            width: source_width,
            height: source_height,
            stride: source_stride,
            damage_regions,
            geometry_generation: snapshot.geometry_generation,
        });
    }

    let stride = presentation
        .width
        .checked_mul(4)
        .context("presentation stride overflow")?;
    let len = stride
        .checked_mul(presentation.height)
        .and_then(|len| usize::try_from(len).ok())
        .context("presentation frame size overflow")?;
    let mut output = vec![0; len];
    fill_black_bars(&mut output, pixel_format);
    copy_scaled_visible_rect(source_data, source_stride, &mut output, stride, geometry)?;

    Ok(PreparedPresentationFrame {
        data: Cow::Owned(output),
        width: presentation.width,
        height: presentation.height,
        stride,
        damage_regions,
        geometry_generation: snapshot.geometry_generation,
    })
}

pub(super) fn dmabuf_zero_copy_allowed(snapshot: &OutputLayoutSnapshot) -> bool {
    snapshot.presentation_geometry.is_identity()
}

pub(super) fn output_downscaling_generation_action(
    current_generation: u32,
    prepared: &PreparedPresentationFrame<'_>,
) -> PresentationGenerationAction {
    if prepared.geometry_generation != current_generation {
        PresentationGenerationAction {
            refresh_processor: true,
            next_generation: prepared.geometry_generation,
            damage_regions: vec![(0, 0, prepared.width as i32, prepared.height as i32)],
        }
    } else {
        PresentationGenerationAction {
            refresh_processor: false,
            next_generation: current_generation,
            damage_regions: prepared.damage_regions.clone(),
        }
    }
}

pub(super) fn presentation_frame_shape(
    source_width: u32,
    source_height: u32,
    source_stride: u32,
    snapshot: &OutputLayoutSnapshot,
) -> Result<(u32, u32, u32)> {
    let source = Size::new(source_width, source_height).context("invalid source size")?;
    let presentation = snapshot.presentation_geometry.presentation();
    let geometry = if snapshot.presentation_geometry.source() == source {
        snapshot.presentation_geometry
    } else {
        PresentationGeometry::new(source, presentation)
    };

    if geometry.is_identity() {
        Ok((source_width, source_height, source_stride))
    } else {
        let stride = presentation
            .width
            .checked_mul(4)
            .context("presentation stride overflow")?;
        Ok((presentation.width, presentation.height, stride))
    }
}

fn fill_black_bars(output: &mut [u8], pixel_format: PixelFormat) {
    for pixel in output.chunks_exact_mut(4) {
        pixel[0] = 0;
        pixel[1] = 0;
        pixel[2] = 0;
        pixel[3] = 0;
        match pixel_format {
            PixelFormat::ARgb32 | PixelFormat::ABgr32 => pixel[0] = 255,
            PixelFormat::BgrA32 | PixelFormat::RgbA32 => pixel[3] = 255,
            PixelFormat::XRgb32
            | PixelFormat::XBgr32
            | PixelFormat::BgrX32
            | PixelFormat::RgbX32 => {}
        }
    }
}

fn copy_scaled_visible_rect(
    source_data: &[u8],
    source_stride: u32,
    output: &mut [u8],
    output_stride: u32,
    geometry: PresentationGeometry,
) -> Result<()> {
    let source = geometry.source();
    let visible = geometry.visible_rect();

    let source_stride = usize::try_from(source_stride).context("source stride out of range")?;
    let output_stride = usize::try_from(output_stride).context("output stride out of range")?;
    let source_required = source_stride
        .checked_mul(source.height as usize)
        .context("source frame size overflow")?;
    if source_data.len() < source_required {
        bail!(
            "source frame too small: got {} bytes, need {}",
            source_data.len(),
            source_required
        );
    }

    for py in visible.y..visible.bottom() {
        let rel_y = py - visible.y;
        let sy = ((u64::from(rel_y) * u64::from(source.height)) / u64::from(visible.height))
            .min(u64::from(source.height - 1)) as usize;
        for px in visible.x..visible.right() {
            let rel_x = px - visible.x;
            let sx = ((u64::from(rel_x) * u64::from(source.width)) / u64::from(visible.width))
                .min(u64::from(source.width - 1)) as usize;

            let source_offset = sy
                .checked_mul(source_stride)
                .and_then(|row| row.checked_add(sx.checked_mul(4)?))
                .context("source pixel offset overflow")?;
            let output_offset = usize::try_from(py)
                .ok()
                .and_then(|row| row.checked_mul(output_stride))
                .and_then(|row| row.checked_add(usize::try_from(px).ok()?.checked_mul(4)?))
                .context("output pixel offset overflow")?;

            output[output_offset..output_offset + 4]
                .copy_from_slice(&source_data[source_offset..source_offset + 4]);
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::display::geometry::PresentationGeometry;

    fn snapshot(source: (u32, u32), presentation: (u32, u32)) -> OutputLayoutSnapshot {
        let source_size = Size::new(source.0, source.1).unwrap();
        let presentation_size = Size::new(presentation.0, presentation.1).unwrap();
        OutputLayoutSnapshot {
            output_name: "DP-1".into(),
            output_w: source.0,
            output_h: source.1,
            layout_extent_w: source.0,
            layout_extent_h: source.1,
            output_offset_x: 0,
            output_offset_y: 0,
            presentation_geometry: PresentationGeometry::new(source_size, presentation_size),
            geometry_generation: 7,
        }
    }

    fn frame(width: usize, height: usize, stride: usize) -> Vec<u8> {
        let mut data = vec![0; stride * height];
        for y in 0..height {
            for x in 0..width {
                let offset = y * stride + x * 4;
                data[offset] = x as u8;
                data[offset + 1] = y as u8;
                data[offset + 2] = 0x80;
                data[offset + 3] = 0xff;
            }
        }
        data
    }

    #[test]
    fn capture_scale_keeps_identity_frame_borrowed_and_damage_identity() {
        let source = frame(4, 2, 24);
        let prepared = prepare_presentation_frame(
            &source,
            4,
            2,
            24,
            PixelFormat::BgrA32,
            &[(1, 0, 2, 1)],
            &snapshot((4, 2), (4, 2)),
        )
        .expect("identity frame prepares");

        assert!(matches!(prepared.data, Cow::Borrowed(_)));
        assert_eq!(prepared.width, 4);
        assert_eq!(prepared.height, 2);
        assert_eq!(prepared.stride, 24);
        assert_eq!(prepared.damage_regions, vec![(1, 0, 2, 1)]);
        assert_eq!(prepared.geometry_generation, 7);
    }

    #[test]
    fn capture_scale_maps_source_frame_to_presentation_with_black_bars() {
        let source = frame(4, 2, 16);
        let prepared = prepare_presentation_frame(
            &source,
            4,
            2,
            16,
            PixelFormat::BgrA32,
            &[(0, 0, 4, 2)],
            &snapshot((4, 2), (4, 4)),
        )
        .expect("scaled frame prepares");

        assert!(matches!(prepared.data, Cow::Owned(_)));
        assert_eq!(prepared.width, 4);
        assert_eq!(prepared.height, 4);
        assert_eq!(prepared.stride, 16);
        assert_eq!(&prepared.data[0..4], &[0, 0, 0, 255]);
        assert_eq!(&prepared.data[16..20], &[0, 0, 0x80, 0xff]);
        assert_eq!(&prepared.data[48..52], &[0, 0, 0, 255]);
    }

    #[test]
    fn capture_scale_maps_damage_before_frame_processor() {
        let source = frame(4, 2, 16);
        let prepared = prepare_presentation_frame(
            &source,
            4,
            2,
            16,
            PixelFormat::BgrX32,
            &[(1, 0, 1, 1)],
            &snapshot((4, 2), (8, 4)),
        )
        .expect("scaled frame prepares");

        assert_eq!(prepared.damage_regions, vec![(2, 0, 2, 2)]);
    }

    #[test]
    fn dmabuf_scaled_output_guard_rejects_non_identity_geometry() {
        assert!(dmabuf_zero_copy_allowed(&snapshot(
            (1920, 1080),
            (1920, 1080)
        )));
        assert!(!dmabuf_zero_copy_allowed(&snapshot(
            (3840, 2160),
            (1920, 1080)
        )));
        assert!(!dmabuf_zero_copy_allowed(&snapshot(
            (1920, 1080),
            (1024, 768)
        )));
    }

    #[test]
    fn capture_scale_reports_presentation_frame_shape_before_frame_processor() {
        assert_eq!(
            presentation_frame_shape(3840, 2160, 3840 * 4, &snapshot((3840, 2160), (1920, 1080)))
                .expect("shape"),
            (1920, 1080, 1920 * 4)
        );
    }

    #[test]
    fn output_downscaling_generation_forces_full_redraw_and_discards_stale_mapped_damage() {
        let source = frame(4, 2, 16);
        let prepared = prepare_presentation_frame(
            &source,
            4,
            2,
            16,
            PixelFormat::BgrX32,
            &[(1, 0, 1, 1)],
            &snapshot((4, 2), (8, 4)),
        )
        .expect("scaled frame prepares");

        assert_eq!(prepared.damage_regions, vec![(2, 0, 2, 2)]);

        let action = output_downscaling_generation_action(6, &prepared);

        assert!(action.refresh_processor);
        assert_eq!(action.next_generation, 7);
        assert_eq!(action.damage_regions, vec![(0, 0, 8, 4)]);
    }

    #[test]
    fn output_downscaling_generation_reuses_current_generation_damage_without_incrementing() {
        let source = frame(4, 2, 16);
        let prepared = prepare_presentation_frame(
            &source,
            4,
            2,
            16,
            PixelFormat::BgrX32,
            &[(1, 0, 1, 1)],
            &snapshot((4, 2), (8, 4)),
        )
        .expect("scaled frame prepares");

        let action = output_downscaling_generation_action(7, &prepared);

        assert!(!action.refresh_processor);
        assert_eq!(action.next_generation, 7);
        assert_eq!(action.damage_regions, vec![(2, 0, 2, 2)]);
    }
}
