//! Public monitor IPC regression tests against an external Unix-socket peer.
//!
//! Isolated test processes exercise the normal environment/socket discovery path
//! without changing the process-global compositor identity of other tests.

use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixListener;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

const CLIENT_TEST: &str = "hyprland::monitor_ipc_tests::monitor_ipc_client";
const ACTIONS_ENV: &str = "HYPR_RDP_MONITOR_IPC_TEST_ACTIONS";
const RULE: &str = "hypr-rdp,1920x1080@60,-9999x0,1";
const LUA_REQUEST: &str = r#"eval hl.monitor({ output = "hypr-rdp", mode = "1920x1080@60", position = "-9999x0", scale = 1 })"#;
const LEGACY_REQUEST: &str = "keyword monitor hypr-rdp,1920x1080@60,-9999x0,1";
const MONITORS: &str =
    r#"[{"name":"hypr-rdp","width":1920,"height":1080,"x":-9999,"y":0,"scale":1}]"#;

#[derive(Serialize, Deserialize)]
enum ClientAction {
    Apply { rule: String, expected: Expected },
    Monitors(Value),
}

#[derive(Serialize, Deserialize)]
enum Expected {
    Accepted,
    Rejected(Vec<String>),
    IoFailure(Vec<String>),
}

enum PeerResponse {
    Bytes(Vec<u8>),
    Stall,
    ResetBeforeRead,
    CloseListener(Vec<u8>),
}

struct Exchange {
    request: String,
    response: PeerResponse,
}

impl Exchange {
    fn text(request: &str, response: &str) -> Self {
        Self {
            request: request.to_owned(),
            response: PeerResponse::Bytes(response.as_bytes().to_vec()),
        }
    }
}

struct ClientProcess(Child);

impl Drop for ClientProcess {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct RuntimeDirectory(std::path::PathBuf);

impl Drop for RuntimeDirectory {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).expect("remove isolated monitor IPC runtime directory");
    }
}

fn apply(rule: &str, expected: Expected) -> ClientAction {
    ClientAction::Apply {
        rule: rule.to_owned(),
        expected,
    }
}

fn run_peer(actions: Vec<ClientAction>, exchanges: Vec<Exchange>) {
    static NEXT_RUNTIME: AtomicUsize = AtomicUsize::new(0);
    let runtime = RuntimeDirectory(std::env::temp_dir().join(format!(
        "hypr-rdp-monitor-ipc-{}-{}",
        std::process::id(),
        NEXT_RUNTIME.fetch_add(1, Ordering::Relaxed)
    )));
    std::fs::create_dir(&runtime.0).expect("create fresh monitor IPC runtime directory");
    let instance = runtime.0.join("hypr/test-instance");
    std::fs::create_dir_all(&instance).expect("create isolated monitor IPC instance");
    let mut listener = if exchanges.is_empty() {
        None
    } else {
        let listener =
            UnixListener::bind(instance.join(".socket.sock")).expect("bind compositor peer");
        listener.set_nonblocking(true).unwrap();
        Some(listener)
    };

    let mut client = ClientProcess(
        Command::new(std::env::current_exe().unwrap())
            .args(["--exact", CLIENT_TEST, "--nocapture", "--test-threads=1"])
            .env("XDG_RUNTIME_DIR", &runtime.0)
            .env("HYPRLAND_INSTANCE_SIGNATURE", "test-instance")
            .env(ACTIONS_ENV, serde_json::to_string(&actions).unwrap())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("start isolated public IPC client"),
    );
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut requests = Vec::new();
    let mut stalled_connections = Vec::new();
    let status = loop {
        if let Some(status) = client.0.try_wait().expect("query IPC client status") {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "public IPC client exceeded deadline"
        );
        let connection = match listener.as_ref() {
            Some(listener) => listener.accept(),
            None => Err(std::io::ErrorKind::WouldBlock.into()),
        };
        match connection {
            Ok((mut stream, _)) => {
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let exchange = exchanges.get(requests.len());
                if exchange.is_some_and(|exchange| {
                    matches!(exchange.response, PeerResponse::ResetBeforeRead)
                }) {
                    let mut byte = 0u8;
                    // SAFETY: the stream is live and byte is a valid one-byte output
                    // buffer. Peek ensures data is unread when the peer closes,
                    // producing a real connection reset rather than a clean EOF.
                    let count = unsafe {
                        libc::recv(
                            stream.as_raw_fd(),
                            (&mut byte as *mut u8).cast(),
                            1,
                            libc::MSG_PEEK,
                        )
                    };
                    assert_eq!(count, 1, "client wrote before peer reset");
                    requests.push("<peer reset before reading request>".to_owned());
                    continue;
                }
                let mut request = String::new();
                stream
                    .read_to_string(&mut request)
                    .expect("read public IPC request");
                match exchange
                    .filter(|exchange| exchange.request == request)
                    .map(|exchange| &exchange.response)
                {
                    Some(PeerResponse::Bytes(bytes)) => stream
                        .write_all(bytes)
                        .expect("reply to public IPC request"),
                    Some(PeerResponse::Stall) => stalled_connections.push(stream),
                    Some(PeerResponse::CloseListener(bytes)) => {
                        stream
                            .write_all(bytes)
                            .expect("reply before removing listener");
                        drop(listener.take());
                    }
                    _ => stream
                        .write_all(b"unknown request")
                        .expect("reject unexpected public IPC request"),
                }
                requests.push(request);
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(2));
            }
            Err(error) => panic!("accept public IPC request: {error}"),
        }
    };
    let mut stdout = String::new();
    let mut stderr = String::new();
    client
        .0
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut stdout)
        .unwrap();
    client
        .0
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .unwrap();
    assert!(
        status.success(),
        "public IPC client failed:\n{stdout}\n{stderr}\nrequests: {requests:?}"
    );
    assert_eq!(
        requests,
        exchanges
            .into_iter()
            .map(|exchange| exchange.request)
            .collect::<Vec<_>>()
    );
}

