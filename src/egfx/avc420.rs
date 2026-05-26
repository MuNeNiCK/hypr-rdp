use ironrdp_egfx::pdu::Avc420Region;
use ironrdp_server::{GfxServerHandle, ServerEvent};
use tokio::sync::mpsc;

use super::EgfxShared;

impl EgfxShared {
    #[allow(clippy::too_many_arguments)]
    pub fn send_tracked_avc420_frame(
        &self,
        handle: &GfxServerHandle,
        sender: &mpsc::UnboundedSender<ServerEvent>,
        surface_id: u16,
        width: u16,
        height: u16,
        h264_data: &[u8],
        timestamp_ms: u32,
        quality: u8,
    ) -> bool {
        let regions = [avc420_full_frame_region(width, height, quality)];
        self.send_tracked_avc420_frame_with_regions(
            handle,
            sender,
            surface_id,
            h264_data,
            &regions,
            timestamp_ms,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn send_tracked_avc420_frame_with_damage(
        &self,
        handle: &GfxServerHandle,
        sender: &mpsc::UnboundedSender<ServerEvent>,
        surface_id: u16,
        width: u16,
        height: u16,
        h264_data: &[u8],
        damage_regions: &[(i32, i32, i32, i32)],
        timestamp_ms: u32,
        quality: u8,
    ) -> bool {
        let regions = damage_regions_to_avc420(damage_regions, width, height, quality);
        if regions.is_empty() {
            self.send_tracked_avc420_frame(
                handle,
                sender,
                surface_id,
                width,
                height,
                h264_data,
                timestamp_ms,
                quality,
            )
        } else {
            self.send_tracked_avc420_frame_with_regions(
                handle,
                sender,
                surface_id,
                h264_data,
                &regions,
                timestamp_ms,
            )
        }
    }

    pub(crate) fn send_tracked_avc420_frame_with_regions(
        &self,
        handle: &GfxServerHandle,
        sender: &mpsc::UnboundedSender<ServerEvent>,
        surface_id: u16,
        h264_data: &[u8],
        regions: &[Avc420Region],
        timestamp_ms: u32,
    ) -> bool {
        if regions.is_empty() {
            tracing::trace!("send_avc420_frame_with_regions: no regions");
            return false;
        }

        self.send_tracked_rdpegfx_frame(handle, sender, "AVC420", "send_avc420_frame", |server| {
            server
                .send_avc420_frame(surface_id, h264_data, regions, timestamp_ms)
                .or_else(|| {
                    tracing::trace!("send_avc420_frame: send_avc420_frame returned None");
                    None
                })
        })
    }

    #[cfg(test)]
    pub(crate) fn send_avc420_frame_with_regions(
        handle: &GfxServerHandle,
        sender: &mpsc::UnboundedSender<ServerEvent>,
        surface_id: u16,
        h264_data: &[u8],
        regions: &[Avc420Region],
        timestamp_ms: u32,
    ) -> bool {
        if sender.is_closed() {
            tracing::trace!("send_avc420_frame: EGFX event channel already closed");
            return false;
        }

        if regions.is_empty() {
            tracing::trace!("send_avc420_frame_with_regions: no regions");
            return false;
        }

        let Some(queued) = Self::queue_avc420_frame_with_regions(
            handle,
            surface_id,
            h264_data,
            regions,
            timestamp_ms,
        ) else {
            return false;
        };

        Self::send_rdpegfx_dvc_messages(sender, queued, "AVC420", "send_avc420_frame")
    }

    #[cfg(test)]
    fn queue_avc420_frame_with_regions(
        handle: &GfxServerHandle,
        surface_id: u16,
        h264_data: &[u8],
        regions: &[Avc420Region],
        timestamp_ms: u32,
    ) -> Option<super::rdpegfx::QueuedRdpegfxFrame> {
        if regions.is_empty() {
            tracing::trace!("send_avc420_frame_with_regions: no regions");
            return None;
        }

        Self::queue_rdpegfx_frame(handle, "send_avc420_frame", |server| {
            server
                .send_avc420_frame(surface_id, h264_data, regions, timestamp_ms)
                .or_else(|| {
                    tracing::trace!("send_avc420_frame: send_avc420_frame returned None");
                    None
                })
        })
    }
}

pub(crate) fn avc420_region_quality(qp: u8) -> u8 {
    100u8.saturating_sub(qp & 0x3f)
}

pub(crate) fn avc420_full_frame_region(width: u16, height: u16, qp: u8) -> Avc420Region {
    Avc420Region::new(0, 0, width, height, qp, avc420_region_quality(qp))
}

pub(crate) fn damage_regions_to_avc420(
    damage_regions: &[(i32, i32, i32, i32)],
    width: u16,
    height: u16,
    qp: u8,
) -> Vec<Avc420Region> {
    damage_regions
        .iter()
        .filter_map(|&(x, y, w, h)| {
            if w <= 0 || h <= 0 {
                return None;
            }

            let left = x.clamp(0, i32::from(width)) as u16;
            let top = y.clamp(0, i32::from(height)) as u16;
            let right = x.saturating_add(w).clamp(0, i32::from(width)) as u16;
            let bottom = y.saturating_add(h).clamp(0, i32::from(height)) as u16;

            if right <= left || bottom <= top {
                return None;
            }

            Some(Avc420Region::new(
                left,
                top,
                right,
                bottom,
                qp,
                avc420_region_quality(qp),
            ))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn avc420_damage_regions_are_clamped_and_exclusive() {
        let regions =
            damage_regions_to_avc420(&[(-10, 5, 30, 10), (1270, 710, 20, 20)], 1280, 720, 23);

        assert_eq!(regions.len(), 2);
        assert_eq!(
            (
                regions[0].left,
                regions[0].top,
                regions[0].right,
                regions[0].bottom,
                regions[0].quantization_parameter,
                regions[0].quality
            ),
            (0, 5, 20, 15, 23, 77)
        );
        assert_eq!(
            (
                regions[1].left,
                regions[1].top,
                regions[1].right,
                regions[1].bottom
            ),
            (1270, 710, 1280, 720)
        );
    }

    #[test]
    fn avc420_damage_regions_drop_empty_after_clamp() {
        let regions = damage_regions_to_avc420(
            &[(10, 10, 0, 5), (2000, 10, 5, 5), (10, 2000, 5, 5)],
            1280,
            720,
            23,
        );

        assert!(regions.is_empty());
    }

    #[test]
    fn avc420_damage_regions_clamp_full_frame_and_preserve_high_qp_metadata() {
        let regions = damage_regions_to_avc420(&[(-64, -32, 4096, 2048)], 1280, 720, 63);

        assert_eq!(regions.len(), 1);
        assert_eq!(
            (
                regions[0].left,
                regions[0].top,
                regions[0].right,
                regions[0].bottom
            ),
            (0, 0, 1280, 720)
        );
        assert_eq!(regions[0].quantization_parameter, 63);
        assert_eq!(regions[0].quality, 37);
    }

    #[test]
    fn avc420_damage_regions_preserve_touching_and_disjoint_rectangles() {
        let regions =
            damage_regions_to_avc420(&[(0, 0, 10, 10), (10, 0, 5, 10), (30, 2, 4, 6)], 64, 64, 23);

        assert_eq!(regions.len(), 3);
        assert_eq!(
            (
                regions[0].left,
                regions[0].top,
                regions[0].right,
                regions[0].bottom
            ),
            (0, 0, 10, 10)
        );
        assert_eq!(
            (
                regions[1].left,
                regions[1].top,
                regions[1].right,
                regions[1].bottom
            ),
            (10, 0, 15, 10)
        );
        assert_eq!(
            (
                regions[2].left,
                regions[2].top,
                regions[2].right,
                regions[2].bottom
            ),
            (30, 2, 34, 8)
        );
    }

    proptest! {
        #[test]
        fn generated_avc420_damage_regions_stay_inside_exclusive_bounds(
            damage_regions in proptest::collection::vec(
                (any::<i32>(), any::<i32>(), any::<i32>(), any::<i32>()),
                0..32
            ),
            width in 1u16..=4096,
            height in 1u16..=4096,
            qp in any::<u8>(),
        ) {
            let regions = damage_regions_to_avc420(&damage_regions, width, height, qp);

            for region in regions {
                prop_assert!(region.left < region.right);
                prop_assert!(region.top < region.bottom);
                prop_assert!(region.right <= width);
                prop_assert!(region.bottom <= height);
                prop_assert_eq!(region.quantization_parameter, qp);
                prop_assert_eq!(region.quality, avc420_region_quality(qp));
            }
        }
    }
}
