const DAMAGE_TILE_SIZE: i32 = 64;
const DAMAGE_MERGE_DISTANCE: i32 = 16;

/// Frame-diff fallback for compositors that report full-frame damage.
///
/// Treat compositor damage as the candidate area, then detect actual changed
/// regions before emitting RDPEGFX region metadata. The detector compares
/// against the last frame that was successfully sent to the client.
pub(super) struct FrameDiffDamageDetector {
    reference_frame: Option<Vec<u8>>,
    reference_stride: usize,
}

impl FrameDiffDamageDetector {
    pub(super) fn new() -> Self {
        Self {
            reference_frame: None,
            reference_stride: 0,
        }
    }

    pub(super) fn invalidate(&mut self) {
        self.reference_frame = None;
        self.reference_stride = 0;
    }

    pub(super) fn update_reference(&mut self, data: &[u8], height: u32, stride: usize) {
        let len = stride.saturating_mul(height as usize).min(data.len());
        self.reference_frame = Some(data[..len].to_vec());
        self.reference_stride = stride;
    }

    pub(super) fn update_reference_regions(
        &mut self,
        data: &[u8],
        width: u32,
        height: u32,
        stride: usize,
        regions: &[(i32, i32, i32, i32)],
    ) {
        let frame_len = stride.saturating_mul(height as usize);
        if self.reference_stride != stride
            || self
                .reference_frame
                .as_ref()
                .is_none_or(|frame| frame.len() < frame_len)
        {
            self.update_reference(data, height, stride);
            return;
        }

        let Some(reference) = self.reference_frame.as_mut() else {
            return;
        };

        for &(x, y, w, h) in regions {
            let Some((left, top, region_w, region_h)) =
                clamp_damage_region(x, y, w, h, width, height)
            else {
                continue;
            };

            let left = left as usize;
            let top = top as usize;
            let width_bytes = region_w as usize * 4;
            let region_h = region_h as usize;

            for row in 0..region_h {
                let start = (top + row).saturating_mul(stride).saturating_add(left * 4);
                let end = start.saturating_add(width_bytes);
                if end <= data.len() && end <= reference.len() {
                    reference[start..end].copy_from_slice(&data[start..end]);
                }
            }
        }
    }

    pub(super) fn detect(
        &self,
        data: &[u8],
        width: u32,
        height: u32,
        stride: usize,
        candidates: &[(i32, i32, i32, i32)],
    ) -> Vec<(i32, i32, i32, i32)> {
        let Some(reference) = &self.reference_frame else {
            return vec![(0, 0, width as i32, height as i32)];
        };

        let frame_len = stride.saturating_mul(height as usize);
        if self.reference_stride != stride || reference.len() < frame_len || data.len() < frame_len
        {
            return vec![(0, 0, width as i32, height as i32)];
        }

        let mut regions = Vec::new();
        for &(x, y, w, h) in candidates {
            let Some((left, top, cand_w, cand_h)) = clamp_damage_region(x, y, w, h, width, height)
            else {
                continue;
            };
            let right = left.saturating_add(cand_w);
            let bottom = top.saturating_add(cand_h);

            let mut tile_y = top;
            while tile_y < bottom {
                let tile_h = DAMAGE_TILE_SIZE.min(bottom - tile_y);
                let mut tile_x = left;
                while tile_x < right {
                    let tile_w = DAMAGE_TILE_SIZE.min(right - tile_x);
                    let tile = (tile_x, tile_y, tile_w, tile_h);
                    if frame_tile_changed(data, reference, stride, tile) {
                        merge_nearby_damage_region(&mut regions, tile, DAMAGE_MERGE_DISTANCE);
                    }
                    tile_x += DAMAGE_TILE_SIZE;
                }
                tile_y += DAMAGE_TILE_SIZE;
            }
        }

        regions
    }
}

