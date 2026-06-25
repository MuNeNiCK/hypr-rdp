use std::fmt;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc as std_mpsc;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use ironrdp_rdpsnd::pdu::AudioFormat;
use ironrdp_rdpsnd::server::RdpsndServerHandler;
use ironrdp_server::{ServerEvent, ServerEventSender, SoundServerFactory};
use tokio::sync::mpsc;

use super::format::{advertised_format, BITS_PER_SAMPLE, CHANNELS, SAMPLE_RATE};
use super::pipewire::run_capture;
use super::routing::{ActiveAudioRouting, AudioMode, AudioRoutingRunner, PipeWireRoutingRunner};

const AUDIO_STARTUP_TIMEOUT: Duration = Duration::from_secs(2);
type AudioStartupStatus = Result<(), String>;

trait AudioCaptureRunner: Send + Sync {
    fn spawn(
        &self,
        sender: mpsc::UnboundedSender<ServerEvent>,
        stop_signal: Arc<AtomicBool>,
        startup_tx: std_mpsc::Sender<AudioStartupStatus>,
    ) -> io::Result<thread::JoinHandle<()>>;
}

struct PipeWireCaptureRunner;

impl AudioCaptureRunner for PipeWireCaptureRunner {
    fn spawn(
        &self,
        sender: mpsc::UnboundedSender<ServerEvent>,
        stop_signal: Arc<AtomicBool>,
        startup_tx: std_mpsc::Sender<AudioStartupStatus>,
    ) -> io::Result<thread::JoinHandle<()>> {
        thread::Builder::new()
            .name("pipewire-audio".into())
            .spawn(move || {
                pipewire::init();

                if let Err(e) = run_capture(sender, Arc::clone(&stop_signal), Some(startup_tx)) {
                    tracing::error!("Audio: PipeWire capture error: {:#}", e);
                }

                // SAFETY: Called once per init(), after all PipeWire resources are dropped
                unsafe {
                    pipewire::deinit();
                }
            })
    }
}

pub struct HyprSoundFactory {
    event_sender: Option<mpsc::UnboundedSender<ServerEvent>>,
    audio_mode: AudioMode,
}

impl HyprSoundFactory {
    pub fn new(audio_mode: AudioMode) -> Self {
        Self {
            event_sender: None,
            audio_mode,
        }
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
            capture_runner: Arc::new(PipeWireCaptureRunner),
            routing_runner: Arc::new(PipeWireRoutingRunner::new()),
            active_routing: None,
            formats: vec![advertised_format()],
            audio_mode: self.audio_mode,
        })
    }
}

struct HyprSoundHandler {
    event_sender: Option<mpsc::UnboundedSender<ServerEvent>>,
    stop_signal: Option<Arc<AtomicBool>>,
    capture_thread: Option<thread::JoinHandle<()>>,
    capture_runner: Arc<dyn AudioCaptureRunner>,
    routing_runner: Arc<dyn AudioRoutingRunner>,
    active_routing: Option<Box<dyn ActiveAudioRouting>>,
    formats: Vec<AudioFormat>,
    audio_mode: AudioMode,
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

        let active_routing = match self.routing_runner.start(self.audio_mode) {
            Ok(active_routing) => active_routing,
            Err(e) => {
                tracing::error!("Audio: failed to configure audio routing: {:#}", e);
                return None;
            }
        };

        let stop_signal = Arc::new(AtomicBool::new(false));
        let (startup_tx, startup_rx) = std_mpsc::channel();

        let handle =
            match self
                .capture_runner
                .spawn(sender.clone(), Arc::clone(&stop_signal), startup_tx)
            {
                Ok(handle) => handle,
                Err(e) => {
                    tracing::error!("Audio: failed to spawn capture thread: {}", e);
                    drop(active_routing);
                    return None;
                }
            };

