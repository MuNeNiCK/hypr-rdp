use std::io::{BufRead, Read};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};

use super::format::{BITS_PER_SAMPLE, CHANNELS, SAMPLE_RATE};

const DEFAULT_REMOTE_SINK_NAME: &str = "hypr_rdp_remote_audio";

/// How long any one `pactl` call may take. These run on the thread that tears a
/// session down, which is the thread that goes back to accepting connections, so
/// a sound server that has stopped answering must not be able to hold it.
const ROUTE_COMMAND_TIMEOUT: Duration = Duration::from_secs(2);
const ROUTE_COMMAND_POLL: Duration = Duration::from_millis(10);

/// How long undoing the routing may take in total.
///
/// The per-call deadline bounds one `pactl`; it does not bound their sum, and
/// teardown is the one path that keeps going after a failure instead of
/// returning. It runs one command per stream that was moved, so a machine with
/// several players and a sound server that answers slowly could hold the thread
/// that goes back to accepting connections for far longer than any single
/// deadline suggests.
const ROUTE_RESTORE_BUDGET: Duration = Duration::from_secs(3);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioMode {
    Mirror,
    Redirect,
    Off,
}

pub(super) trait ActiveAudioRouting: Send {}

pub(super) trait AudioRoutingRunner: Send + Sync {
    fn start(&self, mode: AudioMode) -> Result<Option<Box<dyn ActiveAudioRouting>>>;
}

pub(super) struct PipeWireRoutingRunner {
    command_runner: Arc<dyn RouteCommandRunner>,
    sink_name: String,
    stream_watcher: Arc<dyn SinkInputWatcher>,
}

impl PipeWireRoutingRunner {
    pub(super) fn new() -> Self {
        Self {
            command_runner: Arc::new(SystemCommandRunner),
            sink_name: next_remote_sink_name(),
            stream_watcher: Arc::new(PactlSubscribeWatcher),
        }
    }

    #[cfg(test)]
    pub(super) fn with_runner(command_runner: Arc<dyn RouteCommandRunner>) -> Self {
        struct NoWatchWatcher;
        impl SinkInputWatcher for NoWatchWatcher {
            fn start(
                &self,
                _command_runner: Arc<dyn RouteCommandRunner>,
                _sink_name: &str,
            ) -> Option<StreamWatch> {
                None
            }
        }
        Self {
            command_runner,
            sink_name: DEFAULT_REMOTE_SINK_NAME.to_owned(),
            stream_watcher: Arc::new(NoWatchWatcher),
        }
    }

    fn start_redirect(&self) -> Result<RedirectRouteGuard> {
        let previous_default_sink = default_sink(self.command_runner.as_ref())?;
        let module_id = load_remote_sink(self.command_runner.as_ref(), &self.sink_name)?;
        let mut guard = RedirectRouteGuard {
            command_runner: Arc::clone(&self.command_runner),
            sink_name: self.sink_name.clone(),
            previous_default_sink,
            moved_sink_inputs: Vec::new(),
            module_id: Some(module_id),
            stream_watch: None,
            restored: false,
        };

        if let Err(error) = guard.activate() {
            guard.restore();
            return Err(error);
        }

        // Streams that start after activation follow WirePlumber's
        // remembered per-application targets, not the changed default sink,
        // so they would play on the physical output. Follow their creation
        // and move them over; losing the watch degrades to the previous
        // behavior, so it is not fatal.
        guard.stream_watch = self
            .stream_watcher
            .start(Arc::clone(&self.command_runner), &self.sink_name);

        Ok(guard)
    }
}

/// Moves sink-inputs that appear while the redirect is active onto the
/// remote sink. Only `new` events are followed: a stream the user manually
/// re-routes mid-session afterwards stays where it was put.
pub(super) trait SinkInputWatcher: Send + Sync {
    fn start(
        &self,
        command_runner: Arc<dyn RouteCommandRunner>,
        sink_name: &str,
    ) -> Option<StreamWatch>;
}

struct PactlSubscribeWatcher;

impl SinkInputWatcher for PactlSubscribeWatcher {
    fn start(
        &self,
        command_runner: Arc<dyn RouteCommandRunner>,
        sink_name: &str,
    ) -> Option<StreamWatch> {
        let mut child = match Command::new("pactl")
            .arg("subscribe")
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(child) => child,
            Err(error) => {
                tracing::warn!("Audio: failed to start pactl subscribe: {}", error);
                return None;
            }
        };
        let stdout = child.stdout.take()?;
        let sink_name = sink_name.to_owned();
        let thread = thread::Builder::new()
            .name("hypr-rdp-audio-route".into())
            .spawn(move || {
                let lines = std::io::BufReader::new(stdout)
                    .lines()
                    .map_while(Result::ok);
                follow_new_sink_inputs(lines, command_runner.as_ref(), &sink_name);
            })
            .ok()?;
        Some(StreamWatch {
            child: Some(child),
            thread: Some(thread),
        })
    }
}

