use std::net::SocketAddr;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use ironrdp_server::{
    ConnectionHandler, ConnectionInfo, Credentials, PostConnectionAction, RdpServer,
    SoundServerFactory, TlsIdentityCtx,
};

use crate::audio::{AudioMode, HyprSoundFactory};
use crate::capture::{HyprDisplay, HyprDisplayHandle};
use crate::clipboard::HyprCliprdrFactory;
use crate::config::RuntimeConfig;
use crate::egfx::{EgfxShared, HyprGfxFactory};
use crate::input::{ClientKeyboardLayoutSink, HyprInputHandler, SharedOutputLayout};

mod tls;

pub struct ServerContext {
    server: RdpServer,
    pub display_handle: HyprDisplayHandle,
}

pub async fn setup(config: RuntimeConfig) -> Result<ServerContext> {
    let RuntimeConfig {
        bind,
        cert,
        key,
        username,
        password,
        resolution,
        capture_mode,
        bitrate,
        quality,
        rate_control,
        fps,
        max_frames_in_flight,
        egfx_codec,
        keyboard_layout_policy,
        audio_mode,
        h264_backend,
        resolution_fixed,
        output,
        on_session_start,
        on_session_end,
    } = config;

    let addr = parse_bind_addr(&bind)?;
    let egfx_shared = Arc::new(EgfxShared::with_codec_policy(
        max_frames_in_flight,
        egfx_codec,
    ));
    let output_layout = Arc::new(SharedOutputLayout::new());

    let (display, display_handle, (rdp_width, rdp_height)) = HyprDisplay::new(
        resolution,
        capture_mode,
        Arc::clone(&egfx_shared),
        Arc::clone(&output_layout),
        bitrate,
        quality,
        rate_control,
        fps,
        h264_backend,
        resolution_fixed,
        output,
    )
    .await
    .context("failed to initialize display capture")?;
    egfx_shared.set_surface_size(rdp_width, rdp_height);
    let input_handler =
        HyprInputHandler::new(rdp_width, rdp_height, output_layout, keyboard_layout_policy)
            .context("failed to initialize input handler")?;
    let keyboard_layout_sink = input_handler
        .client_keyboard_layout_handle()
        .context("input handler has no command channel")?;
    let keyboard_layout_sink: Box<dyn ClientKeyboardLayoutSink> = Box::new(keyboard_layout_sink);

    let gfx_factory = HyprGfxFactory::new(Arc::clone(&egfx_shared));
    let cliprdr_factory = HyprCliprdrFactory::new();
    let sound_factory = sound_factory_for_audio_mode(audio_mode);
    let session_hooks = session_hooks_from_config(on_session_start, on_session_end);

    let builder = RdpServer::builder().with_addr(addr);

    let (cert_path, key_path) = tls::resolve_tls_paths(cert.as_deref(), key.as_deref())?;

    let tls_ctx = TlsIdentityCtx::init_from_paths(Path::new(&cert_path), Path::new(&key_path))
        .context("failed to load TLS certificates")?;
    let acceptor = tls_ctx
        .make_acceptor()
        .context("failed to create TLS acceptor")?;

    let credentials = credentials_from_config(&username, &password);
    let secured_builder = match security_mode_for_credentials(&credentials) {
        ServerSecurityMode::Tls => builder.with_tls(acceptor),
        ServerSecurityMode::Hybrid => builder.with_hybrid(acceptor, tls_ctx.pub_key),
    };

    let mut server = secured_builder
        .with_input_handler(input_handler)
        .with_display_handler(display)
        .with_connection_handler(Some(Box::new(ClientConnectionHandler::new(
            keyboard_layout_sink,
            session_hooks,
        ))))
        .with_gfx_factory(Some(Box::new(gfx_factory)))
        .with_cliprdr_factory(Some(Box::new(cliprdr_factory)))
        .with_sound_factory(sound_factory)
        .build();

    server.set_credentials(credentials);

    tracing::info!("RDP server configured for {}", addr);

    Ok(ServerContext {
        server,
        display_handle,
    })
}

fn sound_factory_for_audio_mode(audio_mode: AudioMode) -> Option<Box<dyn SoundServerFactory>> {
    match audio_mode {
        AudioMode::Mirror | AudioMode::Redirect => {
            Some(Box::new(HyprSoundFactory::new(audio_mode)))
        }
        AudioMode::Off => None,
    }
}

pub async fn serve(ctx: &mut ServerContext) -> Result<()> {
    ctx.server.run().await
}