#[test]
fn monitor_ipc_client() {
    let Ok(actions) = std::env::var(ACTIONS_ENV) else {
        return;
    };
    for action in serde_json::from_str::<Vec<ClientAction>>(&actions).unwrap() {
        match action {
            ClientAction::Apply { rule, expected } => match expected {
                Expected::Accepted => super::keyword_monitor(&rule).expect("monitor rule accepted"),
                Expected::Rejected(replies) => {
                    let error = super::keyword_monitor(&rule).expect_err("monitor rule rejected");
                    let message = error.to_string();
                    let mut remaining = message.as_str();
                    for reply in replies {
                        let position = remaining.find(&reply).unwrap_or_else(|| {
                            panic!("missing/out-of-order reply {reply:?} in {message:?}")
                        });
                        remaining = &remaining[position + reply.len()..];
                    }
                }
                Expected::IoFailure(kinds) => {
                    let error =
                        super::keyword_monitor(&rule).expect_err("transport failure propagates");
                    let io_error = error
                        .chain()
                        .find_map(|cause| cause.downcast_ref::<std::io::Error>())
                        .unwrap_or_else(|| panic!("original transport error lost: {error:#}"));
                    let kind = format!("{:?}", io_error.kind());
                    assert!(
                        kinds.contains(&kind),
                        "unexpected transport error: {error:#}"
                    );
                }
            },
            ClientAction::Monitors(expected) => {
                assert_eq!(super::monitors().expect("read back monitors"), expected);
            }
        }
    }
}

#[test]
fn lua_monitor_rule_is_accepted_without_legacy_command() {
    run_peer(
        vec![
            apply(RULE, Expected::Accepted),
            ClientAction::Monitors(json!([{
                "name": "hypr-rdp", "width": 1920, "height": 1080,
                "x": -9999, "y": 0, "scale": 1,
            }])),
        ],
        vec![
            Exchange::text(LUA_REQUEST, "ok"),
            Exchange::text("j/monitors", MONITORS),
        ],
    );
}

#[test]
fn legacy_monitor_rule_is_accepted_after_lua_rejection() {
    run_peer(
        vec![apply(RULE, Expected::Accepted)],
        vec![
            Exchange::text(
                LUA_REQUEST,
                "eval is only supported with the lua config manager",
            ),
            Exchange::text(LEGACY_REQUEST, "ok"),
        ],
    );
}

// Golden requests exercise valid four-field rules and Lua escaping independently
// of the production translator. Reply classifications are literals from the
// agreed response contract and tagged Hyprland config-manager responses.
const RULE_CASES: &[(&str, &str)] = &[
    (RULE, LUA_REQUEST),
    (
        "hypr-rdp,1280x720@60,-9999x0,1.5",
        r#"eval hl.monitor({ output = "hypr-rdp", mode = "1280x720@60", position = "-9999x0", scale = 1.5 })"#,
    ),
    (
        " DP-1 , preferred , auto , auto ",
        r#"eval hl.monitor({ output = "DP-1", mode = "preferred", position = "auto", scale = "auto" })"#,
    ),
    (
        r#"DP-"1,modeline 1\2,0x0,1"#,
        r#"eval hl.monitor({ output = "DP-\"1", mode = "modeline 1\\2", position = "0x0", scale = 1 })"#,
    ),
];

const REPLIES: &[(&str, Option<&str>)] = &[
    ("ok", None),
    ("ok\n", None),
    (" \tok\r\n", None),
    ("", Some("")),
    (" \t\n", Some("")),
    ("okay", Some("okay")),
    ("ok but invalid mode", Some("ok but invalid mode")),
    ("ok\nerror: invalid mode", Some("ok\nerror: invalid mode")),
    ("OK", Some("OK")),
    ("unknown request", Some("unknown request")),
    (" \nunknown request\t", Some("unknown request")),
    (
        "eval is only supported with the lua config manager",
        Some("eval is only supported with the lua config manager"),
    ),
    (
        "keyword can't work with non-legacy parsers. Use eval.",
        Some("keyword can't work with non-legacy parsers. Use eval."),
    ),
    ("error: invalid mode", Some("error: invalid mode")),
];