/// Owns the `pactl subscribe` child and its reader thread; killing the
/// child ends the reader's stream, so stop() is bounded.
pub(super) struct StreamWatch {
    child: Option<Child>,
    thread: Option<thread::JoinHandle<()>>,
}

impl StreamWatch {
    fn stop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for StreamWatch {
    fn drop(&mut self) {
        self.stop();
    }
}

fn follow_new_sink_inputs(
    lines: impl Iterator<Item = String>,
    command_runner: &dyn RouteCommandRunner,
    sink_name: &str,
) {
    for line in lines {
        let Some(input_id) = parse_new_sink_input_event(&line) else {
            continue;
        };
        // Short-lived streams can vanish before the move lands; that is
        // not worth a warning.
        if let Err(error) = move_sink_input(command_runner, input_id, sink_name) {
            tracing::debug!(
                input_id,
                "Audio: failed to move new sink input: {:#}",
                error
            );
        } else {
            tracing::debug!(input_id, sink_name, "Audio: moved new sink input");
        }
    }
}

fn parse_new_sink_input_event(line: &str) -> Option<&str> {
    let id = line.trim().strip_prefix("Event 'new' on sink-input #")?;
    (!id.is_empty() && id.bytes().all(|b| b.is_ascii_digit())).then_some(id)
}

fn next_remote_sink_name() -> String {
    static NEXT_REMOTE_SINK_ID: AtomicU64 = AtomicU64::new(1);

    let id = NEXT_REMOTE_SINK_ID.fetch_add(1, Ordering::Relaxed);
    format!("{DEFAULT_REMOTE_SINK_NAME}_{}_{}", std::process::id(), id)
}

impl AudioRoutingRunner for PipeWireRoutingRunner {
    fn start(&self, mode: AudioMode) -> Result<Option<Box<dyn ActiveAudioRouting>>> {
        match mode {
            AudioMode::Mirror | AudioMode::Off => Ok(None),
            AudioMode::Redirect => Ok(Some(Box::new(self.start_redirect()?))),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct RouteCommandOutput {
    stdout: String,
}

pub(super) trait RouteCommandRunner: Send + Sync {
    fn run(&self, program: &str, args: &[String]) -> Result<RouteCommandOutput>;
}

struct SystemCommandRunner;

/// Reads a child pipe to the end on its own thread, so a child that outruns the
/// pipe buffer cannot block waiting for us while we wait for it.
fn drain(pipe: Option<impl Read + Send + 'static>) -> thread::JoinHandle<String> {
    thread::spawn(move || {
        let mut text = String::new();
        if let Some(mut pipe) = pipe {
            let _ = pipe.read_to_string(&mut text);
        }
        text
    })
}

/// Waits for `child`, giving up after `ROUTE_COMMAND_TIMEOUT`.
///
/// The child is polled here rather than waited on in a helper thread so that the
/// timeout path still owns it: `Child::kill` is safe only while the process has
/// not been reaped, and nothing else reaps it.
fn wait_within(child: &mut Child, description: &str) -> Result<std::process::ExitStatus> {
    let deadline = Instant::now() + ROUTE_COMMAND_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) => {}
            Err(error) => {
                return Err(error).with_context(|| format!("failed to wait for {description}"))
            }
        }

        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("{description} did not finish within {ROUTE_COMMAND_TIMEOUT:?}");
        }

        thread::sleep(ROUTE_COMMAND_POLL);
    }
}

impl RouteCommandRunner for SystemCommandRunner {
    fn run(&self, program: &str, args: &[String]) -> Result<RouteCommandOutput> {
        let description = format!("{program} {}", args.join(" "));
        let mut child = Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("failed to run {program}"))?;

        let stdout = drain(child.stdout.take());
        let stderr = drain(child.stderr.take());
        let status = wait_within(&mut child, &description)?;

        let stdout = stdout.join().unwrap_or_default();
        let stderr = stderr.join().unwrap_or_default();

        if !status.success() {
            bail!("{} failed: {}", description, stderr.trim());
        }

        Ok(RouteCommandOutput { stdout })
    }
}

struct RedirectRouteGuard {
    command_runner: Arc<dyn RouteCommandRunner>,
    sink_name: String,
    previous_default_sink: Option<String>,
    moved_sink_inputs: Vec<SinkInputRoute>,
    module_id: Option<String>,
    stream_watch: Option<StreamWatch>,
    restored: bool,
}

/// Wraps a runner with a shared deadline, so a sequence of commands is bounded
/// as a sequence and not only one command at a time.
struct BudgetedRunner<'a> {
    inner: &'a dyn RouteCommandRunner,
    deadline: Instant,
}