/// Forwards per-connection client metadata to the input-layout policy and
/// drives the configured session hooks.
///
/// IronRDP calls `on_connection_info` once after authentication and initial
/// activation (never during reactivation); the keyboard layout is forwarded
/// to the input module's owner-specific sink, which applies the layout policy
/// and enqueues the keymap command on the input actor. The session hooks run
/// their start command on the same boundary and their end command from
/// `on_disconnected`.
struct ClientConnectionHandler {
    keyboard_layout_sink: Box<dyn ClientKeyboardLayoutSink>,
    session_hooks: Option<SessionHooks>,
}

impl ClientConnectionHandler {
    fn new(
        keyboard_layout_sink: Box<dyn ClientKeyboardLayoutSink>,
        session_hooks: Option<SessionHooks>,
    ) -> Self {
        Self {
            keyboard_layout_sink,
            session_hooks,
        }
    }
}

impl ConnectionHandler for ClientConnectionHandler {
    fn on_connection_info(&mut self, info: &ConnectionInfo) {
        self.keyboard_layout_sink
            .set_keyboard_layout(info.keyboard_layout);
        if let Some(hooks) = &mut self.session_hooks {
            hooks.session_started();
        }
    }

    /// The server calls this only from its own accept loop, so anything that
    /// takes ownership of the loop and drives `run_connection` directly has to
    /// invoke the session-end path itself.
    fn on_disconnected(
        &mut self,
        _peer: SocketAddr,
        _duration: Duration,
        _error: Option<&anyhow::Error>,
    ) -> PostConnectionAction {
        if let Some(hooks) = &mut self.session_hooks {
            hooks.session_ended();
        }
        PostConnectionAction::Continue
    }
}

/// Runs configured shell commands when an authenticated session starts and
/// ends.
///
/// The start hook fires from `on_connection_info`, which the server calls
/// once per connection after credential and auto-reconnect validation
/// succeed — port probes, TLS scanners and failed logins never reach it, and
/// reactivation sequences (client resizes) do not re-fire it. Without
/// configured credentials there is no authentication step, so any fully
/// established session counts; probes still cannot, as they never complete
/// connection setup.
///
/// Commands run on one dedicated thread, one at a time and in session order:
/// each command waits for the previous one to finish, or for
/// `SESSION_HOOK_DEADLINE` — the previous command is then left running and
/// the next one proceeds alongside it, with a warning. That ordering is what
/// makes a do/undo pair (DPMS off/on, monitor reconfiguration) safe across an
/// instant reconnect. Dropping the handler mid-session (service stop) queues
/// the end command and drains what is left within one deadline, so a stuck
/// hook cannot stall shutdown past a service manager's stop timeout.
///
/// A command that outlives its deadline is never killed — `/bin/sh -c`
/// orphans its own children — but its process is reaped as soon as it exits,
/// so no hook leaves a zombie behind while the server runs.
///
/// Driven by [`ClientConnectionHandler`], which owns the single
/// `ConnectionHandler` slot.
struct SessionHooks {
    session_active: bool,
    jobs: Option<mpsc::Sender<HookJob>>,
    shutting_down: Arc<AtomicBool>,
    runner: Option<std::thread::JoinHandle<()>>,
}

enum HookJob {
    SessionStart,
    SessionEnd,
}

const SESSION_HOOK_DEADLINE: Duration = Duration::from_secs(10);

/// How often the hook thread polls a running command for completion.
const HOOK_POLL_INTERVAL: Duration = Duration::from_millis(50);

const HOOK_SESSION_START: &str = "session_start";
const HOOK_SESSION_END: &str = "session_end";

fn session_hooks_from_config(
    on_session_start: Option<String>,
    on_session_end: Option<String>,
) -> Option<SessionHooks> {
    // An empty command is not a hook: the same rule the credentials use.
    let on_session_start = on_session_start.filter(|command| !command.trim().is_empty());
    let on_session_end = on_session_end.filter(|command| !command.trim().is_empty());
    if on_session_start.is_none() && on_session_end.is_none() {
        return None;
    }
    Some(SessionHooks::spawn(
        on_session_start,
        on_session_end,
        SESSION_HOOK_DEADLINE,
    ))
}