pub(super) fn clamp_damage_region(
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    width: u32,
    height: u32,
) -> Option<(i32, i32, i32, i32)> {
    if w <= 0 || h <= 0 {
        return None;
    }

    let width = i32::try_from(width).ok()?;
    let height = i32::try_from(height).ok()?;
    let left = x.clamp(0, width);
    let top = y.clamp(0, height);
    let right = x.saturating_add(w).clamp(0, width);
    let bottom = y.saturating_add(h).clamp(0, height);

    if right <= left || bottom <= top {
        None
    } else {
        Some((left, top, right - left, bottom - top))
    }
}

pub(super) fn merge_damage_region(
    pending: &mut Vec<(i32, i32, i32, i32)>,
    region: (i32, i32, i32, i32),
) {
    if let Some((left, top, width, height)) = pending.first_mut() {
        let right = (*left)
            .saturating_add(*width)
            .max(region.0.saturating_add(region.2));
        let bottom = (*top)
            .saturating_add(*height)
            .max(region.1.saturating_add(region.3));
        *left = (*left).min(region.0);
        *top = (*top).min(region.1);
        *width = right - *left;
        *height = bottom - *top;
    } else {
        pending.push(region);
    }
}

fn merge_nearby_damage_region(
    regions: &mut Vec<(i32, i32, i32, i32)>,
    region: (i32, i32, i32, i32),
    merge_distance: i32,
) {
    let mut merged = region;
    let mut index = 0;
    while index < regions.len() {
        if damage_regions_are_near(regions[index], merged, merge_distance) {
            merged = union_damage_region(regions[index], merged);
            regions.swap_remove(index);
        } else {
            index += 1;
        }
    }
    regions.push(merged);
}

fn damage_regions_are_near(
    a: (i32, i32, i32, i32),
    b: (i32, i32, i32, i32),
    merge_distance: i32,
) -> bool {
    let a_right = a.0.saturating_add(a.2);
    let a_bottom = a.1.saturating_add(a.3);
    let b_right = b.0.saturating_add(b.2);
    let b_bottom = b.1.saturating_add(b.3);

    let gap_x = if b.0 >= a_right {
        b.0 - a_right
    } else {
        a.0.saturating_sub(b_right)
    };
    let gap_y = if b.1 >= a_bottom {
        b.1 - a_bottom
    } else {
        a.1.saturating_sub(b_bottom)
    };

    gap_x <= merge_distance && gap_y <= merge_distance
}

fn union_damage_region(a: (i32, i32, i32, i32), b: (i32, i32, i32, i32)) -> (i32, i32, i32, i32) {
    let left = a.0.min(b.0);
    let top = a.1.min(b.1);
    let right = a.0.saturating_add(a.2).max(b.0.saturating_add(b.2));
    let bottom = a.1.saturating_add(a.3).max(b.1.saturating_add(b.3));
    (left, top, right - left, bottom - top)
}

fn frame_tile_changed(
    current: &[u8],
    reference: &[u8],
    stride: usize,
    tile: (i32, i32, i32, i32),
) -> bool {
    let (x, y, width, height) = tile;
    if x < 0 || y < 0 || width <= 0 || height <= 0 {
        return false;
    }

    let x = x as usize;
    let y = y as usize;
    let width_bytes = width as usize * 4;
    let height = height as usize;

    for row in 0..height {
        let start = (y + row).saturating_mul(stride).saturating_add(x * 4);
        let end = start.saturating_add(width_bytes);
        if end > current.len() || end > reference.len() {
            return true;
        }
        if current[start..end] != reference[start..end] {
            return true;
        }
    }

    false
}