impl RouteCommandRunner for BudgetedRunner<'_> {
    fn run(&self, program: &str, args: &[String]) -> Result<RouteCommandOutput> {
        if Instant::now() >= self.deadline {
            bail!(
                "{} {} skipped: the audio teardown budget of {:?} is spent",
                program,
                args.join(" "),
                ROUTE_RESTORE_BUDGET
            );
        }
        self.inner.run(program, args)
    }
}

impl RedirectRouteGuard {
    fn activate(&mut self) -> Result<()> {
        pactl(
            self.command_runner.as_ref(),
            &["set-default-sink".into(), self.sink_name.clone()],
        )?;
        let remote_sink_id = sink_id_by_name(self.command_runner.as_ref(), &self.sink_name)?
            .context("remote audio sink is missing after loading module")?;
        move_all_sink_inputs(
            self.command_runner.as_ref(),
            &self.sink_name,
            &remote_sink_id,
            &mut self.moved_sink_inputs,
        )?;
        Ok(())
    }

    fn restore(&mut self) {
        if self.restored {
            return;
        }
        self.restored = true;

        // Stop following new streams before moving anything back, so the
        // watcher cannot re-route an input the restore just moved. This is
        // outside the budget below on purpose: it kills a child and joins a
        // thread, and the one command that thread can be inside is already
        // bounded on its own.
        if let Some(mut watch) = self.stream_watch.take() {
            watch.stop();
        }

        // Everything below this point except unloading the module shares one
        // deadline. Unloading does not: while the module is loaded the
        // machine's default output is a sink nobody is listening to, so that
        // command is worth its own wait even when the rest gave up.
        let budgeted = BudgetedRunner {
            inner: self.command_runner.as_ref(),
            deadline: Instant::now() + ROUTE_RESTORE_BUDGET,
        };

        let current_default_sink = match default_sink(&budgeted) {
            Ok(current_default_sink) => current_default_sink,
            Err(error) => {
                tracing::warn!("Audio: failed to read current default sink: {:#}", error);
                None
            }
        };

        if let Some(previous_default_sink) = self.previous_default_sink.as_deref() {
            let should_restore_default = match current_default_sink.as_deref() {
                Some(current) => current == self.sink_name,
                None => true,
            };

            if should_restore_default {
                if let Err(error) = pactl(
                    &budgeted,
                    &["set-default-sink".into(), previous_default_sink.into()],
                ) {
                    tracing::warn!("Audio: failed to restore default sink: {:#}", error);
                }
            }

            let fallback_sink = current_default_sink
                .as_deref()
                .filter(|current| *current != self.sink_name)
                .unwrap_or(previous_default_sink);
            self.restore_sink_inputs(&budgeted, Some(fallback_sink));
        } else {
            self.restore_sink_inputs(
                &budgeted,
                current_default_sink
                    .as_deref()
                    .filter(|current| *current != self.sink_name),
            );
        }

        if let Some(module_id) = self.module_id.take() {
            if let Err(error) = pactl(
                self.command_runner.as_ref(),
                &["unload-module".into(), module_id],
            ) {
                tracing::warn!("Audio: failed to unload remote audio sink: {:#}", error);
            }
        }
    }

    fn restore_sink_inputs(&self, runner: &dyn RouteCommandRunner, fallback_sink: Option<&str>) {
        if let Err(error) = restore_sink_inputs_from_remote(
            runner,
            &self.sink_name,
            &self.moved_sink_inputs,
            fallback_sink,
        ) {
            tracing::warn!("Audio: failed to move sink inputs back: {:#}", error);
        }
    }
}

impl ActiveAudioRouting for RedirectRouteGuard {}

impl Drop for RedirectRouteGuard {
    fn drop(&mut self) {
        self.restore();
    }
}

fn pactl(command_runner: &dyn RouteCommandRunner, args: &[String]) -> Result<RouteCommandOutput> {
    command_runner.run("pactl", args)
}

fn default_sink(command_runner: &dyn RouteCommandRunner) -> Result<Option<String>> {
    let output = pactl(command_runner, &["get-default-sink".into()])?;
    Ok(parse_single_name(&output.stdout))
}