        match startup_rx.recv_timeout(AUDIO_STARTUP_TIMEOUT) {
            Ok(Ok(())) => {
                self.stop_signal = Some(stop_signal);
                self.capture_thread = Some(handle);
            }
            Ok(Err(e)) => {
                tracing::error!("Audio: PipeWire startup failed: {}", e);
                let _ = handle.join();
                drop(active_routing);
                return None;
            }
            Err(std_mpsc::RecvTimeoutError::Timeout) => {
                tracing::error!(
                    timeout_ms = AUDIO_STARTUP_TIMEOUT.as_millis(),
                    "Audio: timed out waiting for PipeWire startup"
                );
                stop_signal.store(true, Ordering::SeqCst);
                self.stop_signal = Some(stop_signal);
                self.capture_thread = Some(handle);
                drop(active_routing);
                return None;
            }
            Err(std_mpsc::RecvTimeoutError::Disconnected) => {
                tracing::error!("Audio: capture thread exited before reporting startup");
                let _ = handle.join();
                drop(active_routing);
                return None;
            }
        }

        self.active_routing = active_routing;
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

        self.active_routing.take();
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

    struct NoopRoutingGuard;

    impl ActiveAudioRouting for NoopRoutingGuard {}

    struct NoopRoutingRunner;

    impl AudioRoutingRunner for NoopRoutingRunner {
        fn start(&self, _mode: AudioMode) -> anyhow::Result<Option<Box<dyn ActiveAudioRouting>>> {
            Ok(None)
        }
    }

    struct ReadyRoutingRunner;

    impl AudioRoutingRunner for ReadyRoutingRunner {
        fn start(&self, mode: AudioMode) -> anyhow::Result<Option<Box<dyn ActiveAudioRouting>>> {
            Ok((mode == AudioMode::Redirect)
                .then(|| Box::new(NoopRoutingGuard) as Box<dyn ActiveAudioRouting>))
        }
    }

    struct FailingRoutingRunner;

    impl AudioRoutingRunner for FailingRoutingRunner {
        fn start(&self, _mode: AudioMode) -> anyhow::Result<Option<Box<dyn ActiveAudioRouting>>> {
            anyhow::bail!("routing unavailable")
        }
    }

    struct PanicRunner;

    impl AudioCaptureRunner for PanicRunner {
        fn spawn(
            &self,
            _sender: mpsc::UnboundedSender<ServerEvent>,
            _stop_signal: Arc<AtomicBool>,
            _startup_tx: std_mpsc::Sender<AudioStartupStatus>,
        ) -> io::Result<thread::JoinHandle<()>> {
            panic!("capture runner should not be called")
        }
    }

    struct ReadyRunner;

    impl AudioCaptureRunner for ReadyRunner {
        fn spawn(
            &self,
            _sender: mpsc::UnboundedSender<ServerEvent>,
            stop_signal: Arc<AtomicBool>,
            startup_tx: std_mpsc::Sender<AudioStartupStatus>,
        ) -> io::Result<thread::JoinHandle<()>> {
            Ok(thread::spawn(move || {
                let _ = startup_tx.send(Ok(()));
                while !stop_signal.load(Ordering::SeqCst) {
                    thread::sleep(Duration::from_millis(1));
                }
            }))
        }
    }

    struct FailingStartupRunner;

    impl AudioCaptureRunner for FailingStartupRunner {
        fn spawn(
            &self,
            _sender: mpsc::UnboundedSender<ServerEvent>,
            _stop_signal: Arc<AtomicBool>,
            startup_tx: std_mpsc::Sender<AudioStartupStatus>,
        ) -> io::Result<thread::JoinHandle<()>> {
            Ok(thread::spawn(move || {
                let _ = startup_tx.send(Err("PipeWire unavailable".to_string()));
            }))
        }
    }

    struct SpawnErrorRunner;

    impl AudioCaptureRunner for SpawnErrorRunner {
        fn spawn(
            &self,
            _sender: mpsc::UnboundedSender<ServerEvent>,
            _stop_signal: Arc<AtomicBool>,
            _startup_tx: std_mpsc::Sender<AudioStartupStatus>,
        ) -> io::Result<thread::JoinHandle<()>> {
            Err(io::Error::other("spawn failed"))
        }
    }

    fn handler_with_runner(
        event_sender: Option<mpsc::UnboundedSender<ServerEvent>>,
        capture_runner: Arc<dyn AudioCaptureRunner>,
    ) -> HyprSoundHandler {
        handler_with_runner_and_routing(
            event_sender,
            capture_runner,
            Arc::new(NoopRoutingRunner),
            AudioMode::Mirror,
        )
    }

