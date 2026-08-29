//! Hyprland IPC socket communication.
//!
//! Direct Unix socket communication instead of spawning hyprctl subprocesses.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::mpsc as std_mpsc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};

/// The command socket, which is what a probe asks and what `send_command`
/// then connects to. One name, so the two cannot drift apart.
const COMMAND_SOCKET: &str = ".socket.sock";
/// The event socket, which `EventStream` reads.
const EVENT_SOCKET: &str = ".socket2.sock";

/// Where an instance's command socket lives.
fn command_socket(hypr_dir: &str, signature: &str) -> String {
    socket_under(hypr_dir, signature, COMMAND_SOCKET)
}

/// What the environment named, if it named anything usable.
///
/// An empty value is the same as unset: a unit whose `Environment=` line lost
/// its expansion, or an exported-but-empty shell variable, both mean nothing.
fn env_signature_from(raw: Option<String>) -> Option<String> {
    raw.filter(|sig| !sig.is_empty())
}

/// `$XDG_RUNTIME_DIR`, with the same empty-means-unset rule as the signature:
/// a unit whose `Environment=` line lost its expansion would otherwise have
/// every path here start at `/hypr`.
fn runtime_dir() -> Result<String> {
    runtime_dir_from(std::env::var("XDG_RUNTIME_DIR").ok())
}

/// The rule, with the read passed in. Exported but empty is the case worth a
/// seam: a unit whose `Environment=` lost its expansion would otherwise start
/// every path at `/hypr` and be told that `/hypr` could not be read.
fn runtime_dir_from(raw: Option<String>) -> Result<String> {
    raw.filter(|dir| !dir.is_empty())
        .context("XDG_RUNTIME_DIR not set")
}

/// The instance to talk to.
///
/// Hyprland builds the signature from its commit hash, `std::time(nullptr)`
/// and a random number, so it is new on every start and cannot be written into
/// a unit. It copies the value into the user manager from `startCompositor`
/// and unsets it again from `cleanEnvironment`, so a unit started while
/// Hyprland is up does inherit a good one. It goes stale two ways all the
/// same: a process that outlives its Hyprland keeps the old value in its own
/// environment, which the manager cannot reach, and a Hyprland that dies
/// without running its shutdown -- `SIGKILL` or a crash, not a plain `SIGTERM`,
/// which it handles cleanly -- leaves the old value for the next unit. So
/// the variable being set is no evidence that it points at anything, and it
/// is checked like any other candidate.
/// Resolve the instance, with every ambient read passed in.
///
/// The cache comes in as a parameter because `INSTANCE` is a process-global
/// `OnceLock`: first writer wins for the life of the binary, so a test that
/// drove the real one would decide the value for every test after it.
fn instance_signature_in(
    cache: &std::sync::OnceLock<String>,
    runtime_dir: &str,
    raw_env: Option<String>,
    is_live: impl Fn(&str) -> Liveness,
) -> Result<String> {
    if let Some(sig) = cache.get() {
        return Ok(sig.clone());
    }

    // A resolution that failed transiently should not fail the caller when
    // another thread has already settled the question.
    let resolved = match resolve_in(runtime_dir, env_signature_from(raw_env), is_live) {
        Ok(resolved) => resolved,
        Err(error) => return cache.get().cloned().ok_or(error),
    };

    // Cache first, then say what was cached. Announcing before the write would
    // let a thread that lost the race report a signature nobody uses.
    let signature = cache.get_or_init(|| resolved.signature.clone()).clone();
    if signature != resolved.signature {
        return Ok(signature);
    }

    match &resolved.stale_env {
        Some(stale) => tracing::warn!(
            stale = %stale,
            instance = %resolved.signature,
            "HYPRLAND_INSTANCE_SIGNATURE names an instance that does not answer; using the live one"
        ),
        None if resolved.from_environment => {
            tracing::debug!(instance = %resolved.signature, "Using HYPRLAND_INSTANCE_SIGNATURE")
        }
        None => tracing::info!(
            instance = %resolved.signature,
            "HYPRLAND_INSTANCE_SIGNATURE is not set; using the live Hyprland instance"
        ),
    }

    Ok(signature)
}

/// Resolve against a given runtime directory, so the whole decision can be
/// driven from a test without touching the environment.
///
/// The directory is only read when the environment's signature does not
/// answer. Reading it first would make an unreadable `hypr/` fatal even for a
/// unit whose variable is perfectly good, which the plain environment lookup
/// this replaced never was.
fn resolve_in(
    runtime_dir: &str,
    from_env: Option<String>,
    is_live: impl Fn(&str) -> Liveness,
) -> Result<ResolvedInstance> {
    let dir = format!("{runtime_dir}/hypr");

    if let Some(sig) = from_env.as_deref() {
        match is_live(&command_socket(&dir, sig)) {
            Liveness::Answered => {
                return Ok(ResolvedInstance {
                    signature: sig.to_owned(),
                    from_environment: true,
                    stale_env: None,
                })
            }
            // Nothing was learned, so nothing has been shown to be wrong with
            // what the unit was given. Keeping it leaves the caller exactly
            // where it was before any of this existed -- the connection it is
            // about to make reports the real reason. Going looking instead
            // would let a full accept queue hand the session to a different
            // live desktop, which is the one outcome this must never produce.
            Liveness::Unknown => {
                tracing::warn!(
                    signature = sig,
                    "Could not tell whether the instance named by \
                     HYPRLAND_INSTANCE_SIGNATURE is live; using it anyway"
                );
                return Ok(ResolvedInstance {
                    signature: sig.to_owned(),
                    from_environment: true,
                    stale_env: None,
                });
            }
            Liveness::Gone => {}
        }
    }

    // The whole sentence is built here rather than glued from two contexts:
    // `main` prints anyhow's `Debug`, one cause per line, so a fragment
    // starting with "and" would be a line attached to nothing.
    let candidates = instance_candidates(&dir)
        .with_context(|| format!("{}, and {dir} could not be read", failure_intro(&from_env)))?;
    resolve_instance(&dir, from_env, candidates, is_live)
}

/// What one probe learned.
///
/// The distinction that matters is not live/dead but *whether we found out*.
/// Treating "I could not tell" as "dead" is what would let a full accept queue
/// or `EMFILE` declare a healthy instance stale -- and then, with a second
/// instance running, quietly move the session into the wrong desktop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Liveness {
    /// Someone is behind the socket.
    Answered,
    /// The instance is gone: the socket refused, or there is no socket.
    Gone,
    /// Nothing was learned, and the reason is about this process rather than
    /// that instance.
    Unknown,
}

/// Whether a failed probe says something about the instance rather than about
/// this process.
///
/// The socket of an instance that died refuses; a cleanly exited one is gone.
/// Anything else -- `EMFILE`, `EACCES` -- is ours, and is worth a line because
/// the failure text cannot tell the difference.
fn probe_error_is_ordinary(kind: std::io::ErrorKind) -> bool {
    matches!(
        kind,
        std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound
    )
}

/// How long one probe may take.
///
/// A `connect` to a unix socket on the same machine is microseconds when the
/// peer is there and immediate when it is not, so this is a ceiling, not a
/// budget. It exists because the one case that neither refuses nor succeeds --
/// a listener whose backlog is full, which the kernel makes `connect` sleep on
/// rather than fail -- would otherwise hang startup with no message at all.
const PROBE_TIMEOUT: Duration = Duration::from_secs(1);

