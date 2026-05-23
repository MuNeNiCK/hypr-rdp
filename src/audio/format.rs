use ironrdp_rdpsnd::pdu::{AudioFormat, WaveFormat};

/// PCM format: 16-bit signed LE, stereo, 44100 Hz
pub(super) const SAMPLE_RATE: u32 = 44100;
pub(super) const CHANNELS: u16 = 2;
pub(super) const BITS_PER_SAMPLE: u16 = 16;
pub(super) const BLOCK_ALIGN: u16 = CHANNELS * (BITS_PER_SAMPLE / 8);

pub(super) fn advertised_format() -> AudioFormat {
    AudioFormat {
        format: WaveFormat::PCM,
        n_channels: CHANNELS,
        n_samples_per_sec: SAMPLE_RATE,
        n_avg_bytes_per_sec: SAMPLE_RATE * BLOCK_ALIGN as u32,
        n_block_align: BLOCK_ALIGN,
        bits_per_sample: BITS_PER_SAMPLE,
        data: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn advertised_audio_format_is_single_pcm_s16le_stereo_stream() {
        let format = advertised_format();

        assert_eq!(format.format, WaveFormat::PCM);
        assert_eq!(format.n_channels, CHANNELS);
        assert_eq!(format.n_samples_per_sec, SAMPLE_RATE);
        assert_eq!(format.n_avg_bytes_per_sec, SAMPLE_RATE * BLOCK_ALIGN as u32);
        assert_eq!(format.n_block_align, BLOCK_ALIGN);
        assert_eq!(format.bits_per_sample, BITS_PER_SAMPLE);
        assert!(format.data.is_none());
    }
}