    fn handler_with_runner_and_routing(
        event_sender: Option<mpsc::UnboundedSender<ServerEvent>>,
        capture_runner: Arc<dyn AudioCaptureRunner>,
        routing_runner: Arc<dyn AudioRoutingRunner>,
        audio_mode: AudioMode,
    ) -> HyprSoundHandler {
        HyprSoundHandler {
            event_sender,
            stop_signal: None,
            capture_thread: None,
            capture_runner,
            routing_runner,
            active_routing: None,
            formats: vec![advertised_format()],
            audio_mode,
        }
    }

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
        let mut handler = handler_with_runner(None, Arc::new(PanicRunner));
        let client_format = client_formats(vec![advertised_format()]);

        assert_eq!(handler.start(&client_format), None);
        assert!(handler.stop_signal.is_none());
        assert!(handler.capture_thread.is_none());
    }

    #[test]
    fn start_rejects_unsupported_client_format_before_capture_spawn() {
        let (sender, _receiver) = mpsc::unbounded_channel::<ServerEvent>();
        let mut handler = handler_with_runner(Some(sender), Arc::new(PanicRunner));
        let client_format = client_formats(vec![pcm_format(48000, CHANNELS, BITS_PER_SAMPLE)]);

        assert_eq!(handler.start(&client_format), None);
        assert!(handler.stop_signal.is_none());
        assert!(handler.capture_thread.is_none());
    }

    #[test]
    fn start_accepts_matching_format_after_capture_runner_reports_ready() {
        let (sender, _receiver) = mpsc::unbounded_channel::<ServerEvent>();
        let mut handler = handler_with_runner(Some(sender), Arc::new(ReadyRunner));
        let client_format = client_formats(vec![advertised_format()]);

        assert_eq!(handler.start(&client_format), Some(0));
        assert!(handler.stop_signal.is_some());
        assert!(handler.capture_thread.is_some());

        handler.stop();
        assert!(handler.stop_signal.is_none());
        assert!(handler.capture_thread.is_none());
    }

    #[test]
    fn start_rejects_matching_format_when_capture_startup_fails() {
        let (sender, _receiver) = mpsc::unbounded_channel::<ServerEvent>();
        let mut handler = handler_with_runner(Some(sender), Arc::new(FailingStartupRunner));
        let client_format = client_formats(vec![advertised_format()]);

        assert_eq!(handler.start(&client_format), None);
        assert!(handler.stop_signal.is_none());
        assert!(handler.capture_thread.is_none());
    }

    #[test]
    fn start_rejects_matching_format_when_capture_spawn_fails() {
        let (sender, _receiver) = mpsc::unbounded_channel::<ServerEvent>();
        let mut handler = handler_with_runner(Some(sender), Arc::new(SpawnErrorRunner));
        let client_format = client_formats(vec![advertised_format()]);

        assert_eq!(handler.start(&client_format), None);
        assert!(handler.stop_signal.is_none());
        assert!(handler.capture_thread.is_none());
    }

    #[test]
    fn start_accepts_redirect_mode_after_routing_and_capture_start() {
        let (sender, _receiver) = mpsc::unbounded_channel::<ServerEvent>();
        let mut handler = handler_with_runner_and_routing(
            Some(sender),
            Arc::new(ReadyRunner),
            Arc::new(ReadyRoutingRunner),
            AudioMode::Redirect,
        );
        let client_format = client_formats(vec![advertised_format()]);

        assert_eq!(handler.start(&client_format), Some(0));
        assert!(handler.active_routing.is_some());

        handler.stop();
        assert!(handler.active_routing.is_none());
    }

    #[test]
    fn start_rejects_redirect_mode_when_routing_fails_before_capture_spawn() {
        let (sender, _receiver) = mpsc::unbounded_channel::<ServerEvent>();
        let mut handler = handler_with_runner_and_routing(
            Some(sender),
            Arc::new(PanicRunner),
            Arc::new(FailingRoutingRunner),
            AudioMode::Redirect,
        );
        let client_format = client_formats(vec![advertised_format()]);

        assert_eq!(handler.start(&client_format), None);
        assert!(handler.stop_signal.is_none());
        assert!(handler.capture_thread.is_none());
        assert!(handler.active_routing.is_none());
    }

    #[test]
    fn sound_factory_backend_advertises_the_local_audio_format() {
        let handler = HyprSoundFactory::new(AudioMode::Mirror).build_backend();

        assert_eq!(handler.get_formats(), &[advertised_format()]);
    }
}