/// Ask a socket whether anyone is behind it.
///
/// The connect happens on a helper thread so it can be abandoned: there is no
/// timed `connect` for a unix socket in `std`, and a probe that cannot be
/// given up is a probe that can wedge the process. An abandoned thread stays
/// in `connect` until the listener accepts -- the kernel will not time it out
/// -- and then hands the compositor a connection that says nothing and closes.
/// That costs one thread, one fd and one backlog slot each, which is the
/// cheaper of the two failures; the count is bounded because a resolution
/// either succeeds and is cached or fails and ends startup.
///
/// A refusal or a missing file is the ordinary answer for a leftover. Anything
/// else is about this process rather than that instance -- `EMFILE` above all,
/// which makes every probe fail at once. The failure text below cannot tell
/// the difference and says only that nothing answered, so this warning is the
/// one place the real reason appears.
fn probe(path: &str) -> Liveness {
    let target = path.to_owned();
    let (done, answer) = std_mpsc::channel();
    if std::thread::Builder::new()
        .name("hypr-probe".into())
        .spawn(move || {
            let _ = done.send(
                UnixStream::connect(&target)
                    .map(|_| ())
                    .map_err(|e| e.kind()),
            );
        })
        .is_err()
    {
        tracing::warn!(path, "Could not spawn a thread to probe a Hyprland socket");
        return Liveness::Unknown;
    }

    match answer.recv_timeout(PROBE_TIMEOUT) {
        Ok(Ok(())) => Liveness::Answered,
        Ok(Err(kind)) if probe_error_is_ordinary(kind) => Liveness::Gone,
        Ok(Err(kind)) => {
            tracing::warn!(path, ?kind, "Could not probe a Hyprland socket");
            Liveness::Unknown
        }
        Err(std_mpsc::RecvTimeoutError::Timeout) => {
            tracing::warn!(
                path,
                budget_ms = PROBE_TIMEOUT.as_millis(),
                "A Hyprland socket neither answered nor refused within its budget"
            );
            Liveness::Unknown
        }
        // The helper thread ended without sending, which it only does by
        // panicking. Nothing was learned about the instance either way.
        Err(std_mpsc::RecvTimeoutError::Disconnected) => {
            tracing::warn!(path, "The thread probing a Hyprland socket did not answer");
            Liveness::Unknown
        }
    }
}

/// The chosen instance, and the signature the environment gave if it turned
/// out to be dead.
#[derive(Debug)]
struct ResolvedInstance {
    signature: String,
    from_environment: bool,
    stale_env: Option<String>,
}

/// How every failure starts: what the environment said, so the reader is not
/// told about a directory while the cause is in their unit.
fn failure_intro(from_env: &Option<String>) -> String {
    match from_env.as_deref() {
        Some(sig) => format!("HYPRLAND_INSTANCE_SIGNATURE names {sig}, which does not answer"),
        None => "HYPRLAND_INSTANCE_SIGNATURE is not set".to_owned(),
    }
}

/// Choose the one live instance among the directories.
///
/// Being the only directory is not evidence of anything. Hyprland leaves the
/// directory it made behind on every exit path: a clean exit removes
/// `hyprland.lock` and, from `CHyprCtl`'s destructor, `.socket.sock` -- and a
/// `SIGKILL` or a crash removes neither, while `.socket2.sock` and the log
/// survive either way, so a directory never becomes empty. Three restarts
/// measured four directories, ten by the end of testing.
///
/// That also rules out the two cheap tests. Such an instance keeps its lock
/// file, so the pid in it is not evidence -- hyprctl only asks `kill(pid, 0)`
/// and never that the pid is a Hyprland, so any live process at that number
/// passes, which was reproduced with planted locks. A pid owned by another
/// user is kept as well, since that call fails with `EPERM` rather than
/// `ESRCH`. It keeps its socket inode too.
///
/// Connecting to the command socket is evidence: the socket of an instance
/// that died refuses, and a cleanly exited one is gone.
fn resolve_instance(
    dir: &str,
    from_env: Option<String>,
    candidates: Vec<String>,
    is_live: impl Fn(&str) -> Liveness,
) -> Result<ResolvedInstance> {
    let intro = failure_intro(&from_env);
    let total = candidates.len();
    let mut live: Vec<String> = Vec::new();
    let mut unknown = 0usize;

    for sig in candidates {
        // The caller probed this one already and it was gone; probing it again
        // only doubles the worst case when probes are slow.
        if from_env.as_deref() == Some(sig.as_str()) {
            continue;
        }
        match is_live(&command_socket(dir, &sig)) {
            Liveness::Answered => live.push(sig),
            Liveness::Gone => {}
            Liveness::Unknown => unknown += 1,
        }
    }

    match live.len() {
        1 => Ok(ResolvedInstance {
            signature: live.remove(0),
            from_environment: false,
            stale_env: from_env,
        }),
        0 if total == 0 => bail!(
            "{intro}, and {dir} holds no Hyprland instance directory; \
             is Hyprland running as this user?"
        ),
        // Blaming leftover directories is only fair when they were actually
        // asked. If some probe could not be made, the cause is here, and the
        // warning it logged is the thing to read.
        0 if unknown > 0 => bail!(
            "{intro}, and {unknown} of the {total} instance directories in {dir} could not be \
             probed at all; the reason was logged, and it is about this process rather than \
             about Hyprland"
        ),
        0 => bail!(
            "{intro}, and none of the {total} instance directories in {dir} answers on its \
             command socket; Hyprland leaves one behind on every exit, so they may be old"
        ),
        live_count => {
            // Sorted here rather than at the source: this is the only place the
            // order becomes visible, and a reader comparing two runs should be
            // shown the same list twice.
            live.sort();
            bail!(
                "{intro}, and {live_count} Hyprland instances are live in {dir} ({}); \
                 set HYPRLAND_INSTANCE_SIGNATURE to choose one",
                live.join(", ")
            )
        }
    }
}

/// The signature this process resolved, for a child that needs to agree with
/// it -- and only if it has already been resolved.
///
/// Deliberately does not resolve: a hook is not a reason to go looking, and a
/// getter that did would make the test suite scan the developer's runtime
/// directory and open a socket to their live compositor as a side effect of
/// `cargo test`.
pub(crate) fn resolved_instance() -> Option<String> {
    INSTANCE.get().cloned()
}

/// Seed the process-wide instance so a test can drive the real path that reads
/// it. First writer wins, so exactly one test may call this.
#[cfg(test)]
pub(crate) fn remember_instance_for_test(signature: &str) {
    let _ = INSTANCE.set(signature.to_owned());
}

/// The instance resolved for this process, decided once.
///
/// Once is deliberate. Re-resolving after a failure would let a server bound
/// to one session migrate into another live one behind the user's back --
/// creating and removing headless outputs in a desktop nobody pointed it at --
/// and it would buy nothing: losing the compositor ends the RDP session, and
/// the input handler's Wayland connection is made once in `server::setup` and
/// is not re-established, so a Hyprland restart needs the server restarted
/// whatever the signature says.
static INSTANCE: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// Every directory name under `$XDG_RUNTIME_DIR/hypr`.
///
/// In whatever order the filesystem gives them: nothing here depends on it,
/// and the one message that lists names sorts them itself.
fn instance_candidates(dir: &str) -> Result<Vec<String>> {
    let entries = std::fs::read_dir(dir)?;
    Ok(entries
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().is_ok_and(|t| t.is_dir()))
        .filter_map(|entry| entry.file_name().into_string().ok())
        .collect())
}

