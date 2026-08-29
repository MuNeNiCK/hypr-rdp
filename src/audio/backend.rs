use std::fmt;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc as std_mpsc;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use ironrdp_rdpsnd::pdu::AudioFormat;
use ironrdp_rdpsnd::server::{NegotiatedFormat, RdpsndError, RdpsndServerHandler};
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
                // pipewire::init() self-guards with a process-wide
                // OnceCell, but deinit() is documented as at most once per
                // process, after all PipeWire threads are done. Calling it
                // per session tore the library down with the init guard
                // still set, so every session after the first failed to
                // create a MainLoop. Never deinit.
                pipewire::init();

                if let Err(e) = run_capture(sender, Arc::clone(&stop_signal), Some(startup_tx)) {
                    tracing::error!("Audio: PipeWire capture error: {:#}", e);
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

#[derive(Debug)]
struct AudioStartError(String);

impl fmt::Display for AudioStartError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for AudioStartError {}

impl fmt::Debug for HyprSoundHandler {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HyprSoundHandler")
            .field("capturing", &self.stop_signal.is_some())
            .finish()
    }
}

impl HyprSoundHandler {
    fn start_capture(&mut self) -> Result<(), AudioStartError> {
        let Some(ref sender) = self.event_sender else {
            return Err(AudioStartError("no event sender".into()));
        };

        let active_routing = self
            .routing_runner
            .start(self.audio_mode)
            .map_err(|e| AudioStartError(format!("failed to configure audio routing: {e:#}")))?;

        let stop_signal = Arc::new(AtomicBool::new(false));
        let (startup_tx, startup_rx) = std_mpsc::channel();

        let handle = self
            .capture_runner
            .spawn(sender.clone(), Arc::clone(&stop_signal), startup_tx)
            .map_err(|e| AudioStartError(format!("failed to spawn capture thread: {e}")))?;

        match startup_rx.recv_timeout(AUDIO_STARTUP_TIMEOUT) {
            Ok(Ok(())) => {
                self.stop_signal = Some(stop_signal);
                self.capture_thread = Some(handle);
            }
            Ok(Err(e)) => {
                let _ = handle.join();
                return Err(AudioStartError(format!("failed to start PipeWire: {e}")));
            }
            Err(std_mpsc::RecvTimeoutError::Timeout) => {
                stop_signal.store(true, Ordering::SeqCst);
                self.stop_signal = Some(stop_signal);
                self.capture_thread = Some(handle);
                return Err(AudioStartError(format!(
                    "timed out waiting for PipeWire startup after {} ms",
                    AUDIO_STARTUP_TIMEOUT.as_millis()
                )));
            }
            Err(std_mpsc::RecvTimeoutError::Disconnected) => {
                let _ = handle.join();
                return Err(AudioStartError(
                    "capture thread exited before reporting startup".into(),
                ));
            }
        }

        self.active_routing = active_routing;
        Ok(())
    }
}

impl RdpsndServerHandler for HyprSoundHandler {
    fn get_formats(&self) -> &[AudioFormat] {
        &self.formats
    }