pub(super) fn damage_area_pixels(
    damage_regions: &[(i32, i32, i32, i32)],
    width: u32,
    height: u32,
) -> u64 {
    damage_regions
        .iter()
        .filter_map(|&(x, y, w, h)| {
            if w <= 0 || h <= 0 {
                return None;
            }

            let left = x.clamp(0, width as i32);
            let top = y.clamp(0, height as i32);
            let right = x.saturating_add(w).clamp(0, width as i32);
            let bottom = y.saturating_add(h).clamp(0, height as i32);

            if right <= left || bottom <= top {
                return None;
            }

            Some(
                u64::try_from(right - left).unwrap_or(0) * u64::try_from(bottom - top).unwrap_or(0),
            )
        })
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn damage_region_clamp_handles_extreme_coordinates_without_overflow() {
        assert_eq!(
            clamp_damage_region(i32::MAX - 1, 0, 100, 10, 1280, 720),
            None
        );
        assert_eq!(
            clamp_damage_region(i32::MIN + 1, 0, 100, 10, 1280, 720),
            None
        );
        assert_eq!(
            clamp_damage_region(i32::MAX - 1, i32::MAX - 1, i32::MAX, i32::MAX, 1280, 720),
            None
        );
    }

    #[test]
    fn damage_region_clamp_and_merge_keeps_pending_union() {
        let mut pending = Vec::new();
        let first = clamp_damage_region(-10, 5, 30, 10, 1280, 720).unwrap();
        let second = clamp_damage_region(100, 50, 20, 20, 1280, 720).unwrap();

        merge_damage_region(&mut pending, first);
        merge_damage_region(&mut pending, second);

        assert_eq!(pending, vec![(0, 5, 120, 65)]);
    }

    #[test]
    fn damage_area_is_clamped() {
        assert_eq!(
            damage_area_pixels(&[(-10, -10, 20, 20), (1270, 710, 20, 20)], 1280, 720),
            200
        );
    }

    #[test]
    fn damage_area_drops_empty_and_overflowed_rectangles() {
        assert_eq!(
            damage_area_pixels(
                &[
                    (10, 10, 0, 10),
                    (10, 10, 10, -1),
                    (i32::MAX - 1, 0, 100, 100),
                    (0, i32::MAX - 1, 100, 100),
                ],
                1280,
                720,
            ),
            0
        );
    }

    #[test]
    fn frame_diff_detector_returns_empty_for_identical_frame() {
        let width = 128;
        let height = 64;
        let stride = width * 4;
        let frame = vec![0x44; stride * height];
        let mut detector = FrameDiffDamageDetector::new();
        detector.update_reference(&frame, height as u32, stride);

        let regions = detector.detect(
            &frame,
            width as u32,
            height as u32,
            stride,
            &[(0, 0, width as i32, height as i32)],
        );

        assert!(regions.is_empty());
    }

    #[test]
    fn frame_diff_detector_limits_full_damage_to_changed_tile() {
        let width = 128;
        let height = 128;
        let stride = width * 4;
        let reference = vec![0; stride * height];
        let mut current = reference.clone();
        current[(70 * stride) + (70 * 4)] = 1;

        let mut detector = FrameDiffDamageDetector::new();
        detector.update_reference(&reference, height as u32, stride);
        let regions = detector.detect(
            &current,
            width as u32,
            height as u32,
            stride,
            &[(0, 0, width as i32, height as i32)],
        );

        assert_eq!(regions, vec![(64, 64, 64, 64)]);
    }

    #[test]
    fn frame_diff_detector_handles_padded_stride() {
        let width = 96;
        let height = 96;
        let stride = width * 4 + 16;
        let reference = vec![0; stride * height];
        let mut current = reference.clone();
        current[(70 * stride) + (70 * 4)] = 1;

        let mut detector = FrameDiffDamageDetector::new();
        detector.update_reference(&reference, height as u32, stride);
        let regions = detector.detect(
            &current,
            width as u32,
            height as u32,
            stride,
            &[(0, 0, width as i32, height as i32)],
        );

        assert_eq!(regions, vec![(64, 64, 32, 32)]);
    }

    #[test]
    fn reference_region_update_preserves_padding_and_unselected_pixels() {
        let width = 4;
        let height = 3;
        let stride = width * 4 + 8;
        let mut reference = vec![0x10; stride * height];
        let mut current = reference.clone();

        for row in 0..height {
            let padding = row * stride + width * 4;
            current[padding..padding + 8].fill(0x99);
        }
        for x in 1..3 {
            let offset = stride + x * 4;
            current[offset..offset + 4].copy_from_slice(&[0xaa, 0xbb, 0xcc, 0xdd]);
        }

        let mut detector = FrameDiffDamageDetector::new();
        detector.update_reference(&reference, height as u32, stride);
        detector.update_reference_regions(
            &current,
            width as u32,
            height as u32,
            stride,
            &[(1, 1, 2, 1)],
        );

        let updated = detector.reference_frame.as_ref().expect("reference exists");
        for x in 1..3 {
            let offset = stride + x * 4;
            reference[offset..offset + 4].copy_from_slice(&[0xaa, 0xbb, 0xcc, 0xdd]);
        }
        assert_eq!(updated, &reference);
    }

    #[test]
    fn reference_region_update_handles_offset_rows_and_adjacent_regions() {
        let width = 8;
        let height = 4;
        let stride = width * 4 + 12;
        let reference = vec![0x10; stride * height];
        let mut current = reference.clone();
        let mut expected = reference.clone();

        for row in 0..height {
            let padding = row * stride + width * 4;
            current[padding..padding + 12].fill(0x99);
        }
        for row in 1..3 {
            for x in 2..5 {
                let offset = row * stride + x * 4;
                let pixel = [x as u8, row as u8, 0xaa, 0xff];
                current[offset..offset + 4].copy_from_slice(&pixel);
                expected[offset..offset + 4].copy_from_slice(&pixel);
            }
        }

        let mut detector = FrameDiffDamageDetector::new();
        detector.update_reference(&reference, height as u32, stride);
        detector.update_reference_regions(
            &current,
            width as u32,
            height as u32,
            stride,
            &[(2, 1, 2, 2), (4, 1, 1, 2)],
        );

        let updated = detector.reference_frame.as_ref().expect("reference exists");
        assert_eq!(updated, &expected);
    }

    #[test]
    fn frame_diff_detector_merges_overlapping_candidate_regions_without_touching_padding() {
        let width = 96;
        let height = 80;
        let stride = width * 4 + 20;
        let reference = vec![0; stride * height];
        let mut current = reference.clone();
        for row in 20..25 {
            for x in 20..25 {
                current[row * stride + x * 4] = 1;
            }
        }
        for row in 0..height {
            let padding = row * stride + width * 4;
            current[padding..padding + 20].fill(0xff);
        }

        let mut detector = FrameDiffDamageDetector::new();
        detector.update_reference(&reference, height as u32, stride);
        let regions = detector.detect(
            &current,
            width as u32,
            height as u32,
            stride,
            &[(0, 0, 64, 64), (16, 16, 64, 64)],
        );

        assert_eq!(regions, vec![(0, 0, 80, 80)]);
    }

    #[test]
    fn frame_diff_detector_keeps_unsent_regions_dirty() {
        let width = 192;
        let height = 64;
        let stride = width * 4;
        let reference = vec![0; stride * height];
        let mut current = reference.clone();
        current[(10 * stride) + (10 * 4)] = 1;
        current[(10 * stride) + (150 * 4)] = 1;

        let mut detector = FrameDiffDamageDetector::new();
        detector.update_reference(&reference, height as u32, stride);
        let regions = detector.detect(
            &current,
            width as u32,
            height as u32,
            stride,
            &[(0, 0, width as i32, height as i32)],
        );
        assert_eq!(regions, vec![(0, 0, 64, 64), (128, 0, 64, 64)]);

        detector.update_reference_regions(
            &current,
            width as u32,
            height as u32,
            stride,
            &[(0, 0, 64, 64)],
        );
        let regions = detector.detect(
            &current,
            width as u32,
            height as u32,
            stride,
            &[(0, 0, width as i32, height as i32)],
        );

        assert_eq!(regions, vec![(128, 0, 64, 64)]);
    }
}
