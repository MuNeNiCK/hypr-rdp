use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;

use ironrdp_rdpsnd::pdu::AudioFormat;
use ironrdp_rdpsnd::server::RdpsndServerHandler;
use ironrdp_server::{ServerEvent, ServerEventSender, SoundServerFactory};
use tokio::sync::mpsc;

use super::format::{advertised_format, BITS_PER_SAMPLE, CHANNELS, SAMPLE_RATE};
use super::pipewire::run_capture;

pub struct HyprSoundFactory {
    event_sender: Option<mpsc::UnboundedSender<ServerEvent>>,
}

impl HyprSoundFactory {
    pub fn new() -> Self {
        Self { event_sender: None }
    }
}

impl ServerEventSender for HyprSoundFactory {
    fn set_sender(&mut self, sender: mpsc::UnboundedSender<ServerEvent>) {
        self.event_sender = Some(sender);
    }
}

impl SoundServerFactory for HyprSoundFactory {
    fn build_backend(&self) -> Box<dyn RdpsndServerHandler> {
        Box::new(HyprSoundHandler {
            event_sender: self.event_sender.clone(),
            stop_signal: None,
            capture_thread: None,
            formats: vec![advertised_format()],
        })
    }
}

struct HyprSoundHandler {
    event_sender: Option<mpsc::UnboundedSender<ServerEvent>>,
    stop_signal: Option<Arc<AtomicBool>>,
    capture_thread: Option<thread::JoinHandle<()>>,
    formats: Vec<AudioFormat>,
}

impl fmt::Debug for HyprSoundHandler {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HyprSoundHandler")
            .field("capturing", &self.stop_signal.is_some())
            .finish()
    }
}

impl RdpsndServerHandler for HyprSoundHandler {
    fn get_formats(&self) -> &[AudioFormat] {
        &self.formats
    }

    fn start(&mut self, client_format: &ironrdp_rdpsnd::pdu::ClientAudioFormatPdu) -> Option<u16> {
        tracing::trace!(
            client_formats = client_format.formats.len(),
            "Audio: starting capture ({}Hz, {}ch, {}bit)",
            SAMPLE_RATE,
            CHANNELS,
            BITS_PER_SAMPLE
        );

        let client_format_index = match matching_client_format_index(&self.formats, client_format) {
            Some(idx) => idx as u16,
            None => {
                tracing::warn!("Audio: client does not support our PCM format, audio disabled");
                return None;
            }
        };

        let Some(ref sender) = self.event_sender else {
            tracing::warn!("Audio: no event sender, audio disabled");
            return None;
        };

        let stop_signal = Arc::new(AtomicBool::new(false));
        self.stop_signal = Some(Arc::clone(&stop_signal));

        let sender = sender.clone();

        match thread::Builder::new()
            .name("pipewire-audio".into())
            .spawn(move || {
                pipewire::init();

                if let Err(e) = run_capture(sender, Arc::clone(&stop_signal)) {
                    tracing::error!("Audio: PipeWire capture error: {:#}", e);
                }

                // SAFETY: Called once per init(), after all PipeWire resources are dropped
                unsafe {
                    pipewire::deinit();
                }
            }) {
            Ok(handle) => {
                self.capture_thread = Some(handle);
            }
            Err(e) => {
                tracing::error!("Audio: failed to spawn capture thread: {}", e);
                self.stop_signal = None;
                return None;
            }
        }

        tracing::trace!(client_format_index, "Audio: PipeWire capture started");
        Some(client_format_index)
    }

