use super::{
    backend::FrameEncoder,
    encoder::{Avc444EncodedFrame, Avc444FrameEncoding},
};

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
