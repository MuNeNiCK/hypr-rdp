use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use ironrdp_server::{GfxServerHandle, ServerEvent};
use tokio::sync::mpsc;

pub const DEFAULT_MAX_FRAMES_IN_FLIGHT: u32 = 3;

/// How long a client is given to open the graphics pipeline before frames go
/// out as bitmaps instead.
///
/// Opening `rdpgfx` is the client's move and there is no message for "I am not
/// going to", so "not ready yet" and "not ever" are the same state from here.
/// A client that never opens it -- FreeRDP 2.x asking for 32-bit colour, among
/// others -- would otherwise be sent nothing at all for the whole session, and
/// paint its own empty viewport: white under GTK, black under X11.
///
/// The wait is worth having: bitmaps mixed into the activation sequence are
/// worse than a moment of nothing. It is not worth having without an end.
/// Two seconds is long against the two round trips a DVC open costs even on a
/// slow link, and short against a person deciding the screen is broken.
pub const GFX_READY_GRACE: Duration = Duration::from_secs(2);
const SUSPEND_FRAME_ACK_QUEUE_DEPTH: u32 = u32::MAX;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum EgfxCodecPolicy {
    #[default]
    Auto,
    Avc420,
    Avc444,
}

pub(in crate::egfx) fn avc444_disabled_by_env() -> bool {
    std::env::var_os("HYPR_RDP_DISABLE_AVC444").is_some()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum EgfxFrameReadiness {
    Ready,
    LocalBackpressure {
        in_flight: u32,
        max: u32,
        client_queue_depth: u32,
        ack_suspended: bool,
    },
    TransportUnavailable,
    TransportNotReady,
    TransportNoChannel,
    TransportBackpressure {
        in_flight: u32,
        client_queue_depth: u32,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct EgfxFrameFlowSnapshot {
    pub(crate) last_queued_frame_id: u32,
    pub(crate) last_acked_frame_id: u32,
    pub(crate) frames_in_flight: u32,
    pub(crate) client_queue_depth: u32,
    pub(crate) frame_ack_suspended: bool,
    pub(crate) frame_ack_stream_established: bool,
    pub(crate) total_queued_frames: u64,
    pub(crate) total_acked_frames: u64,
}

impl EgfxFrameReadiness {
    pub(crate) fn is_ready(self) -> bool {
        matches!(self, Self::Ready)
    }

    pub(crate) fn reason(self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::LocalBackpressure { .. } => "local_backpressure",
            Self::TransportUnavailable => "transport_unavailable",
            Self::TransportNotReady => "transport_not_ready",
            Self::TransportNoChannel => "transport_no_channel",
            Self::TransportBackpressure { .. } => "transport_backpressure",
        }
    }
}

/// Shared EGFX state accessible from factory, handler, and capture thread.
pub struct EgfxShared {
    /// The GFX server handle (set once during build_server_with_handle)
    pub(in crate::egfx) handle: Mutex<Option<GfxServerHandle>>,
    /// Whether EGFX capability negotiation is complete
    pub(in crate::egfx) ready: AtomicBool,
    /// Whether the negotiated capability supports AVC420/AVC444 (H.264)
    pub(in crate::egfx) avc_enabled: AtomicBool,
    /// Whether the negotiated capability supports AVC444.
    pub(in crate::egfx) avc444_enabled: AtomicBool,
    /// Incremented each time on_ready fires; lets capture thread detect re-negotiation
    pub(in crate::egfx) ready_generation: AtomicU32,
    /// Event sender for routing encoded frames to the RDP wire
    pub(in crate::egfx) event_sender: Mutex<Option<mpsc::UnboundedSender<ServerEvent>>>,
    /// Surface size for auto-create in EGFX negotiation
    surface_size: Mutex<(u16, u16)>,
    current_surface: Mutex<Option<(u16, u16, u16)>>,
    /// Flow-control window for latency/throughput tuning.
    max_frames_in_flight: AtomicU32,
    frames_in_flight: AtomicU32,
    client_queue_depth: AtomicU32,
    frame_ack_suspended: AtomicBool,
    frame_ack_stream_established: AtomicBool,
    last_queued_frame_id: AtomicU32,
    last_acked_frame_id: AtomicU32,
    ack_cutoff_frame_id: AtomicU32,
    ack_cutoff_active: AtomicBool,
    total_queued_frames: AtomicU64,
    total_acked_frames: AtomicU64,
    force_full_frame: AtomicBool,
    /// When this connection first wanted to send a frame while the channel was
    /// not ready.
    ///
    /// Here rather than in the frame processor because the capture loop is torn
    /// down and rebuilt on every resize: a client that has already shown it will
    /// not open the channel must not buy a fresh grace period each time it
    /// drags a window.
    gfx_wait_started: Mutex<Option<Instant>>,
    /// Whether the channel has ever been ready on this connection. Once it has,
    /// there is no activation sequence left to protect, so a later loss of
    /// readiness falls back at once instead of freezing the screen again.
    gfx_ever_ready: AtomicBool,
    /// Whether giving up has already been logged, so a client that never opens
    /// the channel is told about once per connection and not once per frame.
    gfx_fallback_logged: AtomicBool,
    codec_policy: EgfxCodecPolicy,
}

impl EgfxShared {
    pub fn with_codec_policy(max_frames_in_flight: u32, codec_policy: EgfxCodecPolicy) -> Self {
        Self {
            handle: Mutex::new(None),
            ready: AtomicBool::new(false),
            avc_enabled: AtomicBool::new(false),
            avc444_enabled: AtomicBool::new(false),
            ready_generation: AtomicU32::new(0),
            event_sender: Mutex::new(None),
            surface_size: Mutex::new((0, 0)),
            current_surface: Mutex::new(None),
            max_frames_in_flight: AtomicU32::new(max_frames_in_flight.max(1)),
            frames_in_flight: AtomicU32::new(0),
            client_queue_depth: AtomicU32::new(0),
            frame_ack_suspended: AtomicBool::new(false),
            frame_ack_stream_established: AtomicBool::new(false),
            last_queued_frame_id: AtomicU32::new(0),
            last_acked_frame_id: AtomicU32::new(0),
            ack_cutoff_frame_id: AtomicU32::new(0),
            ack_cutoff_active: AtomicBool::new(false),
            total_queued_frames: AtomicU64::new(0),
            total_acked_frames: AtomicU64::new(0),
            force_full_frame: AtomicBool::new(false),
            gfx_wait_started: Mutex::new(None),
            gfx_ever_ready: AtomicBool::new(false),
            gfx_fallback_logged: AtomicBool::new(false),
            codec_policy,
        }
    }

    pub fn set_surface_size(&self, width: u16, height: u16) {
        if let Ok(mut guard) = self.surface_size.lock() {
            *guard = (width, height);
        }
    }

    pub fn get_surface_size(&self) -> (u16, u16) {
        self.surface_size.lock().map(|g| *g).unwrap_or((0, 0))
    }

    pub(in crate::egfx) fn current_surface_id(&self, width: u16, height: u16) -> Option<u16> {
        let (surface_id, surface_width, surface_height) =
            self.current_surface.lock().ok()?.as_ref().copied()?;
        (surface_width == width && surface_height == height).then_some(surface_id)
    }

    pub(in crate::egfx) fn set_current_surface(&self, surface_id: u16, width: u16, height: u16) {
        if let Ok(mut guard) = self.current_surface.lock() {
            *guard = Some((surface_id, width, height));
        }
    }

    pub(in crate::egfx) fn clear_current_surface(&self) {
        if let Ok(mut guard) = self.current_surface.lock() {
            *guard = None;
        }
    }

    pub(in crate::egfx) fn max_frames_in_flight(&self) -> u32 {
        self.max_frames_in_flight.load(Ordering::Acquire).max(1)
    }

    pub(in crate::egfx) fn codec_policy(&self) -> EgfxCodecPolicy {
        self.codec_policy
    }

    pub fn is_avc_enabled(&self) -> bool {
        self.avc_enabled.load(Ordering::Acquire)
    }

    pub fn is_avc444_enabled(&self) -> bool {
        self.avc444_enabled.load(Ordering::Acquire) && !avc444_disabled_by_env()
    }

    pub fn is_ready(&self) -> bool {
        self.ready.load(Ordering::Acquire)
    }

    /// Drop readiness the way a channel close does, for tests that need the
    /// transition without a live DVC.
    #[cfg(test)]
    pub(crate) fn mark_not_ready_for_test(&self) {
        self.ready.store(false, Ordering::Release);
    }

    /// Remember, for the rest of this connection, that the client does speak
    /// the graphics pipeline.
    ///
    /// This ends the wait on its own: `bitmap_fallback_due` never looks at the
    /// clock again once this is set, so clearing the clock here as well would
    /// be a store no behaviour depends on.
    pub(in crate::egfx) fn note_gfx_ready(&self) {
        self.gfx_ever_ready.store(true, Ordering::Release);
    }

    /// Whether a frame should go out as a bitmap even though the graphics
    /// pipeline is configured. Only asked when the channel is not ready.
    ///
    /// The first caller starts the clock. Nothing is sent until the grace runs
    /// out, and then everything is -- except for a channel that has already
    /// been ready once, where there is no activation sequence left to protect
    /// and waiting again would only freeze the screen.
    pub fn bitmap_fallback_due(&self, now: Instant) -> bool {
        if self.gfx_ever_ready.load(Ordering::Acquire) {
            return true;
        }

        let Ok(mut started) = self.gfx_wait_started.lock() else {
            // The clock is unreadable, so the wait cannot be bounded. Sending
            // is the failure that a user can see past.
            return true;
        };
        let waited = now.saturating_duration_since(*started.get_or_insert(now));
        if waited < GFX_READY_GRACE {
            return false;
        }
        drop(started);

        if !self.gfx_fallback_logged.swap(true, Ordering::AcqRel) {
            tracing::warn!(
                waited_ms = waited.as_millis(),
                "The client has not opened the graphics pipeline; sending bitmap updates instead"
            );
        }
        true
    }

    pub fn generation(&self) -> u32 {
        self.ready_generation.load(Ordering::Acquire)
    }

    pub(crate) fn request_full_frame(&self) {
        self.force_full_frame.store(true, Ordering::Release);
    }

    pub(crate) fn full_frame_requested(&self) -> bool {
        self.force_full_frame.load(Ordering::Acquire)
    }

    pub(crate) fn take_full_frame_request(&self) -> bool {
        self.force_full_frame.swap(false, Ordering::AcqRel)
    }

    pub(in crate::egfx) fn frames_in_flight(&self) -> u32 {
        self.frames_in_flight.load(Ordering::Acquire)
    }

    pub(in crate::egfx) fn client_queue_depth(&self) -> u32 {
        self.client_queue_depth.load(Ordering::Acquire)
    }

    pub(in crate::egfx) fn frame_ack_suspended(&self) -> bool {
        self.frame_ack_suspended.load(Ordering::Acquire)
    }

    pub(crate) fn preferred_frame_rate(&self, target_fps: u32) -> u32 {
        let target_fps = target_fps.max(1);
        if self.frame_ack_suspended() {
            return target_fps;
        }

        let in_flight = self.frames_in_flight();
        if in_flight <= 1 {
            return target_fps;
        }

        let backpressure_percent = 100 / in_flight.saturating_add(1);
        target_fps
            .saturating_mul(backpressure_percent)
            .saturating_div(100)
            .max(1)
    }

    #[cfg(test)]
    pub(in crate::egfx) fn frame_ack_stream_established(&self) -> bool {
        self.frame_ack_stream_established.load(Ordering::Acquire)
    }

    pub(in crate::egfx) fn should_backpressure_frames(&self) -> bool {
        !self.frame_ack_suspended() && self.frames_in_flight() >= self.max_frames_in_flight()
    }

    pub(crate) fn full_frame_refresh_readiness(&self) -> EgfxFrameReadiness {
        let in_flight = self.frames_in_flight();
        if self.frame_ack_suspended() || in_flight == 0 {
            return EgfxFrameReadiness::Ready;
        }

        EgfxFrameReadiness::LocalBackpressure {
            in_flight,
            max: 1,
            client_queue_depth: self.client_queue_depth(),
            ack_suspended: false,
        }
    }

    pub(in crate::egfx) fn record_frame_queued(&self, frame_id: u32) {
        self.last_queued_frame_id.store(frame_id, Ordering::Release);
        let in_flight = self.frames_in_flight.fetch_add(1, Ordering::AcqRel) + 1;
        let total_queued = self.total_queued_frames.fetch_add(1, Ordering::AcqRel) + 1;
        tracing::trace!(
            frame_id,
            in_flight,
            total_queued,
            client_queue_depth = self.client_queue_depth(),
            ack_suspended = self.frame_ack_suspended(),
            "EGFX: frame queued"
        );
    }

    pub(in crate::egfx) fn record_frame_ack(&self, frame_id: u32, queue_depth: u32) {
        self.client_queue_depth
            .store(queue_depth, Ordering::Release);
        let ack_suspended = queue_depth == SUSPEND_FRAME_ACK_QUEUE_DEPTH;
        self.frame_ack_suspended
            .store(ack_suspended, Ordering::Release);

        if self.ack_cutoff_active.load(Ordering::Acquire)
            && frame_id <= self.ack_cutoff_frame_id.load(Ordering::Acquire)
        {
            tracing::trace!(
                frame_id,
                cutoff = self.ack_cutoff_frame_id.load(Ordering::Acquire),
                "EGFX: ignoring stale frame ack"
            );
            return;
        }

        self.last_acked_frame_id.store(frame_id, Ordering::Release);
        if ack_suspended {
            let queued = self.total_queued_frames.load(Ordering::Acquire);
            self.total_acked_frames.store(queued, Ordering::Release);
            self.frames_in_flight.store(0, Ordering::Release);
            self.frame_ack_stream_established
                .store(false, Ordering::Release);
            return;
        }

        self.frame_ack_stream_established
            .store(true, Ordering::Release);
        let mut current = self.frames_in_flight.load(Ordering::Acquire);
        while current > 0 {
            match self.frames_in_flight.compare_exchange_weak(
                current,
                current - 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    self.total_acked_frames.fetch_add(1, Ordering::AcqRel);
                    return;
                }
                Err(next) => current = next,
            }
        }
    }

    pub(in crate::egfx) fn clear_frame_queue(&self) {
        let queued = self.total_queued_frames.load(Ordering::Acquire);
        let acked = self.total_acked_frames.load(Ordering::Acquire);
        if queued > acked {
            let last = self.last_queued_frame_id.load(Ordering::Acquire);
            self.ack_cutoff_frame_id.store(last, Ordering::Release);
            self.ack_cutoff_active.store(true, Ordering::Release);
            self.total_acked_frames.store(queued, Ordering::Release);
        }

        self.frames_in_flight.store(0, Ordering::Release);
        self.client_queue_depth.store(0, Ordering::Release);
        self.frame_ack_suspended.store(false, Ordering::Release);
    }

    pub(in crate::egfx) fn reset_frame_queue_for_new_client(&self) {
        self.frames_in_flight.store(0, Ordering::Release);
        self.client_queue_depth.store(0, Ordering::Release);
        self.frame_ack_suspended.store(false, Ordering::Release);
        self.frame_ack_stream_established
            .store(false, Ordering::Release);
        self.last_queued_frame_id.store(0, Ordering::Release);
        self.last_acked_frame_id.store(0, Ordering::Release);
        self.ack_cutoff_frame_id.store(0, Ordering::Release);
        self.ack_cutoff_active.store(false, Ordering::Release);
        self.total_queued_frames.store(0, Ordering::Release);
        self.total_acked_frames.store(0, Ordering::Release);
    }

    pub(crate) fn frame_flow_snapshot(&self) -> EgfxFrameFlowSnapshot {
        EgfxFrameFlowSnapshot {
            last_queued_frame_id: self.last_queued_frame_id.load(Ordering::Acquire),
            last_acked_frame_id: self.last_acked_frame_id.load(Ordering::Acquire),
            frames_in_flight: self.frames_in_flight.load(Ordering::Acquire),
            client_queue_depth: self.client_queue_depth.load(Ordering::Acquire),
            frame_ack_suspended: self.frame_ack_suspended.load(Ordering::Acquire),
            frame_ack_stream_established: self.frame_ack_stream_established.load(Ordering::Acquire),
            total_queued_frames: self.total_queued_frames.load(Ordering::Acquire),
            total_acked_frames: self.total_acked_frames.load(Ordering::Acquire),
        }
    }

    pub fn get_handle(&self) -> Option<GfxServerHandle> {
        self.handle.lock().ok()?.clone()
    }

    pub fn get_event_sender(&self) -> Option<mpsc::UnboundedSender<ServerEvent>> {
        self.event_sender.lock().ok()?.clone()
    }

    /// Reset readiness state for a new EGFX server instance.
    /// Called when the factory builds a per-client graphics pipeline server.
    /// The handle and event_sender are preserved (set per-connection by the factory).
    pub fn reset_for_new_client(&self) {
        self.ready.store(false, Ordering::Release);
        self.gfx_ever_ready.store(false, Ordering::Release);
        self.gfx_fallback_logged.store(false, Ordering::Release);
        if let Ok(mut started) = self.gfx_wait_started.lock() {
            *started = None;
        }
        self.avc_enabled.store(false, Ordering::Release);
        self.avc444_enabled.store(false, Ordering::Release);
        self.force_full_frame.store(false, Ordering::Release);
        self.clear_current_surface();
        self.reset_frame_queue_for_new_client();
    }
}

#[cfg(test)]
mod tests {
    use super::{EgfxShared, DEFAULT_MAX_FRAMES_IN_FLIGHT, GFX_READY_GRACE};
    use std::time::{Duration, Instant};

    fn shared() -> EgfxShared {
        EgfxShared::with_codec_policy(DEFAULT_MAX_FRAMES_IN_FLIGHT, super::EgfxCodecPolicy::Auto)
    }

    /// The size of the grace is part of the fix, not a tuning knob, and it is
    /// bounded on both sides. Too long and a person sits in front of a screen
    /// that shows nothing; too short and it fires before the channel open it
    /// exists to wait for -- two round trips -- can possibly have landed, which
    /// puts bitmaps back into the activation sequence.
    ///
    /// Every other assertion here is written in terms of the constant and would
    /// hold at any value, so this is the only thing pinning it.
    #[test]
    fn the_grace_is_bounded_on_both_sides() {
        assert!(
            GFX_READY_GRACE <= Duration::from_secs(5),
            "a client that never opens the channel shows nothing for this long: {GFX_READY_GRACE:?}"
        );
        assert!(
            GFX_READY_GRACE >= Duration::from_millis(500),
            "shorter than a channel open can land in: {GFX_READY_GRACE:?}"
        );
    }

    /// Past the grace every frame is a bitmap. A fallback that cleared the
    /// clock on the way out would re-arm itself: one bitmap, another two
    /// seconds of nothing, one bitmap -- half a frame per second, which still
    /// looks broken.
    #[test]
    fn the_fallback_stays_open_once_it_opens() {
        let shared = shared();
        let start = Instant::now();

        assert!(!shared.bitmap_fallback_due(start));
        for extra_ms in [0, 1, 2, 3] {
            assert!(
                shared
                    .bitmap_fallback_due(start + GFX_READY_GRACE + Duration::from_millis(extra_ms)),
                "the fallback re-armed itself {extra_ms}ms past the grace"
            );
        }
    }

    /// Giving up is said once per connection. The line is half of the fix --
    /// silence is what made this hard to recognise from the outside -- and a
    /// line at frame rate buries the log it was added to.
    #[test]
    fn giving_up_is_announced_once_per_connection() {
        let shared = shared();
        let start = Instant::now();
        let past = start + GFX_READY_GRACE;

        let first = capture_warnings(|| {
            shared.bitmap_fallback_due(start);
            for _ in 0..6 {
                shared.bitmap_fallback_due(past);
            }
        });
        assert_eq!(
            first
                .matches("has not opened the graphics pipeline")
                .count(),
            1,
            "expected exactly one line, got: {first}"
        );

        shared.reset_for_new_client();
        let second = capture_warnings(|| {
            shared.bitmap_fallback_due(past);
            shared.bitmap_fallback_due(past + GFX_READY_GRACE);
        });
        assert_eq!(
            second
                .matches("has not opened the graphics pipeline")
                .count(),
            1,
            "the next client must be told too, got: {second}"
        );
    }

    /// Collects what `tracing` emitted on this thread while `body` ran.
    fn capture_warnings(body: impl FnOnce()) -> String {
        #[derive(Clone)]
        struct Sink(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

        impl std::io::Write for Sink {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.lock().expect("sink").extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Sink {
            type Writer = Sink;
            fn make_writer(&'a self) -> Sink {
                self.clone()
            }
        }

        let buffer = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::fmt()
            .with_writer(Sink(std::sync::Arc::clone(&buffer)))
            .with_ansi(false)
            .finish();
        tracing::subscriber::with_default(subscriber, body);
        let bytes = buffer.lock().expect("sink").clone();
        String::from_utf8(bytes).expect("utf-8")
    }

    #[test]
    fn the_wait_starts_at_the_first_ask_and_ends_after_the_grace() {
        let shared = shared();
        let start = Instant::now();

        assert!(!shared.bitmap_fallback_due(start));
        assert!(!shared.bitmap_fallback_due(start + GFX_READY_GRACE - Duration::from_millis(1)));
        assert!(shared.bitmap_fallback_due(start + GFX_READY_GRACE));
    }

    /// The clock starts when the first frame wants to go out, not when the
    /// connection was made -- a capture thread that takes a moment to produce
    /// its first frame must not have that time counted against the client.
    #[test]
    fn the_clock_starts_at_the_first_ask_not_at_construction() {
        let shared = shared();
        let late = Instant::now() + Duration::from_secs(60);

        assert!(!shared.bitmap_fallback_due(late));
        assert!(shared.bitmap_fallback_due(late + GFX_READY_GRACE));
    }

    /// Once the channel has negotiated there is no activation sequence left to
    /// protect, so losing readiness later must not freeze the screen again.
    #[test]
    fn a_channel_that_was_ready_never_waits_again() {
        let shared = shared();
        let start = Instant::now();

        assert!(!shared.bitmap_fallback_due(start));
        shared.note_gfx_ready();

        assert!(shared.bitmap_fallback_due(start + Duration::from_millis(1)));
    }

    /// The wait belongs to one connection. A new client has not shown anything
    /// yet, so it gets the grace its predecessor used up.
    #[test]
    fn a_new_client_gets_a_fresh_wait() {
        let shared = shared();
        let start = Instant::now();

        assert!(!shared.bitmap_fallback_due(start));
        assert!(shared.bitmap_fallback_due(start + GFX_READY_GRACE));
        shared.note_gfx_ready();
        assert!(shared.bitmap_fallback_due(start + GFX_READY_GRACE));

        shared.reset_for_new_client();

        let reconnect = start + Duration::from_secs(600);
        assert!(
            !shared.bitmap_fallback_due(reconnect),
            "the next client is owed the same wait as the first"
        );
        assert!(shared.bitmap_fallback_due(reconnect + GFX_READY_GRACE));
    }
}