impl SessionHooks {
    fn spawn(
        on_session_start: Option<String>,
        on_session_end: Option<String>,
        deadline: Duration,
    ) -> Self {
        let (jobs, queue) = mpsc::channel();
        let shutting_down = Arc::new(AtomicBool::new(false));
        let queue_shutdown = Arc::clone(&shutting_down);
        let runner = std::thread::Builder::new()
            .name("session-hooks".into())
            .spawn(move || {
                run_hook_queue(
                    &queue,
                    &queue_shutdown,
                    on_session_start,
                    on_session_end,
                    deadline,
                );
            });
        let runner = match runner {
            Ok(runner) => Some(runner),
            Err(error) => {
                tracing::warn!(%error, "Failed to start the session hook thread");
                None
            }
        };
        Self {
            session_active: false,
            jobs: runner.is_some().then_some(jobs),
            shutting_down,
            runner,
        }
    }

    fn send(&self, job: HookJob) {
        if let Some(jobs) = &self.jobs {
            if jobs.send(job).is_err() {
                tracing::warn!("Session hook thread is gone; command not queued");
            }
        }
    }

    fn session_started(&mut self) {
        if self.session_active {
            // The seam promises one call per connection; a second one would
            // queue a start command with no end command to balance it.
            return;
        }
        self.session_active = true;
        tracing::debug!("Session established");
        self.send(HookJob::SessionStart);
    }

    fn session_ended(&mut self) {
        if !self.session_active {
            // A probe or failed handshake: no session was established.
            return;
        }
        self.session_active = false;
        tracing::debug!("Session ended");
        self.send(HookJob::SessionEnd);
    }
}

/// A command still running, with the moment its ordering budget started.
struct RunningHook {
    hook: &'static str,
    child: Child,
    started: Instant,
}

fn run_hook_queue(
    jobs: &mpsc::Receiver<HookJob>,
    shutting_down: &AtomicBool,
    on_session_start: Option<String>,
    on_session_end: Option<String>,
    deadline: Duration,
) {
    let mut running: Option<RunningHook> = None;
    // Commands past their deadline: still ours to reap, never to wait on.
    let mut stragglers: Vec<RunningHook> = Vec::new();
    // Once the handler is dropped the whole remaining queue shares one
    // budget, so a stuck command cannot multiply the stop time by the number
    // of jobs behind it.
    let mut drain_until: Option<Instant> = None;

    while let Ok(job) = jobs.recv() {
        reap_finished(&mut stragglers);
        if drain_until.is_none() && shutting_down.load(Ordering::Acquire) {
            drain_until = Some(Instant::now() + deadline);
        }
        let command = match job {
            HookJob::SessionStart => on_session_start.as_deref().map(|c| (HOOK_SESSION_START, c)),
            HookJob::SessionEnd => on_session_end.as_deref().map(|c| (HOOK_SESSION_END, c)),
        };
        let Some((hook, command)) = command else {
            // No command for this boundary: nothing to order against, but the
            // previous command still has to be reaped when it exits.
            if let Some(previous) = running.take() {
                stragglers.push(previous);
            }
            continue;
        };

        finish_running(&mut running, &mut stragglers, deadline, drain_until);
        running = spawn_session_hook(hook, command);
    }

    // Shutdown: only the end command is worth waiting for — nothing is
    // ordered after a start command once the server is going away.
    match running.take() {
        Some(hook) if hook.hook == HOOK_SESSION_END => {
            let mut hook = Some(hook);
            finish_running(&mut hook, &mut stragglers, deadline, drain_until);
        }
        Some(hook) => stragglers.push(hook),
        None => {}
    }
    // Give commands that just exited a moment to be reaped; whatever is still
    // running outlives the server and belongs to the service manager.
    let reap_until = Instant::now() + HOOK_POLL_INTERVAL * 4;
    while !stragglers.is_empty() {
        reap_finished(&mut stragglers);
        if stragglers.is_empty() || Instant::now() >= reap_until {
            break;
        }
        std::thread::sleep(HOOK_POLL_INTERVAL);
    }
}

/// Wait for `running` to finish so the next command cannot overtake it.
/// Past the deadline the command is handed to `stragglers` and the caller
/// proceeds alongside it.
fn finish_running(
    running: &mut Option<RunningHook>,
    stragglers: &mut Vec<RunningHook>,
    deadline: Duration,
    drain_until: Option<Instant>,
) {
    let Some(mut hook) = running.take() else {
        return;
    };
    // Measured from the command's own start, and never past the shutdown
    // budget once the handler is going away.
    let wait_until = match drain_until {
        Some(drain) => (hook.started + deadline).min(drain),
        None => hook.started + deadline,
    };
    loop {
        match hook.child.try_wait() {
            Ok(Some(status)) => {
                if status.success() {
                    tracing::debug!(hook = hook.hook, "Session hook finished");
                } else {
                    tracing::warn!(hook = hook.hook, %status, "Session hook exited with failure");
                }
                return;
            }
            Ok(None) if Instant::now() < wait_until => {
                std::thread::sleep(
                    HOOK_POLL_INTERVAL.min(wait_until.saturating_duration_since(Instant::now())),
                );
            }
            Ok(None) => {
                tracing::warn!(
                    hook = hook.hook,
                    deadline_secs = deadline.as_secs(),
                    "Session hook still running past the ordering deadline; continuing alongside it"
                );
                stragglers.push(hook);
                return;
            }
            Err(error) => {
                tracing::warn!(hook = hook.hook, %error, "Failed to wait for session hook");
                return;
            }
        }
    }
}