    fn choose_format<'a>(
        &mut self,
        common: &'a [NegotiatedFormat],
    ) -> Option<&'a NegotiatedFormat> {
        common.first()
    }

    fn start(&mut self, format: &NegotiatedFormat) -> Result<(), Box<dyn RdpsndError>> {
        tracing::trace!(
            negotiated_format = ?format.format(),
            "Audio: starting capture ({}Hz, {}ch, {}bit)",
            SAMPLE_RATE,
            CHANNELS,
            BITS_PER_SAMPLE
        );

        self.start_capture()
            .map_err(|e| Box::new(e) as Box<dyn RdpsndError>)?;
        tracing::trace!("Audio: PipeWire capture started");
        Ok(())
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

#[cfg(test)]
mod tests {
    use ironrdp_server::{ServerEvent, SoundServerFactory};
    use tokio::sync::mpsc;

    use super::*;

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

    /// A capture thread that does not notice the stop signal promptly -- what a
    /// PipeWire loop blocked inside a library call looks like from here.
    struct SlowToStopRunner {
        delay: Duration,
    }

    impl AudioCaptureRunner for SlowToStopRunner {
        fn spawn(
            &self,
            _sender: mpsc::UnboundedSender<ServerEvent>,
            _stop_signal: Arc<AtomicBool>,
            startup_tx: std_mpsc::Sender<AudioStartupStatus>,
        ) -> io::Result<thread::JoinHandle<()>> {
            let delay = self.delay;
            Ok(thread::spawn(move || {
                let _ = startup_tx.send(Ok(()));
                thread::sleep(delay);
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

    /// Issue #66: the server wedges with the accept backlog full and CPU at 0%.
    ///
    /// `stop()` is reached from `Drop`, and the drop happens inside
    /// `RdpServer::run`'s accept loop -- the pinned IronRDP clears the static
    /// channel set between `run_connection` returning and the next `accept()`.
    /// Everything `stop()` does is therefore on the loop's own thread, and the
    /// loop runs on a `LocalSet`, so nothing else in the server progresses
    /// meanwhile.
    ///
    /// Startup is already bounded by `AUDIO_STARTUP_TIMEOUT`. Teardown is not
    /// bounded at all: `handle.join()` waits for the capture thread however
    /// long it takes, and the routing guard then runs several `pactl`
    /// subprocesses through a blocking `Command::output()` with no timeout
    /// either. A capture thread that stops noticing the flag -- or a `pactl`
    /// that never returns -- wedges the listener while the process stays alive,
    /// which is why `Restart=always` does not recover it.
    #[test]
    fn stopping_capture_does_not_wait_on_the_capture_thread_forever() {
        let (event_tx, _event_rx) = mpsc::unbounded_channel();
        let mut handler = handler_with_runner(
            Some(event_tx),
            Arc::new(SlowToStopRunner {
                delay: Duration::from_secs(2),
            }),
        );
        handler.start_capture().expect("capture starts");

        let started = std::time::Instant::now();
        handler.stop();
        let elapsed = started.elapsed();

        assert!(
            elapsed < Duration::from_millis(500),
            "stop() blocked the accept loop for {elapsed:?}; teardown has no deadline"
        );
    }

    #[test]
    fn start_capture_rejects_when_event_sender_is_missing() {
        let mut handler = handler_with_runner(None, Arc::new(PanicRunner));

        assert!(handler.start_capture().is_err());
        assert!(handler.stop_signal.is_none());
        assert!(handler.capture_thread.is_none());
    }

    #[test]
    fn start_capture_accepts_after_capture_runner_reports_ready() {
        let (sender, _receiver) = mpsc::unbounded_channel::<ServerEvent>();
        let mut handler = handler_with_runner(Some(sender), Arc::new(ReadyRunner));

        assert!(handler.start_capture().is_ok());
        assert!(handler.stop_signal.is_some());
        assert!(handler.capture_thread.is_some());

        handler.stop();
        assert!(handler.stop_signal.is_none());
        assert!(handler.capture_thread.is_none());
    }

    #[test]
    fn start_capture_rejects_when_capture_startup_fails() {
        let (sender, _receiver) = mpsc::unbounded_channel::<ServerEvent>();
        let mut handler = handler_with_runner(Some(sender), Arc::new(FailingStartupRunner));
        assert!(handler.start_capture().is_err());
        assert!(handler.stop_signal.is_none());
        assert!(handler.capture_thread.is_none());
    }

    #[test]
    fn start_capture_rejects_when_capture_spawn_fails() {
        let (sender, _receiver) = mpsc::unbounded_channel::<ServerEvent>();
        let mut handler = handler_with_runner(Some(sender), Arc::new(SpawnErrorRunner));
        assert!(handler.start_capture().is_err());
        assert!(handler.stop_signal.is_none());
        assert!(handler.capture_thread.is_none());
    }

    #[test]
    fn start_capture_accepts_redirect_mode_after_routing_and_capture_start() {
        let (sender, _receiver) = mpsc::unbounded_channel::<ServerEvent>();
        let mut handler = handler_with_runner_and_routing(
            Some(sender),
            Arc::new(ReadyRunner),
            Arc::new(ReadyRoutingRunner),
            AudioMode::Redirect,
        );
        assert!(handler.start_capture().is_ok());
        assert!(handler.active_routing.is_some());

        handler.stop();
        assert!(handler.active_routing.is_none());
    }

    #[test]
    fn start_capture_rejects_redirect_mode_when_routing_fails_before_capture_spawn() {
        let (sender, _receiver) = mpsc::unbounded_channel::<ServerEvent>();
        let mut handler = handler_with_runner_and_routing(
            Some(sender),
            Arc::new(PanicRunner),
            Arc::new(FailingRoutingRunner),
            AudioMode::Redirect,
        );
        assert!(handler.start_capture().is_err());
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