    fn stop(&mut self) {
        tracing::trace!("Audio: stopping capture");

        if let Some(stop) = self.stop_signal.take() {
            stop.store(true, Ordering::SeqCst);
        }

        if let Some(handle) = self.capture_thread.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for HyprSoundHandler {
    fn drop(&mut self) {
        self.stop();
    }
}

fn matching_client_format_index(
    server_formats: &[AudioFormat],
    client_format: &ironrdp_rdpsnd::pdu::ClientAudioFormatPdu,
) -> Option<usize> {
    let our_format = server_formats.first()?;
    client_format.formats.iter().position(|f| {
        f.format == our_format.format
            && f.n_channels == our_format.n_channels
            && f.n_samples_per_sec == our_format.n_samples_per_sec
            && f.bits_per_sample == our_format.bits_per_sample
    })
}

#[cfg(test)]
mod tests {
    use ironrdp_rdpsnd::pdu::{AudioFormatFlags, ClientAudioFormatPdu, Version, WaveFormat};
    use ironrdp_server::{ServerEvent, SoundServerFactory};
    use tokio::sync::mpsc;

    use super::*;
    use crate::audio::format::BLOCK_ALIGN;

    fn client_formats(formats: Vec<AudioFormat>) -> ClientAudioFormatPdu {
        ClientAudioFormatPdu {
            version: Version::V8,
            flags: AudioFormatFlags::ALIVE,
            volume_left: 0xffff,
            volume_right: 0xffff,
            pitch: 0,
            dgram_port: 0,
            formats,
        }
    }

    fn pcm_format(sample_rate: u32, channels: u16, bits_per_sample: u16) -> AudioFormat {
        AudioFormat {
            format: WaveFormat::PCM,
            n_channels: channels,
            n_samples_per_sec: sample_rate,
            n_avg_bytes_per_sec: sample_rate * u32::from(BLOCK_ALIGN),
            n_block_align: BLOCK_ALIGN,
            bits_per_sample,
            data: None,
        }
    }

    #[test]
    fn matching_client_format_index_selects_first_exact_pcm_match() {
        let server_formats = vec![advertised_format()];
        let client_format = client_formats(vec![
            pcm_format(SAMPLE_RATE, CHANNELS, 8),
            pcm_format(SAMPLE_RATE, CHANNELS, BITS_PER_SAMPLE),
            pcm_format(48000, CHANNELS, BITS_PER_SAMPLE),
        ]);

        assert_eq!(
            matching_client_format_index(&server_formats, &client_format),
            Some(1)
        );
    }

    #[test]
    fn matching_client_format_index_rejects_missing_or_mismatched_format() {
        let server_formats = vec![advertised_format()];

        assert_eq!(
            matching_client_format_index(&server_formats, &client_formats(Vec::new())),
            None
        );
        assert_eq!(
            matching_client_format_index(
                &server_formats,
                &client_formats(vec![pcm_format(48000, CHANNELS, BITS_PER_SAMPLE)])
            ),
            None
        );
    }

    #[test]
    fn start_rejects_matching_format_when_event_sender_is_missing() {
        let mut handler = HyprSoundHandler {
            event_sender: None,
            stop_signal: None,
            capture_thread: None,
            formats: vec![advertised_format()],
        };
        let client_format = client_formats(vec![advertised_format()]);

        assert_eq!(handler.start(&client_format), None);
        assert!(handler.stop_signal.is_none());
        assert!(handler.capture_thread.is_none());
    }

    #[test]
    fn start_rejects_unsupported_client_format_before_capture_spawn() {
        let (sender, _receiver) = mpsc::unbounded_channel::<ServerEvent>();
        let mut handler = HyprSoundHandler {
            event_sender: Some(sender),
            stop_signal: None,
            capture_thread: None,
            formats: vec![advertised_format()],
        };
        let client_format = client_formats(vec![pcm_format(48000, CHANNELS, BITS_PER_SAMPLE)]);

        assert_eq!(handler.start(&client_format), None);
        assert!(handler.stop_signal.is_none());
        assert!(handler.capture_thread.is_none());
    }

    #[test]
    fn sound_factory_backend_advertises_the_local_audio_format() {
        let handler = HyprSoundFactory::new().build_backend();

        assert_eq!(handler.get_formats(), &[advertised_format()]);
    }
}