fn load_remote_sink(command_runner: &dyn RouteCommandRunner, sink_name: &str) -> Result<String> {
    let output = pactl(
        command_runner,
        &[
            "load-module".into(),
            "module-null-sink".into(),
            format!("sink_name={sink_name}"),
            "sink_properties=device.description=hypr-rdp-remote-audio".into(),
            format!(
                "format={}",
                if BITS_PER_SAMPLE == 16 {
                    "s16le"
                } else {
                    "float32le"
                }
            ),
            format!("rate={SAMPLE_RATE}"),
            format!("channels={CHANNELS}"),
        ],
    )?;

    parse_module_id(&output.stdout).context("pactl load-module did not return a module id")
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SinkInputRoute {
    input_id: String,
    sink_id: String,
}

fn move_all_sink_inputs(
    command_runner: &dyn RouteCommandRunner,
    target_sink: &str,
    target_sink_id: &str,
    moved_sink_inputs: &mut Vec<SinkInputRoute>,
) -> Result<()> {
    let routes = sink_input_routes(command_runner)?;
    for route in routes {
        if route.sink_id == target_sink_id {
            continue;
        }
        move_sink_input(command_runner, &route.input_id, target_sink)?;
        moved_sink_inputs.push(route);
    }
    Ok(())
}

fn restore_sink_inputs_from_remote(
    command_runner: &dyn RouteCommandRunner,
    source_sink_name: &str,
    moved_sink_inputs: &[SinkInputRoute],
    fallback_sink: Option<&str>,
) -> Result<()> {
    let Some(source_sink_id) = sink_id_by_name(command_runner, source_sink_name)? else {
        return Ok(());
    };

    for route in sink_input_routes(command_runner)?
        .into_iter()
        .filter(|route| route.sink_id == source_sink_id)
    {
        let original_sink = moved_sink_inputs
            .iter()
            .find(|moved| moved.input_id == route.input_id)
            .map(|moved| moved.sink_id.as_str());
        let Some(target_sink) = original_sink.or(fallback_sink) else {
            continue;
        };

        if let Err(error) = move_sink_input(command_runner, &route.input_id, target_sink) {
            tracing::warn!(
                sink_input = route.input_id,
                target_sink,
                "Audio: failed to restore sink input route: {:#}",
                error
            );

            match (original_sink, fallback_sink) {
                (Some(original_sink), Some(fallback_sink)) if original_sink != fallback_sink => {
                    move_sink_input(command_runner, &route.input_id, fallback_sink)?;
                }
                _ => {
                    return Err(error);
                }
            }
        }
    }
    Ok(())
}

fn move_sink_input(
    command_runner: &dyn RouteCommandRunner,
    sink_input_id: &str,
    target_sink: &str,
) -> Result<()> {
    pactl(
        command_runner,
        &[
            "move-sink-input".into(),
            sink_input_id.into(),
            target_sink.into(),
        ],
    )?;
    Ok(())
}

fn sink_input_routes(command_runner: &dyn RouteCommandRunner) -> Result<Vec<SinkInputRoute>> {
    let output = pactl(
        command_runner,
        &["list".into(), "short".into(), "sink-inputs".into()],
    )?;
    Ok(parse_sink_input_routes(&output.stdout))
}

fn sink_id_by_name(
    command_runner: &dyn RouteCommandRunner,
    sink_name: &str,
) -> Result<Option<String>> {
    let output = pactl(
        command_runner,
        &["list".into(), "short".into(), "sinks".into()],
    )?;
    Ok(parse_sink_id_by_name(&output.stdout, sink_name))
}

fn parse_single_name(output: &str) -> Option<String> {
    output
        .lines()
        .map(str::trim)
        .find(|value| !value.is_empty())
        .map(str::to_owned)
}

fn parse_module_id(output: &str) -> Option<String> {
    output
        .split_whitespace()
        .next()
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn parse_sink_input_routes(output: &str) -> Vec<SinkInputRoute> {
    output
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            Some(SinkInputRoute {
                input_id: fields.next()?.to_owned(),
                sink_id: fields.next()?.to_owned(),
            })
        })
        .collect()
}