/// The path to one of an instance's sockets, from the one signature this
/// process resolved.
fn socket_path_in(runtime_dir: &str, signature: &str, name: &str) -> String {
    socket_under(&format!("{runtime_dir}/hypr"), signature, name)
}

/// The one place a socket path is assembled. Everything that builds one goes
/// through here, so the command socket and the event socket cannot end up
/// pointing at different instances.
fn socket_under(hypr_dir: &str, signature: &str, name: &str) -> String {
    format!("{hypr_dir}/{signature}/{name}")
}

/// The same, resolving the instance first. There is no second way to build one
/// of these paths, so the command socket and the event socket are always the
/// two sockets of one instance.
fn instance_socket(name: &str) -> Result<String> {
    instance_socket_ambient(&INSTANCE, name)
}

/// The same against a caller-supplied cache, so a test can drive both ambient
/// reads -- `XDG_RUNTIME_DIR` and `HYPRLAND_INSTANCE_SIGNATURE` -- without
/// writing into the process-global one.
fn instance_socket_ambient(cache: &std::sync::OnceLock<String>, name: &str) -> Result<String> {
    let runtime_dir = runtime_dir()?;
    instance_socket_in(
        cache,
        &runtime_dir,
        std::env::var("HYPRLAND_INSTANCE_SIGNATURE").ok(),
        probe,
        name,
    )
}

/// The body of `instance_socket`, with every ambient read passed in.
fn instance_socket_in(
    cache: &std::sync::OnceLock<String>,
    runtime_dir: &str,
    raw_env: Option<String>,
    is_live: impl Fn(&str) -> Liveness,
    name: &str,
) -> Result<String> {
    let signature = instance_signature_in(cache, runtime_dir, raw_env, is_live)?;
    Ok(socket_path_in(runtime_dir, &signature, name))
}

/// Send a raw command to Hyprland IPC socket and return the response.
fn send_command(cmd: &str) -> Result<String> {
    let path = instance_socket(COMMAND_SOCKET)?;
    let mut sock = UnixStream::connect(&path)
        .with_context(|| format!("failed to connect to Hyprland socket: {}", path))?;
    sock.set_read_timeout(Some(Duration::from_secs(3)))?;
    sock.write_all(cmd.as_bytes())?;
    sock.shutdown(std::net::Shutdown::Write)?;
    let mut response = String::new();
    sock.read_to_string(&mut response)?;
    Ok(response)
}

/// Send a command that expects "ok" response (dispatch, keyword, output).
fn send_action(cmd: &str) -> Result<()> {
    let response = send_command(cmd)?;
    if response.starts_with("ok") || response.trim().is_empty() {
        Ok(())
    } else {
        bail!("Hyprland IPC error: {}", response.trim())
    }
}

/// Query monitors as JSON value (array).
pub fn monitors() -> Result<serde_json::Value> {
    let response = send_command("j/monitors")?;
    serde_json::from_str(&response).context("failed to parse Hyprland monitors JSON")
}

/// Query input devices as JSON value (object with a "keyboards" array).
pub fn devices() -> Result<serde_json::Value> {
    let response = send_command("j/devices")?;
    serde_json::from_str(&response).context("failed to parse Hyprland devices JSON")
}

/// Query a Hyprland option string value.
pub fn option_string(option: &str) -> Result<Option<String>> {
    let response = send_command(&format!("j/getoption {}", option))?;
    option_string_from_response(&response)
        .with_context(|| format!("failed to parse Hyprland option {}", option))
}

fn option_string_from_response(response: &str) -> Result<Option<String>> {
    let value: serde_json::Value =
        serde_json::from_str(response).context("failed to parse Hyprland option JSON")?;
    Ok(value
        .get("str")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned))
}

/// Create a headless output with a custom name prefix.
/// Hyprland will name it `{name}-1`, `{name}-2`, etc.
pub fn output_create_headless(name: &str) -> Result<()> {
    send_action(&format!("output create headless {}", name))
}

/// Set a monitor rule (e.g. "HEADLESS-1,1920x1080@60,-9999x0,1").
///
/// Hyprland's new (Lua) config parser rejects `keyword` with
/// "keyword can't work with non-legacy parsers. Use eval."
/// In that case, retry as `eval hl.monitor({...})`.
pub fn keyword_monitor(rule: &str) -> Result<()> {
    match send_action(&format!("keyword monitor {}", rule)) {
        Ok(()) => Ok(()),
        Err(e) if is_non_legacy_parser_error(&e) => {
            send_action(&format!("eval {}", monitor_rule_to_lua(rule)?))
        }
        Err(e) => Err(e),
    }
}

fn is_non_legacy_parser_error(error: &anyhow::Error) -> bool {
    error
        .chain()
        .any(|cause| cause.to_string().contains("non-legacy parsers"))
}

fn monitor_rule_to_lua(rule: &str) -> Result<String> {
    let parts: Vec<&str> = rule.splitn(4, ',').map(str::trim).collect();
    let [output, mode, position, scale] = parts.as_slice() else {
        bail!(
            "cannot translate monitor rule to Lua (expected 4 fields): {}",
            rule
        );
    };

    Ok(format!(
        "hl.monitor({{ output = {}, mode = {}, position = {}, scale = {} }})",
        lua_string(output)?,
        lua_string(mode)?,
        lua_string(position)?,
        monitor_scale_to_lua(scale)?
    ))
}

fn monitor_scale_to_lua(scale: &str) -> Result<String> {
    if scale.is_empty() {
        bail!("cannot translate monitor rule to Lua with empty scale");
    }

    if let Ok(value) = scale.parse::<f64>() {
        if value.is_finite() {
            return Ok(scale.to_string());
        }
    }

    lua_string(scale)
}

fn lua_string(value: &str) -> Result<String> {
    let mut escaped = String::with_capacity(value.len() + 2);
    escaped.push('"');

    for ch in value.chars() {
        match ch {
            '\\' => escaped.push_str("\\\\"),
            '"' => escaped.push_str("\\\""),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            ch if ch.is_control() => bail!("cannot encode control character in Lua string"),
            ch => escaped.push(ch),
        }
    }

    escaped.push('"');
    Ok(escaped)
}

/// Remove a named output.
pub fn output_remove(name: &str) -> Result<()> {
    send_action(&format!("output remove {}", name))
}

/// Event stream from Hyprland socket2 (subscription-based).
///
/// Connect before triggering the action to avoid missing events.
/// After connecting, call `ensure_registered()` to guarantee Hyprland
/// has accepted the connection before emitting events.
pub struct EventStream {
    sock: UnixStream,
    buf: Vec<u8>,
}

impl EventStream {
    pub fn connect() -> Result<Self> {
        let path = instance_socket(EVENT_SOCKET)?;
        let sock = UnixStream::connect(&path)
            .with_context(|| format!("failed to connect to Hyprland event socket: {}", path))?;
        sock.set_read_timeout(Some(Duration::from_millis(500)))?;
        Ok(Self {
            sock,
            buf: Vec::new(),
        })
    }

