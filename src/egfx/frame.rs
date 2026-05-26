use super::{
    avc444::{Avc444EncodedFrame, Avc444FrameEncoding},
    backend::FrameEncoder,
};

const AVC444_LOG_REGION_SAMPLE_LIMIT: usize = 4;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum EgfxFrameCodec {
    Avc420,
    Avc444,
}

pub(crate) enum EncodedEgfxFrame {
    Avc420(Vec<u8>),
    Avc444(Avc444EncodedFrame),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum EncodedFrameState {
    Sendable,
    Skipped,
    Invalid,
}

impl EncodedEgfxFrame {
    pub(crate) fn len(&self) -> usize {
        match self {
            Self::Avc420(data) => data.len(),
            Self::Avc444(frame) => frame.stream1.len() + frame.stream2.len(),
        }
    }

    pub(crate) fn state(&self) -> EncodedFrameState {
        match self {
            Self::Avc420(data) if data.is_empty() => EncodedFrameState::Skipped,
            Self::Avc420(data) if data.len() > 32 => EncodedFrameState::Sendable,
            Self::Avc420(_) => EncodedFrameState::Invalid,
            Self::Avc444(frame) => {
                let stream1_has_regions = !frame.stream1_regions.is_empty();
                let stream2_has_regions = !frame.stream2_regions.is_empty();
                let stream1_has_data = !frame.stream1.is_empty();
                let stream2_has_data = !frame.stream2.is_empty();

                if frame.stream1.is_empty()
                    && frame.stream2.is_empty()
                    && !stream1_has_regions
                    && !stream2_has_regions
                {
                    return EncodedFrameState::Skipped;
                }

                match frame.encoding {
                    Avc444FrameEncoding::Luma | Avc444FrameEncoding::Chroma
                        if stream1_has_regions && !stream2_has_regions && !stream2_has_data =>
                    {
                        if stream1_has_data {
                            EncodedFrameState::Sendable
                        } else {
                            EncodedFrameState::Skipped
                        }
                    }
                    Avc444FrameEncoding::LumaAndChroma
                        if stream1_has_regions && stream2_has_regions =>
                    {
                        if stream1_has_data && stream2_has_data {
                            EncodedFrameState::Sendable
                        } else {
                            EncodedFrameState::Skipped
                        }
                    }
                    _ => EncodedFrameState::Invalid,
                }
            }
        }
    }

    pub(crate) fn commit_after_send(&self, encoder: &mut FrameEncoder) {
        if matches!(self, Self::Avc444(_)) {
            encoder.commit_avc444_reference();
        }
    }

    pub(crate) fn log_sent_frame(
        &self,
        frame_id: u32,
        surface_id: u16,
        width: u32,
        height: u32,
        damage_regions: &[(i32, i32, i32, i32)],
    ) {
        let Self::Avc444(frame) = self else {
            return;
        };

        if !avc444_perf_logging_enabled() {
            return;
        }

        let stream1_nal_types = frame.stream1_nal_types();
        let stream2_nal_types = frame.stream2_nal_types();
        let damage_area_pct = region_area_pct(damage_regions, width, height);
        let stream1_area_pct = region_area_pct(&frame.stream1_regions, width, height);
        let stream2_area_pct = region_area_pct(&frame.stream2_regions, width, height);
        let damage_sample = sampled_regions(damage_regions);
        let stream1_sample = sampled_regions(&frame.stream1_regions);
        let stream2_sample = sampled_regions(&frame.stream2_regions);
        tracing::info!(
            target: "hypr_rdp::avc444_perf",
            frame_id,
            surface_id,
            width,
            height,
            pdu_order = "StartFrame,WireToSurface1,EndFrame",
            codec_id = "AVC444v2",
            encoding = ?frame.encoding,
            damage_regions = damage_regions.len(),
            damage_area_pct,
            damage_sample = %damage_sample,
            stream1_bytes = frame.stream1.len(),
            stream2_bytes = frame.stream2.len(),
            stream1_regions = frame.stream1_regions.len(),
            stream2_regions = frame.stream2_regions.len(),
            stream1_area_pct,
            stream2_area_pct,
            stream1_sample = %stream1_sample,
            stream2_sample = %stream2_sample,
            stream1_empty = frame.stream1.is_empty(),
            stream2_empty = frame.stream2.is_empty(),
            stream1_has_idr = frame.stream1_has_idr(),
            stream2_has_idr = frame.stream2_has_idr(),
            stream1_nal_types = ?stream1_nal_types,
            stream2_nal_types = ?stream2_nal_types,
            "AVC444v2 frame sent"
        );
    }
}

fn avc444_perf_logging_enabled() -> bool {
    avc444_perf_logging_enabled_with(|name| std::env::var_os(name).is_some())
}

fn avc444_perf_logging_enabled_with(mut is_set: impl FnMut(&str) -> bool) -> bool {
    is_set("HYPR_RDP_AVC444_PERF")
}

fn region_area_pct(regions: &[(i32, i32, i32, i32)], width: u32, height: u32) -> f64 {
    let frame_pixels = u64::from(width).saturating_mul(u64::from(height));
    if frame_pixels == 0 {
        return 0.0;
    }

    super::avc444::region_area_pixels(regions, width as usize, height as usize) as f64 * 100.0
        / frame_pixels as f64
}

fn sampled_regions(regions: &[(i32, i32, i32, i32)]) -> String {
    let mut sample = regions
        .iter()
        .take(AVC444_LOG_REGION_SAMPLE_LIMIT)
        .map(|&(x, y, w, h)| format!("{x},{y},{w},{h}"))
        .collect::<Vec<_>>()
        .join(";");
    if regions.len() > AVC444_LOG_REGION_SAMPLE_LIMIT {
        if !sample.is_empty() {
            sample.push(';');
        }
        sample.push_str("...");
    }
    sample
}

#[cfg(test)]
mod tests {
    use super::*;