/// Reap whatever finished since the last pass, so a long session cannot
/// accumulate zombies.
fn reap_finished(stragglers: &mut Vec<RunningHook>) {
    stragglers.retain_mut(|hook| match hook.child.try_wait() {
        Ok(Some(status)) => {
            if !status.success() {
                tracing::warn!(hook = hook.hook, %status, "Session hook exited with failure");
            }
            false
        }
        Ok(None) => true,
        Err(error) => {
            tracing::warn!(hook = hook.hook, %error, "Failed to wait for session hook");
            false
        }
    });
}

fn spawn_session_hook(hook: &'static str, command: &str) -> Option<RunningHook> {
    tracing::info!(hook, "Running session hook");
    tracing::debug!(hook, command, "Session hook command");
    match Command::new("/bin/sh")
        .arg("-c")
        .arg(command)
        .stdin(Stdio::null())
        .spawn()
    {
        Ok(child) => Some(RunningHook {
            hook,
            child,
            started: Instant::now(),
        }),
        Err(error) => {
            tracing::warn!(hook, %error, "Failed to spawn /bin/sh for the session hook");
            None
        }
    }
}

impl Drop for SessionHooks {
    fn drop(&mut self) {
        // A service stop mid-session must still run the end command, and the
        // flag keeps the queue from starting anything new while draining.
        self.shutting_down.store(true, Ordering::Release);
        if self.session_active {
            self.session_active = false;
            self.send(HookJob::SessionEnd);
        }
        // Close the channel before joining: the queue thread only returns
        // once its sender is gone.
        drop(self.jobs.take());
        if let Some(runner) = self.runner.take() {
            if runner.join().is_err() {
                tracing::warn!("Session hook thread panicked");
            }
        }
    }
}