    /// Force a socket1 roundtrip so Hyprland's event loop processes our
    /// socket2 accept() before we trigger any actions.
    pub fn ensure_registered(&self) -> Result<()> {
        let _ = monitors()?;
        Ok(())
    }

    /// Return the next `EVENT>>DATA` pair, or `None` when `timeout` elapses
    /// without one. Errors only when the socket itself fails, so callers can
    /// tell an idle stream from a dead one.
    pub fn next_event(&mut self, timeout: Duration) -> Result<Option<(String, String)>> {
        let start = Instant::now();
        let mut raw = [0u8; 4096];

        loop {
            if let Some(newline_pos) = self.buf.iter().position(|&byte| byte == b'\n') {
                let line: Vec<u8> = self.buf.drain(..=newline_pos).collect();
                // Decode only after framing: a multi-byte character can
                // straddle two socket reads.
                let line = String::from_utf8_lossy(&line[..newline_pos]);
                let line = line.trim();
                if let Some((event, data)) = line.split_once(">>") {
                    return Ok(Some((event.to_string(), data.to_string())));
                }
                continue;
            }

            if start.elapsed() >= timeout {
                return Ok(None);
            }

            match self.sock.read(&mut raw) {
                Ok(0) => bail!("Hyprland event socket closed"),
                Ok(n) => {
                    self.buf.extend_from_slice(&raw[..n]);
                }
                Err(e)
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::TimedOut =>
                {
                    continue;
                }
                Err(e) => return Err(e).context("failed to read Hyprland event"),
            }
        }
    }

    /// Wait for an event matching `event_name` (e.g. "monitoradded").
    /// Returns the event data (text after ">>").
    pub fn wait_for(&mut self, event_name: &str, timeout: Duration) -> Result<String> {
        let start = Instant::now();
        let mut raw = [0u8; 4096];

        loop {
            if start.elapsed() >= timeout {
                bail!(
                    "timed out waiting for '{}' event after {}ms",
                    event_name,
                    timeout.as_millis()
                );
            }

            // Check buffered lines first
            while let Some(newline_pos) = self.buf.iter().position(|&byte| byte == b'\n') {
                let line: Vec<u8> = self.buf.drain(..=newline_pos).collect();
                let line = String::from_utf8_lossy(&line[..newline_pos]);
                let line = line.trim();
                if let Some((event, data)) = line.split_once(">>") {
                    if event == event_name {
                        return Ok(data.to_string());
                    }
                }
            }

            // Read more data from socket
            match self.sock.read(&mut raw) {
                Ok(0) => bail!("Hyprland event socket closed"),
                Ok(n) => {
                    self.buf.extend_from_slice(&raw[..n]);
                }
                Err(e)
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::TimedOut =>
                {
                    continue;
                }
                Err(e) => return Err(e).context("failed to read Hyprland event"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        command_socket, env_signature_from, instance_candidates, instance_signature_in,
        instance_socket_ambient, instance_socket_in, probe, probe_error_is_ordinary, resolve_in,
        resolve_instance, runtime_dir_from, socket_path_in, Liveness, COMMAND_SOCKET, EVENT_SOCKET,
        PROBE_TIMEOUT,
    };

    /// The four signatures below are the ones a VM actually produced across
    /// three restarts, digits and all. They share a commit hash and differ in
    /// the timestamp that follows it, so here name order happens to match age
    /// order and the live one sorts last -- taking the first name, what the
    /// issue proposed, picks a corpse. That coincidence is not a rule to lean
    /// on: the hash leads the name, so after an upgrade a new instance can
    /// sort before old ones.
    const LEFTOVERS: [&str; 4] = [
        "efb50993780079460b0cbed1363e2166a2de1d9f_1787993619_455098395",
        "efb50993780079460b0cbed1363e2166a2de1d9f_1787993997_663304392",
        "efb50993780079460b0cbed1363e2166a2de1d9f_1787994004_1191101925",
        "efb50993780079460b0cbed1363e2166a2de1d9f_1787994011_1368409244",
    ];

    /// A probe that reached its answer, phrased the way the tests care about:
    /// answered or gone. `Liveness::Unknown` is always written out in full,
    /// because a test that means "we could not tell" should say so.
    fn told(answered: bool) -> Liveness {
        if answered {
            Liveness::Answered
        } else {
            Liveness::Gone
        }
    }

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_string()).collect()
    }

    /// Only one of them answers, and it is not the one a name sort would reach
    /// first.
    #[test]
    fn the_live_instance_is_found_among_the_leftovers() {
        let live = command_socket("/run/hypr", LEFTOVERS[3]);

        let picked = resolve_instance("/run/hypr", None, names(&LEFTOVERS), |path| {
            told(path == live)
        })
        .expect("one live instance");

        assert_eq!(picked.signature, LEFTOVERS[3]);
        assert!(picked.stale_env.is_none());
        assert!(!picked.from_environment);
    }

    /// The environment is read for real here, so these must not run beside
    /// another test that touches the same two variables.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// The case the budget exists for, built rather than imagined: a listener
    /// whose accept queue is full makes `connect` sleep in the kernel instead
    /// of failing, so a wedged compositor is indistinguishable from a healthy
    /// one until the budget runs out. `Unknown` is the answer that matters --
    /// `Answered` would pick the wedged instance and hang every later call,
    /// and `Gone` would declare a live compositor stale.
    #[test]
    fn a_wedged_socket_is_undecidable_within_the_budget() {
        use std::os::fd::AsRawFd;

        let dir = TempDir::short("hrw");
        let socket = dir.0.join("s.sock");
        let path = socket.to_str().expect("utf-8").to_owned();

        let listener = std::os::unix::net::UnixListener::bind(&socket).expect("bind");
        // Smallest backlog the kernel will take, then fill it. Nothing ever
        // accepts, so the next connect has nowhere to go.
        assert_eq!(unsafe { libc::listen(listener.as_raw_fd(), 0) }, 0);
        let _queued = std::os::unix::net::UnixStream::connect(&socket);

        let (done, answer) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = done.send(probe(&path));
        });