proptest::proptest! {
    #[test]
    fn generated_monitor_rule_backend_and_reply_matrix(
        rule_index in 0usize..RULE_CASES.len(),
        lua_reply_index in 0usize..REPLIES.len(),
        legacy_reply_index in 0usize..REPLIES.len(),
    ) {
        let (rule, lua_request) = RULE_CASES[rule_index];
        let (lua_reply, lua_error) = REPLIES[lua_reply_index];
        let mut exchanges = vec![Exchange::text(lua_request, lua_reply)];
        let expected = if let Some(lua_error) = lua_error {
            let (legacy_reply, legacy_error) = REPLIES[legacy_reply_index];
            exchanges.push(Exchange::text(&format!("keyword monitor {rule}"), legacy_reply));
            if let Some(legacy_error) = legacy_error {
                Expected::Rejected(vec![format!("eval: {lua_error}"), format!("keyword: {legacy_error}")])
            } else {
                Expected::Accepted
            }
        } else {
            Expected::Accepted
        };
        run_peer(vec![apply(rule, expected)], exchanges);
    }
}

#[test]
fn monitor_transport_failures_are_not_replayed() {
    run_peer(
        vec![apply(RULE, Expected::IoFailure(vec!["NotFound".into()]))],
        vec![],
    );
    for legacy in [false, true] {
        for (response, kinds) in [
            (
                PeerResponse::Bytes(b"ok\xff".to_vec()),
                vec!["InvalidData".into()],
            ),
            (
                PeerResponse::Stall,
                vec!["WouldBlock".into(), "TimedOut".into()],
            ),
            (
                PeerResponse::ResetBeforeRead,
                vec!["ConnectionReset".into(), "BrokenPipe".into()],
            ),
        ] {
            let mut exchanges = Vec::new();
            if legacy {
                exchanges.push(Exchange::text(LUA_REQUEST, "unknown request"));
            }
            let request = if matches!(response, PeerResponse::ResetBeforeRead) {
                "<peer reset before reading request>"
            } else if legacy {
                LEGACY_REQUEST
            } else {
                LUA_REQUEST
            };
            exchanges.push(Exchange {
                request: request.into(),
                response,
            });
            run_peer(vec![apply(RULE, Expected::IoFailure(kinds))], exchanges);
        }
    }
    run_peer(
        vec![apply(
            RULE,
            Expected::IoFailure(vec!["ConnectionRefused".into()]),
        )],
        vec![Exchange {
            request: LUA_REQUEST.into(),
            response: PeerResponse::CloseListener(b"unknown request".to_vec()),
        }],
    );
}

#[test]
fn monitor_application_recovers_after_invalid_rules_and_backend_changes() {
    run_peer(
        vec![
            apply(
                "hypr-rdp,1920x1080@60,-9999x0",
                Expected::Rejected(vec!["expected 4 fields".into()]),
            ),
            apply(
                "hypr-rdp,1920x1080@60,-9999x0,",
                Expected::Rejected(vec!["empty scale".into()]),
            ),
            apply(
                "hypr-\u{1},1920x1080@60,-9999x0,1",
                Expected::Rejected(vec!["control character".into()]),
            ),
            apply(RULE, Expected::Accepted),
            apply(
                "hypr-rdp,garbage,-9999x0,1",
                Expected::Rejected(vec!["error: invalid mode".into(), "unknown request".into()]),
            ),
            apply(RULE, Expected::Accepted),
            apply("hypr-rdp,1280x720@60,-9999x0,1.5", Expected::Accepted),
            ClientAction::Monitors(json!([{
                "name": "hypr-rdp", "width": 1280, "height": 720,
                "x": -9999, "y": 0, "scale": 1.5,
            }])),
        ],
        vec![
            Exchange::text(LUA_REQUEST, "unknown request"),
            Exchange::text(LEGACY_REQUEST, "ok"),
            Exchange::text(
                r#"eval hl.monitor({ output = "hypr-rdp", mode = "garbage", position = "-9999x0", scale = 1 })"#,
                "error: invalid mode",
            ),
            Exchange::text(
                "keyword monitor hypr-rdp,garbage,-9999x0,1",
                "unknown request",
            ),
            Exchange::text(LUA_REQUEST, "ok"),
            Exchange::text(
                r#"eval hl.monitor({ output = "hypr-rdp", mode = "1280x720@60", position = "-9999x0", scale = 1.5 })"#,
                "ok",
            ),
            Exchange::text(
                "j/monitors",
                r#"[{"name":"hypr-rdp","width":1280,"height":720,"x":-9999,"y":0,"scale":1.5}]"#,
            ),
        ],
    );
}

#[test]
fn monitor_rejection_preserves_both_replies_in_attempt_order() {
    let lua_error = "error: hl.monitor: error applying field 'mode'\ninvalid mode";
    run_peer(
        vec![apply(
            RULE,
            Expected::Rejected(vec![lua_error.into(), "unknown request".into()]),
        )],
        vec![
            Exchange::text(LUA_REQUEST, lua_error),
            Exchange::text(LEGACY_REQUEST, "unknown request"),
        ],
    );
}