    fn avc444_frame(
        encoding: Avc444FrameEncoding,
        stream1: Vec<u8>,
        stream2: Vec<u8>,
        stream1_regions: Vec<(i32, i32, i32, i32)>,
        stream2_regions: Vec<(i32, i32, i32, i32)>,
    ) -> EncodedEgfxFrame {
        EncodedEgfxFrame::Avc444(Avc444EncodedFrame {
            encoding,
            stream1,
            stream2,
            stream1_regions,
            stream2_regions,
        })
    }

    #[test]
    fn encoded_frame_state_treats_empty_output_as_encoder_skip() {
        assert_eq!(
            EncodedEgfxFrame::Avc420(Vec::new()).state(),
            EncodedFrameState::Skipped
        );
        assert_eq!(
            avc444_frame(
                Avc444FrameEncoding::Luma,
                Vec::new(),
                Vec::new(),
                Vec::new(),
                Vec::new()
            )
            .state(),
            EncodedFrameState::Skipped
        );
    }

    #[test]
    fn avc444_perf_logging_is_opt_in_for_wire_summary() {
        assert!(!avc444_perf_logging_enabled_with(|_| false));
        assert!(avc444_perf_logging_enabled_with(
            |name| name == "HYPR_RDP_AVC444_PERF"
        ));
    }

    #[test]
    fn avc444_log_region_summary_is_bounded_and_reports_area() {
        let regions = vec![
            (0, 0, 10, 10),
            (20, 20, 10, 10),
            (40, 40, 10, 10),
            (60, 60, 10, 10),
            (80, 80, 10, 10),
        ];

        assert_eq!(region_area_pct(&regions[..1], 100, 100), 1.0);
        assert_eq!(
            sampled_regions(&regions),
            "0,0,10,10;20,20,10,10;40,40,10,10;60,60,10,10;..."
        );
    }

    #[test]
    fn avc444_frame_summary_reports_nal_and_idr_shape() {
        let frame = Avc444EncodedFrame {
            encoding: Avc444FrameEncoding::LumaAndChroma,
            stream1: vec![0x00, 0x00, 0x01, 0x65, 0xaa],
            stream2: vec![0x00, 0x00, 0x01, 0x41, 0xbb],
            stream1_regions: vec![(0, 0, 16, 16)],
            stream2_regions: vec![(0, 0, 16, 16)],
        };

        assert_eq!(frame.stream1_nal_types(), vec![5]);
        assert_eq!(frame.stream2_nal_types(), vec![1]);
        assert!(frame.stream1_has_idr());
        assert!(!frame.stream2_has_idr());
    }

    #[test]
    fn encoded_frame_state_treats_empty_avc444_substream_payload_as_encoder_skip() {
        assert_eq!(
            avc444_frame(
                Avc444FrameEncoding::Luma,
                Vec::new(),
                Vec::new(),
                vec![(0, 0, 16, 16)],
                Vec::new()
            )
            .state(),
            EncodedFrameState::Skipped
        );
        assert_eq!(
            avc444_frame(
                Avc444FrameEncoding::LumaAndChroma,
                vec![0x55; 64],
                Vec::new(),
                vec![(0, 0, 16, 16)],
                vec![(16, 0, 16, 16)]
            )
            .state(),
            EncodedFrameState::Skipped
        );
    }

    #[test]
    fn encoded_frame_state_rejects_partial_avc444_payloads() {
        assert_eq!(
            avc444_frame(
                Avc444FrameEncoding::Luma,
                vec![0x55; 64],
                Vec::new(),
                Vec::new(),
                Vec::new()
            )
            .state(),
            EncodedFrameState::Invalid
        );
        assert_eq!(
            avc444_frame(
                Avc444FrameEncoding::LumaAndChroma,
                vec![0x55; 64],
                vec![0xaa; 64],
                vec![(0, 0, 16, 16)],
                Vec::new()
            )
            .state(),
            EncodedFrameState::Invalid
        );
        assert_eq!(
            avc444_frame(
                Avc444FrameEncoding::Luma,
                Vec::new(),
                vec![0x55; 64],
                vec![(0, 0, 16, 16)],
                Vec::new()
            )
            .state(),
            EncodedFrameState::Invalid
        );
    }
}
