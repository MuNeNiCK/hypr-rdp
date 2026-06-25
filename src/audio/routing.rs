use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use anyhow::{bail, Context, Result};

use super::format::{BITS_PER_SAMPLE, CHANNELS, SAMPLE_RATE};

const DEFAULT_REMOTE_SINK_NAME: &str = "hypr_rdp_remote_audio";

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
}

impl PipeWireRoutingRunner {
    pub(super) fn new() -> Self {
        Self {
            command_runner: Arc::new(SystemCommandRunner),
            sink_name: next_remote_sink_name(),
        }
    }

    #[cfg(test)]
    pub(super) fn with_runner(command_runner: Arc<dyn RouteCommandRunner>) -> Self {
        Self {
            command_runner,
            sink_name: DEFAULT_REMOTE_SINK_NAME.to_owned(),
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
            restored: false,
        };

        if let Err(error) = guard.activate() {
            guard.restore();
            return Err(error);
        }

        Ok(guard)
    }
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

impl RouteCommandRunner for SystemCommandRunner {
    fn run(&self, program: &str, args: &[String]) -> Result<RouteCommandOutput> {
        let output = Command::new(program)
            .args(args)
            .output()
            .with_context(|| format!("failed to run {program}"))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("{} {} failed: {}", program, args.join(" "), stderr.trim());
        }

        Ok(RouteCommandOutput {
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        })
    }
}

struct RedirectRouteGuard {
    command_runner: Arc<dyn RouteCommandRunner>,
    sink_name: String,
    previous_default_sink: Option<String>,
    moved_sink_inputs: Vec<SinkInputRoute>,
    module_id: Option<String>,
    restored: bool,
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

        let current_default_sink = match default_sink(self.command_runner.as_ref()) {
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
                    self.command_runner.as_ref(),
                    &["set-default-sink".into(), previous_default_sink.into()],
                ) {
                    tracing::warn!("Audio: failed to restore default sink: {:#}", error);
                }
            }

            let fallback_sink = current_default_sink
                .as_deref()
                .filter(|current| *current != self.sink_name)
                .unwrap_or(previous_default_sink);
            self.restore_sink_inputs(Some(fallback_sink));
        } else {
            self.restore_sink_inputs(
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

    fn restore_sink_inputs(&self, fallback_sink: Option<&str>) {
        if let Err(error) = restore_sink_inputs_from_remote(
            self.command_runner.as_ref(),
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