        // A fixed ceiling, not one derived from the budget: a budget that grew
        // should fail here rather than make the suite hang for as long as it
        // was grown to.
        assert!(
            PROBE_TIMEOUT <= std::time::Duration::from_secs(2),
            "the budget is a startup stall a user waits through: {PROBE_TIMEOUT:?}"
        );
        let answer = answer
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("probe must give up on its own; the budget is the whole point");
        assert_eq!(answer, Liveness::Unknown);
    }

    /// An exported but empty `XDG_RUNTIME_DIR` is not a runtime directory. It
    /// is what a unit whose `Environment=` lost its expansion has, and treating
    /// it as one starts every path at `/hypr` and blames `/hypr`.
    #[test]
    fn an_empty_runtime_directory_counts_as_unset() {
        assert!(runtime_dir_from(None).is_err());
        assert!(runtime_dir_from(Some(String::new())).is_err());
        assert_eq!(
            runtime_dir_from(Some("/run/user/1000".into())).expect("a real one"),
            "/run/user/1000"
        );
    }

    /// The empty-means-unset rule, exercised where it is applied rather than
    /// where it is defined. With the rule bypassed the first probe goes to
    /// `.../hypr//.socket.sock`, which on a live instance is a real path.
    #[test]
    fn an_empty_variable_is_unset_all_the_way_through() {
        let runtime = TempDir::new("empty-var");
        let hypr = runtime.0.join("hypr");
        std::fs::create_dir(&hypr).expect("hypr");
        std::fs::create_dir(hypr.join("live_one")).expect("instance");
        let live = command_socket(hypr.to_str().expect("utf-8"), "live_one");
        let probed = std::sync::Mutex::new(Vec::new());

        let cache = std::sync::OnceLock::new();
        let signature =
            instance_signature_in(&cache, runtime.path(), Some(String::new()), |path| {
                probed.lock().expect("probed").push(path.to_owned());
                told(path == live)
            })
            .expect("an empty variable is no variable");

        assert_eq!(signature, "live_one");
        assert_eq!(
            probed.into_inner().expect("probed"),
            vec![live],
            "an empty signature must never be turned into a path"
        );
    }

    /// The documented escape hatch, end to end through the ambient reads. Two
    /// live sessions, and the variable is the only thing that chooses -- which
    /// is exactly what the several-live failure tells the user to do.
    #[test]
    fn the_variable_still_chooses_between_two_live_sessions() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let runtime = TempDir::short("hre");
        let hypr = runtime.0.join("hypr");
        std::fs::create_dir(&hypr).expect("hypr");

        let mut listeners = Vec::new();
        for signature in ["one", "two"] {
            let instance = hypr.join(signature);
            std::fs::create_dir(&instance).expect("instance");
            listeners.push(
                std::os::unix::net::UnixListener::bind(instance.join(COMMAND_SOCKET))
                    .expect("bind"),
            );
        }

        let previous_runtime = std::env::var("XDG_RUNTIME_DIR").ok();
        let previous_signature = std::env::var("HYPRLAND_INSTANCE_SIGNATURE").ok();
        std::env::set_var("XDG_RUNTIME_DIR", runtime.path());
        std::env::set_var("HYPRLAND_INSTANCE_SIGNATURE", "two");

        let cache = std::sync::OnceLock::new();
        let chosen = instance_socket_ambient(&cache, COMMAND_SOCKET);

        std::env::set_var("XDG_RUNTIME_DIR", "");
        let empty_runtime = instance_socket_ambient(&std::sync::OnceLock::new(), COMMAND_SOCKET);

        match previous_runtime {
            Some(value) => std::env::set_var("XDG_RUNTIME_DIR", value),
            None => std::env::remove_var("XDG_RUNTIME_DIR"),
        }
        match previous_signature {
            Some(value) => std::env::set_var("HYPRLAND_INSTANCE_SIGNATURE", value),
            None => std::env::remove_var("HYPRLAND_INSTANCE_SIGNATURE"),
        }

        assert_eq!(
            chosen.expect("the variable names a live instance"),
            command_socket(hypr.to_str().expect("utf-8"), "two"),
            "the variable is the user's only way to pick a session; it must be read"
        );
        assert!(
            format!("{:#}", empty_runtime.expect_err("empty is unset"))
                .contains("XDG_RUNTIME_DIR not set"),
            "an empty runtime directory must be named as such"
        );
    }

    /// On the success path a stale variable changes nothing the caller can see
    /// except this warning, so the warning is the whole feature: it is the only
    /// thing that tells a user the value baked into their unit is wrong.
    #[test]
    fn a_stale_variable_is_warned_about_by_name() {
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

        let runtime = TempDir::new("stale-warn");
        let hypr = runtime.0.join("hypr");
        std::fs::create_dir(&hypr).expect("hypr");
        std::fs::create_dir(hypr.join("live_one")).expect("instance");
        let live = command_socket(hypr.to_str().expect("utf-8"), "live_one");

        let buffer = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::fmt()
            .with_writer(Sink(buffer.clone()))
            .with_ansi(false)
            .finish();

        let cache = std::sync::OnceLock::new();
        tracing::subscriber::with_default(subscriber, || {
            instance_signature_in(&cache, runtime.path(), Some("stale_one".into()), |path| {
                told(path == live)
            })
            .expect("falls back to the live one");
        });

        let logged = String::from_utf8(buffer.lock().expect("sink").clone()).expect("utf-8");
        for needle in ["WARN", "stale_one", "live_one", "does not answer"] {
            assert!(logged.contains(needle), "{needle:?} missing from: {logged}");
        }
    }

    /// A probe that could not be made says nothing about the instance. The
    /// variable is what the unit was given and nothing has contradicted it, so
    /// it is kept and the connection that follows reports the real reason --
    /// which is where the caller stood before any of this existed.
    #[test]
    fn a_probe_that_learned_nothing_leaves_the_variable_alone() {
        let runtime = TempDir::new("undecided");

        let picked = resolve_in(runtime.path(), Some(LEFTOVERS[0].to_string()), |_| {
            Liveness::Unknown
        })
        .expect("an undecidable probe must not be a startup failure");

        assert_eq!(picked.signature, LEFTOVERS[0]);
        assert!(picked.from_environment);
        assert!(picked.stale_env.is_none());
    }

    /// The case that made this worth distinguishing. A stalled compositor --
    /// `connect` sleeps on a full accept queue rather than failing -- would,
    /// if "could not tell" counted as dead, hand the session to whichever
    /// other desktop happened to answer. The server would then create headless
    /// outputs in a session nobody pointed it at.
    #[test]
    fn an_undecidable_probe_does_not_migrate_to_another_live_instance() {
        let runtime = TempDir::new("no-migrate");
        let hypr = runtime.0.join("hypr");
        std::fs::create_dir(&hypr).expect("hypr");
        for signature in [LEFTOVERS[0], LEFTOVERS[3]] {
            std::fs::create_dir(hypr.join(signature)).expect("instance");
        }
        let other = command_socket(hypr.to_str().expect("utf-8"), LEFTOVERS[3]);

        let picked = resolve_in(runtime.path(), Some(LEFTOVERS[0].to_string()), |path| {
            if path == other {
                Liveness::Answered
            } else {
                Liveness::Unknown
            }
        })
        .expect("the variable is kept");

        assert_eq!(
            picked.signature, LEFTOVERS[0],
            "an unanswered probe must never move the session to another desktop"
        );
        assert!(picked.from_environment);
    }

    /// Blaming leftover directories is only fair when they were asked. If the
    /// probes could not be made, the cause is in this process, and a message
    /// about old directories would send the reader the wrong way.
    #[test]
    fn directories_that_could_not_be_probed_are_not_blamed_on_leftovers() {
        let err = resolve_instance("/run/hypr", None, names(&LEFTOVERS[..3]), |_| {
            Liveness::Unknown
        })
        .expect_err("nothing answered");

        let text = format!("{err:#}");
        assert!(
            text.contains("could not be probed at all"),
            "unexpected message: {text}"
        );
        assert!(
            text.contains("about this process rather than about Hyprland"),
            "the message must not blame Hyprland: {text}"
        );
        assert!(
            !text.contains("they may be old"),
            "leftovers were not shown to be the cause: {text}"
        );
    }

    /// The signature from the environment is probed once. It is also one of the
    /// directories, and re-probing it doubles the worst case in exactly the
    /// slow-probe case the budget exists for.
    #[test]
    fn the_signature_from_the_environment_is_not_probed_twice() {
        let runtime = TempDir::new("probe-once");
        let hypr = runtime.0.join("hypr");
        std::fs::create_dir(&hypr).expect("hypr");
        for signature in [LEFTOVERS[0], LEFTOVERS[3]] {
            std::fs::create_dir(hypr.join(signature)).expect("instance");
        }
        let stale = command_socket(hypr.to_str().expect("utf-8"), LEFTOVERS[0]);
        let live = command_socket(hypr.to_str().expect("utf-8"), LEFTOVERS[3]);
        let probes = std::sync::Mutex::new(Vec::new());

        let picked = resolve_in(runtime.path(), Some(LEFTOVERS[0].to_string()), |path| {
            probes.lock().expect("probes").push(path.to_owned());
            told(path == live)
        })
        .expect("falls back to the live one");

        assert_eq!(picked.signature, LEFTOVERS[3]);
        let probes = probes.into_inner().expect("probes");
        assert_eq!(
            probes.iter().filter(|path| **path == stale).count(),
            1,
            "the stale signature was probed more than once: {probes:?}"
        );
    }

    /// The variable is not evidence. A process that outlives its Hyprland
    /// keeps the old value, and a Hyprland that died leaves one in the user
    /// manager for the next unit -- so a set value is checked like any other
    /// candidate, and the live one is used instead.
    #[test]
    fn a_signature_from_the_environment_is_checked_like_any_other() {
        let runtime = TempDir::new("stale-env");
        let hypr = runtime.0.join("hypr");
        std::fs::create_dir(&hypr).expect("hypr");
        std::fs::create_dir(hypr.join(LEFTOVERS[3])).expect("instance");
        let live = command_socket(hypr.to_str().expect("utf-8"), LEFTOVERS[3]);

        let picked = resolve_in(runtime.path(), Some(LEFTOVERS[0].to_string()), |path| {
            told(path == live)
        })
        .expect("falls back to the live one");

        assert_eq!(picked.signature, LEFTOVERS[3]);
        assert_eq!(picked.stale_env.as_deref(), Some(LEFTOVERS[0]));
        assert!(!picked.from_environment);
    }

    /// One directory and nothing behind it is the ordinary state after a
    /// single crash, and it is not the same as never having run Hyprland.
    #[test]
    fn a_single_stale_directory_is_not_reported_as_no_hyprland() {
        let err = resolve_instance("/run/hypr", None, names(&LEFTOVERS[..1]), |_| {
            Liveness::Gone
        })
        .expect_err("nothing answers");

        let message = format!("{err}");
        assert!(message.contains("1 instance director"), "{message}");
        assert!(message.contains("may be old"), "{message}");
        assert!(
            !message.contains("holds no Hyprland instance"),
            "a leftover is not an absence: {message}"
        );
    }

    /// A dead instance keeps its socket inode when it did not exit cleanly, so
    /// the
    /// probe -- not the presence of a file -- is what rejects them.
    #[test]
    fn instances_that_do_not_answer_are_refused_however_many_there_are() {
        let err = resolve_instance("/run/hypr", None, names(&LEFTOVERS[..3]), |_| {
            Liveness::Gone
        })
        .expect_err("nothing answers");

        let message = format!("{err}");
        assert!(message.contains("3 instance director"), "{message}");
        assert!(message.contains("may be old"), "{message}");
        assert!(message.contains("HYPRLAND_INSTANCE_SIGNATURE"), "{message}");
        assert!(message.contains("/run/hypr"), "{message}");
    }

    /// Nothing to choose from reads differently from "all of them are stale":
    /// the first means Hyprland is not running, the second means it was.
    #[test]
    fn an_empty_runtime_directory_says_hyprland_is_not_running() {
        let err = resolve_instance("/run/hypr", None, Vec::new(), |_| unreachable!())
            .expect_err("no candidates");

        let message = format!("{err}");
        assert!(message.contains("holds no Hyprland instance"), "{message}");
        assert!(message.contains("HYPRLAND_INSTANCE_SIGNATURE"), "{message}");
        assert!(message.contains("/run/hypr"), "{message}");
    }

    /// Two live sessions is a real configuration, and guessing between them
    /// would attach the server to whichever the filesystem listed first.
    #[test]
    fn two_live_instances_are_named_rather_than_guessed_between() {
        // Four directories, two of them live: the message must count the live
        // ones, not the candidates. Handed over in reverse, so a message that
        // merely echoed the input order would name them the other way round.
        let live = [LEFTOVERS[0], LEFTOVERS[1]];
        let mut candidates = names(&LEFTOVERS);
        candidates.reverse();

        let err = resolve_instance("/run/hypr", None, candidates, |path| {
            told(
                live.iter()
                    .any(|sig| path == command_socket("/run/hypr", sig)),
            )
        })
        .expect_err("ambiguous");

        let message = format!("{err}");
        assert!(
            message.contains("2 Hyprland instances are live"),
            "{message}"
        );
        assert!(
            message.contains(&format!("{}, {}", LEFTOVERS[0], LEFTOVERS[1])),
            "both must be named, in order: {message}"
        );
        assert!(message.contains("/run/hypr"), "{message}");
        assert!(
            message.contains("set HYPRLAND_INSTANCE_SIGNATURE"),
            "{message}"
        );
    }

    /// A stale variable must reach the failure text too, or the reader is left
    /// with a message about a directory while the cause is in their unit.
    #[test]
    fn a_stale_signature_from_the_environment_is_named_in_the_failure() {
        let err = resolve_instance(
            "/run/hypr",
            Some(LEFTOVERS[0].to_string()),
            names(&LEFTOVERS[..2]),
            |_| Liveness::Gone,
        )
        .expect_err("nothing answers");

        let message = format!("{err}");
        assert!(message.contains(LEFTOVERS[0]), "{message}");
        assert!(message.contains("does not answer"), "{message}");
    }

    /// The path a probe asks about has to be the path the caller then connects
    /// to. They are built by different functions, so nothing but this ties
    /// them together -- and a probe aimed somewhere else would report liveness
    /// for an instance we cannot talk to.
    #[test]
    fn the_probed_path_is_the_path_that_gets_connected() {
        let runtime = "/run/user/1000";
        let dir = format!("{runtime}/hypr");

        assert_eq!(
            command_socket(&dir, "sig"),
            socket_path_in(runtime, "sig", COMMAND_SOCKET)
        );
        assert_ne!(
            socket_path_in(runtime, "sig", COMMAND_SOCKET),
            socket_path_in(runtime, "sig", EVENT_SOCKET),
            "the command socket and the event socket are different files"
        );
    }

    /// The socket a caller connects to must be built from the instance that was
    /// resolved, not from whatever the environment happens to say. Both names
    /// go through one function, so both sockets always belong to one instance.
    #[test]
    fn a_socket_path_is_built_from_the_resolved_instance() {
        let cache = std::sync::OnceLock::new();
        cache.set("resolved-sig".to_string()).expect("fresh");

        let command = instance_socket_in(
            &cache,
            "/run/user/1000",
            Some("something-else-entirely".into()),
            |_| unreachable!("the cache answers"),
            COMMAND_SOCKET,
        )
        .expect("resolved");
        let events = instance_socket_in(
            &cache,
            "/run/user/1000",
            None,
            |_| unreachable!(),
            EVENT_SOCKET,
        )
        .expect("resolved");

        assert_eq!(command, "/run/user/1000/hypr/resolved-sig/.socket.sock");
        assert_eq!(events, "/run/user/1000/hypr/resolved-sig/.socket2.sock");
    }

    /// The cache is consulted before anything else, and nothing is looked up
    /// when it answers.
    #[test]
    fn a_cached_instance_short_circuits_everything() {
        let cache = std::sync::OnceLock::new();
        cache.set("already-known".to_string()).expect("fresh");

        let sig = instance_signature_in(&cache, "/nonexistent", Some("ignored".into()), |_| {
            unreachable!("nothing may be probed once the cache holds a value")
        })
        .expect("cached");

        assert_eq!(sig, "already-known");
    }

    /// What is resolved is what is remembered, so the second call cannot pick
    /// a different instance from the first.
    #[test]
    fn the_resolved_instance_is_remembered() {
        let runtime = TempDir::new("remember");
        let cache = std::sync::OnceLock::new();
        let wanted = command_socket(&format!("{}/hypr", runtime.path()), "from-env");

        let first = instance_signature_in(&cache, runtime.path(), Some("from-env".into()), |p| {
            told(p == wanted)
        })
        .expect("the variable answers");

        assert_eq!(first, "from-env");
        assert_eq!(cache.get().map(String::as_str), Some("from-env"));

        // Second call: the closure would panic if it were consulted again.
        let second = instance_signature_in(&cache, "/nonexistent", None, |_| unreachable!())
            .expect("cached");
        assert_eq!(second, "from-env");
    }

    /// A failure is not remembered, so a server that starts before its
    /// compositor can resolve on a later attempt.
    #[test]
    fn a_failed_resolution_is_not_remembered() {
        let runtime = TempDir::new("not-cached");
        let cache = std::sync::OnceLock::new();

        assert!(instance_signature_in(&cache, runtime.path(), None, |_| Liveness::Gone).is_err());

        assert!(cache.get().is_none());
    }

    /// The classification behind the warning: only a refusal and a missing
    /// file say something about the instance. Everything else is this process.
    #[test]
    fn only_a_refusal_or_a_missing_file_is_an_ordinary_probe_failure() {
        use std::io::ErrorKind;

        assert!(probe_error_is_ordinary(ErrorKind::ConnectionRefused));
        assert!(probe_error_is_ordinary(ErrorKind::NotFound));
        for ours in [
            ErrorKind::PermissionDenied,
            ErrorKind::TimedOut,
            ErrorKind::OutOfMemory,
        ] {
            assert!(!probe_error_is_ordinary(ours), "{ours:?}");
        }
    }

    /// The path a probe is aimed at, spelled out. The tests above build their
    /// expectation with this same function, so without a literal here any
    /// mapping from signature to string would satisfy them.
    #[test]
    fn the_command_socket_is_where_hyprland_puts_it() {
        assert_eq!(
            command_socket("/run/user/1000/hypr", "sig"),
            "/run/user/1000/hypr/sig/.socket.sock"
        );
    }

    /// Both sockets of one instance sit in one directory. They are resolved
    /// separately, so nothing but this shape keeps them together.
    #[test]
    fn both_sockets_are_built_under_the_same_instance() {
        let command = socket_path_in("/run/user/1000", "sig", ".socket.sock");
        let events = socket_path_in("/run/user/1000", "sig", ".socket2.sock");

        assert_eq!(command, "/run/user/1000/hypr/sig/.socket.sock");
        assert_eq!(events, "/run/user/1000/hypr/sig/.socket2.sock");
    }

    /// An empty variable is a unit whose `Environment=` lost its expansion.
    /// Treating it as a name would send every probe at `.../.socket.sock`.
    #[test]
    fn an_empty_variable_counts_as_unset() {
        assert_eq!(env_signature_from(None), None);
        assert_eq!(env_signature_from(Some(String::new())), None);
        assert_eq!(
            env_signature_from(Some("sig".into())).as_deref(),
            Some("sig")
        );
    }

    /// The real probe, against a real socket. What is pinned is the exact
    /// classification, not just "not live": an unlinked socket and a plain file
    /// must come back `Gone`, because `Unknown` would mean the process could
    /// not find out and would be acted on very differently.
    #[test]
    fn probing_answers_only_for_something_listening() {
        let dir = TempDir::short("hrp");
        let socket = dir.0.join("s.sock");
        let path = socket.to_str().expect("utf-8");

        let listener = std::os::unix::net::UnixListener::bind(&socket).expect("bind");
        assert_eq!(
            probe(path),
            Liveness::Answered,
            "a listening socket must answer"
        );

        drop(listener);
        std::fs::remove_file(&socket).expect("unlink");
        assert_eq!(
            probe(path),
            Liveness::Gone,
            "a missing socket is gone, not undecidable"
        );

        std::fs::write(&socket, b"not a socket").expect("write");
        assert_eq!(
            probe(path),
            Liveness::Gone,
            "a plain file refuses, which is an answer"
        );
    }

    /// The directory is read only when the variable does not answer. Reading
    /// it first made an unreadable `hypr/` fatal for a unit whose variable was
    /// good -- which the plain lookup this replaced never was.
    #[test]
    fn a_live_variable_is_used_without_reading_the_directory() {
        let runtime = TempDir::new("no-scan");
        // No `hypr` subdirectory at all: any read of it would fail.
        let asked = std::cell::RefCell::new(Vec::new());

        let resolved = resolve_in(runtime.path(), Some("sig".into()), |path| {
            asked.borrow_mut().push(path.to_string());
            Liveness::Answered
        })
        .expect("the variable answers, so nothing is read");

        assert_eq!(resolved.signature, "sig");
        assert!(resolved.from_environment);
        assert!(
            resolved.stale_env.is_none(),
            "a variable that answered is not stale"
        );
        // Exactly one question, and about the right file -- a probe aimed
        // anywhere else would report liveness for something we cannot talk to.
        assert_eq!(
            *asked.borrow(),
            vec![format!("{}/hypr/sig/.socket.sock", runtime.path())]
        );
    }

    /// And when it does not answer, the directory under `hypr` is the one
    /// searched -- not the runtime directory itself.
    #[test]
    fn discovery_looks_under_the_hypr_subdirectory() {
        let runtime = TempDir::new("subdir");
        let hypr = runtime.0.join("hypr");
        std::fs::create_dir(&hypr).expect("hypr");
        std::fs::create_dir(hypr.join("live_one")).expect("instance");
        let wanted = command_socket(hypr.to_str().expect("utf-8"), "live_one");

        let resolved = resolve_in(runtime.path(), None, |path| told(path == wanted))
            .expect("one live instance");

        assert_eq!(resolved.signature, "live_one");
    }

    /// A stale variable must not turn the failure into one about a directory.
    #[test]
    fn an_unreadable_directory_still_names_the_stale_variable() {
        let runtime = TempDir::new("unreadable");

        let err = resolve_in(runtime.path(), Some("stale-sig".into()), |_| Liveness::Gone)
            .expect_err("no hypr directory");

        let message = format!("{err}");
        assert!(message.contains("stale-sig"), "{message}");
        assert!(message.contains("does not answer"), "{message}");
    }

    /// `instance_candidates` reads a real directory, so give it one.
    struct TempDir(std::path::PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            Self::at(std::env::temp_dir(), &format!("hypr-rdp-test-{tag}"))
        }

        /// A directory short enough to hold a bindable unix socket.
        ///
        /// `sun_path` is 108 bytes, and `$TMPDIR` on a CI runner is often long
        /// enough on its own to blow that -- so a socket goes under `/tmp`
        /// with a compact name rather than wherever `temp_dir()` points.
        fn short(tag: &str) -> Self {
            Self::at(std::path::PathBuf::from("/tmp"), tag)
        }

        fn at(base: std::path::PathBuf, stem: &str) -> Self {
            let thread = format!("{:?}", std::thread::current().id());
            let digits: String = thread.chars().filter(|c| c.is_ascii_digit()).collect();
            // `/tmp` is world-writable, so the name carries something nobody
            // else can predict; `create_dir` then fails rather than adopting a
            // directory somebody else made. Kept short on purpose: these hold
            // unix sockets, and `sun_path` is 108 bytes.
            let nonce = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos())
                .unwrap_or_default();
            let path = base.join(format!("{stem}-{}-{digits}-{nonce:x}", std::process::id()));
            std::fs::create_dir_all(&path).expect("temp dir");
            Self(path)
        }

        fn path(&self) -> &str {
            self.0.to_str().expect("utf-8 temp path")
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Directories only. A stray file is not an instance. Order is not part of
    /// the contract -- the message that lists names sorts them itself -- so the
    /// result is sorted here rather than expected to arrive that way.
    #[test]
    fn candidates_are_the_subdirectories_and_nothing_else() {
        let dir = TempDir::new("candidates");
        for name in ["zzz_second", "aaa_first"] {
            std::fs::create_dir(self_path(&dir, name)).expect("subdir");
        }
        std::fs::write(self_path(&dir, "stray-file"), b"not an instance").expect("file");

        let mut found = instance_candidates(dir.path()).expect("readable");
        found.sort();

        assert_eq!(
            found,
            vec!["aaa_first".to_string(), "zzz_second".to_string()]
        );
    }

    fn self_path(dir: &TempDir, name: &str) -> std::path::PathBuf {
        dir.0.join(name)
    }

    /// A directory that is not there is reported as one sentence naming the
    /// variable and the path, because `main` prints anyhow's `Debug` one cause
    /// per line and a fragment would be a line attached to nothing.
    #[test]
    fn a_missing_directory_is_reported_with_the_variable_and_the_path() {
        let runtime = TempDir::new("missing");
        let hypr = runtime.0.join("hypr");

        let err =
            resolve_in(runtime.path(), None, |_| Liveness::Gone).expect_err("no hypr directory");

        let message = format!("{err}");
        assert!(
            message.contains("HYPRLAND_INSTANCE_SIGNATURE is not set"),
            "{message}"
        );
        assert!(message.contains("could not be read"), "{message}");
        assert!(message.contains(hypr.to_str().expect("utf-8")), "{message}");
    }

    /// The point of probing the variable before reading the directory: a
    /// directory that cannot be listed is fatal only when we actually need to
    /// list it. Traversable but unlistable (0300) is the case the ordering
    /// exists for -- an absent directory does not exercise it, because that
    /// fails for a different reason.
    #[test]
    fn an_unlistable_directory_does_not_stop_a_live_variable() {
        use std::os::unix::fs::PermissionsExt as _;

        let runtime = TempDir::new("unlistable");
        let hypr = runtime.0.join("hypr");
        std::fs::create_dir(&hypr).expect("hypr");
        std::fs::create_dir(hypr.join("live_one")).expect("instance");
        std::fs::set_permissions(&hypr, std::fs::Permissions::from_mode(0o300)).expect("chmod");

        // Root ignores the mode bits, so there would be nothing to test.
        let listable = std::fs::read_dir(&hypr).is_ok();
        let wanted = command_socket(hypr.to_str().expect("utf-8"), "live_one");
        let resolved = resolve_in(runtime.path(), Some("live_one".into()), |path| {
            told(path == wanted)
        });

        // Restore before asserting, or the temp dir cannot be removed.
        std::fs::set_permissions(&hypr, std::fs::Permissions::from_mode(0o700)).expect("restore");

        let resolved = resolved.expect("the variable answers, so the listing is never needed");
        assert_eq!(resolved.signature, "live_one");
        assert!(resolved.from_environment);
        // Root ignores the mode bits, so the unreadable case simply does not
        // exist there. Asked of the kernel rather than of `USER`, which is not
        // set in a bare container shell.
        let is_root = unsafe { libc::geteuid() } == 0;
        assert!(!listable || is_root);
    }

    use anyhow::anyhow;

    use super::{is_non_legacy_parser_error, monitor_rule_to_lua, option_string_from_response};

    #[test]
    fn option_string_parser_returns_non_empty_string_values() {
        let value = option_string_from_response(
            r#"{"option":"input:kb_layout","str":" de , us ","set":true}"#,
        )
        .expect("option parses");

        assert_eq!(value.as_deref(), Some("de , us"));
    }

    #[test]
    fn option_string_parser_treats_empty_strings_as_unset() {
        let value =
            option_string_from_response(r#"{"option":"input:kb_variant","str":"","set":true}"#)
                .expect("option parses");

        assert_eq!(value, None);
    }

    #[test]
    fn monitor_rule_to_lua_translates_generated_headless_rule() {
        let lua = monitor_rule_to_lua("hypr-rdp-1,1920x1080@60,-9999x0,1")
            .expect("monitor rule translates");

        assert_eq!(
            lua,
            r#"hl.monitor({ output = "hypr-rdp-1", mode = "1920x1080@60", position = "-9999x0", scale = 1 })"#
        );
    }

    #[test]
    fn monitor_rule_to_lua_quotes_non_numeric_scale() {
        let lua = monitor_rule_to_lua("DP-1,preferred,auto,auto").expect("monitor rule translates");

        assert_eq!(
            lua,
            r#"hl.monitor({ output = "DP-1", mode = "preferred", position = "auto", scale = "auto" })"#
        );
    }

    #[test]
    fn monitor_rule_to_lua_escapes_lua_strings() {
        let lua =
            monitor_rule_to_lua(r#"DP-"1,modeline 1\2,0x0,1"#).expect("monitor rule translates");

        assert_eq!(
            lua,
            r#"hl.monitor({ output = "DP-\"1", mode = "modeline 1\\2", position = "0x0", scale = 1 })"#
        );
    }

    #[test]
    fn monitor_rule_to_lua_rejects_malformed_rules() {
        let err = monitor_rule_to_lua("hypr-rdp-1,1920x1080@60,-9999x0")
            .expect_err("malformed rule is rejected");

        assert!(err.to_string().contains("expected 4 fields"));
    }

    #[test]
    fn non_legacy_parser_error_detection_matches_hyprland_message() {
        let err =
            anyhow!("Hyprland IPC error: keyword can't work with non-legacy parsers. Use eval.");

        assert!(is_non_legacy_parser_error(&err));
    }

    #[test]
    fn non_legacy_parser_error_detection_ignores_other_errors() {
        let err = anyhow!("Hyprland IPC error: monitor rule failed");

        assert!(!is_non_legacy_parser_error(&err));
    }
}