fn parse_sink_id_by_name(output: &str, sink_name: &str) -> Option<String> {
    output.lines().find_map(|line| {
        let mut fields = line.split_whitespace();
        let sink_id = fields.next()?;
        let name = fields.next()?;
        (name == sink_name).then(|| sink_id.to_owned())
    })
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use super::*;

    #[derive(Default)]
    struct ScriptedRunner {
        outputs: Mutex<VecDeque<Result<RouteCommandOutput, String>>>,
        calls: Mutex<Vec<Vec<String>>>,
    }

    impl ScriptedRunner {
        fn with_outputs(outputs: Vec<Result<&'static str, &'static str>>) -> Arc<Self> {
            Arc::new(Self {
                outputs: Mutex::new(
                    outputs
                        .into_iter()
                        .map(|result| {
                            result
                                .map(|stdout| RouteCommandOutput {
                                    stdout: stdout.to_owned(),
                                })
                                .map_err(str::to_owned)
                        })
                        .collect(),
                ),
                calls: Mutex::new(Vec::new()),
            })
        }

        fn calls(&self) -> Vec<Vec<String>> {
            self.calls.lock().unwrap().clone()
        }
    }

    /// A runner that takes `delay` on its first call and is instant after, so a
    /// test can spend the teardown budget without spending its own time.
    struct SlowFirstRunner {
        delay: Duration,
        calls: Mutex<Vec<Vec<String>>>,
        served: Mutex<usize>,
    }

    impl RouteCommandRunner for SlowFirstRunner {
        fn run(&self, program: &str, args: &[String]) -> Result<RouteCommandOutput> {
            let mut call = vec![program.to_owned()];
            call.extend(args.iter().cloned());
            self.calls.lock().unwrap().push(call);

            let mut served = self.served.lock().unwrap();
            let first = *served == 0;
            *served += 1;
            drop(served);

            if first {
                std::thread::sleep(self.delay);
            }
            Ok(RouteCommandOutput {
                stdout: "other_sink".to_owned(),
            })
        }
    }

    /// The per-call deadline bounds one command; this bounds their sum. Without
    /// it, teardown runs one command per moved stream and keeps going after
    /// each failure, so a slow sound server holds the thread that returns to
    /// accepting connections for as long as it likes.
    #[test]
    fn a_spent_budget_refuses_further_commands_without_running_them() {
        let inner = ScriptedRunner::with_outputs(vec![Ok("never used")]);
        let spent = BudgetedRunner {
            inner: inner.as_ref(),
            deadline: Instant::now() - Duration::from_millis(1),
        };

        let error = spent
            .run("pactl", &["get-default-sink".to_owned()])
            .expect_err("a spent budget must refuse");

        assert!(
            error.to_string().contains("budget"),
            "unexpected error: {error}"
        );
        assert!(
            inner.calls().is_empty(),
            "a refused command must not reach the sound server"
        );
    }

    /// Unloading the module is not under the shared budget. While it is loaded
    /// the machine's default output is a sink nobody is listening to, so it is
    /// worth its own wait even when everything before it gave up.
    #[test]
    fn the_module_is_unloaded_even_when_the_budget_is_spent() {
        let runner = Arc::new(SlowFirstRunner {
            delay: ROUTE_RESTORE_BUDGET + Duration::from_millis(100),
            calls: Mutex::new(Vec::new()),
            served: Mutex::new(0),
        });
        let mut guard = RedirectRouteGuard {
            command_runner: Arc::clone(&runner) as Arc<dyn RouteCommandRunner>,
            sink_name: DEFAULT_REMOTE_SINK_NAME.to_owned(),
            previous_default_sink: Some("previous_sink".to_owned()),
            moved_sink_inputs: Vec::new(),
            module_id: Some("42".to_owned()),
            stream_watch: None,
            restored: false,
        };

        let started = Instant::now();
        guard.restore();
        let elapsed = started.elapsed();

        let calls = runner.calls.lock().unwrap().clone();
        assert!(
            calls
                .iter()
                .any(|call| call.get(1).map(String::as_str) == Some("unload-module")),
            "the remote sink must come out even after the budget is spent: {calls:?}"
        );
        assert!(
            elapsed < ROUTE_RESTORE_BUDGET + ROUTE_COMMAND_TIMEOUT + Duration::from_secs(2),
            "teardown ran for {elapsed:?}, which is not a bounded teardown"
        );
    }

    /// The real runner, not the scripted one: these spawn processes.
    ///
    /// `pactl` is run on the thread that tears a session down and then goes back
    /// to accepting connections, so the failure this bounds is a sound server
    /// that accepts the request and never answers -- which is what issue #66
    /// describes on the other side of the teardown.
    #[test]
    fn a_command_that_never_finishes_is_given_up_on() {
        let started = Instant::now();
        let result = SystemCommandRunner.run("sleep", &["30".to_owned()]);
        let elapsed = started.elapsed();

        let error = result.expect_err("a command that outlives the budget must fail");
        assert!(
            error.to_string().contains("did not finish within"),
            "unexpected error: {error}"
        );
        assert!(
            elapsed < ROUTE_COMMAND_TIMEOUT + Duration::from_secs(1),
            "gave up after {elapsed:?}, budget is {ROUTE_COMMAND_TIMEOUT:?}"
        );
    }

    #[test]
    fn a_command_that_finishes_still_returns_its_output() {
        let output = SystemCommandRunner
            .run("echo", &["hello".to_owned()])
            .expect("echo should succeed");

        assert_eq!(output.stdout.trim(), "hello");
    }

    /// More output than a pipe buffer holds. Waiting for the child while nothing
    /// reads its stdout would deadlock until the budget expired, turning a
    /// working command into a timeout.
    #[test]
    fn output_larger_than_a_pipe_buffer_does_not_deadlock() {
        let output = SystemCommandRunner
            .run("sh", &["-c".to_owned(), "printf '%0999999d' 0".to_owned()])
            .expect("sh should succeed");

        assert_eq!(output.stdout.len(), 999_999);
    }

    #[test]
    fn a_failing_command_reports_its_stderr() {
        let error = SystemCommandRunner
            .run("sh", &["-c".to_owned(), "echo nope >&2; exit 1".to_owned()])
            .expect_err("a non-zero exit must fail");

        assert!(error.to_string().contains("nope"), "unexpected: {error}");
    }

    impl RouteCommandRunner for ScriptedRunner {
        fn run(&self, program: &str, args: &[String]) -> Result<RouteCommandOutput> {
            let mut call = Vec::with_capacity(args.len() + 1);
            call.push(program.to_owned());
            call.extend(args.iter().cloned());
            self.calls.lock().unwrap().push(call);

            let output = self
                .outputs
                .lock()
                .unwrap()
                .pop_front()
                .expect("scripted output missing");

            output.map_err(anyhow::Error::msg)
        }
    }

    #[test]
    fn new_sink_input_events_are_parsed_and_others_ignored() {
        assert_eq!(
            parse_new_sink_input_event("Event 'new' on sink-input #123"),
            Some("123")
        );
        assert_eq!(
            parse_new_sink_input_event("Event 'change' on sink-input #123"),
            None
        );
        assert_eq!(parse_new_sink_input_event("Event 'new' on sink #4"), None);
        assert_eq!(
            parse_new_sink_input_event("Event 'new' on sink-input #"),
            None
        );
        assert_eq!(
            parse_new_sink_input_event("Event 'new' on sink-input #12x"),
            None
        );
    }

    #[test]
    fn follow_moves_each_new_sink_input_and_survives_move_failures() {
        let runner = ScriptedRunner::with_outputs(vec![Err("gone already"), Ok("")]);
        let lines = vec![
            "Event 'new' on sink-input #9".to_owned(),
            "Event 'change' on sink-input #9".to_owned(),
            "Event 'new' on source-output #4".to_owned(),
            "Event 'new' on sink-input #10".to_owned(),
        ];

        follow_new_sink_inputs(lines.into_iter(), runner.as_ref(), DEFAULT_REMOTE_SINK_NAME);

        let calls = runner.calls();
        assert_eq!(
            calls,
            vec![
                vec![
                    "pactl".to_owned(),
                    "move-sink-input".to_owned(),
                    "9".to_owned(),
                    DEFAULT_REMOTE_SINK_NAME.to_owned()
                ],
                vec![
                    "pactl".to_owned(),
                    "move-sink-input".to_owned(),
                    "10".to_owned(),
                    DEFAULT_REMOTE_SINK_NAME.to_owned()
                ],
            ]
        );
    }

    #[test]
    fn parser_extracts_default_sink_module_id_and_short_list_ids() {
        assert_eq!(
            parse_single_name("alsa_output\n"),
            Some("alsa_output".into())
        );
        assert_eq!(parse_module_id("42\n"), Some("42".into()));
        assert_eq!(
            parse_sink_input_routes("9\t122\tclient\n10\t364\tclient\n"),
            vec![
                SinkInputRoute {
                    input_id: "9".into(),
                    sink_id: "122".into(),
                },
                SinkInputRoute {
                    input_id: "10".into(),
                    sink_id: "364".into(),
                },
            ]
        );
        assert_eq!(
            parse_sink_id_by_name(
                "122\talsa_output\tPipeWire\n364\thypr_rdp_remote_audio\tPipeWire\n",
                DEFAULT_REMOTE_SINK_NAME
            ),
            Some("364".into())
        );
    }

    #[test]
    fn redirect_mode_creates_routes_and_restores_remote_sink() {
        let runner = ScriptedRunner::with_outputs(vec![
            Ok("alsa_output\n"),
            Ok("55\n"),
            Ok(""),
            Ok("364\thypr_rdp_remote_audio\tPipeWire\n"),
            Ok("9\t122\tclient\n10\t777\tclient\n"),
            Ok(""),
            Ok(""),
            Ok("hypr_rdp_remote_audio\n"),
            Ok(""),
            Ok("364\thypr_rdp_remote_audio\tPipeWire\n"),
            Ok("9\t364\tclient\n10\t364\tclient\n11\t364\tclient\n"),
            Ok(""),
            Ok(""),
            Ok(""),
            Ok(""),
        ]);
        let router = PipeWireRoutingRunner::with_runner(runner.clone());

        let guard = router.start(AudioMode::Redirect).unwrap().unwrap();
        drop(guard);

        let calls = runner.calls();
        assert_eq!(calls[0], vec!["pactl", "get-default-sink"]);
        assert_eq!(calls[1][..3], ["pactl", "load-module", "module-null-sink"]);
        assert_eq!(
            calls[2],
            vec!["pactl", "set-default-sink", DEFAULT_REMOTE_SINK_NAME]
        );
        assert_eq!(calls[3], vec!["pactl", "list", "short", "sinks"]);
        assert_eq!(calls[4], vec!["pactl", "list", "short", "sink-inputs"]);
        assert_eq!(
            calls[5],
            vec!["pactl", "move-sink-input", "9", DEFAULT_REMOTE_SINK_NAME]
        );
        assert_eq!(
            calls[6],
            vec!["pactl", "move-sink-input", "10", DEFAULT_REMOTE_SINK_NAME]
        );
        assert_eq!(calls[7], vec!["pactl", "get-default-sink"]);
        assert_eq!(calls[8], vec!["pactl", "set-default-sink", "alsa_output"]);
        assert_eq!(calls[9], vec!["pactl", "list", "short", "sinks"]);
        assert_eq!(calls[10], vec!["pactl", "list", "short", "sink-inputs"]);
        assert_eq!(calls[11], vec!["pactl", "move-sink-input", "9", "122"]);
        assert_eq!(calls[12], vec!["pactl", "move-sink-input", "10", "777"]);
        assert_eq!(
            calls[13],
            vec!["pactl", "move-sink-input", "11", "alsa_output"]
        );
        assert_eq!(calls[14], vec!["pactl", "unload-module", "55"]);
    }

    #[test]
    fn mirror_and_off_do_not_run_routing_commands() {
        let runner = ScriptedRunner::with_outputs(Vec::new());
        let router = PipeWireRoutingRunner::with_runner(runner.clone());

        assert!(router.start(AudioMode::Mirror).unwrap().is_none());
        assert!(router.start(AudioMode::Off).unwrap().is_none());
        assert!(runner.calls().is_empty());
    }

    #[test]
    fn production_runners_use_distinct_remote_sink_names() {
        let first = PipeWireRoutingRunner::new();
        let second = PipeWireRoutingRunner::new();

        assert!(first.sink_name.starts_with(DEFAULT_REMOTE_SINK_NAME));
        assert!(second.sink_name.starts_with(DEFAULT_REMOTE_SINK_NAME));
        assert_ne!(first.sink_name, second.sink_name);
    }

    #[test]
    fn redirect_restore_preserves_user_changed_default_sink() {
        let runner = ScriptedRunner::with_outputs(vec![
            Ok("alsa_output\n"),
            Ok("55\n"),
            Ok(""),
            Ok("364\thypr_rdp_remote_audio\tPipeWire\n"),
            Ok("9\t122\tclient\n"),
            Ok(""),
            Ok("usb_sink\n"),
            Ok("364\thypr_rdp_remote_audio\tPipeWire\n"),
            Ok("9\t364\tclient\n10\t364\tclient\n"),
            Ok(""),
            Ok(""),
            Ok(""),
        ]);
        let router = PipeWireRoutingRunner::with_runner(runner.clone());

        let guard = router.start(AudioMode::Redirect).unwrap().unwrap();
        drop(guard);

        let calls = runner.calls();
        assert_eq!(calls[0], vec!["pactl", "get-default-sink"]);
        assert_eq!(calls[1][..3], ["pactl", "load-module", "module-null-sink"]);
        assert_eq!(
            calls[2],
            vec!["pactl", "set-default-sink", DEFAULT_REMOTE_SINK_NAME]
        );
        assert_eq!(calls[3], vec!["pactl", "list", "short", "sinks"]);
        assert_eq!(calls[4], vec!["pactl", "list", "short", "sink-inputs"]);
        assert_eq!(
            calls[5],
            vec!["pactl", "move-sink-input", "9", DEFAULT_REMOTE_SINK_NAME]
        );
        assert_eq!(calls[6], vec!["pactl", "get-default-sink"]);
        assert_eq!(calls[7], vec!["pactl", "list", "short", "sinks"]);
        assert_eq!(calls[8], vec!["pactl", "list", "short", "sink-inputs"]);
        assert_eq!(calls[9], vec!["pactl", "move-sink-input", "9", "122"]);
        assert_eq!(
            calls[10],
            vec!["pactl", "move-sink-input", "10", "usb_sink"]
        );
        assert_eq!(calls[11], vec!["pactl", "unload-module", "55"]);
    }

    #[test]
    fn redirect_start_failure_restores_inputs_moved_before_failure() {
        let runner = ScriptedRunner::with_outputs(vec![
            Ok("alsa_output\n"),
            Ok("55\n"),
            Ok(""),
            Ok("364\thypr_rdp_remote_audio\tPipeWire\n"),
            Ok("9\t122\tclient\n10\t777\tclient\n"),
            Ok(""),
            Err("move failed"),
            Ok("hypr_rdp_remote_audio\n"),
            Ok(""),
            Ok("364\thypr_rdp_remote_audio\tPipeWire\n"),
            Ok("9\t364\tclient\n10\t777\tclient\n"),
            Ok(""),
            Ok(""),
        ]);
        let router = PipeWireRoutingRunner::with_runner(runner.clone());

        assert!(router.start(AudioMode::Redirect).is_err());

        let calls = runner.calls();
        assert_eq!(calls[0], vec!["pactl", "get-default-sink"]);
        assert_eq!(calls[1][..3], ["pactl", "load-module", "module-null-sink"]);
        assert_eq!(
            calls[2],
            vec!["pactl", "set-default-sink", DEFAULT_REMOTE_SINK_NAME]
        );
        assert_eq!(calls[3], vec!["pactl", "list", "short", "sinks"]);
        assert_eq!(calls[4], vec!["pactl", "list", "short", "sink-inputs"]);
        assert_eq!(
            calls[5],
            vec!["pactl", "move-sink-input", "9", DEFAULT_REMOTE_SINK_NAME]
        );
        assert_eq!(
            calls[6],
            vec!["pactl", "move-sink-input", "10", DEFAULT_REMOTE_SINK_NAME]
        );
        assert_eq!(calls[7], vec!["pactl", "get-default-sink"]);
        assert_eq!(calls[8], vec!["pactl", "set-default-sink", "alsa_output"]);
        assert_eq!(calls[9], vec!["pactl", "list", "short", "sinks"]);
        assert_eq!(calls[10], vec!["pactl", "list", "short", "sink-inputs"]);
        assert_eq!(calls[11], vec!["pactl", "move-sink-input", "9", "122"]);
        assert_eq!(calls[12], vec!["pactl", "unload-module", "55"]);
    }

    #[test]
    fn redirect_restore_treats_activation_remote_inputs_as_untracked() {
        let runner = ScriptedRunner::with_outputs(vec![
            Ok("alsa_output\n"),
            Ok("55\n"),
            Ok(""),
            Ok("364\thypr_rdp_remote_audio\tPipeWire\n"),
            Ok("9\t122\tclient\n10\t364\tclient\n"),
            Ok(""),
            Ok("hypr_rdp_remote_audio\n"),
            Ok(""),
            Ok("364\thypr_rdp_remote_audio\tPipeWire\n"),
            Ok("9\t364\tclient\n10\t364\tclient\n"),
            Ok(""),
            Ok(""),
            Ok(""),
        ]);
        let router = PipeWireRoutingRunner::with_runner(runner.clone());

        let guard = router.start(AudioMode::Redirect).unwrap().unwrap();
        drop(guard);

        let calls = runner.calls();
        assert_eq!(calls[0], vec!["pactl", "get-default-sink"]);
        assert_eq!(calls[1][..3], ["pactl", "load-module", "module-null-sink"]);
        assert_eq!(
            calls[2],
            vec!["pactl", "set-default-sink", DEFAULT_REMOTE_SINK_NAME]
        );
        assert_eq!(calls[3], vec!["pactl", "list", "short", "sinks"]);
        assert_eq!(calls[4], vec!["pactl", "list", "short", "sink-inputs"]);
        assert_eq!(
            calls[5],
            vec!["pactl", "move-sink-input", "9", DEFAULT_REMOTE_SINK_NAME]
        );
        assert_eq!(calls[6], vec!["pactl", "get-default-sink"]);
        assert_eq!(calls[7], vec!["pactl", "set-default-sink", "alsa_output"]);
        assert_eq!(calls[8], vec!["pactl", "list", "short", "sinks"]);
        assert_eq!(calls[9], vec!["pactl", "list", "short", "sink-inputs"]);
        assert_eq!(calls[10], vec!["pactl", "move-sink-input", "9", "122"]);
        assert_eq!(
            calls[11],
            vec!["pactl", "move-sink-input", "10", "alsa_output"]
        );
        assert_eq!(calls[12], vec!["pactl", "unload-module", "55"]);
    }

    #[test]
    fn redirect_start_failure_unloads_created_sink() {
        let runner = ScriptedRunner::with_outputs(vec![
            Ok("alsa_output\n"),
            Ok("55\n"),
            Err("set default failed"),
            Ok("alsa_output\n"),
            Ok(""),
            Ok(""),
        ]);
        let router = PipeWireRoutingRunner::with_runner(runner.clone());

        assert!(router.start(AudioMode::Redirect).is_err());

        let calls = runner.calls();
        assert_eq!(calls[0], vec!["pactl", "get-default-sink"]);
        assert_eq!(calls[1][..3], ["pactl", "load-module", "module-null-sink"]);
        assert_eq!(
            calls[2],
            vec!["pactl", "set-default-sink", DEFAULT_REMOTE_SINK_NAME]
        );
        assert_eq!(calls[3], vec!["pactl", "get-default-sink"]);
        assert_eq!(calls[4], vec!["pactl", "list", "short", "sinks"]);
        assert_eq!(calls[5], vec!["pactl", "unload-module", "55"]);
    }
}