fn credentials_from_config(username: &str, password: &str) -> Option<Credentials> {
    if username.is_empty() && password.is_empty() {
        None
    } else {
        Some(Credentials {
            username: username.to_string(),
            password: password.to_string(),
            domain: None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ServerSecurityMode {
    Tls,
    Hybrid,
}

fn security_mode_for_credentials(credentials: &Option<Credentials>) -> ServerSecurityMode {
    if credentials.is_some() {
        ServerSecurityMode::Hybrid
    } else {
        ServerSecurityMode::Tls
    }
}

fn parse_bind_addr(bind: &str) -> Result<SocketAddr> {
    bind.parse().context("invalid bind address")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    use ironrdp_pdu::gcc::KeyboardType;
    use ironrdp_server::{
        ConnectionHandler, ConnectionInfo, PostConnectionAction, RdpServer, ServerEvent,
    };
    use tokio::net::TcpStream;
    use tokio::sync::mpsc;
    use tokio::sync::oneshot;

    #[test]
    fn missing_hook_commands_disable_connection_handler_wiring() {
        assert!(session_hooks_from_config(None, None).is_none());
        assert!(session_hooks_from_config(Some("true".into()), None).is_some());
        assert!(session_hooks_from_config(None, Some("true".into())).is_some());
    }

    fn test_hooks(log: &Path, connect_command: Option<String>, disconnect: bool) -> SessionHooks {
        SessionHooks::spawn(
            connect_command,
            disconnect.then(|| echo_to_log(log, "end")),
            Duration::from_secs(10),
        )
    }

    fn echo_to_log(log: &Path, word: &str) -> String {
        format!("echo {word} >> '{}'", log.display())
    }

    fn echo_start(log: &Path, prefix: &str) -> Option<String> {
        Some(format!("{prefix}{}", echo_to_log(log, "start")))
    }

    fn hook_log_path(name: &str) -> PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!("hypr-rdp-hook-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_file(&path);
        path
    }

    /// Generous ceiling for positive waits: loaded CI runners stall threads
    /// for seconds; a matching run still returns at the first poll that sees
    /// the expected content.
    const LOG_CEILING: Duration = Duration::from_secs(30);

    fn wait_for_nonempty_log(path: &Path) -> String {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            let content = std::fs::read_to_string(path).unwrap_or_default();
            if !content.trim().is_empty() || std::time::Instant::now() > deadline {
                return content;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    fn wait_for_log(path: &Path, expected: &str, ceiling: Duration) -> String {
        let deadline = std::time::Instant::now() + ceiling;
        loop {
            let content = std::fs::read_to_string(path).unwrap_or_default();
            if content == expected || std::time::Instant::now() > deadline {
                return content;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn test_peer() -> SocketAddr {
        "127.0.0.1:39999".parse().unwrap()
    }

    fn test_connection_info() -> ConnectionInfo {
        ConnectionInfo::new(0x0409, KeyboardType::IBM_ENHANCED, String::new())
    }

    #[test]
    fn probe_disconnect_without_session_fires_no_hooks() {
        let log = hook_log_path("probe");
        let mut hooks = test_hooks(&log, echo_start(&log, ""), true);

        // A connection that never established a session (port probe, failed
        // handshake) only produces on_disconnected.
        hooks.session_ended();
        drop(hooks);

        // Negative watch: a regression that fires the start hook writes the
        // file a few milliseconds after the drop-join returns.
        let watch_until = std::time::Instant::now() + Duration::from_millis(300);
        while std::time::Instant::now() < watch_until {
            assert_eq!(std::fs::read_to_string(&log).unwrap_or_default(), "");
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    #[test]
    fn session_lifecycle_runs_start_then_end_in_order() {
        let log = hook_log_path("order");
        // The start command sleeps, so only the ordered wait can put
        // "start" first in the log.
        let mut hooks = test_hooks(&log, echo_start(&log, "sleep 0.3; "), true);

        hooks.session_started();
        hooks.session_ended();

        assert_eq!(
            wait_for_log(&log, "start\nend\n", LOG_CEILING),
            "start\nend\n"
        );
        std::fs::remove_file(&log).expect("remove hook log");
    }

    #[test]
    fn hook_calls_do_not_block_the_connection_handler() {
        let log = hook_log_path("nonblocking");
        // Small deadline so the implicit drop-join at the end stays cheap.
        let mut hooks = SessionHooks::spawn(
            Some("exec sleep 30 >/dev/null 2>&1".into()),
            Some(echo_to_log(&log, "end")),
            Duration::from_millis(300),
        );

        let start = std::time::Instant::now();
        hooks.session_started();
        hooks.session_ended();

        // The handler only queues jobs; the ordered wait happens on the hook
        // thread. A blocking implementation would sit out the full sleep;
        // the bound only needs to stay far under that.
        assert!(start.elapsed() < Duration::from_secs(2));

        drop(hooks);
        let _ = std::fs::remove_file(&log);
    }

    #[test]
    fn deadline_releases_the_end_hook_past_a_stuck_start_command() {
        let log = hook_log_path("deadline");
        // The sleeper outlives the test process by up to 30s — acceptable on
        // ephemeral runners, and the gap to the 10s ceiling below is what
        // makes an ignored deadline a deterministic failure.
        let mut hooks = SessionHooks::spawn(
            Some("exec sleep 30 >/dev/null 2>&1".into()),
            Some(echo_to_log(&log, "end")),
            Duration::from_millis(100),
        );

        hooks.session_started();
        hooks.session_ended();

        // The stuck start command is left running; the end command
        // must not wait for it past the deadline.
        assert_eq!(
            wait_for_log(&log, "end\n", Duration::from_secs(10)),
            "end\n"
        );
        std::fs::remove_file(&log).expect("remove hook log");
    }

    #[test]
    fn start_only_configuration_skips_the_ordered_wait() {
        let log = hook_log_path("connect-only");
        let mut hooks = test_hooks(&log, Some("exec sleep 30 >/dev/null 2>&1".into()), false);

        hooks.session_started();
        hooks.session_ended();

        let start = std::time::Instant::now();
        drop(hooks); // joins the hook thread
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "no end command configured, so nothing may wait on the start command"
        );
    }

    #[test]
    fn sequential_sessions_reuse_the_handler_in_order() {
        let log = hook_log_path("cycles");
        let mut hooks = test_hooks(&log, echo_start(&log, ""), true);

        // Ordering across sessions is spawn-order only, so wait for each
        // cycle's writes before starting the next one.
        let mut expected = String::new();
        for _ in 0..2 {
            hooks.session_started();
            hooks.session_ended();
            expected.push_str("start\nend\n");
            assert_eq!(wait_for_log(&log, &expected, LOG_CEILING), expected);
        }
        std::fs::remove_file(&log).expect("remove hook log");
    }

    #[test]
    fn config_wiring_passes_the_commands_through_in_order() {
        let log = hook_log_path("wiring");
        let mut hooks = session_hooks_from_config(
            Some(echo_to_log(&log, "start")),
            Some(echo_to_log(&log, "end")),
        )
        .expect("both commands configured");

        hooks.session_started();
        hooks.session_ended();

        assert_eq!(
            wait_for_log(&log, "start\nend\n", LOG_CEILING),
            "start\nend\n"
        );
        std::fs::remove_file(&log).expect("remove hook log");
    }

    #[test]
    fn end_only_configuration_runs_the_end_hook() {
        let log = hook_log_path("disconnect-only");
        let mut hooks = test_hooks(&log, None, true);

        hooks.session_started();
        hooks.session_ended();

        assert_eq!(wait_for_log(&log, "end\n", LOG_CEILING), "end\n");
        std::fs::remove_file(&log).expect("remove hook log");
    }

    #[test]
    fn failed_start_command_does_not_delay_the_end_hook() {
        let log = hook_log_path("failed-connect");
        let mut hooks = test_hooks(&log, Some("false".into()), true);

        hooks.session_started();
        hooks.session_ended();

        // The start command exits (with failure) immediately; the ordered
        // wait must resolve through the completion channel, not the deadline.
        assert_eq!(wait_for_log(&log, "end\n", Duration::from_secs(5)), "end\n");
        std::fs::remove_file(&log).expect("remove hook log");
    }

    #[test]
    fn shutdown_drain_is_bounded_by_the_deadline() {
        let log = hook_log_path("drain-bound");
        let mut hooks = SessionHooks::spawn(
            Some(echo_to_log(&log, "start")),
            Some("exec sleep 30 >/dev/null 2>&1".into()),
            Duration::from_millis(200),
        );

        hooks.session_started();

        let start = std::time::Instant::now();
        drop(hooks); // queues SessionEnd, drains with a bounded wait
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "an end command slower than the deadline must not stall shutdown"
        );
        let _ = std::fs::remove_file(&log);
    }

    #[test]
    fn dropping_the_handler_mid_session_still_runs_the_end_hook() {
        let log = hook_log_path("drop");
        let mut hooks = test_hooks(&log, echo_start(&log, ""), true);

        hooks.session_started();
        drop(hooks); // service stop: the do/undo pair must still complete

        assert_eq!(
            std::fs::read_to_string(&log).expect("hook log"),
            "start\nend\n"
        );
        std::fs::remove_file(&log).expect("remove hook log");
    }

    #[test]
    fn finished_hook_processes_are_reaped() {
        let log = hook_log_path("reap");
        // The command reports its own pid, so the check cannot be confused by
        // the children of the tests running in parallel.
        let mut hooks = SessionHooks::spawn(
            Some(format!("echo $$ >> '{}'", log.display())),
            None,
            Duration::from_secs(10),
        );

        hooks.session_started();
        drop(hooks);

        let pid = wait_for_nonempty_log(&log);
        let pid = pid.trim();
        // A queue that spawns without reaping leaves the command in the
        // process table for the lifetime of the service.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
            // "<pid> (<comm>) <state> ..." — comm itself may contain spaces.
            let zombie = stat
                .rsplit_once(')')
                .is_some_and(|(_, rest)| rest.trim_start().starts_with('Z'));
            if !zombie {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "hook process {pid} was never reaped"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
        std::fs::remove_file(&log).expect("remove hook log");
    }

    #[test]
    fn a_completed_session_does_not_fire_a_second_end_hook_on_drop() {
        let log = hook_log_path("no-double-end");
        let mut hooks = test_hooks(&log, echo_start(&log, ""), true);

        hooks.session_started();
        hooks.session_ended();
        assert_eq!(
            wait_for_log(&log, "start\nend\n", LOG_CEILING),
            "start\nend\n"
        );

        // Service stop after the client already disconnected: the pair is
        // balanced, so the drop must not run the end command again.
        drop(hooks);
        assert_eq!(
            std::fs::read_to_string(&log).expect("hook log"),
            "start\nend\n"
        );
        std::fs::remove_file(&log).expect("remove hook log");
    }

    #[test]
    fn repeated_session_starts_queue_one_command() {
        let log = hook_log_path("double-start");
        let mut hooks = test_hooks(&log, echo_start(&log, ""), true);

        hooks.session_started();
        hooks.session_started();
        hooks.session_ended();
        drop(hooks);

        assert_eq!(
            std::fs::read_to_string(&log).expect("hook log"),
            "start\nend\n"
        );
        std::fs::remove_file(&log).expect("remove hook log");
    }

    #[test]
    fn dropping_the_handler_is_bounded_even_with_a_job_in_flight() {
        let log = hook_log_path("drop-bound-deadlock");
        let mut hooks = test_hooks(&log, echo_start(&log, ""), true);
        hooks.session_started();

        // Drop on a helper thread: closing the job channel after joining the
        // queue thread deadlocks, which would hang the whole suite instead of
        // failing this test.
        let (done, finished) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            drop(hooks);
            let _ = done.send(());
        });

        assert!(
            finished.recv_timeout(Duration::from_secs(30)).is_ok(),
            "drop must close the job channel before joining the queue thread"
        );
        let _ = std::fs::remove_file(&log);
    }

    #[test]
    fn hook_commands_cannot_read_the_server_stdin() {
        let log = hook_log_path("stdin");
        let mut hooks = SessionHooks::spawn(
            Some(format!("readlink /proc/self/fd/0 >> '{}'", log.display())),
            None,
            Duration::from_secs(10),
        );

        hooks.session_started();
        drop(hooks);

        assert_eq!(
            wait_for_log(&log, "/dev/null\n", LOG_CEILING),
            "/dev/null\n",
            "a hook must not inherit the server's stdin"
        );
        std::fs::remove_file(&log).expect("remove hook log");
    }

    #[test]
    fn the_session_hook_deadline_stays_short_enough_for_a_service_stop() {
        // The constant bounds the ordering wait and the shutdown drain.
        // systemd's default TimeoutStopSec is 90s: anything near it turns a
        // stuck hook into a SIGKILL, and zero removes the ordering guarantee.
        assert!(SESSION_HOOK_DEADLINE >= Duration::from_secs(1));
        assert!(SESSION_HOOK_DEADLINE <= Duration::from_secs(30));
    }

    #[test]
    fn connection_handler_drives_hooks_on_both_boundaries() {
        struct NoopSink;
        impl ClientKeyboardLayoutSink for NoopSink {
            fn set_keyboard_layout(&self, _keyboard_layout: u32) {}
        }

        let log = hook_log_path("forwarding");
        let hooks = test_hooks(&log, echo_start(&log, ""), true);
        let mut handler = ClientConnectionHandler::new(Box::new(NoopSink), Some(hooks));

        handler.on_connection_info(&test_connection_info());
        // The start command must already have run on this boundary alone,
        // before any disconnect: that is the boundary the feature is about.
        assert_eq!(wait_for_log(&log, "start\n", LOG_CEILING), "start\n");

        let action = handler.on_disconnected(test_peer(), Duration::from_secs(1), None);
        assert_eq!(action, PostConnectionAction::Continue);

        assert_eq!(
            wait_for_log(&log, "start\nend\n", LOG_CEILING),
            "start\nend\n"
        );
        std::fs::remove_file(&log).expect("remove hook log");
    }

    #[test]
    fn on_connection_info_forwards_keyboard_layout_to_sink() {
        use std::sync::{Arc, Mutex};

        struct RecordingSink {
            layouts: Arc<Mutex<Vec<u32>>>,
        }

        impl ClientKeyboardLayoutSink for RecordingSink {
            fn set_keyboard_layout(&self, keyboard_layout: u32) {
                self.layouts.lock().unwrap().push(keyboard_layout);
            }
        }

        let layouts = Arc::new(Mutex::new(Vec::new()));
        let sink = RecordingSink {
            layouts: Arc::clone(&layouts),
        };
        let mut handler = ClientConnectionHandler::new(Box::new(sink), None);

        handler.on_connection_info(&ConnectionInfo::new(
            0x00000407,
            KeyboardType::IBM_ENHANCED,
            String::new(),
        ));

        assert_eq!(*layouts.lock().unwrap(), vec![0x00000407]);
    }

    #[test]
    fn empty_username_and_password_disable_authentication() {
        assert!(credentials_from_config("", "").is_none());
    }

    #[test]
    fn non_empty_username_or_password_enables_authentication() {
        let with_both = credentials_from_config("user", "pass").expect("credentials");
        assert_eq!(with_both.username, "user");
        assert_eq!(with_both.password, "pass");
        assert_eq!(with_both.domain, None);

        let with_username = credentials_from_config("user", "").expect("credentials");
        assert_eq!(with_username.username, "user");
        assert_eq!(with_username.password, "");

        let with_password = credentials_from_config("", "pass").expect("credentials");
        assert_eq!(with_password.username, "");
        assert_eq!(with_password.password, "pass");
    }

    #[test]
    fn server_security_mode_uses_hybrid_only_when_credentials_are_configured() {
        assert_eq!(
            security_mode_for_credentials(&credentials_from_config("", "")),
            ServerSecurityMode::Tls
        );
        assert_eq!(
            security_mode_for_credentials(&credentials_from_config("user", "pass")),
            ServerSecurityMode::Hybrid
        );
        assert_eq!(
            security_mode_for_credentials(&credentials_from_config("user", "")),
            ServerSecurityMode::Hybrid
        );
        assert_eq!(
            security_mode_for_credentials(&credentials_from_config("", "pass")),
            ServerSecurityMode::Hybrid
        );
    }

    #[test]
    fn audio_mode_off_disables_sound_factory_wiring() {
        assert!(sound_factory_for_audio_mode(AudioMode::Mirror).is_some());
        assert!(sound_factory_for_audio_mode(AudioMode::Redirect).is_some());
        assert!(sound_factory_for_audio_mode(AudioMode::Off).is_none());
    }

    #[test]
    fn invalid_bind_address_is_rejected_before_server_setup() {
        let error = parse_bind_addr("not an address").expect_err("invalid bind must fail");

        assert!(format!("{error:#}").contains("invalid bind address"));
    }

    #[tokio::test]
    async fn server_lifecycle_quit_exits_after_ephemeral_bind() {
        let mut server = RdpServer::builder()
            .with_addr(([127, 0, 0, 1], 0))
            .with_no_security()
            .with_no_input()
            .with_no_display()
            .build();
        let event_sender = server.event_sender().clone();

        let local = tokio::task::LocalSet::new();
        local
            .run_until(async move {
                let server_task = tokio::task::spawn_local(async move { server.run().await });
                let bound_addr = wait_for_local_addr(&event_sender).await;
                assert_eq!(bound_addr.ip().to_string(), "127.0.0.1");
                assert_ne!(bound_addr.port(), 0);

                event_sender
                    .send(ServerEvent::Quit("test quit".into()))
                    .expect("server event receiver");

                tokio::time::timeout(Duration::from_secs(1), server_task)
                    .await
                    .expect("server quit must be bounded")
                    .expect("server task must not panic")
                    .expect("server run must succeed");
            })
            .await;
    }

    #[tokio::test]
    async fn server_lifecycle_client_abort_returns_to_disconnect_handler() {
        let mut server = RdpServer::builder()
            .with_addr(([127, 0, 0, 1], 0))
            .with_no_security()
            .with_no_input()
            .with_no_display()
            .with_connection_handler(Some(Box::new(StopAfterDisconnect)))
            .build();
        let event_sender = server.event_sender().clone();

        let local = tokio::task::LocalSet::new();
        local
            .run_until(async move {
                let server_task = tokio::task::spawn_local(async move { server.run().await });
                let bound_addr = wait_for_local_addr(&event_sender).await;
                let stream = TcpStream::connect(bound_addr)
                    .await
                    .expect("connect to server");
                drop(stream);

                tokio::time::timeout(Duration::from_secs(1), server_task)
                    .await
                    .expect("client abort must be bounded")
                    .expect("server task must not panic")
                    .expect("server run must succeed");
            })
            .await;
    }

    struct StopAfterDisconnect;

    impl ConnectionHandler for StopAfterDisconnect {
        fn on_disconnected(
            &mut self,
            _peer: std::net::SocketAddr,
            _duration: Duration,
            error: Option<&anyhow::Error>,
        ) -> PostConnectionAction {
            assert!(error.is_some(), "raw client abort should end with an error");
            PostConnectionAction::Stop
        }
    }

    async fn wait_for_local_addr(
        event_sender: &mpsc::UnboundedSender<ServerEvent>,
    ) -> std::net::SocketAddr {
        for _ in 0..100 {
            let (tx, rx) = oneshot::channel();
            event_sender
                .send(ServerEvent::GetLocalAddr(tx))
                .expect("server event receiver");
            if let Some(addr) = rx.await.expect("local addr response") {
                return addr;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        panic!("server did not publish local address");
    }
}
